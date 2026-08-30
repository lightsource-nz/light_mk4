//! CST816T capacitive touch controller, ported from mk3's `light_touch_cst816t`.
//!
//! Carried over: reading on a cadence rather than only on the interrupt (the INT pulse is
//! 1-3 ms and a loaded poll loop samples past it), backing off a controller that does not
//! answer (it auto-sleeps, and retrying a sleeping chip aborts transfers on a bus it shares with
//! the IMU), going quiet after enough failures and letting INT wake the cadence, inferring a
//! release from silence while the loop was demonstrably looking -- and the non-blocking reset
//! recovery for a controller that asserts INT but will not answer. That last one was left out
//! of the first cut of this port, and the panel went deaf after four taps on the first run: the
//! failure mk3 documented, reproduced on the first try. The war stories are the spec.

use light_core::hal::{Clock, I2cBus, InputPin, OutputPin};

pub const I2C_ADDR: u8 = 0x15;
pub const REG_GESTURE: u8 = 0x01;
pub const REG_CHIP_ID: u8 = 0xA7;
pub const CHIP_ID: u8 = 0xB5;
const FRAME_LEN: usize = 6;

/// Matched to the controller's own ~83 Hz report rate while a finger is down.
const POLL_INTERVAL_MS: u32 = 10;
/// The least time between two reads even when INT says data is ready. mk3 read "on the spot"
/// from a ~1 kHz loop, so at most a read or two per 1-3 ms pulse; this runtime polls a few
/// hundred thousand times a second, and without a floor the same rule issued back-to-back
/// reads for the whole pulse -- and the controller wedged every few seconds of tapping.
const INT_READ_FLOOR_MS: u32 = 4;
/// Doubling per consecutive unanswered read, up to this.
const BACKOFF_MAX_MS: u32 = 160;
/// After this many unanswered reads the cadence stops and INT is the only way back in.
const QUIET_AFTER_FAILS: u8 = 4;
/// No report for this long, from a controller that IS answering, means the finger lifted.
const RELEASE_TIMEOUT_MS: u32 = 60;
/// ...but only if this many polls actually looked, so a stalled loop does not expire a touch.
const RELEASE_MIN_POLLS: u32 = 8;
/// A failing bus says nothing about the finger, so it gets a much longer rope.
const STALL_RELEASE_MS: u32 = 500;
/// A controller asserting INT but unanswered for this long is wedged, not napping: reset it.
const RECOVER_AFTER_MS: u32 = 2000;
/// ...and not again within this, or a chip still booting would be held in reset forever.
const RECOVER_COOLDOWN_MS: u32 = 1000;
/// The recovery pulse: held low this long, then left alone this long to boot. Timed against
/// polls rather than slept through -- a 60 ms sleep here lands mid-drag.
const RESET_HOLD_MS: u32 = 10;
const RESET_BOOT_MS: u32 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
        Down { x: u16, y: u16 },
        Move { x: u16, y: u16 },
        Up,
        /// The recovery reset fired. Reported so the caller can count it.
        Reset,
}

pub struct Cst816t<B: I2cBus, I: InputPin, R: OutputPin> {
        bus: B,
        int: I,
        reset: R,
        pub active: bool,
        pub x: u16,
        pub y: u16,
        last_report_ms: u32,
        last_attempt_ms: u32,
        idle_polls: u32,
        unanswered: u8,
        /// When the current run of unanswered reads began; 0 when answering.
        unanswered_since_ms: u32,
        last_recover_ms: u32,
        resetting: bool,
        reset_asserted: bool,
        reset_started_ms: u32,
        /// Reads that failed, in total, for the caller to report.
        pub failures: u32,
        /// ...broken down by kind: NACK (asleep), timeout (stretching or stuck), other.
        pub nacks: u32,
        pub timeouts: u32,
        pub bus_errors: u32,
        /// Recovery resets fired, in total.
        pub recoveries: u32,
        /// The controller's own gesture code, latched during the current touch: the engine
        /// reports in whichever frame it recognises the gesture, not necessarily the release
        /// frame. Cleared on each new down so a stale code cannot attach to the next touch.
        last_gesture: u8,
}

const GESTURE_NONE: u8 = 0x00;
const GESTURE_SWIPE_UP: u8 = 0x01;
const GESTURE_SWIPE_DOWN: u8 = 0x02;
const GESTURE_SWIPE_LEFT: u8 = 0x03;
const GESTURE_SWIPE_RIGHT: u8 = 0x04;

impl<B: I2cBus, I: InputPin, R: OutputPin> crate::touch::HardwareGestures for Cst816t<B, I, R> {
        fn read_gesture(&mut self) -> Option<crate::touch::Swipe> {
                use crate::touch::Swipe;
                let code = core::mem::replace(&mut self.last_gesture, GESTURE_NONE);
                // the vertical codes are mapped to their OPPOSITE, which is what the hardware
                // actually does: the code the reference drivers call "swipe up" is reported for
                // a swipe toward increasing y. Confirmed on hardware by mk3, vertical only.
                match code {
                        GESTURE_SWIPE_UP => Some(Swipe::Down),
                        GESTURE_SWIPE_DOWN => Some(Swipe::Up),
                        GESTURE_SWIPE_LEFT => Some(Swipe::Left),
                        GESTURE_SWIPE_RIGHT => Some(Swipe::Right),
                        // nothing latched, or a click/long-press code the tracker has no
                        // equivalent for: decline, and the tracker classifies from coordinates
                        _ => None,
                }
        }
}

impl<B: I2cBus, I: InputPin, R: OutputPin> Cst816t<B, I, R> {
        pub fn new(bus: B, int: I, reset: R, now_ms: u32) -> Self {
                Self {
                        bus,
                        int,
                        reset,
                        active: false,
                        x: 0,
                        y: 0,
                        last_report_ms: now_ms,
                        last_attempt_ms: now_ms,
                        idle_polls: 0,
                        unanswered: 0,
                        unanswered_since_ms: 0,
                        last_recover_ms: 0,
                        resetting: false,
                        reset_asserted: false,
                        reset_started_ms: 0,
                        failures: 0,
                        nacks: 0,
                        timeouts: 0,
                        bus_errors: 0,
                        recoveries: 0,
                        last_gesture: GESTURE_NONE,
                }
        }

        /// The BLOCKING reset, for init: high, low, high, 100 ms each. Correct during module
        /// load where nothing is rendering; the recovery path in `poll` is the one that must
        /// not sleep. Probe immediately after -- the controller sleeps within about a second.
        pub fn reset_blocking(&mut self, clock: &mut dyn Clock) {
                self.reset.set(true);
                clock.delay_ms(100);
                self.reset.set(false);
                clock.delay_ms(100);
                self.reset.set(true);
                clock.delay_ms(100);
                self.resetting = false;
                self.reset_asserted = false;
        }

        /// Read the chip ID. `Ok(Some(id))` when it answered, `Ok(None)` when the ID is not the
        /// expected one (log and continue: the map comes from open-source drivers, not a primary
        /// datasheet), `Err` when the bus did not answer at all.
        pub fn probe(&mut self) -> Result<Option<u8>, light_core::hal::I2cError> {
                let mut id = [0u8];
                self.bus.read_register(I2C_ADDR, REG_CHIP_ID, &mut id)?;
                Ok(if id[0] == CHIP_ID { Some(id[0]) } else { None })
        }

        /// The data-ready line, as a level, for diagnostics.
        pub fn int_asserted(&self) -> bool {
                self.int.is_low()
        }

        fn interval_ms(&self) -> u32 {
                (POLL_INTERVAL_MS << self.unanswered).min(BACKOFF_MAX_MS)
        }

        fn infer_release(&mut self, now_ms: u32) -> Option<Event> {
                if !self.active || self.idle_polls < RELEASE_MIN_POLLS {
                        return None;
                }
                let timeout = if self.unanswered > 0 || self.resetting { STALL_RELEASE_MS } else { RELEASE_TIMEOUT_MS };
                if now_ms.wrapping_sub(self.last_report_ms) < timeout {
                        return None;
                }
                self.active = false;
                Some(Event::Up)
        }

        fn idle(&mut self, now_ms: u32) -> Option<Event> {
                self.idle_polls = self.idle_polls.saturating_add(1);
                self.infer_release(now_ms)
        }

        fn begin_reset(&mut self, now_ms: u32) {
                self.reset.set(false);
                self.resetting = true;
                self.reset_asserted = true;
                self.reset_started_ms = now_ms;
                self.last_recover_ms = now_ms;
                self.recoveries = self.recoveries.wrapping_add(1);
                //   measured from the start of each run, not accumulated across resets --
                // left uncleared it stays past RECOVER_AFTER_MS and the cooldown alone paces
                // further resets
                self.unanswered_since_ms = 0;
        }

        /// One phase per poll. Nothing reads the bus while this is in progress: every read
        /// against a chip held in reset or still booting is an aborted transfer.
        fn service_reset(&mut self, now_ms: u32) -> bool {
                let elapsed = now_ms.wrapping_sub(self.reset_started_ms);
                if self.reset_asserted {
                        if elapsed >= RESET_HOLD_MS {
                                self.reset.set(true);
                                self.reset_asserted = false;
                        }
                        return false;
                }
                if elapsed < RESET_HOLD_MS + RESET_BOOT_MS {
                        return false;
                }
                self.resetting = false;
                //   the failure run starts again from the chip that is up NOW
                self.unanswered = 0;
                self.unanswered_since_ms = 0;
                self.last_attempt_ms = now_ms;
                true
        }

        pub fn poll(&mut self, now_ms: u32) -> Option<Event> {
                if self.resetting {
                        if self.service_reset(now_ms) {
                                return Some(Event::Reset);
                        }
                        return self.idle(now_ms);
                }

                let int_asserted = self.int.is_low();
                let quiet = self.unanswered >= QUIET_AFTER_FAILS;
                //   two questions: may the controller be read at all, and has enough time
                // passed. INT answers the first and used to answer both -- see mk3 for the
                // thousand aborted transfers that shortcut cost when INT was stuck asserted
                let allowed = int_asserted || !quiet;
                let since_attempt = now_ms.wrapping_sub(self.last_attempt_ms);
                let due = (int_asserted && self.unanswered == 0 && since_attempt >= INT_READ_FLOOR_MS)
                        || since_attempt >= self.interval_ms();
                if !allowed || !due {
                        return self.idle(now_ms);
                }
                self.last_attempt_ms = now_ms;

                let mut data = [0u8; FRAME_LEN];
                if let Err(e) = self.bus.read_register(I2C_ADDR, REG_GESTURE, &mut data) {
                        self.failures = self.failures.wrapping_add(1);
                        match e {
                                light_core::hal::I2cError::Nack => self.nacks += 1,
                                light_core::hal::I2cError::Timeout => self.timeouts += 1,
                                light_core::hal::I2cError::Bus => self.bus_errors += 1,
                        }
                        if self.unanswered < QUIET_AFTER_FAILS {
                                self.unanswered += 1;
                        }
                        if self.unanswered_since_ms == 0 {
                                self.unanswered_since_ms = now_ms.max(1);
                        }
                        //   asserting INT means it has data and wants to be read; not answering
                        // on top of that, for this long, is a wedge rather than a nap. a merely
                        // sleeping controller asserts nothing and is left alone
                        if int_asserted
                                && now_ms.wrapping_sub(self.unanswered_since_ms) > RECOVER_AFTER_MS
                                && now_ms.wrapping_sub(self.last_recover_ms) > RECOVER_COOLDOWN_MS
                        {
                                self.begin_reset(now_ms);
                        }
                        return self.idle(now_ms);
                }

                self.unanswered = 0;
                self.unanswered_since_ms = 0;
                self.idle_polls = 0;
                self.last_report_ms = now_ms;

                let gesture = data[0];
                let fingers = data[1];
                let was_active = self.active;
                self.active = fingers > 0;
                if self.active {
                        self.x = (u16::from(data[2] & 0x0F) << 8) | u16::from(data[3]);
                        self.y = (u16::from(data[4] & 0x0F) << 8) | u16::from(data[5]);
                }
                if self.active && !was_active {
                        // discard whatever the engine reported for the previous touch
                        self.last_gesture = GESTURE_NONE;
                }
                if gesture != GESTURE_NONE {
                        self.last_gesture = gesture;
                }
                match (was_active, self.active) {
                        (false, true) => Some(Event::Down { x: self.x, y: self.y }),
                        (true, true) => Some(Event::Move { x: self.x, y: self.y }),
                        (true, false) => Some(Event::Up),
                        (false, false) => None,
                }
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use light_core::hal::I2cError;
        use core::cell::{Cell, RefCell};
        use std::rc::Rc;
        use std::vec::Vec;

        extern crate std;

        struct MockBus {
                /// What the next reads return: a frame, or a NACK.
                answer: Rc<Cell<Option<[u8; 6]>>>,
                reads: Rc<Cell<u32>>,
        }
        impl I2cBus for MockBus {
                fn read_register(&mut self, _: u8, _: u8, out: &mut [u8]) -> Result<(), I2cError> {
                        self.reads.set(self.reads.get() + 1);
                        match self.answer.get() {
                                Some(f) => {
                                        out.copy_from_slice(&f[..out.len()]);
                                        Ok(())
                                }
                                None => Err(I2cError::Nack),
                        }
                }
                fn write_register_byte(&mut self, _: u8, _: u8, _: u8) -> Result<(), I2cError> {
                        Ok(())
                }
        }
        struct MockInt(Rc<Cell<bool>>);
        impl InputPin for MockInt {
                fn is_low(&self) -> bool {
                        self.0.get()
                }
        }
        /// Records every level change with the poll time it happened at.
        struct MockReset(Rc<RefCell<Vec<bool>>>);
        impl OutputPin for MockReset {
                fn set(&mut self, high: bool) {
                        self.0.borrow_mut().push(high);
                }
        }

        struct Rig {
                answer: Rc<Cell<Option<[u8; 6]>>>,
                reads: Rc<Cell<u32>>,
                int: Rc<Cell<bool>>,
                reset_log: Rc<RefCell<Vec<bool>>>,
                touch: Cst816t<MockBus, MockInt, MockReset>,
        }
        fn rig() -> Rig {
                let answer = Rc::new(Cell::new(None));
                let reads = Rc::new(Cell::new(0));
                let int = Rc::new(Cell::new(false));
                let reset_log = Rc::new(RefCell::new(Vec::new()));
                let touch = Cst816t::new(
                        MockBus { answer: answer.clone(), reads: reads.clone() },
                        MockInt(int.clone()),
                        MockReset(reset_log.clone()),
                        0,
                );
                Rig { answer, reads, int, reset_log, touch }
        }
        fn frame(fingers: u8, x: u16, y: u16) -> [u8; 6] {
                [0, fingers, (x >> 8) as u8, x as u8, (y >> 8) as u8, y as u8]
        }

        #[test]
        fn a_touch_is_a_down_then_moves_then_a_read_release() {
                let mut r = rig();
                r.answer.set(Some(frame(1, 100, 200)));
                r.int.set(true);
                let t0 = POLL_INTERVAL_MS;
                assert_eq!(r.touch.poll(t0), Some(Event::Down { x: 100, y: 200 }));
                r.answer.set(Some(frame(1, 101, 202)));
                //   INT still asserted and the controller answering: read again as soon as the
                // floor allows, ahead of the cadence -- but not back-to-back
                assert_eq!(r.touch.poll(t0 + 1), None, "inside the floor, no second read");
                let t1 = t0 + INT_READ_FLOOR_MS;
                assert_eq!(r.touch.poll(t1), Some(Event::Move { x: 101, y: 202 }));
                r.int.set(false);
                r.answer.set(Some(frame(0, 0, 0)));
                assert_eq!(r.touch.poll(t1 + POLL_INTERVAL_MS - 1), None, "not due yet without INT");
                assert_eq!(r.touch.poll(t1 + POLL_INTERVAL_MS), Some(Event::Up));
        }

        #[test]
        fn a_silent_bus_infers_release_on_the_long_rope() {
                let mut r = rig();
                r.answer.set(Some(frame(1, 10, 10)));
                r.int.set(true);
                assert_eq!(r.touch.poll(POLL_INTERVAL_MS), Some(Event::Down { x: 10, y: 10 }));
                r.int.set(false);
                r.answer.set(None);
                let mut ev = None;
                for t in POLL_INTERVAL_MS + 1..700 {
                        if let Some(e) = r.touch.poll(t) {
                                ev = Some((t, e));
                                break;
                        }
                }
                let (t, e) = ev.expect("release inferred");
                assert_eq!(e, Event::Up);
                assert!(t >= 500, "stall rope is 500 ms, released at {t}");
        }

        #[test]
        fn unanswered_reads_back_off_and_then_go_quiet_until_int() {
                let mut r = rig();
                r.answer.set(None);
                let mut t = 0u32;
                let mut read_times = Vec::new();
                while t < 2000 {
                        let before = r.reads.get();
                        let _ = r.touch.poll(t);
                        if r.reads.get() != before {
                                read_times.push(t);
                        }
                        t += 1;
                }
                //   the first read at the base interval, then 20, 40, 80 ms apart, then nothing:
                // the fourth failure is what makes the cadence stop
                assert_eq!(read_times.len(), QUIET_AFTER_FAILS as usize, "{read_times:?}");
                assert_eq!(read_times[0], POLL_INTERVAL_MS);
                let gaps: Vec<u32> = read_times.windows(2).map(|w| w[1] - w[0]).collect();
                assert_eq!(gaps, [20, 40, 80]);
                //   INT gets it back in immediately, even though the interval has not elapsed
                r.answer.set(Some(frame(1, 5, 5)));
                r.int.set(true);
                assert_eq!(r.touch.poll(t), Some(Event::Down { x: 5, y: 5 }));
                assert!(r.reset_log.borrow().is_empty(), "a napping controller is never reset");
        }

        #[test]
        fn int_asserted_but_unanswered_gets_a_reset_after_two_seconds() {
                let mut r = rig();
                r.answer.set(None);
                r.int.set(true);
                let mut reset_at = None;
                let mut released_at = None;
                let mut recovered_at = None;
                for t in 0..5000u32 {
                        let ev = r.touch.poll(t);
                        let log = r.reset_log.borrow();
                        if reset_at.is_none() && log.first() == Some(&false) {
                                reset_at = Some(t);
                        }
                        if reset_at.is_some() && released_at.is_none() && log.len() == 2 {
                                released_at = Some(t);
                        }
                        drop(log);
                        if ev == Some(Event::Reset) {
                                recovered_at = Some(t);
                                //   the chip is back; it answers now
                                r.answer.set(Some(frame(1, 1, 1)));
                        }
                        if recovered_at.is_some() {
                                if let Some(Event::Down { .. }) = ev {
                                        break;
                                }
                        }
                }
                let reset_at = reset_at.expect("reset fired");
                assert!(reset_at > RECOVER_AFTER_MS && reset_at < RECOVER_AFTER_MS + 200, "reset at {reset_at}");
                assert_eq!(released_at.unwrap() - reset_at, RESET_HOLD_MS);
                assert_eq!(recovered_at.unwrap() - reset_at, RESET_HOLD_MS + RESET_BOOT_MS);
                assert_eq!(r.touch.recoveries, 1);
                assert_eq!(*r.reset_log.borrow(), [false, true]);
        }

        #[test]
        fn resets_respect_the_cooldown() {
                let mut r = rig();
                r.answer.set(None);
                r.int.set(true);
                for t in 0..10_000u32 {
                        let _ = r.touch.poll(t);
                }
                //   never answering: one reset per RECOVER_AFTER + cooldown-ish window, not one
                // per failed read. 10 s allows a handful, not hundreds
                let n = r.touch.recoveries;
                assert!((2..=5).contains(&n), "{n} resets in 10 s");
        }
}
