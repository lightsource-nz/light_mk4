//! The Rust side of the touch28 firmware: the touch169's demo on the Waveshare
//! RP2350-Touch-LCD-2.8 -- the first board with the CST328 touch controller, whose bring-up
//! this firmware exists to run. Same shell, same module set, same event bus; what differs is
//! the wiring (`board.rs`), the touch driver, and the glass: 240x320, square corners, no
//! GDDRAM offset.
//!
//! Bring-up checklist (mk3 authored this board's support without hardware): the CST328's
//! 0xCACA probe answers; coordinates track a finger; the IMU axis map is IDENTITY until the
//! three-observation calibration is done, so orientation changes will likely be WRONG at
//! first -- that is the measurement, not a bug; the SPI clock is 40 MHz with reported
//! headroom to 62.5; the `touch`/`render` console instruments are carried for the same
//! wedge-hunting they did on the 1.69.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_input::cst328::{self, Cst328};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_display::st7789::St7789;
use light_input::touch::{Gesture, Tracker};
use light_ui::{scroll, Desc, Page, SwipeDir, Touch, Ui};
use light_core::cli::{Cli, Command as CliCommand, Outcome, Parsed, Words};
use light_core::{debug, info, log, warn, ConstStaticCell, EventBus, LineReader, Mailbox, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer, UpdateError};
use light_draw::{PixelFormat, Rotation};
use light_font::Font;
mod board;
use board::*;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::I2c1;
use light_rp2::spi::Spi1Display;
use light_rp2::{Breathe, Clocks, SysClock};

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

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);

/// Two frame buffers, 150 KB each, in .bss: the panel is pushed from one while the next
/// frame is drawn into the other.
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
static FRAME_BACK: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));

// --- the event bus --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        Touch(cst328::Event),
        /// A swipe, in the panel's own coordinate space.
        Gesture(Gesture),
        /// The board's settled orientation changed.
        Orientation(Orientation),
        Command(Command),
        /// Something a widget emitted.
        Ui(UiAction),
}

#[derive(Clone, Copy, Debug)]
enum Command {
        Stats,
        Backlight(u16),
        UiFocus { next: bool },
        UiActivate,
        UiPress { x: u16, y: u16 },
        UiBack,
        RenderMode(RenderMode),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderMode {
        Normal,
        Paused,
        Repush,
}

#[derive(Clone, Copy, Debug)]
enum UiAction {
        Toggle(u8),
        Item(u8),
        DragConsumed,
}

static EVENTS: EventBus<AppEvent, 16, 5> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();
/// The 1.69's push-versus-touch bisect instruments, carried: whether the CST328 shares the
/// CST816T's SPI-burst sensitivity is one of the questions this board's bring-up answers.
static PUSHING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static TOUCH_HOLD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// --- core 1 --------------------------------------------------------------------------------

fn log_sink(record: &log::Record) {
        let mut line = StackString::<160>::new();
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

/// Square glass on this board: product photos show no rounding worth declaring, so 0 exactly
/// like the OLED rigs. TO BE CONFIRMED: if the glass clips corner content, measure the radius
/// the way the 1.69's was measured.
const CORNER_RADIUS: u8 = 0;
/// mk3's demo config for this board: rows this tall need more than the OLED rigs' 2 px.
const ROW_GAP: u8 = 6;
/// Touch-target height; the extra 40 rows of panel simply show more of the list at once.
const LIST_MIN_ROW: i32 = 56;
const FPS: u32 = 30;
const BG: u16 = 0x0000;

const BACKLIGHT_DIM: u16 = BACKLIGHT_LEVEL_MAX / 10;

const LABEL_OFF: [&str; 3] = ["Alpha", "Beta", "Gamma"];
const LABEL_ON: [&str; 3] = ["Alpha *", "Beta *", "Gamma *"];

static BTN_ALPHA: Desc<AppEvent> = Desc::button(LABEL_OFF[0]).emit(AppEvent::Ui(UiAction::Toggle(0))).tag(1);
static BTN_BETA: Desc<AppEvent> = Desc::button(LABEL_OFF[1]).emit(AppEvent::Ui(UiAction::Toggle(1))).tag(2);
static BTN_GAMMA: Desc<AppEvent> = Desc::button(LABEL_OFF[2]).emit(AppEvent::Ui(UiAction::Toggle(2))).tag(3);
static BTN_MORE: Desc<AppEvent> = Desc::button("More >").navigate(&PAGE_DETAIL);
static BTN_LIST: Desc<AppEvent> = Desc::button("List >").navigate(&PAGE_LIST);
static MAIN_WINDOW: Desc<AppEvent> = Desc::window("mk4 2.8").rounded(CORNER_RADIUS).stack(ROW_GAP).children(&[&BTN_ALPHA, &BTN_BETA, &BTN_GAMMA, &BTN_MORE, &BTN_LIST]);

static LBL_DETAIL: Desc<AppEvent> = Desc::label("swipe right to go back");
static BTN_DIM: Desc<AppEvent> = Desc::button("Dim").emit(AppEvent::Command(Command::Backlight(BACKLIGHT_DIM)));
static BTN_BRIGHT: Desc<AppEvent> = Desc::button("Bright").emit(AppEvent::Command(Command::Backlight(BACKLIGHT_LEVEL_MAX)));
static BTN_BACK: Desc<AppEvent> = Desc::button("< Back").back();
static DETAIL_WINDOW: Desc<AppEvent> = Desc::window("More").rounded(CORNER_RADIUS).stack(ROW_GAP).children(&[&LBL_DETAIL, &BTN_DIM, &BTN_BRIGHT, &BTN_BACK]);

static ITEM_1: Desc<AppEvent> = Desc::button("Item 1").emit(AppEvent::Ui(UiAction::Item(1))).min_size(0, LIST_MIN_ROW);
static ITEM_2: Desc<AppEvent> = Desc::button("Item 2").emit(AppEvent::Ui(UiAction::Item(2))).min_size(0, LIST_MIN_ROW);
static ITEM_3: Desc<AppEvent> = Desc::button("Item 3").emit(AppEvent::Ui(UiAction::Item(3))).min_size(0, LIST_MIN_ROW);
static ITEM_4: Desc<AppEvent> = Desc::button("Item 4").emit(AppEvent::Ui(UiAction::Item(4))).min_size(0, LIST_MIN_ROW);
static ITEM_5: Desc<AppEvent> = Desc::button("Item 5").emit(AppEvent::Ui(UiAction::Item(5))).min_size(0, LIST_MIN_ROW);
static ITEM_6: Desc<AppEvent> = Desc::button("Item 6").emit(AppEvent::Ui(UiAction::Item(6))).min_size(0, LIST_MIN_ROW);
static ITEM_7: Desc<AppEvent> = Desc::button("Item 7").emit(AppEvent::Ui(UiAction::Item(7))).min_size(0, LIST_MIN_ROW);
static BTN_LIST_BACK: Desc<AppEvent> = Desc::button("< Back").back().min_size(0, LIST_MIN_ROW);
static LIST_WINDOW: Desc<AppEvent> =
        Desc::window("List").rounded(CORNER_RADIUS).stack(ROW_GAP).scroll(scroll::VERTICAL).children(&[&ITEM_1, &ITEM_2, &ITEM_3, &ITEM_4, &ITEM_5, &ITEM_6, &ITEM_7, &BTN_LIST_BACK]);

static PAGE_MAIN: Page<AppEvent> = Page::new(&MAIN_WINDOW, None);
static PAGE_DETAIL: Page<AppEvent> = Page::new(&DETAIL_WINDOW, Some(&PAGE_MAIN));
static PAGE_LIST: Page<AppEvent> = Page::new(&LIST_WINDOW, Some(&PAGE_MAIN));

const UI_WIDGETS: usize = 12;

// --- the modules --------------------------------------------------------------------------

struct DisplayMod {
        display: Display<'static, St7789<Spi1Display>>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<AppEvent, UI_WIDGETS>,
        events: Subscription,
        toggled: [bool; 3],
        mode: RenderMode,
        drag_reported: bool,
        draw_us_max: u64,
        push_us_max: u64,
        push_started_us: Option<u64>,
}

impl DisplayMod {
        fn publish(ev: Option<AppEvent>) {
                if let Some(ev) = ev {
                        if let Err(e) = EVENTS.publish(ev) {
                                warn!("event bus full; dropped {e:?}");
                        }
                }
        }

        fn handle(&mut self, ev: AppEvent) {
                match ev {
                        AppEvent::Touch(t) => {
                                if self.mode == RenderMode::Repush {
                                        if let cst328::Event::Down { .. } = t {
                                                if !self.display.busy() {
                                                        let _ = self.display.update_async(light_display::Region::full(DISPLAY_WIDTH, DISPLAY_HEIGHT));
                                                }
                                        }
                                }
                                let outcome = match t {
                                        cst328::Event::Down { x, y } | cst328::Event::Move { x, y } => self.ui.touch(x, y, true),
                                        cst328::Event::Up => self.ui.touch(0, 0, false),
                                        cst328::Event::Reset => return,
                                };
                                match outcome {
                                        Touch::Drag if !self.drag_reported => {
                                                self.drag_reported = true;
                                                Self::publish(Some(AppEvent::Ui(UiAction::DragConsumed)));
                                        }
                                        Touch::Tap { hit, emitted } => {
                                                debug!("tap: {}", if hit { "hit" } else { "no widget there" });
                                                Self::publish(emitted);
                                        }
                                        Touch::DragEnd | Touch::None => self.drag_reported = false,
                                        _ => {}
                                }
                        }
                        AppEvent::Gesture(g) => {
                                if self.ui.swipe_direction(g.start, g.end) == Some(SwipeDir::Right) && self.ui.navigate_back() {
                                        debug!("swipe: returned to the previous page");
                                }
                        }
                        AppEvent::Orientation(o) => {
                                //   the same table as the 1.69's -- but the axis map is the
                                // UNMEASURED identity, so until the calibration session these
                                // rotations are the thing under test, not a fact
                                let rotation = match o {
                                        Orientation::Portrait => Some(Rotation::R0),
                                        Orientation::PortraitFlip => Some(Rotation::R180),
                                        Orientation::LandscapeL => Some(Rotation::R270),
                                        Orientation::LandscapeR => Some(Rotation::R90),
                                        _ => None,
                                };
                                if let Some(r) = rotation {
                                        self.ui.set_rotation(self.layer, r);
                                        let (w, h) = self.ui.logical_size();
                                        info!("orientation {o:?}: canvas now {w}x{h}");
                                }
                        }
                        AppEvent::Command(Command::UiFocus { next }) => {
                                if next {
                                        self.ui.focus_next()
                                } else {
                                        self.ui.focus_prev()
                                }
                        }
                        AppEvent::Command(Command::UiActivate) => {
                                let emitted = self.ui.activate();
                                Self::publish(emitted);
                        }
                        AppEvent::Command(Command::UiPress { x, y }) => {
                                let (hit, emitted) = self.ui.press_at(x, y);
                                info!("ui press {x} {y}: {}", if hit { "hit" } else { "no widget there" });
                                Self::publish(emitted);
                        }
                        AppEvent::Command(Command::RenderMode(m)) => {
                                self.mode = m;
                                info!("render mode {m:?}");
                        }
                        AppEvent::Command(Command::UiBack) => {
                                if !self.ui.navigate_back() {
                                        info!("ui back: nowhere to go from this page");
                                }
                        }
                        AppEvent::Ui(UiAction::Toggle(i)) => {
                                let i = usize::from(i) % 3;
                                self.toggled[i] = !self.toggled[i];
                                if let Some(id) = self.ui.find(i as u8 + 1) {
                                        self.ui.set_label(id, if self.toggled[i] { LABEL_ON[i] } else { LABEL_OFF[i] });
                                }
                                info!("button {i} toggled {}", if self.toggled[i] { "on" } else { "off" });
                        }
                        AppEvent::Ui(UiAction::Item(n)) => info!("list item {n} pressed"),
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

        fn render(&mut self) {
                if self.mode != RenderMode::Normal || (!self.ui.is_dirty() && !self.ui.is_animating()) {
                        return;
                }
                let now = light_rp2::now_us();
                let drew = self.ui.render(self.layer, &mut self.display, &self.font, now);
                let done = light_rp2::now_us();
                if drew || self.ui.is_animating() {
                        self.draw_us_max = self.draw_us_max.max(done - now);
                        self.push_started_us = Some(done);
                }
        }
}

impl Module for DisplayMod {
        fn name(&self) -> &'static str {
                "display"
        }
        fn load(&mut self) -> Result<(), ()> {
                let mut clock = SysClock;
                self.display.init(&mut clock);
                //   no set_offset: 240x320 is the ST7789's full GDDRAM, so the power-on (0,0)
                // is already correct -- the 1.69's row offset of 20 is a fact about its
                // 240x280 window, not about the driver
                self.display.driver().clear(BG);
                self.layer.set_frame_rate(FPS);
                self.layer.bg = BG;
                self.ui.fit(self.layer);
                if let Err(e) = self.ui.navigate(&PAGE_MAIN) {
                        warn!("the main page did not build: {e:?}");
                }
                self.ui.invalidate_all();
                self.render();
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
                                self.push_us_max = self.push_us_max.max(light_rp2::now_us() - started);
                                self.push_started_us = None;
                        }
                }
                while let Some(ev) = EVENTS.poll(&self.events) {
                        self.handle(ev);
                }
                self.render();
                let busy = self.layer.busy(&self.display);
                PUSHING.store(busy, core::sync::atomic::Ordering::Relaxed);
                if self.ui.is_dirty() || self.ui.is_animating() || busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.display.driver().clear(BG);
                info!("display down");
        }
}

/// Owns the CST328 and publishes what it reports. The first hardware this driver has met.
struct TouchMod {
        touch: Cst328<&'static RefCell<I2c1>, Input, Output>,
        tracker: Tracker,
        events: Subscription,
        moves: u32,
}

impl Module for TouchMod {
        fn name(&self) -> &'static str {
                "touch"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.touch.reset_blocking(&mut SysClock);
                //   retried: the first transaction after reset can land while the part is
                // still counting out its own ~120 ms boot timer
                let mut clock = SysClock;
                let mut result = self.touch.probe();
                for _ in 0..2 {
                        if result.is_ok() {
                                break;
                        }
                        light_core::hal::Clock::delay_ms(&mut clock, 20);
                        result = self.touch.probe();
                }
                match result {
                        Ok(Some(fw)) => info!("cst328 firmware marker confirmed (fw {fw:#010x})"),
                        Ok(None) => warn!("cst328 answered without the 0xCACA marker; polling anyway"),
                        Err(e) => warn!("cst328 did not answer the probe: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Command(Command::Stats) => {
                                        info!(
                                                "touch: {} failed reads ({} nack, {} timeout, {} bus), {} resets",
                                                self.touch.failures,
                                                self.touch.nacks,
                                                self.touch.timeouts,
                                                self.touch.bus_errors,
                                                self.touch.recoveries
                                        );
                                }
                                AppEvent::Ui(UiAction::DragConsumed) => self.tracker.suppress(),
                                _ => {}
                        }
                }
                if TOUCH_HOLD.load(core::sync::atomic::Ordering::Relaxed) && PUSHING.load(core::sync::atomic::Ordering::Relaxed) {
                        return Poll::Idle;
                }
                let now_ms = (light_rp2::now_us() / 1000) as u32;
                let Some(ev) = self.touch.poll(now_ms) else { return Poll::Idle };
                match ev {
                        cst328::Event::Down { x, y } => {
                                self.moves = 0;
                                debug!("touch down at {x},{y}");
                        }
                        cst328::Event::Up => debug!("touch up after {} moves at {},{}", self.moves, self.touch.x, self.touch.y),
                        cst328::Event::Reset => match self.touch.probe() {
                                Ok(_) => info!("touch controller reset ({} so far); answering again", self.touch.recoveries),
                                Err(e) => warn!("touch controller reset ({} so far); still not answering: {e:?}", self.touch.recoveries),
                        },
                        cst328::Event::Move { .. } => self.moves += 1,
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
                //   the UNMEASURED identity map -- see board.rs. Orientation output is
                // suspect until the three-observation calibration replaces it
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
                let now_ms = (light_rp2::now_us() / 1000) as u32;
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

struct BoardMod {
        backlight: light_rp2::pwm::PwmOutput,
        events: Subscription,
}

impl Module for BoardMod {
        fn name(&self) -> &'static str {
                "board"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.backlight.set_duty(BACKLIGHT_LEVEL_MAX);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::Command(Command::Backlight(level)) = ev {
                                busy = true;
                                self.backlight.set_duty(level);
                                info!("backlight {level}");
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.backlight.set_duty(0);
        }
}

struct ConsoleMod {
        reader: LineReader<96>,
}

impl ConsoleMod {
        fn dispatch(&mut self, line: &str) -> Poll {
                match CLI.dispatch(line) {
                        Outcome::Quiet => Poll::Idle,
                        Outcome::Shutdown => Poll::Shutdown,
                        Outcome::Event(c) => {
                                if let Command::Stats = c {
                                        info!("console: {} bytes dropped, {} lines dropped; bus: {} refused, {} backlog", CONSOLE_BYTES.dropped(), self.reader.dropped_lines, EVENTS.refused(), EVENTS.backlog());
                                }
                                if let Err(e) = EVENTS.publish(AppEvent::Command(c)) {
                                        warn!("event bus full; dropped {e:?}");
                                }
                                Poll::Busy
                        }
                        Outcome::Handled => Poll::Busy,
                }
        }
}

fn parse_stats(_w: &mut Words) -> Parsed<Command> {
        Parsed::Event(Command::Stats)
}

fn parse_backlight(w: &mut Words) -> Parsed<Command> {
        match w.next().and_then(|s| s.parse::<u16>().ok()) {
                Some(level) if level <= BACKLIGHT_LEVEL_MAX => Parsed::Event(Command::Backlight(level)),
                _ => Parsed::Usage,
        }
}

fn parse_ui(w: &mut Words) -> Parsed<Command> {
        match (w.next(), w.next(), w.next()) {
                (Some("focus"), Some("next"), _) => Parsed::Event(Command::UiFocus { next: true }),
                (Some("focus"), Some("prev"), _) => Parsed::Event(Command::UiFocus { next: false }),
                (Some("activate"), _, _) => Parsed::Event(Command::UiActivate),
                (Some("press"), Some(x), Some(y)) => match (x.parse(), y.parse()) {
                        (Ok(x), Ok(y)) => Parsed::Event(Command::UiPress { x, y }),
                        _ => Parsed::Usage,
                },
                (Some("back"), _, _) => Parsed::Event(Command::UiBack),
                _ => Parsed::Usage,
        }
}

fn parse_touch(w: &mut Words) -> Parsed<Command> {
        match w.next() {
                Some("hold") => {
                        TOUCH_HOLD.store(true, core::sync::atomic::Ordering::Relaxed);
                        info!("touch: reads held while the panel is being pushed");
                        Parsed::Done
                }
                Some("free") => {
                        TOUCH_HOLD.store(false, core::sync::atomic::Ordering::Relaxed);
                        info!("touch: reads not held");
                        Parsed::Done
                }
                _ => Parsed::Usage,
        }
}

fn parse_render(w: &mut Words) -> Parsed<Command> {
        match w.next() {
                Some("pause") => Parsed::Event(Command::RenderMode(RenderMode::Paused)),
                Some("resume") => Parsed::Event(Command::RenderMode(RenderMode::Normal)),
                Some("repush") => Parsed::Event(Command::RenderMode(RenderMode::Repush)),
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[CliCommand<Command>] = &[
        CliCommand { name: "stats", usage: "stats", parse: parse_stats },
        CliCommand { name: "backlight", usage: "backlight 0..1000", parse: parse_backlight },
        CliCommand { name: "ui", usage: "ui focus next|prev | ui activate | ui press X Y | ui back", parse: parse_ui },
        CliCommand { name: "touch", usage: "touch hold|free", parse: parse_touch },
        CliCommand { name: "render", usage: "render pause|resume|repush", parse: parse_render },
];
static CLI: Cli<Command> = Cli::new(COMMANDS);

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

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        log::set_clock(light_rp2::now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, peri {} Hz; spi1 at {} Hz, i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.display_bus.actual_hz, p.touch_bus.actual_hz);

        let front: &'static mut [u8] = FRAME_FRONT.take();
        let back: &'static mut [u8] = FRAME_BACK.take();
        let mut display = Display::new(St7789::new(p.display_bus), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        display.set_back_buffer(back);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        static I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let i2c: &'static RefCell<I2c1> = I2C.init(RefCell::new(p.touch_bus));
        let touch = Cst328::new(i2c, p.touch_int, p.touch_reset, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(i2c));

        let mut board_mod = BoardMod { backlight: p.backlight, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut imu_mod = ImuMod { imu, events: EVENTS.subscribe().expect("subscriber slot") };
        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565));
        static UI: ConstStaticCell<Ui<AppEvent, UI_WIDGETS>> = ConstStaticCell::new(Ui::new());
        let layer: &'static mut FrameLayer = LAYER.take();
        let ui: &'static mut Ui<AppEvent, UI_WIDGETS> = UI.take();
        layer.bg = BG;
        ui.set_font(&font);
        static DISPLAY_MOD: StaticCell<DisplayMod> = StaticCell::new();
        let display_mod = DISPLAY_MOD.init(DisplayMod {
                display,
                layer,
                font,
                ui,
                events: EVENTS.subscribe().expect("subscriber slot"),
                toggled: [false; 3],
                mode: RenderMode::Normal,
                drag_reported: false,
                draw_us_max: 0,
                push_us_max: 0,
                push_started_us: None,
        });
        static TOUCH_MOD: StaticCell<TouchMod> = StaticCell::new();
        let touch_mod = TOUCH_MOD.init(TouchMod { touch, tracker: Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), events: EVENTS.subscribe().expect("subscriber slot"), moves: 0 });
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<5> = Runtime::new();
        rt.add(&mut board_mod).expect("capacity");
        rt.add(display_mod).expect("capacity");
        rt.add(touch_mod).expect("capacity");
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
