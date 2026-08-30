//! SPI4 as a 4-wire display bus, blocking: mk3's STM32 ioport transport for the H7 generation.
//!
//! The H7's SPI is not the F4's with more bits. A transfer size is programmed per transaction
//! (TSIZE, with the peripheral disabled to change it -- TSIZE 0 is "until stopped", which on a
//! display shows as trailing garbage after every write), the transaction is started with
//! CSTART, and the FIFO is primed BEFORE the start: CSTART begins clocking immediately, and
//! starting with nothing to shift is an underrun. End of transfer is EOT, not "TX empty" --
//! the last byte has been handed to the shifter, not sent, and dropping CS before it clocks out
//! truncates it.
//!
//! No DMA yet, so `start_data` is the blocking write and `is_complete` is always true. That is
//! honest rather than lazy: the display core polls completion and simply never sees an
//! incomplete burst, which costs frame rate, not correctness. The H7's DMA is a later slice.
//!
//! Every wait is bounded. An unbounded spin on a peripheral that never raises its flag is a
//! board indistinguishable from a dead one, and mk3 was bitten by exactly that three times.

use light_core::hal::{Clock, SpiDisplayBus};

use crate::gpio::{Output, Pin};
use crate::reg;

const SPI4_BASE: usize = 0x4001_3400;
const CR1: usize = SPI4_BASE + 0x00;
const CR2: usize = SPI4_BASE + 0x04;
const CFG1: usize = SPI4_BASE + 0x08;
const CFG2: usize = SPI4_BASE + 0x0C;
const SR: usize = SPI4_BASE + 0x14;
const IFCR: usize = SPI4_BASE + 0x18;
const TXDR: usize = SPI4_BASE + 0x20;

const CR1_SPE: u32 = 1 << 0;
const CR1_CSTART: u32 = 1 << 9;
const CR1_SSI: u32 = 1 << 12;
const CFG1_DSIZE_8: u32 = 7;
const CFG1_MBR_POS: u32 = 28;
const CFG2_MASTER: u32 = 1 << 22;
const CFG2_SSM: u32 = 1 << 26;
const CFG2_SSOE: u32 = 1 << 29;
const CFG2_AFCNTR: u32 = 1 << 31;
const SR_TXP: u32 = 1 << 1;
const SR_EOT: u32 = 1 << 3;
const IFCR_EOTC: u32 = 1 << 3;
const IFCR_TXTFC: u32 = 1 << 4;
const RCC_APB2ENR_SPI4EN: u32 = 1 << 13;
const SPI4_AF: u32 = 5;
const WAIT_SPINS: u32 = 1_000_000;

pub struct Spi4Display {
        cs: Output,
        dc: Output,
        reset: Option<Output>,
        /// The clock the divider produced, for the log.
        pub actual_hz: u32,
        /// Transfers abandoned on a timed-out wait, for diagnostics.
        pub timeouts: u32,
}

impl Spi4Display {
        /// Bring SPI4 up as a master on `sck`/`mosi`, with software-driven `cs` and `dc`. The
        /// kernel clock is APB2 by reset default; the divider is the largest that stays at or
        /// under `target_hz`.
        pub fn new(apb2_hz: u32, sck: Pin, mosi: Pin, cs: Pin, dc: Pin, reset: Option<Pin>, target_hz: u32) -> Self {
                reg::modify(crate::RCC_APB2ENR, 0, RCC_APB2ENR_SPI4EN);
                let _ = reg::read(crate::RCC_APB2ENR);
                sck.set_alternate(SPI4_AF);
                mosi.set_alternate(SPI4_AF);
                let cs = Output::new(cs, true);
                let dc = Output::new(dc, true);
                let reset = reset.map(|p| Output::new(p, true));
                // MBR n divides by 2^(n+1), n in 0..=7
                let mut mbr = 0u32;
                while mbr < 7 && apb2_hz >> (mbr + 1) > target_hz {
                        mbr += 1;
                }
                let actual_hz = apb2_hz >> (mbr + 1);
                reg::write(CR1, 0);
                reg::write(CFG1, CFG1_DSIZE_8 | (mbr << CFG1_MBR_POS));
                reg::write(CFG2, CFG2_MASTER | CFG2_SSM | CFG2_SSOE | CFG2_AFCNTR);
                reg::modify(CR1, 0, CR1_SSI);
                Self { cs, dc, reset, actual_hz, timeouts: 0 }
        }

        fn wait(&mut self, mask: u32) -> bool {
                let mut spins = WAIT_SPINS;
                while spins > 0 {
                        if reg::read(SR) & mask != 0 {
                                return true;
                        }
                        spins -= 1;
                }
                self.timeouts += 1;
                false
        }

        fn write_blocking(&mut self, bytes: &[u8]) {
                //   the H7 takes TSIZE per transfer, in 16 bits: longer bursts go in pieces
                for chunk in bytes.chunks(0xFFFF) {
                        reg::modify(CR1, CR1_SPE, 0);
                        reg::write(CR2, chunk.len() as u32);
                        reg::modify(CR1, 0, CR1_SPE);
                        let mut i = 0;
                        // prime the FIFO, then start
                        while i < chunk.len() && reg::read(SR) & SR_TXP != 0 {
                                reg::write_u8(TXDR, chunk[i]);
                                i += 1;
                        }
                        reg::modify(CR1, 0, CR1_CSTART);
                        while i < chunk.len() {
                                if !self.wait(SR_TXP) {
                                        break;
                                }
                                reg::write_u8(TXDR, chunk[i]);
                                i += 1;
                        }
                        self.wait(SR_EOT);
                        //   flags cleared and SPE dropped even after a timeout, so one stuck
                        // transfer does not poison every one after it
                        reg::write(IFCR, IFCR_EOTC | IFCR_TXTFC);
                        reg::modify(CR1, CR1_SPE, 0);
                }
        }
}

impl SpiDisplayBus for Spi4Display {
        fn command(&mut self, cmd: u8) {
                self.dc.set(false);
                self.cs.set(false);
                self.write_blocking(&[cmd]);
                self.cs.set(true);
        }

        fn data(&mut self, bytes: &[u8]) {
                self.dc.set(true);
                self.cs.set(false);
                self.write_blocking(bytes);
                self.cs.set(true);
        }

        unsafe fn start_data(&mut self, bytes: &[u8]) {
                self.data(bytes);
        }

        fn is_complete(&mut self) -> bool {
                true
        }

        fn reset_pulse(&mut self, clock: &mut dyn Clock) {
                // a panel whose reset is tied to the board's own reset line has none to pulse
                if let Some(r) = self.reset.as_mut() {
                        r.set(true);
                        clock.delay_ms(5);
                        r.set(false);
                        clock.delay_ms(20);
                        r.set(true);
                        clock.delay_ms(150);
                }
        }
}
