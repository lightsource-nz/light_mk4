//! The default power behaviour every touch349 firmware carries: the screen dims after a spell of
//! no touch, and -- on battery, never on external power -- the board powers itself off after a
//! longer idle. [`PowerManager`] owns the four power peripherals ([`board::take`](crate::board::take)
//! hands them over) and the board module drives it: activity in, backlight commands in, a busy flag
//! while audio runs, and a [`Poll`] out that says when to shut down.
//!
//! Detecting "on battery" is the subtle part, because this board gives firmware no clean signal for
//! it. There is no VBUS or charge-status pin, the chip's USB VBUS-detect reads pinned-on (the device
//! stack forces the override), and the only power reading -- raw VBAT through a divider -- cannot
//! separate USB from battery: the charger has no power-path, so on USB the system drains VBAT
//! between recharge cycles and its slow decline is indistinguishable from a real discharge over any
//! practical window. So "on external power" is taken from **USB device enumeration**: if a host has
//! enumerated us (`light_shell_usb_mounted`, updated on core 1), we are plugged into a computer and
//! stay on. The one gap is a data-less wall charger, which never enumerates and so reads as battery
//! -- the board will power off (park, since VBUS holds the rails) after the idle. That is the
//! accepted trade for a signal that is actually reliable on this hardware.

use light_core::{info, InputPin, Poll};
use light_rp2::adc::Adc;
use light_rp2::gpio::{Input, Output};
use light_rp2::now_us;
use light_rp2::pwm::PwmOutput;

use crate::board::{BACKLIGHT_INVERTED, BACKLIGHT_LEVEL_MAX, BATTERY_DIVIDER, POWER_OFF_HOLD_MS};

unsafe extern "C" {
        /// True while a USB host has this device enumerated. Set on core 1 (which owns TinyUSB) by
        /// the C shell; a plain volatile bool, safe to read from core 0.
        fn light_shell_usb_mounted() -> bool;
}

/// The backlight dims this long after the last touch.
const DIM_AFTER_US: u64 = 15_000_000;
/// The dimmed level, on the console's `0..=BACKLIGHT_LEVEL_MAX` scale: low but not off.
const DIM_LEVEL: u16 = 250;
/// On battery, the board powers off this long after the last activity.
const POWER_OFF_AFTER_US: u64 = 600_000_000;
/// ADC samples averaged per VBAT reading, for a steady `stats` figure.
const BATTERY_SAMPLES: u32 = 16;

/// Owns the four power peripherals and runs the dim and power-off timers. The board module hands it
/// activity, backlight commands and a busy flag, and calls [`tick`](Self::tick) once per poll.
pub struct PowerManager {
        backlight: PwmOutput,
        /// The power latch: high since [`board::take`](crate::board::take); driven low on unload = power off.
        sys_en: Output,
        /// The side button, low when pressed; a [`POWER_OFF_HOLD_MS`] hold is the manual shutdown.
        button: Input,
        battery: Adc,
        /// Last touch/gesture/brightness change: drives the dim.
        last_touch_us: u64,
        /// Start of the current quiet spell (no touch, not busy): drives the power-off timer.
        quiet_since_us: u64,
        dimmed: bool,
        /// The last commanded backlight level, restored on wake.
        level: u16,
        /// Set while audio is capturing or playing: power-off is deferred so a recording is never cut short.
        busy: bool,
        /// The side-button hold start, for the manual power-off gesture.
        pressed_since_ms: Option<u32>,
}

impl PowerManager {
        pub fn new(backlight: PwmOutput, sys_en: Output, button: Input, battery: Adc) -> Self {
                let now = now_us();
                Self {
                        backlight,
                        sys_en,
                        button,
                        battery,
                        last_touch_us: now,
                        quiet_since_us: now,
                        dimmed: false,
                        level: BACKLIGHT_LEVEL_MAX,
                        busy: false,
                        pressed_since_ms: None,
                }
        }

        /// Full brightness on, timers zeroed. Called from the board module's `Module::load`.
        pub fn on_load(&mut self) {
                let now = now_us();
                self.last_touch_us = now;
                self.quiet_since_us = now;
                self.dimmed = false;
                self.apply(self.level);
        }

        /// Backlight off and the power latch released. On battery this powers the board off; on
        /// external power the rails stay up and the runtime parks. Called from `Module::unload`.
        pub fn on_unload(&mut self) {
                self.apply(0);
                self.sys_en.set(false);
        }

        /// User activity (a touch or gesture): wake the screen and restart the idle timers.
        pub fn note_activity(&mut self) {
                let now = now_us();
                self.last_touch_us = now;
                self.quiet_since_us = now;
                if self.dimmed {
                        self.apply(self.level);
                        self.dimmed = false;
                }
        }

        /// Set the backlight to a commanded level (the console `backlight` command). Counts as
        /// activity, and becomes the level restored on the next wake.
        pub fn set_backlight(&mut self, level: u16) {
                self.level = level.min(BACKLIGHT_LEVEL_MAX);
                self.apply(self.level);
                let now = now_us();
                self.last_touch_us = now;
                self.quiet_since_us = now;
                self.dimmed = false;
        }

        /// Mark the app busy (audio capturing or playing) or idle again. While busy the power-off
        /// idle timer is held at zero, so an in-progress recording is never cut short.
        pub fn set_busy(&mut self, busy: bool) {
                self.busy = busy;
        }

        /// Averaged VBAT in millivolts, for the `stats` command.
        pub fn battery_mv(&mut self) -> u32 {
                let mut sum = 0u32;
                for _ in 0..BATTERY_SAMPLES {
                        sum += u32::from(self.battery.read());
                }
                let raw = sum / BATTERY_SAMPLES;
                //   12-bit read across 3.3 V behind the divider
                raw * 3300 * BATTERY_DIVIDER / 4096
        }

        /// Run the timers: dim on schedule, power off on a long idle when not on USB, and answer the
        /// manual button hold. Returns [`Poll::Shutdown`] when the board should power down. The board
        /// module calls this once per `poll`, after routing its events in.
        pub fn tick(&mut self) -> Poll {
                let now = now_us();

                //   the manual gesture: a long hold of the side button powers off, whatever the
                // source. The shutdown flows through the runtime like the console's `quit`, so every
                // module unloads before this module's unload releases the latch
                let now_ms = (now / 1000) as u32;
                if self.button.is_low() {
                        let since = *self.pressed_since_ms.get_or_insert(now_ms);
                        if now_ms.wrapping_sub(since) >= POWER_OFF_HOLD_MS {
                                info!("power button held; shutting down");
                                return Poll::Shutdown;
                        }
                } else {
                        self.pressed_since_ms = None;
                }

                // dim after a spell without a touch
                if !self.dimmed && now.wrapping_sub(self.last_touch_us) >= DIM_AFTER_US {
                        self.apply(DIM_LEVEL);
                        self.dimmed = true;
                }

                //   audio in flight holds the idle clock at now, so the power-off countdown only runs
                // while the board is genuinely quiet -- a recording is never cut short
                if self.busy {
                        self.quiet_since_us = now;
                        return Poll::Idle;
                }

                //   the long idle: power off only on battery. "On battery" = not enumerated by a USB
                // host (this board has no VBUS pin; see the module docs). On USB the rails stay up
                // regardless, so this is inherently a no-op there -- but the gate keeps it from
                // parking the runtime while plugged into a computer
                if now.wrapping_sub(self.quiet_since_us) >= POWER_OFF_AFTER_US && !usb_mounted() {
                        info!("power: idle {}s on battery, powering down", POWER_OFF_AFTER_US / 1_000_000);
                        return Poll::Shutdown;
                }
                Poll::Idle
        }

        fn apply(&mut self, level: u16) {
                //   the driver's usable band, measured on this glass: the backlight is fully dark at
                // or below ~40% LED-on time and only dims visibly between ~45% and 100% -- an
                // RC-filtered threshold drive, not a proportional switch. Level 0 is off; every other
                // level maps linearly onto the band above the floor, so the console's 0..MAX scale is
                // all usable
                const FLOOR: u32 = 450;
                let level = u32::from(level.min(BACKLIGHT_LEVEL_MAX));
                let max = u32::from(BACKLIGHT_LEVEL_MAX);
                let physical = if level == 0 { 0 } else { (FLOOR + level * (max - FLOOR) / max) as u16 };
                let duty = if BACKLIGHT_INVERTED { BACKLIGHT_LEVEL_MAX - physical } else { physical };
                self.backlight.set_duty(duty);
        }
}

/// Whether a USB host has the device enumerated -- "on external (USB) power". See the module docs
/// for why this, and not VBAT, is the signal.
fn usb_mounted() -> bool {
        unsafe { light_shell_usb_mounted() }
}
