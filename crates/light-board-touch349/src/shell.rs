//! The shell ABI glue every touch349 firmware carries. The C shell (`light_mk4_shell`) hands the
//! Rust staticlib its resolved clocks, drains its logs and console input on core 1, and takes its
//! panic message -- all through the `light_shell_*` symbols the shell exports. That handshake is the
//! same for every app on the board, so it lives here; each app keeps only the thin
//! `#[no_mangle]`/`#[panic_handler]` entry points that call in.

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use light_core::log;

unsafe extern "C" {
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        fn light_shell_log(msg: *const u8, len: usize);
        fn light_shell_read_byte() -> i32;
}

/// What the C shell hands `light_app_main`: the resolved clock rates.
#[repr(C)]
pub struct ShellInfo {
        pub clk_sys_hz: u32,
        pub clk_peri_hz: u32,
}

/// A fixed-capacity `fmt::Write` sink on the stack -- for formatting a log line or a panic message
/// without an allocator. Writes past `N` are dropped.
struct StackString<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> StackString<N> {
        fn new() -> Self {
                Self { buf: [0; N], len: 0 }
        }
        fn as_bytes(&self) -> &[u8] {
                &self.buf[..self.len]
        }
}

impl<const N: usize> Write for StackString<N> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let room = N - self.len;
                let take = room.min(s.len());
                self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
                self.len += take;
                Ok(())
        }
}

// --- core 0 stack watermark ----------------------------------------------------------------

//   Core 0's stack fills SCRATCH_Y; the shell gives core 1 a stack in ordinary RAM (a deep core-0
// call chain once landed on core 1's frames in SCRATCH_X and killed the console silently), so
// SCRATCH_X is vacant runway. Paint it plus the bottom of core 0's own bank, and `stack_free`
// reports how deep the deepest call chain reached, for a firmware's `stats`.
const PAINT_BASE: u32 = 0x2008_0000;
/// All of SCRATCH_X plus the bottom kilobyte of SCRATCH_Y: 5 KB.
const PAINT_WORDS: usize = 1280;
const PAINT: u32 = 0xC0DE_55AA;

/// Paint the stack runway. Call FIRST in `light_app_main`, whose own frame sits at the top of core
/// 0's bank, far above the painted region; core 1's stack is elsewhere entirely.
pub fn stack_paint() {
        let p = PAINT_BASE as *mut u32;
        for i in 0..PAINT_WORDS {
                // SAFETY: vacant SCRATCH_X and the unlived bottom of core 0's own bank
                unsafe { core::ptr::write_volatile(p.add(i), PAINT) };
        }
}

/// Untouched painted bytes above the runway base. The nominal stack floor sits at 4096; below that
/// core 0 is living on the runway.
pub fn stack_free() -> u32 {
        let p = PAINT_BASE as *const u32;
        for i in 0..PAINT_WORDS {
                // SAFETY: reads the painted region
                if unsafe { core::ptr::read_volatile(p.add(i)) } != PAINT {
                        return (i * 4) as u32;
                }
        }
        (PAINT_WORDS * 4) as u32
}

// --- core 1 --------------------------------------------------------------------------------

/// Core 1's pulse, counted every [`service_core1`] pass. The console cannot report its own death (a
/// dead core 1 IS a dead console), so a firmware's `stats` reads this from core 0 to diagnose it.
static CORE1_TICKS: AtomicU32 = AtomicU32::new(0);

/// The core 1 pulse count, for a firmware's `stats`.
pub fn core1_ticks() -> u32 {
        CORE1_TICKS.load(Ordering::Relaxed)
}

fn log_line(record: &log::Record) {
        let mut line = StackString::<160>::new();
        let _ = write!(line, "{record}");
        let b = line.as_bytes();
        unsafe { light_shell_log(b.as_ptr(), b.len()) }
}

/// Core 1's service pass, called from each app's `#[no_mangle] light_app_core1_service`: count the
/// pulse, drain up to four log lines to the shell, and pump up to 32 console bytes to `push` (a
/// console-less app passes `|_| {}`, and the bytes are still drained so the shell's buffer never
/// backs up).
pub fn service_core1(mut push: impl FnMut(u8)) {
        CORE1_TICKS.fetch_add(1, Ordering::Relaxed);
        log::drain(4, log_line);
        for _ in 0..32 {
                let b = unsafe { light_shell_read_byte() };
                if b < 0 {
                        break;
                }
                push(b as u8);
        }
}

/// Format a panic and hand it to the shell; never returns. Each app's `#[panic_handler]` calls this.
pub fn panic_report(info: &core::panic::PanicInfo) -> ! {
        let mut msg = StackString::<160>::new();
        let _ = write!(msg, "{info}");
        let b = msg.as_bytes();
        unsafe { light_shell_panic(b.as_ptr(), b.len()) }
}
