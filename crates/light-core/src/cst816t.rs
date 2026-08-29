//! CST816T capacitive touch controller, reduced from mk3's `light_touch_cst816t`.
//!
//! Carried over: reading on a cadence rather than only on the interrupt (the INT pulse is
//! 1-3 ms and a loaded poll loop samples past it), backing off a controller that does not
//! answer (it auto-sleeps, and retrying a sleeping chip aborts transfers on a bus it shares with
//! the IMU), going quiet after enough failures and letting INT wake the cadence, and inferring a
//! release from silence while the loop was demonstrably looking.
//!
//! Left out for the spike: the non-blocking reset recovery for a controller that asserts INT
//! but will not answer. It is a state machine of its own and belongs in the port of the full
//! driver, not the feasibility pass.

use crate::bus::{I2cBus, InputPin};

pub const I2C_ADDR: u8 = 0x15;
pub const REG_GESTURE: u8 = 0x01;
pub const REG_CHIP_ID: u8 = 0xA7;
pub const CHIP_ID: u8 = 0xB5;
const FRAME_LEN: usize = 6;

/// Matched to the controller's own ~83 Hz report rate while a finger is down.
const POLL_INTERVAL_MS: u32 = 10;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
        Down { x: u16, y: u16 },
        Move { x: u16, y: u16 },
        Up,
}

pub struct Cst816t<B: I2cBus, P: InputPin> {
        bus: B,
        int: P,
        pub active: bool,
        pub x: u16,
        pub y: u16,
        last_report_ms: u32,
        last_attempt_ms: u32,
        idle_polls: u32,
        unanswered: u8,
        /// Reads that failed, in total, for the caller to report.
        pub failures: u32,
}

impl<B: I2cBus, P: InputPin> Cst816t<B, P> {
        pub fn new(bus: B, int: P, now_ms: u32) -> Self {
                Self {
                        bus,
                        int,
                        active: false,
                        x: 0,
                        y: 0,
                        last_report_ms: now_ms,
                        last_attempt_ms: now_ms,
                        idle_polls: 0,
                        unanswered: 0,
                        failures: 0,
                }
        }

        /// Read the chip ID. `Ok(Some(id))` when it answered, `Ok(None)` when the ID is not the
        /// expected one (log and continue: the map comes from open-source drivers, not a primary
        /// datasheet), `Err` when the bus did not answer at all.
        pub fn probe(&mut self) -> Result<Option<u8>, crate::bus::I2cError> {
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
                let timeout = if self.unanswered > 0 { STALL_RELEASE_MS } else { RELEASE_TIMEOUT_MS };
                if now_ms.wrapping_sub(self.last_report_ms) < timeout {
                        return None;
                }
                self.active = false;
                Some(Event::Up)
        }

        pub fn poll(&mut self, now_ms: u32) -> Option<Event> {
                let int_asserted = self.int.is_low();
                let quiet = self.unanswered >= QUIET_AFTER_FAILS;
                //   two questions: may the controller be read at all, and has enough time
                // passed. INT answers the first and used to answer both -- see mk3 for the
                // thousand aborted transfers that shortcut cost when INT was stuck asserted
                let allowed = int_asserted || !quiet;
                let due = (int_asserted && self.unanswered == 0)
                        || now_ms.wrapping_sub(self.last_attempt_ms) >= self.interval_ms();
                if !allowed || !due {
                        self.idle_polls = self.idle_polls.saturating_add(1);
                        return self.infer_release(now_ms);
                }
                self.last_attempt_ms = now_ms;

                let mut data = [0u8; FRAME_LEN];
                if self.bus.read_register(I2C_ADDR, REG_GESTURE, &mut data).is_err() {
                        self.failures = self.failures.wrapping_add(1);
                        if self.unanswered < QUIET_AFTER_FAILS {
                                self.unanswered += 1;
                        }
                        self.idle_polls = self.idle_polls.saturating_add(1);
                        return self.infer_release(now_ms);
                }

                self.unanswered = 0;
                self.idle_polls = 0;
                self.last_report_ms = now_ms;

                let fingers = data[1];
                let was_active = self.active;
                self.active = fingers > 0;
                if self.active {
                        self.x = (u16::from(data[2] & 0x0F) << 8) | u16::from(data[3]);
                        self.y = (u16::from(data[4] & 0x0F) << 8) | u16::from(data[5]);
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
        use crate::bus::I2cError;
        use core::cell::Cell;
        use std::rc::Rc;

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

        struct Rig {
                answer: Rc<Cell<Option<[u8; 6]>>>,
                reads: Rc<Cell<u32>>,
                int: Rc<Cell<bool>>,
                touch: Cst816t<MockBus, MockInt>,
        }
        fn rig() -> Rig {
                let answer = Rc::new(Cell::new(None));
                let reads = Rc::new(Cell::new(0));
                let int = Rc::new(Cell::new(false));
                let touch = Cst816t::new(
                        MockBus { answer: answer.clone(), reads: reads.clone() },
                        MockInt(int.clone()),
                        0,
                );
                Rig { answer, reads, int, touch }
        }
        fn frame(fingers: u8, x: u16, y: u16) -> [u8; 6] {
                [0, fingers, (x >> 8) as u8, x as u8, (y >> 8) as u8, y as u8]
        }

        #[test]
        fn a_touch_is_a_down_then_moves_then_a_read_release() {
                let mut r = rig();
                r.answer.set(Some(frame(1, 100, 200)));
                r.int.set(true);
                assert_eq!(r.touch.poll(0), Some(Event::Down { x: 100, y: 200 }));
                r.answer.set(Some(frame(1, 101, 202)));
                //   INT still asserted and the controller answering: read at once
                assert_eq!(r.touch.poll(1), Some(Event::Move { x: 101, y: 202 }));
                r.int.set(false);
                r.answer.set(Some(frame(0, 0, 0)));
                assert_eq!(r.touch.poll(5), None, "not due yet without INT");
                assert_eq!(r.touch.poll(11), Some(Event::Up));
        }

        #[test]
        fn a_silent_but_answering_controller_infers_release() {
                let mut r = rig();
                r.answer.set(Some(frame(1, 10, 10)));
                r.int.set(true);
                assert_eq!(r.touch.poll(0), Some(Event::Down { x: 10, y: 10 }));
                r.int.set(false);
                //   the loop keeps looking (polls between reads count as idle), but the
                // controller reports nothing new for 60 ms...
                for t in 1..60 {
                        //   answers with the SAME frame keep the report time fresh, so make the
                        // reads fail as a sleeping controller would -- while INT stays idle
                        r.answer.set(None);
                        let _ = r.touch.poll(t);
                }
                //   ...failing reads mean a stalled bus, which gets the long rope, not 60 ms
                assert!(r.touch.active);
                let mut ev = None;
                for t in 60..600 {
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
                let mut read_times = std::vec::Vec::new();
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
                let gaps: std::vec::Vec<u32> = read_times.windows(2).map(|w| w[1] - w[0]).collect();
                assert_eq!(gaps, [20, 40, 80]);
                //   INT gets it back in immediately, even though the interval has not elapsed
                r.answer.set(Some(frame(1, 5, 5)));
                r.int.set(true);
                assert_eq!(r.touch.poll(t), Some(Event::Down { x: 5, y: 5 }));
        }
}
