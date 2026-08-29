//! The display core: owns the frame buffer and drives updates as a sequence of chunks.
//!
//! This is mk3's chunk model (`light_display_chunk_model`), the most hardware-tested design in
//! the stack, made host-testable. A driver answers three questions about an update -- how many
//! chunks, how to push chunk N, has the one in flight landed -- and the core owns everything
//! else: the in-progress state, the per-chunk deadline, the chunk index, and the spin-or-yield
//! budget. The three bugs that design was born from (async path missing on one transport,
//! deadline measured over the whole update, "yield on first wait" advancing one chunk per
//! tick) are each a test below.
//!
//! What Rust adds: while an update is in flight the frame buffer cannot be mutated, because
//! [`Display::frame_mut`] returns `None`. mk3 documented that rule; here it is the borrow.

use crate::draw::PixelFormat;
use crate::hal::Clock;

/// An inclusive rectangle in physical (panel) coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
        pub x0: u16,
        pub y0: u16,
        pub x1: u16,
        pub y1: u16,
}

impl Region {
        pub const fn new(x0: u16, y0: u16, x1: u16, y1: u16) -> Self {
                Self { x0, y0, x1, y1 }
        }

        pub const fn full(width: u16, height: u16) -> Self {
                Self { x0: 0, y0: 0, x1: width - 1, y1: height - 1 }
        }

        pub const fn width(&self) -> u16 {
                self.x1 - self.x0 + 1
        }

        pub const fn height(&self) -> u16 {
                self.y1 - self.y0 + 1
        }

        /// The smallest region covering both. This is how a caller honours the rule that a
        /// region update must cover what was drawn before as well as what is drawn now.
        pub fn union(&self, other: &Region) -> Region {
                Region {
                        x0: self.x0.min(other.x0),
                        y0: self.y0.min(other.y0),
                        x1: self.x1.max(other.x1),
                        y1: self.y1.max(other.y1),
                }
        }

        /// Clamp into the panel. Both corners against the same bound, so a region that was
        /// entirely off-panel collapses to an edge rather than inverting -- an inverted region
        /// has a meaningless chunk count and would never complete.
        pub fn clamped(&self, width: u16, height: u16) -> Region {
                Region {
                        x0: self.x0.min(width - 1),
                        y0: self.y0.min(height - 1),
                        x1: self.x1.min(width - 1),
                        y1: self.y1.min(height - 1),
                }
        }
}

/// A read-only view of the frame buffer with its geometry, handed to a driver's `kick`.
pub struct Frame<'a> {
        pub buf: &'a [u8],
        pub width: u16,
        pub height: u16,
        pub format: PixelFormat,
        /// Bytes per row.
        pub stride: usize,
}

impl Frame<'_> {
        /// The bytes of one row-run inside `region` at row `y` (absolute). Whole-pixel formats
        /// only: a packed 1 bpp row has no byte-aligned run for an arbitrary x range.
        pub fn row(&self, region: &Region, y: u16) -> &[u8] {
                debug_assert!(self.format == PixelFormat::Rgb565);
                let start = y as usize * self.stride + region.x0 as usize * 2;
                let len = region.width() as usize * 2;
                &self.buf[start..start + len]
        }

        /// The contiguous bytes of every row in `region` -- only meaningful when the region
        /// spans the full width, which is when consecutive rows are adjacent in memory.
        pub fn rows(&self, region: &Region) -> &[u8] {
                debug_assert!(region.x0 == 0 && region.x1 == self.width - 1);
                let start = region.y0 as usize * self.stride;
                let len = region.height() as usize * self.stride;
                &self.buf[start..start + len]
        }
}

pub trait DisplayDriver {
        /// Bring the controller up. Blocking delays are allowed here and nowhere else.
        fn init(&mut self, clock: &mut dyn Clock, width: u16, height: u16);

        /// How many chunks the update of `region` takes; 0 means nothing to send.
        fn chunk_count(&self, region: &Region) -> u16;

        /// How many chunk completions one poll may spin for. 0 = never spin, yield as soon as
        /// the chunk in flight is not done: right when a chunk is one large transfer worth
        /// overlapping with real work. Nonzero = spin for up to that many: right when chunks
        /// are small and numerous, where a freshly kicked chunk is essentially never complete
        /// on the next check and yielding would advance one chunk per pass.
        ///
        /// Asked once per update, because the answer depends on how THIS region chunked.
        fn chunks_per_poll(&self, region: &Region) -> u16;

        /// Start pushing chunk `index` of `region`. Must not wait for it.
        fn kick(&mut self, frame: &Frame<'_>, region: &Region, index: u16);

        /// Has the chunk most recently kicked finished transferring?
        fn chunk_complete(&mut self) -> bool;

        /// Upper bound on ONE chunk, after which the update is abandoned.
        fn chunk_timeout_ms(&self) -> u32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateError {
        /// An update is already in flight.
        Busy,
        /// A chunk did not complete within the driver's deadline. The update is abandoned;
        /// whatever the transport still has in flight is not cancelled.
        Timeout,
}

struct Update {
        region: Region,
        chunk_index: u16,
        chunk_count: u16,
        per_poll: u16,
        /// When the chunk in flight was kicked. The deadline is measured from here, so it
        /// bounds one transfer, not how long the update has been outstanding between polls.
        chunk_started_us: u64,
}

pub struct Display<'b, D: DisplayDriver> {
        driver: D,
        /// What updates read from.
        buf: &'b mut [u8],
        /// Under double buffering, what drawing goes into; swapped with `buf` per frame.
        back: Option<&'b mut [u8]>,
        width: u16,
        height: u16,
        format: PixelFormat,
        update: Option<Update>,
        now: fn() -> u64,
        /// Updates abandoned on a chunk deadline, for the caller to report.
        pub timeouts: u32,
}

impl<'b, D: DisplayDriver> Display<'b, D> {
        /// `buf` must hold `format.buffer_len(width, height)` bytes.
        pub fn new(driver: D, buf: &'b mut [u8], width: u16, height: u16, format: PixelFormat, now: fn() -> u64) -> Self {
                assert!(buf.len() >= format.buffer_len(width, height));
                Self { driver, buf, back: None, width, height, format, update: None, now, timeouts: 0 }
        }

        /// Give the display a second buffer. Drawing then goes into the back buffer while an
        /// update reads the front, and [`swap`](Self::swap) exchanges them between frames.
        pub fn set_back_buffer(&mut self, back: &'b mut [u8]) {
                assert!(back.len() >= self.format.buffer_len(self.width, self.height));
                self.back = Some(back);
        }

        pub fn is_double_buffered(&self) -> bool {
                self.back.is_some()
        }

        /// Exchange front and back. Refused while an update is reading the front buffer.
        pub fn swap(&mut self) -> Result<(), UpdateError> {
                if self.update.is_some() {
                        return Err(UpdateError::Busy);
                }
                if let Some(back) = self.back.as_mut() {
                        core::mem::swap(&mut self.buf, back);
                }
                Ok(())
        }

        pub fn format(&self) -> PixelFormat {
                self.format
        }

        pub fn init(&mut self, clock: &mut dyn Clock) {
                self.driver.init(clock, self.width, self.height);
        }

        pub fn width(&self) -> u16 {
                self.width
        }

        pub fn height(&self) -> u16 {
                self.height
        }

        pub fn driver(&mut self) -> &mut D {
                &mut self.driver
        }

        pub fn busy(&self) -> bool {
                self.update.is_some()
        }

        /// The buffer for drawing: the back buffer under double buffering, always available;
        /// otherwise the one buffer -- unless an update is reading it, in which case `None`.
        /// Poll or wait first.
        pub fn frame_mut(&mut self) -> Option<&mut [u8]> {
                match self.back.as_mut() {
                        Some(back) => Some(back),
                        None if self.update.is_some() => None,
                        None => Some(self.buf),
                }
        }

        /// The buffer updates read from, for a caller that wants to inspect what is on the way
        /// to the panel. `None` while an update is reading it.
        pub fn front(&self) -> Option<&[u8]> {
                if self.update.is_some() { None } else { Some(self.buf) }
        }

        /// Start pushing `region` to the panel and return. Drive it with [`poll`](Self::poll).
        pub fn update_async(&mut self, region: Region) -> Result<(), UpdateError> {
                if self.update.is_some() {
                        return Err(UpdateError::Busy);
                }
                let region = region.clamped(self.width, self.height);
                let chunk_count = self.driver.chunk_count(&region);
                if chunk_count == 0 {
                        return Ok(());
                }
                let per_poll = self.driver.chunks_per_poll(&region);
                let frame = Frame { buf: self.buf, width: self.width, height: self.height, format: self.format, stride: self.format.stride(self.width) };
                self.driver.kick(&frame, &region, 0);
                self.update = Some(Update { region, chunk_index: 0, chunk_count, per_poll, chunk_started_us: (self.now)() });
                Ok(())
        }

        /// Advance the update in flight. `Ok(true)` while still busy, `Ok(false)` when idle.
        pub fn poll(&mut self) -> Result<bool, UpdateError> {
                let Self { driver, buf, width, height, format, update, now, timeouts, .. } = self;
                let Some(u) = update else { return Ok(false) };
                let frame = Frame { buf, width: *width, height: *height, format: *format, stride: format.stride(*width) };
                let budget = u.per_poll;
                let mut completed = 0u16;
                loop {
                        if !driver.chunk_complete() {
                                if now().saturating_sub(u.chunk_started_us) > u64::from(driver.chunk_timeout_ms()) * 1000 {
                                        *update = None;
                                        *timeouts += 1;
                                        return Err(UpdateError::Timeout);
                                }
                                if budget == 0 || completed >= budget {
                                        return Ok(true);
                                }
                                core::hint::spin_loop();
                                continue;
                        }
                        completed += 1;
                        u.chunk_index += 1;
                        if u.chunk_index >= u.chunk_count {
                                *update = None;
                                return Ok(false);
                        }
                        driver.kick(&frame, &u.region, u.chunk_index);
                        u.chunk_started_us = now();
                }
        }

        /// Drive any update in flight to completion, cooperatively -- the same poll the runtime
        /// runs, inline.
        pub fn wait(&mut self) -> Result<(), UpdateError> {
                while self.poll()? {}
                Ok(())
        }

        pub fn update_blocking(&mut self, region: Region) -> Result<(), UpdateError> {
                self.update_async(region)?;
                self.wait()
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use core::sync::atomic::{AtomicU64, Ordering};
        use std::vec::Vec;

        static NOW: AtomicU64 = AtomicU64::new(0);
        fn now() -> u64 {
                NOW.load(Ordering::Relaxed)
        }
        fn set_now(us: u64) {
                NOW.store(us, Ordering::Relaxed);
        }

        /// Chunks by row unless the region is full-width, like ST7789. Each `chunk_complete`
        /// query decrements a countdown, so the tests can say "complete on the Nth check".
        struct Mock {
                kicks: Vec<(Region, u16)>,
                completes_after: u32,
                remaining: u32,
                queries: u32,
                per_poll_narrow: u16,
        }

        impl Mock {
                fn new(completes_after: u32) -> Self {
                        Self { kicks: Vec::new(), completes_after, remaining: completes_after, queries: 0, per_poll_narrow: 8 }
                }
        }

        impl DisplayDriver for Mock {
                fn init(&mut self, _: &mut dyn Clock, _: u16, _: u16) {}
                fn chunk_count(&self, r: &Region) -> u16 {
                        if r.x0 == 0 && r.x1 == 15 { 1 } else { r.height() }
                }
                fn chunks_per_poll(&self, r: &Region) -> u16 {
                        if r.x0 == 0 && r.x1 == 15 { 0 } else { self.per_poll_narrow }
                }
                fn kick(&mut self, _: &Frame<'_>, r: &Region, i: u16) {
                        self.kicks.push((*r, i));
                        self.remaining = self.completes_after;
                }
                fn chunk_complete(&mut self) -> bool {
                        self.queries += 1;
                        if self.remaining == 0 {
                                true
                        } else {
                                self.remaining -= 1;
                                false
                        }
                }
                fn chunk_timeout_ms(&self) -> u32 {
                        10
                }
        }

        fn display(buf: &mut [u8], mock: Mock) -> Display<'_, Mock> {
                Display::new(mock, buf, 16, 8, PixelFormat::Rgb565, now)
        }

        #[test]
        fn a_full_width_region_is_one_chunk_that_yields_immediately() {
                let mut buf = [0u8; 16 * 8 * 2];
                set_now(0);
                let mut d = display(&mut buf, Mock::new(3));
                d.update_async(Region::full(16, 8)).unwrap();
                assert!(d.frame_mut().is_none(), "the buffer is being read");
                //   per_poll 0: every poll that finds the chunk unfinished returns at once
                assert_eq!(d.poll(), Ok(true));
                assert_eq!(d.poll(), Ok(true));
                assert_eq!(d.poll(), Ok(true));
                assert_eq!(d.poll(), Ok(false));
                assert_eq!(d.driver().kicks.len(), 1);
                assert!(d.frame_mut().is_some());
        }

        #[test]
        fn a_narrow_region_advances_up_to_the_budget_per_poll() {
                let mut buf = [0u8; 16 * 8 * 2];
                set_now(0);
                //   chunks complete on the second query, so a poll that yielded on the first
                // would move one row per pass -- the mk3 bug. with a budget of 3 it spins
                // through three rows before handing back
                let mut mock = Mock::new(1);
                mock.per_poll_narrow = 3;
                let mut d = display(&mut buf, mock);
                d.update_async(Region::new(2, 0, 5, 7)).unwrap();
                assert_eq!(d.poll(), Ok(true));
                assert_eq!(d.driver().kicks.len(), 4, "row 0 at start, then rows 1..=3 in one poll");
                assert_eq!(d.poll(), Ok(true));
                assert_eq!(d.driver().kicks.len(), 7);
                assert_eq!(d.poll(), Ok(false));
                assert_eq!(d.driver().kicks.len(), 8);
                let rows: Vec<u16> = d.driver().kicks.iter().map(|k| k.1).collect();
                assert_eq!(rows, (0..8).collect::<Vec<_>>());
        }

        #[test]
        fn the_deadline_bounds_one_chunk_not_the_whole_update() {
                let mut buf = [0u8; 16 * 8 * 2];
                set_now(0);
                //   chunks complete on the first check
                let mut d = display(&mut buf, Mock::new(0));
                d.update_async(Region::new(2, 0, 5, 7)).unwrap();
                //   the caller comes back very late -- far beyond one chunk's deadline -- but
                // the chunk did complete in the meantime. that is a healthy update, and it
                // must proceed (here: run to completion within the budget) rather than be
                // aborted for the caller's tardiness
                set_now(1_000_000);
                assert_eq!(d.poll(), Ok(false));
                assert_eq!(d.timeouts, 0);
                assert_eq!(d.driver().kicks.len(), 8);
        }

        #[test]
        fn a_stalled_chunk_times_out_and_frees_the_buffer() {
                let mut buf = [0u8; 16 * 8 * 2];
                set_now(0);
                let mut d = display(&mut buf, Mock::new(u32::MAX));
                d.update_async(Region::full(16, 8)).unwrap();
                assert_eq!(d.poll(), Ok(true));
                set_now(10_001);
                assert_eq!(d.poll(), Err(UpdateError::Timeout));
                assert_eq!(d.timeouts, 1);
                assert!(d.frame_mut().is_some(), "an abandoned update no longer holds the buffer");
        }

        #[test]
        fn a_second_update_while_busy_is_refused_not_queued() {
                let mut buf = [0u8; 16 * 8 * 2];
                set_now(0);
                let mut d = display(&mut buf, Mock::new(1));
                d.update_async(Region::full(16, 8)).unwrap();
                assert_eq!(d.update_async(Region::full(16, 8)), Err(UpdateError::Busy));
        }

        #[test]
        fn regions_clamp_without_inverting() {
                let r = Region::new(20, 3, 40, 30).clamped(16, 8);
                assert_eq!(r, Region::new(15, 3, 15, 7));
                assert_eq!(Region::new(0, 0, 3, 3).union(&Region::new(2, 2, 9, 5)), Region::new(0, 0, 9, 5));
        }

        #[test]
        fn frame_rows_index_the_buffer_correctly() {
                let mut buf = [0u8; 16 * 8 * 2];
                for (i, b) in buf.iter_mut().enumerate() {
                        *b = i as u8;
                }
                let f = Frame { buf: &buf, width: 16, height: 8, format: PixelFormat::Rgb565, stride: 32 };
                let row = f.row(&Region::new(2, 0, 5, 7), 3);
                assert_eq!(row.len(), 8);
                assert_eq!(row[0], ((3 * 16 + 2) * 2) as u8);
                let rows = f.rows(&Region::new(0, 2, 15, 3));
                assert_eq!(rows.len(), 2 * 16 * 2);
                assert_eq!(rows[0], (2 * 32) as u8);
        }
}
