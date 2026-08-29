//! The boards on the bench: pins from their schematics, peripherals as owned sets.
//!
//! A board module is the ONLY place that knows which pin is what. `take()` configures every
//! peripheral the board wires up and hands them over once; the application receives owned
//! values and passes each to the driver that needs it, so two drivers cannot share a bus by
//! accident and nothing can reach a peripheral the board did not wire.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::gpio::{Input, Output};
use crate::i2c::I2c1;
use crate::spi::Spi1Display;
use crate::Clocks;

/// The Waveshare RP2350-Touch-LCD-1.69: pins from the board schematic, as recorded in mk3's
/// `light_ui_hw_ws_touch169.h` and confirmed on hardware there.
pub mod touch169 {
        use super::*;

        pub const PIN_DISPLAY_DC: usize = 8;
        pub const PIN_DISPLAY_CS: usize = 9;
        pub const PIN_DISPLAY_SCK: usize = 10;
        pub const PIN_DISPLAY_MOSI: usize = 11;
        pub const PIN_DISPLAY_RESET: usize = 13;
        pub const PIN_DISPLAY_BL: usize = 25;
        pub const DISPLAY_WIDTH: u16 = 240;
        pub const DISPLAY_HEIGHT: u16 = 280;
        /// The visible glass is GDDRAM rows 20..299 -- measured (mk3 board wiring).
        pub const DISPLAY_ROW_OFFSET: u16 = 20;
        /// 40 MHz confirmed clean on hardware; 10 MHz would cap a full frame at 9.3 fps.
        ///
        /// Tried at 10 MHz on 2026-08-29 to test whether the touch controller's wedges (I2C on
        /// pins 6/7 timing out under continuous rendering) track the SPI clock on 10/11: 17
        /// clean taps then a wedge, against wedges every 4-8 taps at 40 MHz. Suggestive, not
        /// decisive -- one run each. Left at 40 MHz, the clock mk3 verified the panel at.
        pub const DISPLAY_SPI_HZ: u32 = 40_000_000;

        pub const PIN_TOUCH_SDA: usize = 6;
        pub const PIN_TOUCH_SCL: usize = 7;
        pub const PIN_TOUCH_INT: usize = 21;
        pub const PIN_TOUCH_RST: usize = 22;
        pub const TOUCH_I2C_HZ: u32 = 300_000;

        /// DMA channel for the display bus: see `Spi1Display` for why the top of the range.
        pub const DISPLAY_DMA_CH: usize = 15;

        pub struct Peripherals {
                pub display_bus: Spi1Display,
                pub backlight: Output,
                pub touch_bus: I2c1,
                pub touch_int: Input,
                pub touch_reset: Output,
        }

        static TAKEN: AtomicBool = AtomicBool::new(false);

        /// Configure and hand over the board's peripherals. Once.
        pub fn take(clocks: &Clocks) -> Option<Peripherals> {
                if TAKEN.swap(true, Ordering::AcqRel) {
                        return None;
                }
                // SAFETY: the flag above makes this the one construction of each peripheral;
                // the shell uses none of them (its USB and timer blocks are not in this set)
                unsafe {
                        Some(Peripherals {
                                display_bus: Spi1Display::new(
                                        clocks.peri_hz,
                                        PIN_DISPLAY_SCK,
                                        PIN_DISPLAY_MOSI,
                                        PIN_DISPLAY_CS,
                                        PIN_DISPLAY_DC,
                                        Some(PIN_DISPLAY_RESET),
                                        DISPLAY_SPI_HZ,
                                        DISPLAY_DMA_CH,
                                ),
                                backlight: Output::new(PIN_DISPLAY_BL, false),
                                touch_bus: I2c1::new(clocks.sys_hz, PIN_TOUCH_SCL, PIN_TOUCH_SDA, TOUCH_I2C_HZ),
                                touch_int: Input::new_pull_up(PIN_TOUCH_INT),
                                touch_reset: Output::new(PIN_TOUCH_RST, true),
                        })
                }
        }
}

/// The po13 rig: a Raspberry Pi Pico 2 wearing the Waveshare Pico-OLED-1.3 (SH1107, 64x128
/// portrait glass, 1 bpp) on SPI1, pins from mk3's `light_display_po13.h`.
pub mod pico2 {
        use super::*;

        pub const PIN_LED: usize = 25;

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
                pub oled_bus: Spi1Display,
        }

        static TAKEN: AtomicBool = AtomicBool::new(false);

        pub fn take(clocks: &Clocks) -> Option<Peripherals> {
                if TAKEN.swap(true, Ordering::AcqRel) {
                        return None;
                }
                // SAFETY: the flag above makes this the one construction of each peripheral
                unsafe {
                        Some(Peripherals {
                                led: Output::new(PIN_LED, false),
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
}
