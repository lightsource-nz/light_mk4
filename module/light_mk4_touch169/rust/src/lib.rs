//! The Rust side of the touch169 firmware.
//!
//! The C shell (`module/light_mk4_shell`) brings the pico-sdk runtime up, puts TinyUSB on
//! core 1, and calls `light_app_main` on core 0 with the clocks it configured; it never returns.
//! Core 1 calls `light_app_core1_service` from its USB loop. Everything the shell provides to
//! Rust is declared in the one `extern` block below.
//!
//! The application is a set of modules over one event bus: the console parses lines into
//! events, the touch driver publishes touches, and every module subscribes and matches on what
//! it cares about. The board's peripherals are taken once as an owned set and handed to the
//! drivers that need them. Drawing goes through the frame layer: every frame is a full repaint
//! into the back buffer, and only what changed reaches the panel.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_core::cst816t::{self, Cst816t};
use light_core::imu::{Imu, Orientation};
use light_core::qmi8658::Qmi8658;
use light_core::st7789::St7789;
use light_core::touch::{Gesture, Swipe, Tracker};
use light_core::{info, log, warn, Display, EventBus, Flip, FrameLayer, LineReader, LogicalRegion, Mailbox, Module, PixelFormat, Point, Poll, Region, Rotation, Runtime, Subscription, UpdateError};
use light_font::Font;
use light_rp2350::boards::touch169::*;
use light_rp2350::gpio::{Input, Output};
use light_rp2350::i2c::I2c1;
use light_rp2350::spi::Spi1Display;
use light_rp2350::{Breathe, Clocks, SysClock};

unsafe extern "C" {
        /// Hands a Rust panic to the shell, which prints it from the core that owns USB and
        /// reboots into BOOTSEL. Never returns.
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        /// Prints one line on the shell's stdio. Core 1 only: the log sink, and nothing else's.
        fn light_shell_log(msg: *const u8, len: usize);
        /// One byte of console input, or -1. Core 1 only.
        fn light_shell_read_byte() -> i32;
}

/// What the shell hands over: the clocks its runtime configured.
#[repr(C)]
pub struct ShellInfo {
        clk_sys_hz: u32,
        clk_peri_hz: u32,
}

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);

/// Two frame buffers, 134 KB each, in .bss: the panel is pushed from one while the next frame
/// is drawn into the other. Handed out exactly once, in `light_app_main`.
static mut FRAME_FRONT: [u8; FRAME_BYTES] = [0; FRAME_BYTES];
static mut FRAME_BACK: [u8; FRAME_BYTES] = [0; FRAME_BYTES];

/// The demo's font, rendered by crush at build time and handed over as a path by
/// `light_mk4_add_font` in the CMake -- a blob in flash, parsed in place, no generated C.
static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));

// --- the event bus --------------------------------------------------------------------------

/// Everything that happens in this application, as one type. The console is one producer of
/// `Command`s; a test, a boot script or a UI would be others, without going through text.
#[derive(Clone, Copy, Debug)]
enum AppEvent {
        Touch(cst816t::Event),
        /// A swipe, in the panel's own coordinate space.
        Gesture(Gesture),
        /// The board's settled orientation changed.
        Orientation(Orientation),
        Command(Command),
}

#[derive(Clone, Copy, Debug)]
enum Command {
        Stats,
        Backlight(bool),
        Square { x: u16, y: u16 },
        Speed { dx: i32, dy: i32 },
}

/// 16 events deep, 5 subscribers: display, touch, imu, board, and one spare.
static EVENTS: EventBus<AppEvent, 16, 5> = EventBus::new();
/// Raw console bytes, core 1 → core 0. Sized for a burst of pasted text.
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

// --- core 1 --------------------------------------------------------------------------------

fn log_sink(record: &log::Record) {
        let mut line = StackString::<160>::new();
        let _ = write!(line, "{record}");
        unsafe { light_shell_log(line.buf.as_ptr(), line.len) }
}

/// Called by the C shell from core 1's USB loop, between `tud_task()` calls. Moves a bounded
/// amount of log to stdio and a bounded amount of console input into the mailbox, so a burst of
/// either cannot starve `tud_task()`.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_service() {
        log::drain(4, log_sink);
        for _ in 0..32 {
                let b = unsafe { light_shell_read_byte() };
                if b < 0 {
                        break;
                }
                //   a full mailbox drops the byte; the line it belonged to will fail to parse
                // and say so, which beats blocking the core that owns USB
                let _ = CONSOLE_BYTES.push(b as u8);
        }
}

// --- the modules --------------------------------------------------------------------------

const BG: u16 = 0x0000;
const FG: u16 = 0xF800; // red, RGB565
const TEXT: u16 = 0xFFFF;
const SQUARE: i32 = 24;
const CAPTION: Point = Point::new(8, 8);
const FPS: u32 = 30;

/// Owns the panel. Every frame repaints the caption and the square; the frame layer works out
/// which panel pixels changed -- the square's old and new places, the caption when its text
/// changes -- and pushes only those. A tap moves the square.
struct DisplayMod {
        display: Display<'static, St7789<Spi1Display>>,
        layer: FrameLayer,
        font: Font<'static>,
        events: Subscription,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        caption: StackString<32>,
        caption_second: u64,
        /// Timing, for `stats`: the longest draw (frame_begin to frame_end) and the longest
        /// push (frame_end until the panel is idle) seen, in microseconds.
        draw_us_max: u64,
        push_us_max: u64,
        push_started_us: Option<u64>,
}

impl DisplayMod {
        fn square(&self) -> LogicalRegion {
                LogicalRegion::new(self.x, self.y, self.x + SQUARE - 1, self.y + SQUARE - 1)
        }

        fn handle(&mut self, ev: AppEvent) {
                match ev {
                        AppEvent::Touch(cst816t::Event::Down { x, y }) => {
                                // the panel reports in its own frame; the square lives in the
                                // canvas's, which the layer's orientation defines
                                let p = self.layer.untransform_point(i32::from(x), i32::from(y));
                                self.place(p.x, p.y);
                        }
                        AppEvent::Command(Command::Square { x, y }) => self.place(i32::from(x), i32::from(y)),
                        AppEvent::Command(Command::Speed { dx, dy }) => {
                                self.dx = dx;
                                self.dy = dy;
                        }
                        AppEvent::Gesture(g) => {
                                // a swipe sends the square that way, in the panel's frame
                                let speed = self.dx.abs().max(self.dy.abs()).max(2);
                                let (dx, dy) = match g.swipe {
                                        Swipe::Left => (-speed, 0),
                                        Swipe::Right => (speed, 0),
                                        Swipe::Up => (0, -speed),
                                        Swipe::Down => (0, speed),
                                };
                                let t = self.layer.transform();
                                // rotate the panel-frame direction into the canvas frame: the
                                // transform's inverse, applied to a vector (no translation)
                                let det = t.a * t.d - t.b * t.c;
                                self.dx = (t.d * dx - t.b * dy) * det;
                                self.dy = (t.a * dy - t.c * dx) * det;
                                info!("swipe {:?} ({}): square now moving ({}, {})", g.swipe, if g.from_hardware { "hw" } else { "sw" }, self.dx, self.dy);
                        }
                        AppEvent::Orientation(o) => {
                                let rotation = match o {
                                        Orientation::Portrait => Some(Rotation::R0),
                                        Orientation::PortraitFlip => Some(Rotation::R180),
                                        // derived by mk3 and confirmed on this board: L is 270, R is 90
                                        Orientation::LandscapeL => Some(Rotation::R270),
                                        Orientation::LandscapeR => Some(Rotation::R90),
                                        // flat has no upright; keep whatever we had
                                        _ => None,
                                };
                                if let Some(r) = rotation {
                                        self.layer.set_orientation(r, Flip::None);
                                        self.layer.invalidate_all();
                                        let (w, h) = self.layer.logical_size();
                                        self.x = self.x.clamp(0, i32::from(w) - SQUARE);
                                        self.y = self.y.clamp(0, i32::from(h) - SQUARE);
                                        info!("orientation {o:?}: canvas now {}x{}", w, h);
                                }
                        }
                        AppEvent::Command(Command::Stats) => {
                                info!(
                                        "display: {} frames, {} skipped, {} chunk timeouts; max draw {} us, max push {} us",
                                        self.layer.frames(),
                                        self.layer.skipped,
                                        self.display.timeouts,
                                        self.draw_us_max,
                                        self.push_us_max
                                );
                                self.draw_us_max = 0;
                                self.push_us_max = 0;
                        }
                        _ => {}
                }
        }

        fn place(&mut self, x: i32, y: i32) {
                let (w, h) = self.layer.logical_size();
                self.x = (x - SQUARE / 2).clamp(0, i32::from(w) - SQUARE);
                self.y = (y - SQUARE / 2).clamp(0, i32::from(h) - SQUARE);
        }

        fn step(&mut self) {
                let (w, h) = self.layer.logical_size();
                let (w, h) = (i32::from(w), i32::from(h));
                let top = CAPTION.y + i32::from(self.font.cell_height()) + 4;
                self.x += self.dx;
                self.y += self.dy;
                if self.x <= 0 || self.x >= w - SQUARE {
                        self.dx = -self.dx;
                        self.x = self.x.clamp(0, w - SQUARE);
                }
                if self.y <= top || self.y >= h - SQUARE {
                        self.dy = -self.dy;
                        self.y = self.y.clamp(top, h - SQUARE);
                }
        }

        /// One frame: full repaint, invalidate what is wrong on the panel.
        fn frame(&mut self, now_us: u64) -> bool {
                let second = now_us / 1_000_000;
                let caption_changed = second != self.caption_second;
                if caption_changed {
                        self.caption_second = second;
                        self.caption = StackString::new();
                        let _ = write!(self.caption, "mk4 {}s {}f", second, self.layer.frames());
                }
                self.step();
                let square = self.square();
                let font = self.font;
                let Some(mut c) = self.layer.frame_begin(&mut self.display, now_us) else { return false };
                c.fg = TEXT;
                let caption_box = c.text(&font, CAPTION, self.caption.as_str());
                c.fill_region(&Region::new(square.x0 as u16, square.y0 as u16, square.x1 as u16, square.y1 as u16), FG);
                drop(c);
                self.layer.invalidate(square);
                if caption_changed {
                        if let Some(r) = caption_box {
                                self.layer.invalidate(r.into());
                        }
                }
                self.layer.frame_end();
                let done = light_rp2350::now_us();
                self.draw_us_max = self.draw_us_max.max(done - now_us);
                self.push_started_us = Some(done);
                true
        }
}

impl Module for DisplayMod {
        fn name(&self) -> &'static str {
                "display"
        }
        fn load(&mut self) -> Result<(), ()> {
                let mut clock = SysClock;
                self.display.init(&mut clock);
                self.display.driver().set_offset(0, DISPLAY_ROW_OFFSET);
                self.display.driver().clear(BG);
                self.layer.set_frame_rate(FPS);
                self.layer.bg = BG;
                // the first frame: nothing on the panel corresponds to what is drawn
                self.layer.invalidate_all();
                self.frame(light_rp2350::now_us());
                info!(
                        "display up: {}x{}, double-buffered at {} fps, font {}px cell {}x{} ({} glyphs, {} bytes)",
                        DISPLAY_WIDTH,
                        DISPLAY_HEIGHT,
                        FPS,
                        self.font.pixel_size(),
                        self.font.cell_width(),
                        self.font.cell_height(),
                        self.font.glyph_count(),
                        FONT_BLOB.len()
                );
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.layer.poll(&mut self.display) {
                        Ok(_) => {}
                        Err(UpdateError::Timeout) => warn!("display chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                if let Some(started) = self.push_started_us {
                        if !self.layer.busy(&self.display) {
                                self.push_us_max = self.push_us_max.max(light_rp2350::now_us() - started);
                                self.push_started_us = None;
                        }
                }
                while let Some(ev) = EVENTS.poll(&self.events) {
                        self.handle(ev);
                }
                if self.frame(light_rp2350::now_us()) || self.layer.busy(&self.display) { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.display.driver().clear(BG);
                info!("display down");
        }
}

/// Owns the touch controller and publishes what it reports: samples, and the swipes the
/// tracker makes of them.
struct TouchMod {
        touch: Cst816t<&'static RefCell<I2c1>, Input, Output>,
        tracker: Tracker,
        events: Subscription,
}

impl Module for TouchMod {
        fn name(&self) -> &'static str {
                "touch"
        }
        fn load(&mut self) -> Result<(), ()> {
                //   reset immediately before the probe: the controller auto-sleeps within about
                // a second of being left alone
                self.touch.reset_blocking(&mut SysClock);
                match self.touch.probe() {
                        Ok(Some(id)) => info!("cst816t chip id confirmed: 0x{id:02x}"),
                        Ok(None) => warn!("cst816t answered with an unexpected chip id"),
                        Err(e) => warn!("cst816t did not answer the chip id read: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::Command(Command::Stats) = ev {
                                info!(
                                        "touch: {} failed reads ({} nack, {} timeout, {} bus), {} resets",
                                        self.touch.failures,
                                        self.touch.nacks,
                                        self.touch.timeouts,
                                        self.touch.bus_errors,
                                        self.touch.recoveries
                                );
                        }
                }
                let now_ms = (light_rp2350::now_us() / 1000) as u32;
                let Some(ev) = self.touch.poll(now_ms) else { return Poll::Idle };
                match ev {
                        cst816t::Event::Down { x, y } => info!("touch down at {x},{y}"),
                        cst816t::Event::Up => info!("touch up"),
                        cst816t::Event::Reset => match self.touch.probe() {
                                Ok(_) => info!("touch controller reset ({} so far); answering again", self.touch.recoveries),
                                Err(e) => warn!("touch controller reset ({} so far); still not answering: {e:?}", self.touch.recoveries),
                        },
                        cst816t::Event::Move { .. } => {}
                }
                if let Err(e) = EVENTS.publish(AppEvent::Touch(ev)) {
                        warn!("event bus full; dropped {e:?}");
                }
                if let Some(g) = self.tracker.feed(ev, Some(&mut self.touch)) {
                        let _ = EVENTS.publish(AppEvent::Gesture(g));
                }
                Poll::Busy
        }
}

/// Owns the IMU: publishes orientation changes, answers `stats` with the current vector.
struct ImuMod {
        imu: Imu<Qmi8658<&'static RefCell<I2c1>>>,
        events: Subscription,
}

impl Module for ImuMod {
        fn name(&self) -> &'static str {
                "imu"
        }
        fn load(&mut self) -> Result<(), ()> {
                match self.imu.driver().probe() {
                        Ok(Some(id)) => info!("qmi8658 chip id confirmed: 0x{id:02x}"),
                        Ok(None) => warn!("qmi8658 answered with an unexpected chip id"),
                        Err(e) => warn!("qmi8658 did not answer the chip id read: {e:?}"),
                }
                if let Err(e) = self.imu.driver().configure() {
                        warn!("qmi8658 configuration failed: {e:?}");
                }
                self.imu.set_axis_map(IMU_AXIS_MAP);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::Command(Command::Stats) = ev {
                                let a = self.imu.accel_mg;
                                info!("imu: accel {} {} {} mg, {:?}, {} failed reads, {}.{} C", a[0], a[1], a[2], self.imu.orientation, self.imu.failures, self.imu.temperature_mc / 1000, (self.imu.temperature_mc % 1000).abs() / 100);
                        }
                }
                let now_ms = (light_rp2350::now_us() / 1000) as u32;
                if !self.imu.poll(now_ms) {
                        return Poll::Idle;
                }
                if let Some(o) = self.imu.take_orientation() {
                        info!("orientation: {o:?}");
                        let _ = EVENTS.publish(AppEvent::Orientation(o));
                }
                Poll::Busy
        }
}

/// Owns the backlight.
struct BoardMod {
        backlight: Output,
        events: Subscription,
}

impl Module for BoardMod {
        fn name(&self) -> &'static str {
                "board"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.backlight.set(true);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::Command(Command::Backlight(on)) = ev {
                                busy = true;
                                self.backlight.set(on);
                                info!("backlight {}", if on { "on" } else { "off" });
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.backlight.set(false);
        }
}

/// The console: bytes from core 1 into lines, lines into commands, commands onto the bus. The
/// string front-end of the event bus; `help`, `loglevel` and `quit` are its own.
struct ConsoleMod {
        reader: LineReader<96>,
}

impl ConsoleMod {
        fn dispatch(&mut self, line: &str) -> Poll {
                info!("> {line}");
                let mut words = line.split_whitespace();
                let Some(cmd) = words.next() else { return Poll::Idle };
                let mut args = words;
                let mut num = |what: &str| -> Option<i32> { args.next().and_then(|s| s.parse().ok()).or_else(|| { warn!("usage: {what}"); None }) };
                let event = match cmd {
                        "help" => {
                                info!("commands: help | stats | backlight on|off | square X Y | speed DX DY | loglevel error|warn|info|debug|trace | quit");
                                None
                        }
                        "stats" => {
                                info!("console: {} bytes dropped, {} lines dropped; bus: {} refused, {} backlog", CONSOLE_BYTES.dropped(), self.reader.dropped_lines, EVENTS.refused(), EVENTS.backlog());
                                Some(Command::Stats)
                        }
                        "backlight" => match args.next() {
                                Some("on") => Some(Command::Backlight(true)),
                                Some("off") => Some(Command::Backlight(false)),
                                _ => {
                                        warn!("usage: backlight on|off");
                                        None
                                }
                        },
                        "square" => match (num("square X Y"), num("square X Y")) {
                                (Some(x), Some(y)) => Some(Command::Square { x: x.clamp(0, i32::from(DISPLAY_WIDTH)) as u16, y: y.clamp(0, i32::from(DISPLAY_HEIGHT)) as u16 }),
                                _ => None,
                        },
                        "speed" => match (num("speed DX DY"), num("speed DX DY")) {
                                (Some(dx), Some(dy)) => Some(Command::Speed { dx, dy }),
                                _ => None,
                        },
                        "loglevel" => {
                                let level = match args.next() {
                                        Some("error") => Some(log::Level::Error),
                                        Some("warn") => Some(log::Level::Warn),
                                        Some("info") => Some(log::Level::Info),
                                        Some("debug") => Some(log::Level::Debug),
                                        Some("trace") => Some(log::Level::Trace),
                                        _ => None,
                                };
                                match level {
                                        Some(l) => {
                                                log::set_max_level(l);
                                                info!("log level {}", l.as_str());
                                        }
                                        None => warn!("usage: loglevel error|warn|info|debug|trace"),
                                }
                                None
                        }
                        "quit" => {
                                info!("shutting down");
                                return Poll::Shutdown;
                        }
                        other => {
                                warn!("unknown command '{other}' -- try help");
                                None
                        }
                };
                if let Some(c) = event {
                        if let Err(e) = EVENTS.publish(AppEvent::Command(c)) {
                                warn!("event bus full; dropped {e:?}");
                        }
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

// --- entry ----------------------------------------------------------------------------------

/// Entry point called by the C shell on core 0 once the runtime is up.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        log::set_clock(light_rp2350::now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, peri {} Hz; spi1 at {} Hz, i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.display_bus.actual_hz, p.touch_bus.actual_hz);

        // SAFETY: the one and only references to the frame buffers, taken before anything can
        // alias them
        let front: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(FRAME_FRONT) };
        let back: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(FRAME_BACK) };
        let mut display = Display::new(St7789::new(p.display_bus), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2350::now_us);
        display.set_back_buffer(back);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        //   one I2C bus, two drivers: shared through a RefCell that lives as long as the
        // application, which on a firmware that never returns is a static's lifetime
        static I2C: static_cell::StaticCell<RefCell<I2c1>> = static_cell::StaticCell::new();
        let i2c: &'static RefCell<I2c1> = I2C.init(RefCell::new(p.touch_bus));
        let touch = Cst816t::new(i2c, p.touch_int, p.touch_reset, (light_rp2350::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(i2c));

        let mut board_mod = BoardMod { backlight: p.backlight, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut imu_mod = ImuMod { imu, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut display_mod = DisplayMod {
                display,
                layer: FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565),
                font,
                events: EVENTS.subscribe().expect("subscriber slot"),
                x: 40,
                y: 60,
                dx: 3,
                dy: 2,
                caption: StackString::new(),
                caption_second: u64::MAX,
                draw_us_max: 0,
                push_us_max: 0,
                push_started_us: None,
        };
        let mut touch_mod = TouchMod { touch, tracker: Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), events: EVENTS.subscribe().expect("subscriber slot") };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<5> = Runtime::new();
        rt.add(&mut board_mod).expect("capacity");
        rt.add(&mut display_mod).expect("capacity");
        rt.add(&mut touch_mod).expect("capacity");
        rt.add(&mut imu_mod).expect("capacity");
        rt.add(&mut console_mod).expect("capacity");
        rt.start().expect("start");
        info!("runtime started; type 'help' on the console");
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

/// A fixed-capacity string for formatting a line without an allocator.
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
                // truncating is the right failure for a line; report success so the formatter
                // keeps going rather than abandoning the message at the first overflow
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
