//! The Rust side of the touch349 firmware: the widget demo on the Waveshare
//! RP2350-Touch-LCD-3.49 -- a 172x640 bar of glass, the first QSPI panel (AXS15231B, LCD and
//! touch in one chip), and the first board on the RP2350's upper GPIO bank.
//!
//! Bring-up checklist (this board's support is authored from Waveshare's reference demo, not
//! a schematic): the panel lights and draws (if dark, SLPOUT/DISPON are the first suspects
//! -- see the driver); touch answers on i2c0 and coordinates track, with the axis directions
//! measured on the glass; the backlight really is inverted; the IMU axis map is
//! IDENTITY-until-measured, so orientation is suspect until calibrated.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_input::axs15231b::{self as axs, Axs15231bTouch};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_display::axs15231b::Axs15231b;
use light_input::touch::{Gesture, Tracker};
use light_ui::{scroll, Desc, Page, SwipeDir, Touch, Ui};
use light_core::cli::{Cli, Command as CliCommand, Outcome, Parsed, Words};
use light_core::{debug, info, log, warn, ConstStaticCell, EventBus, InputPin, LineReader, Mailbox, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer, UpdateError};
use light_draw::{PixelFormat, Rotation};
use light_audio::Es8311;
use light_font::Font;
use light_rtc::{Datetime, Pcf85063a};
mod board;
use board::*;
use light_rp2::adc::Adc;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::{I2c0, I2c1};
use light_rp2::i2s::PioI2sOut;
use light_rp2::qspi::PioQspiDisplayBus;
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

/// Two frame buffers, 215 KB each -- 430 KB of the RP2350's 520: tight but linkable. If a
/// later addition overflows SRAM, the back buffer is the thing to give up (single-buffered
/// costs the animations), or the PSRAM on CS1 is the thing to bring up.
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
static FRAME_BACK: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));

// --- the event bus --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        Touch(axs::Event),
        Gesture(Gesture),
        Orientation(Orientation),
        Command(Command),
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
        RtcShow,
        RtcSet(Datetime),
        Tone { hz: u16, ms: u16 },
        ToneOff,
        Volume(u8),
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

static EVENTS: EventBus<AppEvent, 16, 6> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();
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

/// Bar glass, corners unmeasured: 0 until the glass says otherwise.
const CORNER_RADIUS: u8 = 0;
const ROW_GAP: u8 = 6;
/// A 640-tall list has room; rows sized for a finger on the narrow bar.
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
static MAIN_WINDOW: Desc<AppEvent> = Desc::window("mk4 3.49").rounded(CORNER_RADIUS).stack(ROW_GAP).children(&[&BTN_ALPHA, &BTN_BETA, &BTN_GAMMA, &BTN_MORE, &BTN_LIST]);

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
        display: Display<'static, Axs15231b<PioQspiDisplayBus>>,
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
                                        if let axs::Event::Down { .. } = t {
                                                if !self.display.busy() {
                                                        let _ = self.display.update_async(light_display::Region::full(DISPLAY_WIDTH, DISPLAY_HEIGHT));
                                                }
                                        }
                                }
                                let outcome = match t {
                                        axs::Event::Down { x, y } | axs::Event::Move { x, y } => self.ui.touch(x, y, true),
                                        axs::Event::Up => self.ui.touch(0, 0, false),
                                        axs::Event::Reset => return,
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
                                //   portrait only, MEASURED: the bar rests near-landscape on
                                // its long edge, so ordinary handling flapped LandscapeL/R --
                                // a 180-degree relayout per touch, and every tap then landed
                                // where a widget used to be. A 172 px-tall landscape canvas
                                // was never worth having anyway; the bar rotates end-for-end
                                // (a deliberate gesture, nowhere near the resting pose) and
                                // otherwise holds still
                                let rotation = match o {
                                        Orientation::Portrait => Some(Rotation::R0),
                                        Orientation::PortraitFlip => Some(Rotation::R180),
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
                self.layer.set_frame_rate(FPS);
                self.layer.bg = BG;
                self.ui.fit(self.layer);
                if let Err(e) = self.ui.navigate(&PAGE_MAIN) {
                        warn!("the main page did not build: {e:?}");
                }
                self.ui.invalidate_all();
                self.render();
                info!(
                        "display up: {}x{} AXS15231B over PIO-QSPI, double-buffered at {} fps, font {}px ({} glyphs, {} bytes)",
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
                info!("display down");
        }
}

/// Owns the AXS15231B's touch half: no reset line of its own (the panel's reset is the
/// chip's), so a wedge is reported, never reset from here.
struct TouchMod {
        touch: Axs15231bTouch<I2c0, Input>,
        tracker: Tracker,
        events: Subscription,
        moves: u32,
}

impl Module for TouchMod {
        fn name(&self) -> &'static str {
                "touch"
        }
        fn load(&mut self) -> Result<(), ()> {
                //   after the display module's load has reset and initialised the shared
                // chip; the touch read is the only probe this protocol offers
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
                        Ok(()) => info!("axs15231b touch answering on i2c0"),
                        Err(e) => warn!("axs15231b touch did not answer the probe: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Command(Command::Stats) => {
                                        info!(
                                                "touch: {} failed reads ({} nack, {} timeout, {} bus)",
                                                self.touch.failures,
                                                self.touch.nacks,
                                                self.touch.timeouts,
                                                self.touch.bus_errors
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
                        axs::Event::Down { x, y } => {
                                self.moves = 0;
                                debug!("touch down at {x},{y}");
                        }
                        axs::Event::Up => debug!("touch up after {} moves at {},{}", self.moves, self.touch.x, self.touch.y),
                        axs::Event::Move { .. } => self.moves += 1,
                        axs::Event::Reset => {}
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

/// The PCF85063A on the shared i2c1, beside the IMU. Battery-backed: it keeps time across
/// power-off, and says so -- the oscillator-stop flag marks a time nobody set.
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

/// One period of sine at 20000 amplitude, 32 steps -- plenty for a bring-up beeper.
static SINE: [i16; 32] = [
        0, 3902, 7654, 11111, 14142, 16629, 18478, 19616, 20000, 19616, 18478, 16629, 14142, 11111, 7654, 3902, 0, -3902, -7654, -11111, -14142, -16629, -18478, -19616, -20000, -19616, -18478, -16629,
        -14142, -11111, -7654, -3902,
];

/// The ES8311 codec on the shared i2c1 plus the PIO I2S transport: a tone generator for
/// bring-up, fed through the transport's ping-pong DMA stream (silence when nothing
/// plays), so a long frame draw cannot starve the codec into audible chop.
struct AudioMod {
        codec: Es8311<&'static RefCell<I2c1>>,
        i2s: PioI2sOut,
        pa: Output,
        events: Subscription,
        /// Phase accumulator into [`SINE`]; the top 5 bits index the table.
        phase: u32,
        phase_inc: u32,
        /// Sample frames left to play; 0 is silence.
        remaining: u32,
}

impl Module for AudioMod {
        fn name(&self) -> &'static str {
                "audio"
        }
        fn load(&mut self) -> Result<(), ()> {
                match self.codec.probe() {
                        Ok(Some(id)) => info!("es8311 chip id confirmed: 0x{id:04x}"),
                        Ok(None) => warn!("es8311 answered with an unexpected chip id"),
                        Err(e) => warn!("es8311 did not answer the chip id read: {e:?}"),
                }
                let mut clock = SysClock;
                if let Err(e) = self.codec.init(AUDIO_SAMPLE_HZ, &mut clock) {
                        warn!("es8311 init failed: {e:?}");
                        return Ok(());
                }
                let _ = self.codec.set_volume(73);
                static STREAM_A: ConstStaticCell<[u32; light_rp2::i2s::STREAM_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::STREAM_WORDS]);
                static STREAM_B: ConstStaticCell<[u32; light_rp2::i2s::STREAM_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::STREAM_WORDS]);
                self.i2s.start_stream([STREAM_A.take(), STREAM_B.take()]);
                self.pa.set(true);
                info!("audio up: es8311 master at {} Hz, PIO1 mclk+dout; the UART console pins now carry audio", AUDIO_SAMPLE_HZ);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Command(Command::Tone { hz, ms }) => {
                                        self.phase_inc = ((u64::from(hz) << 32) / u64::from(AUDIO_SAMPLE_HZ)) as u32;
                                        self.remaining = u32::from(ms) * AUDIO_SAMPLE_HZ / 1000;
                                        info!("tone {hz} Hz for {ms} ms");
                                }
                                AppEvent::Command(Command::ToneOff) => {
                                        self.remaining = 0;
                                        info!("tone off");
                                }
                                AppEvent::Command(Command::Volume(v)) => match self.codec.set_volume(v) {
                                        Ok(()) => info!("volume {v}"),
                                        Err(e) => warn!("volume set failed: {e:?}"),
                                },
                                AppEvent::Command(Command::Stats) => {
                                        info!("audio: {} stream underruns", self.i2s.underruns);
                                }
                                _ => {}
                        }
                }
                let playing = self.remaining > 0;
                //   pre-borrowed so the closure captures fields disjoint from self.i2s
                let phase = &mut self.phase;
                let inc = self.phase_inc;
                let remaining = &mut self.remaining;
                self.i2s.refill(|buf| {
                        let mut i = 0;
                        while i + 1 < buf.len() {
                                let s = if *remaining > 0 { SINE[(*phase >> 27) as usize] } else { 0 };
                                let w = (s as u16 as u32) << 16;
                                buf[i] = w;
                                buf[i + 1] = w;
                                if *remaining > 0 {
                                        *phase = phase.wrapping_add(inc);
                                        *remaining -= 1;
                                }
                                i += 2;
                        }
                });
                if playing { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.pa.set(false);
        }
}

struct BoardMod {
        backlight: light_rp2::pwm::PwmOutput,
        /// The power latch: high since board::take(); driven low on unload = power off.
        sys_en: Output,
        /// The side button, low when pressed; held [`POWER_OFF_HOLD_MS`] = shutdown.
        button: Input,
        battery: Adc,
        pressed_since_ms: Option<u32>,
        events: Subscription,
}

impl BoardMod {
        fn apply(&mut self, level: u16) {
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
                                        //   12-bit read across 3.3 V behind the divider
                                        let raw = u32::from(self.battery.read());
                                        let mv = raw * 3300 * BATTERY_DIVIDER / 4096;
                                        info!("battery: {mv} mV (raw {raw})");
                                }
                                _ => {}
                        }
                }
                //   the side button: the reference's 1.5 s hold is the power-off gesture. The
                // shutdown flows through the runtime like the console's `quit`, so every
                // module unloads before this module's unload releases the power latch
                let now_ms = (light_rp2::now_us() / 1000) as u32;
                if self.button.is_low() {
                        let since = *self.pressed_since_ms.get_or_insert(now_ms);
                        if now_ms.wrapping_sub(since) >= POWER_OFF_HOLD_MS {
                                info!("power button held; shutting down");
                                return Poll::Shutdown;
                        }
                } else {
                        self.pressed_since_ms = None;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.apply(0);
                //   on battery this is the power-off; on USB the rails stay up and the
                // runtime parks in the idle loop
                self.sys_en.set(false);
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

fn parse_tone(w: &mut Words) -> Parsed<Command> {
        match w.next() {
                Some("off") => Parsed::Event(Command::ToneOff),
                Some(hz) => {
                        let Ok(hz) = hz.parse::<u16>() else { return Parsed::Usage };
                        if !(20..=10_000).contains(&hz) {
                                return Parsed::Usage;
                        }
                        let ms = match w.next() {
                                None => 500,
                                Some(ms) => match ms.parse::<u16>() {
                                        Ok(ms) if ms > 0 => ms,
                                        _ => return Parsed::Usage,
                                },
                        };
                        Parsed::Event(Command::Tone { hz, ms })
                }
                None => Parsed::Usage,
        }
}

fn parse_volume(w: &mut Words) -> Parsed<Command> {
        match w.next().and_then(|s| s.parse::<u8>().ok()) {
                Some(v) if v <= 100 => Parsed::Event(Command::Volume(v)),
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[CliCommand<Command>] = &[
        CliCommand { name: "stats", usage: "stats", parse: parse_stats },
        CliCommand { name: "rtc", usage: "rtc | rtc set YYYY-MM-DD HH:MM:SS", parse: parse_rtc },
        CliCommand { name: "tone", usage: "tone HZ [MS] | tone off", parse: parse_tone },
        CliCommand { name: "volume", usage: "volume 0..100", parse: parse_volume },
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
        info!("clocks: sys {} Hz, peri {} Hz; touch i2c0 at {} Hz, imu i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.touch_bus.actual_hz, p.imu_bus.actual_hz);

        let front: &'static mut [u8] = FRAME_FRONT.take();
        let back: &'static mut [u8] = FRAME_BACK.take();
        let mut display = Display::new(Axs15231b::new(p.display_bus), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        display.set_back_buffer(back);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        //   the IMU has i2c1 to itself on this board, but the RefCell keeps the same shape
        // as the other boards' shared-bus wiring
        static IMU_I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let imu_i2c: &'static RefCell<I2c1> = IMU_I2C.init(RefCell::new(p.imu_bus));
        let touch = Axs15231bTouch::new(p.touch_bus, p.touch_int, TOUCH_MAP, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(imu_i2c));

        let mut board_mod = BoardMod {
                backlight: p.backlight,
                sys_en: p.sys_en,
                button: p.power_button,
                battery: p.battery,
                pressed_since_ms: None,
                events: EVENTS.subscribe().expect("subscriber slot"),
        };
        let mut imu_mod = ImuMod { imu, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut rtc_mod = RtcMod { rtc: Pcf85063a::new(imu_i2c), events: EVENTS.subscribe().expect("subscriber slot") };
        let mut audio_mod = AudioMod {
                codec: Es8311::new(imu_i2c),
                i2s: p.i2s,
                pa: p.audio_pa,
                events: EVENTS.subscribe().expect("subscriber slot"),
                phase: 0,
                phase_inc: 0,
                remaining: 0,
        };
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

        let mut rt: Runtime<7> = Runtime::new();
        rt.add(&mut board_mod).expect("capacity");
        rt.add(display_mod).expect("capacity");
        rt.add(touch_mod).expect("capacity");
        rt.add(&mut imu_mod).expect("capacity");
        rt.add(&mut rtc_mod).expect("capacity");
        rt.add(&mut audio_mod).expect("capacity");
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
