//! The Rust side of the po13 rig firmware: a Pico 2 with the Pico-OLED-1.3. The LED blinks, the
//! OLED shows mk3's `light_ui` demo on a 1 bpp panel mounted sideways -- the same widget toolkit
//! the touch169 runs, driven from the board's two keys instead of a touch panel: KEY0 moves the
//! focus, KEY1 activates. That is the other half of the pair mk3 kept, proving one widget tree
//! works from either input path.
//!
//! The shell (module/light_mk4_shell) is the same file the touch169 links.

#![no_std]

use core::fmt::Write;
use light_core::button::{Button, ButtonEvent};
use light_display::sh1107::Sh1107;
use light_ui::{scroll, Desc, Page, Ui};
use light_core::{info, log, warn, Blinker, ConstStaticCell, EventBus, LineReader, Mailbox, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer, UpdateError};
use light_draw::{Flip, PixelFormat, Rotation};
use light_font::Font;
use light_rp2350::boards::pico2::*;
use light_rp2350::gpio::{Input, Output};
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
        /// One of the board's keys, debounced: `(key, pressed)`.
        Key(u8, bool),
        /// The UI events as commands, for driving the rig from the console or a script.
        UiFocus { next: bool },
        UiActivate,
        UiBack,
        /// What a widget emitted.
        Toggle(u8),
        Item(u8),
        Stats,
}

static EVENTS: EventBus<AppEvent, 8, 3> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

/// 64x128 at 1 bpp: one kilobyte.
static FRAME: ConstStaticCell<[u8; PixelFormat::Mono1.buffer_len(OLED_WIDTH, OLED_HEIGHT)]> = ConstStaticCell::new([0; PixelFormat::Mono1.buffer_len(OLED_WIDTH, OLED_HEIGHT)]);
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

// --- the interface, as data ---------------------------------------------------------------
//
// The same shape as the touch169's, cut to what 128x64 logical pixels hold: three rows per
// page. The list page overflows on purpose, and KEY0 cycling focus through it is what scrolls it.

const CORNER_RADIUS: u8 = 6;
const ROW_GAP: u8 = 1;
const LIST_MIN_ROW: i32 = 14;
const FPS: u32 = 20;

const LABEL_OFF: [&str; 2] = ["Alpha", "Beta"];
const LABEL_ON: [&str; 2] = ["Alpha *", "Beta *"];

static BTN_ALPHA: Desc<AppEvent> = Desc::button(LABEL_OFF[0]).emit(AppEvent::Toggle(0)).tag(1);
static BTN_BETA: Desc<AppEvent> = Desc::button(LABEL_OFF[1]).emit(AppEvent::Toggle(1)).tag(2);
static BTN_LIST: Desc<AppEvent> = Desc::button("List >").navigate(&PAGE_LIST);
static MAIN_WINDOW: Desc<AppEvent> = Desc::window("mk4").rounded(CORNER_RADIUS).stack(ROW_GAP).children(&[&BTN_ALPHA, &BTN_BETA, &BTN_LIST]);

static ITEM_1: Desc<AppEvent> = Desc::button("Item 1").emit(AppEvent::Item(1)).min_size(0, LIST_MIN_ROW);
static ITEM_2: Desc<AppEvent> = Desc::button("Item 2").emit(AppEvent::Item(2)).min_size(0, LIST_MIN_ROW);
static ITEM_3: Desc<AppEvent> = Desc::button("Item 3").emit(AppEvent::Item(3)).min_size(0, LIST_MIN_ROW);
static ITEM_4: Desc<AppEvent> = Desc::button("Item 4").emit(AppEvent::Item(4)).min_size(0, LIST_MIN_ROW);
static ITEM_5: Desc<AppEvent> = Desc::button("Item 5").emit(AppEvent::Item(5)).min_size(0, LIST_MIN_ROW);
static BTN_LIST_BACK: Desc<AppEvent> = Desc::button("< Back").back().min_size(0, LIST_MIN_ROW);
static LIST_WINDOW: Desc<AppEvent> = Desc::window("List").rounded(CORNER_RADIUS).stack(ROW_GAP).scroll(scroll::VERTICAL).children(&[&ITEM_1, &ITEM_2, &ITEM_3, &ITEM_4, &ITEM_5, &BTN_LIST_BACK]);

static PAGE_MAIN: Page<AppEvent> = Page::new(&MAIN_WINDOW, None);
static PAGE_LIST: Page<AppEvent> = Page::new(&LIST_WINDOW, Some(&PAGE_MAIN));

/// The widest page is the list: a window and six rows.
const UI_WIDGETS: usize = 8;

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

/// The OLED, drawn on sideways: the glass is 64x128 portrait, the interface is 128x64
/// landscape, so the frame layer's canvas is rotated 90 degrees and maps the regions the
/// toolkit invalidates to physical columns for the driver through the same transform.
struct OledMod {
        display: Display<'static, Sh1107<Spi1Display>>,
        // in .bss, built in place: see the touch169's DisplayMod for why
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<AppEvent, UI_WIDGETS>,
        events: Subscription,
        toggled: [bool; 2],
}

impl OledMod {
        fn publish(ev: Option<AppEvent>) {
                if let Some(ev) = ev {
                        let _ = EVENTS.publish(ev);
                }
        }

        fn handle(&mut self, ev: AppEvent) {
                match ev {
                        // KEY0 moves the focus, KEY1 activates: the two-button rig
                        AppEvent::Key(0, true) | AppEvent::UiFocus { next: true } => self.ui.focus_next(),
                        AppEvent::UiFocus { next: false } => self.ui.focus_prev(),
                        AppEvent::Key(1, true) | AppEvent::UiActivate => {
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
                        AppEvent::Stats => info!("oled: {} frames, {} skipped, {} chunk timeouts", self.layer.frames(), self.layer.skipped, self.display.timeouts),
                        _ => {}
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
                self.layer.set_orientation(Rotation::R90, Flip::None);
                self.layer.set_frame_rate(FPS);
                self.ui.fit(self.layer);
                if let Err(e) = self.ui.navigate(&PAGE_MAIN) {
                        warn!("the main page did not build: {e:?}");
                }
                self.ui.invalidate_all();
                self.ui.render(self.layer, &mut self.display, &self.font, now_us());
                info!("oled up: {}x{} glass, {}x{} logical, font {}px cell {}x{}", OLED_WIDTH, OLED_HEIGHT, OLED_HEIGHT, OLED_WIDTH, self.font.pixel_size(), self.font.cell_width(), self.font.cell_height());
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.layer.poll(&mut self.display) {
                        Ok(_) => {}
                        Err(UpdateError::Timeout) => warn!("oled chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                while let Some(ev) = EVENTS.poll(&self.events) {
                        self.handle(ev);
                }
                self.ui.render(self.layer, &mut self.display, &self.font, now_us());
                if self.ui.is_dirty() || self.layer.busy(&self.display) { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.display.driver().clear(false);
        }
}

/// The board's two keys, debounced, onto the bus.
struct KeysMod {
        keys: [Button<Input>; 2],
        presses: u32,
}

impl Module for KeysMod {
        fn name(&self) -> &'static str {
                "keys"
        }
        fn poll(&mut self) -> Poll {
                let now_ms = (now_us() / 1000) as u32;
                let mut busy = false;
                for (i, key) in self.keys.iter_mut().enumerate() {
                        if let Some(ev) = key.poll(now_ms) {
                                busy = true;
                                let pressed = ev == ButtonEvent::Press;
                                if pressed {
                                        self.presses += 1;
                                }
                                info!("key{i} {}", if pressed { "pressed" } else { "released" });
                                let _ = EVENTS.publish(AppEvent::Key(i as u8, pressed));
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
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
                                info!("commands: help | stats | led on|off|blink | ui focus next|prev | ui activate | ui back | quit");
                                None
                        }
                        (Some("stats"), _, _) => {
                                info!("uptime {} s; console: {} bytes dropped; bus: {} refused", now_us() / 1_000_000, CONSOLE_BYTES.dropped(), EVENTS.refused());
                                Some(AppEvent::Stats)
                        }
                        (Some("led"), Some("on"), _) => Some(AppEvent::LedOn),
                        (Some("led"), Some("off"), _) => Some(AppEvent::LedOff),
                        (Some("led"), Some("blink"), _) => Some(AppEvent::LedBlink),
                        (Some("ui"), Some("focus"), Some("next")) => Some(AppEvent::UiFocus { next: true }),
                        (Some("ui"), Some("focus"), Some("prev")) => Some(AppEvent::UiFocus { next: false }),
                        (Some("ui"), Some("activate"), _) => Some(AppEvent::UiActivate),
                        (Some("ui"), Some("back"), _) => Some(AppEvent::UiBack),
                        (Some("ui"), _, _) => {
                                warn!("usage: ui focus next|prev | ui activate | ui back");
                                None
                        }
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
        let frame: &'static mut [u8] = FRAME.take();
        let display = Display::new(Sh1107::new(p.oled_bus), frame, OLED_WIDTH, OLED_HEIGHT, PixelFormat::Mono1, now_us);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        let mut led_mod = LedMod { led: p.led, blinker: Blinker::new(500_000), blinking: true, toggles: 0, events: EVENTS.subscribe().expect("slot") };
        // in .bss rather than on core 0's small stack: the frame layer and the widget arena
        // together are a good part of it
        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(OLED_WIDTH, OLED_HEIGHT, PixelFormat::Mono1));
        static UI: ConstStaticCell<Ui<AppEvent, UI_WIDGETS>> = ConstStaticCell::new(Ui::new());
        let layer: &'static mut FrameLayer = LAYER.take();
        let ui: &'static mut Ui<AppEvent, UI_WIDGETS> = UI.take();
        ui.set_font(&font);
        static OLED_MOD: StaticCell<OledMod> = StaticCell::new();
        let oled_mod = OLED_MOD.init(OledMod { display, layer, font, ui, events: EVENTS.subscribe().expect("slot"), toggled: [false; 2] });
        let mut keys_mod = KeysMod { keys: [Button::new(p.key0, true), Button::new(p.key1, true)], presses: 0 };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<4> = Runtime::new();
        rt.add(&mut led_mod).expect("capacity");
        rt.add(oled_mod).expect("capacity");
        rt.add(&mut keys_mod).expect("capacity");
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
