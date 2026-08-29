//! The Rust side of the touch169 spike firmware.
//!
//! The C shell (`../src/main.c`) brings the pico-sdk runtime up and calls `light_app_main`,
//! which never returns. Everything the shell provides to Rust is declared in the one `extern`
//! block below, so the size of the FFI surface -- one of the things the spike measures -- can be
//! read off this file.
//!
//! Milestone 2: the application is a set of modules the runtime orders and polls. Registration
//! is explicit -- the two `rt.add` calls in `light_app_main` ARE the module list -- and logging
//! goes through the bounded queue, drained to the shell's stdio by a module of its own.

#![no_std]

use core::fmt::Write;
use light_core::{info, log, Blinker, Board, Module, Poll, Runtime};
use light_rp2350::Touch169;

unsafe extern "C" {
        /// Hands a Rust panic to pico-sdk's `panic()`, which knows how to print from whichever
        /// core died. Never returns.
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        /// Prints one line on the shell's stdio: the log sink, and nothing else's.
        fn light_shell_log(msg: *const u8, len: usize);
}

/// Owns the board and blinks its backlight, logging every fifth toggle so the console shows
/// life without becoming a 2 Hz stream of the same line.
struct Blink<B: Board> {
        board: B,
        blinker: Blinker,
        toggles: u32,
}

impl<B: Board> Module for Blink<B> {
        fn name(&self) -> &'static str {
                "blink"
        }
        fn deps(&self) -> &'static [&'static str] {
                //   the log drain must be loaded (and so polled) for anything this module says
                // to reach the console; declaring it makes the order a fact rather than a hope
                &["log_drain"]
        }
        fn load(&mut self) -> Result<(), ()> {
                info!("blinking backlight on GPIO{}", Touch169::PIN_BACKLIGHT);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                if !self.blinker.poll(&mut self.board) {
                        return Poll::Idle;
                }
                self.toggles += 1;
                if self.toggles % 5 == 0 {
                        info!("toggle {}, backlight {}", self.toggles, if self.blinker.is_on() { "on" } else { "off" });
                }
                Poll::Busy
        }
}

/// Moves log records from the queue to the shell's stdio, a bounded number per poll so a burst
/// of logging cannot monopolise a pass.
struct LogDrain;

impl LogDrain {
        const PER_POLL: usize = 4;

        fn sink(record: &log::Record) {
                let mut line = StackBuf::<160> { buf: [0; 160], len: 0 };
                let _ = write!(line, "{record}");
                unsafe { light_shell_log(line.buf.as_ptr(), line.len) }
        }
}

impl Module for LogDrain {
        fn name(&self) -> &'static str {
                "log_drain"
        }
        fn poll(&mut self) -> Poll {
                if log::drain(Self::PER_POLL, Self::sink) == 0 { Poll::Idle } else { Poll::Busy }
        }
        fn unload(&mut self) {
                //   whatever was said on the way down still gets out
                while log::drain(usize::MAX, Self::sink) > 0 {}
        }
}

/// Entry point called by the C shell once the runtime is up.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_main() -> ! {
        log::set_clock(light_rp2350::now_us);
        // SAFETY: called once from main(); the shell touches no peripheral after this
        let board = unsafe { Touch169::new() };

        let mut drain = LogDrain;
        let mut blink = Blink { board, blinker: Blinker::new(500_000), toggles: 0 };

        let mut rt: Runtime<4> = Runtime::new();
        //   registered blink-first to show the runtime reorders by dependency
        rt.add(&mut blink).expect("capacity");
        rt.add(&mut drain).expect("capacity");
        rt.start().expect("start");
        //   nothing to sleep on yet: the blink module is the only pacing and it self-paces
        let result = rt.run(|| {});
        panic!("runtime exited: {result:?}");
}

/// A `core::fmt::Write` over a fixed stack buffer, so text can be formatted with no allocator
/// and handed across the FFI as a pointer and length.
struct StackBuf<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> Write for StackBuf<N> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let room = N - self.len;
                let take = s.len().min(room);
                self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
                self.len += take;
                // truncating is the right failure for a line; report success so the formatter
                // keeps going rather than abandoning the message at the first overflow
                Ok(())
        }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        let mut msg = StackBuf::<160> { buf: [0; 160], len: 0 };
        let _ = write!(msg, "{info}");
        unsafe { light_shell_panic(msg.buf.as_ptr(), msg.len) }
}
