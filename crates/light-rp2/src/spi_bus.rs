//! SPI1 as a plain full-duplex master behind [`light_core::hal::SpiBus`]: blocking
//! byte exchange, caller-owned chip select. The TF slot is the first consumer.
//!
//! Distinct from [`crate::spi::Spi1Display`], which owns the same block for the 4-wire
//! display framing -- a board uses one or the other on SPI1, never both.

use light_core::hal::SpiBus;
use crate::gpio;
use crate::pac;

pub struct Spi1Bus {
        clk_peri_hz: u32,
        pub actual_hz: u32,
}

impl Spi1Bus {
        /// # Safety
        ///
        /// Takes SPI1, which nothing else -- Rust or the C shell -- may use while this
        /// lives. Construct once.
        pub unsafe fn new(clk_peri_hz: u32, sck: usize, mosi: usize, miso: usize, hz: u32) -> Self {
                gpio::set_function(sck, gpio::FUNC_SPI);
                gpio::set_function(mosi, gpio::FUNC_SPI);
                gpio::set_function(miso, gpio::FUNC_SPI);
                //   MISO wants a pull-up: an SD card answers open bus time with 0xFF, and a
                // floating line reads garbage during the response-search window
                gpio::set_pull_up(miso);

                let resets = unsafe { &*pac::RESETS::ptr() };
                resets.reset().modify(|_, w| w.spi1().set_bit());
                resets.reset().modify(|_, w| w.spi1().clear_bit());
                while resets.reset_done().read().spi1().bit_is_clear() {}

                let mut bus = Self { clk_peri_hz, actual_hz: 0 };
                bus.set_rate(hz);
                let spi = unsafe { &*pac::SPI1::ptr() };
                // 8-bit, Motorola frame format, mode 0, MSB first
                spi.sspcr0().modify(|_, w| unsafe { w.dss().bits(7).frf().motorola().spo().clear_bit().sph().clear_bit() });
                spi.sspcr1().modify(|_, w| w.sse().set_bit());
                bus
        }

        /// pico-sdk's divider search, as in the display bus: smallest even prescale into
        /// range, then the largest post-divide at or under the request.
        fn set_rate(&mut self, hz: u32) {
                let spi = unsafe { &*pac::SPI1::ptr() };
                let freq_in = self.clk_peri_hz;
                let mut prescale = 2u32;
                while prescale <= 254 {
                        if (freq_in as u64) < prescale as u64 * 256 * hz as u64 {
                                break;
                        }
                        prescale += 2;
                }
                let mut postdiv = 256u32;
                while postdiv > 1 {
                        if freq_in / (prescale * (postdiv - 1)) > hz {
                                break;
                        }
                        postdiv -= 1;
                }
                let was_enabled = spi.sspcr1().read().sse().bit_is_set();
                spi.sspcr1().modify(|_, w| w.sse().clear_bit());
                spi.sspcpsr().write(|w| unsafe { w.bits(prescale) });
                spi.sspcr0().modify(|_, w| unsafe { w.scr().bits((postdiv - 1) as u8) });
                if was_enabled {
                        spi.sspcr1().modify(|_, w| w.sse().set_bit());
                }
                self.actual_hz = freq_in / (prescale * postdiv);
        }
}

impl SpiBus for Spi1Bus {
        fn transfer(&mut self, tx: u8) -> u8 {
                let spi = unsafe { &*pac::SPI1::ptr() };
                while spi.sspsr().read().tnf().bit_is_clear() {
                        core::hint::spin_loop();
                }
                spi.sspdr().write(|w| unsafe { w.data().bits(u16::from(tx)) });
                while spi.sspsr().read().rne().bit_is_clear() {
                        core::hint::spin_loop();
                }
                spi.sspdr().read().data().bits() as u8
        }

        fn set_hz(&mut self, hz: u32) {
                self.set_rate(hz);
        }
}
