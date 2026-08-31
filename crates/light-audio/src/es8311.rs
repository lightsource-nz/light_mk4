//! Everest ES8311: a mono low-power audio codec on I2C, configured as the I2S MASTER --
//! this side feeds it MCLK and the codec generates BCLK/LRCLK from its own dividers, which
//! keeps the clock arithmetic in the part that documents it. Register sequence from
//! Espressif's reference driver via Waveshare's demo, kept byte for byte where it is
//! "NOT default" magic.
//!
//! Only the 256-Fs MCLK family is carried in the divider table -- that is how the boards
//! at hand generate MCLK -- with the reference's coefficients verbatim (including the
//! 24 kHz row's bclk_div of 8 where every sibling row says 4).

use light_core::hal::{Clock, I2cBus, I2cError};

pub const I2C_ADDR: u8 = 0x18;
/// 0xFD/0xFE read back the part number, literally 0x83, 0x11.
pub const CHIP_ID: u16 = 0x8311;

const REG_RESET: u8 = 0x00;
const REG_CLK01: u8 = 0x01;
const REG_CLK02: u8 = 0x02;
const REG_CLK03: u8 = 0x03;
const REG_CLK04: u8 = 0x04;
const REG_CLK05: u8 = 0x05;
const REG_CLK06: u8 = 0x06;
const REG_CLK07: u8 = 0x07;
const REG_CLK08: u8 = 0x08;
const REG_SDP_IN: u8 = 0x09;
const REG_SDP_OUT: u8 = 0x0A;
const REG_SYS0D: u8 = 0x0D;
const REG_SYS0E: u8 = 0x0E;
const REG_SYS12: u8 = 0x12;
const REG_SYS13: u8 = 0x13;
const REG_ADC1C: u8 = 0x1C;
const REG_DAC_MUTE: u8 = 0x31;
const REG_DAC_VOLUME: u8 = 0x32;
const REG_DAC37: u8 = 0x37;
const REG_ID_LO: u8 = 0xFD;
const REG_ID_HI: u8 = 0xFE;

/// 16-bit resolution in the SDP format registers: code 3 in bits 3:2.
const RES_16BIT: u8 = 3 << 2;

/// The 256-Fs rows of the reference's coefficient table: rate, pre_div, pre_multi,
/// bclk_div. Every row shares adc_div/dac_div = 1, single speed, lrck 0x00FF, OSR 0x10.
const COEFF_256FS: &[(u32, u8, u8, u8)] = &[
        (8_000, 1, 0, 4),
        (16_000, 1, 0, 4),
        (22_050, 1, 0, 4),
        (24_000, 1, 0, 8),
        (32_000, 1, 0, 4),
        (44_100, 1, 0, 4),
        (48_000, 1, 0, 4),
];

pub struct Es8311<B: I2cBus> {
        bus: B,
}

impl<B: I2cBus> Es8311<B> {
        pub fn new(bus: B) -> Self {
                Self { bus }
        }

        fn write(&mut self, reg: u8, value: u8) -> Result<(), I2cError> {
                self.bus.write_register_byte(I2C_ADDR, reg, value)
        }

        fn read(&mut self, reg: u8) -> Result<u8, I2cError> {
                let mut b = [0u8];
                self.bus.read_register(I2C_ADDR, reg, &mut b)?;
                Ok(b[0])
        }

        /// The part number, `Some(0x8311)` when the right chip answers.
        pub fn probe(&mut self) -> Result<Option<u16>, I2cError> {
                let lo = self.read(REG_ID_LO)?;
                let hi = self.read(REG_ID_HI)?;
                let id = u16::from(hi) << 8 | u16::from(lo);
                Ok(if id == CHIP_ID { Some(id) } else { None })
        }

        /// Reset, clock the codec as master from a 256-Fs MCLK, 16-bit I2S, and power the
        /// analog path up to the headphone/speaker drive. `sample_hz` must be one of the
        /// table's rates.
        pub fn init(&mut self, sample_hz: u32, clock: &mut dyn Clock) -> Result<(), I2cError> {
                let Some(&(_, pre_div, pre_multi, bclk_div)) = COEFF_256FS.iter().find(|c| c.0 == sample_hz) else {
                        //   an unsupported rate is a wiring-level configuration error, worth
                        // a loud failure over a silently wrong pitch
                        panic!("es8311: no 256-Fs coefficients for {sample_hz} Hz");
                };
                // reset to defaults, then the power-on command
                self.write(REG_RESET, 0x1F)?;
                clock.delay_ms(20);
                self.write(REG_RESET, 0x00)?;
                self.write(REG_RESET, 0x80)?;

                // all clocks on, MCLK from the MCLK pin, nothing inverted
                self.write(REG_CLK01, 0x3F)?;
                let reg06 = self.read(REG_CLK06)? & !(1 << 5) | 0x03;
                self.write(REG_CLK06, reg06)?;

                // the divider chain for this rate
                let reg02 = self.read(REG_CLK02)? & 0x07 | (pre_div - 1) << 5 | pre_multi << 3;
                self.write(REG_CLK02, reg02)?;
                self.write(REG_CLK03, 0x10)?;
                self.write(REG_CLK04, 0x10)?;
                self.write(REG_CLK05, 0x00)?;
                let reg06 = self.read(REG_CLK06)? & 0xE0 | (bclk_div - 1);
                self.write(REG_CLK06, reg06)?;
                let reg07 = self.read(REG_CLK07)? & 0xC0;
                self.write(REG_CLK07, reg07)?;
                self.write(REG_CLK08, 0xFF)?;

                // master serial port, 16-bit I2S both directions
                let reg00 = self.read(REG_RESET)? | 0x40;
                self.write(REG_RESET, reg00)?;
                self.write(REG_SDP_IN, RES_16BIT)?;
                self.write(REG_SDP_OUT, RES_16BIT)?;

                // the reference's "NOT default" analog power-up, verbatim
                self.write(REG_SYS0D, 0x01)?;
                self.write(REG_SYS0E, 0x02)?;
                self.write(REG_SYS12, 0x00)?;
                self.write(REG_SYS13, 0x10)?;
                self.write(REG_ADC1C, 0x6A)?;
                self.write(REG_DAC37, 0x08)
        }

        /// 0..=100, the reference's mapping onto the DAC volume register.
        pub fn set_volume(&mut self, volume: u8) -> Result<(), I2cError> {
                let v = u32::from(volume.min(100));
                let reg = if v == 0 { 0 } else { (v * 256 / 100 - 1) as u8 };
                self.write(REG_DAC_VOLUME, reg)
        }

        pub fn mute(&mut self, mute: bool) -> Result<(), I2cError> {
                let reg = self.read(REG_DAC_MUTE)?;
                let reg = if mute { reg | 0x60 } else { reg & !0x60 };
                self.write(REG_DAC_MUTE, reg)
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::RefCell;
        use std::rc::Rc;
        use std::vec::Vec;

        extern crate std;

        struct MockState {
                regs: [u8; 0x100],
                writes: Vec<(u8, u8)>,
        }
        impl Default for MockState {
                fn default() -> Self {
                        Self { regs: [0; 0x100], writes: Vec::new() }
                }
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
                        s.writes.push((reg, value));
                        Ok(())
                }
        }
        struct NoClock;
        impl Clock for NoClock {
                fn now_us(&self) -> u64 {
                        0
                }
                fn delay_ms(&mut self, _: u32) {}
        }

        #[test]
        fn init_at_24k_lands_the_reference_register_image() {
                let state = Rc::new(RefCell::new(MockState::default()));
                let mut c = Es8311::new(MockBus(state.clone()));
                c.init(24_000, &mut NoClock).unwrap();
                let s = state.borrow();
                assert_eq!(s.writes[..3], [(REG_RESET, 0x1F), (REG_RESET, 0x00), (REG_RESET, 0x80)]);
                assert_eq!(s.regs[REG_CLK01 as usize], 0x3F);
                //   pre_div 1, pre_multi 0 on defaults: reg02 keeps only its low bits
                assert_eq!(s.regs[REG_CLK02 as usize], 0x00);
                //   the 24 kHz row's oddity: bclk_div 8 -> low bits 7
                assert_eq!(s.regs[REG_CLK06 as usize] & 0x1F, 7);
                assert_eq!(s.regs[REG_RESET as usize] & 0x40, 0x40, "master mode");
                assert_eq!(s.regs[REG_SDP_OUT as usize], RES_16BIT);
                assert_eq!(s.regs[REG_SYS13 as usize], 0x10, "output drive enabled");
        }

        #[test]
        fn volume_uses_the_reference_mapping_and_the_id_gates_the_probe() {
                let state = Rc::new(RefCell::new(MockState::default()));
                state.borrow_mut().regs[REG_ID_LO as usize] = 0x11;
                state.borrow_mut().regs[REG_ID_HI as usize] = 0x83;
                let mut c = Es8311::new(MockBus(state.clone()));
                assert_eq!(c.probe().unwrap(), Some(CHIP_ID));
                c.set_volume(73).unwrap();
                assert_eq!(state.borrow().regs[REG_DAC_VOLUME as usize], 185);
                c.set_volume(0).unwrap();
                assert_eq!(state.borrow().regs[REG_DAC_VOLUME as usize], 0);
                state.borrow_mut().regs[REG_ID_HI as usize] = 0x00;
                assert_eq!(c.probe().unwrap(), None, "a wrong id is not this part");
        }
}
