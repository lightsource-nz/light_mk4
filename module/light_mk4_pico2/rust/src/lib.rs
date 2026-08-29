//! The Rust side of the po13 rig firmware: a Pico 2 with the Pico-OLED-1.3. The LED blinks, the
//! OLED shows a caption in the build-time font and a bouncing square -- the same demo as the
//! touch169's, on a 1 bpp panel mounted sideways, which is what the rasteriser's rotation and
//! the SH1107's column addressing exist for -- and the console drives both.
//!
//! The shell (module/light_mk4_shell) is the same file the touch169 links.

#![no_std]

use core::fmt::Write;
use light_core::sh1107::Sh1107;
use light_core::{info, log, warn, Blinker, Canvas, Display, EventBus, LineReader, Mailbox, Module, PixelFormat, Point, Poll, Region, Rotation, Runtime, Subscription, UpdateError};
use light_font::Font;
use light_rp2350::boards::pico2::*;
use light_rp2350::gpio::Output;
use light_rp2350::spi::Spi1Display;
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
        Square { x: i32, y: i32 },
        Stats,
}

static EVENTS: EventBus<AppEvent, 8, 2> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

/// 64x128 at 1 bpp: one kilobyte.
static mut FRAME: [u8; PixelFormat::Mono1.buffer_len(OLED_WIDTH, OLED_HEIGHT)] = [0; PixelFormat::Mono1.buffer_len(OLED_WIDTH, OLED_HEIGHT)];
static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));

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
                                _ => {}
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

const SQUARE: i32 = 12;
const FRAME_INTERVAL_US: u64 = 50_000;
const CAPTION: Point = Point::new(2, 2);

/// The OLED, drawn on sideways: the glass is 64x128 portrait, the demo is 128x64 landscape, so
/// the canvas is rotated 90 degrees and every region the demo reports is mapped to physical
/// columns for the driver through the same transform.
struct OledMod {
        display: Display<'static, Sh1107<Spi1Display>>,
        font: Font<'static>,
        events: Subscription,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        prev: Option<Region>,
        next_frame_us: u64,
        next_caption_us: u64,
        frames: u32,
        caption_dirty: Option<Region>,
}

impl OledMod {
        fn canvas<'a>(frame: &'a mut [u8]) -> Canvas<'a> {
                let mut c = Canvas::new(frame, PixelFormat::Mono1, OLED_WIDTH, OLED_HEIGHT);
                c.set_rotation(Rotation::R90);
                c.fg = 1;
                c.bg = 0;
                c
        }

        fn square(&self) -> Region {
                Region::new(self.x as u16, self.y as u16, (self.x + SQUARE - 1) as u16, (self.y + SQUARE - 1) as u16)
        }

        fn step(&mut self, top: i32) {
                let (w, h) = (i32::from(OLED_HEIGHT), i32::from(OLED_WIDTH)); // logical, rotated
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
                let sq = self.square();
                let font = self.font;
                let frame = self.display.frame_mut().ok_or(())?;
                let mut c = Self::canvas(frame);
                c.clear();
                c.rect_rounded(Point::new(0, 0), Point::new(127, 63), 6, light_core::draw::corner::ALL, false);
                c.text(&font, CAPTION, "mk4 po13");
                c.fill_region(&sq, 1);
                self.prev = Some(sq);
                self.display.update_async(Region::full(OLED_WIDTH, OLED_HEIGHT)).map_err(|_| ())?;
                info!("oled up: {}x{} glass, {}x{} logical, font {}px cell {}x{}", OLED_WIDTH, OLED_HEIGHT, OLED_HEIGHT, OLED_WIDTH, self.font.pixel_size(), self.font.cell_width(), self.font.cell_height());
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.display.poll() {
                        Ok(true) => return Poll::Busy,
                        Ok(false) => {}
                        Err(UpdateError::Timeout) => warn!("oled chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Square { x, y } => {
                                        self.x = (x - SQUARE / 2).clamp(0, i32::from(OLED_HEIGHT) - SQUARE);
                                        self.y = (y - SQUARE / 2).clamp(0, i32::from(OLED_WIDTH) - SQUARE);
                                }
                                AppEvent::Stats => info!("oled: {} frames, {} chunk timeouts", self.frames, self.display.timeouts),
                                _ => {}
                        }
                }
                let now = now_us();
                if now < self.next_frame_us {
                        return Poll::Idle;
                }
                self.next_frame_us = now + FRAME_INTERVAL_US;
                let caption_due = now >= self.next_caption_us;
                if caption_due {
                        self.next_caption_us = now + 1_000_000;
                }
                let top = CAPTION.y + i32::from(self.font.cell_height()) + 2;
                let old = self.prev.unwrap_or_else(|| self.square());
                self.step(top);
                let new = self.square();
                let font = self.font;
                let frames = self.frames;
                let Some(frame) = self.display.frame_mut() else { return Poll::Busy };
                let mut c = Self::canvas(frame);
                c.fill_region(&old, 0);
                c.fill_region(&new, 1);
                let mut logical = old.union(&new);
                if caption_due {
                        let mut text = StackString::<24>::new();
                        let _ = write!(text, "mk4 {}s {}f", now / 1_000_000, frames);
                        if let Some(r) = c.text_boxed(&font, CAPTION, text.as_str()) {
                                logical = logical.union(&r);
                        }
                }
                // the driver addresses physical columns; the demo thinks in logical rows
                let physical = c.transform_rect(&logical);
                if self.display.update_async(physical).is_ok() {
                        self.prev = Some(new);
                        self.frames += 1;
                }
                let _ = self.caption_dirty.take();
                Poll::Busy
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.display.driver().clear(false);
        }
}

struct ConsoleMod {
        reader: LineReader<96>,
}

impl ConsoleMod {
        fn dispatch(&mut self, line: &str) -> Poll {
                info!("> {line}");
                let mut words = line.split_whitespace();
                let event = match (words.next(), words.next(), words.next()) {
                        (Some("help"), _, _) => {
                                info!("commands: help | stats | led on|off|blink | square X Y | quit");
                                None
                        }
                        (Some("stats"), _, _) => {
                                info!("uptime {} s; console: {} bytes dropped; bus: {} refused", now_us() / 1_000_000, CONSOLE_BYTES.dropped(), EVENTS.refused());
                                Some(AppEvent::Stats)
                        }
                        (Some("led"), Some("on"), _) => Some(AppEvent::LedOn),
                        (Some("led"), Some("off"), _) => Some(AppEvent::LedOff),
                        (Some("led"), Some("blink"), _) => Some(AppEvent::LedBlink),
                        (Some("square"), Some(x), Some(y)) => match (x.parse(), y.parse()) {
                                (Ok(x), Ok(y)) => Some(AppEvent::Square { x, y }),
                                _ => {
                                        warn!("usage: square X Y");
                                        None
                                }
                        },
                        (Some("quit"), _, _) => {
                                info!("shutting down");
                                return Poll::Shutdown;
                        }
                        (Some(other), _, _) => {
                                warn!("unknown command '{other}' -- try help");
                                None
                        }
                        (None, _, _) => None,
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
        let p = take(&clocks).expect("the board's peripherals are taken once");
        // SAFETY: the one and only reference to FRAME
        let frame: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(FRAME) };
        let display = Display::new(Sh1107::new(p.oled_bus), frame, OLED_WIDTH, OLED_HEIGHT, PixelFormat::Mono1, now_us);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        let mut led_mod = LedMod { led: p.led, blinker: Blinker::new(500_000), blinking: true, toggles: 0, events: EVENTS.subscribe().expect("slot") };
        let mut oled_mod = OledMod {
                display,
                font,
                events: EVENTS.subscribe().expect("slot"),
                x: 20,
                y: 30,
                dx: 2,
                dy: 1,
                prev: None,
                next_frame_us: 0,
                next_caption_us: 0,
                frames: 0,
                caption_dirty: None,
        };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<3> = Runtime::new();
        rt.add(&mut led_mod).expect("capacity");
        rt.add(&mut oled_mod).expect("capacity");
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
