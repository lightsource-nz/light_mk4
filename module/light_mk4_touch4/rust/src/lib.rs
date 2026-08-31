//! The Rust side of the touch4 firmware: the widget demo on the Waveshare RP2350-Touch-LCD-4
//! -- 480x480 of RGB (DPI) glass with no GDDRAM, the ST7701S's first bring-up, and the
//! first board fed by `light_rp2::rgb`'s pure-hardware scanout loop.
//!
//! The display architecture is the leg's point: the framebuffer IS the panel. One 450 KB
//! RGB565 buffer in SRAM (single-buffered, the bring-up decision -- the flip hook exists
//! for a second buffer if one ever finds room), scanned out by DMA+PIO forever; the
//! display stack runs over `light_display::scanout::Scanout`, whose every update completes
//! the moment it starts. Drawing races the scan, so a slow redraw can tear -- accepted
//! for bring-up, measured before it is engineered away.
//!
//! Bring-up checklist: the panel lights and draws (if dark: backlight polarity first, then
//! the ST7701S init, then scanout timing); GT911 answers on i2c1 and coordinates track,
//! with directions AND the square panel's possible axis swap measured on the glass; the
//! IMU axis map is IDENTITY-until-measured.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_core::cli::{Cli, Command as CliCommand, Outcome, Parsed, Words};
use light_core::{debug, info, log, warn, ConstStaticCell, EventBus, InputPin, LineReader, Mailbox, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::scanout::Scanout;
use light_display::{Display, FrameLayer, UpdateError};
use light_draw::{PixelFormat, Rotation};
use light_font::Font;
use light_input::gt911::{self, Gt911};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_input::touch::{Gesture, Tracker};
use light_rtc::{Datetime, Pcf85063a};
use light_ui::{scroll, Desc, Page, SwipeDir, Touch, Ui};
mod board;
use board::*;
use light_rp2::adc::Adc;
use light_rp2::gpio::Input;
use light_rp2::i2c::I2c1;
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

const FRAME_PIXELS: usize = DISPLAY_WIDTH as usize * DISPLAY_HEIGHT as usize;

/// The one framebuffer: 450 KB of the RP2350's 520, declared as u16 so the scanout DMA's
/// halfword reads are aligned by construction. The board's take() points the engine here;
/// the display stack draws into it through the byte view.
static FRAME: ConstStaticCell<[u16; FRAME_PIXELS]> = ConstStaticCell::new([0; FRAME_PIXELS]);

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));

// --- the event bus --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        Touch(gt911::Event),
        Gesture(Gesture),
        Orientation(Orientation),
        Command(Command),
        Ui(UiAction),
}

#[derive(Clone, Copy, Debug)]
enum Command {
        Stats,
        Scan,
        Backlight(u16),
        UiFocus { next: bool },
        UiActivate,
        UiPress { x: u16, y: u16 },
        UiBack,
        RenderMode(RenderMode),
        RtcShow,
        RtcSet(Datetime),
        Pattern,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderMode {
        Normal,
        Paused,
}

#[derive(Clone, Copy, Debug)]
enum UiAction {
        Toggle(u8),
        Item(u8),
        DragConsumed,
}

static EVENTS: EventBus<AppEvent, 16, 5> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

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

const CORNER_RADIUS: u8 = 0;
const ROW_GAP: u8 = 8;
const LIST_MIN_ROW: i32 = 64;
const FPS: u32 = 30;
const BG: u16 = 0x0000;

const BACKLIGHT_DIM: u16 = 250;

const LABEL_OFF: [&str; 3] = ["Alpha", "Beta", "Gamma"];
const LABEL_ON: [&str; 3] = ["Alpha *", "Beta *", "Gamma *"];

static BTN_ALPHA: Desc<AppEvent> = Desc::button(LABEL_OFF[0]).emit(AppEvent::Ui(UiAction::Toggle(0))).tag(1);
static BTN_BETA: Desc<AppEvent> = Desc::button(LABEL_OFF[1]).emit(AppEvent::Ui(UiAction::Toggle(1))).tag(2);
static BTN_GAMMA: Desc<AppEvent> = Desc::button(LABEL_OFF[2]).emit(AppEvent::Ui(UiAction::Toggle(2))).tag(3);
static BTN_MORE: Desc<AppEvent> = Desc::button("More >").navigate(&PAGE_DETAIL);
static BTN_LIST: Desc<AppEvent> = Desc::button("List >").navigate(&PAGE_LIST);
static MAIN_WINDOW: Desc<AppEvent> = Desc::window("mk4 4.0").rounded(CORNER_RADIUS).stack(ROW_GAP).children(&[&BTN_ALPHA, &BTN_BETA, &BTN_GAMMA, &BTN_MORE, &BTN_LIST]);

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
        display: Display<'static, Scanout>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<AppEvent, UI_WIDGETS>,
        events: Subscription,
        toggled: [bool; 3],
        mode: RenderMode,
        drag_reported: bool,
        draw_us_max: u64,
        /// Renders deferred because the beam was inside the dirty area -- the tear-free
        /// gate's pulse.
        beam_waits: u32,
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
                                let outcome = match t {
                                        gt911::Event::Down { x, y } | gt911::Event::Move { x, y } => self.ui.touch(x, y, true),
                                        gt911::Event::Up => self.ui.touch(0, 0, false),
                                        gt911::Event::Reset => return,
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
                                //   a square canvas: every rotation is free. Suspect until the
                                // axis map is measured
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
                        AppEvent::Command(Command::Pattern) => {
                                //   bring-up: paint a known pattern straight into the live
                                // buffer with the UI paused, so the glass decodes geometry
                                // and data-pin order. Border, horizontal stripes (16 px),
                                // vertical stripes (16 px), then RED | GREEN | BLUE bars
                                self.mode = RenderMode::Paused;
                                if let Some(buf) = self.display.frame_mut() {
                                        let w = DISPLAY_WIDTH as usize;
                                        let h = DISPLAY_HEIGHT as usize;
                                        // SAFETY: the FRAME static is u16-declared, aligned
                                        let px: &mut [u16] = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u16, w * h) };
                                        for y in 0..h {
                                                for x in 0..w {
                                                        let v = if x < 4 || x >= w - 4 || y < 4 || y >= h - 4 {
                                                                0xFFFF
                                                        } else if y < h / 3 {
                                                                if (y / 16) % 2 == 0 { 0xFFFF } else { 0x0000 }
                                                        } else if y < 2 * h / 3 {
                                                                if (x / 16) % 2 == 0 { 0xFFFF } else { 0x0000 }
                                                        } else if x < w / 3 {
                                                                0xF800
                                                        } else if x < 2 * w / 3 {
                                                                0x07E0
                                                        } else {
                                                                0x001F
                                                        };
                                                        px[y * w + x] = v;
                                                }
                                        }
                                        info!("pattern painted: border, h-stripes, v-stripes, R|G|B bars; ui paused (render resume to restore)");
                                } else {
                                        warn!("pattern: frame busy");
                                }
                        }
                        AppEvent::Command(Command::Stats) => {
                                info!(
                                        "display: {} frames drawn, {} skipped, {} beam waits; max draw {} us (scanout: refresh is hardware)",
                                        self.layer.frames(),
                                        self.layer.skipped,
                                        self.beam_waits,
                                        self.draw_us_max
                                );
                                self.draw_us_max = 0;
                        }
                        _ => {}
                }
        }

        /// Whether a draw started NOW cannot collide with the scan. The panel has no back
        /// buffer -- drawing races the beam in the live framebuffer -- but the engine's DMA
        /// read pointer IS the beam, so the race is winnable by scheduling: a partial region
        /// is safe once the beam is past its bottom row (it will not be back for most of a
        /// frame), or far enough above that the draw finishes first; a full-canvas draw
        /// (and any animation step) starts at the wrap and OUTRUNS the beam -- painting
        /// covers rows at ~3x the 31.5 kHz line scan, so the beam only ever reads finished
        /// rows. Tear-free updates for zero bytes of RAM.
        fn beam_safe(&mut self) -> bool {
                //   a whole draw expressed in beam-lines (measured max 5.3 ms at 31.5 kHz,
                // rounded up), and how far past the wrap still counts as "just wrapped"
                // (vblank reads as row 0)
                const DRAW_LINES: u16 = 176;
                const WRAP_LINES: u16 = 16;
                let beam = light_rp2::rgb::beam_row();
                let span = if self.ui.is_animating() {
                        None
                } else {
                        self.ui.dirty_bounds().and_then(|r| self.layer.to_physical(r)).map(|r| (r.y0, r.y1))
                };
                //   the scan is 480 active lines plus 29 of vertical blanking
                const TOTAL_LINES: i32 = 509;
                let safe = match span {
                        Some((top, bottom)) if bottom - top < DISPLAY_HEIGHT - 1 => {
                                //   "past the bottom" counts only with RUNWAY: the beam
                                // re-enters the region's top after the wrap, and a draw longer
                                // than that trip gets lapped -- the scroll flicker that taught
                                // this. A region too tall for any window falls back to the
                                // start-at-the-wrap rule rather than starving
                                let above = beam + DRAW_LINES < top;
                                let past = beam > bottom && TOTAL_LINES - i32::from(beam) + i32::from(top) > i32::from(DRAW_LINES);
                                let possible = i32::from(top) > i32::from(DRAW_LINES)
                                        || TOTAL_LINES - i32::from(bottom) - 1 + i32::from(top) > i32::from(DRAW_LINES);
                                if possible { past || above } else { beam <= WRAP_LINES }
                        }
                        _ => beam <= WRAP_LINES,
                };
                if !safe {
                        self.beam_waits += 1;
                }
                safe
        }

        fn render(&mut self) {
                if self.mode != RenderMode::Normal || (!self.ui.is_dirty() && !self.ui.is_animating()) {
                        return;
                }
                if !self.beam_safe() {
                        // dirty stays set; poll returns Busy and retries within a line or two
                        return;
                }
                let now = light_rp2::now_us();
                let drew = self.ui.render(self.layer, &mut self.display, &self.font, now);
                let done = light_rp2::now_us();
                if drew || self.ui.is_animating() {
                        self.draw_us_max = self.draw_us_max.max(done - now);
                }
        }
}

impl Module for DisplayMod {
        fn name(&self) -> &'static str {
                "display"
        }
        fn load(&mut self) -> Result<(), ()> {
                //   the panel and the scanout engine are already running (board::take);
                // the driver's init is a formality
                let mut clock = SysClock;
                self.display.init(&mut clock);
                self.layer.set_frame_rate(FPS);
                self.layer.bg = BG;
                //   the buffer is live on the glass: a cleared frame flashes black under the
                // beam before the repaint reaches it, so every frame draws OVER the last --
                // the window interiors cover what the clear used to
                self.layer.draw_over = true;
                self.ui.fit(self.layer);
                if let Err(e) = self.ui.navigate(&PAGE_MAIN) {
                        warn!("the main page did not build: {e:?}");
                }
                self.ui.invalidate_all();
                self.render();
                info!(
                        "display up: {}x{} ST7701S over the RGB scanout (hardware refresh), single-buffered at {} fps, font {}px ({} glyphs, {} bytes)",
                        DISPLAY_WIDTH,
                        DISPLAY_HEIGHT,
                        FPS,
                        self.font.pixel_size(),
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
                while let Some(ev) = EVENTS.poll(&self.events) {
                        self.handle(ev);
                }
                self.render();
                if self.ui.is_dirty() || self.ui.is_animating() { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                info!("display down");
        }
}

struct TouchMod {
        touch: Gt911<&'static RefCell<I2c1>, Input>,
        tracker: Tracker,
        events: Subscription,
        moves: u32,
}

impl Module for TouchMod {
        fn name(&self) -> &'static str {
                "touch"
        }
        fn load(&mut self) -> Result<(), ()> {
                match self.touch.probe() {
                        Ok(Some(id)) => info!("gt911 answering on i2c1 (id {:?})", core::str::from_utf8(&id).unwrap_or("?")),
                        Ok(None) => warn!("touch controller answered with an unexpected id"),
                        Err(e) => warn!("gt911 did not answer the probe: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Command(Command::Stats) => {
                                        info!("touch: {} failed reads ({} nack, {} timeout, {} bus)", self.touch.failures, self.touch.nacks, self.touch.timeouts, self.touch.bus_errors);
                                }
                                AppEvent::Ui(UiAction::DragConsumed) => self.tracker.suppress(),
                                _ => {}
                        }
                }
                let now_ms = (light_rp2::now_us() / 1000) as u32;
                let Some(ev) = self.touch.poll(now_ms) else { return Poll::Idle };
                match ev {
                        gt911::Event::Down { x, y } => {
                                self.moves = 0;
                                debug!("touch down at {x},{y}");
                        }
                        gt911::Event::Up => debug!("touch up after {} moves at {},{}", self.moves, self.touch.x, self.touch.y),
                        gt911::Event::Move { .. } => self.moves += 1,
                        gt911::Event::Reset => {}
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

struct RtcMod {
        rtc: Pcf85063a<&'static RefCell<I2c1>>,
        events: Subscription,
}

impl RtcMod {
        fn report(&mut self) {
                match self.rtc.now() {
                        Ok((t, kept)) => info!(
                                "rtc: {:04}-{:02}-{:02} {:02}:{:02}:{:02} (weekday {}){}",
                                t.year,
                                t.month,
                                t.day,
                                t.hour,
                                t.minute,
                                t.second,
                                t.weekday,
                                if kept { "" } else { " UNSET since power loss" }
                        ),
                        Err(e) => warn!("rtc read failed: {e:?}"),
                }
        }
}

impl Module for RtcMod {
        fn name(&self) -> &'static str {
                "rtc"
        }
        fn load(&mut self) -> Result<(), ()> {
                match self.rtc.init() {
                        Ok(()) => self.report(),
                        Err(e) => warn!("pcf85063a did not answer: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Command(Command::Stats) | AppEvent::Command(Command::RtcShow) => {
                                        busy = true;
                                        self.report();
                                }
                                AppEvent::Command(Command::RtcSet(t)) => {
                                        busy = true;
                                        match self.rtc.set(&t) {
                                                Ok(()) => self.report(),
                                                Err(e) => warn!("rtc set failed: {e:?}"),
                                        }
                                }
                                _ => {}
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}

struct BoardMod {
        backlight: light_rp2::pwm::PwmOutput,
        battery: Adc,
        charging: Input,
        charge_done: Input,
        /// Held for the stall diagnostic; the engine otherwise needs nothing.
        scanout: light_rp2::rgb::RgbScanout,
        events: Subscription,
}

impl BoardMod {
        fn apply(&mut self, level: u16) {
                //   plain linear for bring-up; whether this panel's driver has a threshold
                // band like the 3.49's is measured with a sweep, not assumed
                let duty = if BACKLIGHT_INVERTED { BACKLIGHT_LEVEL_MAX - level.min(BACKLIGHT_LEVEL_MAX) } else { level };
                self.backlight.set_duty(duty);
        }
}

impl Module for BoardMod {
        fn name(&self) -> &'static str {
                "board"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.apply(BACKLIGHT_LEVEL_MAX);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Command(Command::Backlight(level)) => {
                                        busy = true;
                                        self.apply(level);
                                        info!("backlight {level}");
                                }
                                AppEvent::Command(Command::Stats) => {
                                        let raw = u32::from(self.battery.read());
                                        let mv = raw * 3300 * BATTERY_DIVIDER / 4096;
                                        let state = if self.charge_done.is_low() {
                                                "charge done"
                                        } else if self.charging.is_low() {
                                                "charging"
                                        } else {
                                                "on battery/full"
                                        };
                                        info!("battery: {mv} mV (raw {raw}), {state}");
                                        //   the scanout's pulse over 5 ms -- SHORTER than a
                                        // frame, because the progress counter wraps per
                                        // frame and a longer window aliases (the bring-up's
                                        // false "386 kpix/s"). Healthy: ~14,000 kpix/s
                                        let a = self.scanout.frame_progress();
                                        let t0 = light_rp2::now_us();
                                        while light_rp2::now_us() - t0 < 5_000 {
                                                core::hint::spin_loop();
                                        }
                                        let b = self.scanout.frame_progress();
                                        let total = u32::from(DISPLAY_WIDTH) * u32::from(DISPLAY_HEIGHT);
                                        let consumed = if a >= b { a - b } else { a + (total - b) };
                                        info!(
                                                "scanout: {} kpix/s; data machine {} since last stats",
                                                consumed / 5,
                                                if self.scanout.data_stalled() { "STARVED" } else { "kept fed" }
                                        );
                                        let v = self.scanout.dma_view();
                                        info!("scanout dma: reading {:#010x}, frame base {:#010x} (ctrl word at {:#010x})", v[0], v[1], v[2]);
                                }
                                AppEvent::Command(Command::Scan) => {
                                        busy = true;
                                        let p = self.scanout.pad_state();
                                        let d = self.scanout.debug_state();
                                        info!("pio1 padout {:#010x} padoe {:#010x}; pio2 padout {:#010x} padoe {:#010x}", p[0], p[1], p[2], p[3]);
                                        info!("sio gpio_in {:#010x} hi {:#010x}", p[4], p[5]);
                                        info!("pcs: hsync {} vsync {} de {} rgb {}; fstat pio1 {:#010x} pio2 {:#010x}", d[0], d[1], d[2], d[3], d[4], d[5]);
                                }
                                _ => {}
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.apply(0);
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

fn parse_render(w: &mut Words) -> Parsed<Command> {
        match w.next() {
                Some("pause") => Parsed::Event(Command::RenderMode(RenderMode::Paused)),
                Some("resume") => Parsed::Event(Command::RenderMode(RenderMode::Normal)),
                _ => Parsed::Usage,
        }
}

fn split3(s: &str, sep: char) -> Option<(u16, u8, u8)> {
        let mut it = s.split(sep);
        let a = it.next()?.parse().ok()?;
        let b = it.next()?.parse().ok()?;
        let c = it.next()?.parse().ok()?;
        if it.next().is_some() {
                return None;
        }
        Some((a, b, c))
}

/// Zeller's congruence, mapped to 0 = Sunday, the register's convention.
fn weekday(y: u16, m: u8, d: u8) -> u8 {
        let (mut y, mut m) = (i32::from(y), i32::from(m));
        if m < 3 {
                m += 12;
                y -= 1;
        }
        let (k, j) = (y % 100, y / 100);
        let h = (i32::from(d) + 13 * (m + 1) / 5 + k + k / 4 + j / 4 + 5 * j) % 7;
        ((h + 6) % 7) as u8
}

fn parse_rtc(w: &mut Words) -> Parsed<Command> {
        match w.next() {
                None => Parsed::Event(Command::RtcShow),
                Some("set") => {
                        let (Some(date), Some(time)) = (w.next(), w.next()) else { return Parsed::Usage };
                        let Some((year, month, day)) = split3(date, '-') else { return Parsed::Usage };
                        let Some((hour, minute, second)) = split3(time, ':') else { return Parsed::Usage };
                        let valid = (1..=12).contains(&month) && (1..=31).contains(&day) && hour < 24 && minute < 60 && second < 60 && (1970..=2069).contains(&year);
                        if !valid {
                                return Parsed::Usage;
                        }
                        Parsed::Event(Command::RtcSet(Datetime {
                                year,
                                month,
                                day,
                                weekday: weekday(year, month, day),
                                hour: hour as u8,
                                minute: minute as u8,
                                second: second as u8,
                        }))
                }
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[CliCommand<Command>] = &[
        CliCommand { name: "stats", usage: "stats", parse: parse_stats },
        CliCommand { name: "rtc", usage: "rtc | rtc set YYYY-MM-DD HH:MM:SS", parse: parse_rtc },
        CliCommand { name: "backlight", usage: "backlight 0..1000", parse: parse_backlight },
        CliCommand { name: "ui", usage: "ui focus next|prev | ui activate | ui press X Y | ui back", parse: parse_ui },
        CliCommand { name: "render", usage: "render pause|resume", parse: parse_render },
        CliCommand { name: "pattern", usage: "pattern", parse: |_| Parsed::Event(Command::Pattern) },
        CliCommand { name: "scan", usage: "scan", parse: |_| Parsed::Event(Command::Scan) },
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

        let fb: &'static mut [u16; FRAME_PIXELS] = FRAME.take();
        let fb_ptr = fb.as_ptr();
        let p = take(&clocks, fb_ptr).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, peri {} Hz; i2c1 at {} Hz; scanout running", clocks.sys_hz, clocks.peri_hz, p.i2c1.actual_hz);

        // SAFETY: the same static, viewed as bytes for the display stack; the u16
        // declaration guarantees the alignment the scanout DMA needs
        let fb_bytes: &'static mut [u8] = unsafe { core::slice::from_raw_parts_mut(fb.as_mut_ptr() as *mut u8, FRAME_PIXELS * 2) };
        let display = Display::new(Scanout, fb_bytes, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };

        //   touch, IMU and RTC all share i2c1
        static I2C1_CELL: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let i2c1: &'static RefCell<I2c1> = I2C1_CELL.init(RefCell::new(p.i2c1));
        let touch = Gt911::new(i2c1, p.touch_int, TOUCH_MAP, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(i2c1));

        let mut board_mod = BoardMod { backlight: p.backlight, battery: p.battery, charging: p.charging, charge_done: p.charge_done, scanout: p.scanout, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut imu_mod = ImuMod { imu, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut rtc_mod = RtcMod { rtc: Pcf85063a::new(i2c1), events: EVENTS.subscribe().expect("subscriber slot") };
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
                beam_waits: 0,
        });
        static TOUCH_MOD: StaticCell<TouchMod> = StaticCell::new();
        let touch_mod = TOUCH_MOD.init(TouchMod { touch, tracker: Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), events: EVENTS.subscribe().expect("subscriber slot"), moves: 0 });
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<6> = Runtime::new();
        rt.add(&mut board_mod).expect("capacity");
        rt.add(display_mod).expect("capacity");
        rt.add(touch_mod).expect("capacity");
        rt.add(&mut imu_mod).expect("capacity");
        rt.add(&mut rtc_mod).expect("capacity");
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
