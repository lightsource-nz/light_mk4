//! The widget demo every touch board runs -- the APPLICATION, with no hardware in it.
//!
//! Three pages (toggles, a detail page, a scrolling list), driven by touch with swipe-back,
//! reorientable from an IMU, themed, and instrumented from the console. A tangible demo --
//! the 1.69, the 2.8, the 3.49 bar, the 4.0 square -- is a hardware-bound module that
//! constructs the concrete parts and owns everything this crate must not: pins, the display
//! driver and its quirks, the touch controller, board peripherals, the shell ABI, the panic
//! handler.
//!
//! The seams, in order of appearance:
//! - [`DemoEvent`] carries the demo's events plus a board EXTENSION type `X`, so board
//!   modules (audio, battery, PSRAM...) ride the same bus without this crate naming them.
//! - The event bus itself is a board static (its depth and subscriber count are board
//!   facts); it reaches the demo as `&'static dyn `[`Bus`].
//! - [`demo_pages!`] expands the page tree in the board crate, where the event type is
//!   concrete -- the board contributes only its title and touch-target metrics.
//! - [`DisplayMod`] is generic over the display driver, with a [`BoardHook`] for the
//!   driver-specific edges: GDDRAM offsets, panel clears, and any board command that needs
//!   the display or the UI (a bus re-clock, a test pattern).
//! - Every controller reports the one `light_input` touch [`TouchSample`], so touch needs
//!   no genericity at all.

#![no_std]

use light_core::cli::{Cli, Outcome, Parsed, Words};
use light_core::{debug, info, log, warn, Clock, LineReader, Mailbox, Module, Poll, Subscription};
use light_display::{Display, DisplayDriver, FrameLayer, Region, UpdateError};
use light_draw::Rotation;
use light_font::Font;
use light_input::imu::Orientation;
use light_input::touch::Gesture;
use light_ui::{SwipeDir, Touch, Ui};

//   what the page-tree macro and the board crates build against, from one place
pub use light_input::cst816t::Event as TouchSample;
pub use light_ui::{scroll, Desc, Page, Shade};

/// Widget arena size: the deepest page is the list (a window and nine rows).
pub const UI_WIDGETS: usize = 12;
/// The backlight scale every board's PWM runs, `0..=MAX` per-mille.
pub const BACKLIGHT_LEVEL_MAX: u16 = 1000;

pub const LABEL_OFF: [&str; 3] = ["Alpha", "Beta", "Gamma"];
pub const LABEL_ON: [&str; 3] = ["Alpha *", "Beta *", "Gamma *"];

// --- the event bus ------------------------------------------------------------------------

/// Everything that happens in a demo, as one type: the demo's own events, plus the board's
/// extension `X` -- audio, battery, filesystem, whatever the tangible board carries.
#[derive(Clone, Copy, Debug)]
pub enum DemoEvent<X: Copy> {
        Touch(TouchSample),
        /// A swipe, in the panel's own coordinate space.
        Gesture(Gesture),
        /// The board's settled orientation changed.
        Orientation(Orientation),
        Command(Command),
        /// Something a widget emitted.
        Ui(UiAction),
        /// The board's own affair; the demo carries it and looks away.
        Ext(X),
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
        Stats,
        /// A backlight level, 0..=[`BACKLIGHT_LEVEL_MAX`].
        Backlight(u16),
        /// The UI events as commands: `ui focus next|prev`, `ui activate`, `ui press X Y`,
        /// `ui back`. Most of their value is on a bring-up rig: a console drives a board
        /// whose only physical input is a touch panel, and a host script replays an
        /// interaction.
        UiFocus { next: bool },
        UiActivate,
        UiPress { x: u16, y: u16 },
        UiBack,
        /// The rendering bisect: normal; paused (nothing drawn or pushed); or repushing the
        /// UNCHANGED frame on every touch -- all the bus and DMA activity, nothing changing
        /// on the glass -- which separates electrical coupling from the LCD itself
        /// disturbing the touch sensor.
        RenderMode(RenderMode),
        /// The focused cell's gradient, live-tunable like every look-and-feel knob:
        /// `shade FROM TO` in RGB565 hex, `shade off` for the solid inversion.
        FocusShade(Option<Shade>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
        Normal,
        Paused,
        Repush,
}

#[derive(Clone, Copy, Debug)]
pub enum UiAction {
        /// One of the three toggle buttons.
        Toggle(u8),
        /// A list row.
        Item(u8),
        /// A drag scrolled a window: the finger's movement is spent, and the touch module
        /// must not let its release classify as a swipe as well.
        DragConsumed,
}

/// The event bus as the demo sees it: `light_core::Bus`, the capacity-erased view of a
/// bus that stays a BOARD static -- its depth and subscriber count depend on how many
/// board modules ride it.
pub use light_core::Bus;

// --- shared state --------------------------------------------------------------------------

static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();
/// The push-versus-touch bisect instruments (the 1.69's wedge hunt, carried everywhere):
/// whether the panel is mid-push, and whether touch reads should hold while it is.
static PUSHING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static TOUCH_HOLD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// A console byte from the transport the board module owns. Never blocks; a full mailbox
/// drops the byte, and the line it belonged to will fail to parse and say so.
pub fn push_console_byte(b: u8) {
        let _ = CONSOLE_BYTES.push(b);
}

/// Whether a board's touch module should skip this read: `touch hold` is on and the panel
/// is being pushed -- the bisect that separates bus activity from panel emissions.
pub fn touch_reads_held() -> bool {
        TOUCH_HOLD.load(core::sync::atomic::Ordering::Relaxed) && PUSHING.load(core::sync::atomic::Ordering::Relaxed)
}

// --- the page tree, expanded where the event type is concrete ------------------------------

/// The demo's three pages, as statics in the BOARD crate: `PAGE_MAIN`, `PAGE_DETAIL` and
/// `PAGE_LIST`, with the board's window title, row gap and touch-target height baked in --
/// the metrics are hardware (finger size against pixel density), the tree is not.
///
/// ```ignore
/// light_app_ui_demo::demo_pages! {
///         event: AppEvent, title: "mk4 2.8", row_gap: 6, list_min_row: 56,
///         backlight_dim: BACKLIGHT_DIM
/// }
/// ```
#[macro_export]
macro_rules! demo_pages {
        (event: $event:ty, title: $title:expr, row_gap: $row_gap:expr, list_min_row: $list_min_row:expr, backlight_dim: $backlight_dim:expr) => {
                static BTN_ALPHA: $crate::Desc<$event> = $crate::Desc::button($crate::LABEL_OFF[0]).emit(<$event>::Ui($crate::UiAction::Toggle(0))).tag(1);
                static BTN_BETA: $crate::Desc<$event> = $crate::Desc::button($crate::LABEL_OFF[1]).emit(<$event>::Ui($crate::UiAction::Toggle(1))).tag(2);
                static BTN_GAMMA: $crate::Desc<$event> = $crate::Desc::button($crate::LABEL_OFF[2]).emit(<$event>::Ui($crate::UiAction::Toggle(2))).tag(3);
                static BTN_MORE: $crate::Desc<$event> = $crate::Desc::button("More >").navigate(&PAGE_DETAIL);
                static BTN_LIST: $crate::Desc<$event> = $crate::Desc::button("List >").navigate(&PAGE_LIST);
                static MAIN_WINDOW: $crate::Desc<$event> = $crate::Desc::window($title).stack($row_gap).children(&[&BTN_ALPHA, &BTN_BETA, &BTN_GAMMA, &BTN_MORE, &BTN_LIST]);

                static LBL_DETAIL: $crate::Desc<$event> = $crate::Desc::label("swipe right to go back");
                static BTN_DIM: $crate::Desc<$event> = $crate::Desc::button("Dim").emit(<$event>::Command($crate::Command::Backlight($backlight_dim)));
                static BTN_BRIGHT: $crate::Desc<$event> = $crate::Desc::button("Bright").emit(<$event>::Command($crate::Command::Backlight($crate::BACKLIGHT_LEVEL_MAX)));
                static BTN_BACK: $crate::Desc<$event> = $crate::Desc::button("< Back").back();
                static DETAIL_WINDOW: $crate::Desc<$event> = $crate::Desc::window("More").stack($row_gap).children(&[&LBL_DETAIL, &BTN_DIM, &BTN_BRIGHT, &BTN_BACK]);

                static ITEM_1: $crate::Desc<$event> = $crate::Desc::button("Item 1").emit(<$event>::Ui($crate::UiAction::Item(1))).min_size(0, $list_min_row);
                static ITEM_2: $crate::Desc<$event> = $crate::Desc::button("Item 2").emit(<$event>::Ui($crate::UiAction::Item(2))).min_size(0, $list_min_row);
                static ITEM_3: $crate::Desc<$event> = $crate::Desc::button("Item 3").emit(<$event>::Ui($crate::UiAction::Item(3))).min_size(0, $list_min_row);
                static ITEM_4: $crate::Desc<$event> = $crate::Desc::button("Item 4").emit(<$event>::Ui($crate::UiAction::Item(4))).min_size(0, $list_min_row);
                static ITEM_5: $crate::Desc<$event> = $crate::Desc::button("Item 5").emit(<$event>::Ui($crate::UiAction::Item(5))).min_size(0, $list_min_row);
                static ITEM_6: $crate::Desc<$event> = $crate::Desc::button("Item 6").emit(<$event>::Ui($crate::UiAction::Item(6))).min_size(0, $list_min_row);
                static ITEM_7: $crate::Desc<$event> = $crate::Desc::button("Item 7").emit(<$event>::Ui($crate::UiAction::Item(7))).min_size(0, $list_min_row);
                //   the back row is a list item like any other, and deliberately LAST:
                // reaching it means scrolling the whole list, so navigating out doubles as
                // the end-to-end check
                static BTN_LIST_BACK: $crate::Desc<$event> = $crate::Desc::button("< Back").back().min_size(0, $list_min_row);
                static LIST_WINDOW: $crate::Desc<$event> = $crate::Desc::window("List")
                        .stack($row_gap)
                        .scroll($crate::scroll::VERTICAL)
                        .children(&[&ITEM_1, &ITEM_2, &ITEM_3, &ITEM_4, &ITEM_5, &ITEM_6, &ITEM_7, &BTN_LIST_BACK]);

                static PAGE_MAIN: $crate::Page<$event> = $crate::Page::new(&MAIN_WINDOW, None);
                static PAGE_DETAIL: $crate::Page<$event> = $crate::Page::new(&DETAIL_WINDOW, Some(&PAGE_MAIN));
                static PAGE_LIST: $crate::Page<$event> = $crate::Page::new(&LIST_WINDOW, Some(&PAGE_MAIN));
        };
}

// --- the display module --------------------------------------------------------------------

/// What a tangible board tells [`DisplayMod`] about its panel.
pub struct DisplayConfig<X: Copy + 'static> {
        pub width: u16,
        pub height: u16,
        pub fps: u32,
        /// The panel, for the boot log: "240x320 ST7789 over SPI", say.
        pub desc: &'static str,
        /// Whether `render repush` means anything here: a memoryless scanout panel has no
        /// push to repeat, and asking is answered rather than half-obeyed.
        pub repush: bool,
        /// Whether frames draw OVER the previous image instead of starting from a cleared
        /// canvas. For a buffer that is LIVE on the glass (a hardware scanout): a cleared
        /// frame flashes black under the beam before the repaint reaches it, so the window
        /// interiors cover what the clear used to.
        pub draw_over: bool,
        /// The board's measured orientation-to-rotation table: which poses have an upright,
        /// and which rotation shows it. The 3.49 bar, for instance, maps landscape to None
        /// -- resting near-landscape on its long edge, ordinary handling flapped 180
        /// degrees per touch.
        pub rotation_map: fn(Orientation) -> Option<Rotation>,
        pub main_page: &'static Page<DemoEvent<X>>,
}

/// The driver-specific edges of the demo, implemented by the board module: what happens
/// around init and unload (GDDRAM offsets, panel clears -- inherent driver methods this
/// crate cannot name), and any board command that needs the display or the UI.
pub trait BoardHook<D: DisplayDriver, X: Copy + 'static> {
        /// After `Display::init` at load, before the first frame.
        fn after_init(&mut self, _display: &mut Display<'static, D>, _bg: u16) {}
        /// At unload, after the last push completed.
        fn on_unload(&mut self, _display: &mut Display<'static, D>, _bg: u16) {}
        /// A board extension event that wants the display, the layer or the UI -- a bus
        /// re-clock, a test pattern. Everything else rides the bus past this module.
        fn on_ext(&mut self, _view: &mut DemoView<'_, D, X>, _ext: X) {}
        /// Whether a draw may start NOW. `false` defers it: dirty stays set, the module
        /// stays busy, and the next poll asks again. For a panel whose framebuffer is live
        /// on the glass, this is where drawing is scheduled around the beam.
        fn render_gate(&mut self, _ui: &Ui<DemoEvent<X>, UI_WIDGETS>, _layer: &FrameLayer) -> bool {
                true
        }
        /// After the demo's own `stats` line: whatever this board's display adds.
        fn on_stats(&mut self) {}
}

/// The display module's innards, lent to [`BoardHook::on_ext`].
pub struct DemoView<'a, D: DisplayDriver, X: Copy + 'static> {
        pub display: &'a mut Display<'static, D>,
        pub layer: &'a mut FrameLayer,
        pub ui: &'a mut Ui<DemoEvent<X>, UI_WIDGETS>,
        pub mode: &'a mut RenderMode,
}

/// Owns the panel and the widget tree: renders when something is dirty, routes touches and
/// commands into the toolkit, follows the IMU's orientation, and keeps the draw/push timing
/// the `stats` command reports.
pub struct DisplayMod<D: DisplayDriver, C: Clock, X: Copy + 'static, H: BoardHook<D, X>> {
        display: Display<'static, D>,
        //   the two big objects live in .bss as statics built in place (their constructors
        // are const): a stack temporary of either overran core 0's 4 KB stack, and what
        // sits directly below it is core 1's
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<DemoEvent<X>, UI_WIDGETS>,
        clock: C,
        hook: H,
        cfg: DisplayConfig<X>,
        bus: &'static dyn Bus<DemoEvent<X>>,
        sub: Subscription,
        toggled: [bool; 3],
        mode: RenderMode,
        /// Whether the drag in progress has already told the touch module it consumed the
        /// touch.
        drag_reported: bool,
        /// Timing, for `stats`: the longest draw (frame_begin to frame_end) and the longest
        /// push (frame_end until the panel is idle) seen, in microseconds.
        draw_us_max: u64,
        push_us_max: u64,
        push_started_us: Option<u64>,
}

impl<D: DisplayDriver, C: Clock, X: Copy + core::fmt::Debug + 'static, H: BoardHook<D, X>> DisplayMod<D, C, X, H> {
        pub fn new(
                display: Display<'static, D>,
                layer: &'static mut FrameLayer,
                font: Font<'static>,
                ui: &'static mut Ui<DemoEvent<X>, UI_WIDGETS>,
                clock: C,
                bus: &'static dyn Bus<DemoEvent<X>>,
                cfg: DisplayConfig<X>,
                hook: H,
        ) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { display, layer, font, ui, clock, hook, cfg, bus, sub, toggled: [false; 3], mode: RenderMode::Normal, drag_reported: false, draw_us_max: 0, push_us_max: 0, push_started_us: None }
        }

        fn publish(bus: &dyn Bus<DemoEvent<X>>, ev: Option<DemoEvent<X>>) {
                if let Some(ev) = ev {
                        if let Err(e) = bus.publish(ev) {
                                warn!("event bus full; dropped {e:?}");
                        }
                }
        }

        fn handle(&mut self, ev: DemoEvent<X>) {
                match ev {
                        DemoEvent::Touch(t) => {
                                //   the panel's own coordinates go straight in: the toolkit
                                // untransforms them, which keeps touches landing on the
                                // right widget once the interface has been rotated. The
                                // tracker runs the whole tap-versus-drag interaction; this
                                // module's part is one rule: a drag that scrolled has SPENT
                                // the finger's movement
                                if self.mode == RenderMode::Repush {
                                        if let TouchSample::Down { .. } = t {
                                                // the front buffer as it stands, to the whole panel
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
                                                Self::publish(self.bus, Some(DemoEvent::Ui(UiAction::DragConsumed)));
                                        }
                                        Touch::Tap { hit, emitted } => {
                                                debug!("tap: {}", if hit { "hit" } else { "no widget there" });
                                                Self::publish(self.bus, emitted);
                                        }
                                        Touch::DragEnd | Touch::None => self.drag_reported = false,
                                        _ => {}
                                }
                        }
                        DemoEvent::Gesture(g) => {
                                //   swipe right returns to the previous page. Classified
                                // from the gesture's ENDPOINTS in the frame the user is
                                // looking at: the controller's own code is in the panel's
                                // frame, which is wrong in landscape
                                if self.ui.swipe_direction(g.start, g.end) == Some(SwipeDir::Right) && self.ui.navigate_back() {
                                        debug!("swipe: returned to the previous page");
                                }
                        }
                        DemoEvent::Orientation(o) => {
                                if let Some(r) = (self.cfg.rotation_map)(o) {
                                        self.ui.set_rotation(self.layer, r);
                                        let (w, h) = self.ui.logical_size();
                                        info!("orientation {o:?}: canvas now {w}x{h}");
                                }
                        }
                        DemoEvent::Command(Command::UiFocus { next }) => {
                                if next {
                                        self.ui.focus_next()
                                } else {
                                        self.ui.focus_prev()
                                }
                        }
                        DemoEvent::Command(Command::UiActivate) => {
                                let emitted = self.ui.activate();
                                Self::publish(self.bus, emitted);
                        }
                        DemoEvent::Command(Command::UiPress { x, y }) => {
                                // a miss is not an error: tapping empty space is legitimate
                                let (hit, emitted) = self.ui.press_at(x, y);
                                info!("ui press {x} {y}: {}", if hit { "hit" } else { "no widget there" });
                                Self::publish(self.bus, emitted);
                        }
                        DemoEvent::Command(Command::RenderMode(m)) => {
                                if m == RenderMode::Repush && !self.cfg.repush {
                                        info!("render repush means nothing on this display");
                                        return;
                                }
                                //   coming back to Normal, the glass is untrusted: a pause
                                // may have left a test pattern or stale content standing,
                                // so resume repaints everything rather than waiting for the
                                // next interaction to dirty a widget
                                if m == RenderMode::Normal && self.mode != RenderMode::Normal {
                                        self.ui.invalidate_all();
                                }
                                self.mode = m;
                                info!("render mode {m:?}");
                        }
                        DemoEvent::Command(Command::FocusShade(s)) => {
                                self.ui.set_focus_shade(s);
                                match s {
                                        Some(s) => info!("focus shade {:04x} -> {:04x}", s.from, s.to),
                                        None => info!("focus shade off"),
                                }
                        }
                        DemoEvent::Command(Command::UiBack) => {
                                if !self.ui.navigate_back() {
                                        info!("ui back: nowhere to go from this page");
                                }
                        }
                        DemoEvent::Ui(UiAction::Toggle(i)) => {
                                let i = usize::from(i) % 3;
                                self.toggled[i] = !self.toggled[i];
                                if let Some(id) = self.ui.find(i as u8 + 1) {
                                        self.ui.set_label(id, if self.toggled[i] { LABEL_ON[i] } else { LABEL_OFF[i] });
                                }
                                info!("button {i} toggled {}", if self.toggled[i] { "on" } else { "off" });
                        }
                        DemoEvent::Ui(UiAction::Item(n)) => info!("list item {n} pressed"),
                        DemoEvent::Command(Command::Stats) => {
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
                                self.hook.on_stats();
                        }
                        DemoEvent::Ext(x) => {
                                //   the borrow split: the hook gets the innards, not self
                                let Self { display, layer, ui, mode, hook, .. } = self;
                                let mut view = DemoView { display, layer: &mut **layer, ui: &mut **ui, mode };
                                hook.on_ext(&mut view, x);
                        }
                        _ => {}
                }
        }

        fn render(&mut self) {
                if self.mode != RenderMode::Normal || (!self.ui.is_dirty() && !self.ui.is_animating()) {
                        return;
                }
                {
                        let Self { hook, ui, layer, .. } = &mut *self;
                        if !hook.render_gate(&**ui, &**layer) {
                                // dirty stays set; poll returns Busy and retries shortly
                                return;
                        }
                }
                let now = log::now_us();
                //   through Ui::render, which also runs the rotation and page animations
                let drew = self.ui.render(self.layer, &mut self.display, &self.font, now);
                let done = log::now_us();
                if drew || self.ui.is_animating() {
                        self.draw_us_max = self.draw_us_max.max(done - now);
                        self.push_started_us = Some(done);
                }
        }
}

impl<D: DisplayDriver, C: Clock, X: Copy + core::fmt::Debug + 'static, H: BoardHook<D, X>> Module for DisplayMod<D, C, X, H> {
        fn name(&self) -> &'static str {
                "display"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.display.init(&mut self.clock);
                self.hook.after_init(&mut self.display, self.ui.theme().bg);
                self.layer.set_frame_rate(self.cfg.fps);
                self.layer.bg = self.ui.theme().bg;
                self.layer.draw_over = self.cfg.draw_over;
                self.ui.fit(self.layer);
                //   entered through the page system rather than built directly, so the
                // toolkit knows which page it is showing and back has something to reason from
                if let Err(e) = self.ui.navigate(self.cfg.main_page) {
                        warn!("the main page did not build: {e:?}");
                }
                // nothing on the panel matches the freshly built tree yet
                self.ui.invalidate_all();
                self.render();
                info!(
                        "display up: {}x{} ({}) at {} fps, font {}px cell {}x{} ({} glyphs)",
                        self.cfg.width,
                        self.cfg.height,
                        self.cfg.desc,
                        self.cfg.fps,
                        self.font.pixel_size(),
                        self.font.cell_width(),
                        self.font.cell_height(),
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
                self.render();
                let busy = self.layer.busy(&self.display);
                PUSHING.store(busy, core::sync::atomic::Ordering::Relaxed);
                if self.ui.is_dirty() || self.ui.is_animating() || busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.hook.on_unload(&mut self.display, self.ui.theme().bg);
                info!("display down");
        }
}

// --- the console ---------------------------------------------------------------------------

/// The console: the shared CLI owns the grammar and the built-ins; the COMMAND TABLE is the
/// board's (this crate's core commands plus whatever the board adds), so it lives as a
/// static in the board crate and arrives here by reference.
pub struct ConsoleMod<X: Copy + 'static> {
        reader: LineReader<96>,
        cli: &'static Cli<DemoEvent<X>>,
        bus: &'static dyn Bus<DemoEvent<X>>,
}

impl<X: Copy + core::fmt::Debug + 'static> ConsoleMod<X> {
        pub fn new(cli: &'static Cli<DemoEvent<X>>, bus: &'static dyn Bus<DemoEvent<X>>) -> Self {
                Self { reader: LineReader::new(), cli, bus }
        }

        fn dispatch(&mut self, line: &str) -> Poll {
                match self.cli.dispatch(line) {
                        Outcome::Quiet => Poll::Idle,
                        Outcome::Shutdown => Poll::Shutdown,
                        Outcome::Event(ev) => {
                                if let DemoEvent::Command(Command::Stats) = ev {
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

// --- the core command parsers, for the board's command table -------------------------------

pub fn parse_stats<X: Copy>(_w: &mut Words) -> Parsed<DemoEvent<X>> {
        Parsed::Event(DemoEvent::Command(Command::Stats))
}

pub fn parse_backlight<X: Copy>(w: &mut Words) -> Parsed<DemoEvent<X>> {
        match w.next().and_then(|s| s.parse::<u16>().ok()) {
                Some(level) if level <= BACKLIGHT_LEVEL_MAX => Parsed::Event(DemoEvent::Command(Command::Backlight(level))),
                _ => Parsed::Usage,
        }
}

pub fn parse_ui<X: Copy>(w: &mut Words) -> Parsed<DemoEvent<X>> {
        match (w.next(), w.next(), w.next()) {
                (Some("focus"), Some("next"), _) => Parsed::Event(DemoEvent::Command(Command::UiFocus { next: true })),
                (Some("focus"), Some("prev"), _) => Parsed::Event(DemoEvent::Command(Command::UiFocus { next: false })),
                (Some("activate"), _, _) => Parsed::Event(DemoEvent::Command(Command::UiActivate)),
                (Some("press"), Some(x), Some(y)) => match (x.parse(), y.parse()) {
                        (Ok(x), Ok(y)) => Parsed::Event(DemoEvent::Command(Command::UiPress { x, y })),
                        _ => Parsed::Usage,
                },
                (Some("back"), _, _) => Parsed::Event(DemoEvent::Command(Command::UiBack)),
                _ => Parsed::Usage,
        }
}

pub fn parse_touch<X: Copy>(w: &mut Words) -> Parsed<DemoEvent<X>> {
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

pub fn parse_render<X: Copy>(w: &mut Words) -> Parsed<DemoEvent<X>> {
        match w.next() {
                Some("pause") => Parsed::Event(DemoEvent::Command(Command::RenderMode(RenderMode::Paused))),
                Some("resume") => Parsed::Event(DemoEvent::Command(Command::RenderMode(RenderMode::Normal))),
                Some("repush") => Parsed::Event(DemoEvent::Command(Command::RenderMode(RenderMode::Repush))),
                _ => Parsed::Usage,
        }
}

pub fn parse_shade<X: Copy>(w: &mut Words) -> Parsed<DemoEvent<X>> {
        match w.next() {
                Some("off") => Parsed::Event(DemoEvent::Command(Command::FocusShade(None))),
                Some(from) => match (u16::from_str_radix(from, 16), w.next().map(|t| u16::from_str_radix(t, 16))) {
                        (Ok(from), Some(Ok(to))) => Parsed::Event(DemoEvent::Command(Command::FocusShade(Some(Shade { from, to })))),
                        _ => Parsed::Usage,
                },
                None => Parsed::Usage,
        }
}

//   the command-row type, for the demo_commands! expansion
#[doc(hidden)]
pub use light_core::cli::Command as DemoCliRow;

/// The command table: the rows every board carries, spelled once, plus the board's own.
/// Expands to an ARRAY, so take a reference for the CLI's slice:
///
/// ```ignore
/// static COMMANDS: &[CliCommand<AppEvent>] = &light_app_ui_demo::demo_commands![Ext;
///         CliCommand { name: "spi", usage: "spi HZ", parse: parse_spi },
/// ];
/// static CLI: Cli<AppEvent> = Cli::new(COMMANDS);
/// ```
#[macro_export]
macro_rules! demo_commands {
        ($ext:ty $(; $($extra:expr),* $(,)?)?) => {
                [
                        $crate::DemoCliRow { name: "stats", usage: "stats", parse: $crate::parse_stats::<$ext> },
                        $crate::DemoCliRow { name: "backlight", usage: "backlight 0..1000", parse: $crate::parse_backlight::<$ext> },
                        $crate::DemoCliRow { name: "shade", usage: "shade FROM16 TO16 (rgb565 hex) | shade off", parse: $crate::parse_shade::<$ext> },
                        $crate::DemoCliRow { name: "ui", usage: "ui focus next|prev | ui activate | ui press X Y | ui back", parse: $crate::parse_ui::<$ext> },
                        $crate::DemoCliRow { name: "touch", usage: "touch hold|free", parse: $crate::parse_touch::<$ext> },
                        $crate::DemoCliRow { name: "render", usage: "render pause|resume|repush", parse: $crate::parse_render::<$ext> },
                        $($($extra),*)?
                ]
        };
}
