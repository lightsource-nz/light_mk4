//! The LANDSCAPE dictaphone on the Waveshare RP2350-Touch-LCD-3.49 -- a 172x640 QSPI bar
//! of glass with the ES8311 codec, an analog microphone and a TF slot: the hardware-bound
//! instantiation. The application is `light_app_dictaphone_wide` -- the sideways
//! interface over the same `light_dictaphone_core` engine as the portrait app, with no
//! hardware in either; this crate is everything tangible -- the wiring, the AXS15231B
//! panel and its touch half, the PIO I2S transport and its buffers, the card, the battery
//! latch, the shell ABI and the panic handler -- constructed here and handed to the app's
//! modules. Same board, same shell, its own UF2.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_app_dictaphone_wide as dict;
use dict::{dictaphone_commands, dictaphone_wide_pages, AudioSlots, Command, Descent, DisplayConfig, DisplayMod, Event, FsPath, StackString};
use light_input::axs15231b::{self as axs, Axs15231bTouch};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_display::axs15231b::Axs15231b;
use light_input::touch::Tracker;
use light_ui::{Theme, Ui};
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{debug, info, log, warn, AudioStream, ConstStaticCell, EventBus, InputPin, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer};
use light_draw::{PixelFormat, Rotation};
use light_audio::Es8311;
use light_font::Font;
use light_rtc::{Datetime, Pcf85063a};
use light_sd::{SdError, SpiSd};
mod board;
use board::*;
use light_rp2::adc::Adc;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::{I2c0, I2c1};
use light_rp2::i2s::PioI2sOut;
use light_rp2::spi_bus::Spi1Bus;
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
/// costs the animations).
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
static FRAME_BACK: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
/// The look-and-feel: the framework's steel theme, the default for every board with
/// color support. A board-specific override would be a local theme file extending it.
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));

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

/// Core 1's pulse, counted every `light_app_core1_service` pass and reported by `stats`:
/// the console cannot report its own death (a dead core 1 IS a dead console), so core 0
/// carries the diagnosis.
static CORE1_TICKS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The base of SCRATCH_X. Core 0's stack fills SCRATCH_Y above it, and with the shell
/// giving core 1 a stack in ordinary RAM (see `light_mk4_shell/src/main.c` -- a deep
/// core-0 call chain once landed on core 1's frames here and killed the console
/// silently), SCRATCH_X is vacant runway. The watermark paints it plus the bottom of
/// core 0's own bank, so `stats` shows how deep the deepest call chain really reaches.
const PAINT_BASE: u32 = 0x2008_0000;
/// All of SCRATCH_X plus the bottom kilobyte of SCRATCH_Y: 5 KB.
const PAINT_WORDS: usize = 1280;
const PAINT: u32 = 0xC0DE_55AA;

/// Paint the runway. Called FIRST in `light_app_main`, whose own frame sits at the top
/// of core 0's bank, far above the painted region; core 1's stack is elsewhere entirely.
fn stack_paint() {
        let p = PAINT_BASE as *mut u32;
        for i in 0..PAINT_WORDS {
                // SAFETY: vacant SCRATCH_X and the unlived bottom of this core's own bank
                unsafe { core::ptr::write_volatile(p.add(i), PAINT) };
        }
}

/// Untouched painted bytes above the runway's base. The nominal stack floor sits at
/// 4096; below that core 0 is living on the runway.
fn stack_free() -> u32 {
        let p = PAINT_BASE as *const u32;
        for i in 0..PAINT_WORDS {
                // SAFETY: reads the painted region
                if unsafe { core::ptr::read_volatile(p.add(i)) } != PAINT {
                        return (i * 4) as u32;
                }
        }
        (PAINT_WORDS * 4) as u32
}

// --- core 1 --------------------------------------------------------------------------------

fn log_sink(record: &log::Record) {
        let mut line = StackString::<160>::new();
        let _ = write!(line, "{record}");
        let b = line.as_bytes();
        unsafe { light_shell_log(b.as_ptr(), b.len()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_service() {
        CORE1_TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        log::drain(4, log_sink);
        for _ in 0..32 {
                let b = unsafe { light_shell_read_byte() };
                if b < 0 {
                        break;
                }
                dict::push_console_byte(b as u8);
        }
}

// --- the interface, as data ---------------------------------------------------------------

//   Bar glass, corners unmeasured: the theme's screen_radius keeps its default of 0
// until the glass says otherwise.
const FPS: u32 = 30;

dictaphone_wide_pages! {
        event: AppEvent,
        gap: 6,
        //   across the 640 with the 16 px font's 13 px cell: the record button is pinned
        // at its 8-character label (8*13 + the 1 px insets, plus slack), and a recordings
        // column prints a 12-character file name ("REC_0007.WAV") in full. The freed
        // width lands on the play and recordings columns -- "Recordings >" needs 158.
        // "< Back" is 6 characters, pinned left of the scrolling recordings strip
        rec_w: 108,
        list_col_w: 160,
        back_w: 84
}

/// Landscape only: the two horizontal poses follow the IMU end-for-end; the portrait
/// poses are ignored, so tilting the bar upright never leaves the sideways layout.
/// The pairing is MEASURED on the glass -- L->R90 rendered both poses upside down.
fn rotation_map(o: Orientation) -> Option<Rotation> {
        match o {
                Orientation::LandscapeL => Some(Rotation::R270),
                Orientation::LandscapeR => Some(Rotation::R90),
                _ => None,
        }
}

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
        backlight: light_rp2::pwm::PwmOutput,
        /// The power latch: high since board::take(); driven low on unload = power off.
        sys_en: Output,
        /// The side button, low when pressed; held [`POWER_OFF_HOLD_MS`] = shutdown.
        button: Input,
        battery: Adc,
        /// The TF slot; probed on demand (the `sd` command), not at boot -- an empty slot
        /// is this board's ordinary state. Shared with the recorder, per-operation.
        sd: &'static RefCell<SpiSd<Spi1Bus, Output>>,
        pressed_since_ms: Option<u32>,
        events: Subscription,
}

impl BoardMod {
        fn apply(&mut self, level: u16) {
                //   the driver's usable band, MEASURED on this glass: the backlight is
                // fully dark at or below 40% LED-on time and only dims visibly between
                // ~45% and 100% -- an RC-filtered threshold drive, not a proportional
                // switch. Level 0 is off; every other level maps linearly onto the band
                // above the floor, so the console's 0..1000 scale is all usable
                const FLOOR: u32 = 450;
                let level = u32::from(level.min(BACKLIGHT_LEVEL_MAX));
                let max = u32::from(BACKLIGHT_LEVEL_MAX);
                let physical = if level == 0 { 0 } else { (FLOOR + level * (max - FLOOR) / max) as u16 };
                let duty = if BACKLIGHT_INVERTED { BACKLIGHT_LEVEL_MAX - physical } else { physical };
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
                                        //   the instruments that convicted a core-1 stack
                                        // kill once: core 1's pulse, and how deep core 0's
                                        // deepest call chain reached
                                        info!("cores: core1 ticks {}, core0 stack low-water {} B above the runway base", CORE1_TICKS.load(core::sync::atomic::Ordering::Relaxed), stack_free());
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
                //   the side button: a 1.5 s hold is the power-off gesture. The shutdown
                // flows through the runtime like the console's `quit`, so every module
                // unloads before this module's unload releases the power latch
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

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        stack_paint();
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

        //   the one card, shared: the board module's sd probe and the app's recorder and
        // fs commands all borrow it per operation
        static SD_CELL: StaticCell<RefCell<SpiSd<Spi1Bus, Output>>> = StaticCell::new();
        let sd: &'static RefCell<SpiSd<Spi1Bus, Output>> = SD_CELL.init(RefCell::new(SpiSd::new(p.sd_spi, p.sd_cs)));
        let mut board_mod = BoardMod {
                backlight: p.backlight,
                sys_en: p.sys_en,
                button: p.power_button,
                battery: p.battery,
                sd,
                pressed_since_ms: None,
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
        static LIST: ConstStaticCell<[Option<FsPath>; dict::LIST_ROWS]> = ConstStaticCell::new([None; dict::LIST_ROWS]);
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
                        list: LIST.take(),
                },
                &EVENTS,
        );

        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565));
        static UI: ConstStaticCell<Ui<AppEvent, { dict::UI_WIDGETS }>> = ConstStaticCell::new(Ui::new());
        let layer: &'static mut FrameLayer = LAYER.take();
        let ui: &'static mut Ui<AppEvent, { dict::UI_WIDGETS }> = UI.take();
        //   the look-and-feel, from the embedded blob: a bad blob is a build-system bug
        // worth halting on, not styling to guess past
        let theme = match Theme::parse(THEME_BLOB) {
                Ok(t) => t,
                Err(e) => panic!("the embedded theme does not parse: {e:?}"),
        };
        layer.bg = theme.bg;
        ui.set_theme(theme);
        ui.set_font(&font);
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
                        desc: "AXS15231B over PIO-QSPI, double-buffered, sideways",
                        rotation_map,
                        //   the resting pose is LandscapeL (R270): boot in it, or the
                        // first frames flash upside down until the IMU's first report
                        initial_rotation: Rotation::R270,
                        main_page: &PAGE_MAIN,
                        //   the landscape interface flows downward: a child page enters from
                        // the BOTTOM and rises into place, back sinks it back down. Logical, so
                        // it reads the same in both landscape poses.
                        default_descent: Some(Descent::FromBottom),
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

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        let mut msg = StackString::<160>::new();
        let _ = write!(msg, "{info}");
        let b = msg.as_bytes();
        unsafe { light_shell_panic(b.as_ptr(), b.len()) }
}
