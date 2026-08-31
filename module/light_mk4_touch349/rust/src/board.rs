//! Board wiring for the Waveshare RP2350-Touch-LCD-3.49: pins from the board's reference
//! demo (`DEV_Config.h` + `qspi_pio.h`), which is the closest thing to a schematic
//! transcription on hand -- the same provenance discipline as the 2.8's wiring, and every
//! figure below is UNVERIFIED on this bench until its bring-up ticks it off.
//!
//! An RP2350B: 48 GPIOs, and this board actually uses the upper bank -- the touch bus, the
//! backlight and the battery pins all live above 31, which is what grew `light-rp2`'s
//! hi-bank support.

use light_core::atomic::{AtomicBool, Ordering};
use light_input::axs15231b::CoordMap;
use light_input::imu::{self, AxisMap};
use light_rp2::gpio::Input;
use light_rp2::i2c::{I2c0, I2c1};
use light_rp2::pwm::PwmOutput;
use light_rp2::qspi::PioQspiDisplayBus;
use light_rp2::Clocks;

// AXS15231B panel, QSPI through PIO0: SCLK then the four data lines, contiguous -- the bus
// requires that layout. CS and RST are plain GPIOs; PWR_EN gates the panel supply.
pub const PIN_LCD_SCLK: usize = 20;
pub const PIN_LCD_D0: usize = 21;
pub const PIN_LCD_CS: usize = 25;
pub const PIN_LCD_RST: usize = 34;
pub const PIN_LCD_PWR_EN: usize = 37;
pub const PIN_LCD_BL: usize = 36;
/// 172x640 portrait: a bar display, the full GDDRAM (no offsets in the reference).
pub const DISPLAY_WIDTH: u16 = 172;
pub const DISPLAY_HEIGHT: u16 = 640;

/// The reference drives the backlight PWM with `100 - value`: LOW duty is BRIGHT on this
/// board, the opposite of the 2.8's NPN switch. The board module inverts.
pub const BACKLIGHT_INVERTED: bool = true;
pub const BACKLIGHT_LEVEL_MAX: u16 = 1000;
pub const BACKLIGHT_CARRIER_HZ: u32 = 30_000;

// The AXS15231B's touch half: its own I2C instance on the upper bank. No reset line of its
// own -- the LCD's RST resets the whole chip, so the touch driver carries no recovery reset.
pub const PIN_TOUCH_SDA: usize = 32;
pub const PIN_TOUCH_SCL: usize = 33;
pub const PIN_TOUCH_INT: usize = 11;
pub const TOUCH_I2C_HZ: u32 = 300_000;
/// Raw axes: 0..=640 along the bar, 0..=172 across it. Which way each runs on the glass is
/// bring-up's to measure; both start uninverted.
pub const TOUCH_MAP: CoordMap = CoordMap { long_max: 640, short_max: 172, invert_long: false, invert_short: false };

/// QMI8658C on I2C1 (the reference's DEV bus), INT1 on 8, unused.
pub const PIN_IMU_SDA: usize = 6;
pub const PIN_IMU_SCL: usize = 7;
pub const PIN_IMU_INT1: usize = 8;
pub const IMU_I2C_HZ: u32 = 300_000;
/// UNMEASURED identity, as every board starts -- the three-observation session replaces it.
pub const IMU_AXIS_MAP: AxisMap = AxisMap { source: [imu::X, imu::Y, imu::Z], sign: [1, 1, 1] };

/// DMA channel for the display: the top of the range, the RP2350 convention.
pub const DISPLAY_DMA_CH: usize = 15;

// Declared so nothing reuses them; no drivers yet. The audio codec is an ES8311 over PIO
// I2S; PSRAM hangs on the XIP CS1 pin.
pub const PIN_PSRAM_CS: usize = 47;
pub const PIN_SYS_OUT: usize = 38;
pub const PIN_SYS_EN: usize = 39;
pub const PIN_BAT_ADC: usize = 40;

pub struct Peripherals {
        pub display_bus: PioQspiDisplayBus,
        /// PWM-driven, INVERTED (see [`BACKLIGHT_INVERTED`]); starts dark.
        pub backlight: PwmOutput,
        pub touch_bus: I2c0,
        pub touch_int: Input,
        pub imu_bus: I2c1,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// Configure and hand over the board's peripherals. Once.
pub fn take(clocks: &Clocks) -> Option<Peripherals> {
        if TAKEN.swap(true, Ordering::AcqRel) {
                return None;
        }
        // SAFETY: the flag above makes this the one construction of each peripheral; the
        // shell uses none of them
        unsafe {
                Some(Peripherals {
                        display_bus: PioQspiDisplayBus::new(PIN_LCD_SCLK, PIN_LCD_D0, PIN_LCD_CS, PIN_LCD_RST, Some(PIN_LCD_PWR_EN), DISPLAY_DMA_CH),
                        backlight: PwmOutput::new(PIN_LCD_BL, clocks.sys_hz, BACKLIGHT_CARRIER_HZ, BACKLIGHT_LEVEL_MAX),
                        touch_bus: I2c0::new(clocks.sys_hz, PIN_TOUCH_SCL, PIN_TOUCH_SDA, TOUCH_I2C_HZ),
                        touch_int: Input::new_pull_up(PIN_TOUCH_INT),
                        imu_bus: I2c1::new(clocks.sys_hz, PIN_IMU_SCL, PIN_IMU_SDA, IMU_I2C_HZ),
                })
        }
}
