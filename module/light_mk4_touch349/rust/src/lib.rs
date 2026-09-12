//! The touch349 firmware: the widget demo on the Waveshare RP2350-Touch-LCD-3.49 -- a
//! 172x640 bar of glass, the first QSPI panel (AXS15231B, LCD and touch in one chip), and
//! the first board on the RP2350's upper GPIO bank. The application is `light_app_ui_demo`;
//! this crate is the tangible 3.49: the wiring, the QSPI panel, the codec + TF-slot audio
//! leg, the battery latch, the shell ABI and the panic handler.
//!
//! Bring-up checklist (this board's support is authored from Waveshare's reference demo, not
//! a schematic): the panel lights and draws (if dark, SLPOUT/DISPON are the first suspects
//! -- see the driver); touch answers on i2c0 and coordinates track, with the axis directions
//! measured on the glass; the backlight really is inverted; the IMU axis map is
//! IDENTITY-until-measured, so orientation is suspect until calibrated.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_app_ui_demo as demo;
use demo::{demo_commands, demo_pages, BoardHook, Command, DemoEvent, DisplayConfig, DisplayMod};
use light_input::axs15231b::{self as axs, Axs15231bTouch};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_display::axs15231b::Axs15231b;
use light_input::touch::Tracker;
use light_ui::{Fonts, Style, Theme, Ui};
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{debug, info, log, warn, ConstStaticCell, EventBus, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer};
use light_draw::{PixelFormat, Rotation};
use light_audio::Es8311;
use light_font::Font;
use light_rtc::{Datetime, Pcf85063a};
use light_fs::{Fat, File as FsFile, FsError};
use light_sd::{SdError, SpiSd};
use light_board_touch349::{board, PowerManager};
use board::*;
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

/// A single-slot .bss home for a card-borrowing state machine (a [`Recording`], a
/// [`Playback`]) -- NOT core 0's 4 KB SCRATCH_Y stack, where a ~1.2 KB Recording inline
/// was the overflow that spilled into core 1's stack. `StaticCell` wants `Send`, which
/// the `&RefCell` inside cannot offer; this holder makes the single-core argument
/// explicitly instead.
///
/// SAFETY: the runtime is single-core and each slot is taken as `&'static mut` exactly
/// once, at construction.
struct AppSlot<T>(core::cell::UnsafeCell<Option<T>>);
unsafe impl<T> Sync for AppSlot<T> {}
static REC_SLOT: AppSlot<Recording> = AppSlot(core::cell::UnsafeCell::new(None));
static PLAY_SLOT: AppSlot<Playback> = AppSlot(core::cell::UnsafeCell::new(None));

/// A canonical 44-byte PCM WAV header: mono, 16-bit, `sample_hz` -- what makes a
/// recording a file any desktop player opens.
fn wav_header(sample_hz: u32, data_len: u32) -> [u8; 44] {
        let mut h = [0u8; 44];
        h[..4].copy_from_slice(b"RIFF");
        h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
        h[8..12].copy_from_slice(b"WAVE");
        h[12..16].copy_from_slice(b"fmt ");
        h[16..20].copy_from_slice(&16u32.to_le_bytes());
        h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
        h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
        h[24..28].copy_from_slice(&sample_hz.to_le_bytes());
        h[28..32].copy_from_slice(&(sample_hz * 2).to_le_bytes());
        h[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
        h[34..36].copy_from_slice(&16u16.to_le_bytes());
        h[36..40].copy_from_slice(b"data");
        h[40..44].copy_from_slice(&data_len.to_le_bytes());
        h
}

/// One `fs` console command against the TF slot: init the card if this is the first
/// touch since insertion, mount the volume OVER a borrow of the device (the blanket
/// `BlockDevice for &mut T` -- the board keeps the card), act, unmount by drop.
fn fs_command(sd: &mut SpiSd<Spi1Bus, Output>, op: FsOp, path: &str, arg: &str) {
        if sd.card.is_none() {
                let mut clock = SysClock;
                if let Err(e) = sd.init(&mut clock) {
                        info!("fs: no card ({e:?})");
                        return;
                }
        }
        let mut fs = match Fat::mount(&mut *sd) {
                Ok(fs) => fs,
                Err(e) => {
                        info!("fs: mount failed: {e:?}");
                        //   an I/O failure may be a pulled or swapped card: forget the
                        // init so the next command starts from CMD0
                        if matches!(e, FsError::Io(_)) {
                                sd.card = None;
                        }
                        return;
                }
        };
        match op {
                FsOp::Info => {
                        let v = fs.volume_info();
                        info!("fs: {}, {} clusters of {} bytes", if v.fat32 { "FAT32" } else { "FAT16" }, v.cluster_count, v.bytes_per_cluster);
                }
                FsOp::Ls => {
                        let mut count = 0u32;
                        let r = fs.list_dir(path, |e| {
                                count += 1;
                                match (e.is_dir, e.long_name()) {
                                        (true, Some(l)) => info!("  {}/  <{}>", e.name(), l),
                                        (true, None) => info!("  {}/", e.name()),
                                        (false, Some(l)) => info!("  {}  {} B  <{}>", e.name(), e.size, l),
                                        (false, None) => info!("  {}  {} B", e.name(), e.size),
                                }
                        });
                        match r {
                                Ok(()) => info!("fs: {count} entries"),
                                Err(e) => info!("fs: ls failed: {e:?}"),
                        }
                }
                FsOp::Write => {
                        //   create, or append when it already exists: `fs write LOG.TXT hello`
                        // twice is a two-line file
                        let mut f = match fs.create(path) {
                                Ok(f) => f,
                                Err(FsError::Exists) => match fs.append(path) {
                                        Ok(f) => f,
                                        Err(e) => {
                                                info!("fs: append failed: {e:?}");
                                                return;
                                        }
                                },
                                Err(e) => {
                                        info!("fs: create failed: {e:?}");
                                        return;
                                }
                        };
                        let mut data = [0u8; 49];
                        let n = arg.len().min(48);
                        data[..n].copy_from_slice(&arg.as_bytes()[..n]);
                        data[n] = b'\n';
                        match f.write(&mut fs, &data[..n + 1]) {
                                Ok(w) => info!("fs: wrote {} B; {} is now {} B", w, path, f.size()),
                                Err(e) => info!("fs: write failed: {e:?}"),
                        }
                }
                FsOp::Rm => {
                        //   files and (empty) directories both; the entry decides
                        let r = match fs.stat(path) {
                                Ok(e) if e.is_dir => fs.rmdir(path),
                                Ok(_) => fs.remove(path),
                                Err(e) => Err(e),
                        };
                        match r {
                                Ok(()) => info!("fs: removed {}", path),
                                Err(e) => info!("fs: rm failed: {e:?}"),
                        }
                }
                FsOp::Mv => match fs.rename(path, arg) {
                        Ok(()) => info!("fs: {} -> {}", path, arg),
                        Err(e) => info!("fs: mv failed: {e:?}"),
                },
                FsOp::Mkdir => match fs.mkdir(path) {
                        Ok(()) => info!("fs: created {}/", path),
                        Err(e) => info!("fs: mkdir failed: {e:?}"),
                },
                FsOp::Hex => match (arg.parse::<u32>(), fs.open(path)) {
                        (Ok(off), Ok(mut f)) => {
                                //   16 samples as signed decimals: the shape of captured
                                // audio at a glance -- small around zero, rail-to-rail, or
                                // stuck
                                let mut b = [0u8; 32];
                                let r = f.seek(&mut fs, off).and_then(|()| f.read(&mut fs, &mut b));
                                match r {
                                        Ok(n) => {
                                                let mut line = [0i16; 16];
                                                for i in 0..n / 2 {
                                                        line[i] = i16::from_le_bytes([b[i * 2], b[i * 2 + 1]]);
                                                }
                                                info!("fs: {}@{}: {:?}", path, off, &line[..n / 2]);
                                        }
                                        Err(e) => info!("fs: hex failed: {e:?}"),
                                }
                        }
                        _ => info!("fs: hex needs PATH and a byte offset"),
                },
                FsOp::Trunc => match arg.parse::<u32>() {
                        Ok(len) => match fs.open(path) {
                                Ok(mut f) => match f.truncate(&mut fs, len) {
                                        Ok(()) => info!("fs: {} is now {} B", path, f.size()),
                                        Err(e) => info!("fs: trunc failed: {e:?}"),
                                },
                                Err(e) => info!("fs: open failed: {e:?}"),
                        },
                        Err(_) => info!("fs: trunc needs a byte count"),
                },
                FsOp::Cat => match fs.open(path) {
                        Ok(mut f) => {
                                //   a peek, not a pager: the first 120 bytes, as text
                                let mut buf = [0u8; 120];
                                match f.read(&mut fs, &mut buf) {
                                        Ok(n) => {
                                                let text = core::str::from_utf8(&buf[..n]).unwrap_or("<binary>");
                                                info!("fs: {} ({} B): {}", path, f.size(), text);
                                        }
                                        Err(e) => info!("fs: read failed: {e:?}"),
                                }
                        }
                        Err(e) => info!("fs: open failed: {e:?}"),
                },
        }
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
/// The look-and-feel: the framework's steel theme, the default for every board with
/// color support. A board-specific override would be a local theme file extending it.
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));

// --- the event bus --------------------------------------------------------------------------

/// This board's extension events -- the codec + TF-slot audio leg, the RTC, the PSRAM
/// probe, the power path -- riding the demo's bus.
#[derive(Clone, Copy, Debug)]
enum Ext {
        RtcShow,
        RtcSet(Datetime),
        Tone { hz: u16, ms: u16 },
        ToneOff,
        Volume(u8),
        Psram,
        Sd,
        Fs { op: FsOp, path: FsPath, arg: FsPath },
        RecStart(FsPath),
        RecStop,
        RecStatus,
        PlayStart(FsPath),
        PlayStop,
        PlayStatus,
        MicMon(bool),
        MicDbg,
        Synth,
}

type AppEvent = DemoEvent<Ext>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FsOp {
        Info,
        Ls,
        Cat,
        Write,
        Rm,
        Mv,
        Mkdir,
        Trunc,
        Hex,
}

/// A path argument small enough to ride the event bus by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FsPath {
        buf: [u8; 48],
        len: u8,
}

impl FsPath {
        fn new(s: &str) -> Option<Self> {
                if s.len() > 48 {
                        return None;
                }
                let mut buf = [0u8; 48];
                buf[..s.len()].copy_from_slice(s.as_bytes());
                Some(Self { buf, len: s.len() as u8 })
        }

        fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..usize::from(self.len)]).unwrap_or("")
        }
}

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

//   Bar glass, corners unmeasured: the theme's screen_radius keeps its default of 0
// until the glass says otherwise.
const FPS: u32 = 30;

/// The demo's Dim: through the floor mapping this lands ~59% LED-on time, in the lower
/// part of the panel's narrow usable band -- clearly dim, clearly lit. The original MAX/10
/// mapped below the driver's cutoff and read as OFF.
const BACKLIGHT_DIM: u16 = 250;

demo_pages! {
        event: AppEvent,
        title: "mk4 3.49",
        row_gap: 6,
        //   a 640-tall list has room; rows sized for a finger on the narrow bar
        list_min_row: 56,
        backlight_dim: BACKLIGHT_DIM
}

/// Portrait only, MEASURED: the bar rests near-landscape on its long edge, so ordinary
/// handling flapped LandscapeL/R -- a 180-degree relayout per touch, and every tap then
/// landed where a widget used to be. A 172 px-tall landscape canvas was never worth having
/// anyway; the bar rotates end-for-end (a deliberate gesture, nowhere near the resting
/// pose) and otherwise holds still.
fn rotation_map(o: Orientation) -> Option<Rotation> {
        match o {
                Orientation::Portrait => Some(Rotation::R0),
                Orientation::PortraitFlip => Some(Rotation::R180),
                _ => None,
        }
}

/// No init or unload edges: the AXS15231B wants no GDDRAM offset, and its full-frame
/// pushes repaint everything anyway.
struct Hook;

impl BoardHook<Axs15231b<PioQspiDisplayBus>, Ext> for Hook {}

// --- the modules --------------------------------------------------------------------------

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
        /// The card, shared with the board module -- the dictaphone's storage.
        sd: &'static RefCell<SpiSd<Spi1Bus, Output>>,
        /// A recording in flight: the mounted volume and the growing WAV. A `&'static
        /// mut` INTO .bss, never inline: the module lives on core 0's stack, which is
        /// SCRATCH_Y's fixed 4 KB, and a ~1.2 KB Recording inline was the overflow that
        /// spilled into SCRATCH_X -- core 1's stack -- and wedged both cores at once.
        rec: &'static mut Option<Recording>,
        /// A playback in flight; same .bss discipline.
        play: &'static mut Option<Playback>,
        /// One stream-buffer's worth of file bytes, bulk-read per refill so the DAC ring is
        /// filled from RAM, not per-sample off the card (which starved it: 33 underruns in
        /// a measured playback). Sized for a full mono buffer (STREAM_WORDS frames -- one
        /// word each -- * 2 bytes). In .bss, like everything the card touches.
        play_stage: &'static mut [u8; 4096],
        /// Whether the capture buffers were already handed to the transport.
        cap_handed: bool,
        /// The `rec null` bisect: capture runs, everything drains to nowhere.
        rec_null: bool,
        null_bytes: u32,
}

/// One open recording: the volume stays mounted (over [`SdRef`] borrows) and every filled
/// capture buffer appends to the file, write-through, until stop patches the WAV header.
struct Recording {
        fs: Fat<SdRef>,
        file: FsFile,
}

/// One open playback: the file positioned at its PCM data, streamed into the DAC's
/// ping-pong refill until the data chunk ends.
struct Playback {
        fs: Fat<SdRef>,
        file: FsFile,
        channels: u8,
        /// Where the data chunk stops -- the file may carry trailing chunks.
        data_end: u32,
}

impl AudioMod {
        fn rec_start(&mut self, path: &str) {
                if self.rec.is_some() || self.rec_null {
                        info!("rec: already recording");
                        return;
                }
                if path == "null" {
                        //   the freeze bisect: the WHOLE capture pipeline -- mic, PIO, DMA,
                        // buffer hand-off -- with the SD card and filesystem entirely out of
                        // the path. Stable here + frozen with a file convicts the card leg
                        for _ in 0..3 {
                                if self.codec.mic_enable().is_ok() {
                                        break;
                                }
                        }
                        self.pa.set(false);
                        self.start_capture();
                        self.rec_null = true;
                        self.null_bytes = 0;
                        info!("rec: null sink -- capturing and discarding");
                        return;
                }
                {
                        let mut sd = self.sd.borrow_mut();
                        if sd.card.is_none() {
                                let mut clock = SysClock;
                                if let Err(e) = sd.init(&mut clock) {
                                        info!("rec: no card ({e:?})");
                                        return;
                                }
                        }
                }
                info!("rec: card up");
                let mut fs = match Fat::mount(SdRef(self.sd)) {
                        Ok(fs) => fs,
                        Err(e) => {
                                info!("rec: mount failed: {e:?}");
                                return;
                        }
                };
                info!("rec: mounted");
                let mut file = match fs.create(path) {
                        Ok(f) => f,
                        Err(e) => {
                                info!("rec: create failed: {e:?}");
                                return;
                        }
                };
                info!("rec: created");
                if let Err(e) = file.write(&mut fs, &wav_header(AUDIO_SAMPLE_HZ, 0)) {
                        info!("rec: header write failed: {e:?}");
                        return;
                }
                info!("rec: header written");
                //   retried: the first attempt right after the SD burst has been seen to
                // time out where a later one succeeds
                let mut mic = Err(light_core::hal::I2cError::Timeout);
                for attempt in 1..=3 {
                        mic = self.codec.mic_enable();
                        if mic.is_ok() {
                                info!("rec: mic enabled (attempt {attempt})");
                                break;
                        }
                }
                if let Err(e) = mic {
                        warn!("rec: mic enable failed after retries: {e:?}");
                }
                //   speaker amp OFF for the take: live, it clicks with every SD write
                // burst and the microphone records its own speaker
                self.pa.set(false);
                self.start_capture();
                info!("rec: capture started, speaker muted");
                *self.rec = Some(Recording { fs, file });
                info!("rec: recording {} -- mono 16-bit at {} Hz", path, AUDIO_SAMPLE_HZ);
        }

        fn start_capture(&mut self) {
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

        fn rec_stop(&mut self) {
                self.i2s.capture_stop();
                self.pa.set(true);
                if self.rec_null {
                        self.rec_null = false;
                        info!("rec: null sink stopped -- {} B captured and discarded", self.null_bytes);
                        return;
                }
                let Some(mut rec) = self.rec.take() else {
                        info!("rec: not recording");
                        return;
                };
                //   the header written blind at start now learns the real length
                let data = rec.file.size().saturating_sub(44);
                let patch = rec.file.seek(&mut rec.fs, 0).and_then(|()| rec.file.write(&mut rec.fs, &wav_header(AUDIO_SAMPLE_HZ, data)).map(|_| ()));
                match patch {
                        Ok(()) => info!("rec: stopped -- {} B of audio, ~{} ms", data, data / 2 * 1000 / AUDIO_SAMPLE_HZ),
                        Err(e) => warn!("rec: header patch failed: {e:?}"),
                }
        }

        fn play_start(&mut self, path: &str) {
                if self.play.is_some() {
                        info!("play: already playing");
                        return;
                }
                {
                        let mut sd = self.sd.borrow_mut();
                        if sd.card.is_none() {
                                let mut clock = SysClock;
                                if let Err(e) = sd.init(&mut clock) {
                                        info!("play: no card ({e:?})");
                                        return;
                                }
                        }
                }
                let mut fs = match Fat::mount(SdRef(self.sd)) {
                        Ok(fs) => fs,
                        Err(e) => {
                                info!("play: mount failed: {e:?}");
                                return;
                        }
                };
                let mut file = match fs.open(path) {
                        Ok(f) => f,
                        Err(e) => {
                                info!("play: open failed: {e:?}");
                                return;
                        }
                };
                //   the RIFF walk: WAV headers are chunked, and while OUR files are the
                // canonical 44 bytes, a desktop-authored one may carry LIST chunks first
                let mut hdr = [0u8; 12];
                if file.read(&mut fs, &mut hdr).unwrap_or(0) != 12 || &hdr[..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
                        info!("play: not a WAV");
                        return;
                }
                let (mut channels, mut rate, mut pcm16) = (0u16, 0u32, false);
                for _ in 0..16 {
                        let mut ch = [0u8; 8];
                        if file.read(&mut fs, &mut ch).unwrap_or(0) != 8 {
                                info!("play: no data chunk");
                                return;
                        }
                        let len = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
                        if &ch[..4] == b"fmt " && len >= 16 {
                                let mut f = [0u8; 16];
                                if file.read(&mut fs, &mut f).unwrap_or(0) != 16 {
                                        info!("play: short fmt chunk");
                                        return;
                                }
                                let format = u16::from_le_bytes([f[0], f[1]]);
                                channels = u16::from_le_bytes([f[2], f[3]]);
                                rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                                let bits = u16::from_le_bytes([f[14], f[15]]);
                                pcm16 = format == 1 && bits == 16;
                                let extra = file.pos() + (len - 16) + (len & 1);
                                if file.seek(&mut fs, extra).is_err() {
                                        return;
                                }
                        } else if &ch[..4] == b"data" {
                                if !pcm16 || rate != AUDIO_SAMPLE_HZ || !(1..=2).contains(&channels) {
                                        info!("play: unsupported format ({channels} ch, {rate} Hz) -- this codec run takes 16-bit PCM at {} Hz", AUDIO_SAMPLE_HZ);
                                        return;
                                }
                                let data_end = file.pos().saturating_add(len).min(file.size());
                                //   u64: frames * 1000 overflows u32 past ~4.5 min of audio
                                let ms = ((u64::from(data_end - file.pos()) / (u64::from(channels) * 2)) * 1000 / u64::from(AUDIO_SAMPLE_HZ)) as u32;
                                info!("play: {} -- {} ch, ~{} ms", path, channels, ms);
                                *self.play = Some(Playback { fs, file, channels: channels as u8, data_end });
                                return;
                        } else {
                                let next = file.pos().saturating_add(len + (len & 1));
                                if file.seek(&mut fs, next).is_err() {
                                        return;
                                }
                        }
                }
                info!("play: gave up looking for the data chunk");
        }

        fn play_stop(&mut self) {
                if self.play.take().is_some() {
                        info!("play: stopped");
                } else {
                        info!("play: idle");
                }
        }
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
                //   belt and suspenders against the ADC->DAC monitor's feedback loop: the
                // codec reset in init() already clears REG44, but assert it off explicitly
                // so no prior micmon state can ever survive into a running speaker
                let _ = self.codec.set_adc_to_dac(false);
                static STREAM_A: ConstStaticCell<[u32; light_rp2::i2s::STREAM_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::STREAM_WORDS]);
                static STREAM_B: ConstStaticCell<[u32; light_rp2::i2s::STREAM_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::STREAM_WORDS]);
                //   the POLLED path (no IRQ ring): this board is too RAM-tight for the
                // prefetch ring and its audio is light -- beeps and tones a poll keeps fed
                self.i2s.start_stream([STREAM_A.take(), STREAM_B.take()]);
                self.pa.set(true);
                info!("audio up: es8311 master at {} Hz, PIO1 mclk+dout; the UART console pins now carry audio", AUDIO_SAMPLE_HZ);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Ext(Ext::Tone { hz, ms }) => {
                                        self.phase_inc = ((u64::from(hz) << 32) / u64::from(AUDIO_SAMPLE_HZ)) as u32;
                                        self.remaining = u32::from(ms) * AUDIO_SAMPLE_HZ / 1000;
                                        info!("tone {hz} Hz for {ms} ms");
                                }
                                AppEvent::Ext(Ext::ToneOff) => {
                                        self.remaining = 0;
                                        info!("tone off");
                                }
                                AppEvent::Ext(Ext::Volume(v)) => match self.codec.set_volume(v) {
                                        Ok(()) => info!("volume {v}"),
                                        Err(e) => warn!("volume set failed: {e:?}"),
                                },
                                AppEvent::Ext(Ext::RecStart(path)) => {
                                        let path = path;
                                        self.rec_start(path.as_str());
                                }
                                AppEvent::Ext(Ext::RecStop) => self.rec_stop(),
                                AppEvent::Ext(Ext::RecStatus) => {
                                        match self.rec.as_ref() {
                                                Some(r) => info!("rec: recording, {} B so far, {} overruns", r.file.size(), self.i2s.cap_overruns),
                                                None => info!("rec: idle"),
                                        };
                                }
                                AppEvent::Ext(Ext::PlayStart(path)) => {
                                        let path = path;
                                        self.play_start(path.as_str());
                                }
                                AppEvent::Ext(Ext::Synth) => {
                                        //   write a clean 2 s 440 Hz sine to SINE.WAV via the
                                        // ordinary fs path, so `play SINE.WAV` exercises the
                                        // file-playback chain with a KNOWN-good signal --
                                        // splitting "playback broken" from "recording bad"
                                        if self.sd.borrow().card.is_none() {
                                                let mut clock = SysClock;
                                                let _ = self.sd.borrow_mut().init(&mut clock);
                                        }
                                        match Fat::mount(SdRef(self.sd)) {
                                                Ok(mut fs) => {
                                                        let _ = fs.remove("SINE.WAV");
                                                        match fs.create("SINE.WAV") {
                                                                Ok(mut f) => {
                                                                        let samples = AUDIO_SAMPLE_HZ * 2; // 2 s
                                                                        let _ = f.write(&mut fs, &wav_header(AUDIO_SAMPLE_HZ, samples * 2));
                                                                        let inc = ((440u64 << 32) / u64::from(AUDIO_SAMPLE_HZ)) as u32;
                                                                        let mut phase = 0u32;
                                                                        //   960-byte chunks (480 samples) off the stack -- well under 4 KB
                                                                        let mut chunk = [0u8; 960];
                                                                        let mut written = 0u32;
                                                                        let mut ok = true;
                                                                        while written < samples && ok {
                                                                                let n = ((samples - written) as usize).min(480);
                                                                                for s in 0..n {
                                                                                        let v = SINE[(phase >> 27) as usize];
                                                                                        chunk[s * 2..s * 2 + 2].copy_from_slice(&v.to_le_bytes());
                                                                                        phase = phase.wrapping_add(inc);
                                                                                }
                                                                                ok = f.write(&mut fs, &chunk[..n * 2]).is_ok();
                                                                                written += n as u32;
                                                                        }
                                                                        info!("synth: SINE.WAV written ({} samples) -- play it", written);
                                                                }
                                                                Err(e) => info!("synth: create failed: {e:?}"),
                                                        }
                                                }
                                                Err(e) => info!("synth: mount failed: {e:?}"),
                                        }
                                }
                                AppEvent::Ext(Ext::MicMon(on)) => {
                                        //   enable the mic, route ADC->DAC, unmute and drive
                                        // the speaker: the analog front end, alone
                                        let r = if on {
                                                self.codec.mic_enable().and_then(|()| self.codec.set_adc_to_dac(true)).and_then(|()| self.codec.mute(false))
                                        } else {
                                                self.codec.set_adc_to_dac(false)
                                        };
                                        self.pa.set(on);
                                        match r {
                                                Ok(()) => info!("micmon {}: talk near the board", if on { "on -- speaker carries the mic" } else { "off" }),
                                                Err(e) => warn!("micmon failed: {e:?}"),
                                        }
                                }
                                AppEvent::Ext(Ext::MicDbg) => {
                                        //   enable the mic, start capture (speaker muted, NO
                                        // loopback -- cannot feed back), sample the raw DIN
                                        // pad, then stop. Splits "SDOUT dead" from "PIO
                                        // misreads a toggling line"
                                        self.pa.set(false);
                                        let _ = self.codec.set_adc_to_dac(false);
                                        for _ in 0..3 {
                                                if self.codec.mic_enable().is_ok() {
                                                        break;
                                                }
                                        }
                                        static A: ConstStaticCell<[u16; light_rp2::i2s::CAP_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::CAP_WORDS]);
                                        static B: ConstStaticCell<[u16; light_rp2::i2s::CAP_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::CAP_WORDS]);
                                        let bufs = if self.cap_handed {
                                                None
                                        } else {
                                                self.cap_handed = true;
                                                Some([A.take(), B.take()])
                                        };
                                        let _ = bufs; // the probe uses the SM only, no DMA ring
                                        let (highs, pc) = self.i2s.din_probe(4000);
                                        self.i2s.capture_sm_only();
                                        let fifo = self.i2s.din_fifo_probe();
                                        info!("micdbg: DIN GPIO high {}/4000, sm pc {}", highs, pc);
                                        info!("micdbg: raw FIFO words {:#010x} {:#010x} {:#010x} {:#010x}", fifo[0], fifo[1], fifo[2], fifo[3]);
                                }
                                AppEvent::Ext(Ext::PlayStop) => self.play_stop(),
                                AppEvent::Ext(Ext::PlayStatus) => {
                                        match self.play.as_ref() {
                                                Some(p) => info!("play: at {} of {} B", p.file.pos(), p.data_end),
                                                None => info!("play: idle"),
                                        };
                                }
                                AppEvent::Command(Command::Stats) => {
                                        info!("audio: {} stream underruns, {} capture overruns", self.i2s.underruns, self.i2s.cap_overruns);
                                }
                                _ => {}
                        }
                }
                //   drain the capture ring into the file; an I/O failure ends the take
                let mut failed = false;
                if let Some(rec) = self.rec.as_mut() {
                        let file = &mut rec.file;
                        let fs = &mut rec.fs;
                        let mut err: Option<FsError> = None;
                        self.i2s.capture_take(|buf| {
                                if err.is_some() {
                                        return;
                                }
                                // SAFETY: a u16 slice viewed as its little-endian bytes --
                                // exactly WAV's PCM order
                                let bytes = unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 2) };
                                if let Err(e) = file.write(fs, bytes) {
                                        err = Some(e);
                                }
                        });
                        if let Some(e) = err {
                                warn!("rec: write failed ({e:?}); stopping");
                                failed = true;
                        }
                } else if self.rec_null {
                        //   the bisect sink: drain and discard, no card in the path
                        let n = &mut self.null_bytes;
                        self.i2s.capture_take(|buf| *n += (buf.len() * 2) as u32);
                }
                if failed {
                        self.rec_stop();
                }
                let playing = self.remaining > 0 || self.rec.is_some() || self.rec_null || self.play.is_some();
                if let Some(play) = self.play.as_mut() {
                        //   a file plays: ONE bulk read per DAC buffer into the .bss staging
                        // buffer, then the ring is filled from RAM. Per-sample card reads in
                        // this closure starved the stream (33 underruns / distorted speech)
                        let fs = &mut play.fs;
                        let file = &mut play.file;
                        let ch = usize::from(play.channels);
                        let data_end = play.data_end;
                        let stage = &mut self.play_stage;
                        let mut finished = false;
                        let mut failed = false;
                        self.i2s.refill(|buf| {
                                //   one WORD per frame: the sample in both halves plays it
                                // on both slots (two words per sample here halved the pitch)
                                let frames = buf.len();
                                let want = (frames * ch * 2).min(stage.len());
                                let remaining = data_end.saturating_sub(file.pos()) as usize;
                                let want = want.min(remaining);
                                let got = if finished || failed || want == 0 {
                                        0
                                } else {
                                        match file.read(fs, &mut stage[..want]) {
                                                Ok(n) => n,
                                                Err(_) => {
                                                        failed = true;
                                                        0
                                                }
                                        }
                                };
                                if got < frames * ch * 2 {
                                        finished = true;
                                }
                                let mut si = 0usize;
                                for slot in buf.iter_mut() {
                                        //   the left channel of each frame; mono steps 2 bytes, stereo 4
                                        let sample = if si + 1 < got { i16::from_le_bytes([stage[si], stage[si + 1]]) } else { 0 };
                                        let s = u32::from(sample as u16);
                                        *slot = s << 16 | s;
                                        si += ch * 2;
                                }
                        });
                        if failed {
                                warn!("play: read failed; stopping");
                                *self.play = None;
                        } else if finished {
                                info!("play: finished");
                                *self.play = None;
                        }
                } else {
                        //   pre-borrowed so the closure captures fields disjoint from self.i2s
                        let phase = &mut self.phase;
                        let inc = self.phase_inc;
                        let remaining = &mut self.remaining;
                        self.i2s.refill(|buf| {
                                for slot in buf.iter_mut() {
                                        //   one word per frame, the sample on both slots
                                        let s = if *remaining > 0 { SINE[(*phase >> 27) as usize] } else { 0 };
                                        let s = u32::from(s as u16);
                                        *slot = s << 16 | s;
                                        if *remaining > 0 {
                                                *phase = phase.wrapping_add(inc);
                                                *remaining -= 1;
                                        }
                                }
                        });
                }
                if playing { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.pa.set(false);
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
                                AppEvent::Command(Command::Backlight(level)) => {
                                        busy = true;
                                        self.power.set_backlight(level);
                                        info!("backlight {level}");
                                }
                                AppEvent::Command(Command::Stats) => {
                                        info!("battery: {} mV, {}", self.power.battery_mv(), if self.power.on_external_power() { "external power" } else { "on battery" });
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
                                AppEvent::Ext(Ext::Fs { op, path, arg }) => {
                                        busy = true;
                                        fs_command(&mut self.sd.borrow_mut(), op, path.as_str(), arg.as_str());
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

fn parse_play(w: &mut Words) -> Parsed<AppEvent> {
        match w.next() {
                None => Parsed::Event(DemoEvent::Ext(Ext::PlayStatus)),
                Some("stop") => Parsed::Event(DemoEvent::Ext(Ext::PlayStop)),
                Some(name) => match FsPath::new(name) {
                        Some(p) => Parsed::Event(DemoEvent::Ext(Ext::PlayStart(p))),
                        None => Parsed::Usage,
                },
        }
}

fn parse_rec(w: &mut Words) -> Parsed<AppEvent> {
        match w.next() {
                None => Parsed::Event(DemoEvent::Ext(Ext::RecStatus)),
                Some("stop") => Parsed::Event(DemoEvent::Ext(Ext::RecStop)),
                Some(name) => match FsPath::new(name) {
                        Some(p) => Parsed::Event(DemoEvent::Ext(Ext::RecStart(p))),
                        None => Parsed::Usage,
                },
        }
}

fn parse_fs(w: &mut Words) -> Parsed<AppEvent> {
        let op = match w.next() {
                Some("info") | None => FsOp::Info,
                Some("ls") => FsOp::Ls,
                Some("cat") => FsOp::Cat,
                Some("write") => FsOp::Write,
                Some("rm") => FsOp::Rm,
                Some("mv") => FsOp::Mv,
                Some("mkdir") => FsOp::Mkdir,
                Some("trunc") => FsOp::Trunc,
                Some("hex") => FsOp::Hex,
                _ => return Parsed::Usage,
        };
        let path = w.next().unwrap_or("");
        let arg = w.next().unwrap_or("");
        if !matches!(op, FsOp::Info | FsOp::Ls) && path.is_empty() {
                return Parsed::Usage;
        }
        if matches!(op, FsOp::Write | FsOp::Mv | FsOp::Trunc | FsOp::Hex) && arg.is_empty() {
                return Parsed::Usage;
        }
        match (FsPath::new(path), FsPath::new(arg)) {
                (Some(path), Some(arg)) => Parsed::Event(DemoEvent::Ext(Ext::Fs { op, path, arg })),
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
                                        Ok(ms) if ms > 0 => ms,
                                        _ => return Parsed::Usage,
                                },
                        };
                        Parsed::Event(DemoEvent::Ext(Ext::Tone { hz, ms }))
                }
                None => Parsed::Usage,
        }
}

fn parse_volume(w: &mut Words) -> Parsed<AppEvent> {
        match w.next().and_then(|s| s.parse::<u8>().ok()) {
                Some(v) if v <= 100 => Parsed::Event(DemoEvent::Ext(Ext::Volume(v))),
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[CliCommand<AppEvent>] = &demo_commands![Ext;
        CliCommand { name: "rtc", usage: "rtc | rtc set YYYY-MM-DD HH:MM:SS", parse: parse_rtc },
        CliCommand { name: "tone", usage: "tone HZ [MS] | tone off", parse: parse_tone },
        CliCommand { name: "volume", usage: "volume 0..100", parse: parse_volume },
        CliCommand { name: "psram", usage: "psram", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Psram)) },
        CliCommand { name: "sd", usage: "sd", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Sd)) },
        CliCommand { name: "fs", usage: "fs info|ls [P]|cat P|write P TEXT|rm P|mv A B|mkdir P|trunc P N", parse: parse_fs },
        CliCommand { name: "rec", usage: "rec NAME.WAV | rec stop | rec", parse: parse_rec },
        CliCommand { name: "play", usage: "play NAME.WAV | play stop | play", parse: parse_play },
        CliCommand { name: "micmon", usage: "micmon on|off", parse: |w| match w.next() { Some("on") => Parsed::Event(DemoEvent::Ext(Ext::MicMon(true))), Some("off") => Parsed::Event(DemoEvent::Ext(Ext::MicMon(false))), _ => Parsed::Usage } },
        CliCommand { name: "micdbg", usage: "micdbg", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::MicDbg)) },
        CliCommand { name: "synth", usage: "synth", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Synth)) },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

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

        //   the one card, shared: the board module's fs/sd console commands and the
        // recorder both borrow it per operation
        static SD_CELL: StaticCell<RefCell<SpiSd<Spi1Bus, Output>>> = StaticCell::new();
        let sd: &'static RefCell<SpiSd<Spi1Bus, Output>> = SD_CELL.init(RefCell::new(SpiSd::new(p.sd_spi, p.sd_cs)));
        let mut board_mod = BoardMod {
                power: PowerManager::new(p.backlight, p.sys_en, p.power_button, p.battery, p.charge_stat),
                sd,
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
                sd,
                // SAFETY: the one take of each slot (see AppSlot)
                rec: unsafe { &mut *REC_SLOT.0.get() },
                play: unsafe { &mut *PLAY_SLOT.0.get() },
                play_stage: {
                        //   STREAM_WORDS * 2: one full mono refill. 2560 was the size for
                        // the ORIGINAL ring; when the underrun fix grew STREAM_WORDS to
                        // 2048 the dictaphone's copy was resized and this one was not, and
                        // a stage smaller than one refill reads short and declares the
                        // playback finished on its first buffer
                        static STAGE: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
                        STAGE.take()
                },
                cap_handed: false,
                rec_null: false,
                null_bytes: 0,
        };
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
        type BoardDisplayMod = DisplayMod<Axs15231b<PioQspiDisplayBus>, SysClock, Ext, Hook>;
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
                        desc: "AXS15231B over PIO-QSPI, double-buffered",
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
