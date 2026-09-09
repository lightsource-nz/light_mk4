//! The framework's power behaviour, once, as portable policy: the screen dims after a spell of no
//! activity, and -- on battery, never on external power -- the board powers itself off after a
//! longer idle. The policy is the timers and the state machine; everything a particular board can
//! actually *do* -- drive its backlight, tell whether it is on external power, cut its own power,
//! read its battery, read its power button -- is a [`PowerMechanism`] the port supplies.
//!
//! A port that has something to manage builds a [`PowerManager`] over its mechanism and a
//! [`Clock`], drives it from its board module (activity in, backlight commands in, a busy flag
//! while long work runs), and reads a [`Poll`] out that says when to shut down. A port with no
//! battery or backlight simply does not use this -- or supplies a mechanism whose methods are the
//! trivial defaults.
//!
//! **Levels are per-mille**: `0..=1000`, where 0 is off and 1000 is full. The manager works in this
//! scale and the mechanism maps it to whatever the hardware wants (a PWM duty, an inverted duty, a
//! byte). Detecting "on external power" is deliberately the mechanism's problem, because it is
//! irreducibly per-board -- a VBUS pin on one, USB enumeration on another, a charger status line on
//! a third -- and the policy must never have to know which.

#![no_std]

use light_core::hal::Clock;
use light_core::{info, Poll};

/// Full backlight, per-mille.
const FULL_LEVEL: u16 = 1000;
/// The backlight dims to this level after [`DIM_AFTER_US`] without activity.
const DIM_LEVEL: u16 = 250;
/// Dim this long after the last activity.
const DIM_AFTER_US: u64 = 15_000_000;
/// On battery, power off this long after the last activity.
const POWER_OFF_AFTER_US: u64 = 600_000_000;
/// A power-button hold of at least this long is the manual shutdown gesture.
const POWER_OFF_HOLD_MS: u32 = 1500;

/// What a board can do for power management. Every method a particular board lacks has a trivial
/// default, so a port implements only what it has: a backlight-less board leaves `set_backlight`,
/// a battery-less one leaves `battery_mv` and `on_external_power` (defaulting to "always external",
/// i.e. never auto-off), a button-less one leaves `power_button_pressed`.
pub trait PowerMechanism {
        /// Drive the backlight to `level` per-mille (`0..=1000`, 0 = off). Default: no backlight.
        fn set_backlight(&mut self, level: u16) {
                let _ = level;
        }

        /// Whether the board is on external power. When true, the long-idle power-off is suppressed.
        /// Default: always external, so a board that cannot tell never powers itself off.
        fn on_external_power(&self) -> bool {
                true
        }

        /// Cut the board's own power. On a board without a latch this is a no-op and the board
        /// simply parks. Called once, at shutdown. Default: nothing.
        fn power_off(&mut self) {}

        /// Whether the power button is held down right now. Default: no button.
        fn power_button_pressed(&self) -> bool {
                false
        }

        /// The battery voltage in millivolts, for reporting. Default: no gauge.
        fn battery_mv(&mut self) -> Option<u32> {
                None
        }
}

/// The portable power policy over a per-port [`PowerMechanism`] and a [`Clock`]. The board module
/// owns one and drives it; see the crate docs.
pub struct PowerManager<M: PowerMechanism, C: Clock> {
        mech: M,
        clock: C,
        /// Last activity (touch/gesture/brightness change): drives the dim.
        last_touch_us: u64,
        /// Start of the current quiet spell (no activity, not busy): drives the power-off.
        quiet_since_us: u64,
        dimmed: bool,
        /// The last commanded level, restored on wake.
        level: u16,
        /// Set while long work runs (e.g. audio): power-off is deferred so it is never cut short.
        busy: bool,
        /// The power-button hold start.
        pressed_since_ms: Option<u32>,
}

impl<M: PowerMechanism, C: Clock> PowerManager<M, C> {
        pub fn new(mech: M, clock: C) -> Self {
                let now = clock.now_us();
                Self {
                        mech,
                        clock,
                        last_touch_us: now,
                        quiet_since_us: now,
                        dimmed: false,
                        level: FULL_LEVEL,
                        busy: false,
                        pressed_since_ms: None,
                }
        }

        /// Full brightness on, timers zeroed. Call from the board module's `Module::load`.
        pub fn on_load(&mut self) {
                let now = self.clock.now_us();
                self.last_touch_us = now;
                self.quiet_since_us = now;
                self.dimmed = false;
                self.mech.set_backlight(self.level);
        }

        /// Backlight off and power cut. On battery this powers the board off; on external power the
        /// rails stay up and the runtime parks. Call from `Module::unload`.
        pub fn on_unload(&mut self) {
                self.mech.set_backlight(0);
                self.mech.power_off();
        }

        /// User activity: wake the screen and restart the idle timers.
        pub fn note_activity(&mut self) {
                let now = self.clock.now_us();
                self.last_touch_us = now;
                self.quiet_since_us = now;
                if self.dimmed {
                        self.mech.set_backlight(self.level);
                        self.dimmed = false;
                }
        }

        /// Set the backlight to a commanded level (per-mille). Counts as activity, and becomes the
        /// level restored on the next wake.
        pub fn set_backlight(&mut self, level: u16) {
                self.level = level.min(FULL_LEVEL);
                self.mech.set_backlight(self.level);
                let now = self.clock.now_us();
                self.last_touch_us = now;
                self.quiet_since_us = now;
                self.dimmed = false;
        }

        /// Mark long work in flight (or done). While busy the power-off idle timer is held at zero.
        pub fn set_busy(&mut self, busy: bool) {
                self.busy = busy;
        }

        /// The battery voltage in millivolts, or `None` if the board has no gauge.
        pub fn battery_mv(&mut self) -> Option<u32> {
                self.mech.battery_mv()
        }

        /// Run the timers: dim on schedule, power off on a long idle when not on external power, and
        /// answer the manual button hold. Returns [`Poll::Shutdown`] when the board should power
        /// down (the runtime then unloads every module, and this module's [`on_unload`](Self::on_unload)
        /// cuts power). Call once per `poll`, after routing events in.
        pub fn tick(&mut self) -> Poll {
                let now = self.clock.now_us();

                // the manual gesture: a long hold of the power button shuts down, whatever the source
                let now_ms = (now / 1000) as u32;
                if self.mech.power_button_pressed() {
                        let since = *self.pressed_since_ms.get_or_insert(now_ms);
                        if now_ms.wrapping_sub(since) >= POWER_OFF_HOLD_MS {
                                info!("power button held; shutting down");
                                return Poll::Shutdown;
                        }
                } else {
                        self.pressed_since_ms = None;
                }

                // dim after a spell without activity
                if !self.dimmed && now.wrapping_sub(self.last_touch_us) >= DIM_AFTER_US {
                        self.mech.set_backlight(DIM_LEVEL);
                        self.dimmed = true;
                }

                // long work holds the idle clock at now, so the power-off countdown runs only while quiet
                if self.busy {
                        self.quiet_since_us = now;
                        return Poll::Idle;
                }

                // the long idle: power off only when not on external power
                if now.wrapping_sub(self.quiet_since_us) >= POWER_OFF_AFTER_US && !self.mech.on_external_power() {
                        info!("power: idle {}s on battery, powering down", POWER_OFF_AFTER_US / 1_000_000);
                        return Poll::Shutdown;
                }
                Poll::Idle
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use core::cell::Cell;

        /// A clock the test advances by hand.
        struct MockClock(Cell<u64>);
        impl MockClock {
                fn new() -> Self {
                        Self(Cell::new(0))
                }
                fn advance_us(&self, us: u64) {
                        self.0.set(self.0.get() + us);
                }
        }
        impl Clock for MockClock {
                fn now_us(&self) -> u64 {
                        self.0.get()
                }
        }

        /// A mechanism whose inputs the test drives and whose backlight/power-off it records. Uses
        /// `Cell` so `&self` methods can be scripted from the test.
        #[derive(Default)]
        struct MockMech {
                external: Cell<bool>,
                button: Cell<bool>,
                backlight: Cell<u16>,
                powered_off: Cell<bool>,
        }
        impl PowerMechanism for MockMech {
                fn set_backlight(&mut self, level: u16) {
                        self.backlight.set(level);
                }
                fn on_external_power(&self) -> bool {
                        self.external.get()
                }
                fn power_off(&mut self) {
                        self.powered_off.set(true);
                }
                fn power_button_pressed(&self) -> bool {
                        self.button.get()
                }
                fn battery_mv(&mut self) -> Option<u32> {
                        Some(3900)
                }
        }

        // a shared handle to the mechanism's scriptable inputs/outputs, since the manager owns the mech
        fn make() -> PowerManager<MockMech, MockClock> {
                PowerManager::new(MockMech::default(), MockClock::new())
        }

        fn is_shutdown(p: Poll) -> bool {
                matches!(p, Poll::Shutdown)
        }

        #[test]
        fn dims_after_the_idle_and_wakes_on_activity() {
                let mut p = make();
                p.on_load();
                assert_eq!(p.mech.backlight.get(), FULL_LEVEL, "load lights it full");
                p.clock.advance_us(DIM_AFTER_US - 1);
                assert!(is_shutdown(p.tick()) == false);
                assert_eq!(p.mech.backlight.get(), FULL_LEVEL, "not yet dimmed");
                p.clock.advance_us(2);
                let _ = p.tick();
                assert_eq!(p.mech.backlight.get(), DIM_LEVEL, "dims once the idle passes");
                p.note_activity();
                assert_eq!(p.mech.backlight.get(), FULL_LEVEL, "activity wakes it to the last level");
        }

        #[test]
        fn wakes_to_the_last_commanded_level() {
                let mut p = make();
                p.on_load();
                p.set_backlight(600);
                assert_eq!(p.mech.backlight.get(), 600);
                p.clock.advance_us(DIM_AFTER_US + 1);
                let _ = p.tick();
                assert_eq!(p.mech.backlight.get(), DIM_LEVEL, "still dims");
                p.note_activity();
                assert_eq!(p.mech.backlight.get(), 600, "restores the commanded level, not full");
        }

        #[test]
        fn powers_off_on_battery_after_the_long_idle() {
                let mut p = make();
                p.mech.external.set(false); // on battery
                p.on_load();
                p.clock.advance_us(POWER_OFF_AFTER_US - 1);
                assert!(!is_shutdown(p.tick()), "not yet");
                p.clock.advance_us(2);
                assert!(is_shutdown(p.tick()), "powers off once the idle passes, on battery");
        }

        #[test]
        fn stays_on_external_power_however_long_it_idles() {
                let mut p = make();
                p.mech.external.set(true); // on external power
                p.on_load();
                p.clock.advance_us(POWER_OFF_AFTER_US * 3);
                assert!(!is_shutdown(p.tick()), "never auto-powers-off on external power");
        }

        #[test]
        fn busy_defers_the_power_off_until_quiet() {
                let mut p = make();
                p.mech.external.set(false);
                p.on_load();
                p.set_busy(true);
                p.clock.advance_us(POWER_OFF_AFTER_US * 2);
                assert!(!is_shutdown(p.tick()), "busy holds it on however long");
                p.set_busy(false);
                // the quiet clock only starts once busy clears (tick resets it while busy)
                p.clock.advance_us(POWER_OFF_AFTER_US - 1);
                assert!(!is_shutdown(p.tick()), "countdown runs from when busy cleared");
                p.clock.advance_us(2);
                assert!(is_shutdown(p.tick()), "then powers off");
        }

        #[test]
        fn a_long_button_hold_shuts_down_even_on_external_power() {
                let mut p = make();
                p.mech.external.set(true);
                p.on_load();
                p.mech.button.set(true);
                let _ = p.tick(); // press registered
                p.clock.advance_us(u64::from(POWER_OFF_HOLD_MS) * 1000 + 1000);
                assert!(is_shutdown(p.tick()), "the hold shuts down regardless of power source");
        }

        #[test]
        fn a_brief_button_press_does_not_shut_down() {
                let mut p = make();
                p.on_load();
                p.mech.button.set(true);
                let _ = p.tick();
                p.clock.advance_us(500_000); // 0.5 s
                assert!(!is_shutdown(p.tick()));
                p.mech.button.set(false);
                p.clock.advance_us(5_000_000);
                assert!(!is_shutdown(p.tick()), "a released short press is forgotten");
        }
}
