//! A continuous RGB (DPI) scanout engine on PIO, for panels with no GDDRAM -- the ST7701S
//! boards: the chip must stream every pixel of every frame forever, and this module makes
//! that a pure-hardware loop the CPU never services after start.
//!
//! Four state machines, two PIO blocks, ported from Waveshare's reference programs:
//!
//! - `hsync` (PIO1): free-runs at 2x PCLK, side-setting HSYNC+PCLK through the porches and
//!   the active window, raising IRQ 2 once per line;
//! - `vsync` (PIO1): counts lines off that IRQ through the vertical porches, driving VSYNC;
//! - `rgb_de` (PIO2): WATCHES vsync/hsync/pclk as input pins (IRQ flags do not cross PIO
//!   blocks) and raises DE for each active line, handshaking the data machine via IRQ 0;
//! - `rgb` (PIO2): one `pull` per pixel, `out pins, 16` on the PCLK edges while DE is high.
//!
//! The blocks run with GPIOBASE = 16 (an RP2350B feature): every pin index below is the
//! GPIO minus 16, which is how the sync pins in the 20s and data pins up to 39 all fit one
//! block's 32-pin window.
//!
//! FEEDING is where this differs from the reference, which restarts a chunked DMA channel
//! from an interrupt handler -- late IRQ, sheared frame. Here the data channel streams the
//! whole frame in ONE transfer and chains to a one-word reprogram channel, which copies the
//! framebuffer address from a control word back into the data channel's read-address
//! trigger: a two-channel hardware loop, no interrupts, no deadline for software to miss.
//! A buffer flip is one store into the control word, taking effect at the next frame.

use light_core::atomic::{AtomicU32, Ordering};
use crate::gpio;
use crate::pac;

/// Both PIO blocks' pin window starts here; every mapped index is GPIO - 16.
const GPIO_BASE: usize = 16;

const SM_HSYNC: usize = 0;
const SM_VSYNC: usize = 1;
const SM_DE: usize = 0;
const SM_RGB: usize = 1;

/// DREQ for PIO2's TX FIFOs starts at 16.
const DREQ_PIO2_TX0: u8 = 16;

//   the reference's porch timings (in lines for vsync, PCLK cycles for hsync; each SET
// count runs COUNT+1 iterations, kept verbatim)
const V_FRONT: u8 = 7;
const V_PULSE: u8 = 2;
const V_BACK: u8 = 17;
const H_FRONT: u8 = 9;
const H_PULSE: u8 = 7;
const H_BACK: u8 = 9;

// instruction builders -- op class in [15:13], specifics below
fn wait(pol: u8, src: u8, idx: u8) -> u16 {
        0x2000 | u16::from(pol) << 7 | u16::from(src) << 5 | u16::from(idx)
}
const SRC_PIN: u8 = 1;
const SRC_IRQ: u8 = 2;
fn jmp(cond: u8, addr: u16) -> u16 {
        u16::from(cond) << 5 | addr
}
//   JMP conditions, per the datasheet's table: 000 always, 001 !X, 010 X--, 011 !Y,
// 100 Y--. The 011/100 pair cost this module its first bring-up session: 3 is NOT-Y,
// which turned "next line" into "only when the counter is zero" -- one line per frame
const COND_XDEC: u8 = 2;
const COND_YDEC: u8 = 4;
fn set_x(v: u8) -> u16 {
        0xE020 | u16::from(v)
}
fn irq_set(n: u8) -> u16 {
        0xC000 | u16::from(n)
}
const PULL_BLOCK: u16 = 0x80A0;
const MOV_X_OSR: u16 = 0xA027;
const MOV_Y_OSR: u16 = 0xA047;
const MOV_X_Y: u16 = 0xA022;
const NOP: u16 = 0xA042;
const OUT_PINS_16: u16 = 0x6010;
/// 1-bit optional sideset: enable flag in bit 12, the value in bit 11.
fn side1(instr: u16, side: u16) -> u16 {
        instr | 0x1000 | side << 11
}
/// 2-bit optional sideset: enable in bit 12, the value in bits 11:10.
fn side2(instr: u16, side: u16) -> u16 {
        instr | 0x1000 | side << 10
}

/// The framebuffer address the reprogram channel copies into the data channel each frame.
/// One engine per chip (construct-once), so module state is fine -- and an atomic makes the
/// flip race-free against the DMA read.
static FRAME_ADDR: AtomicU32 = AtomicU32::new(0);

pub struct RgbPins {
        pub de: usize,
        pub vsync: usize,
        pub hsync: usize,
        /// Must be `hsync + 1`: HSYNC and PCLK are one sideset group.
        pub pclk: usize,
        /// First of 16 contiguous RGB565 data pins.
        pub data0: usize,
}

pub struct RgbScanout {
        width: u16,
        height: u16,
        dma_data: usize,
}

impl RgbScanout {
        /// Claims PIO1, PIO2 and DMA channels `dma_data`/`dma_ctrl`, none of which anything
        /// else may use while this lives, and starts scanning `fb` -- `width * height`
        /// RGB565 words, which the caller keeps alive and in place forever (the panel is
        /// reading it, always). Refresh needs no further attention.
        ///
        /// # Safety
        ///
        /// Construct once; see above for what it claims and what `fb` promises.
        pub unsafe fn new(pins: RgbPins, width: u16, height: u16, fb: *const u16, sys_hz: u32, pclk_hz: u32, dma_data: usize, dma_ctrl: usize) -> Self {
                assert!(pins.pclk == pins.hsync + 1, "HSYNC and PCLK are one sideset group");
                assert!(pins.de >= GPIO_BASE && pins.data0 + 15 <= 47, "pins must sit in the GPIOBASE-16 window");
                FRAME_ADDR.store(fb as u32, Ordering::Release);

                let resets = unsafe { &*pac::RESETS::ptr() };
                resets.reset().modify(|_, w| w.pio1().clear_bit().pio2().clear_bit());
                while resets.reset_done().read().pio1().bit_is_clear() || resets.reset_done().read().pio2().bit_is_clear() {}
                let sync = unsafe { &*pac::PIO1::ptr() };
                let data = unsafe { &*pac::PIO2::ptr() };
                sync.gpiobase().write(|w| unsafe { w.bits(GPIO_BASE as u32) });
                data.gpiobase().write(|w| unsafe { w.bits(GPIO_BASE as u32) });

                let de_i = (pins.de - GPIO_BASE) as u8;
                let vs_i = (pins.vsync - GPIO_BASE) as u8;
                let hs_i = (pins.hsync - GPIO_BASE) as u8;
                let pc_i = (pins.pclk - GPIO_BASE) as u8;
                let d0_i = (pins.data0 - GPIO_BASE) as u8;

                //   hsync at origin 0 on PIO1: porches and active window at 2 PIO cycles per
                // PCLK, HSYNC in sideset bit 0 and PCLK in bit 1, IRQ 2 once per line
                let h = 0u16;
                let hsync_prog: [u16; 15] = [
                        PULL_BLOCK,
                        MOV_Y_OSR,
                        side2(set_x(H_FRONT), 0b11),
                        side2(NOP, 0b01),
                        side2(jmp(COND_XDEC, h + 3), 0b11),
                        side2(irq_set(2), 0b01),
                        side2(set_x(H_PULSE), 0b10),
                        side2(NOP, 0b00),
                        side2(jmp(COND_XDEC, h + 7), 0b10),
                        side2(set_x(H_BACK), 0b01),
                        side2(NOP, 0b11),
                        side2(jmp(COND_XDEC, h + 10), 0b01),
                        side2(MOV_X_Y, 0b11),
                        side2(NOP, 0b01),
                        side2(jmp(COND_XDEC, h + 13), 0b11),
                ];
                //   vsync at origin 15 on PIO1: line-counting porches off hsync's IRQ 2
                let v = 15u16;
                let vsync_prog: [u16; 14] = [
                        side1(PULL_BLOCK, 1),
                        side1(set_x(V_FRONT), 1),
                        side1(wait(1, SRC_IRQ, 2), 1),
                        side1(jmp(COND_XDEC, v + 2), 1),
                        side1(set_x(V_PULSE), 0),
                        side1(wait(1, SRC_IRQ, 2), 0),
                        side1(jmp(COND_XDEC, v + 5), 0),
                        side1(set_x(V_BACK), 1),
                        side1(wait(1, SRC_IRQ, 2), 1),
                        side1(jmp(COND_XDEC, v + 8), 1),
                        side1(MOV_X_OSR, 1),
                        side1(irq_set(1), 1),
                        side1(wait(1, SRC_IRQ, 2), 1),
                        side1(jmp(COND_XDEC, v + 12), 1),
                ];
                //   rgb_de at origin 0 on PIO2: DE from watching the sync pins, one IRQ 0
                // handshake with the data machine per line
                let d = 0u16;
                let de_prog: [u16; 17] = [
                        side1(PULL_BLOCK, 0),
                        side1(MOV_Y_OSR, 0),
                        side1(set_x(V_BACK), 0),
                        side1(wait(0, SRC_PIN, vs_i), 0),
                        side1(wait(1, SRC_PIN, vs_i), 0),
                        side1(wait(0, SRC_PIN, hs_i), 0),
                        side1(wait(1, SRC_PIN, hs_i), 0),
                        side1(jmp(COND_XDEC, d + 5), 0),
                        side1(wait(0, SRC_PIN, hs_i), 0),
                        side1(wait(1, SRC_PIN, hs_i), 0),
                        side1(set_x(H_BACK), 0),
                        side1(wait(0, SRC_PIN, pc_i), 0),
                        side1(wait(1, SRC_PIN, pc_i), 0),
                        side1(jmp(COND_XDEC, d + 11), 0),
                        side1(wait(1, SRC_PIN, pc_i), 0),
                        side1(wait(1, SRC_IRQ, 0), 1),
                        side1(jmp(COND_YDEC, d + 8), 0),
                ];
                //   rgb at origin 17 on PIO2: a pull per pixel onto the 16 data pins,
                // changing data just after PCLK's falling edge (the panel latches on the
                // rising edge). One DELIBERATE divergence from the reference: each line
                // waits for a DE EDGE (low, then high), not the level. The reference's
                // level wait races its own handshake -- after `irq set 0` the input
                // synchronizer still shows the DE the partner has not yet dropped, the
                // level wait sails through on that stale high, and one line later the
                // partner's `wait 1 irq 0` finds the flag already set: DE collapses to a
                // runt pulse per line that the panel (sampling DE on PCLK edges) never
                // sees at all. Bring-up caught it as the DE machine parked at its hsync
                // wait while rgb streamed -- a black panel fed by perfect-looking DMA
                let r = 17u16;
                let rgb_prog: [u16; 11] = [
                        PULL_BLOCK,
                        MOV_Y_OSR,
                        MOV_X_Y,
                        wait(0, SRC_PIN, de_i),
                        wait(1, SRC_PIN, de_i),
                        PULL_BLOCK,
                        wait(0, SRC_PIN, pc_i),
                        OUT_PINS_16,
                        wait(1, SRC_PIN, pc_i),
                        jmp(COND_XDEC, r + 5),
                        irq_set(0),
                ];
                for (i, ins) in hsync_prog.iter().enumerate() {
                        sync.instr_mem(usize::from(h) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }
                for (i, ins) in vsync_prog.iter().enumerate() {
                        sync.instr_mem(usize::from(v) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }
                for (i, ins) in de_prog.iter().enumerate() {
                        data.instr_mem(usize::from(d) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }
                for (i, ins) in rgb_prog.iter().enumerate() {
                        data.instr_mem(usize::from(r) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }

                //   hsync: sideset HSYNC+PCLK (2 bits + enable), 2 PIO cycles per PCLK
                let smr = sync.sm(SM_HSYNC);
                smr.sm_pinctrl().write(|w| unsafe { w.sideset_base().bits(hs_i).sideset_count().bits(3) });
                smr.sm_execctrl().write(|w| unsafe { w.side_en().set_bit().wrap_bottom().bits(h as u8 + 2).wrap_top().bits(h as u8 + 14) });
                let div256 = u64::from(sys_hz) * 256 / (u64::from(pclk_hz) * 2);
                smr.sm_clkdiv().write(|w| unsafe { w.int().bits((div256 >> 8) as u16).frac().bits((div256 & 0xFF) as u8) });
                Self::set_pindirs_out(sync, SM_HSYNC, hs_i, 2);
                smr.sm_pinctrl().write(|w| unsafe { w.sideset_base().bits(hs_i).sideset_count().bits(3) });
                smr.sm_instr().write(|w| unsafe { w.bits(u32::from(h)) });

                //   vsync: sideset VSYNC (1 bit + enable), full speed
                let smr = sync.sm(SM_VSYNC);
                smr.sm_pinctrl().write(|w| unsafe { w.sideset_base().bits(vs_i).sideset_count().bits(2) });
                smr.sm_execctrl().write(|w| unsafe { w.side_en().set_bit().wrap_bottom().bits(v as u8 + 1).wrap_top().bits(v as u8 + 13) });
                smr.sm_clkdiv().write(|w| unsafe { w.int().bits(1).frac().bits(0) });
                Self::set_pindirs_out(sync, SM_VSYNC, vs_i, 1);
                smr.sm_pinctrl().write(|w| unsafe { w.sideset_base().bits(vs_i).sideset_count().bits(2) });
                smr.sm_instr().write(|w| unsafe { w.bits(u32::from(v)) });

                //   rgb_de: sideset DE (1 bit + enable), full speed
                let smr = data.sm(SM_DE);
                smr.sm_pinctrl().write(|w| unsafe { w.sideset_base().bits(de_i).sideset_count().bits(2) });
                smr.sm_execctrl().write(|w| unsafe { w.side_en().set_bit().wrap_bottom().bits(d as u8 + 1).wrap_top().bits(d as u8 + 16) });
                smr.sm_clkdiv().write(|w| unsafe { w.int().bits(1).frac().bits(0) });
                Self::set_pindirs_out(data, SM_DE, de_i, 1);
                smr.sm_pinctrl().write(|w| unsafe { w.sideset_base().bits(de_i).sideset_count().bits(2) });
                smr.sm_instr().write(|w| unsafe { w.bits(u32::from(d)) });

                //   rgb: OUT over the 16 data pins, full speed, paced by the PCLK waits.
                // TX FIFO joined to 8 words: at 16 Mpix/s a pixel is ~9 system cycles, and
                // the deeper FIFO is the margin against DMA arbitration latency
                let smr = data.sm(SM_RGB);
                smr.sm_pinctrl().write(|w| unsafe { w.out_base().bits(d0_i).out_count().bits(16) });
                smr.sm_execctrl().write(|w| unsafe { w.wrap_bottom().bits(r as u8 + 2).wrap_top().bits(r as u8 + 10) });
                smr.sm_shiftctrl().write(|w| w.fjoin_tx().set_bit().out_shiftdir().set_bit().in_shiftdir().set_bit());
                smr.sm_clkdiv().write(|w| unsafe { w.int().bits(1).frac().bits(0) });
                Self::set_pindirs_out(data, SM_RGB, d0_i, 16);
                smr.sm_pinctrl().write(|w| unsafe { w.out_base().bits(d0_i).out_count().bits(16) });
                smr.sm_instr().write(|w| unsafe { w.bits(u32::from(r)) });

                for pin in [pins.vsync, pins.hsync, pins.pclk] {
                        gpio::set_function(pin, gpio::FUNC_PIO1);
                }
                gpio::set_function(pins.de, gpio::FUNC_PIO2);
                for i in 0..16 {
                        gpio::set_function(pins.data0 + i, gpio::FUNC_PIO2);
                }

                //   the two-channel hardware refresh loop: DATA streams the frame and chains
                // to CTRL, which copies FRAME_ADDR into DATA's read-address TRIGGER register
                let dma = unsafe { &*pac::DMA::ptr() };
                let ctrl = dma.ch(dma_ctrl);
                ctrl.ch_read_addr().write(|w| unsafe { w.bits(FRAME_ADDR.as_ptr() as u32) });
                ctrl.ch_write_addr().write(|w| unsafe { w.bits(dma.ch(dma_data).ch_al3_read_addr_trig().as_ptr() as u32) });
                ctrl.ch_trans_count().write(|w| unsafe { w.bits(1) });
                ctrl.ch_al1_ctrl().write(|w| unsafe {
                        w.data_size()
                                .size_word()
                                .incr_read()
                                .clear_bit()
                                .incr_write()
                                .clear_bit()
                                .treq_sel()
                                .bits(0x3F)
                                .chain_to()
                                .bits(dma_ctrl as u8)
                                .en()
                                .set_bit()
                });
                let datac = dma.ch(dma_data);
                datac.ch_read_addr().write(|w| unsafe { w.bits(fb as u32) });
                datac.ch_write_addr().write(|w| unsafe { w.bits(data.txf(SM_RGB).as_ptr() as u32) });
                datac.ch_trans_count().write(|w| unsafe { w.bits(u32::from(width) * u32::from(height)) });
                datac.ch_al1_ctrl().write(|w| unsafe {
                        w.data_size()
                                .size_halfword()
                                .incr_read()
                                .set_bit()
                                .incr_write()
                                .clear_bit()
                                .treq_sel()
                                .bits(DREQ_PIO2_TX0 + SM_RGB as u8)
                                .chain_to()
                                .bits(dma_ctrl as u8)
                                .en()
                                .set_bit()
                });

                //   config words the programs pull on start
                sync.txf(SM_HSYNC).write(|w| unsafe { w.bits(u32::from(width) - 1) });
                sync.txf(SM_VSYNC).write(|w| unsafe { w.bits(u32::from(height) - 1) });
                data.txf(SM_DE).write(|w| unsafe { w.bits(u32::from(height) - 1) });
                data.txf(SM_RGB).write(|w| unsafe { w.bits(u32::from(width) - 1) });

                //   feed first, then the data block (parks waiting on DE), then the sync
                // block; rgb_de aligns itself to the next VSYNC edge, so start order only
                // costs at most one frame
                dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << dma_data) });
                data.ctrl().modify(|_, w| unsafe { w.sm_enable().bits((1 << SM_DE) | (1 << SM_RGB)) });
                sync.ctrl().modify(|_, w| unsafe { w.sm_enable().bits((1 << SM_HSYNC) | (1 << SM_VSYNC)) });

                Self { width, height, dma_data }
        }

        /// SET PINDIRS over `count` pins from `base` (window-relative), in the 5-pin rounds
        /// the SET group allows.
        fn set_pindirs_out(pio: &pac::pio0::RegisterBlock, sm: usize, base: u8, count: u8) {
                let smr = pio.sm(sm);
                let mut done = 0u8;
                while done < count {
                        let n = (count - done).min(5);
                        smr.sm_pinctrl().write(|w| unsafe { w.set_base().bits(base + done).set_count().bits(n) });
                        // SET PINDIRS, 0b11111 masked to n bits
                        smr.sm_instr().write(|w| unsafe { w.bits(u32::from(0xE080u16 | ((1u16 << n) - 1))) });
                        done += n;
                }
        }

        /// Point the NEXT frame at a different buffer -- the flip for a future double
        /// buffer. Takes effect at the frame boundary; the current frame finishes from the
        /// old address.
        pub fn set_framebuffer(&mut self, fb: *const u16) {
                FRAME_ADDR.store(fb as u32, Ordering::Release);
        }

        /// How many transfers remain in the CURRENT frame's DMA pass -- counts down from
        /// `width * height` and reloads per frame. Two samples a known time apart measure
        /// the real pixel consumption rate, which is the scanout's pulse.
        pub fn frame_progress(&self) -> u32 {
                let dma = unsafe { &*pac::DMA::ptr() };
                dma.ch(self.dma_data).ch_trans_count().read().bits() & 0x0FFF_FFFF
        }

        /// Where the data DMA is READING right now, what the control word says the frame
        /// starts at, and what the reprogram channel reads from -- the live view of the
        /// refresh loop's addressing.
        pub fn dma_view(&self) -> [u32; 3] {
                let dma = unsafe { &*pac::DMA::ptr() };
                [
                        dma.ch(self.dma_data).ch_read_addr().read().bits(),
                        FRAME_ADDR.load(Ordering::Acquire),
                        FRAME_ADDR.as_ptr() as u32,
                ]
        }

        /// The four state machines' program counters -- hsync, vsync, rgb_de, rgb -- plus
        /// both blocks' FSTAT. Where each machine is parked names the wait it is stuck on;
        /// the origin map is in this file's program layout.
        pub fn debug_state(&self) -> [u32; 8] {
                let sync = unsafe { &*pac::PIO1::ptr() };
                let data = unsafe { &*pac::PIO2::ptr() };
                [
                        sync.sm(SM_HSYNC).sm_addr().read().bits(),
                        sync.sm(SM_VSYNC).sm_addr().read().bits(),
                        data.sm(SM_DE).sm_addr().read().bits(),
                        data.sm(SM_RGB).sm_addr().read().bits(),
                        sync.fstat().read().bits(),
                        data.fstat().read().bits(),
                        //   what the sync block DRIVES (window-relative: bit N = GPIO
                        // 16+N), and the raw pad INPUTS from the SIO (absolute GPIO bits)
                        sync.dbg_padout().read().bits(),
                        unsafe { &*pac::SIO::ptr() }.gpio_in().read().bits(),
                ]
        }

        /// What each block DRIVES and OUTPUT-ENABLES at the pads (window-relative: bit N =
        /// GPIO 16+N), plus the raw pad readbacks from the SIO (bank 0 absolute, then
        /// GPIOs 32+ in the HI register). PADOE is the ground truth for "is this pin
        /// actually an output": a pin the programs write but never enable floats.
        pub fn pad_state(&self) -> [u32; 6] {
                let sync = unsafe { &*pac::PIO1::ptr() };
                let data = unsafe { &*pac::PIO2::ptr() };
                let sio = unsafe { &*pac::SIO::ptr() };
                [
                        sync.dbg_padout().read().bits(),
                        sync.dbg_padoe().read().bits(),
                        data.dbg_padout().read().bits(),
                        data.dbg_padoe().read().bits(),
                        sio.gpio_in().read().bits(),
                        sio.gpio_hi_in().read().bits(),
                ]
        }

        /// Whether the data machine has STARVED (pulled on an empty FIFO) since the last
        /// call -- the underrun detector. Reading clears the flag.
        pub fn data_stalled(&mut self) -> bool {
                let data = unsafe { &*pac::PIO2::ptr() };
                let stalled = data.fdebug().read().txstall().bits() & (1 << SM_RGB) != 0;
                if stalled {
                        data.fdebug().write(|w| unsafe { w.bits(1 << (24 + SM_RGB)) });
                }
                stalled
        }

        pub fn size(&self) -> (u16, u16) {
                (self.width, self.height)
        }
}
