//! RP2350 access for the light framework, mk4 spike.
//!
//! Peripherals are driven through `rp235x-pac` register definitions rather than through pico-sdk
//! calls, on purpose: pico-sdk's peripheral API is mostly `static inline` in headers, which no
//! binding generator can export, so every SDK call from Rust would need a hand-written C shim.
//! The spike measures how far the pac gets on its own; pico-sdk stays in charge of the RUNTIME
//! (crt0, boot2, clocks, timer start, multicore, USB) in the C shell that links this.

#![no_std]

use light_core::Board;
use rp235x_pac as pac;

/// GPIO function select for the single-cycle IO block, the same value on RP2040 and RP2350.
const FUNCSEL_SIO: u8 = 5;

/// The Waveshare RP2350-Touch-LCD-1.69, as far as milestone 1 needs it.
pub struct Touch169 {
        p: pac::Peripherals,
}

impl Touch169 {
        /// Backlight enable, active high -- pin 25 per the board schematic (mk3's
        /// `ST_DISPLAY_PIN_BL`).
        pub const PIN_BACKLIGHT: usize = 25;

        /// # Safety
        ///
        /// Takes the pac's peripheral singleton by `steal`, because ownership of the hardware is
        /// shared with the pico-sdk runtime in the C shell and the pac's `take()` cannot know
        /// that. The caller must construct this exactly once and must not use the same
        /// peripherals from C while it lives.
        pub unsafe fn new() -> Self {
                let p = unsafe { pac::Peripherals::steal() };
                let mut board = Self { p };
                board.init_output(Self::PIN_BACKLIGHT);
                board
        }

        fn init_output(&mut self, pin: usize) {
                //   pads first: RP2350 pads power up ISOLATED (ISO set), which RP2040's do not;
                // leaving it set makes every later step look correct while the pin does nothing
                self.p.PADS_BANK0.gpio(pin).modify(|_, w| {
                        w.iso().clear_bit().od().clear_bit().ie().set_bit()
                });
                self.p.IO_BANK0.gpio(pin).gpio_ctrl().write(|w| unsafe { w.funcsel().bits(FUNCSEL_SIO) });
                self.p.SIO.gpio_oe_set().write(|w| unsafe { w.bits(1 << pin) });
        }

        fn write(&mut self, pin: usize, high: bool) {
                let mask = 1u32 << pin;
                if high {
                        self.p.SIO.gpio_out_set().write(|w| unsafe { w.bits(mask) });
                } else {
                        self.p.SIO.gpio_out_clr().write(|w| unsafe { w.bits(mask) });
                }
        }
}

impl Board for Touch169 {
        fn set_backlight(&mut self, on: bool) {
                self.write(Self::PIN_BACKLIGHT, on);
        }

        fn now_us(&self) -> u64 {
                //   the 64-bit timer is read as two halves; re-read until the high half is
                // stable across the low read, the same dance pico-sdk's time_us_64() does
                // without the latching TIMELR/TIMEHR pair, which would be per-core state
                loop {
                        let hi = self.p.TIMER0.timerawh().read().bits();
                        let lo = self.p.TIMER0.timerawl().read().bits();
                        if self.p.TIMER0.timerawh().read().bits() == hi {
                                return ((hi as u64) << 32) | lo as u64;
                        }
                }
        }
}
