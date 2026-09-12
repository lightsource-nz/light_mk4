//! The touch349 board's power mechanism: how this board does what the portable
//! [`light_power_manager`] policy asks -- drive the backlight, tell whether it is on external
//! power, cut its own power, read its battery and its side button. The policy (dim after an idle,
//! power off on battery after a longer one) lives in [`light_power_manager`]; this file is only the
//! board-specific half.
//!
//! Detecting "on external power" is the subtle part. This board has no VBUS sense, the chip's USB
//! VBUS-detect reads pinned-on (the device stack forces the override), and raw VBAT cannot be
//! trended (no power-path charger). Two signals combine, each covering the other's blind spot:
//!
//! - **GPIO 47** (see [`PIN_CHARGE_STAT`]): the ETA6098 charger's STAT line, through a MOSFET, at
//!   this pin. Per the datasheet STAT is an OPEN-DRAIN output with only two states: pulled LOW
//!   while charging, and high-impedance once charging completes -- it is never actively driven
//!   high. And on battery the charger has no input power at all, so it too leaves STAT floating.
//!   So the charger asserts exactly one thing we can trust: charging = external power. At this pin
//!   that is GPIO 47 LOW (unambiguous external); GPIO 47 HIGH is the released/Hi-Z state, which
//!   means charge-complete OR on-battery -- the two are indistinguishable here.
//! - **USB device enumeration** (`light_shell_usb_mounted`): true while a host has us enumerated,
//!   which recovers the one case GPIO 47 cannot -- plugged into a computer with the battery full
//!   (charger done, STAT released, GPIO 47 high).
//!
//! `on_external_power` is `GPIO47 low OR enumerated`. Together they cover charging (on a host or a
//! data-less charger) and full-on-a-host. The one gap is *full* on a data-less wall charger
//! (charging done so GPIO 47 high, no host) -- it reads as battery and parks after the idle, rare
//! and harmless (external power holds the rails; it wakes on the button).

use light_core::{InputPin, Poll};
use light_rp2::adc::Adc;
use light_rp2::gpio::{Input, Output};
use light_rp2::pwm::PwmOutput;
use light_rp2::SysClock;
use light_power_manager::PowerMechanism;

use crate::board::{BACKLIGHT_INVERTED, BACKLIGHT_LEVEL_MAX, BATTERY_DIVIDER};

unsafe extern "C" {
        /// True while a USB host has this device enumerated. Set on core 1 (which owns TinyUSB) by
        /// the C shell; a plain volatile bool, safe to read from core 0. Covers GPIO 47's done-gap.
        fn light_shell_usb_mounted() -> bool;
}

/// ADC samples averaged per VBAT reading, for a steady `stats` figure.
const BATTERY_SAMPLES: u32 = 16;

/// This board's [`PowerMechanism`]: owns the backlight PWM, the power latch, the side button, the
/// battery ADC and the charger-status pin. Handed to a [`light_power_manager::PowerManager`] (see
/// [`PowerManager`]).
pub struct Touch349Power {
        backlight: PwmOutput,
        /// The power latch: high since [`board::take`](crate::board::take); driven low on `power_off` = off.
        sys_en: Output,
        /// The side button, low when pressed.
        button: Input,
        battery: Adc,
        /// The charger-status feedback (see [`board::PIN_CHARGE_STAT`](crate::board)); LOW = charging (external power).
        charge_stat: Input,
}

impl Touch349Power {
        pub fn new(backlight: PwmOutput, sys_en: Output, button: Input, battery: Adc, charge_stat: Input) -> Self {
                Self { backlight, sys_en, button, battery, charge_stat }
        }
}

impl PowerMechanism for Touch349Power {
        fn set_backlight(&mut self, level: u16) {
                //   the driver's usable band, measured on this glass: the backlight is fully dark at
                // or below ~40% LED-on time and only dims visibly between ~45% and 100% -- an
                // RC-filtered threshold drive, not a proportional switch. Level 0 is off; every other
                // per-mille level maps linearly onto the band above the floor, so the whole 0..MAX
                // scale is usable
                const FLOOR: u32 = 450;
                let level = u32::from(level.min(BACKLIGHT_LEVEL_MAX));
                let max = u32::from(BACKLIGHT_LEVEL_MAX);
                let physical = if level == 0 { 0 } else { (FLOOR + level * (max - FLOOR) / max) as u16 };
                let duty = if BACKLIGHT_INVERTED { BACKLIGHT_LEVEL_MAX - physical } else { physical };
                self.backlight.set_duty(duty);
        }

        fn on_external_power(&self) -> bool {
                //   GPIO 47 LOW means the charger is actively pulling its STAT line down, which it
                // only does while charging -- so LOW is an unambiguous "on external power". Its HIGH
                // is ambiguous: the charger releases STAT (Hi-Z) both at charge-complete AND when it
                // is unpowered on battery, so HIGH covers full-and-plugged and on-battery alike. The
                // full-and-plugged case is recovered by USB enumeration instead. See the module docs.
                self.charge_stat.is_low() || unsafe { light_shell_usb_mounted() }
        }

        fn power_off(&mut self) {
                self.sys_en.set(false);
        }

        fn power_button_pressed(&self) -> bool {
                self.button.is_low()
        }

        fn battery_mv(&mut self) -> Option<u32> {
                let mut sum = 0u32;
                for _ in 0..BATTERY_SAMPLES {
                        sum += u32::from(self.battery.read());
                }
                let raw = sum / BATTERY_SAMPLES;
                //   12-bit read across 3.3 V behind the divider
                Some(raw * 3300 * BATTERY_DIVIDER / 4096)
        }
}

/// The board module's power handle: the portable policy over this board's mechanism and clock.
/// A thin facade so the module keeps a board-shaped API (`new` from the four peripherals, a `u32`
/// battery read) while the behaviour itself lives in [`light_power_manager`].
pub struct PowerManager(light_power_manager::PowerManager<Touch349Power, SysClock>);

impl PowerManager {
        pub fn new(backlight: PwmOutput, sys_en: Output, button: Input, battery: Adc, charge_stat: Input) -> Self {
                Self(light_power_manager::PowerManager::new(Touch349Power::new(backlight, sys_en, button, battery, charge_stat), SysClock))
        }

        pub fn on_load(&mut self) {
                self.0.on_load();
        }

        pub fn on_unload(&mut self) {
                self.0.on_unload();
        }

        pub fn note_activity(&mut self) {
                self.0.note_activity();
        }

        pub fn set_backlight(&mut self, level: u16) {
                self.0.set_backlight(level);
        }

        pub fn set_busy(&mut self, busy: bool) {
                self.0.set_busy(busy);
        }

        /// VBAT in millivolts; this board always has a gauge, so the [`Option`] is always `Some`.
        pub fn battery_mv(&mut self) -> u32 {
                self.0.battery_mv().unwrap_or(0)
        }

        /// Whether the board reads as on external power (the charger-status pin), for `stats`.
        pub fn on_external_power(&self) -> bool {
                self.0.on_external_power()
        }

        pub fn tick(&mut self) -> Poll {
                self.0.tick()
        }
}
