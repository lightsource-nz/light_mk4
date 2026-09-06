//! ST7735 controller driver over a 4-wire SPI display bus. Ported from the predecessor C
//! framework, including the init sequence and the MiniSTM32H7's landscape orientation and
//! BGR order.

use crate::display::{DisplayDriver, Frame, Region};
use light_core::hal::{Clock, SpiDisplayBus};

pub const CMD_SWRESET: u8 = 0x01;
pub const CMD_SLPOUT: u8 = 0x11;
pub const CMD_NORON: u8 = 0x13;
pub const CMD_INVOFF: u8 = 0x20;
pub const CMD_INVON: u8 = 0x21;
pub const CMD_DISPON: u8 = 0x29;
pub const CMD_CASET: u8 = 0x2A;
pub const CMD_RASET: u8 = 0x2B;
pub const CMD_RAMWR: u8 = 0x2C;
pub const CMD_MADCTL: u8 = 0x36;
pub const CMD_COLMOD: u8 = 0x3A;
pub const CMD_FRMCTR1: u8 = 0xB1;
pub const CMD_FRMCTR2: u8 = 0xB2;
pub const CMD_FRMCTR3: u8 = 0xB3;
pub const CMD_INVCTR: u8 = 0xB4;
pub const CMD_PWCTR1: u8 = 0xC0;
pub const CMD_PWCTR2: u8 = 0xC1;
pub const CMD_PWCTR3: u8 = 0xC2;
pub const CMD_PWCTR4: u8 = 0xC3;
pub const CMD_PWCTR5: u8 = 0xC4;
pub const CMD_VMCTR1: u8 = 0xC5;
pub const CMD_GMCTRP1: u8 = 0xE0;
pub const CMD_GMCTRN1: u8 = 0xE1;
pub const COLMOD_16BPP: u8 = 0x05;

pub const MADCTL_MY: u8 = 0x80;
pub const MADCTL_MX: u8 = 0x40;
pub const MADCTL_MV: u8 = 0x20;
pub const MADCTL_BGR: u8 = 0x08;
/// The MiniSTM32H7's panel as mounted: landscape, rotated 180, BGR.
pub const MADCTL_LANDSCAPE_ROT180: u8 = MADCTL_MV | MADCTL_MY | MADCTL_BGR;

const CHUNK_TIMEOUT_MS: u32 = 500;
const ROW_CHUNKS_PER_POLL: u16 = 8;

pub struct St7735<B: SpiDisplayBus> {
        bus: B,
        col_offset: u16,
        row_offset: u16,
        madctl: u8,
        width: u16,
        height: u16,
}

impl<B: SpiDisplayBus> St7735<B> {
        pub fn new(bus: B) -> Self {
                Self { bus, col_offset: 0, row_offset: 0, madctl: MADCTL_LANDSCAPE_ROT180, width: 0, height: 0 }
        }

        /// Where the visible glass sits in GDDRAM: a property of the board's glass, not of the
        /// controller. The MiniSTM32H7's 160x80 is at column 1, row 26.
        pub fn set_offset(&mut self, col: u16, row: u16) {
                self.col_offset = col;
                self.row_offset = row;
        }

        /// The memory access control byte, for a board mounted differently. Before `init`.
        pub fn set_madctl(&mut self, madctl: u8) {
                self.madctl = madctl;
        }

        fn cmd(&mut self, cmd: u8, args: &[u8]) {
                self.bus.command(cmd);
                if !args.is_empty() {
                        self.bus.data(args);
                }
        }

        fn set_window(&mut self, r: &Region) {
                let cx0 = r.x0 + self.col_offset;
                let cx1 = r.x1 + self.col_offset;
                let cy0 = r.y0 + self.row_offset;
                let cy1 = r.y1 + self.row_offset;
                self.cmd(CMD_CASET, &[(cx0 >> 8) as u8, cx0 as u8, (cx1 >> 8) as u8, cx1 as u8]);
                self.cmd(CMD_RASET, &[(cy0 >> 8) as u8, cy0 as u8, (cy1 >> 8) as u8, cy1 as u8]);
        }

        /// Blocking clear of the whole panel at the CURRENT offset -- after `set_offset`, so no
        /// band of powered-up GDDRAM outside the window is left for later updates to miss.
        pub fn clear(&mut self, color: u16) {
                let full = Region::full(self.width, self.height);
                self.set_window(&full);
                self.bus.command(CMD_RAMWR);
                let mut row = [0u8; 162 * 2];
                let n = (self.width as usize).min(162);
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

impl<B: SpiDisplayBus> DisplayDriver for St7735<B> {
        fn init(&mut self, clock: &mut dyn Clock, width: u16, height: u16) {
                self.width = width;
                self.height = height;
                self.bus.reset_pulse(clock);
                self.bus.command(CMD_SWRESET);
                clock.delay_ms(150);
                self.bus.command(CMD_SLPOUT);
                clock.delay_ms(120);
                self.cmd(CMD_FRMCTR1, &[0x01, 0x2C, 0x2D]);
                self.cmd(CMD_FRMCTR2, &[0x01, 0x2C, 0x2D]);
                self.cmd(CMD_FRMCTR3, &[0x01, 0x2C, 0x2D, 0x01, 0x2C, 0x2D]);
                self.cmd(CMD_INVCTR, &[0x07]);
                self.cmd(CMD_PWCTR1, &[0xA2, 0x02, 0x84]);
                self.cmd(CMD_PWCTR2, &[0xC5]);
                self.cmd(CMD_PWCTR3, &[0x0A, 0x00]);
                self.cmd(CMD_PWCTR4, &[0x8A, 0x2A]);
                self.cmd(CMD_PWCTR5, &[0x8A, 0xEE]);
                self.cmd(CMD_VMCTR1, &[0x0E]);
                self.cmd(CMD_GMCTRP1, &[0x02, 0x1c, 0x07, 0x12, 0x37, 0x32, 0x29, 0x2d, 0x29, 0x25, 0x2b, 0x39, 0x00, 0x01, 0x03, 0x10]);
                self.cmd(CMD_GMCTRN1, &[0x03, 0x1d, 0x07, 0x06, 0x2e, 0x2c, 0x29, 0x2d, 0x2e, 0x2e, 0x37, 0x3f, 0x00, 0x00, 0x02, 0x10]);
                self.cmd(CMD_COLMOD, &[COLMOD_16BPP]);
                let madctl = self.madctl;
                self.cmd(CMD_MADCTL, &[madctl]);
                self.bus.command(CMD_INVON);
                self.bus.command(CMD_NORON);
                clock.delay_ms(10);
                self.bus.command(CMD_DISPON);
                clock.delay_ms(20);
        }

        fn chunk_count(&self, r: &Region) -> u16 {
                if self.full_width(r) { 1 } else { r.height() }
        }

        fn chunks_per_poll(&self, r: &Region) -> u16 {
                if self.full_width(r) { 0 } else { ROW_CHUNKS_PER_POLL }
        }

        fn kick(&mut self, frame: &Frame<'_>, r: &Region, index: u16) {
                if index == 0 {
                        self.set_window(r);
                        self.bus.command(CMD_RAMWR);
                }
                let bytes = if self.full_width(r) { frame.rows(r) } else { frame.row(r, r.y0 + index) };
                // SAFETY: as for the ST7789 -- the display core keeps the frame alive and
                // unmodified while an update is in flight
                unsafe { self.bus.start_data(bytes) }
        }

        fn chunk_complete(&mut self) -> bool {
                self.bus.is_complete()
        }

        fn chunk_timeout_ms(&self) -> u32 {
                CHUNK_TIMEOUT_MS
        }
}
