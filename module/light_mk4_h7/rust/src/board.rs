//! Board wiring for the WeAct MiniSTM32H7xx (STM32H743VIT6): an on-board 0.96" ST7735S panel,
//! 160x80 in the landscape it is mounted in, on SPI4; one user key; one LED. Pins from mk3's
//! `screentest_hw_mini_stm32h7` and `light_board.h`, all hardware-verified there.
//!
//! This lives with the APPLICATION, not in the port crate: `light-stm32h7` knows the chip and
//! nothing about which WeAct board it sits on.

use light_core::atomic::{AtomicBool, Ordering};
use light_stm32h7::gpio::{Input, Output, Pin};
use light_stm32h7::spi::Spi4Display;
use light_stm32h7::Clocks;

pub const DISPLAY_WIDTH: u16 = 160;
pub const DISPLAY_HEIGHT: u16 = 80;
/// Where the visible glass sits in the ST7735's GDDRAM, measured by mk3.
pub const DISPLAY_COL_OFFSET: u16 = 1;
pub const DISPLAY_ROW_OFFSET: u16 = 26;
pub const DISPLAY_SPI_HZ: u32 = 8_000_000;
pub const PIN_DISPLAY_SCK: Pin = Pin::new('E', 12);
pub const PIN_DISPLAY_MOSI: Pin = Pin::new('E', 14);
pub const PIN_DISPLAY_CS: Pin = Pin::new('E', 11);
pub const PIN_DISPLAY_DC: Pin = Pin::new('E', 13);
/// The panel's reset is tied to the board's own reset line: nothing to pulse.
/// ACTIVE LOW, confirmed by mk3 driving it both ways over SWD: PE10 is TIM1_CH2N, the
/// complementary output, and high turns the backlight OFF. With it off the panel
/// renders perfectly and shows nothing, which reads as a dead display.
pub const PIN_DISPLAY_BL: Pin = Pin::new('E', 10);
/// Active low.
pub const PIN_LED: Pin = Pin::new('E', 3);
/// The user key K1: ACTIVE HIGH, pressing connects it to the supply. mk3's board header
/// said active low, and nothing in mk3 ever read it; sampled over SWD here, the pin sat
/// high under a pull-up whether pressed or not, and under a pull-down went high exactly
/// when pressed.
pub const PIN_KEY: Pin = Pin::new('C', 13);

pub struct Peripherals {
        pub display_bus: Spi4Display,
        /// Low is ON.
        pub backlight: Output,
        /// Low is ON.
        pub led: Output,
        pub key: Input,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// Configure and hand over the board's peripherals. Once.
pub fn take(clocks: &Clocks) -> Option<Peripherals> {
        if TAKEN.swap(true, Ordering::AcqRel) {
                return None;
        }
        Some(Peripherals {
                display_bus: Spi4Display::new(clocks.apb2_hz, PIN_DISPLAY_SCK, PIN_DISPLAY_MOSI, PIN_DISPLAY_CS, PIN_DISPLAY_DC, None, DISPLAY_SPI_HZ),
                // dark until the display is initialised: the first thing on the glass
                // should be a frame, not the panel's power-up noise
                backlight: Output::new(PIN_DISPLAY_BL, true),
                led: Output::new(PIN_LED, true),
                key: Input::new_pull_down(PIN_KEY),
        })
}
