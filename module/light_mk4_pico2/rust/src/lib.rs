//! The Rust side of the bare Pico 2 spike firmware: the on-board LED (GPIO25, the same pin the
//! touch169 uses for its backlight) and the console. Built for the po13 rig -- a Pico 2 in an
//! SWD dock -- so the debugger questions can be answered: probe-rs against the debugprobe, and
//! whether a halt/resume disturbs a firmware whose GPIO goes through SIO registers rather than
//! the GPIO coprocessor.
//!
//! The shell (module/light_mk4_shell) is the same file the touch169 links.

#![no_std]

use core::fmt::Write;
use light_core::{info, log, warn, Blinker, Board, LineReader, Mailbox, Module, Poll, Runtime};
use light_rp2350::{now_us, Backlight};

unsafe extern "C" {
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        fn light_shell_log(msg: *const u8, len: usize);
        fn light_shell_read_byte() -> i32;
}

#[derive(Clone, Copy, Debug)]
enum LedEvent {
        On,
        Off,
        Blink,
        ReportStats,
}

static LED_EVENTS: Mailbox<LedEvent, 4> = Mailbox::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

fn log_sink(record: &log::Record) {
        let mut line = StackBuf::<160> { buf: [0; 160], len: 0 };
        let _ = write!(line, "{record}");
        unsafe { light_shell_log(line.buf.as_ptr(), line.len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_service() {
        log::drain(4, log_sink);
        for _ in 0..32 {
                let b = unsafe { light_shell_read_byte() };
                if b < 0 {
                        break;
                }
                let _ = CONSOLE_BYTES.push(b as u8);
        }
}

/// The LED: blinking by default, or held on or off from the console.
struct LedMod {
        led: Backlight,
        blinker: Blinker,
        blinking: bool,
        toggles: u32,
}

impl Module for LedMod {
        fn name(&self) -> &'static str {
                "led"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = LED_EVENTS.pop() {
                        busy = true;
                        match ev {
                                LedEvent::On => {
                                        self.blinking = false;
                                        self.led.set_backlight(true);
                                }
                                LedEvent::Off => {
                                        self.blinking = false;
                                        self.led.set_backlight(false);
                                }
                                LedEvent::Blink => self.blinking = true,
                                LedEvent::ReportStats => info!("led: {} toggles, blinking={}", self.toggles, self.blinking),
                        }
                }
                if self.blinking && self.blinker.poll(&mut self.led) {
                        self.toggles += 1;
                        busy = true;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.led.set_backlight(false);
        }
}

struct ConsoleMod {
        reader: LineReader<96>,
}

impl ConsoleMod {
        fn dispatch(&mut self, line: &str) -> Poll {
                info!("> {line}");
                let mut words = line.split_whitespace();
                match (words.next(), words.next()) {
                        (Some("help"), _) => info!("commands: help | stats | led on|off|blink | quit"),
                        (Some("stats"), _) => {
                                let _ = LED_EVENTS.push(LedEvent::ReportStats);
                                info!("uptime {} s; console: {} bytes dropped, {} lines dropped", now_us() / 1_000_000, CONSOLE_BYTES.dropped(), self.reader.dropped_lines);
                        }
                        (Some("led"), Some("on")) => {
                                let _ = LED_EVENTS.push(LedEvent::On);
                        }
                        (Some("led"), Some("off")) => {
                                let _ = LED_EVENTS.push(LedEvent::Off);
                        }
                        (Some("led"), Some("blink")) => {
                                let _ = LED_EVENTS.push(LedEvent::Blink);
                        }
                        (Some("quit"), _) => {
                                info!("shutting down");
                                return Poll::Shutdown;
                        }
                        (Some(other), _) => warn!("unknown command '{other}' -- try help"),
                        (None, _) => {}
                }
                Poll::Busy
        }
}

impl Module for ConsoleMod {
        fn name(&self) -> &'static str {
                "console"
        }
        fn poll(&mut self) -> Poll {
                let mut result = Poll::Idle;
                while let Some(b) = CONSOLE_BYTES.pop() {
                        if let Some(line) = self.reader.push(b) {
                                match self.dispatch(line.as_str()) {
                                        Poll::Shutdown => return Poll::Shutdown,
                                        p => result = p,
                                }
                        }
                }
                result
        }
}

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main() -> ! {
        log::set_clock(now_us);
        // SAFETY: constructed once, here
        let led = unsafe { Backlight::new() };
        let mut led_mod = LedMod { led, blinker: Blinker::new(500_000), blinking: true, toggles: 0 };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<2> = Runtime::new();
        rt.add(&mut led_mod).expect("capacity");
        rt.add(&mut console_mod).expect("capacity");
        rt.start().expect("start");
        info!("pico2 runtime started; type 'help' on the console");
        let result = rt.run(|| {});
        match result {
                Ok(()) => info!("runtime stopped cleanly; core 0 idle"),
                Err(e) => warn!("runtime stopped with {e:?}; core 0 idle"),
        }
        loop {
                core::hint::spin_loop();
        }
}

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
