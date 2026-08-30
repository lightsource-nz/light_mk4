//! Board wiring for the WeAct Blackpill (STM32F411CEU6): a LED on PC13 and a key on PA0, both
//! active low -- from mk3's `light_board.h`. mk3 ran its console demo here and read neither,
//! so the key's sense is the header's word until the bench says otherwise (the H7's turned
//! out inverted).
//!
//! This lives with the APPLICATION, not in the port crate: `light-stm32f4` knows the chip and
//! nothing about which board it sits on.

use light_core::atomic::{AtomicBool, Ordering};
use light_stm32f4::gpio::{Input, Output, Pin};
use light_stm32f4::Clocks;

pub const PIN_LED: Pin = Pin::new('C', 13);
pub const PIN_KEY: Pin = Pin::new('A', 0);

pub struct Peripherals {
        /// Low is ON.
        pub led: Output,
        pub key: Input,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// Configure and hand over the board's peripherals. Once.
pub fn take(_clocks: &Clocks) -> Option<Peripherals> {
        if TAKEN.swap(true, Ordering::AcqRel) {
                return None;
        }
        Some(Peripherals { led: Output::new(PIN_LED, true), key: Input::new_pull_up(PIN_KEY) })
}
