//! Portable core of the light framework, mk4 spike.
//!
//! Nothing in this crate touches hardware. Everything it needs from the world comes through
//! traits a board crate implements, which is what lets the same code run under `cargo test` on
//! the host with a mocked board -- the host-first rule mk3 has, made structural.

#![no_std]

/// What the spike's first milestone needs of a board: one output and a clock.
///
/// Deliberately minimal. The real port interface grows from here one primitive at a time, each
/// added when a piece of portable code needs it, so the boundary records what the framework
/// actually depends on rather than what a HAL happens to offer.
pub trait Board {
        /// Drive the panel backlight enable line.
        fn set_backlight(&mut self, on: bool);
        /// Microseconds since boot. Monotonic; wraps only after ~584,000 years.
        fn now_us(&self) -> u64;
}

/// Toggles the backlight every `period_us`, driven by polling rather than blocking, so it can
/// share a loop with everything else the way mk3's periodic tasks do.
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
        pub fn poll(&mut self, board: &mut impl Board) -> bool {
                let now = board.now_us();
                if now < self.next_us {
                        return false;
                }
                self.on = !self.on;
                board.set_backlight(self.on);
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

        struct Mock {
                now: u64,
                backlight: bool,
                transitions: u32,
        }

        impl Board for Mock {
                fn set_backlight(&mut self, on: bool) {
                        self.backlight = on;
                        self.transitions += 1;
                }
                fn now_us(&self) -> u64 {
                        self.now
                }
        }

        #[test]
        fn toggles_once_per_period() {
                let mut b = Mock { now: 0, backlight: false, transitions: 0 };
                let mut blink = Blinker::new(1_000);
                //   poll far more often than the period; only one transition per period may land
                for t in 0..10_000u64 {
                        b.now = t;
                        blink.poll(&mut b);
                }
                assert_eq!(b.transitions, 10);
                assert!(!b.backlight, "ten toggles from off ends off");
        }

        #[test]
        fn first_poll_turns_on_immediately() {
                let mut b = Mock { now: 0, backlight: false, transitions: 0 };
                let mut blink = Blinker::new(1_000);
                assert!(blink.poll(&mut b));
                assert!(b.backlight);
                assert!(!blink.poll(&mut b), "same instant, nothing more to do");
        }

        #[test]
        fn a_stall_does_not_burst() {
                let mut b = Mock { now: 0, backlight: false, transitions: 0 };
                let mut blink = Blinker::new(1_000);
                blink.poll(&mut b);
                //   the loop stalls for fifty periods; on resuming it must toggle once, then
                // resume the normal cadence -- not fire fifty times in a row
                b.now = 50_000;
                assert!(blink.poll(&mut b));
                assert!(!blink.poll(&mut b));
                b.now = 50_999;
                assert!(!blink.poll(&mut b));
                b.now = 51_000;
                assert!(blink.poll(&mut b));
        }
}
