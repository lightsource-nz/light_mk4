//! The frame layer: pacing, buffering and dirty regions between the rasteriser and a display.
//!
//! Ported from the predecessor C framework, where it absorbed four divergent copies of the
//! frame loop; its two contracts hold here:
//!
//! - **Every frame is a full repaint.** `frame_begin` clears the buffer. Under double
//!   buffering the buffer being drawn into was last touched two frames ago, so it cannot be
//!   patched incrementally; redrawing it entirely is what makes region tracking sound.
//! - **Invalidate means "the panel is wrong here", not "I drew here."** Content that moves
//!   leaves the panel wrong where it used to be as well; the layer handles that by pushing what
//!   was invalidated LAST frame too. What carries forward is what the caller invalidated last
//!   frame, never what was pushed -- pushing already merged the frame before, and carrying that
//!   makes the pushed area grow monotonically until every frame sends the whole panel.
//!
//! Regions are logical; they are mapped to physical panel coordinates through the canvas
//! transform at push time, and re-clipped against the canvas as it is NOW, so a region recorded
//! before a rotation cannot become an inverted region a driver never finishes.
//!
//! One update is in flight per display at a time; a frame's disjoint regions are queued and
//! fed to the display one by one from `poll`, so a frame with content at opposite edges sends
//! two small updates rather than one tall narrow union -- the shape drivers chunk a row at a
//! time, at a poll per row.

use heapless::Vec;

use crate::display::{Display, DisplayDriver, Region, UpdateError};
use light_draw::{Canvas, Flip, PixelFormat, Rotation, Transform};

/// A logical region with room to fall off the canvas: content near an edge extends past it,
/// and clipping needs to see that as negative rather than wrapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogicalRegion {
        pub x0: i32,
        pub y0: i32,
        pub x1: i32,
        pub y1: i32,
}

impl LogicalRegion {
        pub const fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
                Self { x0, y0, x1, y1 }
        }

        fn normalised(self) -> Self {
                Self { x0: self.x0.min(self.x1), y0: self.y0.min(self.y1), x1: self.x0.max(self.x1), y1: self.y0.max(self.y1) }
        }

        fn clipped(self, width: u16, height: u16) -> Option<Self> {
                let r = Self { x0: self.x0.max(0), y0: self.y0.max(0), x1: self.x1.min(i32::from(width) - 1), y1: self.y1.min(i32::from(height) - 1) };
                if r.x0 > r.x1 || r.y0 > r.y1 { None } else { Some(r) }
        }

        fn overlaps(&self, o: &Self) -> bool {
                // inclusive: sharing a column is overlapping, not abutting
                self.x1 >= o.x0 && self.x0 <= o.x1 && self.y1 >= o.y0 && self.y0 <= o.y1
        }

        fn union(&self, o: &Self) -> Self {
                Self { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
        }
}

impl From<Region> for LogicalRegion {
        fn from(r: Region) -> Self {
                Self::new(i32::from(r.x0), i32::from(r.y0), i32::from(r.x1), i32::from(r.y1))
        }
}

/// Merge `r` into every region of `list` it touches, restarting after each merge because one
/// addition can bridge two regions that were disjoint. `false` when the list is full.
fn add_region<const N: usize>(list: &mut Vec<LogicalRegion, N>, mut r: LogicalRegion) -> bool {
        loop {
                match list.iter().position(|e| e.overlaps(&r)) {
                        Some(i) => {
                                r = r.union(&list[i]);
                                list.swap_remove(i);
                        }
                        None => break,
                }
        }
        list.push(r).is_ok()
}

/// How many disjoint regions a frame tracks before collapsing to the whole canvas.
pub const MAX_REGIONS: usize = 8;

pub struct FrameLayer {
        width: u16,
        height: u16,
        format: PixelFormat,
        rotation: Rotation,
        flip: Flip,
        pub bg: u16,
        frame_interval_us: u64,
        next_frame_us: u64,
        frames: u32,
        regions: Vec<LogicalRegion, MAX_REGIONS>,
        carried: Vec<LogicalRegion, MAX_REGIONS>,
        /// Physical regions waiting for the display, fed one per update from `poll`.
        pending: Vec<Region, MAX_REGIONS>,
        frame_open: bool,
        /// Frames refused because the display was still busy with the last one.
        pub skipped: u32,
        /// Never clear the canvas at `frame_begin`: every frame draws OVER the previous one.
        /// For a single-buffered scanned panel, where a cleared live buffer flashes black
        /// under the beam before the repaint reaches it. The paint must then cover every
        /// pixel it owns -- the toolkit's windows fill their interiors for exactly this.
        pub draw_over: bool,
}

impl FrameLayer {
        /// `width`/`height` are the PHYSICAL panel dimensions; the logical canvas follows the
        /// rotation. `const`, so a layer can be a `static` initialised in place: it carries three
        /// region lists, and building one on the stack to move into a static is exactly the
        /// temporary that overran core 0's 4 KB stack on the RP2350.
        pub const fn new(width: u16, height: u16, format: PixelFormat) -> Self {
                Self {
                        width,
                        height,
                        format,
                        rotation: Rotation::R0,
                        flip: Flip::None,
                        bg: 0,
                        frame_interval_us: 0,
                        next_frame_us: 0,
                        frames: 0,
                        regions: Vec::new(),
                        carried: Vec::new(),
                        pending: Vec::new(),
                        frame_open: false,
                        skipped: 0,
                        draw_over: false,
                }
        }

        /// Frames per second; 0 leaves the layer unpaced (a frame whenever the display is free).
        pub fn set_frame_rate(&mut self, fps: u32) {
                self.frame_interval_us = if fps == 0 { 0 } else { 1_000_000 / u64::from(fps) };
        }

        /// Changing the geometry discards every region -- one recorded in the old coordinate
        /// space describes nothing in the new one -- and the caller must `invalidate_all`.
        pub fn set_orientation(&mut self, rotation: Rotation, flip: Flip) {
                self.rotation = rotation;
                self.flip = flip;
                self.regions.clear();
                self.carried.clear();
        }

        /// The logical-to-physical transform in force, for callers mapping directions or
        /// points between the panel's frame and the canvas's.
        pub fn transform(&self) -> Transform {
                Transform::for_canvas(self.rotation, self.flip, self.width, self.height)
        }

        /// A physical (panel) point into logical coordinates -- where a touch landed on the
        /// canvas as drawn.
        pub fn untransform_point(&self, phys_x: i32, phys_y: i32) -> light_draw::Point {
                let m = self.transform();
                let det = m.a * m.d - m.b * m.c;
                let px = phys_x - m.tx;
                let py = phys_y - m.ty;
                let (w, h) = self.logical_size();
                light_draw::Point::new(((m.d * px - m.b * py) * det).clamp(0, i32::from(w) - 1), ((m.a * py - m.c * px) * det).clamp(0, i32::from(h) - 1))
        }

        pub fn rotation(&self) -> Rotation {
                self.rotation
        }

        /// The panel's own dimensions, which the blits work in.
        pub fn physical_size(&self) -> (u16, u16) {
                (self.width, self.height)
        }

        pub fn logical_size(&self) -> (u16, u16) {
                match self.rotation {
                        Rotation::R90 | Rotation::R270 => (self.height, self.width),
                        _ => (self.width, self.height),
                }
        }

        pub fn frames(&self) -> u32 {
                self.frames
        }

        /// A canvas over `buf` in this layer's orientation and colours.
        pub fn canvas<'a>(&self, buf: &'a mut [u8]) -> Canvas<'a> {
                let mut c = Canvas::new(buf, self.format, self.width, self.height);
                c.set_rotation(self.rotation);
                c.set_flip(self.flip);
                c.bg = self.bg;
                c.fg = if self.format == PixelFormat::Mono1 { 1 } else { 0xFFFF };
                c
        }

        /// Whether anything is still on its way to the panel.
        pub fn busy<D: DisplayDriver>(&self, display: &Display<'_, D>) -> bool {
                display.busy() || !self.pending.is_empty()
        }

        /// Drive the display and feed it the next queued region when it is free. Call every
        /// pass. Returns `true` while work is in flight.
        pub fn poll<D: DisplayDriver>(&mut self, display: &mut Display<'_, D>) -> Result<bool, UpdateError> {
                let busy = display.poll()?;
                if busy {
                        return Ok(true);
                }
                if self.pending.is_empty() {
                        return Ok(false);
                }
                let next = self.pending.remove(0);
                display.update_async(next)?;
                Ok(true)
        }

        /// Open a frame if the deadline has passed and the display can take one, handing back a
        /// cleared canvas to draw on. `None` means draw nothing this pass and try again: that is
        /// how a display that cannot keep up skips frames instead of tearing.
        pub fn frame_begin<'d, D: DisplayDriver>(&mut self, display: &'d mut Display<'_, D>, now_us: u64) -> Option<Canvas<'d>> {
                self.frame_open_common(display, now_us, true)
        }

        /// As [`frame_begin`](Self::frame_begin), but the canvas is NOT cleared: drawing lands
        /// over whatever the buffer already holds. This is how a single-buffered scanned panel
        /// animates -- the previous image survives in the live buffer wherever the frame does
        /// not overdraw it, standing in for the capture there is no RAM to keep.
        pub fn frame_begin_over<'d, D: DisplayDriver>(&mut self, display: &'d mut Display<'_, D>, now_us: u64) -> Option<Canvas<'d>> {
                self.frame_open_common(display, now_us, false)
        }

        fn frame_open_common<'d, D: DisplayDriver>(&mut self, display: &'d mut Display<'_, D>, now_us: u64, clear: bool) -> Option<Canvas<'d>> {
                if self.frame_open {
                        return None;
                }
                if self.frame_interval_us != 0 && now_us < self.next_frame_us {
                        return None;
                }
                if self.busy(display) {
                        //   the display is still pushing the last frame. a frame is only
                        // counted as skipped once a whole interval has been lost to that --
                        // the deadline moves on by one period, so the count is of frames that
                        // never happened, not of polls that found the panel busy
                        if self.frame_interval_us != 0 && now_us >= self.next_frame_us + self.frame_interval_us {
                                self.next_frame_us += self.frame_interval_us;
                                self.skipped += 1;
                        }
                        return None;
                }
                // set forward from now rather than accumulated, so a stall does not leave a
                // backlog of deadlines to burn through
                self.next_frame_us = now_us + self.frame_interval_us;
                self.frames += 1;
                self.frame_open = true;
                let buf = display.frame_mut()?;
                let mut c = self.canvas(buf);
                if clear && !self.draw_over {
                        c.clear();
                }
                Some(c)
        }

        /// Mark a logical region of the PANEL as wrong. Corners in any order; clipped on the way
        /// in; a list too fragmented to track collapses to the whole canvas, which can only push
        /// more than needed, never less.
        pub fn invalidate(&mut self, r: LogicalRegion) {
                let (w, h) = self.logical_size();
                let Some(r) = r.normalised().clipped(w, h) else { return };
                if !add_region(&mut self.regions, r) {
                        self.invalidate_all();
                }
        }

        pub fn invalidate_all(&mut self) {
                let (w, h) = self.logical_size();
                self.regions.clear();
                let _ = self.regions.push(LogicalRegion::new(0, 0, i32::from(w) - 1, i32::from(h) - 1));
        }

        /// Close the frame: under double buffering, swap so the buffer just drawn is the one
        /// updates read; then queue this frame's regions plus last frame's -- merged where
        /// they overlap, separate where they do not -- as physical updates, and carry only
        /// what the caller invalidated this frame.
        ///
        /// The swap is HERE, not at the next `frame_begin`: swapping late meant every push read
        /// the previous frame, and a moving square left its old positions on the panel as a
        /// trail while the buffer was correct all along.
        pub fn frame_end<D: DisplayDriver>(&mut self, display: &mut Display<'_, D>) {
                if !self.frame_open {
                        return;
                }
                self.frame_open = false;
                // cannot fail: frame_begin required nothing in flight, and nothing starts an
                // update between begin and end
                let _ = display.swap();
                let (w, h) = self.logical_size();
                let mut push: Vec<LogicalRegion, MAX_REGIONS> = Vec::new();
                let mut fits = true;
                for r in self.regions.iter().chain(self.carried.iter()) {
                        if let Some(r) = r.clipped(w, h) {
                                fits &= add_region(&mut push, r);
                        }
                }
                if !fits {
                        push.clear();
                        let _ = push.push(LogicalRegion::new(0, 0, i32::from(w) - 1, i32::from(h) - 1));
                }
                // mapped to the panel through the same transform the canvas draws through
                let transform = Transform::for_canvas(self.rotation, self.flip, self.width, self.height);
                for r in push.iter() {
                        let phys = transform.rect(&Region::new(r.x0 as u16, r.y0 as u16, r.x1 as u16, r.y1 as u16));
                        if self.pending.push(phys).is_err() {
                                // more than the queue holds: the whole panel once instead
                                self.pending.clear();
                                let _ = self.pending.push(Region::full(self.width, self.height));
                                break;
                        }
                }
                self.carried.clear();
                for r in self.regions.iter() {
                        let _ = self.carried.push(*r);
                }
                self.regions.clear();
        }

        /// The physical regions queued but not yet started, for tests and diagnostics.
        pub fn pending(&self) -> &[Region] {
                &self.pending
        }

        /// Where a logical region lands on the PANEL, through the same transform the canvas
        /// draws through -- for a caller scheduling work against physical scan position (a
        /// scanned panel's beam). `None` when the region misses the canvas entirely.
        pub fn to_physical(&self, r: LogicalRegion) -> Option<Region> {
                let (w, h) = self.logical_size();
                let r = r.normalised().clipped(w, h)?;
                let transform = Transform::for_canvas(self.rotation, self.flip, self.width, self.height);
                Some(transform.rect(&Region::new(r.x0 as u16, r.y0 as u16, r.x1 as u16, r.y1 as u16)))
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use crate::display::{DisplayDriver, Frame};
        use light_core::hal::Clock;
        extern crate std;
        use core::sync::atomic::{AtomicU64, Ordering};
        use std::vec::Vec as StdVec;

        static NOW: AtomicU64 = AtomicU64::new(0);
        fn now() -> u64 {
                NOW.load(Ordering::Relaxed)
        }

        /// Completes every chunk on the first check; records every region it was asked to push.
        struct Mock {
                pushed: StdVec<Region>,
        }
        impl DisplayDriver for Mock {
                fn init(&mut self, _: &mut dyn Clock, _: u16, _: u16) {}
                fn chunk_count(&self, _: &Region) -> u16 {
                        1
                }
                fn chunks_per_poll(&self, _: &Region) -> u16 {
                        0
                }
                fn kick(&mut self, _: &Frame<'_>, r: &Region, _: u16) {
                        self.pushed.push(*r);
                }
                fn chunk_complete(&mut self) -> bool {
                        true
                }
                fn chunk_timeout_ms(&self) -> u32 {
                        10
                }
        }

        fn rig<'b>(buf: &'b mut [u8]) -> (FrameLayer, Display<'b, Mock>) {
                let display = Display::new(Mock { pushed: StdVec::new() }, buf, 16, 8, PixelFormat::Mono1, now);
                (FrameLayer::new(16, 8, PixelFormat::Mono1), display)
        }

        /// Run frames of the layer's protocol until the display is idle; returns what was pushed.
        fn flush(layer: &mut FrameLayer, display: &mut Display<'_, Mock>) -> StdVec<Region> {
                while layer.poll(display).unwrap() {}
                core::mem::take(&mut display.driver().pushed)
        }

        #[test]
        fn a_moving_box_pushes_where_it_was_and_where_it_is_and_no_further_back() {
                let mut buf = [0u8; 16];
                let (mut layer, mut display) = rig(&mut buf);
                let frame = |layer: &mut FrameLayer, display: &mut Display<'_, Mock>, x: i32| {
                        let c = layer.frame_begin(display, now()).expect("frame");
                        drop(c);
                        layer.invalidate(LogicalRegion::new(x, 0, x + 1, 1));
                        layer.frame_end(display);
                        flush(layer, display)
                };
                assert_eq!(frame(&mut layer, &mut display, 0), [Region::new(0, 0, 1, 1)]);
                //   the box moved to x=4: last frame's place and this frame's, disjoint, two pushes
                let mut pushed = frame(&mut layer, &mut display, 4);
                pushed.sort_by_key(|r| r.x0);
                assert_eq!(pushed, [Region::new(0, 0, 1, 1), Region::new(4, 0, 5, 1)]);
                //   and to x=8: the x=0 position is NOT pushed again -- carrying what was pushed
                // rather than what was invalidated would have kept it forever
                let mut pushed = frame(&mut layer, &mut display, 8);
                pushed.sort_by_key(|r| r.x0);
                assert_eq!(pushed, [Region::new(4, 0, 5, 1), Region::new(8, 0, 9, 1)]);
        }

        #[test]
        fn overlapping_regions_merge_and_bridging_merges_transitively() {
                let mut buf = [0u8; 16];
                let (mut layer, mut display) = rig(&mut buf);
                let c = layer.frame_begin(&mut display, now()).unwrap();
                drop(c);
                layer.invalidate(LogicalRegion::new(0, 0, 2, 2));
                layer.invalidate(LogicalRegion::new(6, 0, 8, 2));
                //   the bridge touches both: one region results, not three
                layer.invalidate(LogicalRegion::new(2, 1, 6, 1));
                layer.frame_end(&mut display);
                assert_eq!(flush(&mut layer, &mut display), [Region::new(0, 0, 8, 2)]);
        }

        #[test]
        fn regions_off_the_canvas_are_clipped_and_reversed_corners_accepted() {
                let mut buf = [0u8; 16];
                let (mut layer, mut display) = rig(&mut buf);
                let c = layer.frame_begin(&mut display, now()).unwrap();
                drop(c);
                layer.invalidate(LogicalRegion::new(16, 3, 14, 1));
                layer.invalidate(LogicalRegion::new(-5, -5, 1, 1));
                layer.invalidate(LogicalRegion::new(30, 30, 40, 40));
                layer.frame_end(&mut display);
                let mut pushed = flush(&mut layer, &mut display);
                pushed.sort_by_key(|r| r.x0);
                assert_eq!(pushed, [Region::new(0, 0, 1, 1), Region::new(14, 1, 15, 3)]);
        }

        #[test]
        fn too_many_regions_collapse_to_the_whole_canvas() {
                let mut buf = [0u8; 16];
                let (mut layer, mut display) = rig(&mut buf);
                let c = layer.frame_begin(&mut display, now()).unwrap();
                drop(c);
                //   eight disjoint pixels fill the list exactly; the ninth cannot be tracked
                for x in (0..16).step_by(2) {
                        layer.invalidate(LogicalRegion::new(x, 0, x, 0));
                }
                layer.invalidate(LogicalRegion::new(0, 4, 0, 4));
                layer.frame_end(&mut display);
                assert_eq!(flush(&mut layer, &mut display), [Region::full(16, 8)]);
        }

        #[test]
        fn a_rotation_maps_regions_to_physical_columns_and_forgets_old_ones() {
                let mut buf = [0u8; 16];
                let (mut layer, mut display) = rig(&mut buf);
                let c = layer.frame_begin(&mut display, now()).unwrap();
                drop(c);
                layer.invalidate(LogicalRegion::new(0, 0, 3, 1));
                layer.frame_end(&mut display);
                let _ = flush(&mut layer, &mut display);
                // logical 8x16 now; the carried region from the old space must not survive
                layer.set_orientation(Rotation::R90, Flip::None);
                assert_eq!(layer.logical_size(), (8, 16));
                let c = layer.frame_begin(&mut display, now()).unwrap();
                drop(c);
                layer.invalidate(LogicalRegion::new(0, 0, 1, 3));
                layer.frame_end(&mut display);
                //   logical top-left under R90 is the physical top-right: x 12..15, y 0..1
                assert_eq!(flush(&mut layer, &mut display), [Region::new(12, 0, 15, 1)]);
        }

        #[test]
        fn pacing_and_a_busy_display_skip_frames_without_losing_regions() {
                let mut buf = [0u8; 16];
                let (mut layer, mut display) = rig(&mut buf);
                layer.set_frame_rate(10);
                NOW.store(0, Ordering::Relaxed);
                assert!(layer.frame_begin(&mut display, 0).is_some());
                layer.invalidate(LogicalRegion::new(0, 0, 0, 0));
                layer.frame_end(&mut display);
                //   pending, not yet started: a new frame is refused for being busy, and once a
                // whole interval has gone by that way it counts as a skipped frame -- once
                assert!(layer.busy(&display));
                assert!(layer.frame_begin(&mut display, 150_000).is_none());
                assert_eq!(layer.skipped, 0, "late, but not yet a whole frame late");
                assert!(layer.frame_begin(&mut display, 200_000).is_none());
                assert!(layer.frame_begin(&mut display, 210_000).is_none());
                assert_eq!(layer.skipped, 1);
                let _ = flush(&mut layer, &mut display);
                //   ...and for the deadline: 10 fps is a frame every 100 ms from the last
                assert!(layer.frame_begin(&mut display, 50_000).is_none());
                assert!(layer.frame_begin(&mut display, 200_000).is_some());
                layer.frame_end(&mut display);
                assert_eq!(layer.frames(), 2);
        }

        #[test]
        fn double_buffering_pushes_the_frame_just_drawn_not_the_one_before() {
                let mut front = [0u8; 16];
                let mut back = [0u8; 16];
                let (mut layer, mut display) = rig(&mut front);
                display.set_back_buffer(&mut back);
                let mut c = layer.frame_begin(&mut display, now()).unwrap();
                c.set(0, 0, 1);
                drop(c);
                layer.invalidate(LogicalRegion::new(0, 0, 0, 0));
                layer.frame_end(&mut display);
                //   closing the frame is what makes the drawn buffer the front: the pushes
                // queued by frame_end read THIS frame. Swapping at the next begin instead sent
                // every push the previous frame's pixels, and a moving square left a trail.
                assert_eq!(display.front().unwrap()[0], 1, "the frame just drawn is what the panel gets");
                let _ = flush(&mut layer, &mut display);
                //   the next frame draws into the other buffer, cleared: the pixel is gone
                // from the canvas until it is drawn again, which is the full-repaint contract
                let c = layer.frame_begin(&mut display, now()).unwrap();
                assert_eq!(c.get(0, 0), 0);
                drop(c);
                layer.frame_end(&mut display);
                assert_eq!(display.front().unwrap()[0], 0);
        }
}
