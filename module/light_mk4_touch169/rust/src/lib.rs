//! The Rust side of the touch169 spike firmware.
//!
//! The C shell (`../src/main.c`) brings the pico-sdk runtime up and calls `light_app_main`,
//! which never returns. Everything the shell provides to Rust is declared in the one `extern`
//! block below, so the size of the FFI surface -- one of the things the spike measures -- can be
//! read off this file.

#![no_std]

use core::fmt::Write;
use light_core::Blinker;
use light_rp2350::Touch169;

unsafe extern "C" {
        /// Hands a Rust panic to pico-sdk's `panic()`, which knows how to print from whichever
        /// core died. Never returns.
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        /// Prints one line on the shell's stdio. Text for now; records once the log queue exists.
        fn light_shell_log(msg: *const u8, len: usize);
}

fn log(args: core::fmt::Arguments) {
        let mut line = StackBuf::<96> { buf: [0; 96], len: 0 };
        let _ = line.write_fmt(args);
        unsafe { light_shell_log(line.buf.as_ptr(), line.len) }
}

/// Entry point called by the C shell once the runtime is up.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_main() -> ! {
        // SAFETY: called once from main(); the shell touches no peripheral after this
        let mut board = unsafe { Touch169::new() };
        let mut blink = Blinker::new(500_000);
        log(format_args!("rust: up, blinking backlight on GPIO{}", Touch169::PIN_BACKLIGHT));
        let mut toggles: u32 = 0;
        loop {
                if blink.poll(&mut board) {
                        toggles += 1;
                        // every fifth toggle, so the console shows life without becoming a
                        // 2 Hz stream of the same line
                        if toggles % 5 == 0 {
                                log(format_args!("rust: toggle {toggles}, backlight {}", if blink.is_on() { "on" } else { "off" }));
                        }
                }
        }
}

/// A `core::fmt::Write` over a fixed stack buffer, so a panic message can be formatted with no
/// allocator and handed across the FFI as a pointer and length.
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
                // truncating is the right failure for a panic message; report success so the
                // formatter keeps going rather than aborting the message at the first overflow
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
