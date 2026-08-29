//! SH1107 OLED controller over a 4-wire SPI display bus, ported from mk3's
//! `light_display_sh1107` with its addressing lessons: vertical addressing mode, one column per
//! chunk, the page address re-armed per column and the column address always sent as both
//! nibbles, low first. The source buffer is `PixelFormat::Mono1` (row-major, leftmost pixel in
//! bit 0); the controller wants column-major bytes of eight vertically stacked pixels, so each
//! output byte is assembled bit by bit.

use crate::display::{DisplayDriver, Frame, Region};
use crate::hal::{Clock, SpiDisplayBus};

pub const CMD_SET_COL_ADDR_LOW: u8 = 0x00;
pub const CMD_SET_COL_ADDR_HIGH: u8 = 0x10;
pub const CMD_SET_ADDRMODE: u8 = 0x20;
pub const CMD_SET_CONTRAST: u8 = 0x81;
pub const CMD_SET_SEG_REMAP: u8 = 0xA0;
pub const CMD_SET_MUX_RATIO: u8 = 0xA8;
pub const CMD_SET_FORCE_ON: u8 = 0xA4;
pub const CMD_SET_REVERSE: u8 = 0xA6;
pub const CMD_SET_DISPLAY_OFFSET: u8 = 0xD3;
pub const CMD_SET_POWER_MODE: u8 = 0xAD;
pub const CMD_SET_DISPLAY_ON: u8 = 0xAE;
pub const CMD_SET_PAGE_ADDR: u8 = 0xB0;
pub const CMD_SET_SCAN_DIR: u8 = 0xC0;
pub const CMD_SET_DISPLAY_CLK: u8 = 0xD5;
pub const CMD_SET_CHARGE_PERIODS: u8 = 0xD9;
pub const CMD_SET_VCOMH: u8 = 0xDB;
pub const CMD_SET_DISPLAY_START: u8 = 0xDC;

const ADDRMODE_VERTICAL: u8 = 1;
/// GDDRAM depth: a hardware constant, sizing the per-column burst buffer.
const MAX_PAGES: usize = 16;
/// One column is tens of microseconds of bus time; yielding after each would spend a whole
/// pass moving a few bytes. Eight per poll keeps a 128-column sweep to 16 polls.
const CHUNKS_PER_POLL: u16 = 8;
/// A 128-column panel is a little over 1200 bytes including commands: well under 20 ms even
/// at a slow clock. 50 ms per chunk is generous.
const CHUNK_TIMEOUT_MS: u32 = 50;

pub struct Sh1107<B: SpiDisplayBus> {
        bus: B,
        n_pages: usize,
        n_columns: u16,
        /// The pixel offset the panel's glass sits at in the controller's RAM (0xD3).
        display_offset: u8,
        /// The column burst being sent, in this struct because DMA reads it after `kick` returns.
        page_buf: [u8; MAX_PAGES],
        region: Region,
}

impl<B: SpiDisplayBus> Sh1107<B> {
        pub fn new(bus: B) -> Self {
                Self { bus, n_pages: 0, n_columns: 0, display_offset: 0, page_buf: [0; MAX_PAGES], region: Region::new(0, 0, 0, 0) }
        }

        /// The Pico-OLED-1.3 sits at offset 96 (mk3's setting, verified on the panel).
        pub fn set_display_offset(&mut self, offset: u8) {
                self.display_offset = offset;
        }

        fn set_column(&mut self, column: u8) {
                // both nibbles, low first, unconditionally -- what the reference driver does
                self.bus.command(CMD_SET_COL_ADDR_LOW + (column & 0x0F));
                self.bus.command(CMD_SET_COL_ADDR_HIGH + (column >> 4));
        }

        fn cmd2(&mut self, a: u8, b: u8) {
                self.bus.command(a);
                self.bus.command(b);
        }

        /// Blocking clear of the whole panel.
        pub fn clear(&mut self, on: bool) {
                let fill = if on { 0xFF } else { 0x00 };
                let n = self.n_pages;
                self.page_buf[..n].fill(fill);
                let buf = self.page_buf;
                for column in 0..self.n_columns {
                        self.set_column(column as u8);
                        self.bus.command(CMD_SET_PAGE_ADDR);
                        self.bus.data(&buf[..n]);
                }
        }
}

impl<B: SpiDisplayBus> DisplayDriver for Sh1107<B> {
        fn init(&mut self, clock: &mut dyn Clock, width: u16, height: u16) {
                // physical column = buffer x; page = group of 8 buffer rows. width/height are
                // the panel's real orientation (the po13 is 64 wide x 128 tall)
                self.n_columns = width;
                self.n_pages = (height as usize).div_ceil(8).min(MAX_PAGES);
                self.bus.reset_pulse(clock);
                self.bus.command(CMD_SET_DISPLAY_ON); // off
                self.set_column(0);
                self.bus.command(CMD_SET_PAGE_ADDR);
                self.cmd2(CMD_SET_DISPLAY_START, 0);
                self.cmd2(CMD_SET_CONTRAST, 128);
                self.bus.command(CMD_SET_ADDRMODE + ADDRMODE_VERTICAL);
                self.bus.command(CMD_SET_SEG_REMAP); // off
                self.bus.command(CMD_SET_SCAN_DIR); // down
                self.bus.command(CMD_SET_FORCE_ON); // off
                self.bus.command(CMD_SET_REVERSE); // off
                self.cmd2(CMD_SET_MUX_RATIO, 63); // 1:64, this panel has 64 COM lines
                self.cmd2(CMD_SET_DISPLAY_OFFSET, self.display_offset);
                self.cmd2(CMD_SET_DISPLAY_CLK, (4 << 4) | 1);
                self.cmd2(CMD_SET_CHARGE_PERIODS, 2 | (2 << 4));
                self.cmd2(CMD_SET_VCOMH, 0x35);
                self.cmd2(CMD_SET_POWER_MODE, 0x80 | (5 << 1)); // built-in DC-DC off
                self.bus.command(CMD_SET_DISPLAY_ON + 1);
        }

        /// The region's x extent is the column range; its y extent is ignored, because every
        /// column burst covers the full page depth. Column granularity, rounding y outward.
        fn chunk_count(&self, r: &Region) -> u16 {
                if r.x0 >= self.n_columns {
                        return 0;
                }
                r.x1.min(self.n_columns - 1) - r.x0 + 1
        }

        fn chunks_per_poll(&self, _: &Region) -> u16 {
                CHUNKS_PER_POLL
        }

        fn kick(&mut self, frame: &Frame<'_>, r: &Region, index: u16) {
                if index == 0 {
                        self.region = *r;
                }
                let column = r.x0 + index;
                self.set_column(column as u8);
                let stride = frame.stride;
                for page in 0..self.n_pages {
                        let mut out = 0u8;
                        for bit in 0..8u16 {
                                let y = page as u16 * 8 + bit;
                                if y >= frame.height {
                                        break;
                                }
                                let src = frame.buf[y as usize * stride + column as usize / 8];
                                if src & (1 << (column % 8)) != 0 {
                                        out |= 1 << bit;
                                }
                        }
                        self.page_buf[page] = out;
                }
                self.bus.command(CMD_SET_PAGE_ADDR);
                let n = self.n_pages;
                // SAFETY: page_buf lives as long as this driver, and the display core does not
                // call kick again until chunk_complete has reported this burst landed
                unsafe { self.bus.start_data(&self.page_buf[..n]) }
        }

        fn chunk_complete(&mut self) -> bool {
                self.bus.is_complete()
        }

        fn chunk_timeout_ms(&self) -> u32 {
                CHUNK_TIMEOUT_MS
        }
}
