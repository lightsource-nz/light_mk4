//! Board wiring for the Waveshare RP2350-Touch-LCD-3.49: pins from the board's reference
//! demo (`DEV_Config.h` + `qspi_pio.h`), which is the closest thing to a schematic
//! transcription on hand -- the same provenance discipline as the 2.8's wiring.
//! HARDWARE-VERIFIED 2026-08-31: panel, touch (axes measured on the glass), backlight
//! inversion, IMU axis map -- the bring-up checklist is complete.
//!
//! An RP2350B: 48 GPIOs, and this board actually uses the upper bank -- the touch bus, the
//! backlight and the battery pins all live above 31, which is what grew `light-rp2`'s
//! hi-bank support.

use light_core::atomic::{AtomicBool, Ordering};
use light_input::axs15231b::CoordMap;
use light_input::imu::{self, AxisMap};
use light_rp2::adc::Adc;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2s::PioI2sOut;
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
/// Raw axes: 0..=640 along the bar, 0..=172 across it. MEASURED on the glass 2026-08-31:
/// raw long runs 0 at the USB end, but the panel's row 0 is at the far end, so the long
/// axis inverts (the reference's `640 - pointX` agrees); the short axis matches the pixels
/// uninverted. Verified by labelled-widget taps after the flip.
pub const TOUCH_MAP: CoordMap = CoordMap { long_max: 640, short_max: 172, invert_long: true, invert_short: false };

/// QMI8658C on I2C1 (the reference's DEV bus), INT1 on 8, unused.
pub const PIN_IMU_SDA: usize = 6;
pub const PIN_IMU_SCL: usize = 7;
pub const PIN_IMU_INT1: usize = 8;
pub const IMU_I2C_HZ: u32 = 300_000;
/// MEASURED 2026-08-31, three observations: flat/screen-up read raw +Z (out of the glass),
/// title-end-up read raw +X (the raw X axis runs along the bar toward row 0), left-edge-down
/// read raw -Y. So display x = -raw_y, y = +raw_x, z = +raw_z -- determinant +1.
pub const IMU_AXIS_MAP: AxisMap = AxisMap { source: [imu::Y, imu::X, imu::Z], sign: [-1, 1, 1] };

/// DMA channel for the display: the top of the range, the RP2350 convention.
pub const DISPLAY_DMA_CH: usize = 15;

// Battery and the power latch. SYS_EN holds the board's power on when running from the
// battery: the button press that boots it also feeds the latch, and take() drives SYS_EN
// high as its FIRST act so the latch is held before anything slower runs. Releasing it
// (unload) is the power-off. SYS_OUT reads the same side button, low when pressed; the
// reference holds it 1.5 s for shutdown. BAT_ADC is ADC channel 0 (the RP2350B's analog
// pins are GPIO 40..=47) behind a divide-by-3, per the reference's conversion factor.
pub const PIN_SYS_OUT: usize = 38;
pub const PIN_SYS_EN: usize = 39;
pub const PIN_BAT_ADC: usize = 40;
pub const BATTERY_DIVIDER: u32 = 3;
pub const POWER_OFF_HOLD_MS: u32 = 1500;

// ES8311 audio: the codec shares i2c1 with the IMU and RTC, is clocked as the I2S MASTER
// from a PIO-generated 256-Fs MCLK, and its data line is served by the PIO1 slave writer.
// PA_CTRL gates the speaker amplifier. ⚠ PA_CTRL and DOUT sit on GPIO 0 and 1 -- the
// chip's default UART -- so claiming audio RETIRES THE UART CONSOLE on this board; the CDC
// console is unaffected. DIN (the microphone) is declared, no driver yet.
pub const PIN_AUDIO_PA: usize = 0;
pub const PIN_AUDIO_DOUT: usize = 1;
pub const PIN_AUDIO_DIN: usize = 2;
pub const PIN_AUDIO_MCLK: usize = 3;
pub const PIN_AUDIO_BCLK: usize = 4;
pub const PIN_AUDIO_LRCLK: usize = 5;
pub const AUDIO_SAMPLE_HZ: u32 = 24_000;
pub const AUDIO_MCLK_HZ: u32 = AUDIO_SAMPLE_HZ * 256;
/// The stream's ping-pong DMA pair, below the display's channel 15.
pub const AUDIO_DMA_CH: [usize; 2] = [13, 14];

// Declared so nothing reuses it; no driver yet. PSRAM hangs on the XIP CS1 pin.
pub const PIN_PSRAM_CS: usize = 47;

pub struct Peripherals {
        pub display_bus: PioQspiDisplayBus,
        /// PWM-driven, INVERTED (see [`BACKLIGHT_INVERTED`]); starts dark.
        pub backlight: PwmOutput,
        pub touch_bus: I2c0,
        pub touch_int: Input,
        pub imu_bus: I2c1,
        /// The power latch, already driven HIGH; drive low to power off on battery.
        pub sys_en: Output,
        /// The side button, low when pressed.
        pub power_button: Input,
        /// The battery divider on ADC channel 0.
        pub battery: Adc,
        /// The I2S transport: MCLK running, data machine waiting on the codec's clocks.
        pub i2s: PioI2sOut,
        /// The speaker amplifier enable, LOW (amp off) until audio loads.
        pub audio_pa: Output,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// Configure and hand over the board's peripherals. Once.
pub fn take(clocks: &Clocks) -> Option<Peripherals> {
        if TAKEN.swap(true, Ordering::AcqRel) {
                return None;
        }
        //   the latch first: on battery the board is only powered while the user holds the
        // button until this line runs
        let sys_en = Output::new(PIN_SYS_EN, true);
        // SAFETY: the flag above makes this the one construction of each peripheral; the
        // shell uses none of them
        unsafe {
                Some(Peripherals {
                        sys_en,
                        power_button: Input::new_pull_up(PIN_SYS_OUT),
                        battery: Adc::new(PIN_BAT_ADC),
                        i2s: PioI2sOut::new(PIN_AUDIO_DOUT, PIN_AUDIO_BCLK, PIN_AUDIO_LRCLK, PIN_AUDIO_MCLK, clocks.sys_hz, AUDIO_MCLK_HZ, AUDIO_DMA_CH[0], AUDIO_DMA_CH[1]),
                        audio_pa: Output::new(PIN_AUDIO_PA, false),
                        display_bus: PioQspiDisplayBus::new(PIN_LCD_SCLK, PIN_LCD_D0, PIN_LCD_CS, PIN_LCD_RST, Some(PIN_LCD_PWR_EN), DISPLAY_DMA_CH),
                        backlight: PwmOutput::new(PIN_LCD_BL, clocks.sys_hz, BACKLIGHT_CARRIER_HZ, BACKLIGHT_LEVEL_MAX),
                        touch_bus: I2c0::new(clocks.sys_hz, PIN_TOUCH_SCL, PIN_TOUCH_SDA, TOUCH_I2C_HZ),
                        touch_int: Input::new_pull_up(PIN_TOUCH_INT),
                        imu_bus: I2c1::new(clocks.sys_hz, PIN_IMU_SCL, PIN_IMU_SDA, IMU_I2C_HZ),
                })
        }
}
