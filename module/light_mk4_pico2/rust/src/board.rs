//! Board wiring for this demo: a Raspberry Pi Pico -- the RP2040 original or the Pico 2,
//! which are pin-compatible, so this wiring serves whichever chip the tree is built for --
//! wearing the Waveshare Pico-OLED-1.3 (SH1107, 64x128 portrait glass, 1 bpp) on SPI1, the
//! Pico-OLED-1.3's pins as it plugs onto the Pico header.
//!
//! This lives with the APPLICATION, not in the port crate: `light-rp2` knows the chip and
//! nothing about what anyone soldered to it. This particular hardware pairing is this demo's,
//! not the framework's -- another user's Pico wears something else and writes their own forty
//! lines of wiring.

use light_core::atomic::{AtomicBool, Ordering};
use light_rp2::gpio::{Input, Output};
use light_rp2::spi::Spi1Display;
use light_rp2::Clocks;

pub const PIN_LED: usize = 25;
/// The Pico-OLED-1.3's two keys, active low. KEY1's pin can double as a second
/// display's chip select in a two-display setup; here there is one display.
pub const PIN_KEY0: usize = 15;
pub const PIN_KEY1: usize = 17;

pub const PIN_OLED_DC: usize = 8;
pub const PIN_OLED_CS: usize = 9;
pub const PIN_OLED_SCK: usize = 10;
pub const PIN_OLED_MOSI: usize = 11;
pub const PIN_OLED_RESET: usize = 12;
/// The glass is physically portrait: 64 wide, 128 tall (easy to misread from a sideways
/// product photo; established on the device).
pub const OLED_WIDTH: u16 = 64;
pub const OLED_HEIGHT: u16 = 128;
/// The controller's RAM offset the panel sits at (0xD3), verified on the glass.
pub const OLED_DISPLAY_OFFSET: u8 = 96;
/// The verified clock for these small OLED panels; this panel was never re-clocked.
pub const OLED_SPI_HZ: u32 = 10_000_000;
/// The display bus's DMA channel: the top of the range, where pico-sdk's
/// dma_claim_unused_channel (counting up from 0) will not reach -- and the top is a CHIP
/// fact: the RP2040 has 12 channels, the RP2350 16. Channel 15 on an RP2040 panicked the
/// pac's bounds check at first light, straight into BOOTSEL with the message in RAM.
#[cfg(feature = "rp2040")]
pub const OLED_DMA_CH: usize = 11;
#[cfg(feature = "rp2350")]
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
