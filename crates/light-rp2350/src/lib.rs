//! RP2350 port of the light framework.
//!
//! Peripherals are driven through `rp235x-pac` register definitions rather than through pico-sdk
//! calls: pico-sdk's peripheral API is mostly `static inline` in headers, which no binding
//! generator can export, so every SDK call from Rust would need a hand-written C shim. The spike
//! found the pac reaches everything the framework needs. pico-sdk stays in charge of the RUNTIME
//! (crt0, boot2, clocks, timer start, multicore, USB) in the C shell that links this.
//!
//! Ownership: a board's peripherals are taken ONCE, as an owned set, from `boards::<board>::take`.
//! A second call answers `None`. The shell owns nothing the set contains; the set owns nothing
//! the shell uses. That replaces the spike's `steal()`-with-a-doc-comment.

#![no_std]

//   target-only: the implementation reads PRIMASK and SIO registers, neither of which exists
// where `cargo test` runs this crate's (empty) test harness
#[cfg(target_os = "none")]
mod critical;

pub mod boards;
pub mod gpio;
pub mod i2c;
pub mod pwm;
pub mod spi;
#[cfg(feature = "usb-host")]
pub mod tinyusb_midi;

use light_core::{Clock, Idle};
use rp235x_pac as pac;

/// The clock frequencies the runtime configured, passed in by the shell that knows them rather
/// than assumed here. pico-sdk's defaults for RP2350 are 150 MHz for both.
#[derive(Clone, Copy, Debug)]
pub struct Clocks {
        pub sys_hz: u32,
        pub peri_hz: u32,
}

/// Microseconds since boot from the 64-bit TIMER0, which pico-sdk's runtime has already started.
///
/// Read as two halves, re-read until the high half is stable across the low read -- the same
/// dance pico-sdk's `time_us_64()` does, without the latching TIMELR/TIMEHR pair, which is
/// per-core state and would race the other core's use of it.
pub fn now_us() -> u64 {
        let timer = unsafe { &*pac::TIMER0::ptr() };
        loop {
                let hi = timer.timerawh().read().bits();
                let lo = timer.timerawl().read().bits();
                if timer.timerawh().read().bits() == hi {
                        return ((hi as u64) << 32) | lo as u64;
                }
        }
}

/// The system timer as a [`Clock`].
#[derive(Clone, Copy, Default)]
pub struct SysClock;

impl Clock for SysClock {
        fn now_us(&self) -> u64 {
                now_us()
        }
}

/// What the runtime does between idle passes on this chip: nothing that could miss a deadline.
/// The poll-driven drivers keep their own cadences and no interrupt is guaranteed to wake a
/// `wfe`, so this is a breath, not a sleep. A real low-power idle needs a tick to wake on.
#[derive(Clone, Copy, Default)]
pub struct Breathe;

impl Idle for Breathe {
        fn idle(&mut self) {
                core::hint::spin_loop();
        }
}
