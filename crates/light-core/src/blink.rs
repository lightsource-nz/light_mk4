//! A poll-driven toggle, the first thing the spike drove on hardware and still the simplest
//! module to test scheduling behaviour with.

use crate::hal::{Clock, OutputPin};

/// Toggles an output every `period_us`, driven by polling rather than blocking, so it can
/// share a loop with everything else the way every periodic task here does.
pub struct Blinker {
        period_us: u64,
        next_us: u64,
        on: bool,
}

impl Blinker {
        pub fn new(period_us: u64) -> Self {
                Self { period_us, next_us: 0, on: false }
        }

        /// Returns `true` when this poll changed the output.
        pub fn poll(&mut self, pin: &mut impl OutputPin, clock: &impl Clock) -> bool {
                let now = clock.now_us();
                if now < self.next_us {
                        return false;
                }
                self.on = !self.on;
                pin.set(self.on);
                //   scheduled from the deadline, not from `now`, so a late poll does not drift
                // the phase -- unless the loop stalled past a whole period, in which case the
                // schedule restarts from now rather than toggling in a burst to catch up. (the
                // first version of this clamped to `now - period`, a deadline already in the
                // past, and the host test caught it firing twice on resume)
                self.next_us += self.period_us;
                if self.next_us <= now {
                        self.next_us = now + self.period_us;
                }
                true
        }

        pub fn is_on(&self) -> bool {
                self.on
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::Cell;

        struct Pin<'a> {
                on: &'a Cell<bool>,
                transitions: &'a Cell<u32>,
        }
        impl OutputPin for Pin<'_> {
                fn set(&mut self, high: bool) {
                        self.on.set(high);
                        self.transitions.set(self.transitions.get() + 1);
                }
        }
        struct Now(Cell<u64>);
        impl Clock for Now {
                fn now_us(&self) -> u64 {
                        self.0.get()
                }
        }

        #[test]
        fn toggles_once_per_period() {
                let (on, n) = (Cell::new(false), Cell::new(0));
                let mut pin = Pin { on: &on, transitions: &n };
                let clock = Now(Cell::new(0));
                let mut blink = Blinker::new(1_000);
                //   poll far more often than the period; only one transition per period may land
                for t in 0..10_000u64 {
                        clock.0.set(t);
                        blink.poll(&mut pin, &clock);
                }
                assert_eq!(n.get(), 10);
                assert!(!on.get(), "ten toggles from off ends off");
        }

        #[test]
        fn a_stall_does_not_burst() {
                let (on, n) = (Cell::new(false), Cell::new(0));
                let mut pin = Pin { on: &on, transitions: &n };
                let clock = Now(Cell::new(0));
                let mut blink = Blinker::new(1_000);
                assert!(blink.poll(&mut pin, &clock));
                //   the loop stalls for fifty periods; on resuming it must toggle once, then
                // resume the normal cadence -- not fire fifty times in a row
                clock.0.set(50_000);
                assert!(blink.poll(&mut pin, &clock));
                assert!(!blink.poll(&mut pin, &clock));
                clock.0.set(50_999);
                assert!(!blink.poll(&mut pin, &clock));
                clock.0.set(51_000);
                assert!(blink.poll(&mut pin, &clock));
        }
}
