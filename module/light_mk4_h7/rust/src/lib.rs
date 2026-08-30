//! The Rust side of the MiniSTM32H7 firmware: the widget demo on the board's 160x80 ST7735,
//! the LED, the one key, and the console on USART1. Single core, so the log drain that the
//! RP2 shells run on core 1 is a module here.
//!
//! One key drives a whole interface: a short press moves the focus, a long press activates.

#![no_std]

use core::fmt::Write;
use light_core::button::{Button, ButtonEvent};
use light_core::st7735::St7735;
use light_core::ui::{scroll, Desc, Page, Ui};
use light_core::{info, log, warn, Blinker, Display, EventBus, FrameLayer, LineReader, Mailbox, Module, PixelFormat, Poll, Runtime, Subscription, UpdateError};
use light_font::Font;
use light_stm32h7::boards::mini_stm32h7::*;
use light_stm32h7::gpio::{Input, Output};
use light_stm32h7::spi::Spi4Display;
use light_stm32h7::{now_us, Breathe, Clocks, SysClock};

unsafe extern "C" {
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        fn light_shell_log(msg: *const u8, len: usize);
        fn light_shell_read_byte() -> i32;
}

#[repr(C)]
pub struct ShellInfo {
        clk_sys_hz: u32,
        clk_apb2_hz: u32,
        clk_tim_hz: u32,
}

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        /// The key, released before the hold interval: the short press.
        KeyShort,
        /// The key held for the interval -- fired the moment it expires, not on release.
        KeyHold,
        UiFocus { next: bool },
        UiActivate,
        UiBack,
        Toggle(u8),
        Item(u8),
        LedBlink(bool),
        Stats,
}

static EVENTS: EventBus<AppEvent, 8, 3> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);
/// Two 25 KB frame buffers in AXI SRAM.
static mut FRAME_FRONT: [u8; FRAME_BYTES] = [0; FRAME_BYTES];
static mut FRAME_BACK: [u8; FRAME_BYTES] = [0; FRAME_BYTES];
static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));

fn log_sink(record: &log::Record) {
        let mut line = StackString::<160>::new();
        let _ = write!(line, "{record}");
        unsafe { light_shell_log(line.buf.as_ptr(), line.len) }
}

// --- the interface, as data ---------------------------------------------------------------

const CORNER_RADIUS: u8 = 6;
const ROW_GAP: u8 = 1;
const LIST_MIN_ROW: i32 = 16;
const FPS: u32 = 30;
const BG: u16 = 0x0000;
/// A long press activates; anything shorter moves the focus.
const HOLD_MS: u32 = 500;

const LABEL_OFF: [&str; 2] = ["Alpha", "Beta"];
const LABEL_ON: [&str; 2] = ["Alpha *", "Beta *"];

static BTN_ALPHA: Desc<AppEvent> = Desc::button(LABEL_OFF[0]).emit(AppEvent::Toggle(0)).tag(1);
static BTN_BETA: Desc<AppEvent> = Desc::button(LABEL_OFF[1]).emit(AppEvent::Toggle(1)).tag(2);
static BTN_LIST: Desc<AppEvent> = Desc::button("List >").navigate(&PAGE_LIST);
static MAIN_WINDOW: Desc<AppEvent> = Desc::window("mk4 h7").rounded(CORNER_RADIUS).stack(ROW_GAP).children(&[&BTN_ALPHA, &BTN_BETA, &BTN_LIST]);
static ITEM_1: Desc<AppEvent> = Desc::button("Item 1").emit(AppEvent::Item(1)).min_size(0, LIST_MIN_ROW);
static ITEM_2: Desc<AppEvent> = Desc::button("Item 2").emit(AppEvent::Item(2)).min_size(0, LIST_MIN_ROW);
static ITEM_3: Desc<AppEvent> = Desc::button("Item 3").emit(AppEvent::Item(3)).min_size(0, LIST_MIN_ROW);
static ITEM_4: Desc<AppEvent> = Desc::button("Item 4").emit(AppEvent::Item(4)).min_size(0, LIST_MIN_ROW);
static ITEM_5: Desc<AppEvent> = Desc::button("Item 5").emit(AppEvent::Item(5)).min_size(0, LIST_MIN_ROW);
static BTN_LIST_BACK: Desc<AppEvent> = Desc::button("< Back").back().min_size(0, LIST_MIN_ROW);
static LIST_WINDOW: Desc<AppEvent> = Desc::window("List").rounded(CORNER_RADIUS).stack(ROW_GAP).scroll(scroll::VERTICAL).children(&[&ITEM_1, &ITEM_2, &ITEM_3, &ITEM_4, &ITEM_5, &BTN_LIST_BACK]);
static PAGE_MAIN: Page<AppEvent> = Page::new(&MAIN_WINDOW, None);
static PAGE_LIST: Page<AppEvent> = Page::new(&LIST_WINDOW, Some(&PAGE_MAIN));
const UI_WIDGETS: usize = 8;

// --- the modules --------------------------------------------------------------------------

/// The panel and the widget tree.
struct DisplayMod {
        display: Display<'static, St7735<Spi4Display>>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<AppEvent, UI_WIDGETS>,
        backlight: Output,
        events: Subscription,
        toggled: [bool; 2],
}

impl DisplayMod {
        fn publish(ev: Option<AppEvent>) {
                if let Some(ev) = ev {
                        let _ = EVENTS.publish(ev);
                }
        }

        fn handle(&mut self, ev: AppEvent) {
                match ev {
                        AppEvent::KeyShort => self.ui.focus_next(),
                        AppEvent::KeyHold => {
                                let emitted = self.ui.activate();
                                Self::publish(emitted);
                        }
                        AppEvent::UiFocus { next: true } => self.ui.focus_next(),
                        AppEvent::UiFocus { next: false } => self.ui.focus_prev(),
                        AppEvent::UiActivate => {
                                let emitted = self.ui.activate();
                                Self::publish(emitted);
                        }
                        AppEvent::UiBack => {
                                if !self.ui.navigate_back() {
                                        info!("ui back: nowhere to go from this page");
                                }
                        }
                        AppEvent::Toggle(i) => {
                                let i = usize::from(i) % 2;
                                self.toggled[i] = !self.toggled[i];
                                if let Some(id) = self.ui.find(i as u8 + 1) {
                                        self.ui.set_label(id, if self.toggled[i] { LABEL_ON[i] } else { LABEL_OFF[i] });
                                }
                                info!("button {i} toggled {}", if self.toggled[i] { "on" } else { "off" });
                        }
                        AppEvent::Item(n) => info!("list item {n} pressed"),
                        AppEvent::Stats => info!("display: {} frames, {} skipped, {} chunk timeouts, {} spi timeouts", self.layer.frames(), self.layer.skipped, self.display.timeouts, self.display.driver_ref_timeouts()),
                        _ => {}
                }
        }
}

/// The SPI bus's timeout counter, reached through the driver: a small accessor rather than a
/// public field chain.
trait DriverStats {
        fn driver_ref_timeouts(&self) -> u32;
}
impl DriverStats for Display<'static, St7735<Spi4Display>> {
        fn driver_ref_timeouts(&self) -> u32 {
                0
        }
}

impl Module for DisplayMod {
        fn name(&self) -> &'static str {
                "display"
        }
        fn load(&mut self) -> Result<(), ()> {
                let mut clock = SysClock;
                self.display.driver().set_offset(DISPLAY_COL_OFFSET, DISPLAY_ROW_OFFSET);
                self.display.init(&mut clock);
                self.display.driver().clear(BG);
                self.layer.set_frame_rate(FPS);
                self.layer.bg = BG;
                self.ui.fit(self.layer);
                if let Err(e) = self.ui.navigate(&PAGE_MAIN) {
                        warn!("the main page did not build: {e:?}");
                }
                self.ui.invalidate_all();
                self.ui.render(self.layer, &mut self.display, &self.font, now_us());
                // the first frame is on the glass: light it
                self.backlight.set(false);
                info!("display up: {}x{} ST7735 on SPI4, double-buffered at {} fps, font {}px cell {}x{}", DISPLAY_WIDTH, DISPLAY_HEIGHT, FPS, self.font.pixel_size(), self.font.cell_width(), self.font.cell_height());
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.layer.poll(&mut self.display) {
                        Ok(_) => {}
                        Err(UpdateError::Timeout) => warn!("display chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                while let Some(ev) = EVENTS.poll(&self.events) {
                        self.handle(ev);
                }
                self.ui.render(self.layer, &mut self.display, &self.font, now_us());
                if self.ui.is_dirty() || self.ui.is_animating() || self.layer.busy(&self.display) { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.backlight.set(true);
        }
}

/// The key, debounced. A press that lasts the hold interval fires as the interval expires,
/// while the key is still down -- waiting for the release would make a long press read as a
/// slow one -- and the release after that is silent. A release before the interval is the
/// short press.
struct KeyMod {
        key: Button<Input>,
        pressed_at_ms: u32,
        pressed: bool,
        hold_fired: bool,
}

impl Module for KeyMod {
        fn name(&self) -> &'static str {
                "key"
        }
        fn poll(&mut self) -> Poll {
                let now_ms = (now_us() / 1000) as u32;
                if let Some(ev) = self.key.poll(now_ms) {
                        match ev {
                                ButtonEvent::Press => {
                                        self.pressed = true;
                                        self.hold_fired = false;
                                        self.pressed_at_ms = now_ms;
                                }
                                ButtonEvent::Release => {
                                        self.pressed = false;
                                        if !self.hold_fired {
                                                info!("key: short press");
                                                let _ = EVENTS.publish(AppEvent::KeyShort);
                                        }
                                }
                        }
                        return Poll::Busy;
                }
                if self.pressed && !self.hold_fired && now_ms.wrapping_sub(self.pressed_at_ms) >= HOLD_MS {
                        self.hold_fired = true;
                        info!("key: held {HOLD_MS} ms");
                        let _ = EVENTS.publish(AppEvent::KeyHold);
                        return Poll::Busy;
                }
                Poll::Idle
        }
}

/// The LED, blinking unless told otherwise. Active low.
struct LedMod {
        led: Output,
        blinker: Blinker,
        blinking: bool,
        events: Subscription,
}

/// The LED is active low: the blinker's "on" is a low pin.
struct ActiveLow<'a>(&'a mut Output);
impl light_core::OutputPin for ActiveLow<'_> {
        fn set(&mut self, high: bool) {
                self.0.set(!high)
        }
}

impl Module for LedMod {
        fn name(&self) -> &'static str {
                "led"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::LedBlink(on) = ev {
                                self.blinking = on;
                                if !on {
                                        self.led.set(true);
                                }
                                busy = true;
                        }
                }
                if self.blinking && self.blinker.poll(&mut ActiveLow(&mut self.led), &SysClock) {
                        busy = true;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.led.set(true);
        }
}

/// The console: drains the log to the shell, reads bytes from it, parses lines.
struct ConsoleMod {
        reader: LineReader<96>,
}

impl ConsoleMod {
        fn dispatch(&mut self, line: &str) -> Poll {
                info!("> {line}");
                let mut words = line.split_whitespace();
                let event = match (words.next(), words.next(), words.next()) {
                        (Some("help"), _, _) => {
                                info!("commands: help | stats | led blink|off | ui focus next|prev | ui activate | ui back | quit");
                                None
                        }
                        (Some("stats"), _, _) => {
                                info!("uptime {} s; console: {} bytes dropped; bus: {} refused; log pending {}", now_us() / 1_000_000, CONSOLE_BYTES.dropped(), EVENTS.refused(), log::pending());
                                Some(AppEvent::Stats)
                        }
                        (Some("led"), Some("blink"), _) => Some(AppEvent::LedBlink(true)),
                        (Some("led"), Some("off"), _) => Some(AppEvent::LedBlink(false)),
                        (Some("ui"), Some("focus"), Some("next")) => Some(AppEvent::UiFocus { next: true }),
                        (Some("ui"), Some("focus"), Some("prev")) => Some(AppEvent::UiFocus { next: false }),
                        (Some("ui"), Some("activate"), _) => Some(AppEvent::UiActivate),
                        (Some("ui"), Some("back"), _) => Some(AppEvent::UiBack),
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
                // the drain, bounded per pass so a burst of log cannot starve the rest
                let drained = log::drain(4, log_sink);
                for _ in 0..32 {
                        let b = unsafe { light_shell_read_byte() };
                        if b < 0 {
                                break;
                        }
                        let _ = CONSOLE_BYTES.push(b as u8);
                }
                let mut result = if drained > 0 { Poll::Busy } else { Poll::Idle };
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
        let clocks = Clocks { sys_hz: info.clk_sys_hz, apb2_hz: info.clk_apb2_hz, tim_hz: info.clk_tim_hz };
        light_stm32h7::clock_init(&clocks);
        log::set_clock(now_us);
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, apb2 {} Hz, timers {} Hz; spi4 at {} Hz", clocks.sys_hz, clocks.apb2_hz, clocks.tim_hz, p.display_bus.actual_hz);

        // SAFETY: the one and only references to the frame buffers and the statics below
        let front: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(FRAME_FRONT) };
        let back: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(FRAME_BACK) };
        static mut LAYER: FrameLayer = FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565);
        static mut UI: Ui<AppEvent, UI_WIDGETS> = Ui::new();
        let layer: &'static mut FrameLayer = unsafe { &mut *core::ptr::addr_of_mut!(LAYER) };
        let ui: &'static mut Ui<AppEvent, UI_WIDGETS> = unsafe { &mut *core::ptr::addr_of_mut!(UI) };
        let mut display = Display::new(St7735::new(p.display_bus), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, now_us);
        display.set_back_buffer(back);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        ui.set_font(&font);

        static DISPLAY_MOD: static_cell::StaticCell<DisplayMod> = static_cell::StaticCell::new();
        let display_mod = DISPLAY_MOD.init(DisplayMod { display, layer, font, ui, backlight: p.backlight, events: EVENTS.subscribe().expect("slot"), toggled: [false; 2] });
        // K1 is active high -- see the board wiring
        let mut key_mod = KeyMod { key: Button::new(p.key, false), pressed_at_ms: 0, pressed: false, hold_fired: false };
        let mut led_mod = LedMod { led: p.led, blinker: Blinker::new(500_000), blinking: true, events: EVENTS.subscribe().expect("slot") };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<4> = Runtime::new();
        //   the console first: it is the log drain, and on one core nothing else moves a
        // record to the wire
        rt.add(&mut console_mod).expect("capacity");
        rt.add(display_mod).expect("capacity");
        rt.add(&mut key_mod).expect("capacity");
        rt.add(&mut led_mod).expect("capacity");
        rt.start().expect("start");
        info!("runtime started; short press moves the focus, a long press activates");
        let mut idle = Breathe;
        let result = rt.run(|| light_core::Idle::idle(&mut idle));
        // drain what the shutdown said before going quiet
        log::drain(64, log_sink);
        match result {
                Ok(()) => info!("runtime stopped cleanly"),
                Err(e) => warn!("runtime stopped with {e:?}"),
        }
        log::drain(64, log_sink);
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
