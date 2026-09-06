//! PCM-to-duty conversion for PWM audio: the portable half of the framework's second
//! audio provider, ported from the predecessor C framework. A passive piezo or buzzer on a
//! PWM-capable pin plays 8-bit unsigned duty values at a supersonic carrier; this module
//! is the ONE place a source sample format is interpreted on the way there. The transport
//! -- the carrier, the pacing DMA, the tone mode -- is a port crate's business
//! (`light_rp2::pwm_audio`).
//!
//! The load-bearing property, kept from the original along with its test suite: silence is MID-SCALE
//! ([`DUTY_SILENCE`]), and volume attenuates towards it, never towards zero. Attenuating
//! towards zero slides the output's DC level with the volume, and a piezo renders a DC
//! step as an audible click. Every way of getting this arithmetic wrong is audible and
//! none of them are visible in the code -- which is why the tests below exist (several
//! were added originally only because a mutant survived the first version).

/// Volume is per-mille, `0..=1000` -- the original scale, kept so the tested arithmetic ports
/// verbatim. (The ES8311 codec's `set_volume` runs 0..100 because that is a register
/// mapping; this is a sample-domain attenuation, a different thing.)
pub const VOLUME_MAX: u16 = 1000;

/// Duty values are 8-bit unsigned: what one byte-wide DMA write into a wrap-255 PWM
/// compare register plays with no conversion on the way.
pub const DUTY_MAX: u8 = 255;
/// Mid-scale: silence, and the value the transport parks the output at.
pub const DUTY_SILENCE: u8 = 128;

/// Source sample encodings. Uncompressed PCM only; the enum is the seam a compressed
/// format would slot into without the call signature changing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
        /// Unsigned 8-bit PCM, centred on 128. At full volume the conversion is the
        /// identity, which is what lets a pre-baked asset play straight out of flash.
        PcmU8,
        /// Signed 16-bit PCM -- WAV's native format.
        PcmS16,
}

/// One PCM sample to 8-bit unsigned duty at `volume`.
pub fn pcm_to_duty(sample: i32, encoding: Encoding, volume: u16) -> u8 {
        let volume = i32::from(volume.min(VOLUME_MAX));

        //   everything is brought to a signed value centred on zero first, so there is one
        // attenuation and one re-centring rather than a separate path per encoding
        let centred = match encoding {
                Encoding::PcmU8 => (sample & 0xFF) - 128, // -128..=127
                Encoding::PcmS16 => {
                        //   an arithmetic shift, NOT a divide by 256: a divide truncates
                        // towards zero, which maps two adjacent input codes onto the same
                        // output either side of silence and puts a flat step in the middle
                        // of every waveform
                        sample.clamp(-32768, 32767) >> 8 // -128..=127
                }
        };

        //   attenuate about zero -- which after re-centring below is silence, not the
        // bottom of the range
        let centred = centred * volume / i32::from(VOLUME_MAX);

        //   the arithmetic above cannot leave 0..=255, but the clamp costs nothing and
        // means a future encoding cannot silently wrap the duty past the PWM's wrap value
        (centred + i32::from(DUTY_SILENCE)).clamp(0, i32::from(DUTY_MAX)) as u8
}

#[cfg(test)]
mod tests {
        use super::*;

        const SIL: u8 = DUTY_SILENCE;

        #[test]
        fn anchors_define_the_mapping() {
                //   silence must land on mid-scale. Landing on 0 instead is the classic
                // signed-to-unsigned slip, and it does not sound quiet -- it pins the
                // transducer to one rail
                assert_eq!(pcm_to_duty(0, Encoding::PcmS16, VOLUME_MAX), SIL);
                assert_eq!(pcm_to_duty(128, Encoding::PcmU8, VOLUME_MAX), SIL);
                // full scale reaches the ends without wrapping past them
                assert_eq!(pcm_to_duty(-32768, Encoding::PcmS16, VOLUME_MAX), 0);
                assert_eq!(pcm_to_duty(32767, Encoding::PcmS16, VOLUME_MAX), DUTY_MAX);
                assert_eq!(pcm_to_duty(0, Encoding::PcmU8, VOLUME_MAX), 0);
                assert_eq!(pcm_to_duty(255, Encoding::PcmU8, VOLUME_MAX), DUTY_MAX);
        }

        #[test]
        fn u8_at_full_volume_is_the_identity() {
                //   what the zero-copy path assumes: a pre-baked U8 asset plays where it
                // lies because this conversion would not change a byte of it
                for u in 0..=255 {
                        assert_eq!(pcm_to_duty(u, Encoding::PcmU8, VOLUME_MAX), u as u8, "at {u}");
                }
        }

        #[test]
        fn s16_mapping_is_monotonic() {
                //   louder input never produces quieter output
                let mut prev = 0u8;
                let mut s = -32768i32;
                while s <= 32767 {
                        let d = pcm_to_duty(s, Encoding::PcmS16, VOLUME_MAX);
                        assert!(d >= prev, "not monotonic at {s}: {d} after {prev}");
                        prev = d;
                        s += 7;
                }
        }

        #[test]
        fn volume_collapses_to_silence_not_zero() {
                //   volume 0 must be flat MID-SCALE for every input. If attenuation ran
                // towards zero this would be 0 instead -- equally "silent" in the sense of
                // constant, but a full-scale DC step away from the waveform, which a piezo
                // renders as a click every time the volume drops
                for s in [-32768, -20000, -1, 0, 1, 20000, 32767] {
                        assert_eq!(pcm_to_duty(s, Encoding::PcmS16, 0), SIL, "input {s}");
                }
                //   and the range shrinks monotonically ABOUT THE MIDPOINT as volume falls
                // -- midpoint drift IS the DC step this arrangement exists to avoid
                let mut prev_span = i32::MAX;
                let mut v = VOLUME_MAX as i32;
                while v >= 0 {
                        let lo = i32::from(pcm_to_duty(-32768, Encoding::PcmS16, v as u16));
                        let hi = i32::from(pcm_to_duty(32767, Encoding::PcmS16, v as u16));
                        let span = hi - lo;
                        assert!(span <= prev_span, "volume {v} widened the span to {span}");
                        let mid = (hi + lo) / 2;
                        assert!(mid >= i32::from(SIL) - 1 && mid <= i32::from(SIL), "volume {v} moved the midpoint to {mid}");
                        prev_span = span;
                        v -= 50;
                }
        }

        #[test]
        fn half_volume_is_half_the_excursion() {
                let full = i32::from(pcm_to_duty(32767, Encoding::PcmS16, VOLUME_MAX)) - i32::from(SIL);
                let half = i32::from(pcm_to_duty(32767, Encoding::PcmS16, VOLUME_MAX / 2)) - i32::from(SIL);
                assert!(half * 2 >= full - 2 && half * 2 <= full + 2, "half excursion {half} vs full {full}");
        }

        #[test]
        fn out_of_range_clamps_never_wraps() {
                //   an encoder handing over a value outside the 16-bit range must not wrap
                // the duty past the PWM wrap -- that would alias to a completely wrong level
                assert_eq!(pcm_to_duty(100_000, Encoding::PcmS16, VOLUME_MAX), DUTY_MAX);
                assert_eq!(pcm_to_duty(-100_000, Encoding::PcmS16, VOLUME_MAX), 0);
                // and volume above the maximum must not amplify
                assert_eq!(pcm_to_duty(32767, Encoding::PcmS16, 5000), DUTY_MAX);
        }
}
