//! SPI1 as a 4-wire display bus, with DMA-backed asynchronous data bursts.
//!
//! Register-level port of pico-sdk's `hardware_spi` init and blocking write, and of mk3's
//! `light_ioport` RP2 backend for the DMA burst and its completion rule.

use light_core::{Clock, SpiDisplayBus};
use rp235x_pac as pac;

use crate::gpio::{self, Output};

/// clk_peri as pico-sdk's runtime configures it on RP2350: undivided clk_sys, 150 MHz.
const CLK_PERI_HZ: u32 = 150_000_000;

pub struct Spi1Display {
        cs: Output,
        dc: Output,
        reset: Option<Output>,
        /// A DMA channel this bus owns for its lifetime. Taken from the top of the range, where
        /// pico-sdk's `dma_claim_unused_channel` (counting up from 0) will not reach in a shell
        /// that uses none -- a convention, not a claim the SDK can see.
        dma_ch: usize,
        pub actual_hz: u32,
}

impl Spi1Display {
        /// # Safety
        ///
        /// Takes SPI1 and DMA channel `dma_ch`, which nothing else -- Rust or the C shell --
        /// may use while this lives. Construct once.
        pub unsafe fn new(sck: usize, mosi: usize, cs: usize, dc: usize, reset: Option<usize>, hz: u32, dma_ch: usize) -> Self {
                gpio::set_function(sck, gpio::FUNC_SPI);
                gpio::set_function(mosi, gpio::FUNC_SPI);
                let cs = Output::new(cs, true);
                let dc = Output::new(dc, true);
                let reset = reset.map(|p| Output::new(p, true));

                let resets = unsafe { &*pac::RESETS::ptr() };
                resets.reset().modify(|_, w| w.spi1().set_bit());
                resets.reset().modify(|_, w| w.spi1().clear_bit());
                while resets.reset_done().read().spi1().bit_is_clear() {}

                let mut bus = Self { cs, dc, reset, dma_ch, actual_hz: 0 };
                bus.set_baudrate(hz);
                let spi = unsafe { &*pac::SPI1::ptr() };
                // 8-bit, Motorola frame format, mode 0, MSB first (the only order the PL022 has)
                spi.sspcr0().modify(|_, w| unsafe { w.dss().bits(7).frf().motorola().spo().clear_bit().sph().clear_bit() });
                // DREQs always on: harmless if DMA is not listening
                spi.sspdmacr().write(|w| w.txdmae().set_bit().rxdmae().set_bit());
                spi.sspcr1().modify(|_, w| w.sse().set_bit());
                bus
        }

        /// pico-sdk's divider search: the smallest even prescale that brings the rate into the
        /// post-divider's range, then the largest post-divide at or under the request. The
        /// achieved rate is generally not the requested one; `actual_hz` says what it is.
        pub fn set_baudrate(&mut self, hz: u32) -> u32 {
                let spi = unsafe { &*pac::SPI1::ptr() };
                let mut prescale = 2u32;
                while prescale <= 254 {
                        if (CLK_PERI_HZ as u64) < prescale as u64 * 256 * hz as u64 {
                                break;
                        }
                        prescale += 2;
                }
                let mut postdiv = 256u32;
                while postdiv > 1 {
                        if CLK_PERI_HZ / (prescale * (postdiv - 1)) > hz {
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
                self.actual_hz = CLK_PERI_HZ / (prescale * postdiv);
                self.actual_hz
        }

        fn write_blocking(&mut self, bytes: &[u8]) {
                let spi = unsafe { &*pac::SPI1::ptr() };
                for &b in bytes {
                        while spi.sspsr().read().tnf().bit_is_clear() {}
                        spi.sspdr().write(|w| unsafe { w.bits(u32::from(b)) });
                }
                // drain RX, wait for the shift register, drain again, clear the overrun flag
                while spi.sspsr().read().rne().bit_is_set() {
                        let _ = spi.sspdr().read();
                }
                while spi.sspsr().read().bsy().bit_is_set() {}
                while spi.sspsr().read().rne().bit_is_set() {
                        let _ = spi.sspdr().read();
                }
                spi.sspicr().write(|w| w.roric().clear_bit_by_one());
        }
}

impl SpiDisplayBus for Spi1Display {
        fn command(&mut self, cmd: u8) {
                self.dc.set(false);
                self.cs.set(false);
                self.write_blocking(&[cmd]);
                self.cs.set(true);
        }

        fn data(&mut self, bytes: &[u8]) {
                self.dc.set(true);
                self.cs.set(false);
                self.write_blocking(bytes);
                self.cs.set(true);
        }

        unsafe fn start_data(&mut self, bytes: &[u8]) {
                self.dc.set(true);
                self.cs.set(false);
                let spi = unsafe { &*pac::SPI1::ptr() };
                let dma = unsafe { &*pac::DMA::ptr() };
                let ch = dma.ch(self.dma_ch);
                ch.ch_read_addr().write(|w| unsafe { w.bits(bytes.as_ptr() as u32) });
                ch.ch_write_addr().write(|w| unsafe { w.bits(spi.sspdr().as_ptr() as u32) });
                ch.ch_trans_count().write(|w| unsafe { w.bits(bytes.len() as u32) });
                // byte transfers, paced by SPI1's TX DREQ, reading forward, writing the one
                // FIFO address; chained to itself, which means no chain. writing CTRL_TRIG
                // is what starts it
                ch.ch_ctrl_trig().write(|w| unsafe {
                        w.data_size()
                                .size_byte()
                                .incr_read()
                                .set_bit()
                                .incr_write()
                                .clear_bit()
                                .treq_sel()
                                .spi1_tx()
                                .chain_to()
                                .bits(self.dma_ch as u8)
                                .en()
                                .set_bit()
                });
        }

        fn is_complete(&mut self) -> bool {
                // DMA done only means the FIFO has been fed; the shift register can still be
                // clocking out the last byte or two, and raising CS before it empties corrupts
                // the tail of the transfer. both must be idle
                let spi = unsafe { &*pac::SPI1::ptr() };
                let dma = unsafe { &*pac::DMA::ptr() };
                if dma.ch(self.dma_ch).ch_ctrl_trig().read().busy().bit_is_set() || spi.sspsr().read().bsy().bit_is_set() {
                        return false;
                }
                self.cs.set(true);
                true
        }

        fn reset_pulse(&mut self, clock: &mut dyn Clock) {
                if let Some(reset) = &mut self.reset {
                        reset.set(true);
                        clock.delay_ms(100);
                        reset.set(false);
                        clock.delay_ms(100);
                        reset.set(true);
                        clock.delay_ms(100);
                }
        }
}
