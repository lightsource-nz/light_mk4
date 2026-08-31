//! NXP PCF85063A: a battery-backed I2C RTC. Register map from the part's datasheet via
//! Waveshare's reference driver -- BCD time in registers 0x04..=0x0A, control at 0x00.
//!
//! The reference's conventions are kept where the silicon leaves a choice: the year
//! register's 0..=99 counts from 1970 (so a clock set by the factory demo reads back
//! correctly), and init selects the 12.5 pF crystal load the board's crystal wants.

use light_core::hal::{I2cBus, I2cError};

pub const I2C_ADDR: u8 = 0x51;

const REG_CTRL1: u8 = 0x00;
const REG_SECONDS: u8 = 0x04;
/// CTRL1: 12.5 pF crystal load, 24-hour mode, clock running.
const CTRL1_CAP_12P5: u8 = 0x01;
/// Seconds register bit 7: the oscillator-stop flag -- the time has not been trusted
/// since power was last lost. Cleared by writing the seconds register (any set()).
const SECONDS_OS: u8 = 0x80;
/// The year register counts 0..=99 from here.
pub const YEAR_BASE: u16 = 1970;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Datetime {
        pub year: u16,
        /// 1..=12.
        pub month: u8,
        /// 1..=31.
        pub day: u8,
        /// 0..=6, 0 is Sunday.
        pub weekday: u8,
        pub hour: u8,
        pub minute: u8,
        pub second: u8,
}

fn to_bcd(v: u8) -> u8 {
        (v / 10) << 4 | (v % 10)
}

fn from_bcd(v: u8) -> u8 {
        (v >> 4) * 10 + (v & 0x0F)
}

pub struct Pcf85063a<B: I2cBus> {
        bus: B,
}

impl<B: I2cBus> Pcf85063a<B> {
        pub fn new(bus: B) -> Self {
                Self { bus }
        }

        /// Normal mode, 24-hour format, 12.5 pF load; doubles as the probe -- an absent part
        /// NACKs the write.
        pub fn init(&mut self) -> Result<(), I2cError> {
                self.bus.write_register_byte(I2C_ADDR, REG_CTRL1, CTRL1_CAP_12P5)
        }

        /// The current time, and whether the part has KEPT time since it was last set --
        /// false means the oscillator stopped (battery ran out, first power-up) and the
        /// fields are not to be trusted until a set().
        pub fn now(&mut self) -> Result<(Datetime, bool), I2cError> {
                let mut regs = [0u8; 7];
                self.bus.read_register(I2C_ADDR, REG_SECONDS, &mut regs)?;
                let kept = regs[0] & SECONDS_OS == 0;
                Ok((
                        Datetime {
                                second: from_bcd(regs[0] & 0x7F),
                                minute: from_bcd(regs[1] & 0x7F),
                                hour: from_bcd(regs[2] & 0x3F),
                                day: from_bcd(regs[3] & 0x3F),
                                weekday: from_bcd(regs[4] & 0x07),
                                month: from_bcd(regs[5] & 0x1F),
                                year: u16::from(from_bcd(regs[6])) + YEAR_BASE,
                        },
                        kept,
                ))
        }

        /// Set every field in one bus write (the part latches time registers during a
        /// multi-byte access, so the written instant is coherent). Also clears the
        /// oscillator-stop flag.
        pub fn set(&mut self, t: &Datetime) -> Result<(), I2cError> {
                let year = t.year.saturating_sub(YEAR_BASE).min(99) as u8;
                let frame = [
                        REG_SECONDS,
                        to_bcd(t.second),
                        to_bcd(t.minute),
                        to_bcd(t.hour),
                        to_bcd(t.day),
                        to_bcd(t.weekday),
                        to_bcd(t.month),
                        to_bcd(year),
                ];
                self.bus.write_raw(I2C_ADDR, &frame)
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
                regs: [u8; 0x12],
                writes: Vec<Vec<u8>>,
        }
        struct MockBus(Rc<RefCell<MockState>>);
        impl I2cBus for MockBus {
                fn read_register(&mut self, addr: u8, reg: u8, dst: &mut [u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        let s = self.0.borrow();
                        dst.copy_from_slice(&s.regs[reg as usize..reg as usize + dst.len()]);
                        Ok(())
                }
                fn write_register_byte(&mut self, addr: u8, reg: u8, value: u8) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        let mut s = self.0.borrow_mut();
                        s.regs[reg as usize] = value;
                        s.writes.push(std::vec![reg, value]);
                        Ok(())
                }
                fn write_raw(&mut self, addr: u8, src: &[u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        let mut s = self.0.borrow_mut();
                        let reg = src[0] as usize;
                        for (i, b) in src[1..].iter().enumerate() {
                                s.regs[reg + i] = *b;
                        }
                        s.writes.push(src.to_vec());
                        Ok(())
                }
                fn read_raw(&mut self, _: u8, _: &mut [u8]) -> Result<(), I2cError> {
                        panic!("not used by this part");
                }
        }

        #[test]
        fn a_set_time_reads_back_and_the_wire_carries_bcd() {
                let state = Rc::new(RefCell::new(MockState::default()));
                let mut rtc = Pcf85063a::new(MockBus(state.clone()));
                rtc.init().unwrap();
                assert_eq!(state.borrow().writes[0], std::vec![REG_CTRL1, CTRL1_CAP_12P5]);
                let t = Datetime { year: 2026, month: 8, day: 31, weekday: 1, hour: 23, minute: 59, second: 41 };
                rtc.set(&t).unwrap();
                //   one raw frame: register, then seven BCD bytes; 2026 stores as 56 from 1970
                assert_eq!(state.borrow().writes[1], std::vec![REG_SECONDS, 0x41, 0x59, 0x23, 0x31, 0x01, 0x08, 0x56]);
                let (back, kept) = rtc.now().unwrap();
                assert_eq!(back, t);
                assert!(kept, "set cleared the oscillator-stop flag");
        }

        #[test]
        fn the_oscillator_stop_flag_marks_the_time_untrusted() {
                let state = Rc::new(RefCell::new(MockState::default()));
                state.borrow_mut().regs[REG_SECONDS as usize] = SECONDS_OS | 0x15;
                let mut rtc = Pcf85063a::new(MockBus(state));
                let (t, kept) = rtc.now().unwrap();
                assert!(!kept, "power was lost; the fields are not to be trusted");
                assert_eq!(t.second, 15, "the flag bit is masked out of the value");
        }
}
