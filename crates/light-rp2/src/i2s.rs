//! I2S audio through PIO, shaped the way the ES8311 boards wire it: the CODEC is the I2S
//! MASTER. This side only generates MCLK (a squarewave state machine) and answers the
//! codec's BCLK/LRCLK as a slave data-out -- which keeps the whole clock-divider problem in
//! the codec's own registers, where its datasheet solves it.
//!
//! Data is fed by PING-PONG DMA, never by polled FIFO writes: the four-word TX FIFO holds
//! 83 us of audio at 24 kHz and a single display draw is two hundred times that, which on
//! hardware was perfectly audible as chop. Two buffers chained through two DMA channels
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
use light_core::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

//   THE OUTPUT PREFETCH RING, drained from the DMA-completion INTERRUPT. The two ping-pong
// DMA buffers still carry the audio to the codec, but they are refilled by the IRQ from this
// ring rather than by the poll loop -- so a blocked main loop (a card read, a display push)
// no longer starves the codec: only the ring running dry does, and the ring is topped up
// toward full each poll with a lead that rides such gaps out. Single-producer (the poll
// loop, via `stream_push`) / single-consumer (the IRQ) on ONE core, so monotonic head/tail
// with acquire/release ordering is the whole synchronisation -- each index is written by
// exactly one side. The board supplies the backing store (so a board with no audio links
// none of it); its pointer and length are published here for the IRQ once, at start.
struct StreamRing {
        buf: AtomicU32,
        cap: AtomicUsize,
        /// Producer cursor (the poll loop): monotonic, wrapping; index is `head % cap`.
        head: AtomicUsize,
        /// Consumer cursor (the IRQ): monotonic, wrapping; index is `tail % cap`.
        tail: AtomicUsize,
        /// The two DMA buffers and their channels, for the IRQ to refill and re-arm.
        dma_buf: [AtomicU32; 2],
        dma_ch: [AtomicUsize; 2],
        /// Silence-filled output buffers while `active`, since the last reset.
        underruns: AtomicU32,
        /// The IRQ is live and the pointers above are valid.
        armed: AtomicBool,
        /// Gate on underrun accounting: an idle ring draining to silence is not starvation.
        active: AtomicBool,
}
// SAFETY: the ring is a single-producer/single-consumer queue on one core; every field is
// an atomic and each cursor is written by exactly one side (see the type's doc).
unsafe impl Sync for StreamRing {}

static STREAM: StreamRing = StreamRing {
        buf: AtomicU32::new(0),
        cap: AtomicUsize::new(0),
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        dma_buf: [AtomicU32::new(0), AtomicU32::new(0)],
        dma_ch: [AtomicUsize::new(0), AtomicUsize::new(0)],
        underruns: AtomicU32::new(0),
        armed: AtomicBool::new(false),
        active: AtomicBool::new(false),
};

/// RP2350 NVIC line for the DMA's IRQ 0 output (`DMA_IRQ_0`, from the SDK's intctrl regs).
const DMA_IRQ_0: u32 = 10;

unsafe extern "C" {
        //   the SDK owns the RAM vector table; install the handler through it rather than a
        // cortex-m-rt vector this shell does not have. `irq_handler_t` is `void(*)(void)`.
        fn irq_set_exclusive_handler(num: u32, handler: extern "C" fn());
        fn irq_set_enabled(num: u32, enabled: bool);
}

impl StreamRing {
        fn free(&self) -> usize {
                let cap = self.cap.load(Ordering::Relaxed);
                if cap == 0 {
                        return 0;
                }
                let head = self.head.load(Ordering::Relaxed);
                let tail = self.tail.load(Ordering::Acquire);
                cap - head.wrapping_sub(tail)
        }

        /// Producer: fill the contiguous free region and commit what `fill` wrote.
        fn push(&self, fill: &mut dyn FnMut(&mut [u32]) -> usize) {
                let cap = self.cap.load(Ordering::Relaxed);
                let base = self.buf.load(Ordering::Relaxed) as *mut u32;
                if cap == 0 || base.is_null() {
                        return;
                }
                let head = self.head.load(Ordering::Relaxed);
                let tail = self.tail.load(Ordering::Acquire);
                let free = cap - head.wrapping_sub(tail);
                if free == 0 {
                        return;
                }
                let widx = head % cap;
                let contig = (cap - widx).min(free);
                // SAFETY: [widx, widx+contig) is the free region, disjoint from the consumer's
                // [tail, head); the IRQ reads only the latter until head advances below.
                let slice = unsafe { core::slice::from_raw_parts_mut(base.add(widx), contig) };
                let n = fill(slice).min(contig);
                self.head.store(head.wrapping_add(n), Ordering::Release);
        }

        fn pending(&self) -> usize {
                let head = self.head.load(Ordering::Relaxed);
                let tail = self.tail.load(Ordering::Acquire);
                head.wrapping_sub(tail)
        }

        /// Consumer (IRQ): copy one DMA buffer's worth from the ring, silence past the end.
        fn drain_into(&self, dst: *mut u32) {
                let cap = self.cap.load(Ordering::Relaxed);
                let base = self.buf.load(Ordering::Relaxed) as *const u32;
                let head = self.head.load(Ordering::Acquire);
                let mut tail = self.tail.load(Ordering::Relaxed);
                let avail = if cap == 0 { 0 } else { head.wrapping_sub(tail).min(STREAM_WORDS) };
                for i in 0..STREAM_WORDS {
                        let w = if i < avail {
                                // SAFETY: base/cap are valid while armed; idx is in bounds
                                let v = unsafe { *base.add(tail % cap) };
                                tail = tail.wrapping_add(1);
                                v
                        } else {
                                0
                        };
                        // SAFETY: dst is one of the two DMA buffers, each STREAM_WORDS long
                        unsafe { *dst.add(i) = w };
                }
                self.tail.store(tail, Ordering::Release);
                if avail < STREAM_WORDS && self.active.load(Ordering::Relaxed) {
                        self.underruns.fetch_add(1, Ordering::Relaxed);
                }
        }
}

/// The DMA-completion handler for the output ring's two channels: refill each finished
/// buffer from the ring and re-arm it for the chain. Installed on `DMA_IRQ_0` at stream
/// start. Nothing here touches the card or the FS -- it is a pure RAM copy plus two register
/// writes, so it is safe to run at interrupt time.
#[unsafe(no_mangle)]
pub extern "C" fn light_i2s_dma_irq() {
        if !STREAM.armed.load(Ordering::Acquire) {
                return;
        }
        let dma = unsafe { &*pac::DMA::ptr() };
        let ints = dma.ints0().read().bits();
        for i in 0..2 {
                let ch = STREAM.dma_ch[i].load(Ordering::Relaxed);
                if ints & (1 << ch) == 0 {
                        continue;
                }
                // clear this channel's IRQ (write-1-to-clear) before re-arming
                dma.ints0().write(|w| unsafe { w.bits(1 << ch) });
                let buf = STREAM.dma_buf[i].load(Ordering::Relaxed) as *mut u32;
                STREAM.drain_into(buf);
                let c = dma.ch(ch);
                c.ch_read_addr().write(|w| unsafe { w.bits(buf as u32) });
                c.ch_trans_count().write(|w| unsafe { w.bits(STREAM_WORDS as u32) });
        }
}

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
/// 5 s test take); 200 ms each rides them out.
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
        //   the POLLED path's state; the IRQ path keeps its ring/drain/underruns in the
        // `STREAM` static instead (the interrupt reaches them without a handle)
        last_busy: [bool; 2],
        /// Times the whole stream starved on the POLLED path (both buffers drained).
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

        /// Hand over the two DMA buffers and start the chained ring for the POLLED path: each
        /// completed buffer waits for [`refill`](Self::refill) on the poll loop. No interrupt,
        /// no prefetch ring -- for a board too RAM-tight for [`start_stream_irq`]'s ring and
        /// with only light audio (a beep, a tone) that a poll can keep fed.
        pub fn start_stream(&mut self, bufs: [&'static mut [u32; STREAM_WORDS]; 2]) {
                self.configure_channel(self.ch[0], self.ch[1], bufs[0]);
                self.configure_channel(self.ch[1], self.ch[0], bufs[1]);
                self.bufs = Some(bufs);
                self.last_busy = [true, false];
                let dma = unsafe { &*pac::DMA::ptr() };
                dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << self.ch[0]) });
        }

        /// Refill whichever buffer the polled ring has finished with (see [`start_stream`]):
        /// `fill` is called with each completed buffer and the channel is re-armed. Both idle
        /// means the stream starved -- counted, refilled and restarted. Not used with the IRQ
        /// path, which re-arms from the interrupt instead.
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
                        light_core::warn!("i2s: stream ring drained; restarted");
                        self.underruns = self.underruns.wrapping_add(1);
                        self.last_busy = [true, false];
                        dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << self.ch[0]) });
                }
        }

        /// Underruns on the POLLED path; the IRQ path uses [`stream_underruns`](Self::stream_underruns).
        pub fn underruns(&self) -> u32 {
                self.underruns
        }

        /// Zero the polled-path underrun count.
        pub fn reset_polled_underruns(&mut self) {
                self.underruns = 0;
        }

        /// Hand over the two DMA buffers (their content plays first -- zeros are silence) and
        /// the PREFETCH `ring` (the board's backing store), then start: the ring's channels
        /// raise `DMA_IRQ_0` on each buffer's completion, and [`light_i2s_dma_irq`] refills
        /// that buffer from `ring` and re-arms it. The poll loop only keeps `ring` fed via
        /// [`stream_push`](Self::stream_push), so a blocked loop cannot starve the codec.
        pub fn start_stream_irq(&mut self, bufs: [&'static mut [u32; STREAM_WORDS]; 2], ring: &'static mut [u32]) {
                self.configure_channel(self.ch[0], self.ch[1], bufs[0]);
                self.configure_channel(self.ch[1], self.ch[0], bufs[1]);
                //   publish the ring and the DMA buffers/channels for the IRQ, THEN arm: the
                // handler bails until `armed`, so it never reads a half-written pointer set
                STREAM.buf.store(ring.as_mut_ptr() as u32, Ordering::Relaxed);
                STREAM.cap.store(ring.len(), Ordering::Relaxed);
                STREAM.head.store(0, Ordering::Relaxed);
                STREAM.tail.store(0, Ordering::Relaxed);
                STREAM.dma_buf[0].store(bufs[0].as_mut_ptr() as u32, Ordering::Relaxed);
                STREAM.dma_buf[1].store(bufs[1].as_mut_ptr() as u32, Ordering::Relaxed);
                STREAM.dma_ch[0].store(self.ch[0], Ordering::Relaxed);
                STREAM.dma_ch[1].store(self.ch[1], Ordering::Relaxed);
                self.bufs = Some(bufs);
                let dma = unsafe { &*pac::DMA::ptr() };
                //   raise IRQ 0 when either stream channel completes a buffer
                dma.inte0().modify(|r, w| unsafe { w.bits(r.bits() | (1 << self.ch[0]) | (1 << self.ch[1])) });
                STREAM.armed.store(true, Ordering::Release);
                // SAFETY: install our handler on the SDK's vector table and enable the line
                unsafe {
                        irq_set_exclusive_handler(DMA_IRQ_0, light_i2s_dma_irq);
                        irq_set_enabled(DMA_IRQ_0, true);
                }
                dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << self.ch[0]) });
        }

        /// Free words in the prefetch ring; see [`stream_push`](Self::stream_push).
        pub fn stream_free(&self) -> usize {
                STREAM.free()
        }

        /// Top the prefetch ring up: `fill` is handed the contiguous free region and returns
        /// how many words it wrote. Call in a loop while [`stream_free`](Self::stream_free)
        /// is non-zero; the IRQ drains what is committed here into the DMA buffers.
        pub fn stream_push(&mut self, fill: &mut dyn FnMut(&mut [u32]) -> usize) {
                STREAM.push(fill);
        }

        /// Gate underrun accounting: `true` while sound is intended, `false` when idle so a
        /// ring draining to silence is not miscounted as starvation.
        pub fn set_active(&mut self, active: bool) {
                STREAM.active.store(active, Ordering::Relaxed);
        }

        /// Words still queued in the ring; see [`stream_pending`](crate::i2s) contract.
        pub fn stream_pending(&self) -> usize {
                STREAM.pending()
        }

        /// Drop everything queued so the codec falls to silence at once. Done in a brief
        /// IRQ-off window so the drain cannot observe the two cursors mid-reset (the only
        /// place either cursor is written by the other side's owner).
        pub fn stream_clear(&mut self) {
                // SAFETY: disable then re-enable this crate's own DMA IRQ line
                unsafe { irq_set_enabled(DMA_IRQ_0, false) };
                STREAM.head.store(0, Ordering::Relaxed);
                STREAM.tail.store(0, Ordering::Relaxed);
                unsafe { irq_set_enabled(DMA_IRQ_0, true) };
        }

        /// Buffers the IRQ played as silence for want of data while active, since reset.
        pub fn stream_underruns(&self) -> u32 {
                STREAM.underruns.load(Ordering::Relaxed)
        }

        /// Zero the underrun counter.
        pub fn reset_underruns(&mut self) {
                STREAM.underruns.store(0, Ordering::Relaxed);
        }
}
