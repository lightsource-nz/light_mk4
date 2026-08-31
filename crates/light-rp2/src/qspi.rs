//! A QSPI display bus on a PIO state machine, for the AXS15231B-family panels (the
//! RP2350-Touch-LCD-3.49 is the first board). Ported from Waveshare's `qspi_pio` reference
//! with its one structural insight kept: EVERYTHING runs on the single 4-bit-wide state
//! machine. Serial command frames are not a second PIO program -- each command byte is
//! EXPANDED in software so that its eight bits land on D0 alone across eight QSPI clocks,
//! while pixel data streams raw at four bits per clock, DMA-fed. One program, two framings.
//!
//! The program is two instructions, hand-assembled here because the C shell owns pioasm and
//! this crate only owns registers:
//!
//! ```text
//! .side_set 1 opt          ; SCLK
//!     out pins, 4  side 0  ; 0x7004
//!     nop          side 1  ; 0xB842
//! ```
//!
//! Chip select is a plain GPIO, asserted around whole frames by this bus -- a register write
//! is ONE frame carrying header and data, which is the reason [`QspiDisplayBus`] exists as a
//! trait distinct from the 4-wire SPI one.

use light_core::hal::{Clock, QspiDisplayBus};

use crate::gpio::{self, Output};
use crate::pac;

/// `out pins, 4 side 0` with optional sideset (enable bit 12).
const INSTR_OUT4: u16 = 0x7004;
/// `nop side 1` (mov y, y with sideset enable and the side bit).
const INSTR_NOP_SIDE1: u16 = 0xB842;

pub struct PioQspiDisplayBus {
        cs: Output,
        reset: Output,
        sm: usize,
        dma_ch: usize,
        /// Whether a pixel frame is open (CS low with DMA possibly in flight).
        in_pixels: bool,
}

impl PioQspiDisplayBus {
        /// Claims PIO0 state machine `sm` and DMA channel `dma_ch`, which nothing else --
        /// Rust or the C shell -- may use while this lives. `sclk` and the four contiguous
        /// data pins `d0..d0+3` are muxed to PIO0; `cs` and `reset` stay GPIO. `power_en`,
        /// where the board has one, is driven high and forgotten.
        ///
        /// The QSPI clock is sys/4 (the reference's clkdiv of 2 over the 2-cycle loop):
        /// 37.5 MHz at the stock 150 MHz.
        ///
        /// # Safety
        ///
        /// Construct once; see above for what it claims.
        pub unsafe fn new(sclk: usize, d0: usize, cs: usize, reset: usize, power_en: Option<usize>, dma_ch: usize) -> Self {
                let cs = Output::new(cs, true);
                let reset = Output::new(reset, true);
                if let Some(p) = power_en {
                        let mut en = Output::new(p, true);
                        en.set(true);
                        core::mem::forget(en);
                }

                let sm_index = 0usize;
                let pio = unsafe { &*pac::PIO0::ptr() };
                let resets = unsafe { &*pac::RESETS::ptr() };
                resets.reset().modify(|_, w| w.pio0().clear_bit());
                while resets.reset_done().read().pio0().bit_is_clear() {}

                // the two-instruction program, at offsets 0 and 1
                pio.instr_mem(0).write(|w| unsafe { w.bits(u32::from(INSTR_OUT4)) });
                pio.instr_mem(1).write(|w| unsafe { w.bits(u32::from(INSTR_NOP_SIDE1)) });

                let smr = pio.sm(sm_index);
                //   out pins: the four data lines; sideset: SCLK, 2 bits counted (value +
                // the optional-enable bit), enable flag in EXECCTRL
                smr.sm_pinctrl().write(|w| unsafe {
                        w.out_base().bits(d0 as u8).out_count().bits(4).sideset_base().bits(sclk as u8).sideset_count().bits(2)
                });
                smr.sm_execctrl().write(|w| unsafe { w.side_en().set_bit().wrap_bottom().bits(0).wrap_top().bits(1) });
                //   autopull at 8: one FIFO word carries one byte (pushed `<< 24`), shifted
                // out MSB-first (left)
                smr.sm_shiftctrl().write(|w| unsafe { w.autopull().set_bit().pull_thresh().bits(8).out_shiftdir().clear_bit() });
                // sys/2 per PIO cycle, two cycles per nibble: QSPI clock = sys/4
                smr.sm_clkdiv().write(|w| unsafe { w.int().bits(2).frac().bits(0) });

                //   pin directions: SCLK + D0..D3 are outputs FROM THE SM's point of view,
                // set by executing `set pindirs` with SET mapped over the five contiguous
                // pins (SCLK at d0-1 on this board's wiring: 20, 21..24 -- contiguity is
                // assumed and asserted)
                debug_assert!(d0 == sclk + 1, "SCLK and D0..D3 contiguous, SCLK first");
                smr.sm_pinctrl().modify(|_, w| unsafe { w.set_base().bits(sclk as u8).set_count().bits(5) });
                // SET PINDIRS, 0b11111
                smr.sm_instr().write(|w| unsafe { w.bits(0xE09F) });
                // restore the run mapping (SET no longer needed)
                smr.sm_pinctrl().write(|w| unsafe {
                        w.out_base().bits(d0 as u8).out_count().bits(4).sideset_base().bits(sclk as u8).sideset_count().bits(2)
                });

                for pin in [sclk, d0, d0 + 1, d0 + 2, d0 + 3] {
                        gpio::set_function(pin, gpio::FUNC_PIO0);
                }

                // enable the state machine
                pio.ctrl().modify(|_, w| unsafe { w.sm_enable().bits(1 << sm_index) });

                Self { cs, reset, sm: sm_index, dma_ch, in_pixels: false }
        }

        fn pio() -> &'static pac::pio0::RegisterBlock {
                unsafe { &*pac::PIO0::ptr() }
        }

        fn tx_full(&self) -> bool {
                Self::pio().fstat().read().txfull().bits() & (1 << self.sm) != 0
        }

        /// Push one byte into the SM, MSB-aligned as the autopull expects.
        fn push_byte(&mut self, b: u8) {
                while self.tx_full() {
                        core::hint::spin_loop();
                }
                Self::pio().txf(self.sm).write(|w| unsafe { w.bits(u32::from(b) << 24) });
        }

        /// The reference's software 1-bit framing: each source byte becomes four bytes in
        /// which its bits, MSB first, occupy D0 across eight clocks (two source bits per
        /// expanded byte: one in the low nibble's D0, one in the high nibble's D0).
        fn push_expanded(&mut self, val: u8) {
                for i in (0..4).rev() {
                        let bit_lo = (val >> (2 * i)) & 1;
                        let bit_hi = (val >> (2 * i + 1)) & 1;
                        self.push_byte(bit_lo | (bit_hi << 4));
                }
        }

        /// Wait until the SM has genuinely finished clocking: TXSTALL is cleared before a
        /// transfer and sets only when the SM starves with an empty FIFO.
        fn clear_stall(&mut self) {
                Self::pio().fdebug().write(|w| unsafe { w.bits(1 << (24 + self.sm)) });
        }

        fn stalled(&self) -> bool {
                Self::pio().fdebug().read().txstall().bits() & (1 << self.sm) != 0
        }

        /// Blocking drain: everything pushed so far has left the wire.
        fn drain(&mut self) {
                self.clear_stall();
                while !self.stalled() {
                        core::hint::spin_loop();
                }
        }
}

impl QspiDisplayBus for PioQspiDisplayBus {
        fn write_register(&mut self, cmd: u8, data: &[u8]) {
                //   one CS frame: the serial header (opcode 0x02, the command in the middle
                // address byte), then the parameters -- everything in expanded 1-bit framing
                self.cs.set(false);
                self.push_expanded(0x02);
                self.push_expanded(0x00);
                self.push_expanded(cmd);
                self.push_expanded(0x00);
                for &b in data {
                        self.push_expanded(b);
                }
                self.drain();
                self.cs.set(true);
        }

        fn begin_pixels(&mut self, cmd: u8) {
                //   opcode 0x32: the pixel-interface write, after which the data lines carry
                // raw 4-bit-per-clock pixel bytes
                self.cs.set(false);
                self.push_expanded(0x32);
                self.push_expanded(0x00);
                self.push_expanded(cmd);
                self.push_expanded(0x00);
                self.drain();
                self.in_pixels = true;
        }

        unsafe fn start_data(&mut self, bytes: &[u8]) {
                self.clear_stall();
                let pio = Self::pio();
                let dma = unsafe { &*pac::DMA::ptr() };
                let ch = dma.ch(self.dma_ch);
                ch.ch_read_addr().write(|w| unsafe { w.bits(bytes.as_ptr() as u32) });
                ch.ch_write_addr().write(|w| unsafe { w.bits(pio.txf(self.sm).as_ptr() as u32) });
                ch.ch_trans_count().write(|w| unsafe { w.bits(bytes.len() as u32) });
                //   byte transfers paced by PIO0's TX DREQ for this SM (DREQ 0..3 are
                // PIO0_TX0..3 on both chips); chained to itself = no chain
                ch.ch_ctrl_trig().write(|w| unsafe {
                        w.data_size()
                                .size_byte()
                                .incr_read()
                                .set_bit()
                                .incr_write()
                                .clear_bit()
                                .treq_sel()
                                .bits(self.sm as u8)
                                .chain_to()
                                .bits(self.dma_ch as u8)
                                .en()
                                .set_bit()
                });
        }

        fn is_complete(&mut self) -> bool {
                let dma = unsafe { &*pac::DMA::ptr() };
                if dma.ch(self.dma_ch).ch_ctrl_trig().read().busy().bit_is_set() {
                        return false;
                }
                //   DMA done only means the FIFO is fed; the SM is still clocking until it
                // stalls on the empty FIFO
                if !self.stalled() {
                        return false;
                }
                if self.in_pixels {
                        self.cs.set(true);
                        self.in_pixels = false;
                }
                true
        }

        fn reset_pulse(&mut self, clock: &mut dyn Clock) {
                // the reference's timings for this panel: generous, init-only
                self.reset.set(true);
                clock.delay_ms(200);
                self.reset.set(false);
                clock.delay_ms(200);
                self.reset.set(true);
                clock.delay_ms(200);
        }
}
