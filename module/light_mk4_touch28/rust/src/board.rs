//! Board wiring for the Waveshare RP2350-Touch-LCD-2.8: pins from the board's official
//! schematic, cross-checked against two independent open-source drivers for this exact board
//! -- the same paper trail the 1.69's pinout had before ITS bring-up. mk3 committed this
//! wiring build-verified only; this is its first board on a bench.
//!
//! This lives with the APPLICATION, not in the port crate: `light-rp2` knows the chip and
//! nothing about what anyone soldered to it.

use light_core::atomic::{AtomicBool, Ordering};
use light_input::imu::{self, AxisMap};
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::I2c1;
use light_rp2::pwm::PwmOutput;
use light_rp2::spi::Spi1Display;
use light_rp2::Clocks;

// ST7789T3 panel, SPI 4-wire on real SPI1 (GPIO10/11 mux to SPI1 SCK/TX). The panel's MISO
// return is wired on GP12 but unused -- every display driver here is write-only.
pub const PIN_DISPLAY_SCK: usize = 10;
pub const PIN_DISPLAY_MOSI: usize = 11;
pub const PIN_DISPLAY_CS: usize = 13;
pub const PIN_DISPLAY_DC: usize = 14;
pub const PIN_DISPLAY_RESET: usize = 15;
pub const PIN_DISPLAY_BL: usize = 16;
/// 240x320: the ST7789's full native GDDRAM, so unlike the 1.69's 240x280 window there is no
/// row offset to measure -- the frame buffer covers the controller's memory exactly.
pub const DISPLAY_WIDTH: u16 = 240;
pub const DISPLAY_HEIGHT: u16 = 320;
/// Same figure and rationale as the 1.69: 40 MHz is the working ST7789 clock on short traces.
/// One open-source driver for this exact board runs 62.5 MHz, so there may be headroom above
/// -- TO BE CONFIRMED: too fast shows up as corrupt pixels, not a clean failure.
pub const DISPLAY_SPI_HZ: u32 = 40_000_000;

// CST328 touch controller -- shared I2C1 (also carrying the IMU and, unused, the RTC), with
// its own reset net (the 2.0" sibling shares the LCD's; this one does not).
pub const PIN_TOUCH_SDA: usize = 6;
pub const PIN_TOUCH_SCL: usize = 7;
pub const PIN_TOUCH_RST: usize = 17;
pub const PIN_TOUCH_INT: usize = 18;
pub const TOUCH_I2C_HZ: u32 = 300_000;

/// QMI8658C on the same bus; INT1/INT2 (8/9) wired but unused -- the data-ready bit arrives
/// inside the sample frame.
pub const PIN_IMU_INT1: usize = 8;
/// How this board mounts the QMI8658C: UNKNOWN until measured -- declared as the identity
/// map because a wrong guess costs the same three-observation calibration session either
/// way, and identity at least states plainly that nothing has been measured. The 1.69's map
/// (transposed X/Y, inverted Z) is a fact about ITS layout, not a family trait.
pub const IMU_AXIS_MAP: AxisMap = AxisMap { source: [imu::X, imu::Y, imu::Z], sign: [1, 1, 1] };

/// DMA channel for the display bus: see `Spi1Display` for why the top of the range. This is
/// an RP2350-only board, so 15 is not the RP2040 trap the po13 wiring documents.
pub const DISPLAY_DMA_CH: usize = 15;

/// Backlight: an NPN low-side switch behind a 1k base resistor -- driving the GPIO high
/// lights the panel, same polarity as the 1.69. Levels run `0..=BACKLIGHT_LEVEL_MAX`.
pub const BACKLIGHT_LEVEL_MAX: u16 = 1000;
pub const BACKLIGHT_CARRIER_HZ: u32 = 30_000;

// Declared so nothing reuses them; no drivers yet. The audio is a PCM5101A I2S DAC on
// 2/3/4 (no PWM buzzer on this board); KEY_BAT reads low while pressed; BAT_EN must be
// driven high early to stay alive on battery -- irrelevant on USB.
pub const PIN_I2S_BCK: usize = 2;
pub const PIN_I2S_LRCK: usize = 3;
pub const PIN_I2S_DIN: usize = 4;
pub const PIN_RTC_INT: usize = 5;
pub const PIN_KEY_BAT: usize = 25;
pub const PIN_BAT_EN: usize = 26;

pub struct Peripherals {
        pub display_bus: Spi1Display,
        /// PWM-driven; starts dark.
        pub backlight: PwmOutput,
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
                        backlight: PwmOutput::new(PIN_DISPLAY_BL, clocks.sys_hz, BACKLIGHT_CARRIER_HZ, BACKLIGHT_LEVEL_MAX),
                        touch_bus: I2c1::new(clocks.sys_hz, PIN_TOUCH_SCL, PIN_TOUCH_SDA, TOUCH_I2C_HZ),
                        touch_int: Input::new_pull_up(PIN_TOUCH_INT),
                        touch_reset: Output::new(PIN_TOUCH_RST, true),
                })
        }
}
