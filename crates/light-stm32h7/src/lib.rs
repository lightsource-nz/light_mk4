//! STM32H743 chip access for the light framework, on bare CMSIS: the port behind
//! `light_core::hal`, and the counterpart of `light-rp2`. Board wiring -- which pins carry
//! what on whichever board the chip sits on -- lives with the application, not here.
//!
//! The C shell (`module/light_mk4_shell_cmsis`) owns what mk3's chip port owned: the CMSIS
//! startup file and linker script, the clock tree (400 MHz off the crystal, with PLL1's Q output
//! for the SPI kernel clock and PLL3 at 48 MHz for USB), the caches, and the console -- ITM and
//! USART1 both, since they fail in opposite ways. This crate owns the peripherals the drivers
//! talk to, through the registers directly. mk3's finding that matters most is carried in
//! [`spi::Spi4Display`]: the H7's SPI is a different generation from the F4's, with a transfer
//! size programmed per transaction and a FIFO to prime before the start.
//!
//! No pac: the handful of registers this port touches are written against the reference
//! manual, as mk3's C was, which keeps the crate small and its failures legible.

#![no_std]

//   the critical-section implementation is cortex-m's single-core one, and a crate nothing
// names is a crate the linker never sees: this `use` is what keeps its acquire/release symbols
// in the image
use cortex_m as _;
use core::sync::atomic::{AtomicU32, Ordering};
use light_core::hal::{Clock, Idle};

pub mod gpio;
pub mod spi;

/// Register access: the port's whole peripheral vocabulary.
mod reg {
        #[inline(always)]
        pub fn read(addr: usize) -> u32 {
                // SAFETY: every address this module is handed is a memory-mapped register
                unsafe { core::ptr::read_volatile(addr as *const u32) }
        }
        #[inline(always)]
        pub fn write(addr: usize, v: u32) {
                unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
        }
        #[inline(always)]
        pub fn modify(addr: usize, clear: u32, set: u32) {
                write(addr, (read(addr) & !clear) | set);
        }
        #[inline(always)]
        pub fn write_u8(addr: usize, v: u8) {
                unsafe { core::ptr::write_volatile(addr as *mut u8, v) }
        }
}

pub const RCC_BASE: usize = 0x5802_4400;
pub const RCC_AHB4ENR: usize = RCC_BASE + 0x0E0;
pub const RCC_APB1LENR: usize = RCC_BASE + 0x0E8;
pub const RCC_APB2ENR: usize = RCC_BASE + 0x0F0;

const TIM2_BASE: usize = 0x4000_0000;
const TIM2_CR1: usize = TIM2_BASE + 0x00;
const TIM2_EGR: usize = TIM2_BASE + 0x14;
const TIM2_CNT: usize = TIM2_BASE + 0x24;
const TIM2_PSC: usize = TIM2_BASE + 0x28;
const TIM2_ARR: usize = TIM2_BASE + 0x2C;

/// What the shell hands over: the clocks its runtime configured.
#[derive(Clone, Copy, Debug)]
pub struct Clocks {
        pub sys_hz: u32,
        /// APB2, which SPI4/5 take their kernel clock from by reset default.
        pub apb2_hz: u32,
        /// The APB1 timers' clock: twice APB1 whenever APB1 is prescaled, which it is.
        pub tim_hz: u32,
}

/// The microsecond clock: TIM2, the 32-bit timer, free-running at 1 MHz, read as a 64-bit
/// count by tracking its wraps. Atomics with relaxed ordering: one core and one caller in
/// practice, and a torn pair could only misplace a wrap by one read.
static LAST_CNT: AtomicU32 = AtomicU32::new(0);
static WRAPS: AtomicU32 = AtomicU32::new(0);

/// Start TIM2 at 1 MHz. Once, before `now_us` means anything.
pub fn clock_init(clocks: &Clocks) {
        reg::modify(RCC_APB1LENR, 0, 1 << 0); // TIM2EN
        let _ = reg::read(RCC_APB1LENR);
        reg::write(TIM2_CR1, 0);
        reg::write(TIM2_PSC, (clocks.tim_hz / 1_000_000).saturating_sub(1));
        reg::write(TIM2_ARR, 0xFFFF_FFFF);
        reg::write(TIM2_EGR, 1); // UG: load the prescaler
        reg::write(TIM2_CNT, 0);
        reg::write(TIM2_CR1, 1); // CEN
}

/// Microseconds since `clock_init`. Monotonic as long as it is read at least once an hour,
/// which a polled runtime does many thousand times a second.
pub fn now_us() -> u64 {
        let cnt = reg::read(TIM2_CNT);
        if cnt < LAST_CNT.load(Ordering::Relaxed) {
                WRAPS.fetch_add(1, Ordering::Relaxed);
        }
        LAST_CNT.store(cnt, Ordering::Relaxed);
        (u64::from(WRAPS.load(Ordering::Relaxed)) << 32) | u64::from(cnt)
}

/// The system timer as a [`Clock`].
#[derive(Clone, Copy, Default)]
pub struct SysClock;

impl Clock for SysClock {
        fn now_us(&self) -> u64 {
                now_us()
        }
}

/// Between idle passes: a breath, not a sleep, for the reason the RP2350 port gives.
#[derive(Clone, Copy, Default)]
pub struct Breathe;

impl Idle for Breathe {
        fn idle(&mut self) {
                core::hint::spin_loop();
        }
}
