//! Board wiring for the Waveshare RP2350-Touch-LCD-4: pins from the board's vendor BSP
//! (`bsp_st7701.h`, `bsp_gt911.h`, `bsp_battery.h`), which is the closest thing to a
//! schematic transcription on hand. Every figure below is UNVERIFIED on hardware until
//! its bring-up ticks it off.
//!
//! An RP2350B, and the second board on the upper GPIO bank. The panel has NO GDDRAM: the
//! 480x480 RGB565 framebuffer lives in SRAM (450 KB of the 520 -- single-buffered, the
//! bring-up decision) and `light_rp2::rgb`'s hardware loop scans it out forever. take()
//! therefore RECEIVES the framebuffer pointer: the application owns the buffer, the board
//! starts the engine over it.

use light_core::atomic::{AtomicBool, Ordering};
use light_core::hal::Clock;
use light_input::gt911::CoordMap;
use light_input::imu::{self, AxisMap};
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::I2c1;
use light_rp2::pwm::PwmOutput;
use light_rp2::rgb::{RgbPins, RgbScanout};
use light_rp2::{adc::Adc, Clocks, SysClock};

// The RGB (DPI) interface: DE + syncs + PCLK, then 16 contiguous data pins.
pub const PIN_LCD_DE: usize = 20;
pub const PIN_LCD_VSYNC: usize = 21;
pub const PIN_LCD_HSYNC: usize = 22;
pub const PIN_LCD_PCLK: usize = 23;
pub const PIN_LCD_DATA0: usize = 24;
pub const LCD_PCLK_HZ: u32 = 16_000_000;
pub const DISPLAY_WIDTH: u16 = 480;
pub const DISPLAY_HEIGHT: u16 = 480;

// The ST7701S's 9-bit control SPI, bit-banged once at init.
pub const PIN_LCD_CS: usize = 18;
pub const PIN_LCD_SCK: usize = 2;
pub const PIN_LCD_SDA: usize = 3;
pub const PIN_LCD_RST: usize = 19;

/// The reference drives the backlight PWM with `100 - value`: LOW duty is BRIGHT,
/// like the 3.49. Whether this panel's driver also has a threshold band is bring-up's
/// to measure -- sweep it.
pub const PIN_LCD_BL: usize = 40;
pub const BACKLIGHT_INVERTED: bool = true;
pub const BACKLIGHT_LEVEL_MAX: u16 = 1000;
pub const BACKLIGHT_CARRIER_HZ: u32 = 5_000;

// GT911 touch on the shared i2c1; the part latches address 0x5D from INT held LOW through
// reset, which take() performs before handing the released INT over.
pub const PIN_TOUCH_SDA: usize = 6;
pub const PIN_TOUCH_SCL: usize = 7;
pub const PIN_TOUCH_INT: usize = 17;
pub const PIN_TOUCH_RST: usize = 16;
pub const I2C1_HZ: u32 = 400_000;
/// Raw axes 0..=479 each way; directions and the square panel's possible swap are
/// bring-up's to measure. All start unmapped.
pub const TOUCH_MAP: CoordMap = CoordMap { x_max: 479, y_max: 479, invert_x: false, invert_y: false, swap_xy: false };

/// QMI8658C and the PCF85063A RTC share i2c1 with the touch controller.
/// MEASURED 2026-09-01, the three-observation session: flat screen-up read raw Z = -1g
/// (device Z = -Z), and the upright poses put gravity on raw X and raw Y (device X from
/// Y, device Y from X). The horizontal SIGNS came from the glass, not the session: the
/// first fit was 180 degrees off -- the poses were described in the holder's frame, which
/// turned out to be mirrored from R0's -- and the UI rendering consistently upside down
/// in every upright pose is exactly that signature (flip both, never one: the map's
/// determinant must stay +1, a proper rotation, as a real mounting must be).
pub const IMU_AXIS_MAP: AxisMap = AxisMap { source: [imu::Y, imu::X, imu::Z], sign: [1, 1, -1] };

// Battery: ADC channel 1 (GPIO 41), with the charger's status pins (low-active).
pub const PIN_BAT_ADC: usize = 41;
pub const PIN_BAT_CHRG: usize = 42;
pub const PIN_BAT_DONE: usize = 43;
/// MEASURED ÷2: the vendor formula says ÷3, but ÷3 read an impossible 6.6 V on hardware
/// while ÷2 reads a plausible 4.4 V charged pack. The vendor formula fits their OWN board
/// revision, not this one.
pub const BATTERY_DIVIDER: u32 = 2;

/// The scanout's two DMA channels: the frame streamer and its reprogram partner.
pub const RGB_DMA_DATA_CH: usize = 15;
pub const RGB_DMA_CTRL_CH: usize = 14;

// MEASURED ABSENT (2026-09-01): GPIO 47 is the RP2350B's XIP CS1 and the vendor demo
// carries PSRAM plumbing, but the SDK's chip-ID auto-detect reads nothing and both vendor
// examples ship with enabled_psram = false -- family boilerplate, not fitment, exactly
// like the 3.49. The detection stays wired (a fitted variant degrades up, not down) and
// the `psram` console command reports what it found. Consequence: the capture-based
// animations (push transition, rotation spin) stay off this board's menu for good.
pub const PIN_PSRAM_CS: usize = 47;

// Declared so nothing reuses them; no drivers yet.
pub const PIN_BUZZER: usize = 1;

pub struct Peripherals {
        /// Running: the panel is initialized and the engine is scanning the framebuffer.
        pub scanout: RgbScanout,
        /// PWM-driven, INVERTED; starts dark.
        pub backlight: PwmOutput,
        pub i2c1: I2c1,
        pub touch_int: Input,
        pub battery: Adc,
        pub charging: Input,
        pub charge_done: Input,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// Configure and hand over the board's peripherals. Once. `fb` is the application's
/// `width * height` RGB565 framebuffer, which the scanout engine reads forever.
pub fn take(clocks: &Clocks, fb: *const u16) -> Option<Peripherals> {
        if TAKEN.swap(true, Ordering::AcqRel) {
                return None;
        }
        let mut clock = SysClock;

        //   the GT911's address latch: INT low through the reset pulse selects 0x5D
        let mut touch_int_out = Output::new(PIN_TOUCH_INT, false);
        let mut touch_rst = Output::new(PIN_TOUCH_RST, true);
        touch_int_out.set(false);
        touch_rst.set(false);
        clock.delay_ms(50);
        touch_rst.set(true);
        clock.delay_ms(250);
        core::mem::forget(touch_rst);
        core::mem::forget(touch_int_out);
        let touch_int = Input::new_pull_up(PIN_TOUCH_INT);

        //   the panel init over the bit-banged 9-bit SPI, then the scanout engine over the
        // application's framebuffer
        let mut cs = Output::new(PIN_LCD_CS, true);
        let mut sck = Output::new(PIN_LCD_SCK, false);
        let mut sda = Output::new(PIN_LCD_SDA, false);
        let mut rst = Output::new(PIN_LCD_RST, true);
        light_display::st7701s::init(&mut cs, &mut sck, &mut sda, &mut rst, &mut clock);

        // SAFETY: the flag above makes this the one construction of each peripheral; the
        // shell uses none of them
        unsafe {
                Some(Peripherals {
                        scanout: RgbScanout::new(
                                RgbPins { de: PIN_LCD_DE, vsync: PIN_LCD_VSYNC, hsync: PIN_LCD_HSYNC, pclk: PIN_LCD_PCLK, data0: PIN_LCD_DATA0 },
                                DISPLAY_WIDTH,
                                DISPLAY_HEIGHT,
                                fb,
                                clocks.sys_hz,
                                LCD_PCLK_HZ,
                                RGB_DMA_DATA_CH,
                                RGB_DMA_CTRL_CH,
                        ),
                        backlight: PwmOutput::new(PIN_LCD_BL, clocks.sys_hz, BACKLIGHT_CARRIER_HZ, BACKLIGHT_LEVEL_MAX),
                        i2c1: I2c1::new(clocks.sys_hz, PIN_TOUCH_SCL, PIN_TOUCH_SDA, I2C1_HZ),
                        touch_int,
                        battery: Adc::new(PIN_BAT_ADC),
                        charging: Input::new_pull_up(PIN_BAT_CHRG),
                        charge_done: Input::new_pull_up(PIN_BAT_DONE),
                })
        }
}
