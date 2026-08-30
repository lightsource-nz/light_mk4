//! GPIO through the pac: function select, pads, SIO input/output.

use light_core::{InputPin, OutputPin};
use crate::pac;

/// GPIO function selects, the same numbers on both chips (RP2040 datasheet table 2.19.2,
/// RP2350 table 9.4.1). Only the ones in use.
pub const FUNC_SPI: u8 = 1;
pub const FUNC_I2C: u8 = 3;
pub const FUNC_SIO: u8 = 5;

/// Route `pin` to `func`, with the pad configured the way pico-sdk's `gpio_set_function` does:
/// input enabled, output not disabled, and -- RP2350 only -- isolation cleared, since its pads
/// power up ISOLATED and everything else looks correct while the pin does nothing. The RP2040
/// has no such bit.
pub fn set_function(pin: usize, func: u8) {
        let pads = unsafe { &*pac::PADS_BANK0::ptr() };
        let io = unsafe { &*pac::IO_BANK0::ptr() };
        pads.gpio(pin).modify(|_, w| w.ie().set_bit().od().clear_bit());
        io.gpio(pin).gpio_ctrl().write(|w| unsafe { w.funcsel().bits(func) });
        #[cfg(feature = "rp2350")]
        pads.gpio(pin).modify(|_, w| w.iso().clear_bit());
}

pub fn set_pull_up(pin: usize) {
        let pads = unsafe { &*pac::PADS_BANK0::ptr() };
        pads.gpio(pin).modify(|_, w| w.pue().set_bit().pde().clear_bit());
}

/// A push-pull output on SIO.
pub struct Output {
        pin: usize,
}

impl Output {
        /// Configured and driven to `initial` BEFORE it becomes an output, so a chip-select
        /// never glitches low on its way up.
        pub fn new(pin: usize, initial: bool) -> Self {
                let mut out = Self { pin };
                set_function(pin, FUNC_SIO);
                out.set(initial);
                let sio = unsafe { &*pac::SIO::ptr() };
                sio.gpio_oe_set().write(|w| unsafe { w.bits(1 << pin) });
                out
        }

        pub fn set(&mut self, high: bool) {
                let sio = unsafe { &*pac::SIO::ptr() };
                let mask = 1u32 << self.pin;
                if high {
                        sio.gpio_out_set().write(|w| unsafe { w.bits(mask) });
                } else {
                        sio.gpio_out_clr().write(|w| unsafe { w.bits(mask) });
                }
        }
}

impl OutputPin for Output {
        fn set(&mut self, high: bool) {
                Output::set(self, high)
        }
}

impl Output {
        pub fn pin(&self) -> usize {
                self.pin
        }
}

/// An input with the internal pull-up.
pub struct Input {
        pin: usize,
}

impl Input {
        pub fn new_pull_up(pin: usize) -> Self {
                set_function(pin, FUNC_SIO);
                set_pull_up(pin);
                let sio = unsafe { &*pac::SIO::ptr() };
                sio.gpio_oe_clr().write(|w| unsafe { w.bits(1 << pin) });
                Self { pin }
        }
}

impl InputPin for Input {
        fn is_low(&self) -> bool {
                let sio = unsafe { &*pac::SIO::ptr() };
                sio.gpio_in().read().bits() & (1 << self.pin) == 0
        }
}
