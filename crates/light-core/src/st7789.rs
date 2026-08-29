//! ST7789 controller driver over a 4-wire SPI display bus. Ported from mk3's
//! `light_display_st7789`, including the row offset that board found by measurement.

use crate::hal::{Clock, SpiDisplayBus};
use crate::display::{DisplayDriver, Frame, Region};

pub const CMD_SWRESET: u8 = 0x01;
pub const CMD_SLPOUT: u8 = 0x11;
pub const CMD_INVOFF: u8 = 0x20;
pub const CMD_INVON: u8 = 0x21;
pub const CMD_DISPOFF: u8 = 0x28;
pub const CMD_DISPON: u8 = 0x29;
pub const CMD_CASET: u8 = 0x2A;
pub const CMD_RASET: u8 = 0x2B;
pub const CMD_RAMWR: u8 = 0x2C;
pub const CMD_MADCTL: u8 = 0x36;
pub const CMD_COLMOD: u8 = 0x3A;
pub const COLMOD_16BPP: u8 = 0x55;

/// Upper bound on one chunk. Worst case is bus-bound: a full 240x280x2 = 134400-byte frame at
/// 10 MHz is ~108 ms; 500 ms leaves headroom for a slower clock or contention.
const CHUNK_TIMEOUT_MS: u32 = 500;
/// Row chunks to spin through per poll when a region is chunked by row -- each is a short
/// burst, and yielding after every one would cost a whole pass to move almost nothing.
const ROW_CHUNKS_PER_POLL: u16 = 8;

pub struct St7789<B: SpiDisplayBus> {
        bus: B,
        col_offset: u16,
        row_offset: u16,
        width: u16,
        height: u16,
}

impl<B: SpiDisplayBus> St7789<B> {
        pub fn new(bus: B) -> Self {
                Self { bus, col_offset: 0, row_offset: 0, width: 0, height: 0 }
        }

        /// Where the visible glass sits in GDDRAM. The touch169's panel shows rows 20..299, so
        /// its row offset is 20 -- measured, not guessed (see mk3's board wiring for how).
        pub fn set_offset(&mut self, col: u16, row: u16) {
                self.col_offset = col;
                self.row_offset = row;
        }

        fn set_window(&mut self, r: &Region) {
                let cx0 = r.x0 + self.col_offset;
                let cx1 = r.x1 + self.col_offset;
                let cy0 = r.y0 + self.row_offset;
                let cy1 = r.y1 + self.row_offset;
                self.bus.command(CMD_CASET);
                self.bus.data(&[(cx0 >> 8) as u8, cx0 as u8, (cx1 >> 8) as u8, cx1 as u8]);
                self.bus.command(CMD_RASET);
                self.bus.data(&[(cy0 >> 8) as u8, cy0 as u8, (cy1 >> 8) as u8, cy1 as u8]);
        }

        /// Blocking clear of the whole panel, using the CURRENT offset -- so call it after
        /// `set_offset`, or the band the offset moves the window over stays uninitialised (the
        /// noise strip mk3 found at rows 280..299).
        pub fn clear(&mut self, color: u16) {
                let full = Region::full(self.width, self.height);
                self.set_window(&full);
                self.bus.command(CMD_RAMWR);
                let mut row = [0u8; 320 * 2];
                let n = (self.width as usize).min(320);
                for px in row[..n * 2].chunks_exact_mut(2) {
                        px[0] = (color >> 8) as u8;
                        px[1] = color as u8;
                }
                for _ in 0..self.height {
                        self.bus.data(&row[..n * 2]);
                }
        }

        fn full_width(&self, r: &Region) -> bool {
                r.x0 == 0 && r.x1 == self.width - 1
        }
}

impl<B: SpiDisplayBus> DisplayDriver for St7789<B> {
        fn init(&mut self, clock: &mut dyn Clock, width: u16, height: u16) {
                self.width = width;
                self.height = height;
                self.bus.reset_pulse(clock);
                self.bus.command(CMD_SWRESET);
                clock.delay_ms(150); // datasheet: >=120 ms after SWRESET
                self.bus.command(CMD_SLPOUT);
                clock.delay_ms(120); // datasheet: >=120 ms after SLPOUT
                self.bus.command(CMD_COLMOD);
                self.bus.data(&[COLMOD_16BPP]);
                self.bus.command(CMD_MADCTL);
                self.bus.data(&[0x00]);
                // this board's panel shows inverted colours with inversion off: LC polarity
                self.bus.command(CMD_INVON);
                self.bus.command(CMD_DISPON);
                clock.delay_ms(20);
        }

        fn chunk_count(&self, r: &Region) -> u16 {
                // a full-width region is one contiguous run in the frame buffer, so it is a
                // single chunk however tall; a narrower one must go a row per burst
                if self.full_width(r) { 1 } else { r.height() }
        }

        fn chunks_per_poll(&self, r: &Region) -> u16 {
                if self.full_width(r) { 0 } else { ROW_CHUNKS_PER_POLL }
        }

        fn kick(&mut self, frame: &Frame<'_>, r: &Region, index: u16) {
                // the window is armed once: RAMWR streams into it and wraps from x1 back to x0
                // on each new row by itself, so every later row is just more data
                if index == 0 {
                        self.set_window(r);
                        self.bus.command(CMD_RAMWR);
                }
                let bytes = if self.full_width(r) { frame.rows(r) } else { frame.row(r, r.y0 + index) };
                // SAFETY: `frame` borrows the display core's buffer, which refuses mutation
                // while an update is in flight, and stays alive for the core's lifetime
                unsafe { self.bus.start_data(bytes) }
        }

        fn chunk_complete(&mut self) -> bool {
                self.bus.is_complete()
        }

        fn chunk_timeout_ms(&self) -> u32 {
                CHUNK_TIMEOUT_MS
        }
}
