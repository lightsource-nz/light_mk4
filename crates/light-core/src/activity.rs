//! A standard, port-agnostic activity beacon. Any input source -- a touch driver, an IMU, a button
//! -- reports user activity here without knowing who consumes it, and a power manager polls it to
//! keep the screen awake and defer power-off. This is how "the user is doing something" travels
//! from whatever hardware a board happens to have to the power policy, without either end naming the
//! other or the application's event vocabulary.
//!
//! It is a generation counter, not a timestamp: [`note_activity`] bumps it, and a consumer that
//! remembers a previous [`generation`] knows activity happened if it differs. So a reporter needs no
//! clock, and there is nothing to clear. One relaxed atomic increment, cross-core safe.

use portable_atomic::{AtomicU32, Ordering};

static GENERATION: AtomicU32 = AtomicU32::new(0);

/// Report user activity: a touch, a gesture, a deliberate motion, a button press. Call it from
/// wherever real user input is detected -- the reporter need not know who, if anyone, is watching.
pub fn note_activity() {
        GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The current activity generation. A caller that remembers a previous value knows activity
/// happened since if this differs -- how a power manager consumes the beacon.
pub fn generation() -> u32 {
        GENERATION.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
        #[test]
        fn note_bumps_the_generation() {
                //   this is the only test that touches the global, so the two reads bracket exactly
                // one increment
                let before = super::generation();
                super::note_activity();
                assert_eq!(super::generation(), before.wrapping_add(1));
        }
}
