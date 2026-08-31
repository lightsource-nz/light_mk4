//! The AXS15231B's touch half -- the same chip as the 3.49" panel's LCD controller, answering
//! on I2C. Not register-addressed at all: the host writes an 11-byte command blob and reads
//! the answer frame back raw, which is what `I2cBus`'s `write_raw`/`read_raw` exist for.
//! Protocol from Waveshare's reference driver; coordinates are 12-bit, the long axis 0..=640
//! and the short 0..=172 on this glass.
//!
//! The poll architecture is the cst816t/cst328 discipline -- cadence, INT read floor,
//! backoff, quiet mode, inferred release -- WITHOUT the reset recovery: the touch controller
//! IS the display controller, and its reset line is the panel's. Resetting a wedged touch
//! would black the glass mid-recovery, so a wedge here is reported and left to the
//! application (whose real remedy is the panel init path).

use light_core::hal::{I2cBus, InputPin};

pub use crate::cst816t::Event;

pub const I2C_ADDR: u8 = 0x3B;

/// The read-touch command, from the reference driver. Byte 7 (0x0E) smells like a length;
/// the rest is unexplained vendor framing, copied faithfully.
const READ_TOUCH_CMD: [u8; 11] = [0xB5, 0xAB, 0xA5, 0x5A, 0x00, 0x00, 0x00, 0x0E, 0x00, 0x00, 0x00];
/// The answer frame. The reference reads 32 bytes; the first point lives in [1..=5].
const READ_LEN: usize = 32;

const POLL_INTERVAL_MS: u32 = 10;
const INT_READ_FLOOR_MS: u32 = 4;
const BACKOFF_MAX_MS: u32 = 160;
const QUIET_AFTER_FAILS: u8 = 4;
const RELEASE_TIMEOUT_MS: u32 = 60;
const RELEASE_MIN_POLLS: u32 = 8;
const STALL_RELEASE_MS: u32 = 500;

/// How raw panel coordinates map onto the display's frame: the raw long axis is 0..=640 and
/// the raw short axis 0..=172, and which way each runs is a fact about the glass measured on
/// bring-up, not guessed.
#[derive(Clone, Copy, Debug)]
pub struct CoordMap {
        /// Raw long-axis extent (640 on the 3.49).
        pub long_max: u16,
        /// Raw short-axis extent (172 on the 3.49).
        pub short_max: u16,
        /// Invert the long axis (y on a portrait declaration).
        pub invert_long: bool,
        /// Invert the short axis (x on a portrait declaration).
        pub invert_short: bool,
}

pub struct Axs15231bTouch<B: I2cBus, I: InputPin> {
        bus: B,
        int: I,
        map: CoordMap,
        pub active: bool,
        pub x: u16,
        pub y: u16,
        last_report_ms: u32,
        last_attempt_ms: u32,
        idle_polls: u32,
        unanswered: u8,
        pub failures: u32,
        pub nacks: u32,
        pub timeouts: u32,
        pub bus_errors: u32,
}

impl<B: I2cBus, I: InputPin> crate::touch::HardwareGestures for Axs15231bTouch<B, I> {
        fn read_gesture(&mut self) -> Option<crate::touch::Swipe> {
                // no gesture engine surfaced by this protocol: the tracker classifies
                None
        }
}

impl<B: I2cBus, I: InputPin> Axs15231bTouch<B, I> {
        pub fn new(bus: B, int: I, map: CoordMap, now_ms: u32) -> Self {
                Self {
                        bus,
                        int,
                        map,
                        active: false,
                        x: 0,
                        y: 0,
                        last_report_ms: now_ms,
                        last_attempt_ms: now_ms,
                        idle_polls: 0,
                        unanswered: 0,
                        failures: 0,
                        nacks: 0,
                        timeouts: 0,
                        bus_errors: 0,
                }
        }

        /// Whether the part answers at all: the touch read itself is the only probe this
        /// protocol offers -- there is no ID register on the touch interface.
        pub fn probe(&mut self) -> Result<(), light_core::hal::I2cError> {
                self.bus.write_raw(I2C_ADDR, &READ_TOUCH_CMD)?;
                let mut frame = [0u8; READ_LEN];
                self.bus.read_raw(I2C_ADDR, &mut frame)
        }

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

        fn idle(&mut self, now_ms: u32) -> Option<Event> {
                self.idle_polls = self.idle_polls.saturating_add(1);
                self.infer_release(now_ms)
        }

        pub fn poll(&mut self, now_ms: u32) -> Option<Event> {
                let int_asserted = self.int.is_low();
                let quiet = self.unanswered >= QUIET_AFTER_FAILS;
                let allowed = int_asserted || !quiet;
                let since_attempt = now_ms.wrapping_sub(self.last_attempt_ms);
                let due = (int_asserted && self.unanswered == 0 && since_attempt >= INT_READ_FLOOR_MS)
                        || since_attempt >= self.interval_ms();
                if !allowed || !due {
                        return self.idle(now_ms);
                }
                self.last_attempt_ms = now_ms;

                let mut frame = [0u8; READ_LEN];
                let r = self.bus.write_raw(I2C_ADDR, &READ_TOUCH_CMD).and_then(|()| self.bus.read_raw(I2C_ADDR, &mut frame));
                if let Err(e) = r {
                        self.failures = self.failures.wrapping_add(1);
                        match e {
                                light_core::hal::I2cError::Nack => self.nacks += 1,
                                light_core::hal::I2cError::Timeout => self.timeouts += 1,
                                light_core::hal::I2cError::Bus => self.bus_errors += 1,
                        }
                        if self.unanswered < QUIET_AFTER_FAILS {
                                self.unanswered += 1;
                        }
                        return self.idle(now_ms);
                }

                self.unanswered = 0;
                self.idle_polls = 0;
                self.last_report_ms = now_ms;

                let fingers = frame[1];
                //   the first point: 12-bit long-axis value in [2..=3], short-axis in [4..=5]
                let mut long = (u16::from(frame[2] & 0x0F) << 8) | u16::from(frame[3]);
                let mut short = (u16::from(frame[4] & 0x0F) << 8) | u16::from(frame[5]);
                long = long.min(self.map.long_max);
                short = short.min(self.map.short_max);
                if self.map.invert_long {
                        long = self.map.long_max - long;
                }
                if self.map.invert_short {
                        short = self.map.short_max - short;
                }

                let was_active = self.active;
                self.active = fingers > 0 && fingers <= 2;
                if self.active {
                        //   the display is declared in portrait (short wide, long tall), so
                        // x is the short axis and y the long
                        self.x = short;
                        self.y = long;
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
        use core::cell::{Cell, RefCell};
        use light_core::hal::I2cError;
        use std::rc::Rc;
        use std::vec::Vec;

        extern crate std;

        struct MockBus {
                answer: Rc<Cell<Option<[u8; 6]>>>,
                writes: Rc<RefCell<Vec<Vec<u8>>>>,
        }
        impl I2cBus for MockBus {
                fn read_register(&mut self, _: u8, _: u8, _: &mut [u8]) -> Result<(), I2cError> {
                        panic!("not a register-addressed part");
                }
                fn write_register_byte(&mut self, _: u8, _: u8, _: u8) -> Result<(), I2cError> {
                        panic!("not a register-addressed part");
                }
                fn write_raw(&mut self, addr: u8, src: &[u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        self.writes.borrow_mut().push(src.to_vec());
                        if self.answer.get().is_none() {
                                return Err(I2cError::Nack);
                        }
                        Ok(())
                }
                fn read_raw(&mut self, addr: u8, dst: &mut [u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        assert_eq!(dst.len(), READ_LEN);
                        match self.answer.get() {
                                Some(head) => {
                                        dst[..6].copy_from_slice(&head);
                                        Ok(())
                                }
                                None => Err(I2cError::Nack),
                        }
                }
        }
        struct MockInt(Rc<Cell<bool>>);
        impl InputPin for MockInt {
                fn is_low(&self) -> bool {
                        self.0.get()
                }
        }

        fn frame(fingers: u8, long: u16, short: u16) -> [u8; 6] {
                [0, fingers, (long >> 8) as u8, long as u8, (short >> 8) as u8, short as u8]
        }

        #[test]
        fn the_command_blob_goes_out_and_coordinates_map_to_portrait() {
                let answer = Rc::new(Cell::new(None));
                let int = Rc::new(Cell::new(true));
                let writes = Rc::new(RefCell::new(Vec::new()));
                let map = CoordMap { long_max: 640, short_max: 172, invert_long: false, invert_short: false };
                let mut t = Axs15231bTouch::new(MockBus { answer: answer.clone(), writes: writes.clone() }, MockInt(int.clone()), map, 0);
                answer.set(Some(frame(1, 500, 100)));
                assert_eq!(t.poll(POLL_INTERVAL_MS), Some(Event::Down { x: 100, y: 500 }), "short axis is x, long is y");
                assert_eq!(writes.borrow()[0], READ_TOUCH_CMD.to_vec());
                //   out-of-range raw values clamp rather than escaping the glass
                answer.set(Some(frame(1, 0xFFF, 0xFFF)));
                assert_eq!(t.poll(POLL_INTERVAL_MS + INT_READ_FLOOR_MS), Some(Event::Move { x: 172, y: 640 }));
                answer.set(Some(frame(0, 0, 0)));
                assert_eq!(t.poll(POLL_INTERVAL_MS + INT_READ_FLOOR_MS + POLL_INTERVAL_MS), Some(Event::Up));
        }

        #[test]
        fn inversions_apply_after_clamping() {
                let answer = Rc::new(Cell::new(None));
                let int = Rc::new(Cell::new(true));
                let writes = Rc::new(RefCell::new(Vec::new()));
                let map = CoordMap { long_max: 640, short_max: 172, invert_long: true, invert_short: true };
                let mut t = Axs15231bTouch::new(MockBus { answer: answer.clone(), writes }, MockInt(int), map, 0);
                answer.set(Some(frame(1, 40, 12)));
                assert_eq!(t.poll(POLL_INTERVAL_MS), Some(Event::Down { x: 160, y: 600 }));
        }

        #[test]
        fn unanswered_reads_back_off_and_quiet_holds_once_int_releases() {
                let answer = Rc::new(Cell::new(None));
                let int = Rc::new(Cell::new(true));
                let writes = Rc::new(RefCell::new(Vec::new()));
                let map = CoordMap { long_max: 640, short_max: 172, invert_long: false, invert_short: false };
                let mut t = Axs15231bTouch::new(MockBus { answer, writes: writes.clone() }, MockInt(int.clone()), map, 0);
                for now in 0..3000u32 {
                        assert!(t.poll(now).is_none());
                }
                //   INT asserted claims data, so quiet mode still reads -- but only at the
                // backoff ceiling: ~3000/160 attempts, never one per poll. And with no reset
                // line there is nothing else this driver may do about a wedge
                assert!(t.failures >= 4);
                let while_int = writes.borrow().len();
                assert!((10..=25).contains(&while_int), "backoff ceiling paced the traffic: {while_int}");
                //   INT released: the quiet controller is left entirely alone
                int.set(false);
                for now in 3000..6000u32 {
                        assert!(t.poll(now).is_none());
                }
                assert_eq!(writes.borrow().len(), while_int, "no reads while quiet and INT idle");
        }
}
