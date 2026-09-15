//! The dictaphone's shared board wiring for the Waveshare RP2350-Touch-LCD-3.49 -- a 172x640 QSPI
//! bar of glass with the ES8311 codec, an analog microphone and a TF slot. Both dictaphone
//! executables (the portrait `dictaphone_touch349` and the landscape `dictaphone_wide_touch349`)
//! build against this: the AXS15231B panel and its touch half, the QMI8658 IMU, the PCF85063A RTC,
//! the PIO I2S transport and its buffers, the card and the battery latch, the console table, and the
//! runtime that binds them to the `light_dictaphone_core` engine -- all identical between the two.
//! [`run`] is the whole firmware; each executable passes only its orientation [`RunConfig`] (the
//! rotation map, the starting rotation and page-transition flow) and its embedded blobs.

#![no_std]

use core::cell::RefCell;
use light_dictaphone_core as dict;
use dict::{dictaphone_commands, keep_recording, AudioSlots, AudioStatus, Command, DisplayConfig, DisplayMod, Event, FilePicker, Order, UiSource};
use light_input::axs15231b::{self as axs, Axs15231bTouch};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_display::axs15231b::Axs15231b;
use light_input::touch::Tracker;
use light_ui::{Fonts, Lui, Style, Theme, Ui};
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{debug, info, log, warn, AudioStream, ConstStaticCell, EventBus, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer};
use light_draw::{PixelFormat, Rotation};
use light_audio::Es8311;
use light_font::Font;
use light_rtc::{Datetime, Pcf85063a};
use light_sd::{SdError, SpiSd};
use light_board_touch349::{board, core1_ticks, stack_free, stack_paint, PowerManager, ShellInfo};
use board::*;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::{I2c0, I2c1};
use light_rp2::i2s::PioI2sOut;
use light_rp2::spi_bus::Spi1Bus;
use light_rp2::qspi::PioQspiDisplayBus;
use light_rp2::{Breathe, Clocks, SysClock};

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);

/// Two frame buffers, 215 KB each -- 430 KB of the RP2350's 520: tight but linkable. If a
/// later addition overflows SRAM, the back buffer is the thing to give up (single-buffered
/// costs the animations).
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
static FRAME_BACK: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

//   each executable pumps console bytes into the core's mailbox from its own core-1 service, and the
// landscape one names a `Descent` for its config; the re-exports save both a direct
// light_dictaphone_core dependency
pub use dict::{push_console_byte, Descent};

/// What one dictaphone executable hands [`run`]: its embedded blobs and the few things the two
/// orientations differ by. Everything else -- the wiring, the modules, the runtime -- is shared.
pub struct RunConfig {
        /// The font, theme and UI-design blobs, `include_bytes!`'d in the executable (their paths are
        /// per-build env vars the shared crate cannot see).
        pub font: &'static [u8],
        pub theme: &'static [u8],
        pub ui: &'static [u8],
        /// This orientation's IMU-pose-to-display-rotation map.
        pub rotation_map: fn(Orientation) -> Option<Rotation>,
        /// The rotation the interface boots in, before the first IMU report.
        pub initial_rotation: Rotation,
        /// The page-transition flow, or `None` for the toolkit's layout-derived default.
        pub default_descent: Option<Descent>,
        /// The panel description for the boot log.
        pub desc: &'static str,
}

// --- the event bus --------------------------------------------------------------------------

/// This board's extension events -- the RTC and the raw card probe -- riding the app's bus.
#[derive(Clone, Copy, Debug)]
enum Ext {
        RtcShow,
        RtcSet(Datetime),
        /// Probe the card from CMD0 and read block 0: the layer below `fs`.
        Sd,
}

type AppEvent = Event<Ext>;

static EVENTS: EventBus<AppEvent, 16, 6> = EventBus::new();

//   Bar glass, corners unmeasured: the theme's screen_radius keeps its default of 0
// until the glass says otherwise.
const FPS: u32 = 30;

// --- storage: the TF slot as the app's Store -----------------------------------------------

/// The TF slot as a SHAREABLE block device: a borrow of the board's one card, taken per
/// block operation -- which is what lets the `fs` console commands and a live recording's
/// mounted volume coexist on one `SpiSd`.
struct SdRef(&'static RefCell<SpiSd<Spi1Bus, Output>>);

fn sd_block_error(e: SdError) -> light_core::hal::BlockError {
        match e {
                SdError::Timeout => light_core::hal::BlockError::Timeout,
                _ => light_core::hal::BlockError::Io,
        }
}

impl light_core::hal::BlockDevice for SdRef {
        fn block_count(&self) -> u32 {
                self.0.borrow().card.map(|c| c.blocks).unwrap_or(0)
        }
        fn read_block(&mut self, lba: u32, out: &mut [u8; 512]) -> Result<(), light_core::hal::BlockError> {
                self.0.borrow_mut().read_block(lba, out).map_err(sd_block_error)
        }
        fn write_block(&mut self, lba: u32, data: &[u8; 512]) -> Result<(), light_core::hal::BlockError> {
                self.0.borrow_mut().write_block(lba, data).map_err(sd_block_error)
        }
}

/// A single-slot .bss home for a card-borrowing state machine (a [`dict::Recording`], a
/// [`dict::Playback`]) -- NOT core 0's 4 KB stack. `StaticCell` wants `Send`, which the
/// `&RefCell` inside cannot offer; this holder makes the single-core argument explicitly
/// instead.
///
/// SAFETY: the runtime is single-core and each slot is taken as `&'static mut` exactly
/// once, at construction.
struct AppSlot<T>(core::cell::UnsafeCell<Option<T>>);
unsafe impl<T> Sync for AppSlot<T> {}
static REC_SLOT: AppSlot<dict::Recording<SdRef>> = AppSlot(core::cell::UnsafeCell::new(None));
static PLAY_SLOT: AppSlot<dict::Playback<SdRef>> = AppSlot(core::cell::UnsafeCell::new(None));

/// The app's [`dict::Store`]: ready the card on first touch since insertion, hand a
/// borrowing device per mounted operation, forget a card that failed mid-operation.
struct SdStore(&'static RefCell<SpiSd<Spi1Bus, Output>>);

impl dict::Store for SdStore {
        type Dev = SdRef;

        fn open(&mut self) -> Option<SdRef> {
                if self.0.borrow().card.is_none() {
                        let mut clock = SysClock;
                        if let Err(e) = self.0.borrow_mut().init(&mut clock) {
                                info!("card: init failed ({e:?})");
                                return None;
                        }
                }
                Some(SdRef(self.0))
        }

        fn reset(&mut self) {
                self.0.borrow_mut().card = None;
        }
}

// --- the sample transport: PIO I2S as the app's AudioStream --------------------------------

/// The PIO I2S transport wearing the portable [`AudioStream`] contract. The DMA ping-pong
/// buffers are THIS crate's statics, handed to the transport on first use.
struct I2sStream {
        i2s: PioI2sOut,
        cap_handed: bool,
}

impl AudioStream for I2sStream {
        fn start(&mut self) {
                static STREAM_A: ConstStaticCell<[u32; light_rp2::i2s::STREAM_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::STREAM_WORDS]);
                static STREAM_B: ConstStaticCell<[u32; light_rp2::i2s::STREAM_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::STREAM_WORDS]);
                //   the IRQ-drained prefetch ring: two DMA buffers deep (~340 ms), the lead
                // that rides out a card stall while the poll loop is blocked filling it
                static RING: ConstStaticCell<[u32; light_rp2::i2s::STREAM_WORDS * 2]> = ConstStaticCell::new([0; light_rp2::i2s::STREAM_WORDS * 2]);
                self.i2s.start_stream_irq([STREAM_A.take(), STREAM_B.take()], RING.take());
        }

        fn capture_start(&mut self) {
                static CAP_A: ConstStaticCell<[u16; light_rp2::i2s::CAP_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::CAP_WORDS]);
                static CAP_B: ConstStaticCell<[u16; light_rp2::i2s::CAP_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::CAP_WORDS]);
                let bufs = if self.cap_handed {
                        None
                } else {
                        self.cap_handed = true;
                        Some([CAP_A.take(), CAP_B.take()])
                };
                self.i2s.capture_start(bufs);
        }

        fn capture_stop(&mut self) {
                self.i2s.capture_stop();
        }

        fn capture_take(&mut self, sink: &mut dyn FnMut(&[u16])) {
                self.i2s.capture_take(|buf| sink(buf));
        }

        fn stream_free(&self) -> usize {
                self.i2s.stream_free()
        }

        fn stream_push(&mut self, fill: &mut dyn FnMut(&mut [u32]) -> usize) {
                self.i2s.stream_push(fill);
        }

        fn set_active(&mut self, active: bool) {
                self.i2s.set_active(active);
        }

        fn stream_pending(&self) -> usize {
                self.i2s.stream_pending()
        }

        fn stream_clear(&mut self) {
                self.i2s.stream_clear();
        }

        fn underruns(&self) -> u32 {
                self.i2s.stream_underruns()
        }

        fn cap_overruns(&self) -> u32 {
                self.i2s.cap_overruns
        }

        fn reset_stats(&mut self) {
                self.i2s.reset_underruns();
                self.i2s.cap_overruns = 0;
        }

        fn debug_probe(&mut self) {
                //   sample the raw DIN pad, then read the PIO FIFO directly. Splits "the
                // codec's data line is dead" from "the state machine misreads a toggling
                // line"
                let (highs, pc) = self.i2s.din_probe(4000);
                self.i2s.capture_sm_only();
                let fifo = self.i2s.din_fifo_probe();
                info!("micdbg: DIN GPIO high {}/4000, sm pc {}", highs, pc);
                info!("micdbg: raw FIFO words {:#010x} {:#010x} {:#010x} {:#010x}", fifo[0], fifo[1], fifo[2], fifo[3]);
        }
}

// --- the board's own modules ---------------------------------------------------------------

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
                                AppEvent::Ui(dict::UiAction::DragConsumed) => self.tracker.suppress(),
                                _ => {}
                        }
                }
                if dict::touch_reads_held() {
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
        /// The shared touch349 power behaviour: dim on idle, power off on battery. Owns the
        /// backlight, the power latch, the side button and the battery ADC.
        power: PowerManager,
        /// The TF slot; probed on demand (the `sd` command), not at boot -- an empty slot
        /// is this board's ordinary state. Shared with the recorder, per-operation.
        sd: &'static RefCell<SpiSd<Spi1Bus, Output>>,
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
                        match ev {
                                //   audio in flight defers power-off, so a recording is never cut short
                                AppEvent::Status(s) => self.power.set_busy(!matches!(s, AudioStatus::Idle)),
                                AppEvent::Command(Command::Backlight(level)) => {
                                        busy = true;
                                        self.power.set_backlight(level);
                                        info!("backlight {level}");
                                }
                                AppEvent::Command(Command::Stats) => {
                                        info!("battery: {} mV, {}", self.power.battery_mv(), if self.power.on_external_power() { "external power" } else { "on battery" });
                                        //   the instruments that convicted a core-1 stack
                                        // kill once: core 1's pulse, and how deep core 0's
                                        // deepest call chain reached
                                        info!("cores: core1 ticks {}, core0 stack low-water {} B above the runway base", core1_ticks(), stack_free());
                                }
                                AppEvent::Ext(Ext::Sd) => {
                                        busy = true;
                                        let mut clock = SysClock;
                                        let mut sd = self.sd.borrow_mut();
                                        match sd.init(&mut clock) {
                                                Ok(card) => {
                                                        let mb = card.blocks / 2048;
                                                        let kind = if card.high_capacity { "SDHC/XC" } else { "SDSC" };
                                                        let mut block = [0u8; 512];
                                                        match sd.read_block(0, &mut block) {
                                                                Ok(()) => {
                                                                        let sig = block[510] == 0x55 && block[511] == 0xAA;
                                                                        info!("sd: {mb} MB {kind} ({} blocks); block 0 read, boot signature {}", card.blocks, if sig { "present" } else { "absent" });
                                                                }
                                                                Err(e) => warn!("sd: {mb} MB {kind} identified but block 0 read failed: {e:?}"),
                                                        }
                                                }
                                                Err(e) => info!("sd: {e:?}"),
                                        }
                                }
                                _ => {}
                        }
                }
                //   the power manager runs the dim and power-off timers and the button-hold
                // gesture; its shutdown flows through the runtime like the console's `quit`, so
                // every module unloads before this module's unload releases the power latch
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
                None => Parsed::Event(Event::Ext(Ext::RtcShow)),
                Some("set") => {
                        let (Some(date), Some(time)) = (w.next(), w.next()) else { return Parsed::Usage };
                        let Some((year, month, day)) = split3(date, '-') else { return Parsed::Usage };
                        let Some((hour, minute, second)) = split3(time, ':') else { return Parsed::Usage };
                        let valid = (1..=12).contains(&month) && (1..=31).contains(&day) && hour < 24 && minute < 60 && second < 60 && (1970..=2069).contains(&year);
                        if !valid {
                                return Parsed::Usage;
                        }
                        Parsed::Event(Event::Ext(Ext::RtcSet(Datetime {
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

static COMMANDS: &[CliCommand<AppEvent>] = &dictaphone_commands![Ext;
        CliCommand { name: "rtc", usage: "rtc | rtc set YYYY-MM-DD HH:MM:SS", parse: parse_rtc },
        CliCommand { name: "sd", usage: "sd", parse: |_| Parsed::Event(Event::Ext(Ext::Sd)) },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

// --- entry ----------------------------------------------------------------------------------

/// The whole dictaphone firmware: bring the board up, wire the modules to the engine, and run the
/// runtime forever. An executable calls this from its `light_app_main` with its [`RunConfig`].
pub fn run(info: &ShellInfo, cfg: RunConfig) -> ! {
        stack_paint();
        log::set_clock(light_rp2::now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, peri {} Hz; touch i2c0 at {} Hz, imu i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.touch_bus.actual_hz, p.imu_bus.actual_hz);

        let front: &'static mut [u8] = FRAME_FRONT.take();
        let back: &'static mut [u8] = FRAME_BACK.take();
        let mut display = Display::new(Axs15231b::new(p.display_bus), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        display.set_back_buffer(back);
        let font = match Font::parse(cfg.font) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        //   the IMU has i2c1 to itself on this board, but the RefCell keeps the same shape
        // as the other boards' shared-bus wiring
        static IMU_I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let imu_i2c: &'static RefCell<I2c1> = IMU_I2C.init(RefCell::new(p.imu_bus));
        let touch = Axs15231bTouch::new(p.touch_bus, p.touch_int, TOUCH_MAP, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(imu_i2c));

        //   the one card, shared: the board module's sd probe and the app's recorder and
        // fs commands all borrow it per operation
        static SD_CELL: StaticCell<RefCell<SpiSd<Spi1Bus, Output>>> = StaticCell::new();
        let sd: &'static RefCell<SpiSd<Spi1Bus, Output>> = SD_CELL.init(RefCell::new(SpiSd::new(p.sd_spi, p.sd_cs)));
        let mut board_mod = BoardMod {
                power: PowerManager::new(p.backlight, p.sys_en, p.power_button, p.battery, p.charge_stat),
                sd,
                events: EVENTS.subscribe().expect("subscriber slot"),
        };
        let mut imu_mod = ImuMod { imu, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut rtc_mod = RtcMod { rtc: Pcf85063a::new(imu_i2c), events: EVENTS.subscribe().expect("subscriber slot") };

        //   the app's big audio state, in .bss slots this module owns -- the AudioMod
        // value itself lives on core 0's small stack (its &RefCell fields cannot ride a
        // StaticCell), and a ~1.2 KB Recording inline once overflowed that stack straight
        // into core 1's
        //   a small frame-aligned read scratch for the playback path; the stall-riding DEPTH
        // is the transport's IRQ-drained prefetch ring (see I2sStream::start), not this
        static STAGE: ConstStaticCell<[u8; 2048]> = ConstStaticCell::new([0; 2048]);
        static PICKER: ConstStaticCell<FilePicker<{ dict::LIST_ROWS }>> = ConstStaticCell::new(FilePicker::new(Order::NameDescending, keep_recording));
        let mut audio_mod = dict::AudioMod::new(
                Es8311::new(imu_i2c),
                I2sStream { i2s: p.i2s, cap_handed: false },
                p.audio_pa,
                SysClock,
                SdStore(sd),
                AUDIO_SAMPLE_HZ,
                //   tuned on the glass: PGA code 7 with +18 dB digital puts normal speech
                // peaks ~45% of full scale -- and a DELIBERATELY loud take measured at
                // +2 dB more gain pinned full scale, so this is as hot as a recorder
                // without a limiter should default. The analog field is treacherous --
                // 0x17 -> 0x18 COLLAPSED the gain ~20 dB (the encoding is not linear);
                // retune digitally, in REG17, only
                (0x17, 0xE3),
                AudioSlots {
                        // SAFETY: the one take of each slot (see AppSlot)
                        rec: unsafe { &mut *REC_SLOT.0.get() },
                        play: unsafe { &mut *PLAY_SLOT.0.get() },
                        play_stage: STAGE.take(),
                        picker: PICKER.take(),
                },
                &EVENTS,
        );

        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565));
        static UI: ConstStaticCell<Ui<AppEvent, { dict::UI_WIDGETS }>> = ConstStaticCell::new(Ui::new());
        let layer: &'static mut FrameLayer = LAYER.take();
        let ui: &'static mut Ui<AppEvent, { dict::UI_WIDGETS }> = UI.take();
        //   the look-and-feel, from the embedded blob: a bad blob is a build-system bug
        // worth halting on, not styling to guess past
        let theme = match Theme::parse(cfg.theme) {
                Ok(t) => t,
                Err(e) => panic!("the embedded theme does not parse: {e:?}"),
        };
        layer.bg = theme.bg;
        ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
        //   the interface, from the embedded blob: a bad blob is a build-system bug worth halting
        // on, like the theme
        let lui = match Lui::parse(cfg.ui) {
                Ok(l) => l,
                Err(e) => panic!("the embedded UI design does not parse: {e:?}"),
        };
        type BoardDisplayMod = DisplayMod<Axs15231b<PioQspiDisplayBus>, SysClock, Ext>;
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
                        desc: cfg.desc,
                        rotation_map: cfg.rotation_map,
                        initial_rotation: cfg.initial_rotation,
                        source: UiSource::Blob(lui),
                        //   the layout axis comes from the design's orientation; the descent is this
                        // orientation's page-transition flow (portrait defaults, landscape rises)
                        default_descent: cfg.default_descent,
                },
        ));
        static TOUCH_MOD: StaticCell<TouchMod> = StaticCell::new();
        let touch_mod = TOUCH_MOD.init(TouchMod { touch, tracker: Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), events: EVENTS.subscribe().expect("subscriber slot"), moves: 0 });
        let mut console_mod = dict::ConsoleMod::new(&CLI, &EVENTS);

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
