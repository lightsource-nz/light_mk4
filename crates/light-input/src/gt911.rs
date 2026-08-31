//! Goodix GT911: a multi-touch controller with 16-bit registers, on the RGB panel boards.
//! Protocol per the datasheet via Waveshare's reference: status at 0x814E (bit 7 = report
//! ready, low nibble = touch count), points from 0x814F at 8 bytes each, and the status
//! byte must be written back to 0 to release the buffer.
//!
//! Reports are EXPLICIT in both directions -- a release arrives as a ready status with
//! zero points -- so this driver trusts them, with the report-silence timeout kept only as
//! a stall backstop. Only the first point is tracked; the framework's tracker classifies.
//!
//! Address selection: the part latches its I2C address from the INT level during reset,
//! which is board wiring -- the board holds INT low through reset for 0x5D, then hands
//! this driver the released INT as an input.

use light_core::hal::{I2cBus, I2cError, InputPin};

pub use crate::cst816t::Event;

pub const I2C_ADDR: u8 = 0x5D;

const REG_STATUS: u16 = 0x814E;
const REG_PRODUCT_ID: u16 = 0x8140;
const STATUS_READY: u8 = 0x80;

const POLL_INTERVAL_MS: u32 = 10;
const INT_READ_FLOOR_MS: u32 = 4;
const BACKOFF_MAX_MS: u32 = 160;
const QUIET_AFTER_FAILS: u8 = 4;
/// The stall backstop only: releases normally arrive as explicit zero-point reports.
const RELEASE_TIMEOUT_MS: u32 = 200;
const RELEASE_MIN_POLLS: u32 = 8;

/// How raw panel coordinates map onto the display's frame, measured on bring-up.
#[derive(Clone, Copy, Debug)]
pub struct CoordMap {
        pub x_max: u16,
        pub y_max: u16,
        pub invert_x: bool,
        pub invert_y: bool,
        /// Applied AFTER the inversions: x and y exchange (a square panel can hide this
        /// until measured).
        pub swap_xy: bool,
}

pub struct Gt911<B: I2cBus, I: InputPin> {
        bus: B,
        int: I,
        map: CoordMap,
        pub active: bool,
        pub x: u16,
        pub y: u16,
        last_active_ms: u32,
        last_attempt_ms: u32,
        idle_polls: u32,
        unanswered: u8,
        pub failures: u32,
        pub nacks: u32,
        pub timeouts: u32,
        pub bus_errors: u32,
}

impl<B: I2cBus, I: InputPin> crate::touch::HardwareGestures for Gt911<B, I> {
        fn read_gesture(&mut self) -> Option<crate::touch::Swipe> {
                None
        }
}

impl<B: I2cBus, I: InputPin> Gt911<B, I> {
        pub fn new(bus: B, int: I, map: CoordMap, now_ms: u32) -> Self {
                Self {
                        bus,
                        int,
                        map,
                        active: false,
                        x: 0,
                        y: 0,
                        last_active_ms: now_ms,
                        last_attempt_ms: now_ms,
                        idle_polls: 0,
                        unanswered: 0,
                        failures: 0,
                        nacks: 0,
                        timeouts: 0,
                        bus_errors: 0,
                }
        }

        /// The product id, `Some` when it spells "911".
        pub fn probe(&mut self) -> Result<Option<[u8; 4]>, I2cError> {
                let mut id = [0u8; 4];
                self.bus.read_register16(I2C_ADDR, REG_PRODUCT_ID, &mut id)?;
                Ok(if &id[..3] == b"911" { Some(id) } else { None })
        }

        fn interval_ms(&self) -> u32 {
                (POLL_INTERVAL_MS << self.unanswered).min(BACKOFF_MAX_MS)
        }

        fn infer_release(&mut self, now_ms: u32, affirmed: bool) -> Option<Event> {
                if !self.active || (!affirmed && self.idle_polls < RELEASE_MIN_POLLS) {
                        return None;
                }
                if now_ms.wrapping_sub(self.last_active_ms) < RELEASE_TIMEOUT_MS {
                        return None;
                }
                self.active = false;
                Some(Event::Up)
        }

        fn idle(&mut self, now_ms: u32) -> Option<Event> {
                self.idle_polls = self.idle_polls.saturating_add(1);
                self.infer_release(now_ms, false)
        }

        fn fail(&mut self, e: I2cError, now_ms: u32) -> Option<Event> {
                self.failures = self.failures.wrapping_add(1);
                match e {
                        I2cError::Nack => self.nacks += 1,
                        I2cError::Timeout => self.timeouts += 1,
                        I2cError::Bus => self.bus_errors += 1,
                }
                if self.unanswered < QUIET_AFTER_FAILS {
                        self.unanswered += 1;
                }
                self.idle(now_ms)
        }

        pub fn poll(&mut self, now_ms: u32) -> Option<Event> {
                let int_asserted = self.int.is_low();
                let quiet = self.unanswered >= QUIET_AFTER_FAILS;
                let allowed = int_asserted || !quiet;
                let since_attempt = now_ms.wrapping_sub(self.last_attempt_ms);
                let due = (int_asserted && self.unanswered == 0 && since_attempt >= INT_READ_FLOOR_MS) || since_attempt >= self.interval_ms();
                if !allowed || !due {
                        return self.idle(now_ms);
                }
                self.last_attempt_ms = now_ms;

                let mut status = [0u8];
                if let Err(e) = self.bus.read_register16(I2C_ADDR, REG_STATUS, &mut status) {
                        return self.fail(e, now_ms);
                }
                self.unanswered = 0;
                self.idle_polls = 0;
                if status[0] & STATUS_READY == 0 {
                        //   no new report; the buffer stays untouched
                        return self.infer_release(now_ms, false);
                }
                let points = status[0] & 0x0F;
                let result = if points == 0 {
                        Ok(None)
                } else {
                        let mut p = [0u8; 8];
                        self.bus.read_register16(I2C_ADDR, REG_STATUS + 1, &mut p).map(|()| Some(p))
                };
                //   release the report buffer whatever the point read did
                let ack = self.bus.write_register16(I2C_ADDR, REG_STATUS, &[0]);
                let point = match (result, ack) {
                        (Ok(p), Ok(())) => p,
                        (Err(e), _) | (_, Err(e)) => return self.fail(e, now_ms),
                };

                let was_active = self.active;
                match point {
                        None => {
                                //   the explicit release report
                                self.active = false;
                                if was_active { Some(Event::Up) } else { None }
                        }
                        Some(p) => {
                                let mut x = (u16::from(p[2]) << 8 | u16::from(p[1])).min(self.map.x_max);
                                let mut y = (u16::from(p[4]) << 8 | u16::from(p[3])).min(self.map.y_max);
                                if self.map.invert_x {
                                        x = self.map.x_max - x;
                                }
                                if self.map.invert_y {
                                        y = self.map.y_max - y;
                                }
                                if self.map.swap_xy {
                                        core::mem::swap(&mut x, &mut y);
                                }
                                self.last_active_ms = now_ms;
                                self.active = true;
                                self.x = x;
                                self.y = y;
                                if was_active {
                                        Some(Event::Move { x, y })
                                } else {
                                        Some(Event::Down { x, y })
                                }
                        }
                }
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::RefCell;
        use std::rc::Rc;
        use std::vec::Vec;

        extern crate std;

        #[derive(Default)]
        struct MockState {
                status: u8,
                point: [u8; 8],
                writes: Vec<(u16, Vec<u8>)>,
        }
        struct MockBus(Rc<RefCell<MockState>>);
        impl I2cBus for MockBus {
                fn read_register(&mut self, _: u8, _: u8, _: &mut [u8]) -> Result<(), I2cError> {
                        panic!("16-bit part");
                }
                fn write_register_byte(&mut self, _: u8, _: u8, _: u8) -> Result<(), I2cError> {
                        panic!("16-bit part");
                }
                fn read_register16(&mut self, addr: u8, reg: u16, out: &mut [u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        let s = self.0.borrow();
                        match reg {
                                REG_STATUS => out[0] = s.status,
                                r if r == REG_STATUS + 1 => out.copy_from_slice(&s.point[..out.len()]),
                                _ => panic!("unexpected register {reg:#x}"),
                        }
                        Ok(())
                }
                fn write_register16(&mut self, addr: u8, reg: u16, src: &[u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        let mut s = self.0.borrow_mut();
                        if reg == REG_STATUS && src == [0] {
                                s.status = 0;
                        }
                        s.writes.push((reg, src.to_vec()));
                        Ok(())
                }
        }
        struct MockInt(Rc<core::cell::Cell<bool>>);
        impl InputPin for MockInt {
                fn is_low(&self) -> bool {
                        self.0.get()
                }
        }

        const MAP: CoordMap = CoordMap { x_max: 480, y_max: 480, invert_x: false, invert_y: false, swap_xy: false };

        #[test]
        fn a_report_parses_and_acknowledges_and_the_release_is_explicit() {
                let state = Rc::new(RefCell::new(MockState::default()));
                let int = Rc::new(core::cell::Cell::new(true));
                let mut t = Gt911::new(MockBus(state.clone()), MockInt(int), MAP, 0);
                state.borrow_mut().status = STATUS_READY | 1;
                state.borrow_mut().point = [0, 0x34, 0x01, 0x64, 0x00, 0, 0, 0]; // x=0x134=308, y=0x64=100
                assert_eq!(t.poll(POLL_INTERVAL_MS), Some(Event::Down { x: 308, y: 100 }));
                assert_eq!(state.borrow().writes.last().unwrap(), &(REG_STATUS, std::vec![0]), "the buffer was released");
                //   the buffer is now clear: silence, not a release
                assert_eq!(t.poll(POLL_INTERVAL_MS * 2), None);
                assert!(t.active);
                //   the explicit zero-point report ends the touch
                state.borrow_mut().status = STATUS_READY;
                assert_eq!(t.poll(POLL_INTERVAL_MS * 3), Some(Event::Up));
                assert!(!t.active);
        }

        #[test]
        fn the_map_applies_inversions_then_the_swap() {
                let state = Rc::new(RefCell::new(MockState::default()));
                let int = Rc::new(core::cell::Cell::new(true));
                let map = CoordMap { x_max: 480, y_max: 480, invert_x: true, invert_y: false, swap_xy: true };
                let mut t = Gt911::new(MockBus(state.clone()), MockInt(int), map, 0);
                state.borrow_mut().status = STATUS_READY | 1;
                state.borrow_mut().point = [0, 100, 0, 30, 0, 0, 0, 0];
                //   x raw 100 -> inverted 380; then the swap: x=30, y=380
                assert_eq!(t.poll(POLL_INTERVAL_MS), Some(Event::Down { x: 30, y: 380 }));
        }
}
