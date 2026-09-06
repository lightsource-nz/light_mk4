//! The touch4 firmware: the widget demo on the Waveshare RP2350-Touch-LCD-4 -- 480x480 of
//! RGB (DPI) glass with no GDDRAM, the ST7701S's first bring-up, and the first board fed by
//! `light_rp2::rgb`'s pure-hardware scanout loop. The application is `light_app_ui_demo`;
//! this crate is the tangible 4.0: the scanout, the beam-racing render gate, the GT911,
//! the IMU, the RTC, the battery, the PSRAM probe, the shell ABI and the panic handler.
//!
//! The display architecture is the leg's point: the framebuffer IS the panel. One 450 KB
//! RGB565 buffer in SRAM (single-buffered, the bring-up decision -- the flip hook exists
//! for a second buffer if one ever finds room), scanned out by DMA+PIO forever; the
//! display stack runs over `light_display::scanout::Scanout`, whose every update completes
//! the moment it starts. Drawing races the scan; the render gate schedules around the beam.
//!
//! Bring-up checklist: the panel lights and draws (if dark: backlight polarity first, then
//! the ST7701S init, then scanout timing); GT911 answers on i2c1 and coordinates track,
//! with directions AND the square panel's possible axis swap measured on the glass; the
//! IMU axis map is IDENTITY-until-measured.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_app_ui_demo as demo;
use demo::{demo_commands, demo_pages, BoardHook, Command, DemoEvent, DemoView, DisplayConfig, DisplayMod, RenderMode};
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{debug, info, log, warn, ConstStaticCell, EventBus, InputPin, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::scanout::Scanout;
use light_display::{Display, FrameLayer};
use light_draw::{PixelFormat, Rotation};
use light_font::Font;
use light_input::gt911::{self, Gt911};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_input::touch::Tracker;
use light_rtc::{Datetime, Pcf85063a};
use light_ui::{Theme, Ui};
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
        /// From this board's psram_info.c: what the SDK's runtime init detected on CS1.
        fn light_board_psram_size() -> u32;
}

/// The XIP CS1 window, through the UNCACHED alias -- a memtest through the cache would
/// largely test the cache.
const PSRAM_UNCACHED_BASE: u32 = 0x1500_0000;

/// Write-and-read three 4 KB regions (start, middle, end) with an address-derived pattern.
/// Returns (words checked, mismatches).
fn psram_test(size: u32) -> (u32, u32) {
        let (mut checked, mut bad) = (0u32, 0u32);
        for base in [0u32, size / 2, size.saturating_sub(4096)] {
                let p = (PSRAM_UNCACHED_BASE + base) as *mut u32;
                for i in 0..1024usize {
                        // SAFETY: within the detected PSRAM window; nothing else lives there
                        unsafe { core::ptr::write_volatile(p.add(i), (base ^ i as u32).wrapping_mul(0x9E37_79B9)) };
                }
                for i in 0..1024usize {
                        let want = (base ^ i as u32).wrapping_mul(0x9E37_79B9);
                        // SAFETY: as above
                        if unsafe { core::ptr::read_volatile(p.add(i)) } != want {
                                bad += 1;
                        }
                        checked += 1;
                }
        }
        (checked, bad)
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
/// The look-and-feel: the framework's steel theme, the default for every board with
/// color support. A board-specific override would be a local theme file extending it.
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));

// --- the event bus --------------------------------------------------------------------------

/// This board's extension events, riding the demo's bus.
#[derive(Clone, Copy, Debug)]
enum Ext {
        /// The scanout's pad and state-machine snapshot, for the wiring bisect.
        Scan,
        /// Probe and memtest whatever PSRAM the runtime detected on CS1.
        Psram,
        RtcShow,
        RtcSet(Datetime),
        /// Paint the bring-up test pattern straight into the live buffer, UI paused.
        Pattern,
}

type AppEvent = DemoEvent<Ext>;

static EVENTS: EventBus<AppEvent, 16, 5> = EventBus::new();

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
                demo::push_console_byte(b as u8);
        }
}

// --- the interface, as data ---------------------------------------------------------------

const FPS: u32 = 30;
const BACKLIGHT_DIM: u16 = 250;

demo_pages! {
        event: AppEvent,
        title: "mk4 4.0",
        row_gap: 8,
        list_min_row: 64,
        backlight_dim: BACKLIGHT_DIM
}

/// A square canvas: every rotation is free. Suspect until the axis map is measured.
fn rotation_map(o: Orientation) -> Option<Rotation> {
        match o {
                Orientation::Portrait => Some(Rotation::R0),
                Orientation::PortraitFlip => Some(Rotation::R180),
                Orientation::LandscapeL => Some(Rotation::R270),
                Orientation::LandscapeR => Some(Rotation::R90),
                _ => None,
        }
}

// --- the driver-specific edges -------------------------------------------------------------

/// The scanout's render scheduling and bring-up instruments. No init or unload edges: the
/// panel and the engine are already running when the module loads (board::take), and the
/// buffer is live on the glass -- nothing to clear that would not flash.
struct Hook {
        /// Renders deferred because the beam was inside the dirty area -- the tear-free
        /// gate's pulse.
        beam_waits: u32,
}

impl BoardHook<Scanout, Ext> for Hook {
        /// Whether a draw started NOW cannot collide with the scan. The panel has no back
        /// buffer -- drawing races the beam in the live framebuffer -- but the engine's DMA
        /// read pointer IS the beam, so the race is winnable by scheduling: a partial region
        /// is safe once the beam is past its bottom row (it will not be back for most of a
        /// frame), or far enough above that the draw finishes first; a full-canvas draw
        /// (and any animation step) starts at the wrap and OUTRUNS the beam -- painting
        /// covers rows at ~3x the 31.5 kHz line scan, so the beam only ever reads finished
        /// rows. Tear-free updates for zero bytes of RAM.
        fn render_gate(&mut self, ui: &Ui<AppEvent, { demo::UI_WIDGETS }>, layer: &FrameLayer) -> bool {
                //   a whole draw expressed in beam-lines (measured max 5.3 ms at 31.5 kHz,
                // rounded up), and how far past the wrap still counts as "just wrapped"
                // (vblank reads as row 0)
                const DRAW_LINES: u16 = 176;
                const WRAP_LINES: u16 = 16;
                let beam = light_rp2::rgb::beam_row();
                let span = if ui.is_animating() {
                        None
                } else {
                        ui.dirty_bounds().and_then(|r| layer.to_physical(r)).map(|r| (r.y0, r.y1))
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

        fn on_stats(&mut self) {
                info!("scanout: {} beam waits (refresh is hardware)", self.beam_waits);
        }

        fn on_ext(&mut self, view: &mut DemoView<'_, Scanout, Ext>, ext: Ext) {
                match ext {
                        Ext::Pattern => {
                                //   bring-up: paint a known pattern straight into the live
                                // buffer with the UI paused, so the glass decodes geometry
                                // and data-pin order. Border, horizontal stripes (16 px),
                                // vertical stripes (16 px), then RED | GREEN | BLUE bars
                                *view.mode = RenderMode::Paused;
                                if let Some(buf) = view.display.frame_mut() {
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
                        //   Scan and Psram are the board module's; the display hook only
                        // carries what needs the display
                        _ => {}
                }
        }
}

// --- the board's own modules ---------------------------------------------------------------

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
                                AppEvent::Ui(demo::UiAction::DragConsumed) => self.tracker.suppress(),
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
                                AppEvent::Command(Command::Stats) | AppEvent::Ext(Ext::RtcShow) => {
                                        busy = true;
                                        self.report();
                                }
                                AppEvent::Ext(Ext::RtcSet(t)) => {
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
                                AppEvent::Ext(Ext::Scan) => {
                                        busy = true;
                                        let p = self.scanout.pad_state();
                                        let d = self.scanout.debug_state();
                                        info!("pio1 padout {:#010x} padoe {:#010x}; pio2 padout {:#010x} padoe {:#010x}", p[0], p[1], p[2], p[3]);
                                        info!("sio gpio_in {:#010x} hi {:#010x}", p[4], p[5]);
                                        info!("pcs: hsync {} vsync {} de {} rgb {}; fstat pio1 {:#010x} pio2 {:#010x}", d[0], d[1], d[2], d[3], d[4], d[5]);
                                }
                                AppEvent::Ext(Ext::Psram) => {
                                        busy = true;
                                        let size = unsafe { light_board_psram_size() };
                                        if size == 0 {
                                                info!("psram: none detected");
                                        } else {
                                                let (checked, bad) = psram_test(size);
                                                info!("psram: {} KB detected; {} words tested, {} mismatches", size / 1024, checked, bad);
                                        }
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

// --- the console table ---------------------------------------------------------------------

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

fn parse_rtc(w: &mut Words) -> Parsed<AppEvent> {
        match w.next() {
                None => Parsed::Event(DemoEvent::Ext(Ext::RtcShow)),
                Some("set") => {
                        let (Some(date), Some(time)) = (w.next(), w.next()) else { return Parsed::Usage };
                        let Some((year, month, day)) = split3(date, '-') else { return Parsed::Usage };
                        let Some((hour, minute, second)) = split3(time, ':') else { return Parsed::Usage };
                        let valid = (1..=12).contains(&month) && (1..=31).contains(&day) && hour < 24 && minute < 60 && second < 60 && (1970..=2069).contains(&year);
                        if !valid {
                                return Parsed::Usage;
                        }
                        Parsed::Event(DemoEvent::Ext(Ext::RtcSet(Datetime {
                                year,
                                month,
                                day,
                                weekday: weekday(year, month, day),
                                hour: hour as u8,
                                minute: minute as u8,
                                second: second as u8,
                        })))
                }
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[CliCommand<AppEvent>] = &demo_commands![Ext;
        CliCommand { name: "rtc", usage: "rtc | rtc set YYYY-MM-DD HH:MM:SS", parse: parse_rtc },
        CliCommand { name: "pattern", usage: "pattern", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Pattern)) },
        CliCommand { name: "scan", usage: "scan", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Scan)) },
        CliCommand { name: "psram", usage: "psram", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Psram)) },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

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
        //   Rgb565Le, NOT Rgb565: the scanout DMA reads this buffer as native u16s, where
        // the push panels take big-endian bytes down a wire. The white-on-black bring-up
        // could not see the difference (those colors are byte-swap invariant); the steel
        // theme's first showing could
        let display = Display::new(Scanout, fb_bytes, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565Le, light_rp2::now_us);
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
        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565Le));
        static UI: ConstStaticCell<Ui<AppEvent, { demo::UI_WIDGETS }>> = ConstStaticCell::new(Ui::new());
        let layer: &'static mut FrameLayer = LAYER.take();
        let ui: &'static mut Ui<AppEvent, { demo::UI_WIDGETS }> = UI.take();
        //   the look-and-feel, from the embedded blob: a bad blob is a build-system bug
        // worth halting on, not styling to guess past
        let theme = match Theme::parse(THEME_BLOB) {
                Ok(t) => t,
                Err(e) => panic!("the embedded theme does not parse: {e:?}"),
        };
        layer.bg = theme.bg;
        ui.set_theme(theme);
        ui.set_font(&font);
        type BoardDisplayMod = DisplayMod<Scanout, SysClock, Ext, Hook>;
        static DISPLAY_MOD: StaticCell<BoardDisplayMod> = StaticCell::new();
        let display_mod = DISPLAY_MOD.init(DisplayMod::new(
                display,
                layer,
                font,
                ui,
                SysClock,
                &EVENTS,
                DisplayConfig {
                        width: DISPLAY_WIDTH,
                        height: DISPLAY_HEIGHT,
                        fps: FPS,
                        desc: "ST7701S over the RGB scanout (hardware refresh), single-buffered",
                        //   a memoryless scanout has no push to repeat
                        repush: false,
                        //   the buffer is live on the glass: a cleared frame flashes black
                        // under the beam before the repaint reaches it, so every frame draws
                        // OVER the last -- the window interiors cover what the clear used to
                        draw_over: true,
                        rotation_map,
                        main_page: &PAGE_MAIN,
                },
                Hook { beam_waits: 0 },
        ));
        static TOUCH_MOD: StaticCell<TouchMod> = StaticCell::new();
        let touch_mod = TOUCH_MOD.init(TouchMod { touch, tracker: Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), events: EVENTS.subscribe().expect("subscriber slot"), moves: 0 });
        let mut console_mod = demo::ConsoleMod::new(&CLI, &EVENTS);

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
