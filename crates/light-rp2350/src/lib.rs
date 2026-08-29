//! RP2350 access for the light framework, mk4 spike.
//!
//! Peripherals are driven through `rp235x-pac` register definitions rather than through pico-sdk
//! calls, on purpose: pico-sdk's peripheral API is mostly `static inline` in headers, which no
//! binding generator can export, so every SDK call from Rust would need a hand-written C shim.
//! The spike measures how far the pac gets on its own; pico-sdk stays in charge of the RUNTIME
//! (crt0, boot2, clocks, timer start, multicore, USB) in the C shell that links this.
//!
//! Ownership of peripherals is by convention, not by the pac's `take()`: the C shell owns the
//! runtime blocks and this crate owns what it constructs, and each constructor says so.

#![no_std]

//   target-only: the implementation reads PRIMASK and SIO registers, neither of which exists
// where `cargo test` runs this crate's (empty) test harness
#[cfg(target_os = "none")]
mod critical;

pub mod gpio;
pub mod i2c;
pub mod spi;

use light_core::{Board, Clock};
use rp235x_pac as pac;

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

/// The system timer as a [`Clock`], for init sequences.
pub struct SysClock;

impl Clock for SysClock {
        fn now_us(&self) -> u64 {
                now_us()
        }
}

/// The Waveshare RP2350-Touch-LCD-1.69: pins from the board schematic, as recorded in mk3's
/// `light_ui_hw_ws_touch169.h`.
pub mod touch169 {
        pub const PIN_DISPLAY_DC: usize = 8;
        pub const PIN_DISPLAY_CS: usize = 9;
        pub const PIN_DISPLAY_SCK: usize = 10;
        pub const PIN_DISPLAY_MOSI: usize = 11;
        pub const PIN_DISPLAY_RESET: usize = 13;
        pub const PIN_DISPLAY_BL: usize = 25;
        pub const DISPLAY_WIDTH: u16 = 240;
        pub const DISPLAY_HEIGHT: u16 = 280;
        /// The visible glass is GDDRAM rows 20..299 -- measured (mk3 board wiring).
        pub const DISPLAY_ROW_OFFSET: u16 = 20;
        /// 40 MHz confirmed clean on hardware; 10 MHz would cap a full frame at 9.3 fps.
        ///
        /// Tried at 10 MHz on 2026-08-29 to test whether the touch controller's wedges (I2C on
        /// pins 6/7 timing out under continuous rendering) track the SPI clock on 10/11: 17
        /// clean taps then a wedge, against wedges every 4-8 taps at 40 MHz. Suggestive, not
        /// decisive -- one run each. Left at 40 MHz, the clock mk3 verified the panel at.
        pub const DISPLAY_SPI_HZ: u32 = 40_000_000;

        pub const PIN_TOUCH_SDA: usize = 6;
        pub const PIN_TOUCH_SCL: usize = 7;
        pub const PIN_TOUCH_INT: usize = 21;
        pub const PIN_TOUCH_RST: usize = 22;
        pub const TOUCH_I2C_HZ: u32 = 300_000;

        /// DMA channel for the display bus: see `Spi1Display` for why the top of the range.
        pub const DISPLAY_DMA_CH: usize = 15;
}

/// The backlight line, the first thing the spike drove.
pub struct Backlight {
        pin: gpio::Output,
}

impl Backlight {
        /// # Safety
        ///
        /// Takes the backlight pin; construct once.
        pub unsafe fn new() -> Self {
                Self { pin: gpio::Output::new(touch169::PIN_DISPLAY_BL, false) }
        }
}

impl Board for Backlight {
        fn set_backlight(&mut self, on: bool) {
                self.pin.set(on);
        }

        fn now_us(&self) -> u64 {
                now_us()
        }
}
