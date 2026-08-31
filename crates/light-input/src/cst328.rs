//! CST328 capacitive touch controller (Hynitron), ported from mk3's `light_touch_cst328`.
//!
//! NOT a bigger CST816: registers are 16 bits wide (the reason `I2cBus` grew its 16-bit
//! operations), the address differs, mode changes are address-only transactions with no data
//! byte, the coordinates are packed 12-bit rather than nibble-masked bytes, and there is no
//! gesture engine at all -- the software tracker carries swipes on this part.
//!
//! The poll architecture -- cadence, the INT read floor, backoff, quiet mode, non-blocking
//! reset recovery, inferred release -- is [`cst816t`](crate::cst816t)'s wholesale: every one
//! of those defenses is against the shared bus and a controller that sleeps, not against
//! anything CST816-specific, and each was earned against a measured failure there. What this
//! part changes is only the wire protocol underneath them.
//!
//! The register map is cross-referenced from two independent open-source drivers for the
//! Waveshare RP2350-Touch-LCD-2.8 (the first board here to carry this chip), not from a
//! primary datasheet; the timing constants are the CST816T's measured values carried over as
//! informed defaults. Bring-up checklist: the 0xCACA probe answers, coordinates track a
//! finger, and the cadence numbers get re-derived if the panel feels deaf or laggy.

use light_core::hal::{Clock, I2cBus, InputPin, OutputPin};

/// The one event type for touch controllers: the tracker and the applications match on it.
pub use crate::cst816t::Event;

pub const I2C_ADDR: u8 = 0x1A;

/// Mode changes: the 16-bit register address alone IS the command, written with no data byte
/// as a complete transaction.
pub const REG_MODE_DEBUG_INFO: u16 = 0xD101;
pub const REG_MODE_NORMAL: u16 = 0xD109;
/// Firmware info word, readable in debug-info mode only. The top half is a fixed 0xCACA --
/// the only presence check this part offers; there is no chip-ID register.
pub const REG_INFO_FW: u16 = 0xD1FC;
pub const INFO_FW_MARKER: u8 = 0xCA;
/// First contact record: [state, XH, YH, XL/YL, pressure]. The chip reports five contacts;
/// only the first is read -- the model is single-touch throughout, so the rest would be bus
/// traffic and dead code (5 bytes instead of 27).
pub const REG_FINGER_1: u16 = 0xD000;
const FINGER_DATA_LEN: usize = 5;
const FINGER_STATE_DOWN: u8 = 0x06;

// Timing: the CST816T's hardware-measured values, inherited as defaults -- the failure modes
// they guard belong to the bus and the poll loop, not the chip. UNVERIFIED on the CST328.
const POLL_INTERVAL_MS: u32 = 10;
const INT_READ_FLOOR_MS: u32 = 4;
const BACKOFF_MAX_MS: u32 = 160;
const QUIET_AFTER_FAILS: u8 = 4;
const RELEASE_TIMEOUT_MS: u32 = 60;
const RELEASE_MIN_POLLS: u32 = 8;
const STALL_RELEASE_MS: u32 = 500;
const RECOVER_AFTER_MS: u32 = 2000;
const RECOVER_COOLDOWN_MS: u32 = 1000;
const RESET_HOLD_MS: u32 = 10;
/// Longer than the CST816T's 50: this part is reported to need ~120 ms before it answers.
const RESET_BOOT_MS: u32 = 130;

pub struct Cst328<B: I2cBus, I: InputPin, R: OutputPin> {
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
        unanswered_since_ms: u32,
        last_recover_ms: u32,
        resetting: bool,
        reset_asserted: bool,
        reset_started_ms: u32,
        pub failures: u32,
        pub nacks: u32,
        pub timeouts: u32,
        pub bus_errors: u32,
        pub recoveries: u32,
}

impl<B: I2cBus, I: InputPin, R: OutputPin> crate::touch::HardwareGestures for Cst328<B, I, R> {
        fn read_gesture(&mut self) -> Option<crate::touch::Swipe> {
                // no gesture engine on this part: decline unconditionally and the tracker
                // classifies from coordinates -- the hw/sw split was built for exactly this
                None
        }
}

impl<B: I2cBus, I: InputPin, R: OutputPin> Cst328<B, I, R> {
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
                }
        }

        /// The BLOCKING reset, for init only. The 150 ms tail covers the ~120 ms this part is
        /// reported to need before it answers; the retried probe absorbs the remainder.
        pub fn reset_blocking(&mut self, clock: &mut dyn Clock) {
                self.reset.set(true);
                clock.delay_ms(50);
                self.reset.set(false);
                clock.delay_ms(10);
                self.reset.set(true);
                clock.delay_ms(150);
                self.resetting = false;
                self.reset_asserted = false;
        }

        /// The presence probe: switch to debug-info mode, read the firmware word, check the
        /// 0xCACA marker -- then back to NORMAL MODE REGARDLESS, because debug mode reports no
        /// touches and failing to leave it would turn a cosmetic problem into a dead panel.
        /// `Ok(Some(_))` marker seen; `Ok(None)` answered without it (log and continue: the
        /// map is unverified); `Err` no answer.
        pub fn probe(&mut self) -> Result<Option<u32>, light_core::hal::I2cError> {
                let r = (|| {
                        self.bus.write_command16(I2C_ADDR, REG_MODE_DEBUG_INFO)?;
                        let mut info = [0u8; 4];
                        self.bus.read_register16(I2C_ADDR, REG_INFO_FW, &mut info)?;
                        Ok(if info[2] == INFO_FW_MARKER && info[3] == INFO_FW_MARKER {
                                Some(u32::from_le_bytes(info))
                        } else {
                                None
                        })
                })();
                let _ = self.bus.write_command16(I2C_ADDR, REG_MODE_NORMAL);
                r
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
                self.unanswered_since_ms = 0;
        }

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
                //   the reset lands the chip in normal reporting mode's power-on default, so
                // nothing must be re-programmed; the confirming transaction is the caller's
                // probe on Event::Reset
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
                let allowed = int_asserted || !quiet;
                let since_attempt = now_ms.wrapping_sub(self.last_attempt_ms);
                let due = (int_asserted && self.unanswered == 0 && since_attempt >= INT_READ_FLOOR_MS)
                        || since_attempt >= self.interval_ms();
                if !allowed || !due {
                        return self.idle(now_ms);
                }
                self.last_attempt_ms = now_ms;

                let mut data = [0u8; FINGER_DATA_LEN];
                if let Err(e) = self.bus.read_register16(I2C_ADDR, REG_FINGER_1, &mut data) {
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

                //   first contact record: state byte, then packed 12-bit coordinates -- X's
                // top eight bits in byte 1, Y's in byte 2, the two low nibbles sharing byte 3
                // with X in the high nibble. Byte 4 is pressure, unused.
                let was_active = self.active;
                self.active = (data[0] & 0x0F) == FINGER_STATE_DOWN;
                if self.active {
                        self.x = (u16::from(data[1]) << 4) | u16::from(data[3] >> 4);
                        self.y = (u16::from(data[2]) << 4) | u16::from(data[3] & 0x0F);
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
        use crate::touch::HardwareGestures;
        use core::cell::{Cell, RefCell};
        use light_core::hal::I2cError;
        use std::rc::Rc;
        use std::vec::Vec;

        extern crate std;

        struct MockBus {
                answer: Rc<Cell<Option<[u8; 5]>>>,
                /// (register, was_command) in order -- checks the 16-bit framing and sequence.
                log: Rc<RefCell<Vec<(u16, bool)>>>,
                fw: [u8; 4],
        }
        impl I2cBus for MockBus {
                fn read_register(&mut self, _: u8, _: u8, _: &mut [u8]) -> Result<(), I2cError> {
                        panic!("the CST328 has no 8-bit registers");
                }
                fn write_register_byte(&mut self, _: u8, _: u8, _: u8) -> Result<(), I2cError> {
                        panic!("the CST328 has no 8-bit registers");
                }
                fn read_register16(&mut self, addr: u8, reg: u16, out: &mut [u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        self.log.borrow_mut().push((reg, false));
                        if reg == REG_INFO_FW {
                                out.copy_from_slice(&self.fw);
                                return Ok(());
                        }
                        match self.answer.get() {
                                Some(f) => {
                                        out.copy_from_slice(&f[..out.len()]);
                                        Ok(())
                                }
                                None => Err(I2cError::Nack),
                        }
                }
                fn write_command16(&mut self, addr: u8, reg: u16) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        self.log.borrow_mut().push((reg, true));
                        Ok(())
                }
        }
        struct MockInt(Rc<Cell<bool>>);
        impl InputPin for MockInt {
                fn is_low(&self) -> bool {
                        self.0.get()
                }
        }
        struct MockReset;
        impl OutputPin for MockReset {
                fn set(&mut self, _: bool) {}
        }

        fn rig(fw: [u8; 4]) -> (Rc<Cell<Option<[u8; 5]>>>, Rc<Cell<bool>>, Rc<RefCell<Vec<(u16, bool)>>>, Cst328<MockBus, MockInt, MockReset>) {
                let answer = Rc::new(Cell::new(None));
                let int = Rc::new(Cell::new(false));
                let log = Rc::new(RefCell::new(Vec::new()));
                let t = Cst328::new(MockBus { answer: answer.clone(), log: log.clone(), fw }, MockInt(int.clone()), MockReset, 0);
                (answer, int, log, t)
        }

        /// [state, XH, YH, XL|YL, pressure] with the 12-bit packing.
        fn finger(down: bool, x: u16, y: u16) -> [u8; 5] {
                [if down { FINGER_STATE_DOWN } else { 0 }, (x >> 4) as u8, (y >> 4) as u8, (((x & 0xF) << 4) | (y & 0xF)) as u8, 0]
        }

        #[test]
        fn the_probe_switches_modes_and_always_switches_back() {
                let (_, _, log, mut t) = rig([0x12, 0x34, 0xCA, 0xCA]);
                assert!(matches!(t.probe(), Ok(Some(_))));
                assert_eq!(*log.borrow(), [(REG_MODE_DEBUG_INFO, true), (REG_INFO_FW, false), (REG_MODE_NORMAL, true)]);
                //   and without the marker: answered, unconfirmed, but NORMAL mode restored --
                // debug mode reports no touches, so staying in it is a dead panel
                let (_, _, log, mut t) = rig([0, 0, 0, 0]);
                assert!(matches!(t.probe(), Ok(None)));
                assert_eq!(log.borrow().last(), Some(&(REG_MODE_NORMAL, true)));
        }

        #[test]
        fn packed_coordinates_decode_and_a_touch_lives_a_full_life() {
                let (answer, int, _, mut t) = rig([0; 4]);
                answer.set(Some(finger(true, 0x123, 0x256)));
                int.set(true);
                assert_eq!(t.poll(POLL_INTERVAL_MS), Some(Event::Down { x: 0x123, y: 0x256 }));
                answer.set(Some(finger(true, 0x124, 0x257)));
                assert_eq!(t.poll(POLL_INTERVAL_MS + INT_READ_FLOOR_MS), Some(Event::Move { x: 0x124, y: 0x257 }));
                int.set(false);
                answer.set(Some(finger(false, 0, 0)));
                assert_eq!(t.poll(POLL_INTERVAL_MS + INT_READ_FLOOR_MS + POLL_INTERVAL_MS), Some(Event::Up));
        }

        #[test]
        fn no_gesture_engine_means_the_tracker_classifies() {
                let (_, _, _, mut t) = rig([0; 4]);
                assert_eq!(t.read_gesture(), None);
        }

        #[test]
        fn unanswered_reads_back_off_and_a_stuck_int_earns_a_reset() {
                let (answer, int, _, mut t) = rig([0; 4]);
                answer.set(None);
                int.set(true);
                let mut reset_event_at = None;
                for now in 0..4000u32 {
                        if t.poll(now) == Some(Event::Reset) {
                                reset_event_at = Some(now);
                                break;
                        }
                }
                let at = reset_event_at.expect("recovery reset fired");
                assert!(at > RECOVER_AFTER_MS, "not before the wedge threshold: {at}");
                assert_eq!(t.recoveries, 1);
        }
}
