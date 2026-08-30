//! crossfire: every USB-MIDI instrument on the host port hears every other. mk3's crossfire on
//! the mk4 stack, on the po13 rig: a Pico 2 whose native USB port hosts the instruments (one,
//! or a hub of them), the Pico-OLED-1.3 as the status display, the console on the UART.
//!
//! The forwarding engine is `light_midi`, portable and host-tested; the transport is
//! TinyUSB through the shell's host role (`light_rp2::tinyusb_midi`); this file is the wiring:
//! a module that drives the stack and the engine, a module that draws the status, the LED, and
//! the console.

#![no_std]

use core::fmt::Write;
use light_midi::{Forwarder, HUB_PORT_NONE};
use light_display::sh1107::Sh1107;
use light_core::{info, log, warn, ConstStaticCell, EventBus, LineReader, Mailbox, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer, LogicalRegion, UpdateError};
use light_draw::{Flip, PixelFormat, Point, Rotation};
use light_font::Font;
mod board;
use board::*;
use light_rp2::gpio::Output;
use light_rp2::spi::Spi1Display;
use light_rp2::tinyusb_midi::{MidiEvent, UsbMidiHost};
use light_rp2::{now_us, Breathe, Clocks, SysClock};

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

/// USB device slots the engine tracks: TinyUSB's CFG_TUH_MIDI, which tusb_config.h sets to the
/// same four. The engine indexes its table with the mount index directly, so the two must agree.
const USB_SLOTS: usize = 4;

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        /// The mounted set changed: the display's text is stale.
        Status,
        /// The RX/TX indicators changed.
        Indicators { rx: bool, tx: bool },
        /// Whether anything is mounted, for the LED.
        Mounted(bool),
        Stats,
        /// Tear the host controller down and bring it back, from the console.
        UsbReset,
        /// Whether the engine's root-port-empty verdict resets the controller by itself.
        AutoReset(bool),
}

static EVENTS: EventBus<AppEvent, 8, 3> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

/// 64x128 at 1 bpp: one kilobyte.
static FRAME: ConstStaticCell<[u8; PixelFormat::Mono1.buffer_len(OLED_WIDTH, OLED_HEIGHT)]> = ConstStaticCell::new([0; PixelFormat::Mono1.buffer_len(OLED_WIDTH, OLED_HEIGHT)]);
static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));

fn log_sink(record: &log::Record) {
        let mut line = StackString::<160>::new();
        let _ = write!(line, "{record}");
        unsafe { light_shell_log(line.buf.as_ptr(), line.len) }
}

/// Core 1: the UART log drain and console read. No USB here -- the host stack is core 0's.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_service() {
        CORE1_PASSES.fetch_add(1, light_core::atomic::Ordering::Relaxed);
        log::drain(4, log_sink);
        for _ in 0..32 {
                let b = unsafe { light_shell_read_byte() };
                if b < 0 {
                        break;
                }
                let _ = CONSOLE_BYTES.push(b as u8);
        }
}

/// The status the display shows, published by the USB module and read by the OLED module: the
/// engine itself stays private to the module that drives it.
#[derive(Clone, Copy, Debug, Default)]
struct Status {
        mounted: u8,
        hub_addr: u8,
        /// Which of the hub's first four ports carry an instrument.
        ports: [bool; USB_SLOTS],
        rx: bool,
        tx: bool,
}

static STATUS: light_core::Mailbox<Status, 1> = light_core::Mailbox::new();

/// Heartbeats, one per core, for a post-mortem that reads memory without halting anything:
/// whether each core is still executing its loop is the first question, and it should not
/// take a debugger session that disturbs the answer.
static CORE0_PASSES: light_core::atomic::AtomicU32 = light_core::atomic::AtomicU32::new(0);
static CORE1_PASSES: light_core::atomic::AtomicU32 = light_core::atomic::AtomicU32::new(0);

/// Owns the host stack and the forwarding engine. Every pass: run the stack, apply what it
/// reported, forward what arrived, and say what changed.
struct UsbMod {
        host: UsbMidiHost,
        forwarder: Forwarder<USB_SLOTS>,
        events: Subscription,
        reset_pending: bool,
        /// mk3 reset the controller whenever a disconnect emptied the root port, working
        /// around a stale buffer-control state (hathach/tinyusb#3533) -- and the RP2350 needs
        /// it too: without the reset the next enumeration panicked inside the USB IRQ. The
        /// reset itself hung in tusb_deinit(), which closed devices after tearing down the
        /// port's critical section; that is fixed in the pico-sdk TinyUSB fork. `usb autoreset
        /// off` keeps the switch for the bench
        auto_reset: bool,
        packets: u32,
        status: Status,
}

impl UsbMod {
        fn publish_status(&mut self) {
                let mut s = Status { mounted: self.forwarder.usb_mounted_count() as u8, hub_addr: self.forwarder.hub_addr(), ports: [false; USB_SLOTS], rx: self.status.rx, tx: self.status.tx };
                for (i, p) in s.ports.iter_mut().enumerate() {
                        *p = self.forwarder.hub_port_occupied(i as u8 + 1);
                }
                self.status = s;
                // the mailbox holds the latest only: a stale status is worthless
                let _ = STATUS.pop();
                let _ = STATUS.push(s);
        }
}

impl Module for UsbMod {
        fn name(&self) -> &'static str {
                "usb"
        }
        fn poll(&mut self) -> Poll {
                CORE0_PASSES.fetch_add(1, light_core::atomic::Ordering::Relaxed);
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Stats => info!("usb: {} mounted, hub addr {}, {} packets forwarded, {} dropped (cable), {} events dropped, auto-reset {}", self.forwarder.usb_mounted_count(), self.forwarder.hub_addr(), self.packets, self.forwarder.dropped, self.host.dropped_events(), if self.auto_reset { "on" } else { "off" }),
                                AppEvent::UsbReset => self.reset_pending = true,
                                AppEvent::AutoReset(on) => {
                                        self.auto_reset = on;
                                        info!("usb: controller auto-reset on an empty root port {}", if on { "on" } else { "off" });
                                }
                                _ => {}
                        }
                }
                //   the reset is done here, at the top of a pass, never from inside the
                // callback that asked for it
                //   a reset unmounts everything, and those unmounts empty the bus, which would
                // ask for a second reset: the verdicts of the pass that follows a reset are not
                // honoured
                let mut just_reset = false;
                if self.reset_pending {
                        self.reset_pending = false;
                        info!("resetting the USB host controller");
                        self.host.reset();
                        just_reset = true;
                }
                self.host.task();
                let mut busy = false;
                while let Some(ev) = self.host.next_event() {
                        busy = true;
                        let change = match ev {
                                MidiEvent::Mounted { idx, mount, bus } => {
                                        match bus {
                                                Some(b) if b.hub_addr != 0 => info!("USB-MIDI device mounted: idx {idx} daddr {} rx {} tx {}, on hub {} port {}", mount.daddr, mount.rx_cables, mount.tx_cables, b.hub_addr, b.hub_port),
                                                _ => info!("USB-MIDI device mounted: idx {idx} daddr {} rx {} tx {}, on the root port", mount.daddr, mount.rx_cables, mount.tx_cables),
                                        }
                                        self.forwarder.mount(idx, mount, bus)
                                }
                                MidiEvent::Unmounted { idx } => {
                                        info!("USB-MIDI device unmounted: idx {idx}");
                                        self.forwarder.unmount(idx)
                                }
                        };
                        let Some(c) = change else {
                                warn!("the host stack reported a slot the engine does not have");
                                continue;
                        };
                        if c.reset_host && !just_reset {
                                if self.auto_reset {
                                        self.reset_pending = true;
                                } else {
                                        info!("root port empty; the controller is left as it is (`usb autoreset on` to reset it)");
                                }
                        }
                        let _ = EVENTS.publish(AppEvent::Mounted(c.any_usb_mounted));
                        self.publish_status();
                        let _ = EVENTS.publish(AppEvent::Status);
                }
                let now_ms = (now_us() / 1000) as u32;
                let activity = self.forwarder.service(&mut self.host, now_ms);
                if activity.forwarded {
                        self.packets += 1;
                        busy = true;
                }
                let (rx, tx, changed) = self.forwarder.indicators(now_ms);
                if changed {
                        self.status.rx = rx;
                        self.status.tx = tx;
                        self.publish_status();
                        let _ = EVENTS.publish(AppEvent::Indicators { rx, tx });
                        busy = true;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}

/// The status display: two lines of text and the RX/TX indicators, on the OLED rotated so the
/// text runs along the long side. Event-driven, unpaced: a mount or a burst of MIDI, not a clock.
/// The indicator band is pushed on its own when only an indicator changed -- under the rotation
/// it is a handful of the panel's columns, and pushing the whole panel for it would visibly wipe
/// across the glass on every burst.
struct OledMod {
        display: Display<'static, Sh1107<Spi1Display>>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        events: Subscription,
        status: Status,
        dirty: bool,
        indicators_only: bool,
}

const INDICATOR_SIZE: i32 = 12;
const INDICATOR_TX_X: i32 = 20;

impl OledMod {
        fn indicator_y(&self) -> i32 {
                2 * i32::from(self.font.cell_height()) + 4
        }

        fn frame(&mut self) -> bool {
                let font = self.font;
                let s = self.status;
                let y = self.indicator_y();
                let Some(mut c) = self.layer.frame_begin(&mut self.display, now_us()) else { return false };
                c.text(&font, Point::new(0, 0), "Crossfire");
                let mut line = StackString::<16>::new();
                if s.hub_addr != 0 {
                        // which ports are occupied rather than how many devices: with four
                        // sockets in front of you that is the question you have
                        let _ = line.write_str("hub ");
                        for (i, p) in s.ports.iter().enumerate() {
                                let _ = line.write_char(if *p { (b'1' + i as u8) as char } else { '-' });
                        }
                } else {
                        let _ = write!(line, "devices: {}", s.mounted);
                }
                c.text(&font, Point::new(0, i32::from(font.cell_height())), line.as_str());
                if s.rx {
                        c.rect(Point::new(0, y), Point::new(INDICATOR_SIZE, y + INDICATOR_SIZE), true);
                }
                if s.tx {
                        c.rect(Point::new(INDICATOR_TX_X, y), Point::new(INDICATOR_TX_X + INDICATOR_SIZE, y + INDICATOR_SIZE), true);
                }
                drop(c);
                if self.indicators_only {
                        self.layer.invalidate(LogicalRegion::new(0, y, INDICATOR_TX_X + INDICATOR_SIZE, y + INDICATOR_SIZE));
                } else {
                        self.layer.invalidate_all();
                }
                self.layer.frame_end(&mut self.display);
                self.dirty = false;
                self.indicators_only = true;
                true
        }
}

impl Module for OledMod {
        fn name(&self) -> &'static str {
                "oled"
        }
        fn load(&mut self) -> Result<(), ()> {
                let mut clock = SysClock;
                self.display.driver().set_display_offset(OLED_DISPLAY_OFFSET);
                self.display.init(&mut clock);
                self.display.driver().clear(false);
                self.layer.set_orientation(Rotation::R90, Flip::None);
                self.dirty = true;
                self.indicators_only = false;
                self.frame();
                info!("status display up: {}x{} logical, {}px font", OLED_HEIGHT, OLED_WIDTH, self.font.pixel_size());
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.layer.poll(&mut self.display) {
                        Ok(_) => {}
                        Err(UpdateError::Timeout) => warn!("oled chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Status => {
                                        self.dirty = true;
                                        self.indicators_only = false;
                                }
                                AppEvent::Indicators { .. } => self.dirty = true,
                                AppEvent::Stats => info!("oled: {} frames, {} skipped, {} chunk timeouts", self.layer.frames(), self.layer.skipped, self.display.timeouts),
                                _ => {}
                        }
                }
                if self.dirty {
                        if let Some(s) = STATUS.pop() {
                                self.status = s;
                        }
                        self.frame();
                }
                if self.dirty || self.layer.busy(&self.display) { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.display.driver().clear(false);
        }
}

/// The LED: lit while anything is mounted.
struct LedMod {
        led: Output,
        events: Subscription,
}

impl Module for LedMod {
        fn name(&self) -> &'static str {
                "led"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::Mounted(on) = ev {
                                self.led.set(on);
                                busy = true;
                        }
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
                match words.next() {
                        Some("help") => info!("commands: help | stats | usb reset | usb autoreset on|off | quit"),
                        Some("usb") => match words.next() {
                                Some("reset") => {
                                        let _ = EVENTS.publish(AppEvent::UsbReset);
                                }
                                Some("autoreset") => match words.next() {
                                        Some("on") => {
                                                let _ = EVENTS.publish(AppEvent::AutoReset(true));
                                        }
                                        Some("off") => {
                                                let _ = EVENTS.publish(AppEvent::AutoReset(false));
                                        }
                                        _ => warn!("usage: usb autoreset on|off"),
                                },
                                _ => warn!("usage: usb reset | usb autoreset on|off"),
                        },
                        Some("stats") => {
                                info!("uptime {} s; console: {} bytes dropped; bus: {} refused; passes core0 {} core1 {}; log dropped {}", now_us() / 1_000_000, CONSOLE_BYTES.dropped(), EVENTS.refused(), CORE0_PASSES.load(light_core::atomic::Ordering::Relaxed), CORE1_PASSES.load(light_core::atomic::Ordering::Relaxed), log::pending());
                                let _ = EVENTS.publish(AppEvent::Stats);
                        }
                        Some("quit") => {
                                info!("shutting down");
                                return Poll::Shutdown;
                        }
                        Some(other) => warn!("unknown command '{other}' -- try help"),
                        None => {}
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
        let p = take(&clocks).expect("the board's peripherals are taken once");
        let frame: &'static mut [u8] = FRAME.take();
        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(OLED_WIDTH, OLED_HEIGHT, PixelFormat::Mono1));
        let layer: &'static mut FrameLayer = LAYER.take();
        let display = Display::new(Sh1107::new(p.oled_bus), frame, OLED_WIDTH, OLED_HEIGHT, PixelFormat::Mono1, now_us);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        info!("crossfire: sys {} Hz; host stack on core 0, console on the UART", clocks.sys_hz);
        // the host stack, on THIS core -- see the shell
        let host = UsbMidiHost::init();
        info!("USB host stack up: {} MIDI slots, hub aware", USB_SLOTS);

        static USB_MOD: StaticCell<UsbMod> = StaticCell::new();
        let usb_mod = USB_MOD.init(UsbMod { host, forwarder: Forwarder::new(), events: EVENTS.subscribe().expect("slot"), reset_pending: false, auto_reset: true, packets: 0, status: Status::default() });
        static OLED_MOD: StaticCell<OledMod> = StaticCell::new();
        let oled_mod = OLED_MOD.init(OledMod { display, layer, font, events: EVENTS.subscribe().expect("slot"), status: Status::default(), dirty: true, indicators_only: false });
        let mut led_mod = LedMod { led: p.led, events: EVENTS.subscribe().expect("slot") };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };
        let _ = (p.key0, p.key1, HUB_PORT_NONE);

        let mut rt: Runtime<4> = Runtime::new();
        rt.add(usb_mod).expect("capacity");
        rt.add(oled_mod).expect("capacity");
        rt.add(&mut led_mod).expect("capacity");
        rt.add(&mut console_mod).expect("capacity");
        rt.start().expect("start");
        info!("runtime started; plug an instrument in");
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

struct StackString<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> StackString<N> {
        const fn new() -> Self {
                Self { buf: [0; N], len: 0 }
        }
        fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
        }
}

impl<const N: usize> Write for StackString<N> {
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
        let mut msg = StackString::<160>::new();
        let _ = write!(msg, "{info}");
        unsafe { light_shell_panic(msg.buf.as_ptr(), msg.len) }
}
