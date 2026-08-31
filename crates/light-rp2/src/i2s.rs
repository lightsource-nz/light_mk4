//! I2S audio through PIO, shaped the way the ES8311 boards wire it: the CODEC is the I2S
//! MASTER. This side only generates MCLK (a squarewave state machine) and answers the
//! codec's BCLK/LRCLK as a slave data-out -- which keeps the whole clock-divider problem in
//! the codec's own registers, where its datasheet solves it.
//!
//! Data is fed by PING-PONG DMA, never by polled FIFO writes: the four-word TX FIFO holds
//! 83 us of audio at 24 kHz and a single display draw is two hundred times that, which on
//! the bench was perfectly audible as chop. Two buffers chained through two DMA channels
//! carry ~21 ms each; the application refills whichever one completed on its own schedule,
//! and a stream that starves anyway is counted, not guessed about.
//!
//! Two hand-assembled programs on PIO1 (the C shell owns pioasm; this crate owns registers):
//!
//! ```text
//! ; mclk: a 5-cycle loop -- MCLK = sys / (clkdiv * 5), fractional divider allowed
//!     set pins, 1
//!     nop
//!     set pins, 0
//!     nop
//!     jmp 0
//!
//! ; dout: Waveshare's reference I2S slave writer. One `pull block` per CHANNEL HALF-FRAME,
//! ; 16 bits shifted out MSB-first on the codec's falling BCLK edges; the sample sits in
//! ; the TOP 16 bits of the pushed word. The wait pins are baked into the instructions
//! ; (PIO wait-on-gpio addresses an absolute pin), assembled here from the board's wiring.
//!     pull block
//!     wait 1 gpio LRCLK
//!     pull block
//!     wait 0 gpio LRCLK
//!     wait 1 gpio BCLK
//!     set x, 15
//!     wait 0 gpio BCLK ; out pins, 1 ; wait 1 gpio BCLK ; jmp x--
//!     wait 1 gpio LRCLK
//!     wait 1 gpio BCLK
//!     set x, 15
//!     wait 0 gpio BCLK ; out pins, 1 ; wait 1 gpio BCLK ; jmp x--
//!     jmp 2
//! ```

use crate::gpio::{self, Input};
use crate::pac;

const SM_MCLK: usize = 0;
const SM_DOUT: usize = 1;
const MCLK_ORIGIN: u16 = 0;
const DOUT_ORIGIN: u16 = 5;
/// DREQ for PIO1's TX FIFOs starts at 8 on both chips.
const DREQ_PIO1_TX0: u8 = 8;

/// Words per stream buffer: 512 frames (each frame is two words, left then right), ~21 ms
/// at 24 kHz -- comfortably past the longest poll-to-poll gap a frame draw causes.
pub const STREAM_WORDS: usize = 1024;

/// The MCLK generator and the slave data-out, on PIO1 state machines 0 and 1, with the
/// stream's two DMA channels.
pub struct PioI2sOut {
        //   held so the pads stay configured as inputs for the codec's clocks; the pull-ups
        // are irrelevant against the codec's push-pull drivers
        _bclk: Input,
        _lrclk: Input,
        ch: [usize; 2],
        bufs: Option<[&'static mut [u32; STREAM_WORDS]; 2]>,
        last_busy: [bool; 2],
        /// Times the whole stream starved (both buffers drained before a refill).
        pub underruns: u32,
}

impl PioI2sOut {
        /// Claims PIO1 state machines 0 and 1, DMA channels `dma_a`/`dma_b` and the four
        /// pins, none of which anything else may use while this lives. MCLK starts
        /// immediately; the data machine sits waiting on the codec's clocks.
        ///
        /// # Safety
        ///
        /// Construct once; see above for what it claims.
        pub unsafe fn new(dout: usize, bclk: usize, lrclk: usize, mclk: usize, sys_hz: u32, mclk_hz: u32, dma_a: usize, dma_b: usize) -> Self {
                let pio = unsafe { &*pac::PIO1::ptr() };
                let resets = unsafe { &*pac::RESETS::ptr() };
                resets.reset().modify(|_, w| w.pio1().clear_bit());
                while resets.reset_done().read().pio1().bit_is_clear() {}

                let mclk_prog: [u16; 5] = [0xe001, 0xa042, 0xe000, 0xa042, MCLK_ORIGIN];
                let w1 = |pin: usize| 0x2080 | pin as u16;
                let w0 = |pin: usize| 0x2000 | pin as u16;
                let o = DOUT_ORIGIN;
                let dout_prog: [u16; 18] = [
                        0x80a0,
                        w1(lrclk),
                        0x80a0,
                        w0(lrclk),
                        w1(bclk),
                        0xe02f,
                        w0(bclk),
                        0x6001,
                        w1(bclk),
                        0x0040 | (o + 6),
                        w1(lrclk),
                        w1(bclk),
                        0xe02f,
                        w0(bclk),
                        0x6001,
                        w1(bclk),
                        0x0040 | (o + 13),
                        o + 2,
                ];
                for (i, ins) in mclk_prog.iter().enumerate() {
                        pio.instr_mem(usize::from(MCLK_ORIGIN) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }
                for (i, ins) in dout_prog.iter().enumerate() {
                        pio.instr_mem(usize::from(DOUT_ORIGIN) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }

                //   the MCLK machine: SET drives the one pin; 5 instructions per period, so
                // the divider is sys / (mclk * 5) -- fractional, which lands 6.144 MHz
                // exactly at 150 MHz (4 + 226/256)
                let smr = pio.sm(SM_MCLK);
                smr.sm_pinctrl().write(|w| unsafe { w.set_base().bits(mclk as u8).set_count().bits(1) });
                smr.sm_execctrl().write(|w| unsafe { w.wrap_bottom().bits(MCLK_ORIGIN as u8).wrap_top().bits(MCLK_ORIGIN as u8 + 4) });
                let div256 = u64::from(sys_hz) * 256 / (u64::from(mclk_hz) * 5);
                smr.sm_clkdiv().write(|w| unsafe { w.int().bits((div256 >> 8) as u16).frac().bits((div256 & 0xFF) as u8) });
                // SET PINDIRS, 1: the pin is the SM's output
                smr.sm_instr().write(|w| unsafe { w.bits(0xE081) });

                //   the data machine: OUT drives DOUT, 16 bits per half-frame shifted left
                // (MSB first), explicit pulls, full-speed clock (the codec's BCLK paces it)
                let smr = pio.sm(SM_DOUT);
                smr.sm_pinctrl().write(|w| unsafe { w.out_base().bits(dout as u8).out_count().bits(1).set_base().bits(dout as u8).set_count().bits(1) });
                smr.sm_execctrl().write(|w| unsafe { w.wrap_bottom().bits(DOUT_ORIGIN as u8).wrap_top().bits(DOUT_ORIGIN as u8 + 17) });
                smr.sm_shiftctrl().write(|w| unsafe { w.autopull().clear_bit().pull_thresh().bits(0).out_shiftdir().clear_bit() });
                smr.sm_clkdiv().write(|w| unsafe { w.int().bits(1).frac().bits(0) });
                smr.sm_instr().write(|w| unsafe { w.bits(0xE081) });
                // start at the program's origin
                smr.sm_instr().write(|w| unsafe { w.bits(u32::from(DOUT_ORIGIN)) });

                gpio::set_function(mclk, gpio::FUNC_PIO1);
                gpio::set_function(dout, gpio::FUNC_PIO1);
                let bclk = Input::new_pull_up(bclk);
                let lrclk = Input::new_pull_up(lrclk);

                pio.ctrl().modify(|_, w| unsafe { w.sm_enable().bits((1 << SM_MCLK) | (1 << SM_DOUT)) });

                Self { _bclk: bclk, _lrclk: lrclk, ch: [dma_a, dma_b], bufs: None, last_busy: [false; 2], underruns: 0 }
        }

        fn configure_channel(&self, ch: usize, other: usize, buf: &[u32; STREAM_WORDS]) {
                let dma = unsafe { &*pac::DMA::ptr() };
                let pio = unsafe { &*pac::PIO1::ptr() };
                let c = dma.ch(ch);
                c.ch_read_addr().write(|w| unsafe { w.bits(buf.as_ptr() as u32) });
                c.ch_write_addr().write(|w| unsafe { w.bits(pio.txf(SM_DOUT).as_ptr() as u32) });
                c.ch_trans_count().write(|w| unsafe { w.bits(STREAM_WORDS as u32) });
                //   word transfers paced by PIO1's TX DREQ for the data machine, each
                // channel chained to the other: the ring never stops between buffers.
                // AL1_CTRL, not CTRL_TRIG: programming must not start anything
                c.ch_al1_ctrl().write(|w| unsafe {
                        w.data_size()
                                .size_word()
                                .incr_read()
                                .set_bit()
                                .incr_write()
                                .clear_bit()
                                .treq_sel()
                                .bits(DREQ_PIO1_TX0 + SM_DOUT as u8)
                                .chain_to()
                                .bits(other as u8)
                                .en()
                                .set_bit()
                });
        }

        /// Hand over the two stream buffers (their current content plays first -- zeros are
        /// silence) and start the ring: A drains, chains to B, and each completed buffer
        /// waits for [`refill`](Self::refill).
        pub fn start_stream(&mut self, bufs: [&'static mut [u32; STREAM_WORDS]; 2]) {
                self.configure_channel(self.ch[0], self.ch[1], bufs[0]);
                self.configure_channel(self.ch[1], self.ch[0], bufs[1]);
                self.bufs = Some(bufs);
                self.last_busy = [true, false];
                let dma = unsafe { &*pac::DMA::ptr() };
                dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << self.ch[0]) });
        }

        /// Refill whichever buffer the ring has finished with: `fill` is called with each
        /// such buffer, and the channel is re-armed for the chain to trigger. Both channels
        /// idle means the stream starved -- counted, refilled and restarted.
        pub fn refill(&mut self, mut fill: impl FnMut(&mut [u32; STREAM_WORDS])) {
                let Some(bufs) = self.bufs.as_mut() else { return };
                let dma = unsafe { &*pac::DMA::ptr() };
                let mut busy = [false; 2];
                for i in 0..2 {
                        busy[i] = dma.ch(self.ch[i]).ch_ctrl_trig().read().busy().bit_is_set();
                        if self.last_busy[i] && !busy[i] {
                                fill(bufs[i]);
                                let c = dma.ch(self.ch[i]);
                                c.ch_read_addr().write(|w| unsafe { w.bits(bufs[i].as_ptr() as u32) });
                                c.ch_trans_count().write(|w| unsafe { w.bits(STREAM_WORDS as u32) });
                        }
                        self.last_busy[i] = busy[i];
                }
                if !busy[0] && !busy[1] {
                        //   the whole ring drained before this poll: restart it rather than
                        // leave the codec clocking silence out of a stalled FIFO forever
                        self.underruns = self.underruns.wrapping_add(1);
                        self.last_busy = [true, false];
                        dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << self.ch[0]) });
                }
        }
}
