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
//! drivers that need them.
//!
//! What it shows is mk3's `light_ui` demo: one rounded window filling the glass with a stack of
//! buttons in it, a second page reached from the last row and returned from with a swipe, and a
//! scrolling list. The widget tree is `static` data; a button EMITS an application event, which
//! goes over the same bus a console line does, so a tap, `ui activate` at the console and a
//! script are three spellings of one thing.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_input::cst816t::{self, Cst816t};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_display::st7789::St7789;
use light_input::touch::{Gesture, Tracker};
use light_ui::{scroll, Desc, Page, SwipeDir, Touch, Ui};
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
/// is drawn into the other. A `ConstStaticCell` is built in place and handed out exactly
/// once, by `take()`, which panics on a second call -- no `static mut`, no aliasing to reason
/// about
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
static FRAME_BACK: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

/// The demo's font, rendered by crush at build time and handed over as a path by
/// `light_mk4_add_font` in the CMake -- a blob in flash, parsed in place, no generated C.
static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));

// --- the event bus --------------------------------------------------------------------------

/// Everything that happens in this application, as one type. The console is one producer of
/// `Command`s; a widget, a test or a boot script are others, without going through text.
#[derive(Clone, Copy, Debug)]
enum AppEvent {
        Touch(cst816t::Event),
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
        /// A backlight level, 0..=BACKLIGHT_LEVEL_MAX.
        Backlight(u16),
        /// The UI events as commands: `ui focus next|prev`, `ui activate`, `ui press X Y`,
        /// `ui back`. Most of their value is on a bring-up rig: a console drives a board whose
        /// only physical input is a touch panel, and a host script replays an interaction.
        UiFocus { next: bool },
        UiActivate,
        UiPress { x: u16, y: u16 },
        UiBack,
        /// The rendering bisect: normal; paused (nothing drawn or pushed); or repushing the
        /// UNCHANGED frame on every touch -- all the bus and DMA activity, nothing changing on
        /// the glass -- which separates electrical coupling from the LCD itself disturbing the
        /// touch sensor.
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
        /// One of the three toggle buttons.
        Toggle(u8),
        /// A list row.
        Item(u8),
        /// A drag scrolled a window: the finger's movement is spent, and the touch module must
        /// not let its release classify as a swipe as well.
        DragConsumed,
}

/// 16 events deep, 5 subscribers: display, touch, imu, board, and one spare.
static EVENTS: EventBus<AppEvent, 16, 5> = EventBus::new();
/// Raw console bytes, core 1 → core 0. Sized for a burst of pasted text.
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();
/// The display module says whether a panel push is in flight; the touch module reads it. One
/// bisect found the CST816T fails its reads while the SPI/DMA burst runs, whatever the clock
/// rate and whether the picture changes, and a read that fails counts toward its reset --
/// so while `touch hold` is on, the controller is left alone until the push is over.
static PUSHING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static TOUCH_HOLD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

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

// --- the interface, as data ---------------------------------------------------------------

/// The glass's corner radius, near enough: the root window's frame follows it instead of
/// floating in a square inside it.
const CORNER_RADIUS: u8 = 24;
const ROW_GAP: u8 = 2;
/// Rows in the scrolling list are pinned to this, so the list overflows rather than shrinking.
const LIST_MIN_ROW: i32 = 44;
const FPS: u32 = 30;
const BG: u16 = 0x0000;
const FG: u16 = 0xFFFF;

/// A tenth: the panel stays readable, the way an idle device dims rather than goes dark.
const BACKLIGHT_DIM: u16 = BACKLIGHT_LEVEL_MAX / 10;

const LABEL_OFF: [&str; 3] = ["Alpha", "Beta", "Gamma"];
const LABEL_ON: [&str; 3] = ["Alpha *", "Beta *", "Gamma *"];

static BTN_ALPHA: Desc<AppEvent> = Desc::button(LABEL_OFF[0]).emit(AppEvent::Ui(UiAction::Toggle(0))).tag(1);
static BTN_BETA: Desc<AppEvent> = Desc::button(LABEL_OFF[1]).emit(AppEvent::Ui(UiAction::Toggle(1))).tag(2);
static BTN_GAMMA: Desc<AppEvent> = Desc::button(LABEL_OFF[2]).emit(AppEvent::Ui(UiAction::Toggle(2))).tag(3);
static BTN_MORE: Desc<AppEvent> = Desc::button("More >").navigate(&PAGE_DETAIL);
static BTN_LIST: Desc<AppEvent> = Desc::button("List >").navigate(&PAGE_LIST);
static MAIN_WINDOW: Desc<AppEvent> = Desc::window("mk4 demo").rounded(CORNER_RADIUS).stack(ROW_GAP).children(&[&BTN_ALPHA, &BTN_BETA, &BTN_GAMMA, &BTN_MORE, &BTN_LIST]);

static LBL_DETAIL: Desc<AppEvent> = Desc::label("swipe right to go back");
//   the whole press IS the command: no handler, the button emits the same event the console's
// `backlight off` does
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
// the back row is a list item like any other, and deliberately LAST: reaching it means
// scrolling the whole list, so navigating out doubles as the end-to-end check
static BTN_LIST_BACK: Desc<AppEvent> = Desc::button("< Back").back().min_size(0, LIST_MIN_ROW);
static LIST_WINDOW: Desc<AppEvent> =
        Desc::window("List").rounded(CORNER_RADIUS).stack(ROW_GAP).scroll(scroll::VERTICAL).children(&[&ITEM_1, &ITEM_2, &ITEM_3, &ITEM_4, &ITEM_5, &ITEM_6, &ITEM_7, &BTN_LIST_BACK]);

static PAGE_MAIN: Page<AppEvent> = Page::new(&MAIN_WINDOW, None);
static PAGE_DETAIL: Page<AppEvent> = Page::new(&DETAIL_WINDOW, Some(&PAGE_MAIN));
static PAGE_LIST: Page<AppEvent> = Page::new(&LIST_WINDOW, Some(&PAGE_MAIN));

/// The widest page is the list: a window and eight rows.
const UI_WIDGETS: usize = 12;

// --- the modules --------------------------------------------------------------------------

/// Owns the panel and the widget tree. Input arrives as bus events; what a widget emits goes
/// back onto the bus; the frame layer works out which panel pixels changed.
struct DisplayMod {
        display: Display<'static, St7789<Spi1Display>>,
        //   the two big objects live in .bss as statics built in place (their constructors are
        // const): a stack temporary of either overran core 0's 4 KB stack, and what sits
        // directly below it is core 1's
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<AppEvent, UI_WIDGETS>,
        events: Subscription,
        toggled: [bool; 3],
        /// See Command::RenderMode.
        mode: RenderMode,
        /// Whether the drag in progress has already told the touch module it consumed the touch.
        drag_reported: bool,
        /// Timing, for `stats`: the longest draw (frame_begin to frame_end) and the longest
        /// push (frame_end until the panel is idle) seen, in microseconds.
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
                                //   the panel's own coordinates go straight in: the toolkit
                                // untransforms them, which keeps touches landing on the right
                                // widget once the interface has been rotated. The tracker runs
                                // the whole tap-versus-drag interaction; this module's part is
                                // one rule: a drag that scrolled has SPENT the finger's movement
                                if self.mode == RenderMode::Repush {
                                        if let cst816t::Event::Down { .. } = t {
                                                // the front buffer as it stands, to the whole panel
                                                if !self.display.busy() {
                                                        let _ = self.display.update_async(light_display::Region::full(DISPLAY_WIDTH, DISPLAY_HEIGHT));
                                                }
                                        }
                                }
                                let outcome = match t {
                                        cst816t::Event::Down { x, y } | cst816t::Event::Move { x, y } => self.ui.touch(x, y, true),
                                        cst816t::Event::Up => self.ui.touch(0, 0, false),
                                        cst816t::Event::Reset => return,
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
                                //   swipe right returns to the previous page. Classified from the
                                // gesture's ENDPOINTS in the frame the user is looking at: the
                                // controller's own code is in the panel's frame, which is wrong
                                // in landscape
                                if self.ui.swipe_direction(g.start, g.end) == Some(SwipeDir::Right) && self.ui.navigate_back() {
                                        debug!("swipe: returned to the previous page");
                                }
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
                                // a miss is not an error: tapping empty space is legitimate
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
                //   through Ui::render, which also runs the rotation and page animations; the
                // phase split that found Font::pixel is done by running the frame by hand with
                // frame_begin / paint / commit / frame_end when it is wanted again
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
                self.display.driver().set_offset(0, DISPLAY_ROW_OFFSET);
                self.display.driver().clear(BG);
                self.layer.set_frame_rate(FPS);
                self.layer.bg = BG;
                self.ui.fit(self.layer);
                //   entered through the page system rather than built directly, so the toolkit
                // knows which page it is showing and back has something to reason from
                if let Err(e) = self.ui.navigate(&PAGE_MAIN) {
                        warn!("the main page did not build: {e:?}");
                }
                // nothing on the panel matches the freshly built tree yet
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

/// Owns the touch controller and publishes what it reports: samples, and the swipes the
/// tracker makes of them.
struct TouchMod {
        touch: Cst816t<&'static RefCell<I2c1>, Input, Output>,
        tracker: Tracker,
        events: Subscription,
        /// Move samples seen during the touch in progress, reported on its release: tells a
        /// tap from a drag the controller chopped into pieces.
        moves: u32,
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
                                // the interface scrolled with this touch: its release is not a swipe
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
                        cst816t::Event::Down { x, y } => {
                                self.moves = 0;
                                debug!("touch down at {x},{y}");
                        }
                        cst816t::Event::Up => debug!("touch up after {} moves at {},{}", self.moves, self.touch.x, self.touch.y),
                        cst816t::Event::Reset => match self.touch.probe() {
                                Ok(_) => info!("touch controller reset ({} so far); answering again", self.touch.recoveries),
                                Err(e) => warn!("touch controller reset ({} so far); still not answering: {e:?}", self.touch.recoveries),
                        },
                        cst816t::Event::Move { .. } => self.moves += 1,
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

/// Owns the backlight.
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
                                // the slice and pad state, which is what found the slice mapping wrong
                                let r = self.backlight.registers(PIN_DISPLAY_BL);
                                debug!("backlight pwm: csr {:#x} div {:#x} top {} ctr {} cc {:#x} ctrl {:#x} status {:#x}", r[0], r[1], r[2], r[3], r[4], r[5], r[6]);
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.backlight.set_duty(0);
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
                let event = match cmd {
                        "help" => {
                                info!("commands: help | stats | backlight N | ui focus next|prev | ui activate | ui press X Y | ui back | render pause|resume|repush | touch hold|free | loglevel error|warn|info|debug|trace | quit");
                                None
                        }
                        "stats" => {
                                info!("console: {} bytes dropped, {} lines dropped; bus: {} refused, {} backlog", CONSOLE_BYTES.dropped(), self.reader.dropped_lines, EVENTS.refused(), EVENTS.backlog());
                                Some(Command::Stats)
                        }
                        "backlight" => match args.next().and_then(|s| s.parse::<u16>().ok()) {
                                Some(level) if level <= BACKLIGHT_LEVEL_MAX => Some(Command::Backlight(level)),
                                _ => {
                                        warn!("usage: backlight 0..{}", BACKLIGHT_LEVEL_MAX);
                                        None
                                }
                        },
                        "ui" => match (args.next(), args.next(), args.next()) {
                                (Some("focus"), Some("next"), _) => Some(Command::UiFocus { next: true }),
                                (Some("focus"), Some("prev"), _) => Some(Command::UiFocus { next: false }),
                                (Some("activate"), _, _) => Some(Command::UiActivate),
                                (Some("press"), Some(x), Some(y)) => match (x.parse(), y.parse()) {
                                        (Ok(x), Ok(y)) => Some(Command::UiPress { x, y }),
                                        _ => {
                                                warn!("usage: ui press X Y (panel coordinates)");
                                                None
                                        }
                                },
                                (Some("back"), _, _) => Some(Command::UiBack),
                                _ => {
                                        warn!("usage: ui focus next|prev | ui activate | ui press X Y | ui back");
                                        None
                                }
                        },
                        "touch" => match args.next() {
                                Some("hold") => {
                                        TOUCH_HOLD.store(true, core::sync::atomic::Ordering::Relaxed);
                                        info!("touch: reads held while the panel is being pushed");
                                        None
                                }
                                Some("free") => {
                                        TOUCH_HOLD.store(false, core::sync::atomic::Ordering::Relaxed);
                                        info!("touch: reads not held");
                                        None
                                }
                                _ => {
                                        warn!("usage: touch hold|free");
                                        None
                                }
                        },
                        "render" => match args.next() {
                                Some("pause") => Some(Command::RenderMode(RenderMode::Paused)),
                                Some("resume") => Some(Command::RenderMode(RenderMode::Normal)),
                                Some("repush") => Some(Command::RenderMode(RenderMode::Repush)),
                                _ => {
                                        warn!("usage: render pause|resume|repush");
                                        None
                                }
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
        //   one I2C bus, two drivers: shared through a RefCell that lives as long as the
        // application, which on a firmware that never returns is a static's lifetime
        static I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let i2c: &'static RefCell<I2c1> = I2C.init(RefCell::new(p.touch_bus));
        let touch = Cst816t::new(i2c, p.touch_int, p.touch_reset, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(i2c));

        let mut board_mod = BoardMod { backlight: p.backlight, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut imu_mod = ImuMod { imu, events: EVENTS.subscribe().expect("subscriber slot") };
        //   built in place in .bss -- see DisplayMod -- and taken once
        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565));
        static UI: ConstStaticCell<Ui<AppEvent, UI_WIDGETS>> = ConstStaticCell::new(Ui::new());
        let layer: &'static mut FrameLayer = LAYER.take();
        let ui: &'static mut Ui<AppEvent, UI_WIDGETS> = UI.take();
        layer.bg = BG;
        ui.set_font(&font);
        let _ = FG;
        // module state is 'static in any case: the runtime never returns
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

/// A fixed-capacity string for formatting a line without an allocator.
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
