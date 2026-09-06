//! PWM streaming audio out: the transport half of the framework's second audio provider,
//! ported from the predecessor C framework's PWM streaming and tone paths (hw-verified
//! on a piezo there). Two completely different configurations of one PWM slice:
//!
//! - **The sample path**: "DAC mode" -- undivided clock, wrap 255 (a ~586 kHz carrier on
//!   a 150 MHz part, far above anything audible, leaving 8 bits of duty resolution) --
//!   with a WORD-wide DMA writing channel-positioned duty values into the compare
//!   register, paced by one of the DMA block's pacing timers at the sample rate for no
//!   CPU at all. Word-wide, never single bytes: the APB bridge replicates narrow
//!   writes across the lanes -- see [`PwmAudio::duty_word`], where that finding lives.
//!   Samples start as [`light_audio::pwm::pcm_to_duty`]'s 8-bit duty and are positioned
//!   by `duty_word` on their way into the buffer.
//! - **The tone path**: the PWM frequency IS the note, at 50% duty. A piezo is a sharply
//!   resonant device: a square wave near its resonance is far louder than PCM through the
//!   same pin will ever be, which is why this is a distinct mode and not synthesized
//!   samples.
//!
//! Silence is duty mid-scale, and stopping parks there: a piezo renders a DC step as a
//! click, so the output never jumps rails on start, stop or volume change.

use crate::gpio::{self, set_function};
use crate::pac;
use crate::pwm::FUNC_PWM;

/// The DAC mode: undivided clock, wrap 255.
const DAC_WRAP: u16 = 255;
const DAC_SILENCE: u16 = 128;
/// The tone mode's wrap: coarse enough to leave the 8-bit divider room to reach down
/// into the audible range -- 1000 counts puts the achievable band comfortably either
/// side of a piezo's resonance.
const TONE_WRAP: u16 = 1000;

/// DREQ number of DMA pacing timer 0; timers 1..3 follow. The same on both chips.
const DREQ_DMA_TIMER0: u8 = 59;

pub struct PwmAudio {
        pin: usize,
        slice: usize,
        channel_b: bool,
        sys_hz: u32,
        dma_ch: usize,
        dma_timer: usize,
}

impl PwmAudio {
        /// Claim `pin`'s PWM slice for audio, with a board-assigned DMA channel and one of
        /// the DMA block's four pacing timers. Parks in DAC mode at silence, so the first
        /// sample does not step the DC level.
        pub fn new(pin: usize, sys_hz: u32, dma_ch: usize, dma_timer: usize) -> Self {
                //   the slice map is the port crate's one subtlety, same as PwmOutput's:
                // GPIO 0..31 land on slices 0..7 as (n / 2) & 7; only 32..47 reach the four
                // extra RP2350 slices; channel B when the pin is odd
                let slice = if pin < 32 { (pin / 2) & 7 } else { 8 + ((pin / 2) & 3) };
                let channel_b = pin % 2 == 1;
                let resets = unsafe { &*pac::RESETS::ptr() };
                resets.reset().modify(|_, w| w.pwm().clear_bit());
                while resets.reset_done().read().pwm().bit_is_clear() {}
                let out = Self { pin, slice, channel_b, sys_hz, dma_ch, dma_timer };
                out.configure(DAC_WRAP, 1);
                out.set_level(DAC_SILENCE);
                out
        }

        /// The slice into a given wrap/divider, output enabled, the pin re-routed to PWM --
        /// re-routed because [`tone_off`](Self::tone_off) hands the pin back to SIO, and a
        /// caller that stops and restarts would otherwise drive a slice that no longer
        /// reaches the pin, and get silence with no error.
        fn configure(&self, wrap: u16, div: u8) {
                let pwm = unsafe { &*pac::PWM::ptr() };
                let ch = pwm.ch(self.slice);
                ch.csr().write(|w| w.en().clear_bit());
                ch.div().write(|w| unsafe { w.int().bits(div).frac().bits(0) });
                ch.top().write(|w| unsafe { w.top().bits(wrap) });
                ch.ctr().write(|w| unsafe { w.ctr().bits(0) });
                set_function(self.pin, FUNC_PWM);
                ch.csr().write(|w| w.en().set_bit());
        }

        fn set_level(&self, level: u16) {
                let pwm = unsafe { &*pac::PWM::ptr() };
                let ch = pwm.ch(self.slice);
                if self.channel_b {
                        ch.cc().modify(|_, w| unsafe { w.b().bits(level) });
                } else {
                        ch.cc().modify(|_, w| unsafe { w.a().bits(level) });
                }
        }

        /// A duty sample positioned for this pin's channel within the 32-bit CC register
        /// (A in the low half, B in the high). The stream is WORD-sized transfers of these:
        /// an earlier port streamed single bytes at the channel's byte offset, but the APB bridge
        /// upgrades narrow writes to word width by REPLICATING the byte across the lanes,
        /// so a duty D landed in the compare as D * 257 -- above the 255 wrap for every
        /// nonzero sample, pinning the output at constant high, which a piezo renders as
        /// silence. Found here when the sample path moved data at exactly the right pace
        /// and made no sound at all.
        pub fn duty_word(&self, duty: u8) -> u32 {
                if self.channel_b {
                        u32::from(duty) << 16
                } else {
                        u32::from(duty)
                }
        }

        /// A continuous square wave at `hz`: the carrier IS the note, 50% duty (a square
        /// wave drives a piezo hardest; amplitude is not meaningfully controllable this
        /// way -- volume belongs to the sample path).
        pub fn tone(&mut self, hz: u32) {
                if hz == 0 {
                        self.tone_off();
                        return;
                }
                //   f = sys / (div * (wrap + 1)), solved for div and rounded to nearest;
                // the hardware divider is 8-bit, so anything past 255 simply cannot go lower
                let period = u32::from(TONE_WRAP) + 1;
                let div = ((self.sys_hz + (hz * period) / 2) / (hz * period)).clamp(1, 255) as u8;
                self.configure(TONE_WRAP, div);
                self.set_level(TONE_WRAP / 2);
        }

        /// Silence after a tone: the slice off and the pin driven LOW rather than left
        /// floating -- a piezo across a floating pin picks up whatever the neighbouring
        /// lines are doing and hisses.
        pub fn tone_off(&mut self) {
                let pwm = unsafe { &*pac::PWM::ptr() };
                pwm.ch(self.slice).csr().write(|w| w.en().clear_bit());
                //   constructing the Output routes the pin to SIO and drives it
                let _low = gpio::Output::new(self.pin, false);
        }

        /// Play `duty.len()` samples at `sample_rate`, zero-copy -- the buffer plays from
        /// where it lies. Each entry is a [`duty_word`](Self::duty_word)-positioned CC
        /// value, word-sized because of the APB narrow-write replication documented there.
        /// `false` when a play is already in flight or the rate is zero.
        pub fn play(&mut self, duty: &'static [u32], sample_rate: u32) -> bool {
                if sample_rate == 0 || duty.is_empty() || self.busy() {
                        return false;
                }
                //   into DAC mode, which a preceding tone will have configured away from
                self.configure(DAC_WRAP, 1);

                //   the pacing timer issues a DREQ at sys * X / Y; X = 1 keeps Y inside its
                // 16 bits for every rate above sys/65535 (~2.3 kHz on a 150 MHz part) and
                // holds the error to the rounding of a single divide
                let y = ((self.sys_hz + sample_rate / 2) / sample_rate).clamp(1, 0xFFFF) as u16;
                let dma = unsafe { &*pac::DMA::ptr() };
                //   X in the high half, Y in the low, TREQ rate = sys * X / Y -- the
                // register layout pico-sdk's dma_timer_set_fraction writes. The pac gives
                // each timer its own type, hence the dispatch
                let fraction = 1u32 << 16 | u32::from(y);
                match self.dma_timer {
                        0 => dma.timer0().write(|w| unsafe { w.bits(fraction) }),
                        1 => dma.timer1().write(|w| unsafe { w.bits(fraction) }),
                        2 => dma.timer2().write(|w| unsafe { w.bits(fraction) }),
                        _ => dma.timer3().write(|w| unsafe { w.bits(fraction) }),
                }

                let pwm = unsafe { &*pac::PWM::ptr() };
                let c = dma.ch(self.dma_ch);
                c.ch_read_addr().write(|w| unsafe { w.bits(duty.as_ptr() as u32) });
                c.ch_write_addr().write(|w| unsafe { w.bits(pwm.ch(self.slice).cc().as_ptr() as u32) });
                c.ch_trans_count().write(|w| unsafe { w.bits(duty.len() as u32) });
                c.ch_ctrl_trig().write(|w| unsafe {
                        w.data_size()
                                .size_word()
                                .incr_read()
                                .set_bit()
                                .incr_write()
                                .clear_bit()
                                .treq_sel()
                                .bits(DREQ_DMA_TIMER0 + self.dma_timer as u8)
                                .chain_to()
                                .bits(self.dma_ch as u8)
                                .en()
                                .set_bit()
                });
                true
        }

        pub fn busy(&self) -> bool {
                let dma = unsafe { &*pac::DMA::ptr() };
                dma.ch(self.dma_ch).ch_ctrl_trig().read().busy().bit_is_set()
        }

        /// Abort a play in flight and park at silence: stopping mid-sample settles the
        /// output at zero average instead of holding whatever duty it had reached.
        pub fn stop(&mut self) {
                let dma = unsafe { &*pac::DMA::ptr() };
                //   EN off before the abort, the discipline the capture path settled on
                dma.ch(self.dma_ch).ch_al1_ctrl().modify(|_, w| w.en().clear_bit());
                dma.chan_abort().write(|w| unsafe { w.bits(1 << self.dma_ch) });
                for _ in 0..1_000_000u32 {
                        if dma.chan_abort().read().bits() == 0 {
                                break;
                        }
                }
                self.set_level(DAC_SILENCE);
        }
}
