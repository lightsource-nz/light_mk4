//! One PWM output on a pin, through the pac: a backlight, a buzzer.
//!
//! Ported from the predecessor C framework's backlight driver, whose one finding is kept: the
//! carrier sits around 30 kHz --
//! above anything the eye or a camera shutter picks up as flicker, AND above the audible range,
//! unlike the 8-9 kHz it once ran at, which was squarely audible as an LED-driver whine on one
//! board's LCD backlight. Resolution comes from the wrap alone, not from the divider.

use crate::pac;

use crate::gpio::set_function;

/// GPIO function select for PWM, the same on both chips.
pub const FUNC_PWM: u8 = 4;

pub struct PwmOutput {
        slice: usize,
        channel_b: bool,
        top: u16,
}

impl PwmOutput {
        /// Route `pin` to its PWM slice, counting to `top` at about `carrier_hz`. The divider is
        /// integer and clamped, so the carrier is approximate; `top` is exact. Starts at duty 0.
        pub fn new(pin: usize, sys_hz: u32, carrier_hz: u32, top: u16) -> Self {
                //   GPIO 0..31 map onto slices 0..7 exactly as on the RP2040 -- (n / 2) & 7 --
                // and only GPIO 32..47 reach the four extra slices; channel B when n is odd
                // (pico-sdk's PWM_GPIO_SLICE_NUM). Not (n / 2) % 12: that put GPIO 25 on slice
                // 0, which ran happily while the pad it was not wired to stayed low
                let slice = if pin < 32 { (pin / 2) & 7 } else { 8 + ((pin / 2) & 3) };
                let channel_b = pin % 2 == 1;
                let resets = unsafe { &*pac::RESETS::ptr() };
                //   the SDK's runtime unresets the block already; making sure costs nothing and
                // means this driver does not depend on that
                resets.reset().modify(|_, w| w.pwm().clear_bit());
                while resets.reset_done().read().pwm().bit_is_clear() {}
                let pwm = unsafe { &*pac::PWM::ptr() };
                let ch = pwm.ch(slice);
                let div = (sys_hz / (u32::from(top) + 1) / carrier_hz.max(1)).clamp(1, 255) as u8;
                ch.csr().write(|w| w.en().clear_bit());
                ch.div().write(|w| unsafe { w.int().bits(div).frac().bits(0) });
                ch.top().write(|w| unsafe { w.top().bits(top) });
                ch.ctr().write(|w| unsafe { w.ctr().bits(0) });
                let mut out = Self { slice, channel_b, top };
                out.set_duty(0);
                set_function(pin, FUNC_PWM);
                ch.csr().write(|w| w.en().set_bit());
                out
        }

        pub fn top(&self) -> u16 {
                self.top
        }

        /// The slice's CSR, DIV, TOP, CTR and CC registers and the pin's IO status, for bring-up.
        pub fn registers(&self, pin: usize) -> [u32; 7] {
                let pwm = unsafe { &*pac::PWM::ptr() };
                let io = unsafe { &*pac::IO_BANK0::ptr() };
                let ch = pwm.ch(self.slice);
                [ch.csr().read().bits(), ch.div().read().bits(), ch.top().read().bits(), ch.ctr().read().bits(), ch.cc().read().bits(), io.gpio(pin).gpio_ctrl().read().bits(), io.gpio(pin).gpio_status().read().bits()]
        }

        /// Duty in `0..=top`; `top` is fully on (the compare is inclusive of the wrap).
        pub fn set_duty(&mut self, level: u16) {
                let level = level.min(self.top);
                //   a level equal to top+1 would be 100% exactly; top leaves one count low,
                // which no backlight can show. Fully on means the full period
                let level = if level == self.top { self.top + 1 } else { level };
                let pwm = unsafe { &*pac::PWM::ptr() };
                let ch = pwm.ch(self.slice);
                if self.channel_b {
                        ch.cc().modify(|_, w| unsafe { w.b().bits(level) });
                } else {
                        ch.cc().modify(|_, w| unsafe { w.a().bits(level) });
                }
        }
}
