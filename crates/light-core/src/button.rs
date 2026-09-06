//! A debounced button over any input, ported from the predecessor C framework.
//!
//! The driver only answers "is the contact closed right now"; the debounce lives here once,
//! for the same reason the async update state machine lives in the display core rather than
//! in every driver. Any raw change restarts the window, so a chattering contact never
//! accumulates the stability the debounced state needs to move.

use crate::hal::InputPin;

/// Default window: long enough to swallow a cheap tactile switch's bounce (under 5 ms,
/// specified up to 10), short enough that a press still feels immediate.
pub const DEBOUNCE_MS_DEFAULT: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonEvent {
        Press,
        Release,
}

pub struct Button<P: InputPin> {
        pin: P,
        /// The switch pulls the pin LOW when pressed -- the usual wiring, with a pull-up.
        active_low: bool,
        debounce_ms: u32,
        /// The debounced level.
        pub pressed: bool,
        raw_last: bool,
        raw_change_ms: u32,
        pending: Option<ButtonEvent>,
}

impl<P: InputPin> Button<P> {
        pub fn new(pin: P, active_low: bool) -> Self {
                Self { pin, active_low, debounce_ms: DEBOUNCE_MS_DEFAULT, pressed: false, raw_last: false, raw_change_ms: 0, pending: None }
        }

        pub fn set_debounce(&mut self, ms: u32) {
                self.debounce_ms = ms;
        }

        /// Sample and advance the debounce. Returns the edge if the debounced state changed.
        /// Safe, and expected, to call far more often than the button changes.
        pub fn poll(&mut self, now_ms: u32) -> Option<ButtonEvent> {
                let low = self.pin.is_low();
                let raw = if self.active_low { low } else { !low };
                if raw != self.raw_last {
                        self.raw_last = raw;
                        self.raw_change_ms = now_ms;
                        return None;
                }
                if raw == self.pressed {
                        return None;
                }
                if now_ms.wrapping_sub(self.raw_change_ms) < self.debounce_ms {
                        return None;
                }
                self.pressed = raw;
                let ev = if raw { ButtonEvent::Press } else { ButtonEvent::Release };
                self.pending = Some(ev);
                Some(ev)
        }

        /// Collect the pending edge once. Only one is held; edges alternate, so a dropped pair
        /// still leaves `pressed` correct.
        pub fn take(&mut self) -> Option<ButtonEvent> {
                self.pending.take()
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::Cell;

        struct Pin<'a>(&'a Cell<bool>);
        impl InputPin for Pin<'_> {
                fn is_low(&self) -> bool {
                        self.0.get()
                }
        }

        #[test]
        fn bounce_is_swallowed_and_a_settled_level_is_reported_once() {
                let low = Cell::new(false);
                let mut b = Button::new(Pin(&low), true);
                //   press with 6 ms of chatter, then stable
                let mut t = 0;
                for i in 0..6 {
                        low.set(i % 2 == 0);
                        assert_eq!(b.poll(t), None);
                        t += 1;
                }
                low.set(true);
                for _ in 0..19 {
                        assert_eq!(b.poll(t), None);
                        t += 1;
                }
                // 20 ms of stability after the last change
                t = 6 + 20;
                assert_eq!(b.poll(t), Some(ButtonEvent::Press));
                assert!(b.pressed);
                assert_eq!(b.poll(t + 1), None, "held, not repeated");
                assert_eq!(b.take(), Some(ButtonEvent::Press));
                assert_eq!(b.take(), None);
                low.set(false);
                assert_eq!(b.poll(t + 2), None);
                assert_eq!(b.poll(t + 22), Some(ButtonEvent::Release));
        }

        #[test]
        fn active_high_wiring_inverts_the_sense() {
                let low = Cell::new(true);
                let mut b = Button::new(Pin(&low), false);
                assert_eq!(b.poll(0), None);
                assert_eq!(b.poll(25), None, "low is released for an active-high button");
                low.set(false);
                assert_eq!(b.poll(26), None);
                assert_eq!(b.poll(47), Some(ButtonEvent::Press));
        }
}
