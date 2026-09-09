//! The touch28 firmware: the widget demo on the Waveshare RP2350-Touch-LCD-2.8 -- the
//! first board with the CST328 touch controller, whose bring-up this firmware exists to
//! run. The application is `light_app_ui_demo`, with no hardware in it; this crate is the
//! tangible 2.8: the wiring (`board.rs`), the ST7789 over SPI, the CST328, the IMU, the
//! shell ABI and the panic handler.
//!
//! Bring-up checklist (this board's support was authored without hardware): the CST328's
//! 0xCACA probe answers; coordinates track a finger; the IMU axis map is IDENTITY until the
//! three-observation calibration is done, so orientation changes will likely be WRONG at
//! first -- that is the measurement, not a bug; the SPI clock is 40 MHz with reported
//! headroom to 62.5; the `touch`/`render` console instruments are carried for the same
//! wedge-hunting they did on the 1.69.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_app_ui_demo as demo;
use demo::{demo_commands, demo_pages, BoardHook, Command, DemoEvent, DemoView, DisplayConfig, DisplayMod};
use light_input::cst328::{self, Cst328};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_display::st7789::St7789;
use light_input::touch::Tracker;
use light_ui::{Theme, Ui};
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{info, log, warn, ConstStaticCell, EventBus, InputPin, Module, Poll, Runtime, StaticCell, Subscription};
use light_power_manager::{PowerManager, PowerMechanism};
use light_display::{Display, FrameLayer};
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
/// The look-and-feel: the framework's steel theme, the default for every board with
/// color support. A board-specific override would be a local theme file extending it.
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));

// --- the event bus --------------------------------------------------------------------------

/// This board's extension events, riding the demo's bus.
#[derive(Clone, Copy, Debug)]
enum Ext {
        /// Re-clock the display bus live: the SPI-headroom probe. Too fast shows up as
        /// corrupt pixels rather than a clean failure, so the eye is the instrument.
        SpiHz(u32),
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

//   Square glass on this board: product photos show no rounding worth declaring, so the
// theme's screen_radius keeps its default of 0. TO BE CONFIRMED: if the glass clips
// corner content, measure the radius the way the 1.69's was measured and set the
// `screen_radius` metric in a local theme file extending "default" (see the 1.69's).
const FPS: u32 = 30;
const BACKLIGHT_DIM: u16 = BACKLIGHT_LEVEL_MAX / 10;

demo_pages! {
        event: AppEvent,
        title: "mk4 2.8",
        //   rows this tall need more than the small OLED panels'
        // 2 px gap, and 56 px is the touch-target height -- the extra 40 rows of panel
        // simply show more of the list at once
        row_gap: 6,
        list_min_row: 56,
        backlight_dim: BACKLIGHT_DIM
}

/// The same table as the 1.69's -- but the axis map is the UNMEASURED identity, so until
/// the calibration session these rotations are the thing under test, not a fact.
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
                //   no set_offset: 240x320 is the ST7789's full GDDRAM, so the power-on
                // (0,0) is already correct -- the 1.69's row offset of 20 is a fact about
                // its 240x280 window, not about the driver
                display.driver().clear(bg);
        }
        fn on_unload(&mut self, display: &mut Display<'static, St7789<Spi1Display>>, bg: u16) {
                display.driver().clear(bg);
        }
        fn on_ext(&mut self, view: &mut DemoView<'_, St7789<Spi1Display>, Ext>, ext: Ext) {
                match ext {
                        Ext::SpiHz(hz) => {
                                //   never mid-push: a divider change under a DMA burst
                                // tears the transfer
                                let _ = view.display.wait();
                                let actual = view.display.driver().bus_mut().set_baudrate(hz);
                                info!("spi re-clocked: asked {hz} Hz, running {actual} Hz");
                                //   repaint everything at the new clock, so corruption
                                // shows immediately rather than on the next interaction
                                view.ui.invalidate_all();
                        }
                }
        }
}

// --- the board's own modules ---------------------------------------------------------------

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
                        cst328::Event::Down { x, y } => {
                                self.moves = 0;
                                light_core::debug!("touch down at {x},{y}");
                        }
                        cst328::Event::Up => light_core::debug!("touch up after {} moves at {},{}", self.moves, self.touch.x, self.touch.y),
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

unsafe extern "C" {
        /// True while a USB host has this device enumerated; set on core 1 by the shell. This
        /// board has no VBUS pin, so enumeration is its "on external power" signal (see the 349).
        fn light_shell_usb_mounted() -> bool;
}

/// This board's [`PowerMechanism`]: the backlight is an NPN low-side switch (high = lit, linear,
/// no floor), the battery latch powers it off, the side key is the button, and there is no battery
/// gauge. External power is USB enumeration, as on the 349.
struct Touch28Power {
        backlight: light_rp2::pwm::PwmOutput,
        bat_en: Output,
        key_bat: Input,
}

impl PowerMechanism for Touch28Power {
        fn set_backlight(&mut self, level: u16) {
                //   an NPN low-side PWM switch is close to linear, so the per-mille level is the
                // duty directly -- no floor band like the 349's RC-filtered drive
                self.backlight.set_duty(level.min(BACKLIGHT_LEVEL_MAX));
        }

        fn on_external_power(&self) -> bool {
                unsafe { light_shell_usb_mounted() }
        }

        fn power_off(&mut self) {
                self.bat_en.set(false);
        }

        fn power_button_pressed(&self) -> bool {
                self.key_bat.is_low()
        }
}

struct BoardMod {
        power: PowerManager<Touch28Power, SysClock>,
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
                //   the shared power policy: dim on idle, power off on battery (touch/IMU/key feed
                // the activity beacon it watches), and the key-hold shutdown
                if let Poll::Shutdown = self.power.tick() {
                        return Poll::Shutdown;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.power.on_unload();
        }
}

// --- the console table ---------------------------------------------------------------------

fn parse_spi(w: &mut Words) -> Parsed<AppEvent> {
        match w.next().and_then(|s| s.parse::<u32>().ok()) {
                //   the achievable rates at clk_peri 150 MHz are coarse (75, 37.5, 25...);
                // the driver reports what it actually got
                Some(hz) if (1_000_000..=100_000_000).contains(&hz) => Parsed::Event(DemoEvent::Ext(Ext::SpiHz(hz))),
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[CliCommand<AppEvent>] = &demo_commands![Ext;
        CliCommand { name: "spi", usage: "spi HZ (1000000..100000000; the headroom probe)", parse: parse_spi },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

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

        let power = PowerManager::new(Touch28Power { backlight: p.backlight, bat_en: p.bat_en, key_bat: p.key_bat }, SysClock);
        let mut board_mod = BoardMod { power, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut imu_mod = ImuMod { imu, events: EVENTS.subscribe().expect("subscriber slot") };
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
        ui.set_theme(theme);
        ui.set_font(&font);
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
