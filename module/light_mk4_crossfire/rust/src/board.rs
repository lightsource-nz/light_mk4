//! Board wiring for crossfire's Pico: the LED, and -- when the Pico sits in the po13 dock --
//! the Waveshare Pico-OLED-1.3 (SH1107, 64x128 portrait glass, 1 bpp) on SPI1 as the status
//! display, pins from mk3's `light_display_po13.h`. The product board is a stock Pico; the
//! OLED and keys are the bench rig's.
//!
//! This lives with the APPLICATION, not in the port crate: `light-rp2` knows the chip and
//! nothing about what anyone soldered to it. A crossfire built for different status hardware
//! changes this file and nothing else.

use light_core::atomic::{AtomicBool, Ordering};
use light_rp2::gpio::{Input, Output};
use light_rp2::spi::Spi1Display;
use light_rp2::Clocks;

pub const PIN_LED: usize = 25;
/// The Pico-OLED-1.3's two keys, active low.
pub const PIN_KEY0: usize = 15;
pub const PIN_KEY1: usize = 17;

pub const PIN_OLED_DC: usize = 8;
pub const PIN_OLED_CS: usize = 9;
pub const PIN_OLED_SCK: usize = 10;
pub const PIN_OLED_MOSI: usize = 11;
pub const PIN_OLED_RESET: usize = 12;
/// The glass is physically portrait: 64 wide, 128 tall.
pub const OLED_WIDTH: u16 = 64;
pub const OLED_HEIGHT: u16 = 128;
/// The controller's RAM offset the panel sits at (0xD3), mk3's verified value.
pub const OLED_DISPLAY_OFFSET: u8 = 96;
/// mk3's `SPI_BAUDRATE` for the OLED rigs; this panel was never re-clocked.
pub const OLED_SPI_HZ: u32 = 10_000_000;
pub const OLED_DMA_CH: usize = 15;

pub struct Peripherals {
        pub led: Output,
        pub key0: Input,
        pub key1: Input,
        pub oled_bus: Spi1Display,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// Configure and hand over the board's peripherals. Once.
pub fn take(clocks: &Clocks) -> Option<Peripherals> {
        if TAKEN.swap(true, Ordering::AcqRel) {
                return None;
        }
        // SAFETY: the flag above makes this the one construction of each peripheral;
        // the shell's USB and timer blocks are not in this set
        unsafe {
                Some(Peripherals {
                        led: Output::new(PIN_LED, false),
                        key0: Input::new_pull_up(PIN_KEY0),
                        key1: Input::new_pull_up(PIN_KEY1),
                        oled_bus: Spi1Display::new(
                                clocks.peri_hz,
                                PIN_OLED_SCK,
                                PIN_OLED_MOSI,
                                PIN_OLED_CS,
                                PIN_OLED_DC,
                                Some(PIN_OLED_RESET),
                                OLED_SPI_HZ,
                                OLED_DMA_CH,
                        ),
                })
        }
}
