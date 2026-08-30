//! STM32F411 board access for the light framework, on bare CMSIS: the WeAct Blackpill. The
//! same shape as `light-stm32h7` at the F4's addresses -- GPIO on AHB1, a different RCC map --
//! with a microsecond clock on the 32-bit TIM2. No bus drivers yet: the Blackpill carries a
//! LED, a key and a console, and that is what mk3 ran on it.

#![no_std]

//   the critical-section implementation is cortex-m's single-core one, and a crate nothing
// names is a crate the linker never sees
use core::sync::atomic::{AtomicU32, Ordering};
use cortex_m as _;
use light_core::hal::{Clock, Idle};

pub mod boards;
pub mod gpio;

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
}

pub const RCC_BASE: usize = 0x4002_3800;
pub const RCC_AHB1ENR: usize = RCC_BASE + 0x30;
pub const RCC_APB1ENR: usize = RCC_BASE + 0x40;
pub const RCC_APB2ENR: usize = RCC_BASE + 0x44;

const TIM2_BASE: usize = 0x4000_0000;
const TIM2_CR1: usize = TIM2_BASE + 0x00;
const TIM2_EGR: usize = TIM2_BASE + 0x14;
const TIM2_CNT: usize = TIM2_BASE + 0x24;
const TIM2_PSC: usize = TIM2_BASE + 0x28;
const TIM2_ARR: usize = TIM2_BASE + 0x2C;

/// What the shell hands over.
#[derive(Clone, Copy, Debug)]
pub struct Clocks {
        pub sys_hz: u32,
        pub apb2_hz: u32,
        pub tim_hz: u32,
}

//   the wrap tracking is two words read and written by whichever context calls now_us():
// atomics with relaxed ordering, since a torn or reordered pair could only misplace a wrap by
// one read, and there is one core and one caller in practice
static LAST_CNT: AtomicU32 = AtomicU32::new(0);
static WRAPS: AtomicU32 = AtomicU32::new(0);

/// Start TIM2 at 1 MHz. Once, before `now_us` means anything.
pub fn clock_init(clocks: &Clocks) {
        reg::modify(RCC_APB1ENR, 0, 1 << 0); // TIM2EN
        let _ = reg::read(RCC_APB1ENR);
        reg::write(TIM2_CR1, 0);
        reg::write(TIM2_PSC, (clocks.tim_hz / 1_000_000).saturating_sub(1));
        reg::write(TIM2_ARR, 0xFFFF_FFFF);
        reg::write(TIM2_EGR, 1);
        reg::write(TIM2_CNT, 0);
        reg::write(TIM2_CR1, 1);
}

/// Microseconds since `clock_init`, read at least once an hour to stay monotonic.
pub fn now_us() -> u64 {
        let cnt = reg::read(TIM2_CNT);
        if cnt < LAST_CNT.load(Ordering::Relaxed) {
                WRAPS.fetch_add(1, Ordering::Relaxed);
        }
        LAST_CNT.store(cnt, Ordering::Relaxed);
        (u64::from(WRAPS.load(Ordering::Relaxed)) << 32) | u64::from(cnt)
}

#[derive(Clone, Copy, Default)]
pub struct SysClock;

impl Clock for SysClock {
        fn now_us(&self) -> u64 {
                now_us()
        }
}

#[derive(Clone, Copy, Default)]
pub struct Breathe;

impl Idle for Breathe {
        fn idle(&mut self) {
                core::hint::spin_loop();
        }
}
