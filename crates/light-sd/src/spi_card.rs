//! SD card in SPI mode: the standard bring-up dance (CMD0, CMD8, ACMD41, CMD58, CSD) and
//! single-block reads. Sequence per the SD Simplified Physical Layer spec; CRC stays off
//! (SPI mode's default) except for the two init commands whose CRCs are constants.
//!
//! Init MUST run below 400 kHz -- cards ignore faster clocks until initialized -- and the
//! driver switches the bus to `DATA_HZ` once the card is up.

use light_core::hal::{Clock, OutputPin, SpiBus};

/// Below the 400 kHz init ceiling.
pub const INIT_HZ: u32 = 300_000;
/// Conservative data rate; every card class does 12.5 MHz.
pub const DATA_HZ: u32 = 12_000_000;

const CMD0_GO_IDLE: u8 = 0;
const CMD8_SEND_IF_COND: u8 = 8;
const CMD9_SEND_CSD: u8 = 9;
const CMD16_SET_BLOCKLEN: u8 = 16;
const CMD17_READ_SINGLE: u8 = 17;
const CMD55_APP_CMD: u8 = 55;
const CMD58_READ_OCR: u8 = 58;
const ACMD41_SD_SEND_OP_COND: u8 = 41;

const R1_IDLE: u8 = 0x01;
const R1_ILLEGAL_COMMAND: u8 = 0x04;
const DATA_TOKEN: u8 = 0xFE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdError {
        /// Nothing answered CMD0: no card in the slot (or no pull-up on MISO).
        NoCard,
        /// The card answered but the init sequence could not complete.
        Unusable,
        /// A response or data token did not arrive in time.
        Timeout,
        /// An R1 with error bits set; the raw value.
        Response(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardInfo {
        /// SDHC/SDXC: block-addressed. Byte-addressed (v1/standard v2) otherwise.
        pub high_capacity: bool,
        /// Capacity in 512-byte blocks, from the CSD.
        pub blocks: u32,
}

pub struct SpiSd<B: SpiBus, O: OutputPin> {
        bus: B,
        cs: O,
        pub card: Option<CardInfo>,
}

impl<B: SpiBus, O: OutputPin> SpiSd<B, O> {
        /// `cs` must have been constructed deasserted (high).
        pub fn new(bus: B, cs: O) -> Self {
                Self { bus, cs, card: None }
        }

        fn command(&mut self, cmd: u8, arg: u32, crc: u8) -> Result<u8, SdError> {
                // open bus time, then the 6-byte frame
                self.bus.transfer(0xFF);
                self.bus.transfer(0x40 | cmd);
                for shift in [24, 16, 8, 0] {
                        self.bus.transfer((arg >> shift) as u8);
                }
                self.bus.transfer(crc);
                //   the R1 arrives within 8 byte times, bit 7 clear
                for _ in 0..9 {
                        let r = self.bus.transfer(0xFF);
                        if r & 0x80 == 0 {
                                return Ok(r);
                        }
                }
                Err(SdError::Timeout)
        }

        /// A whole command transaction: CS framed, with a trailing clock byte after
        /// release, which some cards need to let go of MISO.
        fn transact(&mut self, cmd: u8, arg: u32, crc: u8) -> Result<u8, SdError> {
                self.cs.set(false);
                let r = self.command(cmd, arg, crc);
                self.cs.set(true);
                self.bus.transfer(0xFF);
                r
        }

        /// Read a data block of `out.len()` bytes under an already-open command: wait for
        /// the 0xFE token, take the payload, discard the CRC.
        fn read_data(&mut self, out: &mut [u8]) -> Result<(), SdError> {
                let mut token = 0xFFu8;
                for _ in 0..200_000 {
                        token = self.bus.transfer(0xFF);
                        if token != 0xFF {
                                break;
                        }
                }
                if token != DATA_TOKEN {
                        return Err(SdError::Timeout);
                }
                for b in out.iter_mut() {
                        *b = self.bus.transfer(0xFF);
                }
                self.bus.transfer(0xFF);
                self.bus.transfer(0xFF);
                Ok(())
        }

        /// The init dance. On success the bus runs at [`DATA_HZ`] and `card` is filled in.
        pub fn init(&mut self, clock: &mut dyn Clock) -> Result<CardInfo, SdError> {
                self.card = None;
                self.bus.set_hz(INIT_HZ);
                // 80+ clocks with CS high wake the card into SPI mode readiness
                self.cs.set(true);
                for _ in 0..10 {
                        self.bus.transfer(0xFF);
                }
                //   CMD0 with its constant CRC; a few tries, because the first can land
                // mid-wakeup. No response at all is the empty slot, not a bus fault
                let mut idle = false;
                for _ in 0..8 {
                        match self.transact(CMD0_GO_IDLE, 0, 0x95) {
                                Ok(R1_IDLE) => {
                                        idle = true;
                                        break;
                                }
                                Ok(_) | Err(SdError::Timeout) => {}
                                Err(e) => return Err(e),
                        }
                }
                if !idle {
                        return Err(SdError::NoCard);
                }

                //   CMD8: v2 cards echo the check pattern; v1 cards answer illegal-command
                self.cs.set(false);
                let r = self.command(CMD8_SEND_IF_COND, 0x1AA, 0x87)?;
                let v2 = if r == R1_IDLE {
                        let mut echo = [0u8; 4];
                        for b in echo.iter_mut() {
                                *b = self.bus.transfer(0xFF);
                        }
                        self.cs.set(true);
                        self.bus.transfer(0xFF);
                        if echo[2] & 0x0F != 0x01 || echo[3] != 0xAA {
                                return Err(SdError::Unusable);
                        }
                        true
                } else {
                        self.cs.set(true);
                        self.bus.transfer(0xFF);
                        if r & R1_ILLEGAL_COMMAND == 0 {
                                return Err(SdError::Response(r));
                        }
                        false
                };

                //   ACMD41 until the card leaves idle; HCS offered to v2 cards. The spec
                // allows a full second
                let deadline = clock.now_us() + 1_000_000;
                loop {
                        let r = self.transact(CMD55_APP_CMD, 0, 0x01)?;
                        if r & !R1_IDLE != 0 {
                                return Err(SdError::Response(r));
                        }
                        let arg = if v2 { 1 << 30 } else { 0 };
                        let r = self.transact(ACMD41_SD_SEND_OP_COND, arg, 0x01)?;
                        if r == 0x00 {
                                break;
                        }
                        if r != R1_IDLE {
                                return Err(SdError::Response(r));
                        }
                        if clock.now_us() > deadline {
                                return Err(SdError::Timeout);
                        }
                }

                //   CMD58: the OCR's CCS bit says block-addressed
                let mut high_capacity = false;
                if v2 {
                        self.cs.set(false);
                        let r = self.command(CMD58_READ_OCR, 0, 0x01)?;
                        if r != 0 {
                                self.cs.set(true);
                                return Err(SdError::Response(r));
                        }
                        let mut ocr = [0u8; 4];
                        for b in ocr.iter_mut() {
                                *b = self.bus.transfer(0xFF);
                        }
                        self.cs.set(true);
                        self.bus.transfer(0xFF);
                        high_capacity = ocr[0] & 0x40 != 0;
                }
                if !high_capacity {
                        let r = self.transact(CMD16_SET_BLOCKLEN, 512, 0x01)?;
                        if r != 0 {
                                return Err(SdError::Response(r));
                        }
                }

                //   capacity from the CSD
                self.cs.set(false);
                let r = self.command(CMD9_SEND_CSD, 0, 0x01)?;
                if r != 0 {
                        self.cs.set(true);
                        return Err(SdError::Response(r));
                }
                let mut csd = [0u8; 16];
                let read = self.read_data(&mut csd);
                self.cs.set(true);
                self.bus.transfer(0xFF);
                read?;
                let blocks = match csd[0] >> 6 {
                        //   CSD v2: C_SIZE in units of 512 KB
                        1 => {
                                let c_size = (u32::from(csd[7] & 0x3F) << 16) | (u32::from(csd[8]) << 8) | u32::from(csd[9]);
                                (c_size + 1) * 1024
                        }
                        //   CSD v1: the three-field multiplier dance
                        0 => {
                                let c_size = (u32::from(csd[6] & 0x03) << 10) | (u32::from(csd[7]) << 2) | (u32::from(csd[8]) >> 6);
                                let c_size_mult = ((csd[9] & 0x03) << 1) | (csd[10] >> 7);
                                let read_bl_len = csd[5] & 0x0F;
                                ((c_size + 1) << (c_size_mult + 2)) << read_bl_len >> 9
                        }
                        _ => return Err(SdError::Unusable),
                };

                self.bus.set_hz(DATA_HZ);
                let info = CardInfo { high_capacity, blocks };
                self.card = Some(info);
                Ok(info)
        }

        /// Read one 512-byte block. `lba` is always a block index; the driver applies the
        /// byte addressing an older card wants.
        pub fn read_block(&mut self, lba: u32, out: &mut [u8; 512]) -> Result<(), SdError> {
                let Some(card) = self.card else { return Err(SdError::Unusable) };
                let addr = if card.high_capacity { lba } else { lba * 512 };
                self.cs.set(false);
                let r = self.command(CMD17_READ_SINGLE, addr, 0x01)?;
                let result = if r != 0 { Err(SdError::Response(r)) } else { self.read_data(out) };
                self.cs.set(true);
                self.bus.transfer(0xFF);
                result
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::RefCell;
        use std::rc::Rc;
        use std::vec::Vec;

        extern crate std;

        /// A scripted card: replies from a queue keyed by nothing but order, recording
        /// every transmitted byte. 0xFF is the resting reply when the script is silent.
        #[derive(Default)]
        struct CardState {
                tx: Vec<u8>,
                replies: std::collections::VecDeque<u8>,
        }
        struct MockBus(Rc<RefCell<CardState>>);
        impl SpiBus for MockBus {
                fn transfer(&mut self, tx: u8) -> u8 {
                        let mut s = self.0.borrow_mut();
                        s.tx.push(tx);
                        s.replies.pop_front().unwrap_or(0xFF)
                }
                fn set_hz(&mut self, _hz: u32) {}
        }
        struct MockCs;
        impl OutputPin for MockCs {
                fn set(&mut self, _high: bool) {}
        }
        struct NoClock(u64);
        impl Clock for NoClock {
                fn now_us(&self) -> u64 {
                        self.0
                }
                fn delay_ms(&mut self, _: u32) {}
        }

        fn script(replies: &[u8]) -> Rc<RefCell<CardState>> {
                let s = Rc::new(RefCell::new(CardState::default()));
                s.borrow_mut().replies = replies.iter().copied().collect();
                s
        }

        #[test]
        fn a_v2_block_card_initializes_and_decodes_its_csd() {
                //   the replies, in wire order: 10 wakeup bytes, then per-command open byte
                // + 6 frame bytes echo 0xFF before each response
                let mut replies = std::vec![0xFF; 10]; // wakeup
                replies.extend([0xFF; 8]); // CMD0 frame + open
                replies.push(0x01); // R1 idle
                replies.push(0xFF); // trailing clock
                replies.extend([0xFF; 8]); // CMD8
                replies.push(0x01); // R1 idle
                replies.extend([0x00, 0x00, 0x01, 0xAA]); // echo
                replies.push(0xFF); // trailing
                replies.extend([0xFF; 8]); // CMD55
                replies.push(0x01);
                replies.push(0xFF);
                replies.extend([0xFF; 8]); // ACMD41
                replies.push(0x00); // ready
                replies.push(0xFF);
                replies.extend([0xFF; 8]); // CMD58
                replies.push(0x00);
                replies.extend([0xC0, 0xFF, 0x80, 0x00]); // OCR: CCS set
                replies.push(0xFF);
                replies.extend([0xFF; 8]); // CMD9
                replies.push(0x00);
                replies.push(0xFE); // data token, immediately
                //   a CSD v2 with C_SIZE = 0x003B37 (15159 -> ~7.7 GB card)
                let csd = [0x40u8, 0x0E, 0x00, 0x32, 0x5B, 0x59, 0x00, 0x00, 0x3B, 0x37, 0x7F, 0x80, 0x0A, 0x40, 0x40, 0xC1];
                replies.extend(csd);
                replies.extend([0x00, 0x00]); // CRC
                replies.push(0xFF);
                let state = script(&replies);
                let mut sd = SpiSd::new(MockBus(state.clone()), MockCs);
                let info = sd.init(&mut NoClock(0)).unwrap();
                assert!(info.high_capacity);
                assert_eq!(info.blocks, (0x3B37 + 1) * 1024);
                //   the CMD0 frame went out with its constant CRC
                let tx = state.borrow().tx.clone();
                let cmd0 = tx.windows(6).position(|w| w == [0x40, 0, 0, 0, 0, 0x95]);
                assert!(cmd0.is_some(), "CMD0 with CRC 0x95 on the wire");
                let cmd8 = tx.windows(6).position(|w| w == [0x48, 0, 0, 0x01, 0xAA, 0x87]);
                assert!(cmd8.is_some(), "CMD8 with the check pattern and CRC 0x87");
        }

        #[test]
        fn a_block_read_addresses_by_lba_on_high_capacity_cards() {
                let mut replies = std::vec![0xFF; 8]; // CMD17 frame
                replies.push(0x00); // R1
                replies.push(0xFE); // token
                let mut data = [0u8; 512];
                for (i, b) in data.iter_mut().enumerate() {
                        *b = i as u8;
                }
                replies.extend(data);
                replies.extend([0x00, 0x00, 0xFF]);
                let state = script(&replies);
                let mut sd = SpiSd::new(MockBus(state.clone()), MockCs);
                sd.card = Some(CardInfo { high_capacity: true, blocks: 1000 });
                let mut out = [0u8; 512];
                sd.read_block(1234, &mut out).unwrap();
                assert_eq!(out[..4], [0, 1, 2, 3]);
                assert_eq!(out[511], 255);
                let tx = state.borrow().tx.clone();
                let want = [0x51, 0, 0, (1234u32 >> 8) as u8, (1234u32 & 0xFF) as u8, 0x01];
                assert!(tx.windows(6).any(|w| w == want), "CMD17 carried the LBA, not a byte address");
        }

        #[test]
        fn an_empty_slot_reports_no_card() {
                let state = script(&[]); // nothing ever answers: all 0xFF
                let mut sd = SpiSd::new(MockBus(state), MockCs);
                assert_eq!(sd.init(&mut NoClock(0)), Err(SdError::NoCard));
        }
}
