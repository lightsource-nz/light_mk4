//! Board wiring for the po13 rig: a Raspberry Pi Pico -- the RP2040 original or the Pico 2,
//! which are pin-compatible, so this wiring serves whichever chip the tree is built for --
//! wearing the Waveshare Pico-OLED-1.3 (SH1107, 64x128 portrait glass, 1 bpp) on SPI1, pins
//! from mk3's `light_display_po13.h`.
//!
//! This lives with the APPLICATION, not in the port crate: `light-rp2` knows the chip and
//! nothing about what anyone soldered to it. This particular bench setup is this demo's, not
//! the framework's -- another user's Pico wears something else and writes their own forty
//! lines of wiring.

use light_core::atomic::{AtomicBool, Ordering};
use light_rp2::gpio::{Input, Output};
use light_rp2::spi::Spi1Display;
use light_rp2::Clocks;

pub const PIN_LED: usize = 25;
/// The Pico-OLED-1.3's two keys, active low. KEY1 shares its pin with the secondary
/// display's chip select in mk3's two-display rig; here there is one display.
pub const PIN_KEY0: usize = 15;
pub const PIN_KEY1: usize = 17;

pub const PIN_OLED_DC: usize = 8;
pub const PIN_OLED_CS: usize = 9;
pub const PIN_OLED_SCK: usize = 10;
pub const PIN_OLED_MOSI: usize = 11;
pub const PIN_OLED_RESET: usize = 12;
/// The glass is physically portrait: 64 wide, 128 tall (mk3 chased a sideways photo of
/// this board for a while before establishing that on the device).
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
        // SAFETY: the flag above makes this the one construction of each peripheral
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
