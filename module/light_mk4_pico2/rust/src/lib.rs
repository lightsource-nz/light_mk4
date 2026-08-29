//! The Rust side of the bare Pico 2 firmware: the on-board LED and the console. Built for the
//! po13 rig -- a Pico 2 in an SWD dock -- so the debugger questions can be answered.
//!
//! The shell (module/light_mk4_shell) is the same file the touch169 links.

#![no_std]

use core::fmt::Write;
use light_core::{info, log, warn, Blinker, EventBus, LineReader, Mailbox, Module, Poll, Runtime, Subscription};
use light_rp2350::boards::pico2;
use light_rp2350::gpio::Output;
use light_rp2350::{now_us, Breathe, Clocks, SysClock};

unsafe extern "C" {
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        fn light_shell_log(msg: *const u8, len: usize);
        fn light_shell_read_byte() -> i32;
}

#[repr(C)]
pub struct ShellInfo {
        clk_sys_hz: u32,
        clk_peri_hz: u32,
}

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        LedOn,
        LedOff,
        LedBlink,
        Stats,
}

static EVENTS: EventBus<AppEvent, 8, 2> = EventBus::new();
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
        led: Output,
        blinker: Blinker,
        blinking: bool,
        toggles: u32,
        events: Subscription,
}

impl Module for LedMod {
        fn name(&self) -> &'static str {
                "led"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        busy = true;
                        match ev {
                                AppEvent::LedOn => {
                                        self.blinking = false;
                                        self.led.set(true);
                                }
                                AppEvent::LedOff => {
                                        self.blinking = false;
                                        self.led.set(false);
                                }
                                AppEvent::LedBlink => self.blinking = true,
                                AppEvent::Stats => info!("led: {} toggles, blinking={}", self.toggles, self.blinking),
                        }
                }
                if self.blinking && self.blinker.poll(&mut self.led, &SysClock) {
                        self.toggles += 1;
                        busy = true;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.led.set(false);
        }
}

struct ConsoleMod {
        reader: LineReader<96>,
}

impl ConsoleMod {
        fn dispatch(&mut self, line: &str) -> Poll {
                info!("> {line}");
                let mut words = line.split_whitespace();
                let event = match (words.next(), words.next()) {
                        (Some("help"), _) => {
                                info!("commands: help | stats | led on|off|blink | quit");
                                None
                        }
                        (Some("stats"), _) => {
                                info!("uptime {} s; console: {} bytes dropped, {} lines dropped; bus: {} refused", now_us() / 1_000_000, CONSOLE_BYTES.dropped(), self.reader.dropped_lines, EVENTS.refused());
                                Some(AppEvent::Stats)
                        }
                        (Some("led"), Some("on")) => Some(AppEvent::LedOn),
                        (Some("led"), Some("off")) => Some(AppEvent::LedOff),
                        (Some("led"), Some("blink")) => Some(AppEvent::LedBlink),
                        (Some("quit"), _) => {
                                info!("shutting down");
                                return Poll::Shutdown;
                        }
                        (Some(other), _) => {
                                warn!("unknown command '{other}' -- try help");
                                None
                        }
                        (None, _) => None,
                };
                if let Some(e) = event {
                        let _ = EVENTS.publish(e);
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
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        log::set_clock(now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = pico2::take(&clocks).expect("the board's peripherals are taken once");
        let mut led_mod = LedMod { led: p.led, blinker: Blinker::new(500_000), blinking: true, toggles: 0, events: EVENTS.subscribe().expect("slot") };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<2> = Runtime::new();
        rt.add(&mut led_mod).expect("capacity");
        rt.add(&mut console_mod).expect("capacity");
        rt.start().expect("start");
        info!("pico2 runtime started at {} Hz; type 'help' on the console", clocks.sys_hz);
        let mut idle = Breathe;
        let result = rt.run(|| light_core::Idle::idle(&mut idle));
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
                let take = s.len().min(N - self.len);
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
