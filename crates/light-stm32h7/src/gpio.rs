//! GPIO on the H7: ports A..K on AHB4, each a block of the same registers. A pin is a (port,
//! pin) pair here -- `Pin::new('E', 12)` -- rather than the flat number the RP2 has, and the
//! port's clock is enabled the moment a pin is configured, since an unclocked port reads as
//! zero and ignores writes without any error.

use light_core::{InputPin, OutputPin};

use crate::reg;

const GPIO_BASE: usize = 0x5802_0000;
const PORT_STRIDE: usize = 0x400;
const MODER: usize = 0x00;
const OTYPER: usize = 0x04;
const OSPEEDR: usize = 0x08;
const PUPDR: usize = 0x0C;
const IDR: usize = 0x10;
const BSRR: usize = 0x18;
const AFRL: usize = 0x20;

/// A pin on a port: `Pin::new('E', 12)` is PE12.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pin {
        port: u8,
        pin: u8,
}

impl Pin {
        pub const fn new(port: char, pin: u8) -> Self {
                Self { port: (port as u8) - b'A', pin }
        }

        fn base(self) -> usize {
                GPIO_BASE + usize::from(self.port) * PORT_STRIDE
        }

        fn clock_enable(self) {
                reg::modify(crate::RCC_AHB4ENR, 0, 1 << self.port);
                let _ = reg::read(crate::RCC_AHB4ENR);
        }

        /// Mode bits: 0 input, 1 output, 2 alternate function, 3 analog.
        fn set_mode(self, mode: u32) {
                let shift = u32::from(self.pin) * 2;
                reg::modify(self.base() + MODER, 3 << shift, mode << shift);
        }

        /// Push-pull, high speed, no pull: the configuration every bus line here wants.
        fn set_fast_push_pull(self) {
                let shift = u32::from(self.pin) * 2;
                reg::modify(self.base() + OTYPER, 1 << self.pin, 0);
                reg::modify(self.base() + OSPEEDR, 3 << shift, 3 << shift);
                reg::modify(self.base() + PUPDR, 3 << shift, 0);
        }

        /// Route the pin to alternate function `af`.
        pub fn set_alternate(self, af: u32) {
                self.clock_enable();
                self.set_fast_push_pull();
                let afr = self.base() + AFRL + usize::from(self.pin >> 3) * 4;
                let shift = u32::from(self.pin & 7) * 4;
                reg::modify(afr, 0xF << shift, af << shift);
                self.set_mode(2);
        }
}

/// A push-pull output.
pub struct Output {
        pin: Pin,
}

impl Output {
        /// Driven to `initial` BEFORE it becomes an output, so a chip select never glitches.
        pub fn new(pin: Pin, initial: bool) -> Self {
                pin.clock_enable();
                let mut out = Self { pin };
                out.set(initial);
                pin.set_fast_push_pull();
                pin.set_mode(1);
                out
        }

        pub fn set(&mut self, high: bool) {
                // BSRR: one write, no read-modify-write for an interrupt to land inside
                let bit = 1u32 << self.pin.pin;
                reg::write(self.pin.base() + BSRR, if high { bit } else { bit << 16 });
        }
}

impl OutputPin for Output {
        fn set(&mut self, high: bool) {
                Output::set(self, high)
        }
}

/// An input with the internal pull-up.
pub struct Input {
        pin: Pin,
}

impl Input {
        pub fn new_pull_up(pin: Pin) -> Self {
                pin.clock_enable();
                let shift = u32::from(pin.pin) * 2;
                reg::modify(pin.base() + PUPDR, 3 << shift, 1 << shift);
                pin.set_mode(0);
                Self { pin }
        }
}

impl InputPin for Input {
        fn is_low(&self) -> bool {
                reg::read(self.pin.base() + IDR) & (1 << self.pin.pin) == 0
        }
}
