//! The `critical_section` implementation for RP2350: interrupts off on this core AND a hardware
//! spinlock held against the other core.
//!
//! This is the primitive mk3 had three divergent versions of (lock-free CAS on host, pico-sdk
//! `critical_section_t` on RP2, a saved PRIMASK on STM32 that was silently not re-entrant).
//! Here there is exactly one, the portable code sees only `critical_section::with`, and nesting
//! is handled: a section entered while interrupts are already disabled is treated as nested and
//! does not touch the spinlock, which is what makes it safe to call from inside another section
//! -- the case the STM32 port's contract forbade in a comment.
//!
//! Spinlock 31 is the one pico-sdk and rp2040-hal leave for exactly this purpose; the SDK's own
//! `critical_section_t` uses claimed locks from the striped range, so the two never contend for
//! the same lock -- they simply do not protect each other's data, which is the expected split
//! while the shell and the Rust side own separate state.
//!
//! The chip has two ISAs and this file serves both: the interrupt mask is PRIMASK on the
//! Cortex-M33 pair and `mstatus.MIE` on the Hazard3 pair; the spinlock is the same SIO register
//! either way. The bootrom leaves lock 31 free on both, and the debugger reading it acquires it
//! on both -- write 1 to release.

use rp235x_pac as pac;

const SPINLOCK: usize = 31;

/// Whether interrupts are enabled on this core.
#[inline(always)]
fn interrupts_active() -> bool {
        #[cfg(target_arch = "arm")]
        {
                cortex_m::register::primask::read().is_active()
        }
        #[cfg(target_arch = "riscv32")]
        {
                let mstatus: u32;
                // SAFETY: a CSR read
                unsafe { core::arch::asm!("csrr {0}, mstatus", out(reg) mstatus) };
                mstatus & (1 << 3) != 0
        }
        #[cfg(not(any(target_arch = "arm", target_arch = "riscv32")))]
        {
                true
        }
}

#[inline(always)]
fn interrupts_disable() {
        #[cfg(target_arch = "arm")]
        cortex_m::interrupt::disable();
        #[cfg(target_arch = "riscv32")]
        // SAFETY: clears MIE; the matching enable is in release()
        unsafe {
                core::arch::asm!("csrci mstatus, 8")
        };
}

/// # Safety
/// Only from the outermost section's release.
#[inline(always)]
unsafe fn interrupts_enable() {
        #[cfg(target_arch = "arm")]
        unsafe {
                cortex_m::interrupt::enable()
        };
        #[cfg(target_arch = "riscv32")]
        unsafe {
                core::arch::asm!("csrsi mstatus, 8")
        };
}

struct Impl;
critical_section::set_impl!(Impl);

unsafe impl critical_section::Impl for Impl {
        unsafe fn acquire() -> critical_section::RawRestoreState {
                let was_active = interrupts_active();
                interrupts_disable();
                if was_active {
                        //   outermost section: take the lock. reading the SIO spinlock register
                        // returns nonzero when this read acquired it, zero when it is held
                        let sio = unsafe { &*pac::SIO::ptr() };
                        while sio.spinlock(SPINLOCK).read().bits() == 0 {
                                core::hint::spin_loop();
                        }
                        //   ensure everything after this point sees memory as of the acquire
                        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
                }
                was_active
        }

        unsafe fn release(was_active: critical_section::RawRestoreState) {
                if was_active {
                        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
                        //   any write releases
                        let sio = unsafe { &*pac::SIO::ptr() };
                        sio.spinlock(SPINLOCK).write(|w| unsafe { w.bits(1) });
                        //   only the outermost section re-enables interrupts; a nested one
                        // leaves them as it found them, which is disabled
                        unsafe { interrupts_enable() };
                }
        }
}
