//! Board wiring for the Waveshare RP2350-Touch-LCD-1.69: pins from the board schematic,
//! confirmed on hardware.
//!
//! This lives with the APPLICATION, not in the port crate: `light-rp2` knows the chip and
//! nothing about what anyone soldered to it. This module is the one place that knows which pin
//! carries what, and `take()` configures every peripheral the board wires up and hands them
//! over once -- the application receives owned values, so two drivers cannot share a bus by
//! accident and nothing can reach a peripheral the board did not wire.

use light_core::atomic::{AtomicBool, Ordering};
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::I2c1;
use light_rp2::pwm::PwmOutput;
use light_rp2::pwm_audio::PwmAudio;
use light_rp2::spi::Spi1Display;
use light_rp2::Clocks;

pub const PIN_DISPLAY_DC: usize = 8;
pub const PIN_DISPLAY_CS: usize = 9;
pub const PIN_DISPLAY_SCK: usize = 10;
pub const PIN_DISPLAY_MOSI: usize = 11;
pub const PIN_DISPLAY_RESET: usize = 13;
pub const PIN_DISPLAY_BL: usize = 25;
pub const DISPLAY_WIDTH: u16 = 240;
pub const DISPLAY_HEIGHT: u16 = 280;
/// The visible glass is GDDRAM rows 20..299 -- measured on hardware.
pub const DISPLAY_ROW_OFFSET: u16 = 20;
/// 40 MHz confirmed clean on hardware; 10 MHz would cap a full frame at 9.3 fps.
///
/// Tried at 10 MHz on 2026-08-29 to test whether the touch controller's wedges (I2C on
/// pins 6/7 timing out under continuous rendering) track the SPI clock on 10/11: 17
/// clean taps then a wedge, against wedges every 4-8 taps at 40 MHz. Suggestive, not
/// decisive -- one run each. Left at 40 MHz, the clock the panel is verified at.
pub const DISPLAY_SPI_HZ: u32 = 40_000_000;

pub const PIN_TOUCH_SDA: usize = 6;
pub const PIN_TOUCH_SCL: usize = 7;
pub const PIN_TOUCH_INT: usize = 21;
pub const PIN_TOUCH_RST: usize = 22;
pub const TOUCH_I2C_HZ: u32 = 300_000;
/// The QMI8658C shares I2C1 with the touch controller, at its own address. INT1/INT2
/// (23/24) are wired but unused: the data-ready bit arrives inside the sample frame.
pub const PIN_IMU_INT1: usize = 23;
/// How the IMU is mounted, CONFIRMED on hardware with the +1g-points-up
/// convention: chip +Y points right, chip +X points up the screen, chip +Z points into
/// it. A real rotation -- the transposition and the inversion corroborate each other.
pub const IMU_AXIS_MAP: light_input::imu::AxisMap = light_input::imu::AxisMap { source: [light_input::imu::Y, light_input::imu::X, light_input::imu::Z], sign: [1, 1, -1] };

/// DMA channel for the display bus: see `Spi1Display` for why the top of the range.
pub const DISPLAY_DMA_CH: usize = 15;

/// The piezo, on the pin it is wired to and verified
/// audible: GPIO 2 is free of every on-board function on this board.
pub const PIN_BUZZER: usize = 2;
/// The sample stream's DMA channel and pacing timer, below the display's channel.
pub const AUDIO_DMA_CH: usize = 13;
pub const AUDIO_DMA_TIMER: usize = 0;

/// Backlight levels run `0..=BACKLIGHT_LEVEL_MAX`, a per-mille scale.
pub const BACKLIGHT_LEVEL_MAX: u16 = 1000;
pub const BACKLIGHT_CARRIER_HZ: u32 = 30_000;

pub struct Peripherals {
        pub display_bus: Spi1Display,
        /// PWM-driven; starts dark.
        pub backlight: PwmOutput,
        pub touch_bus: I2c1,
        pub touch_int: Input,
        pub touch_reset: Output,
        /// The piezo: tones and paced-DMA duty streams. Parked silent.
        pub buzzer: PwmAudio,
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
                        buzzer: PwmAudio::new(PIN_BUZZER, clocks.sys_hz, AUDIO_DMA_CH, AUDIO_DMA_TIMER),
                })
        }
}
