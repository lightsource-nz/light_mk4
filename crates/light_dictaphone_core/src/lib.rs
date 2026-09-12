//! The dictaphone's ENGINE: a voice recorder with no hardware and no page layout in it.
//! Recordings are auto-named `REC_NNNN.WAV` (8.3: the filesystem writes short names only)
//! and land as ordinary mono 16-bit WAV any desktop player opens. The full diagnostic
//! console (`rec`/`play`/`fs`/`tone`/`micdbg`/...) lives here: it is the same machinery
//! any interface's buttons drive.
//!
//! A UI crate (`light_app_dictaphone`, `light_app_dictaphone_wide`) supplies the page
//! TREE -- what the interface looks like -- while this crate supplies everything the tree
//! emits into: the event vocabulary, the audio and storage state machines, and the
//! display-driving [`DisplayMod`] (render loop, touch routing, command plumbing -- the
//! machinery every layout shares). A hardware-bound module then constructs the concrete
//! parts and owns everything neither crate may: pins, the display driver, the touch
//! controller, the I2S transport and its buffers, the card, the shell ABI and the panic
//! handler. The seams, in order of appearance:
//! - [`Event<X>`] carries the app's events plus a board EXTENSION type, on a bus that
//!   stays a board static and reaches the app as `&'static dyn `[`Bus`].
//! - [`Store`] is the card as the app sees it: bring it up per insertion, hand a
//!   [`BlockDevice`](light_core::hal::BlockDevice) per mounted operation.
//! - The codec is a [`light_audio::Es8311`] over any [`light_core::I2cBus`]; the sample
//!   transport is any [`light_core::AudioStream`] -- the port owns buffers and DMA.
//! - The UI crate's pages macro expands the tree in the board crate, where the event type
//!   is concrete; [`dictaphone_commands!`] splices the app's console rows ahead of the
//!   board's own.

#![no_std]

use core::fmt::Write;
use light_audio::Es8311;
use light_core::cli::{Cli, Outcome, Parsed, Words};
use light_core::hal::BlockDevice;
use light_core::{debug, info, log, warn, AudioStream, Bus, Clock, I2cBus, LineReader, Mailbox, Module, OutputPin, Poll, Subscription};
use light_display::{Display, DisplayDriver, FrameLayer, Region, UpdateError};
use light_draw::Rotation;
use light_font::Font;
use light_fs::{Fat, File as FsFile, FsError};
use light_input::imu::Orientation;
use light_input::touch::Gesture;
use light_ui::{Fonts, IndicatorShape, Lui, LuiChild, Style, SwipeDir, TextSlot, Touch, Ui};

//   what the page-tree macro and the board crates build against, from one place
pub use light_input::cst816t::Event as TouchSample;
pub use light_ui::{file_list, scroll, Axis, Desc, Descent, Page};
pub use light_ui_components::{DirEntry, FilePicker, Order};

/// Widget arena size: the deepest page is the recordings list. Its widest form is the wide
/// layout's -- a window, the pinned back button, the two pinned paging buttons, the scrolling
/// strip, and the strip's eight rows: thirteen, plus a little headroom.
pub const UI_WIDGETS: usize = 15;
/// The backlight scale, `0..=MAX` per-mille.
pub const BACKLIGHT_LEVEL_MAX: u16 = 1000;
/// How many recordings the list page shows: the newest N, one fixed row each -- the page
/// tree is static, its TEXT is not.
pub const LIST_ROWS: usize = 8;

/// The cap on a single take: recording stops itself at this many seconds (five minutes),
/// so a forgotten take cannot fill the card or overflow the WAV's 32-bit sizes.
pub const MAX_REC_SECS: u32 = 5 * 60;

/// Widget tags: how a module finds a widget again to rewrite its text at runtime.
/// The status -- elapsed time, playing/ready -- rides the title bar now, and the
/// recording light is the title bar's own flashing indicator dot, so neither needs a tag.
pub const TAG_REC: u8 = 2;
pub const TAG_PLAY: u8 = 3;
/// The recordings list's paging buttons, shown only when a previous/next page exists.
pub const TAG_PREV: u8 = 4;
pub const TAG_NEXT: u8 = 5;
/// The recordings list's rows carry `TAG_ROW_BASE + row`.
pub const TAG_ROW_BASE: u8 = 0x10;

// --- the event bus ------------------------------------------------------------------------

/// Everything that happens in a dictaphone, as one type: the app's own events, plus the
/// board's extension `X` -- an RTC, a raw card probe, whatever the tangible board carries.
#[derive(Clone, Copy, Debug)]
pub enum Event<X: Copy> {
        Touch(TouchSample),
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
        /// Whether the recordings list has a previous/next page to page to, published after each
        /// scan or page turn; the display shows or hides the paging buttons to match.
        ListNav { prev: bool, next: bool },
        /// The board's own affair; the app carries it and looks away.
        Ext(X),
}

/// What the audio module is doing, in the terms the interface shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioStatus {
        Idle,
        /// A recording in flight, with its elapsed seconds.
        Recording { secs: u16 },
        /// A playback in flight: elapsed and total seconds.
        Playing { secs: u16, total: u16 },
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
        Stats,
        Backlight(u16),
        UiFocus { next: bool },
        UiActivate,
        UiPress { x: u16, y: u16 },
        UiBack,
        RenderMode(RenderMode),
        Tone { hz: u16, ms: u16 },
        ToneOff,
        Volume(u8),
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
pub enum FsOp {
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
pub struct FsPath {
        buf: [u8; 48],
        len: u8,
}

impl FsPath {
        pub fn new(s: &str) -> Option<Self> {
                if s.len() > 48 {
                        return None;
                }
                let mut buf = [0u8; 48];
                buf[..s.len()].copy_from_slice(s.as_bytes());
                Some(Self { buf, len: s.len() as u8 })
        }

        pub fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..usize::from(self.len)]).unwrap_or("")
        }

        pub fn is_empty(&self) -> bool {
                self.len == 0
        }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
        Normal,
        Paused,
        Repush,
}

#[derive(Clone, Copy, Debug)]
pub enum UiAction {
        /// The big button: start a recording, or stop the one in flight.
        RecToggle,
        /// Play the most recent recording, or stop the playback in flight.
        PlayToggle,
        /// The recordings page was opened: scan the card and fill the rows.
        FilesOpen,
        /// A recordings-list row was tapped.
        PlayRow(u8),
        /// The recordings list's "next"/"previous" page buttons.
        FilesNext,
        FilesPrev,
        /// Return to the main page -- the recordings page's back button, on the data-driven
        /// (blob) interface. The const-page interface navigates back structurally instead, so it
        /// never emits this.
        Back,
        DragConsumed,
}

/// The blob-interface event ids: a design's button carries one in its `event` field, and the
/// display module maps it back to a [`UiAction`] with [`ui_action`]. Ids `1..16` are the fixed
/// actions; a recordings-list row `i` carries `ROW_BASE + i`. A JSON design cannot hold comments,
/// so this module is the readable side of that contract -- the numbers in `design.json` mean these.
pub mod ui_event {
        pub const REC_TOGGLE: u16 = 1;
        pub const PLAY_TOGGLE: u16 = 2;
        pub const FILES_OPEN: u16 = 3;
        pub const FILES_NEXT: u16 = 4;
        pub const FILES_PREV: u16 = 5;
        pub const BACK: u16 = 6;
        /// Row `i` carries `ROW_BASE + i` (and, by the same offset, tag `TAG_ROW_BASE + i`).
        pub const ROW_BASE: u16 = 0x10;
}

/// A blob child's `event` id, as the design authored it, to the [`UiAction`] the app runs.
/// Unknown ids (and the reserved 0) map to `None`, so a button with no event emits nothing.
pub fn ui_action(event: u16) -> Option<UiAction> {
        use ui_event::*;
        match event {
                REC_TOGGLE => Some(UiAction::RecToggle),
                PLAY_TOGGLE => Some(UiAction::PlayToggle),
                FILES_OPEN => Some(UiAction::FilesOpen),
                FILES_NEXT => Some(UiAction::FilesNext),
                FILES_PREV => Some(UiAction::FilesPrev),
                BACK => Some(UiAction::Back),
                e if e >= ROW_BASE && (e - ROW_BASE) < LIST_ROWS as u16 => Some(UiAction::PlayRow((e - ROW_BASE) as u8)),
                _ => None,
        }
}

/// The blob interface's page indices, fixed for the dictaphone's two pages -- the `design.json`
/// must order them this way (the main page first). The const-page interface follows parent links
/// and does not use these.
pub const PAGE_MAIN: usize = 0;
pub const PAGE_FILES: usize = 1;

/// Map a blob child to the app event its button emits: the design's `event` id through
/// [`ui_action`]. The child index is unused -- the id, not the position, carries the meaning.
fn map_ui_event<X: Copy>(_: usize, child: &LuiChild<'static>) -> Option<Event<X>> {
        ui_action(child.event).map(Event::Ui)
}

// --- shared state --------------------------------------------------------------------------

static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();
/// The push-versus-touch bisect instruments: whether the panel is mid-push, and whether
/// touch reads should hold while it is.
static PUSHING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static TOUCH_HOLD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// A console byte from the transport the board module owns. Never blocks; a full mailbox
/// drops the byte, and the line it belonged to will fail to parse and say so.
pub fn push_console_byte(b: u8) {
        let _ = CONSOLE_BYTES.push(b);
}

/// Whether a board's touch module should skip this read: `touch hold` is on and the panel
/// is being pushed.
pub fn touch_reads_held() -> bool {
        TOUCH_HOLD.load(core::sync::atomic::Ordering::Relaxed) && PUSHING.load(core::sync::atomic::Ordering::Relaxed)
}

// --- storage -------------------------------------------------------------------------------

/// The card as the app sees it: something that may need bringing up per insertion, and
/// that hands a [`BlockDevice`] per mounted operation. The board owns the physical card
/// and its bus; the app owns the volumes it mounts over it.
pub trait Store {
        //   'static: an open Recording or Playback lives in a .bss slot, so the device
        // it mounts over cannot borrow from the store's stack frame
        type Dev: BlockDevice + 'static;

        /// Ready the card (initialising it on first touch since insertion) and hand a
        /// device for one mounted operation. `None` = no usable card; the implementation
        /// logs the reason, the caller logs the context.
        fn open(&mut self) -> Option<Self::Dev>;

        /// Forget a card that failed mid-operation -- pulled or swapped, typically -- so
        /// the next [`open`](Self::open) starts from power-up.
        fn reset(&mut self);
}

/// A canonical 44-byte PCM WAV header: mono, 16-bit, `sample_hz` -- what makes a
/// recording a file any desktop player opens.
pub fn wav_header(sample_hz: u32, data_len: u32) -> [u8; 44] {
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

/// One `fs` console command against the card: ready it, mount the volume, act, unmount by
/// drop.
fn fs_command<S: Store>(store: &mut S, op: FsOp, path: &str, arg: &str) {
        let Some(dev) = store.open() else {
                info!("fs: no card");
                return;
        };
        let mut fs = match Fat::mount(dev) {
                Ok(fs) => fs,
                Err(e) => {
                        info!("fs: mount failed: {e:?}");
                        //   an I/O failure may be a pulled or swapped card: forget the
                        // init so the next command starts from CMD0
                        if matches!(e, FsError::Io(_)) {
                                store.reset();
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

// --- the display module --------------------------------------------------------------------

/// Where the display module's page tree comes from. A const-fn [`Page`] tree follows the pages'
/// parent links for navigation; an LUI blob (UI as data, authored as `design.json` and compiled by
/// crush) drives navigation from the design's app-event ids, with the page transition preserved by
/// [`Ui::navigate_lui`]. A blob is window-plus-flat-children, so an interface with a nested
/// scrolling strip -- the landscape recordings list -- stays on the const path until the format
/// grows nesting; the upright interface, whose list is a flat scrolling window, is a blob.
#[derive(Clone, Copy)]
pub enum UiSource<X: Copy + 'static> {
        /// A const-`Page` tree, with the axis its generic (`Linear`) windows run along -- the blob
        /// path takes that axis from the design instead (see [`UiSource::Blob`]).
        Pages { root: &'static Page<Event<X>>, axis: Axis },
        /// An LUI design blob. Its `orientation` (authored in the design, [`Lui::landscape`])
        /// decides the layout axis, so the design is the single source and the firmware states no
        /// axis of its own.
        Blob(Lui<'static>),
}

/// What a tangible board tells [`DisplayMod`] about its panel.
pub struct DisplayConfig<X: Copy + 'static> {
        pub width: u16,
        pub height: u16,
        pub fps: u32,
        /// The panel, for the boot log.
        pub desc: &'static str,
        /// The rotation the interface starts in, before any orientation event: the UI
        /// crate's choice -- a portrait layout starts at R0, a landscape one sideways.
        pub initial_rotation: Rotation,
        /// The board's measured orientation-to-rotation table.
        pub rotation_map: fn(Orientation) -> Option<Rotation>,
        /// Where the page tree comes from: a const-`Page` tree or an LUI design blob.
        pub source: UiSource<X>,
        /// The direction child pages open across this interface. `None` keeps the toolkit's
        /// layout-derived flow (a `Row` page rises, everything else slides left); a landscape
        /// interface points it one way for the whole tree. It is expressed logically, so it
        /// stays correct through the display rotation the orientation map applies.
        pub default_descent: Option<Descent>,
}

/// Owns the panel and the widget tree: renders when something is dirty, routes touches
/// and commands into the toolkit, follows the audio module's status into the interface's
/// labels, and keeps the draw/push timing `stats` reports.
pub struct DisplayMod<D: DisplayDriver, C: Clock, X: Copy + 'static> {
        display: Display<'static, D>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<Event<X>, UI_WIDGETS>,
        clock: C,
        cfg: DisplayConfig<X>,
        bus: &'static dyn Bus<Event<X>>,
        sub: Subscription,
        mode: RenderMode,
        /// The blob interface's current page index (see [`PAGE_MAIN`]/[`PAGE_FILES`]); the display
        /// drives navigation itself on that path. Unused on the const-`Page` path.
        page: usize,
        /// What the title bar shows, and what gates the recording light.
        status: AudioStatus,
        /// The recording light's current phase, and when it last toggled: the flash runs
        /// off the display poll at 2 Hz, faster than the once-a-second status events.
        blink_on: bool,
        blink_last_us: u64,
        drag_reported: bool,
        draw_us_max: u64,
        push_us_max: u64,
        push_started_us: Option<u64>,
}

impl<D: DisplayDriver, C: Clock, X: Copy + core::fmt::Debug + 'static> DisplayMod<D, C, X> {
        pub fn new(
                display: Display<'static, D>,
                layer: &'static mut FrameLayer,
                font: Font<'static>,
                ui: &'static mut Ui<Event<X>, UI_WIDGETS>,
                clock: C,
                bus: &'static dyn Bus<Event<X>>,
                cfg: DisplayConfig<X>,
        ) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { display, layer, font, ui, clock, cfg, bus, sub, mode: RenderMode::Normal, page: PAGE_MAIN, status: AudioStatus::Idle, blink_on: false, blink_last_us: 0, drag_reported: false, draw_us_max: 0, push_us_max: 0, push_started_us: None }
        }

        fn publish(bus: &dyn Bus<Event<X>>, ev: Option<Event<X>>) {
                if let Some(ev) = ev {
                        if let Err(e) = bus.publish(ev) {
                                warn!("event bus full; dropped {e:?}");
                        }
                }
        }

        fn handle(&mut self, ev: Event<X>) {
                match ev {
                        Event::Touch(t) => {
                                if self.mode == RenderMode::Repush {
                                        if let TouchSample::Down { .. } = t {
                                                if !self.display.busy() {
                                                        let _ = self.display.update_async(Region::full(self.cfg.width, self.cfg.height));
                                                }
                                        }
                                }
                                let now = log::now_us();
                                let outcome = match t {
                                        TouchSample::Down { x, y } | TouchSample::Move { x, y } => self.ui.touch(x, y, true, now),
                                        TouchSample::Up => self.ui.touch(0, 0, false, now),
                                        TouchSample::Reset => return,
                                };
                                match outcome {
                                        Touch::Drag if !self.drag_reported => {
                                                self.drag_reported = true;
                                                Self::publish(self.bus, Some(Event::Ui(UiAction::DragConsumed)));
                                        }
                                        Touch::Tap { hit, emitted } => {
                                                debug!("tap: {}", if hit { "hit" } else { "no widget there" });
                                                //   run the whole press-flash BLIP synchronously here, before
                                                // the emitted action runs: paint and push the flash, hold it
                                                // for its short window, then paint and push the reverted
                                                // button. A record/play handler blocks core 0 on card I/O far
                                                // longer than the flash should last, so the loop cannot end
                                                // the flash on its own -- it would linger for the whole delay
                                                if emitted.is_some() {
                                                        self.flush_display();
                                                        self.render();
                                                        self.flush_display();
                                                        let until = now + light_ui::ACTIVATE_FLASH_US;
                                                        while log::now_us() < until {}
                                                        // a render past the deadline lapses the flash; push that frame too
                                                        self.render();
                                                        self.flush_display();
                                                }
                                                Self::publish(self.bus, emitted);
                                                //   navigation is the display's own on the blob path:
                                                // the emitted event still rides the bus (the recorder
                                                // scans on FilesOpen), but the page move happens here,
                                                // in step with the tap, not through a bus round-trip.
                                                // A no-op on the const-page path (that navigates
                                                // structurally from the button itself)
                                                if let Some(Event::Ui(action)) = emitted {
                                                        self.nav_for(action);
                                                }
                                        }
                                        Touch::DragEnd | Touch::None => self.drag_reported = false,
                                        _ => {}
                                }
                        }
                        Event::Gesture(g) => {
                                if self.ui.swipe_direction(g.start, g.end) == Some(SwipeDir::Right) && self.back() {
                                        debug!("swipe: returned to the previous page");
                                }
                        }
                        Event::Orientation(o) => {
                                if let Some(r) = (self.cfg.rotation_map)(o) {
                                        self.ui.set_rotation(self.layer, r);
                                        let (w, h) = self.ui.logical_size();
                                        info!("orientation {o:?}: canvas now {w}x{h}");
                                }
                        }
                        Event::Command(Command::UiFocus { next }) => {
                                if next {
                                        self.ui.focus_next()
                                } else {
                                        self.ui.focus_prev()
                                }
                        }
                        Event::Command(Command::UiActivate) => {
                                let emitted = self.ui.activate();
                                Self::publish(self.bus, emitted);
                                if let Some(Event::Ui(action)) = emitted {
                                        self.nav_for(action);
                                }
                        }
                        Event::Command(Command::UiPress { x, y }) => {
                                let (hit, emitted) = self.ui.press_at(x, y);
                                info!("ui press {x} {y}: {}", if hit { "hit" } else { "no widget there" });
                                Self::publish(self.bus, emitted);
                                if let Some(Event::Ui(action)) = emitted {
                                        self.nav_for(action);
                                }
                        }
                        Event::Command(Command::RenderMode(m)) => {
                                //   coming back to Normal, the glass is untrusted: a pause
                                // may have left stale content standing, so resume repaints
                                // everything rather than waiting for the next interaction
                                if m == RenderMode::Normal && self.mode != RenderMode::Normal {
                                        self.ui.invalidate_all();
                                }
                                self.mode = m;
                                info!("render mode {m:?}");
                        }
                        Event::Command(Command::UiBack) => {
                                if !self.back() {
                                        info!("ui back: nowhere to go from this page");
                                }
                        }
                        Event::Status(s) => {
                                self.status = s;
                                self.render_status();
                                self.set_transport_labels();
                        }
                        Event::RowText { row, name } => {
                                //   an unused row (an empty name, e.g. the tail of the last page) is
                                // hidden, not left as a blank cell; the ListNav that follows the row
                                // batch relays the list so the freed space closes up
                                self.ui.set_list_row(TAG_ROW_BASE, row, name.as_str());
                        }
                        Event::ListNav { prev, next } => {
                                //   the paging buttons appear only when there is a page that way;
                                // find() is None when the files page is not built -- the right
                                // no-op. Then relay: this event ends a list update (the row batch
                                // before it), so the hidden rows and paging buttons close up
                                if let Some(id) = self.ui.find(TAG_PREV) {
                                        self.ui.set_visible(id, prev);
                                }
                                if let Some(id) = self.ui.find(TAG_NEXT) {
                                        self.ui.set_visible(id, next);
                                }
                                self.ui.relayout();
                        }
                        Event::Command(Command::Stats) => {
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

        /// The record/play button labels, following the audio module's state. `find` is `None`
        /// for a tag on a page that is not built (the recordings page has neither button), which
        /// is exactly the right no-op. Called on every status change and after a page is (re)built,
        /// so a page entered mid-recording shows `# Stop`, not the design's static `* Record`.
        fn set_transport_labels(&mut self) {
                let (rec_label, play_label) = match self.status {
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

        /// Move to a page of the blob interface with the transition ([`Ui::navigate_lui`]) and
        /// re-apply the live transport state onto it (a `# Stop` mid-recording, the title-bar time
        /// and indicator), which the design's static labels do not carry. A no-op off the blob path.
        fn show_lui_page(&mut self, page: usize, back: bool) {
                let UiSource::Blob(lui) = self.cfg.source else { return };
                let Some(p) = lui.page(page) else {
                        warn!("dictaphone: the design has no page {page}");
                        return;
                };
                if let Err(e) = self.ui.navigate_lui(&p, back, map_ui_event::<X>) {
                        warn!("dictaphone: design page {page} did not build: {e:?}");
                        return;
                }
                self.page = page;
                self.set_transport_labels();
                self.render_status();
        }

        /// Navigate for a tapped action on the blob path: open the recordings list, or return from
        /// it. The const-page path navigates structurally from the button itself, so this no-ops
        /// there (its buttons carry `Nav::To`/`Nav::Back`, and swipe/`UiBack` reach [`Self::back`]).
        fn nav_for(&mut self, action: UiAction) {
                if !matches!(self.cfg.source, UiSource::Blob(_)) {
                        return;
                }
                match action {
                        UiAction::FilesOpen => self.show_lui_page(PAGE_FILES, false),
                        UiAction::Back => {
                                if self.page != PAGE_MAIN {
                                        self.show_lui_page(PAGE_MAIN, true);
                                }
                        }
                        _ => {}
                }
        }

        /// Go back one page: follow the parent link on the const path, or return to the main page on
        /// the blob path. `false`, changing nothing, when there is nowhere to go -- so a swipe or
        /// `UiBack` at the top means nothing there, as it did before.
        fn back(&mut self) -> bool {
                match self.cfg.source {
                        UiSource::Pages { .. } => self.ui.navigate_back(),
                        UiSource::Blob(_) => {
                                if self.page == PAGE_MAIN {
                                        return false;
                                }
                                self.show_lui_page(PAGE_MAIN, true);
                                true
                        }
                }
        }

        /// The status, in the title bar: `"<page> - REC 0:12"` and the like, composed onto
        /// the current page's own name so it reads right whichever page is showing. The body
        /// carries only the flashing recording light now (see [`Self::blink_tick`]).
        fn render_status(&mut self) {
                let Some(root) = self.ui.root() else { return };
                //   the transport state shows as a drawn symbol -- a flashing dot recording, a
                // steady triangle playing -- with the time beside it. A two-row title bar (the
                // narrow portrait one) puts the name on row 1 with the symbol and the time on
                // row 2; a one-row bar (the wide one, with room) keeps it all inline.
                let (base, two_row) = self
                        .ui
                        .get(root)
                        .and_then(|w| w.window())
                        .map(|win| (win.title.unwrap_or(""), win.subtitle.is_some()))
                        .unwrap_or(("", false));
                let col = self.symbol_col(base);
                let mut detail = StackString::<{ TextSlot::CAP }>::new();
                let indicator = match self.status {
                        AudioStatus::Idle => None,
                        AudioStatus::Recording { secs } => {
                                let _ = write!(detail, "{}:{:02}", secs / 60, secs % 60);
                                Some((IndicatorShape::Dot, self.blink_on, col))
                        }
                        AudioStatus::Playing { secs, total } => {
                                let _ = write!(detail, "{}:{:02} / {}:{:02}", secs / 60, secs % 60, total / 60, total % 60);
                                Some((IndicatorShape::Play, true, col))
                        }
                };
                if two_row {
                        // row 1 stays the static name; the time rides row 2 (blank when idle)
                        self.ui.set_subtitle(root, detail.as_str());
                } else {
                        // name, a blank cell for the symbol, then the inline time
                        let mut line = StackString::<{ TextSlot::CAP }>::new();
                        if detail.as_str().is_empty() {
                                let _ = write!(line, "{base}");
                        } else {
                                let _ = write!(line, "{base}   {}", detail.as_str());
                        }
                        self.ui.set_text(root, line.as_str());
                }
                self.ui.set_indicator(root, indicator);
                //   leaving the recording state resets the flash phase, so the next take
                // starts lit on its first tick
                if !matches!(self.status, AudioStatus::Recording { .. }) {
                        self.blink_on = false;
                        self.blink_last_us = 0;
                }
        }

        /// The title cell the status symbol sits on: one blank past the page's (static) name,
        /// so it tracks whichever page is showing and never overlaps the title.
        fn symbol_col(&self, base: &str) -> u16 {
                (base.chars().count() + 1) as u16
        }

        /// One step of the recording light's 2 Hz flash, driven from the display poll so it
        /// runs between the once-a-second status events. A no-op unless recording; toggles the
        /// inline title-bar dot between lit and dark at a fixed cell, so the title never
        /// reflows. Only the title bar's rect repaints.
        fn blink_tick(&mut self, now: u64) {
                if !matches!(self.status, AudioStatus::Recording { .. }) {
                        return;
                }
                if self.blink_last_us != 0 && now.saturating_sub(self.blink_last_us) < 500_000 {
                        return;
                }
                self.blink_last_us = now;
                self.blink_on = !self.blink_on;
                if let Some(root) = self.ui.root() {
                        let base = self.ui.get(root).and_then(|w| w.window()).and_then(|win| win.title).unwrap_or("");
                        let col = self.symbol_col(base);
                        self.ui.set_indicator(root, Some((IndicatorShape::Dot, self.blink_on, col)));
                }
        }

        fn render(&mut self) {
                if self.mode != RenderMode::Normal || (!self.ui.is_dirty() && !self.ui.is_animating()) {
                        return;
                }
                let now = log::now_us();
                //   one face for every role today; the theme is the UI's own, so the metrics it
                // laid out with and these glyphs are the one style
                let style = Style::new(*self.ui.theme(), Fonts::uniform(&self.font));
                let drew = self.ui.render(self.layer, &mut self.display, &style, now);
                let done = log::now_us();
                if drew || self.ui.is_animating() {
                        self.draw_us_max = self.draw_us_max.max(done - now);
                        self.push_started_us = Some(done);
                }
        }

        /// Drive an in-flight chunked push to the panel to completion, right now. The frame
        /// otherwise finishes over later polls; this is for the one case that cannot wait --
        /// painting a tap's acknowledgement before a blocking handler (a card open, a take
        /// starting) runs and stops the loop from polling. Bounded so a wedged transfer can
        /// never hang the app.
        fn flush_display(&mut self) {
                let mut spins = 0u32;
                while self.layer.busy(&self.display) {
                        if self.layer.poll(&mut self.display).is_err() {
                                break;
                        }
                        spins += 1;
                        if spins > 100_000 {
                                warn!("display flush did not settle");
                                break;
                        }
                }
        }
}

impl<D: DisplayDriver, C: Clock, X: Copy + core::fmt::Debug + 'static> Module for DisplayMod<D, C, X> {
        fn name(&self) -> &'static str {
                "display"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.display.init(&mut self.clock);
                self.layer.set_frame_rate(self.cfg.fps);
                self.layer.bg = self.ui.theme().bg;
                self.ui.fit(self.layer);
                if self.cfg.initial_rotation != Rotation::R0 {
                        self.ui.set_rotation(self.layer, self.cfg.initial_rotation);
                }
                self.ui.set_default_descent(self.cfg.default_descent);
                //   the layout axis: stated by a const-Page interface, but taken from the DESIGN for
                // a blob (its orientation), so the design is the single source and the editor and
                // firmware cannot disagree about which way it lays out
                let axis = match self.cfg.source {
                        UiSource::Pages { axis, .. } => axis,
                        UiSource::Blob(lui) => {
                                if lui.landscape() {
                                        Axis::Horizontal
                                } else {
                                        Axis::Vertical
                                }
                        }
                };
                self.ui.set_layout_axis(axis);
                //   the root is empty, so this first build snaps (navigate/navigate_lui start no
                // transition until there is an outgoing page to slide off)
                match self.cfg.source {
                        UiSource::Pages { root, .. } => {
                                if let Err(e) = self.ui.navigate(root) {
                                        warn!("the main page did not build: {e:?}");
                                }
                        }
                        UiSource::Blob(_) => self.show_lui_page(PAGE_MAIN, false),
                }
                self.ui.invalidate_all();
                self.render();
                info!(
                        "display up: {}x{} ({}) at {} fps, font {}px ({} glyphs)",
                        self.cfg.width,
                        self.cfg.height,
                        self.cfg.desc,
                        self.cfg.fps,
                        self.font.pixel_size(),
                        self.font.glyph_count()
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
                                self.push_us_max = self.push_us_max.max(log::now_us() - started);
                                self.push_started_us = None;
                        }
                }
                while let Some(ev) = self.bus.poll(&self.sub) {
                        self.handle(ev);
                }
                self.blink_tick(log::now_us());
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

// --- the audio module ----------------------------------------------------------------------

/// One period of sine at 20000 amplitude, 32 steps -- plenty for a test tone.
static SINE: [i16; 32] = [
        0, 3902, 7654, 11111, 14142, 16629, 18478, 19616, 20000, 19616, 18478, 16629, 14142, 11111, 7654, 3902, 0, -3902, -7654, -11111, -14142, -16629, -18478, -19616, -20000, -19616, -18478, -16629,
        -14142, -11111, -7654, -3902,
];

/// `REC_NNNN.WAV` -> `NNNN`; anything else is not one of ours.
fn rec_number(name: &str) -> Option<u32> {
        let digits = name.strip_prefix("REC_")?.strip_suffix(".WAV")?;
        if digits.len() != 4 {
                return None;
        }
        digits.parse().ok()
}

/// The [`FilePicker`] filter for the recordings list: our `REC_NNNN.WAV` takes, and only ones
/// with PCM past the 44-byte header -- a header-only take has nothing to play, so it earns no
/// row. The board hands this to the picker at construction; the picker never learns what a
/// recording is.
pub fn keep_recording(entry: &DirEntry) -> bool {
        !entry.is_dir && entry.size > 44 && rec_number(entry.name()).is_some()
}

/// One open recording: the volume stays mounted and every filled capture buffer appends
/// to the file, write-through, until stop patches the WAV header.
pub struct Recording<Dev: BlockDevice> {
        fs: Fat<Dev>,
        file: FsFile,
}

/// One open playback: the file positioned at its PCM data, streamed into the DAC's
/// refill until the data chunk ends.
pub struct Playback<Dev: BlockDevice> {
        fs: Fat<Dev>,
        file: FsFile,
        channels: u8,
        /// Where the data chunk stops -- the file may carry trailing chunks.
        data_end: u32,
        /// Where the PCM begins -- with `data_end`, what elapsed/total time is read from.
        data_start: u32,
}

/// The codec plus the sample transport plus the card: recording, playback, the test tone,
/// and every audio diagnostic. The transport streams continuously (silence when nothing
/// plays), so a long frame draw cannot starve the codec into audible chop.
pub struct AudioMod<S: Store, B: I2cBus, A: AudioStream, P: OutputPin, C: Clock, X: Copy + 'static> {
        codec: Es8311<B>,
        stream: A,
        pa: P,
        clock: C,
        sample_hz: u32,
        bus: &'static dyn Bus<Event<X>>,
        sub: Subscription,
        /// Phase accumulator into [`SINE`]; the top 5 bits index the table.
        phase: u32,
        phase_inc: u32,
        /// Sample frames left to play; 0 is silence.
        remaining: u32,
        /// The card, shared with whatever board module also serves it.
        store: S,
        /// A recording in flight. A `&'static mut` INTO .bss, never inline: the module
        /// lives on core 0's small stack, and a ~1.2 KB Recording inline once overflowed
        /// it straight into core 1's.
        rec: &'static mut Option<Recording<S::Dev>>,
        /// A playback in flight; same .bss discipline.
        play: &'static mut Option<Playback<S::Dev>>,
        /// A small scratch for frame-aligned card reads on the playback path: one push reads
        /// up to this many bytes, converts them to output words, and the loop repeats to top
        /// the transport's prefetch ring up. The DEPTH that rides out card stalls lives in
        /// that ring (in the port, drained by the DMA IRQ); this is just the read buffer.
        play_stage: &'static mut [u8],
        /// The `rec null` bisect: capture runs, everything drains to nowhere.
        rec_null: bool,
        null_bytes: u32,
        /// The most recent recording -- what "Play last" plays.
        last_name: Option<FsPath>,
        /// The recordings list, newest first: the system file-picker component, scanning the
        /// card and driving the list rows. In .bss like everything sizeable here.
        picker: &'static mut FilePicker<LIST_ROWS>,
        /// Publish-on-change: the last [`AudioStatus`] the interface was told about. The
        /// elapsed second is part of the value, so a live take updates itself.
        last_status: AudioStatus,
        /// The mic path's gain pair (SYSTEM14: mic select + analog PGA; ADC17: digital
        /// volume), applied at every record start -- `micgain` retunes it live.
        mic14: u8,
        mic17: u8,
        /// The worst gap between this module's polls while audio was in flight -- the
        /// number the stream buffer's duration has to beat. `stats` prints and resets it.
        poll_us_last: u64,
        poll_gap_max_us: u32,
}

/// What a board hands [`AudioMod::new`] besides the hardware: the .bss slots for the big
/// state, allocated where the board's memory map wants them.
pub struct AudioSlots<Dev: BlockDevice + 'static> {
        pub rec: &'static mut Option<Recording<Dev>>,
        pub play: &'static mut Option<Playback<Dev>>,
        pub play_stage: &'static mut [u8],
        pub picker: &'static mut FilePicker<LIST_ROWS>,
}

impl<S: Store, B: I2cBus, A: AudioStream, P: OutputPin, C: Clock, X: Copy + core::fmt::Debug + 'static> AudioMod<S, B, A, P, C, X> {
        /// `mic_gain` is the board's tuned (SYSTEM14, ADC17) register pair.
        pub fn new(codec: Es8311<B>, stream: A, pa: P, clock: C, store: S, sample_hz: u32, mic_gain: (u8, u8), slots: AudioSlots<S::Dev>, bus: &'static dyn Bus<Event<X>>) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self {
                        codec,
                        stream,
                        pa,
                        clock,
                        sample_hz,
                        bus,
                        sub,
                        phase: 0,
                        phase_inc: 0,
                        remaining: 0,
                        store,
                        rec: slots.rec,
                        play: slots.play,
                        play_stage: slots.play_stage,
                        rec_null: false,
                        null_bytes: 0,
                        last_name: None,
                        picker: slots.picker,
                        last_status: AudioStatus::Idle,
                        mic14: mic_gain.0,
                        mic17: mic_gain.1,
                        poll_us_last: 0,
                        poll_gap_max_us: 0,
                }
        }

        /// What the interface should say right now, read straight from the state.
        fn status(&self) -> AudioStatus {
                let rate = self.sample_hz * 2;
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

        /// Mount the card for a UI operation, or say why not.
        fn mount(&mut self) -> Option<Fat<S::Dev>> {
                let dev = self.store.open()?;
                match Fat::mount(dev) {
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

        /// Open the list page: scan the card fresh (first page) and publish it. The scan, the
        /// newest-first ordering, and the [`keep_recording`] filter are all the system
        /// [`FilePicker`]; this keeps the RowText/ListNav bridge to the display module.
        fn scan_files(&mut self) {
                match self.mount() {
                        Some(mut fs) => {
                                let _ = self.picker.scan(&mut fs, "/");
                        }
                        None => self.picker.clear(),
                }
                self.publish_list();
                if self.last_name.is_none() {
                        self.last_name = self.picker.name(0).and_then(FsPath::new);
                }
        }

        /// Page the list one step forward (`forward`) or back, then publish the new page. The
        /// [`FilePicker`] re-lists the card and keeps only the page's window, so every recording
        /// is reachable however many there are.
        fn page_files(&mut self, forward: bool) {
                if let Some(mut fs) = self.mount() {
                        let _ = if forward { self.picker.next_page(&mut fs, "/") } else { self.picker.prev_page(&mut fs, "/") };
                }
                self.publish_list();
        }

        /// Publish the current page: a RowText per row (an empty name is an unused row), and the
        /// ListNav telling the display which paging buttons to show.
        fn publish_list(&mut self) {
                let empty = FsPath::new("").unwrap_or(FsPath { buf: [0; 48], len: 0 });
                for i in 0..LIST_ROWS {
                        let name = self.picker.name(i).and_then(FsPath::new).unwrap_or(empty);
                        let _ = self.bus.publish(Event::RowText { row: i as u8, name });
                }
                let _ = self.bus.publish(Event::ListNav { prev: self.picker.has_prev(), next: self.picker.has_next() });
        }

        fn rec_start(&mut self, path: &str) {
                if self.rec.is_some() || self.rec_null {
                        info!("rec: already recording");
                        return;
                }
                if path == "null" {
                        //   the freeze bisect: the WHOLE capture pipeline -- mic, transport,
                        // buffer hand-off -- with the card and filesystem entirely out of
                        // the path. Stable here + frozen with a file convicts the card leg
                        for _ in 0..3 {
                                if self.codec.mic_enable().is_ok() {
                                        break;
                                }
                        }
                        self.pa.set(false);
                        self.stream.capture_start();
                        self.rec_null = true;
                        self.null_bytes = 0;
                        info!("rec: null sink -- capturing and discarding");
                        return;
                }
                //   speaker amp OFF *before* the card setup, not after: opening, mounting
                // and writing the header block core 0 for the best part of a second, and the
                // idle DAC ring underruns and restarts through those stalls -- a click each
                // time, audible through a live speaker in the gap before capture begins. Any
                // failure path below turns it back on, since no take will follow to do so.
                self.pa.set(false);
                let Some(dev) = self.store.open() else {
                        info!("rec: no card");
                        self.pa.set(true);
                        return;
                };
                info!("rec: card up");
                let mut fs = match Fat::mount(dev) {
                        Ok(fs) => fs,
                        Err(e) => {
                                info!("rec: mount failed: {e:?}");
                                self.pa.set(true);
                                return;
                        }
                };
                info!("rec: mounted");
                let mut file = match fs.create(path) {
                        Ok(f) => f,
                        Err(e) => {
                                info!("rec: create failed: {e:?}");
                                self.pa.set(true);
                                return;
                        }
                };
                info!("rec: created");
                if let Err(e) = file.write(&mut fs, &wav_header(self.sample_hz, 0)) {
                        info!("rec: header write failed: {e:?}");
                        self.pa.set(true);
                        return;
                }
                info!("rec: header written");
                //   retried: the first attempt right after the SD burst has been seen to
                // time out where a later one succeeds
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
                //   the amp is already off (muted before the card setup above); it stays off
                // for the take, since live it clicks with every SD write burst and the
                // microphone records its own speaker
                self.stream.capture_start();
                info!("rec: capture started, speaker muted");
                *self.rec = Some(Recording { fs, file });
                self.last_name = FsPath::new(path);
                info!("rec: recording {} -- mono 16-bit at {} Hz", path, self.sample_hz);
        }

        fn rec_stop(&mut self) {
                self.stream.capture_stop();
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
                let data = rec.file.size().saturating_sub(44);
                if data == 0 {
                        //   nothing captured -- a record/stop faster than one buffer fills.
                        // Discard the take rather than leave a header-only WAV that would list
                        // as an unplayable row (the file handle is a plain index, nothing to
                        // close). Clearing last_name lets the next "play last"/files scan
                        // repopulate it from the real recordings.
                        match self.last_name {
                                Some(p) => match rec.fs.remove(p.as_str()) {
                                        Ok(()) => info!("rec: discarded empty take {}", p.as_str()),
                                        Err(e) => warn!("rec: could not remove empty take {}: {e:?}", p.as_str()),
                                },
                                None => warn!("rec: empty take with no name to remove"),
                        }
                        self.last_name = None;
                        return;
                }
                //   the header written blind at start now learns the real length
                let patch = rec.file.seek(&mut rec.fs, 0).and_then(|()| rec.file.write(&mut rec.fs, &wav_header(self.sample_hz, data)).map(|_| ()));
                match patch {
                        Ok(()) => info!("rec: stopped -- {} B of audio, ~{} ms", data, (u64::from(data) / 2 * 1000 / u64::from(self.sample_hz)) as u32),
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
                let Some(dev) = self.store.open() else {
                        info!("play: no card");
                        return;
                };
                let mut fs = match Fat::mount(dev) {
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
                                if !pcm16 || rate != self.sample_hz || !(1..=2).contains(&channels) {
                                        info!("play: unsupported format ({channels} ch, {rate} Hz) -- this codec run takes 16-bit PCM at {} Hz", self.sample_hz);
                                        return;
                                }
                                let data_start = file.pos();
                                let data_end = data_start.saturating_add(len).min(file.size());
                                //   u64: the frame count times 1000 overflows u32 past ~4.5
                                // minutes of audio, and a long take (or a stray big WAV on
                                // the card) is a duration to print, not a panic
                                let ms = ((u64::from(data_end - data_start) / (u64::from(channels) * 2)) * 1000 / u64::from(self.sample_hz)) as u32;
                                info!("play: {} -- {} ch, ~{} ms", path, channels, ms);
                                //   drop any tail a previous play or stop left queued, so this
                                // file starts clean rather than after stale audio
                                self.stream.stream_clear();
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
                        //   drop the buffered lead so the codec falls silent now, not after the
                        // ring plays out what was queued ahead
                        self.stream.stream_clear();
                        self.stream.set_active(false);
                        info!("play: stopped");
                } else {
                        info!("play: idle");
                }
        }
}

impl<S: Store, B: I2cBus, A: AudioStream, P: OutputPin, C: Clock, X: Copy + core::fmt::Debug + 'static> Module for AudioMod<S, B, A, P, C, X> {
        fn name(&self) -> &'static str {
                "audio"
        }
        fn load(&mut self) -> Result<(), ()> {
                match self.codec.probe() {
                        Ok(Some(id)) => info!("es8311 chip id confirmed: 0x{id:04x}"),
                        Ok(None) => warn!("es8311 answered with an unexpected chip id"),
                        Err(e) => warn!("es8311 did not answer the chip id read: {e:?}"),
                }
                if let Err(e) = self.codec.init(self.sample_hz, &mut self.clock) {
                        warn!("es8311 init failed: {e:?}");
                        return Ok(());
                }
                //   tuned with the mic gain: 85 audibly overdrove the little speaker on
                // healthy-level takes, 70 was near-inaudible (the register is ~0.5 dB per
                // step of this 0..100 scale -- the knob is steep)
                let _ = self.codec.set_volume(78);
                //   belt and suspenders against the ADC->DAC monitor's feedback loop: the
                // codec reset in init() already clears REG44, but assert it off explicitly
                // so no prior micmon state can ever survive into a running speaker
                let _ = self.codec.set_adc_to_dac(false);
                self.stream.start();
                self.pa.set(true);
                info!("audio up: es8311 master at {} Hz", self.sample_hz);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                //   the worst poll-to-poll gap while audio is in flight: measured, not
                // guessed, because the stream buffer's duration is a budget this gap spends
                let now = log::now_us();
                if self.rec.is_some() || self.play.is_some() || self.rec_null || self.remaining > 0 {
                        if self.poll_us_last != 0 {
                                self.poll_gap_max_us = self.poll_gap_max_us.max((now - self.poll_us_last) as u32);
                        }
                        self.poll_us_last = now;
                } else {
                        self.poll_us_last = 0;
                }
                while let Some(ev) = self.bus.poll(&self.sub) {
                        match ev {
                                Event::Command(Command::Tone { hz, ms }) => {
                                        self.phase_inc = ((u64::from(hz) << 32) / u64::from(self.sample_hz)) as u32;
                                        self.remaining = u32::from(ms) * self.sample_hz / 1000;
                                        info!("tone {hz} Hz for {ms} ms");
                                }
                                Event::Command(Command::ToneOff) => {
                                        self.remaining = 0;
                                        info!("tone off");
                                }
                                Event::Command(Command::Volume(v)) => match self.codec.set_volume(v) {
                                        Ok(()) => info!("volume {v}"),
                                        Err(e) => warn!("volume set failed: {e:?}"),
                                },
                                Event::Command(Command::RecStart(path)) => {
                                        let path = path;
                                        self.rec_start(path.as_str());
                                }
                                Event::Command(Command::RecStop) => self.rec_stop(),
                                Event::Command(Command::MicGain { r14, r17 }) => {
                                        self.mic14 = r14;
                                        self.mic17 = r17;
                                        //   applied immediately too, so a live take retunes
                                        // mid-recording and the next one starts here
                                        match self.codec.mic_config(r14, r17) {
                                                Ok(()) => info!("micgain: REG14 0x{r14:02x}, REG17 0x{r17:02x}"),
                                                Err(e) => warn!("micgain: applied at next rec ({e:?})"),
                                        }
                                }
                                Event::Ui(UiAction::RecToggle) => {
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
                                Event::Ui(UiAction::PlayToggle) => {
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
                                Event::Ui(UiAction::FilesOpen) => self.scan_files(),
                                Event::Ui(UiAction::FilesNext) => self.page_files(true),
                                Event::Ui(UiAction::FilesPrev) => self.page_files(false),
                                Event::Ui(UiAction::PlayRow(i)) => {
                                        if let Some(p) = self.picker.name(usize::from(i)).and_then(FsPath::new) {
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
                                Event::Command(Command::Fs { op, path, arg }) => {
                                        fs_command(&mut self.store, op, path.as_str(), arg.as_str());
                                }
                                Event::Command(Command::RecStatus) => {
                                        match self.rec.as_ref() {
                                                Some(r) => info!("rec: recording, {} B so far, {} overruns", r.file.size(), self.stream.cap_overruns()),
                                                None => info!("rec: idle"),
                                        };
                                }
                                Event::Command(Command::PlayStart(path)) => {
                                        let path = path;
                                        self.play_start(path.as_str());
                                }
                                Event::Command(Command::Synth) => {
                                        //   write a clean 2 s 440 Hz sine to SINE.WAV via the
                                        // ordinary fs path, so `play SINE.WAV` exercises the
                                        // file-playback chain with a KNOWN-good signal --
                                        // splitting "playback broken" from "recording bad"
                                        match self.mount() {
                                                Some(mut fs) => {
                                                        let _ = fs.remove("SINE.WAV");
                                                        match fs.create("SINE.WAV") {
                                                                Ok(mut f) => {
                                                                        let samples = self.sample_hz * 2; // 2 s
                                                                        let _ = f.write(&mut fs, &wav_header(self.sample_hz, samples * 2));
                                                                        let inc = ((440u64 << 32) / u64::from(self.sample_hz)) as u32;
                                                                        let mut phase = 0u32;
                                                                        //   960-byte chunks (480 samples) off the stack -- small
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
                                                None => info!("synth: no card"),
                                        }
                                }
                                Event::Command(Command::MicMon(on)) => {
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
                                Event::Command(Command::MicDbg) => {
                                        //   enable the mic, then let the port probe its own
                                        // capture path (speaker muted, NO loopback -- cannot
                                        // feed back). Splits "mic line dead" from "transport
                                        // misreads a toggling line"
                                        self.pa.set(false);
                                        let _ = self.codec.set_adc_to_dac(false);
                                        for _ in 0..3 {
                                                if self.codec.mic_enable().is_ok() {
                                                        break;
                                                }
                                        }
                                        self.stream.debug_probe();
                                }
                                Event::Command(Command::PlayStop) => self.play_stop(),
                                Event::Command(Command::PlayStatus) => {
                                        match self.play.as_ref() {
                                                Some(p) => info!("play: at {} of {} B", p.file.pos(), p.data_end),
                                                None => info!("play: idle"),
                                        };
                                }
                                Event::Command(Command::Stats) => {
                                        //   reset on read, so each reading covers the interval
                                        // since the last -- a cumulative count here spent a
                                        // debugging session being misread as per-playback
                                        info!("audio: {} stream underruns, {} capture overruns; max poll gap {} us", self.stream.underruns(), self.stream.cap_overruns(), self.poll_gap_max_us);
                                        self.stream.reset_stats();
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
                        self.stream.capture_take(&mut |buf| {
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
                        self.stream.capture_take(&mut |buf| *n += (buf.len() * 2) as u32);
                }
                if failed {
                        self.rec_stop();
                }
                //   the length cap: a take stops itself at MAX_REC_SECS. Checked here, right
                // after the drain, so the file size the elapsed seconds are read from already
                // includes this poll's captured bytes. rec_stop patches the header and clears
                // the take, and the publish-on-change at the end of poll flips the UI to idle
                if let Some(rec) = self.rec.as_ref() {
                        let secs = rec.file.size().saturating_sub(44) / (self.sample_hz * 2);
                        if secs >= MAX_REC_SECS {
                                info!("rec: reached the {}s cap -- stopping", MAX_REC_SECS);
                                self.rec_stop();
                        }
                }
                let playing = self.remaining > 0 || self.rec.is_some() || self.rec_null || self.play.is_some();
                if self.play.is_some() {
                        //   a file plays: top the IRQ-drained prefetch ring up toward full off
                        // the card. The IRQ copies the ring into the DMA buffers on its own, so
                        // a blocked poll (a slow card read right here) spends the ring's lead,
                        // not the codec's deadline. Card bytes convert to one output WORD per
                        // frame, the 16-bit sample in both slots; `channels * 2` bytes advance a
                        // frame (stereo's left channel is what plays).
                        self.stream.set_active(true);
                        let stream = &mut self.stream;
                        let play = self.play.as_mut().expect("play present");
                        let scratch = &mut *self.play_stage;
                        let fs = &mut play.fs;
                        let file = &mut play.file;
                        let data_end = play.data_end;
                        let stride = usize::from(play.channels) * 2;
                        let mut failed = false;
                        while stream.stream_free() > 0 {
                                let mut produced = 0usize;
                                stream.stream_push(&mut |dst| {
                                        //   frame-aligned card read into the scratch, then convert;
                                        // the scratch bounds one push, the outer loop tops up the rest
                                        let want_words = dst.len().min(scratch.len() / stride);
                                        let remaining = data_end.saturating_sub(file.pos()) as usize;
                                        let want_bytes = (want_words * stride).min(remaining - remaining % stride);
                                        if want_bytes == 0 {
                                                return 0;
                                        }
                                        let mut got = 0;
                                        while got < want_bytes {
                                                match file.read(fs, &mut scratch[got..want_bytes]) {
                                                        Ok(0) => break,
                                                        Ok(n) => got += n,
                                                        Err(_) => {
                                                                failed = true;
                                                                break;
                                                        }
                                                }
                                        }
                                        let frames = got / stride;
                                        for f in 0..frames {
                                                let b = f * stride;
                                                let sample = i16::from_le_bytes([scratch[b], scratch[b + 1]]);
                                                let s = u32::from(sample as u16);
                                                dst[f] = s << 16 | s;
                                        }
                                        produced = frames;
                                        frames
                                });
                                if failed || produced == 0 {
                                        break;
                                }
                        }
                        //   done producing when the data chunk is spent; finish only once the
                        // ring has also drained, so the tail is not cut off. A card error ends
                        // it at once. Clearing the ring drops any queued tail to silence.
                        let exhausted = self.play.as_ref().map(|p| p.file.pos() >= p.data_end).unwrap_or(true);
                        if failed {
                                warn!("play: read failed; stopping");
                                self.stream.stream_clear();
                                self.stream.set_active(false);
                                *self.play = None;
                        } else if exhausted && self.stream.stream_pending() == 0 {
                                info!("play: finished");
                                self.stream.set_active(false);
                                *self.play = None;
                        }
                } else if self.remaining > 0 {
                        //   the test tone: synthesise sine straight into the ring, one word per
                        // frame, capped at the frames left so the tone runs its exact length
                        self.stream.set_active(true);
                        let stream = &mut self.stream;
                        let phase = &mut self.phase;
                        let inc = self.phase_inc;
                        let remaining = &mut self.remaining;
                        while stream.stream_free() > 0 && *remaining > 0 {
                                let mut produced = 0usize;
                                stream.stream_push(&mut |dst| {
                                        let n = dst.len().min(*remaining as usize);
                                        for slot in dst[..n].iter_mut() {
                                                let s = u32::from(SINE[(*phase >> 27) as usize] as u16);
                                                *slot = s << 16 | s;
                                                *phase = phase.wrapping_add(inc);
                                        }
                                        *remaining -= n as u32;
                                        produced = n;
                                        n
                                });
                                if produced == 0 {
                                        break;
                                }
                        }
                } else {
                        //   idle (or recording): nothing to play. The ring drains to silence and
                        // underrun accounting is gated off, so that is not counted as starvation.
                        self.stream.set_active(false);
                }
                //   publish-on-change: the elapsed second is part of the status value, so a
                // live take announces itself once a second and transitions announce at once
                let s = self.status();
                if s != self.last_status {
                        self.last_status = s;
                        let _ = self.bus.publish(Event::Status(s));
                }
                if playing { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.pa.set(false);
        }
}

// --- the console ---------------------------------------------------------------------------

/// The console: the shared CLI owns the grammar and the built-ins; the COMMAND TABLE is
/// the board's (this crate's rows plus whatever the board adds), so it lives as a static
/// in the board crate and arrives here by reference.
pub struct ConsoleMod<X: Copy + 'static> {
        reader: LineReader<96>,
        cli: &'static Cli<Event<X>>,
        bus: &'static dyn Bus<Event<X>>,
}

impl<X: Copy + core::fmt::Debug + 'static> ConsoleMod<X> {
        pub fn new(cli: &'static Cli<Event<X>>, bus: &'static dyn Bus<Event<X>>) -> Self {
                Self { reader: LineReader::new(), cli, bus }
        }

        fn dispatch(&mut self, line: &str) -> Poll {
                match self.cli.dispatch(line) {
                        Outcome::Quiet => Poll::Idle,
                        Outcome::Shutdown => Poll::Shutdown,
                        Outcome::Event(ev) => {
                                if let Event::Command(Command::Stats) = ev {
                                        info!("console: {} bytes dropped, {} lines dropped; bus: {} refused, {} backlog", CONSOLE_BYTES.dropped(), self.reader.dropped_lines, self.bus.refused(), self.bus.backlog());
                                }
                                if let Err(e) = self.bus.publish(ev) {
                                        warn!("event bus full; dropped {e:?}");
                                }
                                Poll::Busy
                        }
                        Outcome::Handled => Poll::Busy,
                }
        }
}

impl<X: Copy + core::fmt::Debug + 'static> Module for ConsoleMod<X> {
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

// --- the command parsers, for the board's command table ------------------------------------

pub fn parse_stats<X: Copy>(_w: &mut Words) -> Parsed<Event<X>> {
        Parsed::Event(Event::Command(Command::Stats))
}

pub fn parse_play<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next() {
                None => Parsed::Event(Event::Command(Command::PlayStatus)),
                Some("stop") => Parsed::Event(Event::Command(Command::PlayStop)),
                Some(name) => match FsPath::new(name) {
                        Some(p) => Parsed::Event(Event::Command(Command::PlayStart(p))),
                        None => Parsed::Usage,
                },
        }
}

pub fn parse_rec<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next() {
                None => Parsed::Event(Event::Command(Command::RecStatus)),
                Some("stop") => Parsed::Event(Event::Command(Command::RecStop)),
                Some(name) => match FsPath::new(name) {
                        Some(p) => Parsed::Event(Event::Command(Command::RecStart(p))),
                        None => Parsed::Usage,
                },
        }
}

pub fn parse_fs<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
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
                (Some(path), Some(arg)) => Parsed::Event(Event::Command(Command::Fs { op, path, arg })),
                _ => Parsed::Usage,
        }
}

pub fn parse_backlight<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next().and_then(|s| s.parse::<u16>().ok()) {
                Some(level) if level <= BACKLIGHT_LEVEL_MAX => Parsed::Event(Event::Command(Command::Backlight(level))),
                _ => Parsed::Usage,
        }
}

pub fn parse_ui<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match (w.next(), w.next(), w.next()) {
                (Some("focus"), Some("next"), _) => Parsed::Event(Event::Command(Command::UiFocus { next: true })),
                (Some("focus"), Some("prev"), _) => Parsed::Event(Event::Command(Command::UiFocus { next: false })),
                (Some("activate"), _, _) => Parsed::Event(Event::Command(Command::UiActivate)),
                (Some("press"), Some(x), Some(y)) => match (x.parse(), y.parse()) {
                        (Ok(x), Ok(y)) => Parsed::Event(Event::Command(Command::UiPress { x, y })),
                        _ => Parsed::Usage,
                },
                (Some("back"), _, _) => Parsed::Event(Event::Command(Command::UiBack)),
                _ => Parsed::Usage,
        }
}

pub fn parse_touch<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
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

pub fn parse_render<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next() {
                Some("pause") => Parsed::Event(Event::Command(Command::RenderMode(RenderMode::Paused))),
                Some("resume") => Parsed::Event(Event::Command(Command::RenderMode(RenderMode::Normal))),
                Some("repush") => Parsed::Event(Event::Command(Command::RenderMode(RenderMode::Repush))),
                _ => Parsed::Usage,
        }
}

pub fn parse_tone<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next() {
                Some("off") => Parsed::Event(Event::Command(Command::ToneOff)),
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
                        Parsed::Event(Event::Command(Command::Tone { hz, ms }))
                }
                None => Parsed::Usage,
        }
}

pub fn parse_volume<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next().and_then(|s| s.parse::<u8>().ok()) {
                Some(v) if v <= 100 => Parsed::Event(Event::Command(Command::Volume(v))),
                _ => Parsed::Usage,
        }
}

pub fn parse_micmon<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next() {
                Some("on") => Parsed::Event(Event::Command(Command::MicMon(true))),
                Some("off") => Parsed::Event(Event::Command(Command::MicMon(false))),
                _ => Parsed::Usage,
        }
}

pub fn parse_micgain<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match (w.next().and_then(|s| u8::from_str_radix(s, 16).ok()), w.next().and_then(|s| u8::from_str_radix(s, 16).ok())) {
                (Some(r14), Some(r17)) => Parsed::Event(Event::Command(Command::MicGain { r14, r17 })),
                _ => Parsed::Usage,
        }
}

pub fn parse_micdbg<X: Copy>(_w: &mut Words) -> Parsed<Event<X>> {
        Parsed::Event(Event::Command(Command::MicDbg))
}

pub fn parse_synth<X: Copy>(_w: &mut Words) -> Parsed<Event<X>> {
        Parsed::Event(Event::Command(Command::Synth))
}

//   the command-row type, for the dictaphone_commands! expansion
#[doc(hidden)]
pub use light_core::cli::Command as DictCliRow;

/// The command table: the rows this application carries, spelled once, plus the board's
/// own. Expands to an ARRAY, so take a reference for the CLI's slice.
#[macro_export]
macro_rules! dictaphone_commands {
        ($ext:ty $(; $($extra:expr),* $(,)?)?) => {
                [
                        $crate::DictCliRow { name: "stats", usage: "stats", parse: $crate::parse_stats::<$ext> },
                        $crate::DictCliRow { name: "tone", usage: "tone HZ [MS] | tone off", parse: $crate::parse_tone::<$ext> },
                        $crate::DictCliRow { name: "volume", usage: "volume 0..100", parse: $crate::parse_volume::<$ext> },
                        $crate::DictCliRow { name: "fs", usage: "fs info|ls [P]|cat P|write P TEXT|rm P|mv A B|mkdir P|trunc P N|hex P OFF|peak P", parse: $crate::parse_fs::<$ext> },
                        $crate::DictCliRow { name: "rec", usage: "rec NAME.WAV | rec stop | rec", parse: $crate::parse_rec::<$ext> },
                        $crate::DictCliRow { name: "play", usage: "play NAME.WAV | play stop | play", parse: $crate::parse_play::<$ext> },
                        $crate::DictCliRow { name: "micmon", usage: "micmon on|off", parse: $crate::parse_micmon::<$ext> },
                        $crate::DictCliRow { name: "micdbg", usage: "micdbg", parse: $crate::parse_micdbg::<$ext> },
                        $crate::DictCliRow { name: "synth", usage: "synth", parse: $crate::parse_synth::<$ext> },
                        $crate::DictCliRow { name: "micgain", usage: "micgain R14HEX R17HEX (e.g. micgain 17 DF)", parse: $crate::parse_micgain::<$ext> },
                        $crate::DictCliRow { name: "backlight", usage: "backlight 0..1000", parse: $crate::parse_backlight::<$ext> },
                        $crate::DictCliRow { name: "ui", usage: "ui focus next|prev | ui activate | ui press X Y | ui back", parse: $crate::parse_ui::<$ext> },
                        $crate::DictCliRow { name: "touch", usage: "touch hold|free", parse: $crate::parse_touch::<$ext> },
                        $crate::DictCliRow { name: "render", usage: "render pause|resume|repush", parse: $crate::parse_render::<$ext> },
                        $($($extra),*)?
                ]
        };
}

/// A fixed-capacity string for formatting a line without an allocator.
pub struct StackString<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> StackString<N> {
        pub const fn new() -> Self {
                Self { buf: [0; N], len: 0 }
        }
        pub fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
        }
        pub fn as_bytes(&self) -> &[u8] {
                &self.buf[..self.len]
        }
}

impl<const N: usize> Default for StackString<N> {
        fn default() -> Self {
                Self::new()
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
