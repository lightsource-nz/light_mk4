//! The Rust side of the touch169 spike firmware.
//!
//! The C shell (`../src/main.c`) brings the pico-sdk runtime up, puts TinyUSB on core 1, and
//! calls `light_app_main` on core 0, which never returns. Core 1 calls `light_app_core1_service`
//! from its USB loop. Everything the shell provides to Rust is declared in the one `extern`
//! block below, so the size of the FFI surface -- one of the things the spike measures -- can be
//! read off this file.
//!
//! Milestone 4: a console. Core 1 reads bytes from the CDC port and drains the log queue; core 0
//! turns the bytes into lines, the lines into commands, and the commands into events in typed
//! mailboxes that the display, touch and board modules consume. The mailboxes replace the
//! ad-hoc static the touch module used to reach the display through.

#![no_std]

use core::fmt::Write;
use light_core::cst816t::{Cst816t, Event};
use light_core::draw::Rgb565;
use light_core::st7789::St7789;
use light_core::{info, log, warn, Board, Display, LineReader, Mailbox, Module, Poll, Region, Runtime, UpdateError};
use light_font::Font;
use light_rp2350::gpio::{Input, Output};
use light_rp2350::i2c::I2c1;
use light_rp2350::spi::Spi1Display;
use light_rp2350::touch169::*;
use light_rp2350::{Backlight, SysClock};

unsafe extern "C" {
        /// Hands a Rust panic to the shell, which prints it from the core that owns USB and
        /// reboots into BOOTSEL. Never returns.
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        /// Prints one line on the shell's stdio. Core 1 only: the log sink, and nothing else's.
        fn light_shell_log(msg: *const u8, len: usize);
        /// One byte of console input, or -1. Core 1 only.
        fn light_shell_read_byte() -> i32;
}

const BYTES_PER_PIXEL: usize = 2;
const FRAME_BYTES: usize = DISPLAY_WIDTH as usize * DISPLAY_HEIGHT as usize * BYTES_PER_PIXEL;

/// The frame buffer: 134 KB, so it lives in .bss rather than on the 2 KB main stack. Handed
/// out exactly once, in `light_app_main`.
static mut FRAME: [u8; FRAME_BYTES] = [0; FRAME_BYTES];

/// The demo's font, rendered by crush at build time and handed over as a path by
/// `light_mk4_add_font` in the CMake -- a blob in flash, parsed in place, no generated C.
static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));

// --- the mailboxes: how modules, and core 1, reach each other ---------------------------------

#[derive(Clone, Copy, Debug)]
enum DisplayEvent {
        MoveTo { x: u16, y: u16 },
        Speed { dx: i32, dy: i32 },
        ReportStats,
}

#[derive(Clone, Copy, Debug)]
enum TouchEvent {
        ReportStats,
}

#[derive(Clone, Copy, Debug)]
enum BoardEvent {
        Backlight(bool),
}

static DISPLAY_EVENTS: Mailbox<DisplayEvent, 8> = Mailbox::new();
static TOUCH_EVENTS: Mailbox<TouchEvent, 4> = Mailbox::new();
static BOARD_EVENTS: Mailbox<BoardEvent, 4> = Mailbox::new();
/// Raw console bytes, core 1 → core 0. Sized for a burst of pasted text.
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

// --- core 1 --------------------------------------------------------------------------------

fn log_sink(record: &log::Record) {
        let mut line = StackBuf::<160> { buf: [0; 160], len: 0 };
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
const SQUARE: u16 = 24;
const FRAME_INTERVAL_US: u64 = 33_333;
/// Where the caption sits; the square keeps below it.
const CAPTION_X: u16 = 8;
const CAPTION_Y: u16 = 8;

fn fill_rect(buf: &mut [u8], width: u16, r: &Region, color: u16) {
        Rgb565 { buf, width, height: DISPLAY_HEIGHT }.fill(r, color);
}

/// Owns the panel. Paints a bouncing square with region updates, honouring the rule that a
/// region must cover what was drawn before as well as what is drawn now -- and a caption in
/// the build-time font, redrawn once a second.
struct DisplayMod {
        display: Display<'static, St7789<Spi1Display>>,
        font: Font<'static>,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        prev: Option<Region>,
        next_frame_us: u64,
        next_caption_us: u64,
        frames: u32,
        /// A caption waiting to be pushed with the next region update.
        caption_dirty: Option<Region>,
}

impl DisplayMod {
        fn square(&self) -> Region {
                Region::new(self.x as u16, self.y as u16, self.x as u16 + SQUARE - 1, self.y as u16 + SQUARE - 1)
        }

        fn handle(&mut self, ev: DisplayEvent) {
                match ev {
                        DisplayEvent::MoveTo { x, y } => {
                                self.x = (i32::from(x) - i32::from(SQUARE) / 2).clamp(0, i32::from(DISPLAY_WIDTH - SQUARE));
                                self.y = (i32::from(y) - i32::from(SQUARE) / 2).clamp(0, i32::from(DISPLAY_HEIGHT - SQUARE));
                        }
                        DisplayEvent::Speed { dx, dy } => {
                                self.dx = dx;
                                self.dy = dy;
                        }
                        DisplayEvent::ReportStats => {
                                info!("display: {} frames, {} chunk timeouts, {} events dropped", self.frames, self.display.timeouts, DISPLAY_EVENTS.dropped());
                        }
                }
        }

        fn step(&mut self) {
                let top = i32::from(CAPTION_Y) + i32::from(self.font.cell_height()) + 4;
                self.x += self.dx;
                self.y += self.dy;
                if self.x <= 0 || self.x >= i32::from(DISPLAY_WIDTH - SQUARE) {
                        self.dx = -self.dx;
                        self.x = self.x.clamp(0, i32::from(DISPLAY_WIDTH - SQUARE));
                }
                if self.y <= top || self.y >= i32::from(DISPLAY_HEIGHT - SQUARE) {
                        self.dy = -self.dy;
                        self.y = self.y.clamp(top, i32::from(DISPLAY_HEIGHT - SQUARE));
                }
        }

        /// Draws the caption into the frame and records its box for the next push.
        fn draw_caption(&mut self, now_us: u64) {
                let Some(frame) = self.display.frame_mut() else { return };
                let mut fb = Rgb565 { buf: frame, width: DISPLAY_WIDTH, height: DISPLAY_HEIGHT };
                let mut text = heapless_string::<32>();
                let _ = write!(text, "mk4 {}s {}f", now_us / 1_000_000, self.frames);
                let font = self.font;
                if let Some(r) = fb.text(&font, CAPTION_X, CAPTION_Y, text.as_str(), TEXT, BG) {
                        self.caption_dirty = Some(match self.caption_dirty {
                                Some(d) => d.union(&r),
                                None => r,
                        });
                }
        }
}

/// A fixed-capacity string for formatting a line without an allocator.
fn heapless_string<const N: usize>() -> StackString<N> {
        StackString { buf: [0; N], len: 0 }
}

struct StackString<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> StackString<N> {
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

impl Module for DisplayMod {
        fn name(&self) -> &'static str {
                "display"
        }
        fn load(&mut self) -> Result<(), ()> {
                let mut clock = SysClock;
                self.display.init(&mut clock);
                self.display.driver().set_offset(0, DISPLAY_ROW_OFFSET);
                // after the offset, so the band the offset moves the window over is blanked too
                self.display.driver().clear(BG);
                let square = self.square();
                let frame = self.display.frame_mut().ok_or(())?;
                fill_rect(frame, DISPLAY_WIDTH, &Region::full(DISPLAY_WIDTH, DISPLAY_HEIGHT), BG);
                fill_rect(frame, DISPLAY_WIDTH, &square, FG);
                self.draw_caption(light_rp2350::now_us());
                self.caption_dirty = None;
                // the first push is the whole frame: one full-width chunk, the yield-per-poll path
                self.display.update_async(Region::full(DISPLAY_WIDTH, DISPLAY_HEIGHT)).map_err(|_| ())?;
                self.prev = Some(square);
                info!(
                        "display up: {}x{}, font {}px cell {}x{} ({} glyphs, {} bytes), first frame in flight",
                        DISPLAY_WIDTH,
                        DISPLAY_HEIGHT,
                        self.font.pixel_size(),
                        self.font.cell_width(),
                        self.font.cell_height(),
                        self.font.glyph_count(),
                        FONT_BLOB.len()
                );
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.display.poll() {
                        Ok(true) => return Poll::Busy,
                        Ok(false) => {}
                        Err(UpdateError::Timeout) => warn!("display chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                while let Some(ev) = DISPLAY_EVENTS.pop() {
                        self.handle(ev);
                }
                let now = light_rp2350::now_us();
                if now < self.next_frame_us {
                        return Poll::Idle;
                }
                self.next_frame_us = now + FRAME_INTERVAL_US;
                if now >= self.next_caption_us {
                        self.next_caption_us = now + 1_000_000;
                        self.draw_caption(now);
                }
                let old = self.prev.unwrap_or_else(|| self.square());
                self.step();
                let new = self.square();
                let Some(frame) = self.display.frame_mut() else { return Poll::Busy };
                fill_rect(frame, DISPLAY_WIDTH, &old, BG);
                fill_rect(frame, DISPLAY_WIDTH, &new, FG);
                // union of where it was and where it is: a narrow region, so row-chunked with
                // the spin budget -- the other path through the chunk protocol. a fresh caption
                // joins the region the second it is drawn
                let mut region = old.union(&new);
                if let Some(c) = self.caption_dirty.take() {
                        region = region.union(&c);
                }
                if self.display.update_async(region).is_ok() {
                        self.prev = Some(new);
                        self.frames += 1;
                }
                Poll::Busy
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.display.driver().clear(BG);
                info!("display down");
        }
}

/// Owns the touch controller; a tap moves the square, through the display's mailbox.
struct TouchMod {
        touch: Cst816t<I2c1, Input, Output>,
}

impl Module for TouchMod {
        fn name(&self) -> &'static str {
                "touch"
        }
        fn load(&mut self) -> Result<(), ()> {
                //   reset immediately before the probe: the controller auto-sleeps within about
                // a second of being left alone, and anything that runs between the pulse and
                // the first read -- the display's init did, at first -- can use that second up
                self.touch.reset_blocking(&mut SysClock);
                match self.touch.probe() {
                        Ok(Some(id)) => info!("cst816t chip id confirmed: 0x{id:02x}"),
                        Ok(None) => warn!("cst816t answered with an unexpected chip id"),
                        Err(e) => warn!("cst816t did not answer the chip id read: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = TOUCH_EVENTS.pop() {
                        match ev {
                                TouchEvent::ReportStats => info!(
                                        "touch: {} failed reads ({} nack, {} timeout, {} bus), {} resets",
                                        self.touch.failures,
                                        self.touch.nacks,
                                        self.touch.timeouts,
                                        self.touch.bus_errors,
                                        self.touch.recoveries
                                ),
                        }
                }
                let now = light_rp2350::now_us();
                match self.touch.poll((now / 1000) as u32) {
                        Some(Event::Down { x, y }) => {
                                info!("touch down at {x},{y}");
                                let _ = DISPLAY_EVENTS.push(DisplayEvent::MoveTo { x, y });
                                Poll::Busy
                        }
                        Some(Event::Up) => {
                                info!("touch up");
                                Poll::Busy
                        }
                        Some(Event::Move { .. }) => Poll::Busy,
                        Some(Event::Reset) => {
                                //   re-probe now, while it is freshly awake: the cheapest check
                                // that the reset actually brought it back
                                match self.touch.probe() {
                                        Ok(_) => info!("touch controller reset ({} so far); answering again", self.touch.recoveries),
                                        Err(e) => warn!("touch controller reset ({} so far); still not answering: {e:?}", self.touch.recoveries),
                                }
                                Poll::Busy
                        }
                        None => Poll::Idle,
                }
        }
}

/// Owns the backlight.
struct BoardMod {
        backlight: Backlight,
}

impl Module for BoardMod {
        fn name(&self) -> &'static str {
                "board"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.backlight.set_backlight(true);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = BOARD_EVENTS.pop() {
                        busy = true;
                        match ev {
                                BoardEvent::Backlight(on) => {
                                        self.backlight.set_backlight(on);
                                        info!("backlight {}", if on { "on" } else { "off" });
                                }
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.backlight.set_backlight(false);
        }
}

/// The console: bytes from core 1 into lines, lines into commands, commands into events. This
/// is the string front-end of what will become the typed event bus; UI, boot and tests would
/// inject the same events without going through text.
struct ConsoleMod {
        reader: LineReader<96>,
}

impl ConsoleMod {
        fn dispatch(&mut self, line: &str) -> Poll {
                info!("> {line}");
                let mut words = line.split_whitespace();
                let Some(cmd) = words.next() else { return Poll::Idle };
                let mut args = words;
                match cmd {
                        "help" => {
                                info!("commands: help | stats | backlight on|off | square X Y | speed DX DY | loglevel error|warn|info|debug|trace | quit");
                        }
                        "stats" => {
                                let _ = DISPLAY_EVENTS.push(DisplayEvent::ReportStats);
                                let _ = TOUCH_EVENTS.push(TouchEvent::ReportStats);
                                info!("console: {} bytes dropped, {} lines dropped", CONSOLE_BYTES.dropped(), self.reader.dropped_lines);
                        }
                        "backlight" => match args.next() {
                                Some("on") => {
                                        let _ = BOARD_EVENTS.push(BoardEvent::Backlight(true));
                                }
                                Some("off") => {
                                        let _ = BOARD_EVENTS.push(BoardEvent::Backlight(false));
                                }
                                _ => warn!("usage: backlight on|off"),
                        },
                        "square" => match (args.next().and_then(|s| s.parse().ok()), args.next().and_then(|s| s.parse().ok())) {
                                (Some(x), Some(y)) => {
                                        let _ = DISPLAY_EVENTS.push(DisplayEvent::MoveTo { x, y });
                                }
                                _ => warn!("usage: square X Y"),
                        },
                        "speed" => match (args.next().and_then(|s| s.parse().ok()), args.next().and_then(|s| s.parse().ok())) {
                                (Some(dx), Some(dy)) => {
                                        let _ = DISPLAY_EVENTS.push(DisplayEvent::Speed { dx, dy });
                                }
                                _ => warn!("usage: speed DX DY"),
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
                        }
                        "quit" => {
                                info!("shutting down");
                                return Poll::Shutdown;
                        }
                        other => warn!("unknown command '{other}' -- try help"),
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
pub extern "C" fn light_app_main() -> ! {
        log::set_clock(light_rp2350::now_us);

        // SAFETY: each peripheral is constructed exactly once, here, and the shell touches none
        // of them after handing over
        let backlight = unsafe { Backlight::new() };
        let spi = unsafe {
                Spi1Display::new(
                        PIN_DISPLAY_SCK,
                        PIN_DISPLAY_MOSI,
                        PIN_DISPLAY_CS,
                        PIN_DISPLAY_DC,
                        Some(PIN_DISPLAY_RESET),
                        DISPLAY_SPI_HZ,
                        DISPLAY_DMA_CH,
                )
        };
        info!("spi1 at {} Hz", spi.actual_hz);
        // SAFETY: the one and only reference to FRAME, taken before anything can alias it
        let frame: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(FRAME) };
        let display = Display::new(St7789::new(spi), frame, DISPLAY_WIDTH, DISPLAY_HEIGHT, BYTES_PER_PIXEL as u16, light_rp2350::now_us);

        let i2c = unsafe { I2c1::new(PIN_TOUCH_SCL, PIN_TOUCH_SDA, TOUCH_I2C_HZ) };
        info!("i2c1 at {} Hz", i2c.actual_hz);
        let int = Input::new_pull_up(PIN_TOUCH_INT);
        let reset = Output::new(PIN_TOUCH_RST, true);
        let touch = Cst816t::new(i2c, int, reset, (light_rp2350::now_us() / 1000) as u32);

        let mut board_mod = BoardMod { backlight };
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        let mut display_mod = DisplayMod {
                display,
                font,
                x: 40,
                y: 60,
                dx: 3,
                dy: 2,
                prev: None,
                next_frame_us: 0,
                next_caption_us: 0,
                frames: 0,
                caption_dirty: None,
        };
        let mut touch_mod = TouchMod { touch };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<4> = Runtime::new();
        rt.add(&mut board_mod).expect("capacity");
        rt.add(&mut display_mod).expect("capacity");
        rt.add(&mut touch_mod).expect("capacity");
        rt.add(&mut console_mod).expect("capacity");
        rt.start().expect("start");
        info!("runtime started; type 'help' on the console");
        let result = rt.run(|| {});
        //   core 1 keeps draining the log, so the last words get out; core 0 has nothing left
        // to do and no stdio of its own
        match result {
                Ok(()) => info!("runtime stopped cleanly; core 0 idle"),
                Err(e) => warn!("runtime stopped with {e:?}; core 0 idle"),
        }
        loop {
                core::hint::spin_loop();
        }
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
