//! GPIO on the F4: ports A..H on AHB1, the same register block as the H7's at a different base.

use light_core::{InputPin, OutputPin};

use crate::reg;

const GPIO_BASE: usize = 0x4002_0000;
const PORT_STRIDE: usize = 0x400;
const MODER: usize = 0x00;
const OTYPER: usize = 0x04;
const OSPEEDR: usize = 0x08;
const PUPDR: usize = 0x0C;
const IDR: usize = 0x10;
const BSRR: usize = 0x18;

/// A pin on a port: `Pin::new('C', 13)` is PC13.
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
                reg::modify(crate::RCC_AHB1ENR, 0, 1 << self.port);
                let _ = reg::read(crate::RCC_AHB1ENR);
        }

        fn set_mode(self, mode: u32) {
                let shift = u32::from(self.pin) * 2;
                reg::modify(self.base() + MODER, 3 << shift, mode << shift);
        }

        fn set_fast_push_pull(self) {
                let shift = u32::from(self.pin) * 2;
                reg::modify(self.base() + OTYPER, 1 << self.pin, 0);
                reg::modify(self.base() + OSPEEDR, 3 << shift, 3 << shift);
                reg::modify(self.base() + PUPDR, 3 << shift, 0);
        }
}

pub struct Output {
        pin: Pin,
}

impl Output {
        pub fn new(pin: Pin, initial: bool) -> Self {
                pin.clock_enable();
                let mut out = Self { pin };
                out.set(initial);
                pin.set_fast_push_pull();
                pin.set_mode(1);
                out
        }

        pub fn set(&mut self, high: bool) {
                let bit = 1u32 << self.pin.pin;
                reg::write(self.pin.base() + BSRR, if high { bit } else { bit << 16 });
        }
}

impl OutputPin for Output {
        fn set(&mut self, high: bool) {
                Output::set(self, high)
        }
}

pub struct Input {
        pin: Pin,
}

impl Input {
        pub fn new_pull_up(pin: Pin) -> Self {
                Self::with_pull(pin, 1)
        }

        pub fn new_pull_down(pin: Pin) -> Self {
                Self::with_pull(pin, 2)
        }

        fn with_pull(pin: Pin, pupd: u32) -> Self {
                pin.clock_enable();
                let shift = u32::from(pin.pin) * 2;
                reg::modify(pin.base() + PUPDR, 3 << shift, pupd << shift);
                pin.set_mode(0);
                Self { pin }
        }
}

impl InputPin for Input {
        fn is_low(&self) -> bool {
                reg::read(self.pin.base() + IDR) & (1 << self.pin.pin) == 0
        }
}
