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
//! ; dout: Waveshare's reference I2S slave writer. One `pull block` per FRAME -- the wrap
//! ; returns to the second pull, so the steady-state loop consumes ONE 32-bit word per
//! ; LRCLK period: its TOP 16 bits shift out MSB-first during the left half, its LOW 16
//! ; during the right half. (A fill that wrote two words per sample here played every
//! ; sample twice -- speech an octave down, measured as a 2.00 s file taking 4.03 s.)
//! ; The first pull + wait is entry alignment only; that word is discarded. The wait pins
//! ; are baked into the instructions (PIO wait-on-gpio addresses an absolute pin),
//! ; assembled here from the board's wiring.
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
const SM_DIN: usize = 2;
const MCLK_ORIGIN: u16 = 0;
const DOUT_ORIGIN: u16 = 5;
const DIN_ORIGIN: u16 = 23;
/// DREQ for PIO1's TX FIFOs starts at 8 on both chips; RX follows at 12.
const DREQ_PIO1_TX0: u8 = 8;
const DREQ_PIO1_RX0: u8 = 12;

/// Words per stream buffer: ONE word per frame (top 16 bits = left slot, low 16 = right --
/// see the dout program above), so 2048 words is 2048 frames, ~85 ms at 24 kHz. Sized to
/// ride out the WORST poll-to-poll gap, MEASURED, not guessed: with rendering paused a
/// playback still hit a 64.8 ms gap -- an SD card's occasional internal read stall, the
/// same medium behaviour the capture buffers are sized for -- and 53 ms buffers restarted
/// the ring mid-play. 85 ms covers the measured stall with margin and still links beside
/// the 3.49's dual framebuffers and core 1's relocated stack.
pub const STREAM_WORDS: usize = 2048;

/// Samples per CAPTURE buffer: mono 16-bit, 200 ms at 24 kHz per buffer. Sized against
/// the medium, not the poll: an SD card's occasional garbage-collection stall runs
/// 100-250 ms, and with only 100 ms buffers those stalls cost audio (7 overruns in a
/// 5 s bench take); 200 ms each rides them out.
pub const CAP_WORDS: usize = 4800;

/// The MCLK generator and the slave data-out, on PIO1 state machines 0 and 1, with the
/// stream's two DMA channels.
pub struct PioI2sOut {
        //   held so the pads stay configured as inputs for the codec's clocks; the pull-ups
        // are irrelevant against the codec's push-pull drivers
        _bclk: Input,
        _lrclk: Input,
        pin_bclk: usize,
        pin_lrclk: usize,
        ch: [usize; 2],
        bufs: Option<[&'static mut [u32; STREAM_WORDS]; 2]>,
        last_busy: [bool; 2],
        /// Times the whole stream starved (both buffers drained before a refill).
        pub underruns: u32,
        //   the capture (microphone) side, present after `attach_capture`
        _din: Option<Input>,
        pin_din: usize,
        cap_ch: [usize; 2],
        cap_bufs: Option<[&'static mut [u16; CAP_WORDS]; 2]>,
        cap_last_busy: [bool; 2],
        cap_running: bool,
        /// Times both capture buffers filled before a take -- audio LOST, not guessed at.
        pub cap_overruns: u32,
}

impl PioI2sOut {
        /// Claims PIO1 state machines 0 and 1, DMA channels `dma_a`/`dma_b` and the four
        /// pins, none of which anything else may use while this lives. MCLK starts
        /// immediately; the data machine sits waiting on the codec's clocks.
        ///
        /// # Safety
        ///
        /// Construct once; see above for what it claims.
        pub unsafe fn new(dout: usize, bclk_pin: usize, lrclk_pin: usize, mclk: usize, sys_hz: u32, mclk_hz: u32, dma_a: usize, dma_b: usize) -> Self {
                let (bclk, lrclk) = (bclk_pin, lrclk_pin);
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

                Self {
                        _bclk: bclk,
                        _lrclk: lrclk,
                        pin_bclk: bclk_pin,
                        pin_lrclk: lrclk_pin,
                        ch: [dma_a, dma_b],
                        bufs: None,
                        last_busy: [false; 2],
                        underruns: 0,
                        _din: None,
                        pin_din: 0,
                        cap_ch: [0; 2],
                        cap_bufs: None,
                        cap_last_busy: [false; 2],
                        cap_running: false,
                        cap_overruns: 0,
                }
        }

        /// Add the capture (microphone) machine: PIO1 state machine 2 samples the codec's
        /// ADC output on `din` against the same BCLK/LRCLK the codec masters -- the LEFT
        /// half-frame, 16 bits MSB-first on rising edges, after the I2S one-bit delay.
        /// Claims the state machine and DMA channels `dma_a`/`dma_b`; the machine sits
        /// disabled until [`capture_start`](Self::capture_start).
        ///
        /// # Safety
        ///
        /// Call once, after `new`; nothing else may use what it claims.
        pub unsafe fn attach_capture(&mut self, din: usize, dma_a: usize, dma_b: usize) {
                let pio = unsafe { &*pac::PIO1::ptr() };
                let w1 = |pin: usize| 0x2080 | pin as u16;
                let w0 = |pin: usize| 0x2000 | pin as u16;
                let o = DIN_ORIGIN;
                //   sample the RIGHT slot: the ES8311's mono ADC lands there, and reading
                // the LEFT slot returned exact zeros (the whole cause of silent captures --
                // ADC and analog mic both proven live by the ADC->DAC monitor). Full
                // per-frame resync (wrap to the top) so each grab catches a clean edge
                let din_prog: [u16; 9] = [
                        w0(self.pin_lrclk), // wait for the left half / idle
                        w1(self.pin_lrclk), // the right half begins on this rising edge
                        w1(self.pin_bclk),  // the I2S delay bit's rising edge
                        0xE02F,             // set x, 15
                        w0(self.pin_bclk),
                        w1(self.pin_bclk), // data valid on the rising edge
                        0x4001,            // in pins, 1
                        0x0040 | (o + 4),  // jmp x--
                        0x8020,            // push block; the wrap returns to the top
                ];
                for (i, ins) in din_prog.iter().enumerate() {
                        pio.instr_mem(usize::from(DIN_ORIGIN) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }
                let smr = pio.sm(SM_DIN);
                smr.sm_pinctrl().write(|w| unsafe { w.in_base().bits(din as u8) });
                //   wrap to the very top (instr 0) so every frame re-waits for a clean
                // LRCLK low->high edge before grabbing the right slot
                smr.sm_execctrl().write(|w| unsafe { w.wrap_bottom().bits(DIN_ORIGIN as u8).wrap_top().bits(DIN_ORIGIN as u8 + 8) });
                //   shift LEFT explicitly: the default here is shift-RIGHT, which lands the
                // 16-bit sample in the HIGH half of the pushed word ([31:16]) while the
                // halfword DMA reads the LOW half -- exact-zero captures despite a live SM
                // (the raw-FIFO probe read 0xa0000000, real audio in the wrong half). Left
                // puts the sample in [15:0] where the halfword read grabs it. RX joined to
                // 8 words of margin.
                smr.sm_shiftctrl().write(|w| w.fjoin_rx().set_bit().in_shiftdir().clear_bit());
                smr.sm_clkdiv().write(|w| unsafe { w.int().bits(1).frac().bits(0) });
                //   DIN routed to the PIO, exactly as the vendor's pio_gpio_init does --
                // NOT an SIO input. On RP2350 a state machine's `in pins` reads the pad
                // through the input path that funcsel selects, and an SIO-function pad read
                // back constant zero here (the whole cause of silent captures); a pull-up
                // is wrong besides, fighting the codec's push-pull SDOUT. set_function
                // enables the input and clears the pad isolation, which is all PIO needs.
                gpio::set_function(din, gpio::FUNC_PIO1);
                self._din = None;
                self.pin_din = din;
                self.cap_ch = [dma_a, dma_b];
        }

        fn configure_cap_channel(&self, ch: usize, other: usize, buf: &[u16; CAP_WORDS]) {
                let dma = unsafe { &*pac::DMA::ptr() };
                let pio = unsafe { &*pac::PIO1::ptr() };
                let c = dma.ch(ch);
                c.ch_read_addr().write(|w| unsafe { w.bits(pio.rxf(SM_DIN).as_ptr() as u32) });
                c.ch_write_addr().write(|w| unsafe { w.bits(buf.as_ptr() as u32) });
                c.ch_trans_count().write(|w| unsafe { w.bits(CAP_WORDS as u32) });
                //   halfword transfers: the sample sits in the RX register's low 16 bits,
                // and a narrow FIFO read still pops
                c.ch_al1_ctrl().write(|w| unsafe {
                        w.data_size()
                                .size_halfword()
                                .incr_read()
                                .clear_bit()
                                .incr_write()
                                .set_bit()
                                .treq_sel()
                                .bits(DREQ_PIO1_RX0 + SM_DIN as u8)
                                .chain_to()
                                .bits(other as u8)
                                .en()
                                .set_bit()
                });
        }

        /// Start capturing. The first call hands over the two buffers; later restarts pass
        /// `None` and reuse them. Stale FIFO content is flushed, the machine restarts at
        /// its origin, and the ring runs until [`capture_stop`](Self::capture_stop).
        pub fn capture_start(&mut self, bufs: Option<[&'static mut [u16; CAP_WORDS]; 2]>) {
                if let Some(b) = bufs {
                        self.cap_bufs = Some(b);
                }
                let Some(cap) = self.cap_bufs.as_ref() else { return };
                let pio = unsafe { &*pac::PIO1::ptr() };
                let dma = unsafe { &*pac::DMA::ptr() };
                while pio.fstat().read().rxempty().bits() & (1 << SM_DIN) as u8 == 0 {
                        let _ = pio.rxf(SM_DIN).read();
                }
                self.configure_cap_channel(self.cap_ch[0], self.cap_ch[1], cap[0]);
                self.configure_cap_channel(self.cap_ch[1], self.cap_ch[0], cap[1]);
                self.cap_last_busy = [true, false];
                self.cap_running = true;
                pio.sm(SM_DIN).sm_instr().write(|w| unsafe { w.bits(u32::from(DIN_ORIGIN)) });
                pio.ctrl().modify(|r, w| unsafe { w.sm_enable().bits(r.sm_enable().bits() | (1 << SM_DIN)) });
                dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << self.cap_ch[0]) });
        }

        /// Stop capturing: the machine disabled, both channels disarmed and aborted.
        /// Buffers stay for the next start.
        pub fn capture_stop(&mut self) {
                if !self.cap_running {
                        return;
                }
                let pio = unsafe { &*pac::PIO1::ptr() };
                let dma = unsafe { &*pac::DMA::ptr() };
                pio.ctrl().modify(|r, w| unsafe { w.sm_enable().bits(r.sm_enable().bits() & !(1 << SM_DIN)) });
                //   EN off BEFORE the abort, and the wait BOUNDED: aborting a channel that
                // is stalled on a DREQ which will now never arrive can hold the abort bit
                // up -- an unbounded spin here wedged core 0 with the console unpolled,
                // the CDC RX filling, and the host blocking on its next write
                for ch in self.cap_ch {
                        dma.ch(ch).ch_al1_ctrl().modify(|_, w| w.en().clear_bit());
                }
                dma.chan_abort().write(|w| unsafe { w.bits((1 << self.cap_ch[0]) | (1 << self.cap_ch[1])) });
                for _ in 0..1_000_000u32 {
                        if dma.chan_abort().read().bits() == 0 {
                                break;
                        }
                }
                self.cap_running = false;
        }

        /// Probe the raw DIN pad: sample GPIO input `n` times and count highs, alongside
        /// the DIN state machine's program counter. A bring-up bisect for silent capture --
        /// a healthy mix of highs and lows means the codec's SDOUT is toggling (so the
        /// fault is in the PIO framing), all-low or all-high means the serial line is dead.
        pub fn din_probe(&self, n: u32) -> (u32, u32) {
                let sio = unsafe { &*pac::SIO::ptr() };
                let bit = 1u32 << (self.pin_din & 31);
                let mut highs = 0u32;
                for _ in 0..n {
                        if sio.gpio_in().read().bits() & bit != 0 {
                                highs += 1;
                        }
                }
                let pio = unsafe { &*pac::PIO1::ptr() };
                let pc = pio.sm(SM_DIN).sm_addr().read().bits();
                (highs, pc)
        }

        /// Enable the DIN state machine alone -- no DMA armed -- so its RX FIFO fills for
        /// [`din_fifo_probe`](Self::din_fifo_probe). Flushes stale words and restarts the
        /// program at its origin.
        pub fn capture_sm_only(&mut self) {
                let pio = unsafe { &*pac::PIO1::ptr() };
                pio.ctrl().modify(|r, w| unsafe { w.sm_enable().bits(r.sm_enable().bits() & !(1 << SM_DIN)) });
                while pio.fstat().read().rxempty().bits() & (1 << SM_DIN) as u8 == 0 {
                        let _ = pio.rxf(SM_DIN).read();
                }
                pio.sm(SM_DIN).sm_instr().write(|w| unsafe { w.bits(u32::from(DIN_ORIGIN)) });
                pio.ctrl().modify(|r, w| unsafe { w.sm_enable().bits(r.sm_enable().bits() | (1 << SM_DIN)) });
        }

        /// Drain up to four raw words the DIN machine has pushed into its RX FIFO -- what
        /// the state machine captured, BEFORE any DMA. Non-zero here with a silent file
        /// convicts the DMA/write path; zero convicts the SM framing. The state machine
        /// must be enabled (a capture in progress) for the FIFO to fill.
        pub fn din_fifo_probe(&self) -> [u32; 4] {
                let pio = unsafe { &*pac::PIO1::ptr() };
                let mut out = [0u32; 4];
                for slot in out.iter_mut() {
                        //   wait briefly for a word, then take it
                        let mut spin = 0u32;
                        while pio.fstat().read().rxempty().bits() & (1 << SM_DIN) as u8 != 0 {
                                spin += 1;
                                if spin > 2_000_000 {
                                        return out;
                                }
                        }
                        *slot = pio.rxf(SM_DIN).read().bits();
                }
                out
        }

        /// Hand each freshly FILLED capture buffer to `take`, then re-arm it for the ring.
        /// Both channels idle means audio was lost while nobody collected -- counted, and
        /// the ring restarted.
        pub fn capture_take(&mut self, mut take: impl FnMut(&[u16; CAP_WORDS])) {
                if !self.cap_running {
                        return;
                }
                let Some(bufs) = self.cap_bufs.as_mut() else { return };
                let dma = unsafe { &*pac::DMA::ptr() };
                let mut busy = [false; 2];
                for i in 0..2 {
                        busy[i] = dma.ch(self.cap_ch[i]).ch_ctrl_trig().read().busy().bit_is_set();
                        if self.cap_last_busy[i] && !busy[i] {
                                take(bufs[i]);
                                let c = dma.ch(self.cap_ch[i]);
                                c.ch_write_addr().write(|w| unsafe { w.bits(bufs[i].as_ptr() as u32) });
                                c.ch_trans_count().write(|w| unsafe { w.bits(CAP_WORDS as u32) });
                        }
                        self.cap_last_busy[i] = busy[i];
                }
                if !busy[0] && !busy[1] {
                        self.cap_overruns = self.cap_overruns.wrapping_add(1);
                        self.cap_last_busy = [true, false];
                        dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << self.cap_ch[0]) });
                }
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
                        // leave the codec clocking silence out of a stalled FIFO forever.
                        // Logged with a timestamp because the COUNT alone misled a tuning
                        // session: poll gaps measured ~18 ms against 53 ms buffers cannot
                        // drain the ring, so a lone increment is a transition-boundary
                        // artifact -- the log line says WHEN, which says which
                        light_core::warn!("i2s: stream ring drained; restarted");
                        self.underruns = self.underruns.wrapping_add(1);
                        self.last_busy = [true, false];
                        dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << self.ch[0]) });
                }
        }
}
