//! The touch169 firmware: the widget demo on the Waveshare RP2350-Touch-LCD-1.69. The
//! application is `light_app_ui_demo`, with no hardware in it; this crate is the tangible
//! 1.69: the wiring (`board.rs`), the ST7789 over SPI, the CST816T, the IMU, the piezo,
//! the shell ABI and the panic handler.
//!
//! The C shell (`module/light_mk4_shell`) brings the pico-sdk runtime up, puts TinyUSB on
//! core 1, and calls `light_app_main` on core 0 with the clocks it configured; it never
//! returns. Core 1 calls `light_app_core1_service` from its USB loop. Everything the shell
//! provides to Rust is declared in the one `extern` block below.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_app_ui_demo as demo;
use demo::{demo_commands, demo_pages, BoardHook, Command, DemoEvent, DisplayConfig, DisplayMod};
use light_input::cst816t::{self, Cst816t};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_display::st7789::St7789;
use light_input::touch::Tracker;
use light_ui::{Fonts, Style, Theme, Ui};
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{debug, info, log, warn, ConstStaticCell, EventBus, Module, Poll, Runtime, StaticCell, Subscription};
use light_power_manager::{PowerManager, PowerMechanism};
use light_display::{Display, FrameLayer};
use light_draw::{PixelFormat, Rotation};
use light_font::Font;
use light_audio::pwm::{pcm_to_duty, Encoding, VOLUME_MAX as AUDIO_VOLUME_MAX};
mod board;
use board::*;
use light_rp2::gpio::{Input, Output};
use light_rp2::pwm_audio::PwmAudio;
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
/// The look-and-feel, compiled from `theme/round169.json`: the framework's default theme
/// plus this glass's corner curvature. That screen_radius of 42 is MEASURED, not
/// estimated, by a calibration sweep on the glass (a rounded rect at inset d closes its
/// corners inside glass of radius R exactly at r = R - d; the transition sat between 36
/// and 40 at d = 2, and the upper end taken). An estimate is exactly how this went wrong
/// twice: a first guess of 20 and a later guess of 24 both left the frame's
/// corners swallowed by the glass, invisibly in code -- only the sweep showed it.
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));

// --- the event bus --------------------------------------------------------------------------

/// This board's extension events -- the piezo -- riding the demo's bus.
#[derive(Clone, Copy, Debug)]
enum Ext {
        /// A square wave at `hz` on the piezo, for `ms` (0 = until `tone off`).
        Tone { hz: u16, ms: u16 },
        ToneOff,
        /// A synthesized sine at `hz` through the SAMPLE path: DMA-paced duty out of a
        /// .bss buffer, the half of the provider a resonant tone does not exercise. The
        /// acoustic check runs it AT the piezo's resonance -- a piezo renders anything far
        /// from resonance near-silently however correct the stream is.
        Beep(u16),
        /// The sample path's volume, `0..=1000` per-mille (the PWM provider's scale).
        Volume(u16),
}

type AppEvent = DemoEvent<Ext>;

static EVENTS: EventBus<AppEvent, 16, 6> = EventBus::new();

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

/// No inset: the frame sits flush to the glass edge, its rounded corners following the
/// curve (the `screen_radius` metric -- see [`THEME_BLOB`] for the measurement behind the 42).
const SAFE_INSET: u8 = 0;
const FPS: u32 = 30;
/// A tenth: the panel stays readable, the way an idle device dims rather than goes dark.
const BACKLIGHT_DIM: u16 = BACKLIGHT_LEVEL_MAX / 10;

demo_pages! {
        event: AppEvent,
        title: "mk4 demo",
        row_gap: 2,
        //   rows in the scrolling list are pinned to this, so the list overflows rather
        // than shrinking
        list_min_row: 44,
        backlight_dim: BACKLIGHT_DIM
}

/// Confirmed on this board: L is 270, R is 90; flat has no upright,
/// so the canvas keeps whatever it had.
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

struct Hook;

impl BoardHook<St7789<Spi1Display>, Ext> for Hook {
        fn after_init(&mut self, display: &mut Display<'static, St7789<Spi1Display>>, bg: u16) {
                //   the 1.69's row offset of 20 is a fact about its 240x280 window into the
                // ST7789's GDDRAM
                display.driver().set_offset(0, DISPLAY_ROW_OFFSET);
                display.driver().clear(bg);
        }
        fn on_unload(&mut self, display: &mut Display<'static, St7789<Spi1Display>>, bg: u16) {
                display.driver().clear(bg);
        }
}

// --- the board's own modules ---------------------------------------------------------------

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
                                AppEvent::Ui(demo::UiAction::DragConsumed) => self.tracker.suppress(),
                                _ => {}
                        }
                }
                if demo::touch_reads_held() {
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

/// This board's [`PowerMechanism`]: just the backlight (a non-inverted, near-linear PWM drive).
/// The 1.69 has no battery, latch or power button, so the trait defaults do the rest -- most
/// importantly `on_external_power` defaults to `true`, which correctly disables the on-battery
/// power-off on a board that only ever runs from USB. Touch and IMU feed the activity beacon
/// through their shared drivers, so the screen still dims on idle and wakes on use.
struct Touch169Power {
        backlight: light_rp2::pwm::PwmOutput,
}

impl PowerMechanism for Touch169Power {
        fn set_backlight(&mut self, level: u16) {
                self.backlight.set_duty(level.min(BACKLIGHT_LEVEL_MAX));
        }
}

struct BoardMod {
        power: PowerManager<Touch169Power, SysClock>,
        events: Subscription,
}

impl Module for BoardMod {
        fn name(&self) -> &'static str {
                "board"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.power.on_load();
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::Command(Command::Backlight(level)) = ev {
                                busy = true;
                                self.power.set_backlight(level);
                                info!("backlight {level}");
                        }
                }
                //   the shared power policy: dim on idle, wake on activity (touch/IMU via the
                // beacon). No power-off here -- this board has no battery to save
                if let Poll::Shutdown = self.power.tick() {
                        return Poll::Shutdown;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.power.on_unload();
        }
}

/// One period of sine at 20000 amplitude, 32 steps -- plenty for a bring-up beeper.
static SINE: [i16; 32] = [
        0, 3902, 7654, 11111, 14142, 16629, 18478, 19616, 20000, 19616, 18478, 16629, 14142, 11111, 7654, 3902, 0, -3902, -7654, -11111, -14142, -16629, -18478, -19616, -20000, -19616, -18478, -16629,
        -14142, -11111, -7654, -3902,
];

/// The beep asset: 300 ms at this rate, synthesized into .bss on demand.
const BEEP_RATE: u32 = 22_050;
const BEEP_SAMPLES: usize = (BEEP_RATE as usize * 300) / 1000;

/// The piezo -- the PWM audio provider, both of its paths: resonant square-wave tones
/// (what a piezo is actually good at) and the DMA-paced duty sample stream.
struct AudioMod {
        buzzer: PwmAudio,
        events: Subscription,
        /// Sample-path volume, per-mille -- applied at synthesis, the provider's contract.
        volume: u16,
        /// A timed tone's end, `now_ms`-relative; `None` while silent or untimed.
        tone_end_ms: Option<u32>,
        /// The duty buffer the beep synthesizes into -- channel-positioned CC words, the
        /// transport's word-wide format; in .bss, taken once.
        beep: &'static mut [u32; BEEP_SAMPLES],
        playing: bool,
}

impl Module for AudioMod {
        fn name(&self) -> &'static str {
                "audio"
        }
        fn load(&mut self) -> Result<(), ()> {
                info!("audio up: pwm provider on GPIO {} (dma ch {}, pacing timer {})", PIN_BUZZER, AUDIO_DMA_CH, AUDIO_DMA_TIMER);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let now_ms = (light_rp2::now_us() / 1000) as u32;
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Ext(Ext::Tone { hz, ms }) => {
                                        busy = true;
                                        self.buzzer.tone(u32::from(hz));
                                        self.tone_end_ms = (ms > 0).then(|| now_ms.wrapping_add(u32::from(ms)));
                                        info!("tone {hz} Hz{}", if ms > 0 { " (timed)" } else { "" });
                                }
                                AppEvent::Ext(Ext::ToneOff) => {
                                        self.buzzer.tone_off();
                                        self.tone_end_ms = None;
                                        info!("tone off");
                                }
                                AppEvent::Ext(Ext::Beep(hz)) => {
                                        if self.buzzer.busy() {
                                                info!("beep: already playing");
                                                continue;
                                        }
                                        //   synthesized at the CURRENT volume: the sample path
                                        // converts once, up front, the provider's contract
                                        let inc = ((u64::from(hz) << 32) / u64::from(BEEP_RATE)) as u32;
                                        let mut phase = 0u32;
                                        for b in self.beep.iter_mut() {
                                                *b = self.buzzer.duty_word(pcm_to_duty(i32::from(SINE[(phase >> 27) as usize]), Encoding::PcmS16, self.volume));
                                                phase = phase.wrapping_add(inc);
                                        }
                                        // SAFETY: a .bss static taken once; between here and
                                        // the transfer's end it is shared only with the DMA
                                        // reader, and the busy() gate above keeps this the one
                                        // writer between plays
                                        let duty: &'static [u32] = unsafe { core::slice::from_raw_parts(self.beep.as_ptr(), self.beep.len()) };
                                        if self.buzzer.play(duty, BEEP_RATE) {
                                                self.playing = true;
                                                busy = true;
                                                info!("beep: {} Hz sine, {} duty samples at {} Hz", hz, BEEP_SAMPLES, BEEP_RATE);
                                        } else {
                                                warn!("beep: play refused");
                                        }
                                }
                                AppEvent::Ext(Ext::Volume(v)) => {
                                        self.volume = v.min(AUDIO_VOLUME_MAX);
                                        info!("volume {} (applies at the next beep)", self.volume);
                                }
                                AppEvent::Command(Command::Stats) => {
                                        info!("audio: {}, volume {}", if self.buzzer.busy() { "playing" } else if self.tone_end_ms.is_some() { "toning" } else { "idle" }, self.volume);
                                }
                                _ => {}
                        }
                }
                if let Some(end) = self.tone_end_ms {
                        if now_ms.wrapping_sub(end) < 0x8000_0000 {
                                self.buzzer.tone_off();
                                self.tone_end_ms = None;
                                info!("tone done");
                        } else {
                                busy = true;
                        }
                }
                if self.playing && !self.buzzer.busy() {
                        self.playing = false;
                        info!("beep done");
                }
                if busy || self.playing { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.buzzer.stop();
                self.buzzer.tone_off();
        }
}

// --- the console table ---------------------------------------------------------------------

fn parse_tone(w: &mut Words) -> Parsed<AppEvent> {
        match w.next() {
                Some("off") => Parsed::Event(DemoEvent::Ext(Ext::ToneOff)),
                Some(hz) => {
                        let Ok(hz) = hz.parse::<u16>() else { return Parsed::Usage };
                        if !(20..=10_000).contains(&hz) {
                                return Parsed::Usage;
                        }
                        let ms = match w.next() {
                                None => 500,
                                Some(ms) => match ms.parse::<u16>() {
                                        Ok(ms) => ms,
                                        _ => return Parsed::Usage,
                                },
                        };
                        Parsed::Event(DemoEvent::Ext(Ext::Tone { hz, ms }))
                }
                None => Parsed::Usage,
        }
}

fn parse_volume(w: &mut Words) -> Parsed<AppEvent> {
        match w.next().and_then(|s| s.parse::<u16>().ok()) {
                Some(v) if v <= AUDIO_VOLUME_MAX => Parsed::Event(DemoEvent::Ext(Ext::Volume(v))),
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[CliCommand<AppEvent>] = &demo_commands![Ext;
        CliCommand { name: "tone", usage: "tone HZ [MS] | tone off", parse: parse_tone },
        CliCommand { name: "beep", usage: "beep [HZ]", parse: |w| match w.next() {
                None => Parsed::Event(DemoEvent::Ext(Ext::Beep(880))),
                Some(hz) => match hz.parse::<u16>() {
                        Ok(hz) if (20..=10_000).contains(&hz) => Parsed::Event(DemoEvent::Ext(Ext::Beep(hz))),
                        _ => Parsed::Usage,
                },
        } },
        CliCommand { name: "volume", usage: "volume 0..1000", parse: parse_volume },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

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

        let power = PowerManager::new(Touch169Power { backlight: p.backlight }, SysClock);
        let mut board_mod = BoardMod { power, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut imu_mod = ImuMod { imu, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut audio_mod = AudioMod {
                buzzer: p.buzzer,
                events: EVENTS.subscribe().expect("subscriber slot"),
                volume: AUDIO_VOLUME_MAX,
                tone_end_ms: None,
                beep: {
                        static BEEP: ConstStaticCell<[u32; BEEP_SAMPLES]> = ConstStaticCell::new([0; BEEP_SAMPLES]);
                        BEEP.take()
                },
                playing: false,
        };
        //   built in place in .bss -- see the demo's DisplayMod -- and taken once
        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565));
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
        ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
        ui.set_safe_inset(SAFE_INSET);
        // module state is 'static in any case: the runtime never returns
        type BoardDisplayMod = DisplayMod<St7789<Spi1Display>, SysClock, Ext, Hook>;
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
                        desc: "ST7789 over SPI, double-buffered",
                        repush: true,
                        draw_over: false,
                        rotation_map,
                        main_page: &PAGE_MAIN,
                },
                Hook,
        ));
        static TOUCH_MOD: StaticCell<TouchMod> = StaticCell::new();
        let touch_mod = TOUCH_MOD.init(TouchMod { touch, tracker: Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), events: EVENTS.subscribe().expect("subscriber slot"), moves: 0 });
        let mut console_mod = demo::ConsoleMod::new(&CLI, &EVENTS);

        let mut rt: Runtime<6> = Runtime::new();
        rt.add(&mut board_mod).expect("capacity");
        rt.add(display_mod).expect("capacity");
        rt.add(touch_mod).expect("capacity");
        rt.add(&mut imu_mod).expect("capacity");
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
