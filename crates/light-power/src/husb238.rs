//! Hynetek HUSB238 USB Type-C Power Delivery SINK controller -- a "PD trigger": it
//! negotiates a contract with a USB-C source and hands the selected voltage to whatever is
//! downstream. It manages no rails of its own, which is why it is a [`PowerSource`] rather
//! than anything resembling a PMIC.
//!
//! PROVENANCE, because it changes how much the numbers below should be trusted: this map
//! comes from third-party libraries and a register-information sheet, NOT from a datasheet we
//! hold. Everything structural was confirmed against real hardware by mk3 on 2026-08-26 --
//! the address, the register file, and the per-voltage detect flags, which appeared on
//! exactly the five voltages a 45W charger offers and were clear on the 18V it does not.
//! The current-code table is corroborated arithmetically: it yields 45W at both 15V/3.0A
//! and 20V/2.25A independently, which a wrong table would not.
//!
//! The part is marked HUSB238; a board carrying one may read HUSB328 -- the same part with
//! the digits transposed.
//!
//! Every register is read and written ONE BYTE AT A TIME, and writes go through
//! [`I2cBus::write_register_byte`], the strictly framed two-byte transaction. mk3 measured
//! why that matters: the general write path sent the register address and payload as two
//! transfers separated by a repeated START, the chip acknowledged everything and stored
//! NOTHING -- SRC_PDO_SEL took five writes of five different values and read back 0x00 after
//! every one, so the negotiation appeared to do nothing while every return code said success.

use light_core::hal::I2cBus;
use light_core::info;

use crate::{PowerSource, Profile, Reading, MAX_PROFILES};

pub const I2C_ADDR: u8 = 0x08;

pub const REG_PD_STATUS0: u8 = 0x00;
pub const REG_PD_STATUS1: u8 = 0x01;
/// The six SRC_PDO registers are contiguous from 5V upward, which is what lets the driver
/// walk them by index rather than naming each one.
pub const REG_SRC_PDO_FIRST: u8 = 0x02;
pub const PDO_COUNT: usize = 6;
pub const REG_SRC_PDO_SEL: u8 = 0x08;
pub const REG_GO_COMMAND: u8 = 0x09;

/// SRC_PDO_nV layout: bit 7 says the source offers this voltage, bits 0..=3 code the current.
const PDO_DETECTED: u8 = 0x80;
const PDO_CURRENT_MASK: u8 = 0x0F;
/// PD_STATUS0: high nibble codes the negotiated voltage, low nibble the current.
const STATUS0_VOLTAGE_SHIFT: u8 = 4;
const STATUS0_CURRENT_MASK: u8 = 0x0F;
/// SRC_PDO_SEL: the PDO to request, in the high nibble, using the same voltage codes.
const PDO_SEL_SHIFT: u8 = 4;
/// GO_COMMAND: the bottom five bits are a command. REQUEST_PDO acts on whatever SRC_PDO_SEL
/// holds, so the two are always written as a pair and in that order.
const GO_REQUEST_PDO: u8 = 0x01;
/// PD_STATUS0's voltage code 0 means "nothing negotiated"; the six real voltages are 1..=6,
/// which is what lets a profile index become a selection code by adding one.
const VOLTAGE_CODE_NONE: u8 = 0;

/// The source voltages the six SRC_PDO registers describe, in register order. Fixed by the
/// part, not by the supply: a charger offering none of them still has six registers, all
/// with their detect bit clear.
const PDO_MILLIVOLTS: [u16; PDO_COUNT] = [5000, 9000, 12000, 15000, 18000, 20000];

/// The current code table, shared by the SRC_PDO registers and PD_STATUS0 -- see the module
/// doc for why it is believed correct despite its provenance.
const CURRENT_MA: [u16; 16] = [500, 700, 1000, 1250, 1500, 1750, 2000, 2250, 2500, 2750, 3000, 3250, 3500, 4000, 4500, 5000];

pub struct Husb238<B: I2cBus> {
        bus: B,
}

impl<B: I2cBus> Husb238<B> {
        /// No reset pin, no recovery machinery: this part has no reset line, and an
        /// unpowered HUSB238 is silent because nothing is plugged into its USB-C port --
        /// which no amount of resetting will fix, and which is its ordinary resting state.
        pub fn new(bus: B) -> Self {
                Self { bus }
        }

        fn read_reg(&mut self, reg: u8) -> Option<u8> {
                let mut b = [0u8; 1];
                self.bus.read_register(I2C_ADDR, reg, &mut b).ok()?;
                Some(b[0])
        }

        fn write_reg(&mut self, reg: u8, value: u8) -> bool {
                self.bus.write_register_byte(I2C_ADDR, reg, value).is_ok()
        }
}

impl<B: I2cBus> PowerSource for Husb238<B> {
        /// Genuinely a USB PD sink: its profiles are a remote source's advertised
        /// capabilities and its active point is a negotiated contract, both of which can
        /// change without this device being asked.
        fn is_pd(&self) -> bool {
                true
        }

        /// The profile list's SHAPE is fixed by the part, so it is filled in once: six
        /// entries at the six voltages, all initially unavailable. Poll then only updates
        /// availability and current -- and the indices a consumer holds stay meaningful
        /// across a change of supply.
        fn init(&mut self, profiles: &mut [Profile; MAX_PROFILES]) -> u8 {
                for (i, p) in profiles.iter_mut().take(PDO_COUNT).enumerate() {
                        *p = Profile { millivolts: PDO_MILLIVOLTS[i], milliamps: 0, available: false };
                }
                //   a presence check, log-and-continue: the part has no ID register, so the
                // closest thing to a probe is whether it answers at all -- and it very
                // reasonably may not, with nothing plugged into its port. Poll will find it
                // when it wakes
                match self.read_reg(REG_PD_STATUS0) {
                        Some(s) => info!("husb238 answering (PD_STATUS0={s:02x})"),
                        None => info!("husb238 not answering yet -- expected when no USB-C source is plugged into it"),
                }
                PDO_COUNT as u8
        }

        fn poll(&mut self, profiles: &mut [Profile; MAX_PROFILES]) -> Option<Reading> {
                //   PD_STATUS0 first, and its success decides whether the rest is worth
                // attempting: seven failed transactions against an unpowered chip is seven
                // timed-out transfers on a bus that may be shared, where one answers the
                // same question
                let status0 = self.read_reg(REG_PD_STATUS0)?;

                let v_code = status0 >> STATUS0_VOLTAGE_SHIFT;
                let i_code = status0 & STATUS0_CURRENT_MASK;
                //   what is on the rail, whether or not anyone agreed to it
                let (active_mv, active_ma) = if v_code == VOLTAGE_CODE_NONE || usize::from(v_code) > PDO_COUNT {
                        (0, 0)
                } else {
                        (PDO_MILLIVOLTS[usize::from(v_code) - 1], CURRENT_MA[usize::from(i_code)])
                };

                //   ...and whether it is a NEGOTIATED contract, which PD_STATUS0 alone
                // cannot say: it reports the voltage present, and an unattached sink sits at
                // the USB-C 5V default. SRC_PDO_SEL distinguishes them -- zero until a PDO
                // has actually been requested. Observed on the mk3 bench: charger attached
                // and supplying 5V, PD_STATUS0 reading 0x13, SRC_PDO_SEL 0x00 -- the two
                // disagreeing is precisely the case this exists to get right
                let sel = self.read_reg(REG_SRC_PDO_SEL)?;
                let contract_active = (sel >> PDO_SEL_SHIFT) != VOLTAGE_CODE_NONE;

                //   the capability list, read every time rather than once: it belongs to
                // whatever is plugged in at this moment -- swap the charger and these change
                // with no notification of any kind
                for i in 0..PDO_COUNT {
                        //   a partial read leaves the rest of the list as it was, which is
                        // better than half-clearing it: the next poll will get the truth
                        let pdo = self.read_reg(REG_SRC_PDO_FIRST + i as u8)?;
                        let p = &mut profiles[i];
                        p.available = pdo & PDO_DETECTED != 0;
                        //   zeroed rather than decoded when this voltage is not on offer: an
                        // absent PDO reads 0x00, whose current code decodes to the table's
                        // first entry -- so a straight decode reports "18000 mV 500 mA,
                        // unavailable", and that 500 is not a small number, it is a
                        // meaningless one that looks like a measurement
                        p.milliamps = if p.available { CURRENT_MA[usize::from(pdo & PDO_CURRENT_MASK)] } else { 0 };
                }
                Some(Reading { active_mv, active_ma, contract_active })
        }

        /// The one call that acts rather than observes: name the PDO, then tell it to go.
        /// NOT verified here, deliberately -- the request is handed to a negotiation that
        /// completes in its own time, and reading PD_STATUS0 back immediately would report
        /// the OLD contract and look like a failure. The next poll reports what happened.
        fn select(&mut self, index: u8) -> bool {
                let sel = (index + 1) << PDO_SEL_SHIFT;
                if !self.write_reg(REG_SRC_PDO_SEL, sel) {
                        return false;
                }
                //   order matters and is not interchangeable: GO_COMMAND acts on whatever
                // SRC_PDO_SEL holds at the moment it is written, so a REQUEST_PDO issued
                // first would re-request the PREVIOUS selection -- which succeeds, and
                // leaves the rail at the wrong voltage with every return code saying it
                // worked
                self.write_reg(REG_GO_COMMAND, GO_REQUEST_PDO)
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use crate::Power;
        use light_core::hal::I2cError;
        extern crate std;
        use std::vec::Vec;

        /// An I2C bus with an eleven-byte register file, or nobody home.
        struct FakeBus {
                present: bool,
                regs: [u8; 10],
                writes: Vec<(u8, u8)>,
        }

        impl FakeBus {
                fn with(status0: u8, sel: u8, pdos: [u8; PDO_COUNT]) -> Self {
                        let mut regs = [0u8; 10];
                        regs[usize::from(REG_PD_STATUS0)] = status0;
                        regs[usize::from(REG_SRC_PDO_SEL)] = sel;
                        for (i, p) in pdos.iter().enumerate() {
                                regs[usize::from(REG_SRC_PDO_FIRST) + i] = *p;
                        }
                        Self { present: true, regs, writes: Vec::new() }
                }
        }

        impl I2cBus for FakeBus {
                fn read_register(&mut self, addr: u8, reg: u8, out: &mut [u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        assert_eq!(out.len(), 1, "this part transfers one byte per transaction");
                        if !self.present {
                                return Err(I2cError::Nack);
                        }
                        out[0] = self.regs[usize::from(reg)];
                        Ok(())
                }
                fn write_register_byte(&mut self, addr: u8, reg: u8, value: u8) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        if !self.present {
                                return Err(I2cError::Nack);
                        }
                        self.writes.push((reg, value));
                        self.regs[usize::from(reg)] = value;
                        Ok(())
                }
        }

        /// The mk3 bench's 45W charger: 5/9/12/15/20V offered, 18V present but not offered.
        /// The current codes are the arithmetic corroboration -- 15V/3.0A and 20V/2.25A are
        /// both 45W, which a wrong table would not produce.
        fn charger_45w() -> [u8; PDO_COUNT] {
                [0x80 | 0x0A, 0x80 | 0x0A, 0x80 | 0x0A, 0x80 | 0x0A, 0x00, 0x80 | 0x07]
        }

        #[test]
        fn the_capability_list_decodes_with_absent_pdos_zeroed() {
                let mut power = Power::new(Husb238::new(FakeBus::with(0x13, 0x00, charger_45w())));
                assert!(power.poll(0));
                assert_eq!(power.profile_count(), 6);
                let p15 = power.profile(3).unwrap();
                assert_eq!((p15.millivolts, p15.milliamps, p15.available), (15000, 3000, true));
                let p20 = power.profile(5).unwrap();
                assert_eq!((p20.millivolts, p20.milliamps, p20.available), (20000, 2250, true));
                assert_eq!(u32::from(p15.millivolts) * u32::from(p15.milliamps), u32::from(p20.millivolts) * u32::from(p20.milliamps), "both 45 W");
                let p18 = power.profile(4).unwrap();
                assert_eq!((p18.millivolts, p18.milliamps, p18.available), (18000, 0, false), "absent PDO reports no current, not the table's first entry");
        }

        #[test]
        fn the_default_rail_is_reported_but_not_called_a_contract() {
                //   the mk3 bench observation verbatim: PD_STATUS0 0x13 (5V present),
                // SRC_PDO_SEL 0x00 (nothing ever requested)
                let mut power = Power::new(Husb238::new(FakeBus::with(0x13, 0x00, charger_45w())));
                power.poll(0);
                let r = power.active();
                assert_eq!(r.active_mv, 5000);
                assert!(!r.contract_active);
                //   and with SEL holding a requested code, the same rail IS a contract
                let mut power = Power::new(Husb238::new(FakeBus::with(0x13, 0x10, charger_45w())));
                power.poll(0);
                assert!(power.active().contract_active);
        }

        #[test]
        fn select_writes_sel_then_go_in_that_order() {
                let mut power = Power::new(Husb238::new(FakeBus::with(0x13, 0x00, charger_45w())));
                power.poll(0);
                power.set_max_millivolts(12000);
                assert!(power.select_profile(2, 0));
                assert_eq!(power.source().bus.writes, [(REG_SRC_PDO_SEL, 0x30), (REG_GO_COMMAND, GO_REQUEST_PDO)], "12V is code 3 in the high nibble; GO acts on whatever SEL holds, so SEL must land first");
        }

        #[test]
        fn a_silent_part_is_a_resting_state_and_a_granted_contract_resolves() {
                let mut power = Power::new(Husb238::new(FakeBus::with(0x13, 0x00, charger_45w())));
                power.set_poll_interval(0);
                power.set_max_millivolts(12000);
                assert!(power.poll(0));
                assert!(power.select_profile(2, 1));
                //   the source grants it: PD_STATUS0 reads 12V (code 3) at 3A (code 0x0A),
                // SEL still holds the request
                power.source().bus.regs[usize::from(REG_PD_STATUS0)] = 0x3A;
                power.poll(500);
                assert_eq!(power.request_state(), crate::RequestState::Active);
                assert_eq!(power.active().active_mv, 12000);
                //   then the plug is pulled: silence is ordinary, and everything known about
                // the source goes with it
                power.source().bus.present = false;
                assert!(!power.poll(1000));
                assert_eq!(power.active().active_mv, 0);
                assert!(!power.profile(2).unwrap().available);
        }
}
