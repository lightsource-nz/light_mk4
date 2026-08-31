//! One-shot ADC reads on a pin, through the pac: a battery divider, a trimpot.
//!
//! The ADC runs from clk_adc, which pico-sdk's runtime has already parked on the 48 MHz USB
//! PLL; a single conversion is 96 of those cycles (~2 us), so a blocking read costs less than
//! a log line and needs no async machinery.

use crate::pac;

/// GPIO function select for "none" -- an analog pad belongs to no digital function.
const FUNC_NULL: u8 = 0x1f;

/// An analog input channel, claimed from its pin. Channels are GPIO 26..=29 on the RP2040 and
/// the RP2350A (channels 0..=3), GPIO 40..=47 on the RP2350B (channels 0..=7).
pub struct Adc {
        channel: u8,
}

impl Adc {
        pub fn new(pin: usize) -> Self {
                let resets = unsafe { &*pac::RESETS::ptr() };
                resets.reset().modify(|_, w| w.adc().clear_bit());
                while resets.reset_done().read().adc().bit_is_clear() {}
                //   the pad goes fully analog, the way pico-sdk's adc_gpio_init leaves it:
                // digital input disabled, output disabled, pulls off, no function
                let pads = unsafe { &*pac::PADS_BANK0::ptr() };
                let io = unsafe { &*pac::IO_BANK0::ptr() };
                pads.gpio(pin).modify(|_, w| w.ie().clear_bit().od().set_bit().pue().clear_bit().pde().clear_bit());
                io.gpio(pin).gpio_ctrl().write(|w| unsafe { w.funcsel().bits(FUNC_NULL) });
                #[cfg(feature = "rp2350")]
                pads.gpio(pin).modify(|_, w| w.iso().clear_bit());
                let adc = unsafe { &*pac::ADC::ptr() };
                adc.cs().modify(|_, w| w.en().set_bit());
                while adc.cs().read().ready().bit_is_clear() {}
                let channel = if pin >= 40 { pin - 40 } else { pin - 26 } as u8;
                Self { channel }
        }

        /// One blocking conversion: 12 bits, 0..=4095 across the 3.3 V pad supply.
        pub fn read(&mut self) -> u16 {
                let adc = unsafe { &*pac::ADC::ptr() };
                adc.cs().modify(|_, w| unsafe { w.ainsel().bits(self.channel).start_once().set_bit() });
                while adc.cs().read().ready().bit_is_clear() {}
                adc.result().read().result().bits()
        }
}
