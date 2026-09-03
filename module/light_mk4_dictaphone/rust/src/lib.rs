//! The dictaphone: a voice recorder with a dedicated touch interface, the second
//! application for the Waveshare RP2350-Touch-LCD-3.49 (a 172x640 QSPI bar of glass with
//! the ES8311 codec, an analog microphone and a TF slot). Same board wiring as the
//! `light_mk4_touch349` demo -- wiring is the application's, copied, per the architecture
//! rule -- with the demo's widget pages replaced by the recorder's.
//!
//! The interface: a main page with a live status line (elapsed time while recording or
//! playing), one big record/stop button, play-last, and a recordings list -- the newest
//! [`LIST_ROWS`] takes, tap to play. Recordings are auto-named `REC_NNNN.WAV` (8.3: the
//! filesystem writes short names only) and land as ordinary mono 16-bit WAV any desktop
//! player opens. The full bench console (`rec`/`play`/`fs`/`tone`/`micdbg`/...) stays: it
//! is the same machinery the buttons drive.

#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use light_input::axs15231b::{self as axs, Axs15231bTouch};
use light_input::imu::{Imu, Orientation};
use light_input::qmi8658::Qmi8658;
use light_display::axs15231b::Axs15231b;
use light_input::touch::{Gesture, Tracker};
use light_ui::{scroll, Desc, Page, SwipeDir, TextSlot, Touch, Ui};
use light_core::cli::{Cli, Command as CliCommand, Outcome, Parsed, Words};
use light_core::{debug, info, log, warn, ConstStaticCell, EventBus, InputPin, LineReader, Mailbox, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer, UpdateError};
use light_draw::{PixelFormat, Rotation};
use light_audio::Es8311;
use light_font::Font;
use light_rtc::{Datetime, Pcf85063a};
use light_fs::{Fat, File as FsFile, FsError};
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
                FsOp::Peak => match fs.open(path) {
                        //   the whole take's level in one line: peak and RMS of the PCM,
                        // what gain tuning actually needs (16-sample hex windows sample
                        // 0.1% of a take and miss every real peak)
                        Ok(mut f) => {
                                let r = f.seek(&mut fs, 44);
                                let mut buf = [0u8; 512];
                                let (mut peak, mut sum, mut n) = (0i32, 0u64, 0u64);
                                let mut err = r.err();
                                while err.is_none() {
                                        match f.read(&mut fs, &mut buf) {
                                                Ok(0) => break,
                                                Ok(got) => {
                                                        for i in 0..got / 2 {
                                                                let s = i32::from(i16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]));
                                                                peak = peak.max(s.abs());
                                                                sum += (s * s) as u64;
                                                                n += 1;
                                                        }
                                                }
                                                Err(e) => err = Some(e),
                                        }
                                }
                                if let Some(e) = err {
                                        info!("fs: peak failed: {e:?}");
                                } else {
                                        let mean = if n > 0 { sum / n } else { 0 };
                                        let mut rms = 0u64;
                                        while (rms + 1) * (rms + 1) <= mean {
                                                rms += 1;
                                        }
                                        info!("fs: {} peak {} rms {} over {} samples", path, peak, rms, n);
                                }
                        }
                        Err(e) => info!("fs: open failed: {e:?}"),
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

// --- the event bus --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        Touch(axs::Event),
        Gesture(Gesture),
        Orientation(Orientation),
        Command(Command),
        Ui(UiAction),
        /// The audio module's state, published whenever it changes (which includes every
        /// elapsed second); the display renders it into the status line and button labels.
        Status(AudioStatus),
        /// A recordings-list row's text: the scan hands names to the display one row at a
        /// time, so neither module holds the other's data.
        RowText { row: u8, name: FsPath },
}

/// What the audio module is doing, in the terms the interface shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioStatus {
        Idle,
        /// A recording in flight, with its elapsed seconds.
        Recording { secs: u16 },
        /// A playback in flight: elapsed and total seconds.
        Playing { secs: u16, total: u16 },
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
        /// The mic path's two gain registers, raw -- live tuning without a reflash.
        MicGain { r14: u8, r17: u8 },
}

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
        Peak,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderMode {
        Normal,
        Paused,
        Repush,
}

#[derive(Clone, Copy, Debug)]
enum UiAction {
        /// The big button: start a recording, or stop the one in flight.
        RecToggle,
        /// Play the most recent recording, or stop the playback in flight.
        PlayToggle,
        /// The recordings page was opened: scan the card and fill the rows.
        FilesOpen,
        /// A recordings-list row was tapped.
        PlayRow(u8),
        DragConsumed,
}

static EVENTS: EventBus<AppEvent, 16, 6> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();
/// Core 1's pulse, counted every `light_app_core1_service` pass and shown in the status
/// line by core 0: the console cannot report its own death (a dead core 1 IS a dead
/// console -- observed as "the app wedged" until the glass proved core 0 was fine), so
/// the screen carries the diagnosis instead.
static CORE1_TICKS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The base of SCRATCH_X. Core 0's stack fills SCRATCH_Y above it, and with the shell
/// now giving core 1 a stack in ordinary RAM (see `light_mk4_shell/src/main.c` -- a deep
/// core-0 call chain landed on core 1's frames here and killed the console silently),
/// SCRATCH_X is vacant runway. The watermark paints it plus the bottom of core 0's own
/// bank, so the diag row shows how deep the deepest call chain really reaches.
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
        CORE1_TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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

/// Widget tags: how a module finds a widget again to rewrite its text at runtime.
const TAG_STATUS: u8 = 1;
const TAG_REC: u8 = 2;
const TAG_PLAY: u8 = 3;
/// The recordings list's rows carry `TAG_ROW_BASE + row`.
const TAG_ROW_BASE: u8 = 0x10;

/// How many recordings the list page shows: the newest N, one fixed row each -- the page
/// tree is static, its TEXT is not.
const LIST_ROWS: usize = 8;

static LBL_STATUS: Desc<AppEvent> = Desc::label("ready").tag(TAG_STATUS).min_size(0, 40);
static BTN_REC: Desc<AppEvent> = Desc::button("* Record").emit(AppEvent::Ui(UiAction::RecToggle)).tag(TAG_REC).min_size(0, 88);
static BTN_PLAY: Desc<AppEvent> = Desc::button("Play last").emit(AppEvent::Ui(UiAction::PlayToggle)).tag(TAG_PLAY).min_size(0, LIST_MIN_ROW);
static BTN_FILES: Desc<AppEvent> = Desc::button("Recordings >").emit(AppEvent::Ui(UiAction::FilesOpen)).navigate(&PAGE_FILES).min_size(0, LIST_MIN_ROW);
static MAIN_WINDOW: Desc<AppEvent> = Desc::window("Dictaphone").rounded(CORNER_RADIUS).stack(ROW_GAP).children(&[&LBL_STATUS, &BTN_REC, &BTN_PLAY, &BTN_FILES]);

static ROW_0: Desc<AppEvent> = Desc::button("-").emit(AppEvent::Ui(UiAction::PlayRow(0))).tag(TAG_ROW_BASE).min_size(0, LIST_MIN_ROW);
static ROW_1: Desc<AppEvent> = Desc::button("-").emit(AppEvent::Ui(UiAction::PlayRow(1))).tag(TAG_ROW_BASE + 1).min_size(0, LIST_MIN_ROW);
static ROW_2: Desc<AppEvent> = Desc::button("-").emit(AppEvent::Ui(UiAction::PlayRow(2))).tag(TAG_ROW_BASE + 2).min_size(0, LIST_MIN_ROW);
static ROW_3: Desc<AppEvent> = Desc::button("-").emit(AppEvent::Ui(UiAction::PlayRow(3))).tag(TAG_ROW_BASE + 3).min_size(0, LIST_MIN_ROW);
static ROW_4: Desc<AppEvent> = Desc::button("-").emit(AppEvent::Ui(UiAction::PlayRow(4))).tag(TAG_ROW_BASE + 4).min_size(0, LIST_MIN_ROW);
static ROW_5: Desc<AppEvent> = Desc::button("-").emit(AppEvent::Ui(UiAction::PlayRow(5))).tag(TAG_ROW_BASE + 5).min_size(0, LIST_MIN_ROW);
static ROW_6: Desc<AppEvent> = Desc::button("-").emit(AppEvent::Ui(UiAction::PlayRow(6))).tag(TAG_ROW_BASE + 6).min_size(0, LIST_MIN_ROW);
static ROW_7: Desc<AppEvent> = Desc::button("-").emit(AppEvent::Ui(UiAction::PlayRow(7))).tag(TAG_ROW_BASE + 7).min_size(0, LIST_MIN_ROW);
static BTN_FILES_BACK: Desc<AppEvent> = Desc::button("< Back").back().min_size(0, LIST_MIN_ROW);
static FILES_WINDOW: Desc<AppEvent> = Desc::window("Recordings")
        .rounded(CORNER_RADIUS)
        .stack(ROW_GAP)
        .scroll(scroll::VERTICAL)
        .children(&[&ROW_0, &ROW_1, &ROW_2, &ROW_3, &ROW_4, &ROW_5, &ROW_6, &ROW_7, &BTN_FILES_BACK]);

static PAGE_MAIN: Page<AppEvent> = Page::new(&MAIN_WINDOW, None);
static PAGE_FILES: Page<AppEvent> = Page::new(&FILES_WINDOW, Some(&PAGE_MAIN));

const UI_WIDGETS: usize = 12;

// --- the modules --------------------------------------------------------------------------

struct DisplayMod {
        display: Display<'static, Axs15231b<PioQspiDisplayBus>>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<AppEvent, UI_WIDGETS>,
        events: Subscription,
        mode: RenderMode,
        /// What the status line shows.
        status: AudioStatus,
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
                        AppEvent::Status(s) => {
                                //   the action buttons follow the audio module's state;
                                // find() returns None for tags on the page that is not
                                // built, which is exactly the right no-op
                                self.status = s;
                                self.render_status();
                                let (rec_label, play_label) = match s {
                                        AudioStatus::Idle => ("* Record", "Play last"),
                                        AudioStatus::Recording { .. } => ("# Stop", "Play last"),
                                        AudioStatus::Playing { .. } => ("* Record", "# Stop"),
                                };
                                if let Some(id) = self.ui.find(TAG_REC) {
                                        self.ui.set_label(id, rec_label);
                                }
                                if let Some(id) = self.ui.find(TAG_PLAY) {
                                        self.ui.set_label(id, play_label);
                                }
                        }
                        AppEvent::RowText { row, name } => {
                                if let Some(id) = self.ui.find(TAG_ROW_BASE + row) {
                                        if name.len == 0 {
                                                self.ui.set_label(id, "-");
                                        } else {
                                                self.ui.set_text(id, name.as_str());
                                        }
                                }
                        }
                        AppEvent::Command(Command::Stats) => {
                                info!(
                                        "display: {} frames, {} skipped, {} chunk timeouts; max draw {} us, max push {} us",
                                        self.layer.frames(),
                                        self.layer.skipped,
                                        self.display.timeouts,
                                        self.draw_us_max,
                                        self.push_us_max
                                );
                                //   the instruments that convicted the core-1 stack kill,
                                // off the glass and onto the (now trustworthy) console
                                info!("cores: core1 ticks {}, core0 stack low-water {} B above the runway base", CORE1_TICKS.load(core::sync::atomic::Ordering::Relaxed), stack_free());
                                self.draw_us_max = 0;
                                self.push_us_max = 0;
                        }
                        _ => {}
                }
        }

        /// The status line: the audio state plus core 1's pulse. The pulse is the point --
        /// a dead core 1 is a dead console, so the DIAGNOSIS has to ride the display: a
        /// frozen number under a live UI says core 1 halted; a counting number under a
        /// silent console says core 1 is fine and the CDC transport is what died.
        fn render_status(&mut self) {
                let mut line = StackString::<{ TextSlot::CAP }>::new();
                match self.status {
                        AudioStatus::Idle => {
                                let _ = write!(line, "ready");
                        }
                        AudioStatus::Recording { secs } => {
                                let _ = write!(line, "REC {}:{:02}", secs / 60, secs % 60);
                        }
                        AudioStatus::Playing { secs, total } => {
                                let _ = write!(line, "play {}:{:02}/{}:{:02}", secs / 60, secs % 60, total / 60, total % 60);
                        }
                }
                if let Some(id) = self.ui.find(TAG_STATUS) {
                        self.ui.set_text(id, line.as_str());
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
        /// a bench playback). Sized for a full mono buffer (STREAM_WORDS frames -- one
        /// word each -- * 2 bytes). In .bss, like everything the card touches.
        play_stage: &'static mut [u8; 4096],
        /// Whether the capture buffers were already handed to the transport.
        cap_handed: bool,
        /// The `rec null` bisect: capture runs, everything drains to nowhere.
        rec_null: bool,
        null_bytes: u32,
        /// The most recent recording -- what "Play last" plays.
        last_name: Option<FsPath>,
        /// The list page's rows, newest first; in .bss like everything sizeable here.
        list: &'static mut [Option<FsPath>; LIST_ROWS],
        /// Publish-on-change: the last [`AudioStatus`] the interface was told about. The
        /// elapsed second is part of the value, so a live take updates itself.
        last_status: AudioStatus,
        /// The mic path's gain pair (SYSTEM14: mic select + analog PGA; ADC17: digital
        /// volume), applied at every record start -- `micgain` retunes it live.
        mic14: u8,
        mic17: u8,
        /// The staging buffer's fill state: bytes valid, bytes consumed. The stage is
        /// topped up OUTSIDE the DAC refill so a slow card read gets a whole buffer
        /// period of slack instead of racing the ring's deadline.
        stage_filled: usize,
        stage_used: usize,
        /// The worst gap between this module's polls while audio was in flight -- the
        /// number the stream buffer's duration has to beat. `stats` prints and resets it.
        poll_us_last: u64,
        poll_gap_max_us: u32,
}

/// `REC_NNNN.WAV` -> `NNNN`; anything else is not one of ours.
fn rec_number(name: &str) -> Option<u32> {
        let digits = name.strip_prefix("REC_")?.strip_suffix(".WAV")?;
        if digits.len() != 4 {
                return None;
        }
        digits.parse().ok()
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
        /// Where the PCM begins -- with `data_end`, what elapsed/total time is read from.
        data_start: u32,
}

impl AudioMod {
        /// What the interface should say right now, read straight from the state.
        fn status(&self) -> AudioStatus {
                let rate = AUDIO_SAMPLE_HZ * 2;
                if let Some(r) = self.rec.as_ref() {
                        AudioStatus::Recording { secs: (r.file.size().saturating_sub(44) / rate) as u16 }
                } else if let Some(p) = self.play.as_ref() {
                        let rate = rate * u32::from(p.channels);
                        AudioStatus::Playing {
                                secs: (p.file.pos().saturating_sub(p.data_start) / rate) as u16,
                                total: (p.data_end.saturating_sub(p.data_start) / rate) as u16,
                        }
                } else {
                        AudioStatus::Idle
                }
        }

        /// Mount the card (initialising it on first touch) for a UI operation, or say why not.
        fn mount(&mut self) -> Option<Fat<SdRef>> {
                if self.sd.borrow().card.is_none() {
                        let mut clock = SysClock;
                        if let Err(e) = self.sd.borrow_mut().init(&mut clock) {
                                info!("card: none ({e:?})");
                                return None;
                        }
                }
                match Fat::mount(SdRef(self.sd)) {
                        Ok(fs) => Some(fs),
                        Err(e) => {
                                info!("card: mount failed: {e:?}");
                                None
                        }
                }
        }

        /// The next free auto-name, `REC_NNNN.WAV`: one past the highest on the card.
        fn next_name(&mut self) -> Option<FsPath> {
                let mut fs = self.mount()?;
                let mut max = 0u32;
                let _ = fs.list_dir("/", |e| {
                        if let Some(n) = rec_number(e.name()) {
                                max = max.max(n);
                        }
                });
                let mut s = StackString::<16>::new();
                let _ = write!(s, "REC_{:04}.WAV", (max + 1) % 10000);
                FsPath::new(s.as_str())
        }

        /// Fill the list page: the newest [`LIST_ROWS`] recordings, one RowText per row
        /// (an empty name is an unused row), and remember them for taps.
        fn scan_files(&mut self) {
                for slot in self.list.iter_mut() {
                        *slot = None;
                }
                if let Some(mut fs) = self.mount() {
                        let list = &mut *self.list;
                        let _ = fs.list_dir("/", |e| {
                                if e.is_dir || rec_number(e.name()).is_none() {
                                        return;
                                }
                                let Some(p) = FsPath::new(e.name()) else { return };
                                //   insertion, highest number (lexicographic on the
                                // zero-padded name) first; the displaced row carries on down
                                let mut cand = Some(p);
                                for slot in list.iter_mut() {
                                        match (*slot, cand) {
                                                (None, Some(_)) => *slot = cand.take(),
                                                (Some(cur), Some(c)) if c.as_str() > cur.as_str() => {
                                                        cand = Some(cur);
                                                        *slot = Some(c);
                                                }
                                                _ => {}
                                        }
                                }
                        });
                }
                let empty = FsPath::new("").unwrap_or(FsPath { buf: [0; 48], len: 0 });
                for (i, slot) in self.list.iter().enumerate() {
                        let _ = EVENTS.publish(AppEvent::RowText { row: i as u8, name: slot.unwrap_or(empty) });
                }
                if self.last_name.is_none() {
                        self.last_name = self.list[0];
                }
        }

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
                //   retried: the first attempt right after the SD burst has timed out on
                // the bench where a later one succeeds
                let mut mic = Err(light_core::hal::I2cError::Timeout);
                for attempt in 1..=3 {
                        mic = self.codec.mic_config(self.mic14, self.mic17);
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
                self.last_name = FsPath::new(path);
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
                //   step logs like rec's: twice a wedge struck between the command echo and
                // the first outcome line, and nothing said which step held the core
                info!("play: starting {path}");
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
                info!("play: mounted");
                let mut file = match fs.open(path) {
                        Ok(f) => f,
                        Err(e) => {
                                info!("play: open failed: {e:?}");
                                return;
                        }
                };
                info!("play: open, {} B", file.size());
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
                                let data_start = file.pos();
                                let data_end = data_start.saturating_add(len).min(file.size());
                                let ms = (data_end - data_start) / (u32::from(channels) * 2) * 1000 / AUDIO_SAMPLE_HZ;
                                info!("play: {} -- {} ch, ~{} ms", path, channels, ms);
                                self.stage_filled = 0;
                                self.stage_used = 0;
                                *self.play = Some(Playback { fs, file, channels: channels as u8, data_end, data_start });
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
                //   tuned with the mic gain above: 85 audibly overdrove the little
                // speaker on healthy-level takes, 70 was near-inaudible (the register is
                // ~0.5 dB per step of this 0..100 scale -- the knob is steep)
                let _ = self.codec.set_volume(78);
                //   belt and suspenders against the ADC->DAC monitor's feedback loop: the
                // codec reset in init() already clears REG44, but assert it off explicitly
                // so no prior micmon state can ever survive into a running speaker
                let _ = self.codec.set_adc_to_dac(false);
                static STREAM_A: ConstStaticCell<[u32; light_rp2::i2s::STREAM_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::STREAM_WORDS]);
                static STREAM_B: ConstStaticCell<[u32; light_rp2::i2s::STREAM_WORDS]> = ConstStaticCell::new([0; light_rp2::i2s::STREAM_WORDS]);
                self.i2s.start_stream([STREAM_A.take(), STREAM_B.take()]);
                self.pa.set(true);
                info!("audio up: es8311 master at {} Hz, PIO1 mclk+dout; the UART console pins now carry audio", AUDIO_SAMPLE_HZ);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                //   the worst poll-to-poll gap while audio is in flight: measured, not
                // guessed, because the stream buffer's 53 ms is a budget this gap spends
                let now = light_rp2::now_us();
                if self.rec.is_some() || self.play.is_some() || self.rec_null || self.remaining > 0 {
                        if self.poll_us_last != 0 {
                                self.poll_gap_max_us = self.poll_gap_max_us.max((now - self.poll_us_last) as u32);
                        }
                        self.poll_us_last = now;
                } else {
                        self.poll_us_last = 0;
                }
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
                                AppEvent::Command(Command::RecStart(path)) => {
                                        let path = path;
                                        self.rec_start(path.as_str());
                                }
                                AppEvent::Command(Command::RecStop) => self.rec_stop(),
                                AppEvent::Command(Command::MicGain { r14, r17 }) => {
                                        self.mic14 = r14;
                                        self.mic17 = r17;
                                        //   applied immediately too, so a live take retunes
                                        // mid-recording and the next one starts here
                                        match self.codec.mic_config(r14, r17) {
                                                Ok(()) => info!("micgain: REG14 0x{r14:02x}, REG17 0x{r17:02x}"),
                                                Err(e) => warn!("micgain: applied at next rec ({e:?})"),
                                        }
                                }
                                AppEvent::Ui(UiAction::RecToggle) => {
                                        if self.rec.is_some() || self.rec_null {
                                                self.rec_stop();
                                        } else {
                                                if self.play.is_some() {
                                                        self.play_stop();
                                                }
                                                if let Some(name) = self.next_name() {
                                                        self.rec_start(name.as_str());
                                                }
                                        }
                                }
                                AppEvent::Ui(UiAction::PlayToggle) => {
                                        if self.play.is_some() {
                                                self.play_stop();
                                        } else if self.rec.is_some() {
                                                info!("play: stop the recording first");
                                        } else {
                                                if self.last_name.is_none() {
                                                        //   fresh boot: the newest take on the card is "last"
                                                        self.scan_files();
                                                }
                                                match self.last_name {
                                                        Some(p) => self.play_start(p.as_str()),
                                                        None => info!("play: nothing recorded yet"),
                                                }
                                        }
                                }
                                AppEvent::Ui(UiAction::FilesOpen) => self.scan_files(),
                                AppEvent::Ui(UiAction::PlayRow(i)) => {
                                        if let Some(p) = self.list.get(usize::from(i)).copied().flatten() {
                                                if self.rec.is_some() {
                                                        info!("play: stop the recording first");
                                                } else {
                                                        if self.play.is_some() {
                                                                self.play_stop();
                                                        }
                                                        self.play_start(p.as_str());
                                                        self.last_name = Some(p);
                                                }
                                        }
                                }
                                AppEvent::Command(Command::RecStatus) => {
                                        match self.rec.as_ref() {
                                                Some(r) => info!("rec: recording, {} B so far, {} overruns", r.file.size(), self.i2s.cap_overruns),
                                                None => info!("rec: idle"),
                                        };
                                }
                                AppEvent::Command(Command::PlayStart(path)) => {
                                        let path = path;
                                        self.play_start(path.as_str());
                                }
                                AppEvent::Command(Command::Synth) => {
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
                                AppEvent::Command(Command::MicMon(on)) => {
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
                                AppEvent::Command(Command::MicDbg) => {
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
                                AppEvent::Command(Command::PlayStop) => self.play_stop(),
                                AppEvent::Command(Command::PlayStatus) => {
                                        match self.play.as_ref() {
                                                Some(p) => info!("play: at {} of {} B", p.file.pos(), p.data_end),
                                                None => info!("play: idle"),
                                        };
                                }
                                AppEvent::Command(Command::Stats) => {
                                        //   reset on read, so each reading covers the interval
                                        // since the last -- a cumulative count here spent a
                                        // debugging session being misread as per-playback
                                        info!("audio: {} stream underruns, {} capture overruns; max poll gap {} us", self.i2s.underruns, self.i2s.cap_overruns, self.poll_gap_max_us);
                                        self.i2s.underruns = 0;
                                        self.i2s.cap_overruns = 0;
                                        self.poll_gap_max_us = 0;
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
                        //   a file plays. The card read happens HERE, outside the refill:
                        // topping the stage up when it empties gives a slow read a whole
                        // buffer period of slack, where a read inside the refill callback
                        // raced the ring's deadline and an occasional card stall was an
                        // audible click. The refill itself is a pure RAM copy.
                        let fs = &mut play.fs;
                        let file = &mut play.file;
                        let ch = usize::from(play.channels);
                        let data_end = play.data_end;
                        let stage = &mut *self.play_stage;
                        let mut finished = false;
                        let mut failed = false;
                        if self.stage_used >= self.stage_filled {
                                let remaining = data_end.saturating_sub(file.pos()) as usize;
                                let want = stage.len().min(remaining);
                                if want == 0 {
                                        finished = true;
                                } else {
                                        match file.read(fs, &mut stage[..want]) {
                                                Ok(0) => finished = true,
                                                Ok(n) => {
                                                        self.stage_filled = n;
                                                        self.stage_used = 0;
                                                }
                                                Err(_) => failed = true,
                                        }
                                }
                        }
                        let su = &mut self.stage_used;
                        let sf = self.stage_filled;
                        self.i2s.refill(|buf| {
                                //   one WORD per frame, the sample in both slots; the stage
                                // drains by `ch * 2` bytes per frame (mono 2, stereo 4 --
                                // the left channel is what plays) and past its end the
                                // frame is silence
                                for slot in buf.iter_mut() {
                                        let sample = if *su + 1 < sf {
                                                let v = i16::from_le_bytes([stage[*su], stage[*su + 1]]);
                                                *su += ch * 2;
                                                v
                                        } else {
                                                *su = sf;
                                                0
                                        };
                                        let s = u32::from(sample as u16);
                                        *slot = s << 16 | s;
                                }
                        });
                        if failed {
                                warn!("play: read failed; stopping");
                                *self.play = None;
                        } else if finished && self.stage_used >= self.stage_filled {
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
                //   publish-on-change: the elapsed second is part of the status value, so a
                // live take announces itself once a second and transitions announce at once
                let s = self.status();
                if s != self.last_status {
                        self.last_status = s;
                        let _ = EVENTS.publish(AppEvent::Status(s));
                }
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
                                }
                                AppEvent::Command(Command::Sd) => {
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
                                AppEvent::Command(Command::Fs { op, path, arg }) => {
                                        busy = true;
                                        fs_command(&mut self.sd.borrow_mut(), op, path.as_str(), arg.as_str());
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

fn parse_play(w: &mut Words) -> Parsed<Command> {
        match w.next() {
                None => Parsed::Event(Command::PlayStatus),
                Some("stop") => Parsed::Event(Command::PlayStop),
                Some(name) => match FsPath::new(name) {
                        Some(p) => Parsed::Event(Command::PlayStart(p)),
                        None => Parsed::Usage,
                },
        }
}

fn parse_rec(w: &mut Words) -> Parsed<Command> {
        match w.next() {
                None => Parsed::Event(Command::RecStatus),
                Some("stop") => Parsed::Event(Command::RecStop),
                Some(name) => match FsPath::new(name) {
                        Some(p) => Parsed::Event(Command::RecStart(p)),
                        None => Parsed::Usage,
                },
        }
}

fn parse_fs(w: &mut Words) -> Parsed<Command> {
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
                Some("peak") => FsOp::Peak,
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
                (Some(path), Some(arg)) => Parsed::Event(Command::Fs { op, path, arg }),
                _ => Parsed::Usage,
        }
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
        CliCommand { name: "sd", usage: "sd", parse: |_| Parsed::Event(Command::Sd) },
        CliCommand { name: "fs", usage: "fs info|ls [P]|cat P|write P TEXT|rm P|mv A B|mkdir P|trunc P N", parse: parse_fs },
        CliCommand { name: "rec", usage: "rec NAME.WAV | rec stop | rec", parse: parse_rec },
        CliCommand { name: "play", usage: "play NAME.WAV | play stop | play", parse: parse_play },
        CliCommand { name: "micmon", usage: "micmon on|off", parse: |w| match w.next() { Some("on") => Parsed::Event(Command::MicMon(true)), Some("off") => Parsed::Event(Command::MicMon(false)), _ => Parsed::Usage } },
        CliCommand { name: "micdbg", usage: "micdbg", parse: |_| Parsed::Event(Command::MicDbg) },
        CliCommand { name: "synth", usage: "synth", parse: |_| Parsed::Event(Command::Synth) },
        CliCommand { name: "micgain", usage: "micgain R14HEX R17HEX (e.g. micgain 17 DF)", parse: |w| match (w.next().and_then(|s| u8::from_str_radix(s, 16).ok()), w.next().and_then(|s| u8::from_str_radix(s, 16).ok())) {
                (Some(r14), Some(r17)) => Parsed::Event(Command::MicGain { r14, r17 }),
                _ => Parsed::Usage,
        } },
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

        //   the one card, shared: the board module's fs/sd console commands and the
        // recorder both borrow it per operation
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
                        static STAGE: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
                        STAGE.take()
                },
                cap_handed: false,
                rec_null: false,
                null_bytes: 0,
                last_name: None,
                list: {
                        static LIST: ConstStaticCell<[Option<FsPath>; LIST_ROWS]> = ConstStaticCell::new([None; LIST_ROWS]);
                        LIST.take()
                },
                last_status: AudioStatus::Idle,
                //   tuned on the glass 2026-09-04: PGA code 7 with +18 dB digital puts
                // normal speech peaks ~45% of full scale -- and a DELIBERATELY loud take
                // measured at +2 dB more gain pinned full scale, so this is as hot as a
                // recorder without a limiter should default. The analog field is
                // treacherous -- 0x17 -> 0x18 COLLAPSED the gain ~20 dB (the encoding is
                // not linear); retune digitally, in REG17, only
                mic14: 0x17,
                mic17: 0xE3,
                stage_filled: 0,
                stage_used: 0,
                poll_us_last: 0,
                poll_gap_max_us: 0,
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
                mode: RenderMode::Normal,
                status: AudioStatus::Idle,
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

        fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
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
