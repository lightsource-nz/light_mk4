//! Gesture recognition over a stream of touch samples, ported from the predecessor framework's gesture layer.
//!
//! A controller driver reports down/move/up samples; this tracks a touch from its down to its
//! release and classifies the drag as a swipe when the finger lifts -- so the reported start and
//! end are both final. The controller's own gesture engine, if it has one, gets first refusal
//! for the CLASSIFICATION, but the endpoints always come from this tracking: no controller
//! reports where a gesture happened. A consumer that spent the movement itself (a drag-scroll)
//! can `suppress` the touch in progress so its release is not also reported as a swipe.
//!
//! Directions are in the device's own coordinate space -- the panel's physical orientation.
//! An application drawing through a rotated canvas maps them itself.

use crate::cst816t::Event;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swipe {
        /// Toward y = 0.
        Up,
        Down,
        /// Toward x = 0.
        Left,
        Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gesture {
        pub swipe: Swipe,
        pub start: (u16, u16),
        pub end: (u16, u16),
        /// Classified by the controller's engine rather than from the sampled coordinates.
        pub from_hardware: bool,
}

/// A controller with its own gesture engine answers this as a touch ends. A pure query: it
/// must not report a stale result from an earlier touch.
pub trait HardwareGestures {
        fn read_gesture(&mut self) -> Option<Swipe>;
}

pub struct Tracker {
        swipe_min_distance: u16,
        tracking: bool,
        start: (u16, u16),
        last: (u16, u16),
        suppressed: bool,
        pending: Option<Gesture>,
}

impl Tracker {
        /// The swipe threshold defaults to an eighth of the shorter axis, so it scales with the
        /// panel rather than suiting whichever display it was developed against.
        pub fn new(x_max: u16, y_max: u16) -> Self {
                Self { swipe_min_distance: x_max.min(y_max) / 8, tracking: false, start: (0, 0), last: (0, 0), suppressed: false, pending: None }
        }

        pub fn set_swipe_min_distance(&mut self, distance: u16) {
                self.swipe_min_distance = distance;
        }

        /// Claim the touch in progress: its release classifies as nothing. Gated on a touch
        /// being in progress, so a stray call cannot eat the NEXT gesture.
        pub fn suppress(&mut self) {
                if self.tracking {
                        self.suppressed = true;
                }
        }

        pub fn tracking(&self) -> bool {
                self.tracking
        }

        /// Feed one sample. Returns a gesture when this sample ended a touch that was one.
        pub fn feed(&mut self, sample: Event, hardware: Option<&mut dyn HardwareGestures>) -> Option<Gesture> {
                match sample {
                        Event::Down { x, y } => {
                                self.tracking = true;
                                self.start = (x, y);
                                self.last = (x, y);
                                // each touch starts unclaimed
                                self.suppressed = false;
                                None
                        }
                        Event::Move { x, y } => {
                                if self.tracking {
                                        self.last = (x, y);
                                }
                                None
                        }
                        Event::Up => {
                                if !self.tracking {
                                        return None;
                                }
                                self.tracking = false;
                                if self.suppressed {
                                        return None;
                                }
                                let dx = i32::from(self.last.0) - i32::from(self.start.0);
                                let dy = i32::from(self.last.1) - i32::from(self.start.1);
                                let (swipe, from_hardware) = match hardware.and_then(|h| h.read_gesture()) {
                                        Some(s) => (Some(s), true),
                                        None => (self.classify(dx, dy), false),
                                };
                                let g = Gesture { swipe: swipe?, start: self.start, end: self.last, from_hardware };
                                self.pending = Some(g);
                                Some(g)
                        }
                        Event::Reset => None,
                }
        }

        /// The dominant axis wins, so a swipe only has to be mostly straight.
        fn classify(&self, dx: i32, dy: i32) -> Option<Swipe> {
                let min = i32::from(self.swipe_min_distance);
                if dx.abs() >= dy.abs() {
                        if dx.abs() < min {
                                return None;
                        }
                        Some(if dx > 0 { Swipe::Right } else { Swipe::Left })
                } else {
                        if dy.abs() < min {
                                return None;
                        }
                        // y grows downward in device coordinates
                        Some(if dy > 0 { Swipe::Down } else { Swipe::Up })
                }
        }

        /// Collect the pending gesture, so each is reported once however often this is asked.
        /// Only one is held; a second completing before the first is collected replaces it.
        pub fn take(&mut self) -> Option<Gesture> {
                self.pending.take()
        }
}

#[cfg(test)]
mod tests {
        use super::*;

        struct Hw(Option<Swipe>);
        impl HardwareGestures for Hw {
                fn read_gesture(&mut self) -> Option<Swipe> {
                        self.0.take()
                }
        }

        #[test]
        fn a_drag_past_the_threshold_is_a_swipe_along_its_dominant_axis() {
                let mut t = Tracker::new(240, 280); // threshold 30
                assert_eq!(t.feed(Event::Down { x: 100, y: 100 }, None), None);
                assert_eq!(t.feed(Event::Move { x: 120, y: 105 }, None), None);
                assert_eq!(t.feed(Event::Move { x: 150, y: 110 }, None), None);
                let g = t.feed(Event::Up, None).unwrap();
                assert_eq!(g.swipe, Swipe::Right);
                assert_eq!((g.start, g.end), ((100, 100), (150, 110)));
                assert!(!g.from_hardware);
                assert_eq!(t.take(), Some(g));
                assert_eq!(t.take(), None, "reported once");
                //   a tap: no travel, no gesture
                t.feed(Event::Down { x: 5, y: 5 }, None);
                assert_eq!(t.feed(Event::Up, None), None);
                //   upward: toward y = 0
                t.feed(Event::Down { x: 50, y: 200 }, None);
                t.feed(Event::Move { x: 55, y: 100 }, None);
                assert_eq!(t.feed(Event::Up, None).unwrap().swipe, Swipe::Up);
        }

        #[test]
        fn hardware_classification_wins_but_endpoints_are_ours() {
                let mut t = Tracker::new(240, 280);
                let mut hw = Hw(Some(Swipe::Left));
                t.feed(Event::Down { x: 10, y: 10 }, Some(&mut hw));
                t.feed(Event::Move { x: 12, y: 11 }, Some(&mut hw));
                let g = t.feed(Event::Up, Some(&mut hw)).unwrap();
                assert_eq!(g.swipe, Swipe::Left, "the engine's word, though we saw no travel");
                assert!(g.from_hardware);
                assert_eq!(g.end, (12, 11));
        }

        #[test]
        fn a_suppressed_touch_ends_in_nothing_and_the_next_touch_is_fresh() {
                let mut t = Tracker::new(240, 280);
                t.suppress(); // no touch in progress: must not leak forward
                t.feed(Event::Down { x: 0, y: 0 }, None);
                t.feed(Event::Move { x: 200, y: 0 }, None);
                assert_eq!(t.feed(Event::Up, None).unwrap().swipe, Swipe::Right, "a stray suppress did not eat it");
                t.feed(Event::Down { x: 0, y: 0 }, None);
                t.suppress();
                t.feed(Event::Move { x: 200, y: 0 }, None);
                assert_eq!(t.feed(Event::Up, None), None, "claimed by the scroller");
                t.feed(Event::Down { x: 0, y: 0 }, None);
                t.feed(Event::Move { x: 200, y: 0 }, None);
                assert!(t.feed(Event::Up, None).is_some(), "the one after is unclaimed");
        }
}
