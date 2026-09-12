//! The widget toolkit, ported from the predecessor C framework.
//!
//! A retained tree of windows, buttons and labels over the frame layer. The tree is built from
//! `const` descriptors that live in flash; navigating tears the current page down and builds
//! the next, so only one page's widgets exist at a time. Widgets live in a fixed arena owned by
//! the [`Ui`] -- no allocator, and a tree too big for it is a build error at the call that adds
//! the widget, never a silent drop.
//!
//! **What comes out is an event, not a callback.** The predecessor's buttons carried a C
//! function pointer, a `void *` and a command string; here a button carries what it *emits*
//! -- a value of the application's own event type -- and, optionally, where it navigates.
//! Activation returns the emitted value for the caller to publish on its bus, which is the
//! "command tree is the event bus" decision applied to the UI: a tap, a console line and a
//! boot script all end up as the same event, and a handler runs on its own module's poll
//! rather than inside the input path.
//!
//! **Hardware-free**, as its predecessor was: nothing here knows what a touch controller, a
//! push button or an IMU is. The application maps its devices onto the input calls (a few
//! lines per app), and the toolkit can be exercised entirely on the host.
//!
//! Coordinates: every widget rect is in ABSOLUTE logical canvas coordinates, never
//! parent-relative, so a hit test, a clip and an invalidation are the same arithmetic wherever a
//! widget sits. Input arrives in PANEL coordinates -- what a touch controller reports -- and is
//! untransformed here, because the toolkit is the one thing that knows the rotation.

#![no_std]

use heapless::Vec;

use light_display::{Display, DisplayDriver, Region};
use light_draw::{lerp565, Canvas, Flip, Point, Rotation, Transform};
use light_display::frames::{FrameLayer, LogicalRegion, MAX_REGIONS};
use light_core::{debug, error, trace, warn};
use light_font::Font;

pub mod theme;
pub use theme::Theme;

pub mod lui;
pub use lui::{Lui, LuiChild, LuiError, LuiPage, LuiRuntime};

/// Which typographic role a piece of text plays. A [`Style`] binds a font per role, so a title
/// can be set in a different face or size from body text; today's UIs use one face for every
/// role ([`Fonts::uniform`]). Add a role here and every consumer keeps compiling -- [`Fonts`]
/// takes a font for each role, and `uniform` fills them all from one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontRole {
        /// Window titles and the title bar: subtitle and status indicator.
        Title,
        /// Everything else -- button labels, list rows, labels -- and the fallback role.
        Body,
}

impl FontRole {
        /// The number of roles: the width of a [`Style`]'s font set and the metric arrays.
        pub const COUNT: usize = 2;
        /// Every role, for iterating the set.
        pub const ALL: [FontRole; Self::COUNT] = [FontRole::Title, FontRole::Body];
}

/// The fonts a [`Style`] binds, one per [`FontRole`]. Borrowed, never owned: glyph data stays
/// transient -- handed to [`Ui::render`] each frame -- so a UI's font can be swapped freely and
/// the [`Ui`] carries no font lifetime (it keeps only the cell METRICS, taken at
/// [`Ui::set_style`]).
#[derive(Clone, Copy)]
pub struct Fonts<'f> {
        title: &'f Font<'f>,
        body: &'f Font<'f>,
}

impl<'f> Fonts<'f> {
        /// One face for every role: the single-font look every current UI uses.
        pub const fn uniform(font: &'f Font<'f>) -> Self {
                Self { title: font, body: font }
        }

        /// A distinct face -- or size -- per role.
        pub const fn new(title: &'f Font<'f>, body: &'f Font<'f>) -> Self {
                Self { title, body }
        }

        /// The font bound to `role`.
        pub const fn font(&self, role: FontRole) -> &'f Font<'f> {
                match role {
                        FontRole::Title => self.title,
                        FontRole::Body => self.body,
                }
        }
}

/// A complete look to render with: a [`Theme`] (colours and metrics) plus the [`Fonts`] for
/// every [`FontRole`]. Bound into a [`Ui`] with [`Ui::set_style`] and handed to
/// [`Ui::render`] each frame -- the same value for both, so the metrics a UI lays out with and
/// the glyphs it paints with can never come from different fonts.
#[derive(Clone, Copy)]
pub struct Style<'f> {
        pub theme: Theme,
        pub fonts: Fonts<'f>,
}

impl<'f> Style<'f> {
        pub const fn new(theme: Theme, fonts: Fonts<'f>) -> Self {
                Self { theme, fonts }
        }
}

/// A widget rectangle: inclusive, logical, signed -- a widget positioned partly off the canvas
/// is clipped here before anything reaches the rasteriser.
pub type Rect = LogicalRegion;

/// How far a finger may wander (logical pixels, either axis) before a touch stops being a
/// prospective tap and becomes a drag. 16, raised from a first guess of 8, which classified
/// real taps as travel: a fingertip rolls several pixels as it presses and the CST816T adds its
/// own jitter, so taps were dropped or turned into 1 px scrolls. Still below any swipe threshold.
pub const DRAG_SLOP: i32 = 16;

/// How long a contact must rest -- within [`DRAG_SLOP`] of where it landed -- before its
/// release counts as a TAP. A brush or a jittered graze is shorter than this and is dropped
/// rather than fired as a press: the deliberateness filter. A small fraction of a second, so
/// an ordinary tap (~100 ms) clears it comfortably while an accidental flick does not.
pub const TAP_MIN_HOLD_US: u64 = 50_000;

/// How long a button wears its pressed look after activation: a brief acknowledgement so a
/// tap reads as landed even when the action behind it (opening a card, starting a take) is
/// slow to change anything else. See [`Ui::touch`].
pub const ACTIVATE_FLASH_US: u64 = 120_000;

/// Longest label the toolkit renders. Labels are truncated to their widget anyway; this bounds
/// the work a single draw does.
pub const TEXT_MAX: usize = 64;

/// How long a rotation takes to animate: long enough to read as a turn rather than a glitch,
/// short enough not to feel like waiting. Input is still collected during it; only the drawing
/// is given over to the animation.
pub const ROTATE_MS: u32 = 280;

/// How long a page transition takes. Shorter than a rotation: a rotation re-orients the whole
/// interface and wants to be followed, while a page change is a step through a structure the
/// user already has in mind, and waiting for it is what makes an interface feel slow.
pub const PAGE_MOVE_MS: u32 = 180;

/// A handle to a widget in its `Ui`'s arena. Stale after the widget is destroyed: the arena
/// answers `None` for it, and a handle from a torn-down page cannot reach another page's widget
/// except by index reuse, which is why handlers conventionally navigate last and touch nothing
/// afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetId(u8);

/// Whether a window's content may exceed its frame and be moved through it, per axis. OR-able.
pub mod scroll {
        pub const NONE: u8 = 0;
        pub const VERTICAL: u8 = 1 << 0;
        pub const HORIZONTAL: u8 = 1 << 1;
}

/// A selectable list as reusable data: the rows of a file picker or menu, generated once
/// instead of hand-written per screen. Emits a `pub static` slice of row [`Desc`]s ready to
/// drop into a scrolling window's `children`; each row `i` carries tag `tag_base + i` (address
/// it later with [`Ui::set_list_text`]/[`Ui::fill_list`]) and emits the app event the caller's
/// `select` maps its index to -- so the toolkit stays event-agnostic. Sizing is the caller's
/// (`min_size`: a height for a vertical list, a width for a horizontal one), and the list runs
/// along whichever [`Axis`] its window carries, so one definition serves portrait and landscape.
/// Optional `prev:`/`next:` buttons (each a `&'static Desc`) bracket the rows -- `prev` before the
/// first, `next` after the last -- for a paged list: show or hide them per page with
/// [`Ui::set_visible`] (a hidden one collapses out on the next [`Ui::relayout`]). An optional
/// `back:` button is appended last, for a layout that scrolls the way back in with the list rather
/// than pinning it. Supply them in this order: `prev`, `next`, `back`.
///
/// ```ignore
/// light_ui::file_list! {
///         FILE_ROWS,
///         event: AppEvent,
///         tag_base: TAG_ROW_BASE,
///         min_size: (0, 56),
///         select: |i| AppEvent::Pick(i),
///         indices: [0, 1, 2, 3, 4, 5, 6, 7],
///         prev: &BTN_PREV,
///         next: &BTN_NEXT,
///         back: &BTN_BACK,
/// }
/// // static FILE_ROWS: &[&Desc<AppEvent>]  --  Desc::frame().linear(gap).children(FILE_ROWS)
/// ```
#[macro_export]
macro_rules! file_list {
        (
                $name:ident,
                event: $ev:ty,
                tag_base: $base:expr,
                min_size: ($w:expr, $h:expr),
                select: |$i:ident| $emit:expr,
                indices: [$($idx:literal),* $(,)?]
                $(, prev: $prev:expr)?
                $(, next: $next:expr)?
                $(, back: $back:expr)?
                $(,)?
        ) => {
                pub static $name: &[&$crate::Desc<$ev>] = &[
                        $( $prev, )?
                        $(
                                &$crate::Desc::button("-")
                                        .emit({ let $i: u8 = $idx; $emit })
                                        .tag(($base) + $idx)
                                        .min_size($w, $h),
                        )*
                        $( $next, )?
                        $( $back, )?
                ];
        };
}

/// How a window arranges its children when layout is (re-)run. Recorded on the window so a
/// rotation or resize can re-apply it without the application being told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
        /// Children placed by hand; relayout leaves them where they are.
        None,
        /// Equal-height rows, one per visible child, `gap` pixels apart. Pins the vertical
        /// axis regardless of the tree's [`Axis`].
        Stack { gap: u8 },
        /// Equal-width columns, one per visible child, `gap` pixels apart: the horizontal
        /// counterpart of `Stack`. Pins the horizontal axis regardless of the tree's [`Axis`].
        Row { gap: u8 },
        /// A line of children along whichever axis the tree carries
        /// ([`Ui::set_layout_axis`]): `Stack` when the tree is [`Axis::Vertical`], `Row` when
        /// [`Axis::Horizontal`]. The window keeps this generic recording, so flipping the
        /// tree axis re-lays it the other way without the descriptor changing. This is how a
        /// tree is authored once and instantiated portrait or landscape from the outside.
        Linear { gap: u8 },
}

/// The axis a generic [`Layout::Linear`] runs along: a whole tree's primary layout
/// direction, set with [`Ui::set_layout_axis`]. `Vertical` -- the default -- stacks children
/// top to bottom; `Horizontal` lays them side by side. `Stack` and `Row` ignore it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
        Vertical,
        Horizontal,
}

/// The edge a child page enters from as it opens: press a button and the incoming page
/// slides in from this edge, the outgoing one retreating the same way; back navigation runs
/// the reverse. Named by the origin edge, which is how it reads at the call site
/// (`FromBottom` rises up into view). A whole tree can be pointed one way with
/// [`Ui::set_default_descent`], and a single page can override it with [`Page::descend`].
/// Left unset everywhere, a page keeps the historical layout-derived flow (a `Row` page
/// enters from the bottom, everything else from the right) -- see [`Ui`]'s navigation section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Descent {
        FromTop,
        FromBottom,
        FromLeft,
        FromRight,
}

impl Descent {
        /// Whether this descent runs the vertical axis (`FromTop`/`FromBottom`) rather than
        /// the horizontal one.
        pub const fn vertical(self) -> bool {
                matches!(self, Descent::FromTop | Descent::FromBottom)
        }

        /// The signed unit of this descent's axis in the transition engine's logical
        /// convention (see `Ui::page_step`): `FromBottom`/`FromLeft` are `+1`,
        /// `FromTop`/`FromRight` are `-1`. The vertical and horizontal axes carry different
        /// senses because the engine's cover-forward path runs logical `+y` while its reveal
        /// path runs logical `-x`; the two forward defaults, `Row`->`FromBottom` and
        /// `Stack`->`FromRight`, fall out as `+1` and `-1` respectively, which is what keeps
        /// the historical flow byte-for-byte.
        const fn axis_sign(self) -> i32 {
                match self {
                        Descent::FromBottom | Descent::FromLeft => 1,
                        Descent::FromTop | Descent::FromRight => -1,
                }
        }
}

/// Where a button takes the interface when activated, after emitting its event.
#[derive(Clone, Copy)]
pub enum Nav<A: 'static> {
        Stay,
        To(&'static Page<A>),
        Back,
}

impl<A> core::fmt::Debug for Nav<A> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                match self {
                        Nav::Stay => f.write_str("Stay"),
                        Nav::To(p) => write!(f, "To({:p})", *p),
                        Nav::Back => f.write_str("Back"),
                }
        }
}

#[derive(Clone, Copy, Debug)]
pub struct Window {
        pub title: Option<&'static str>,
        /// Gap between the frame and the content area the stack layout divides.
        pub padding: u8,
        pub border: bool,
        /// 0 for a square frame; otherwise the frame is rounded and content keeps clear of the
        /// curve -- see [`Ui::set_corner_radius`]. Resolved from the theme at creation --
        /// [`Theme::screen_radius`] for the outer window, [`Theme::radius`] inside -- unless
        /// the descriptor names its own.
        pub corner_radius: u8,
        pub layout: Layout,
        /// `scroll::*` flags. While any is set, painting and hit-testing clip the children to the
        /// viewport, which is what lets content live partly outside the frame.
        pub scroll: u8,
        /// How far the content is moved, per axis, >= 0: `scroll_y == 20` means the content has
        /// moved UP by 20 and its first 20 rows are above the viewport. Rects stay absolute --
        /// scrolling shifts every rect under the window -- so these exist to be clamped and
        /// reasoned about, not as a second coordinate space.
        pub scroll_x: i32,
        pub scroll_y: i32,
        /// Extent of the laid-out content from the viewport origin at scroll 0. Maintained by the
        /// stack layout; measured from the children on demand for a hand-placed window.
        pub content_w: i32,
        pub content_h: i32,
        /// A status symbol drawn INLINE in the title bar -- a recording light, a play head --
        /// `Some((shape, lit, col))` centres it on title character cell `col`, so the caller
        /// leaves a blank there for it (e.g. the space in `REC 0:12`). `lit` false is the dark
        /// phase of a flash; `None` is off. Because it sits on a space the title already holds,
        /// blinking never reflows the text. See [`Ui::set_indicator`] and [`IndicatorShape`].
        pub indicator: Option<(IndicatorShape, bool, u16)>,
        /// An optional SECOND title row: `None` is a one-row header, `Some` makes the header
        /// band two rows and carries the runtime text of the lower one (empty draws blank).
        /// A narrow bar splits a status too long for one row onto it. See [`Ui::set_subtitle`].
        pub subtitle: Option<TextSlot>,
}

/// The shape of a title-bar [`indicator`](Window::indicator): a filled `Dot` (a recording
/// light) or a right-pointing `Play` triangle (a play head). Both paint in
/// [`Theme::indicator`](crate::theme::Theme::indicator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndicatorShape {
        Dot,
        Play,
}

#[derive(Clone, Copy, Debug)]
pub struct Button<A: 'static> {
        pub label: &'static str,
        /// Published by the caller when the button is activated.
        pub emit: Option<A>,
        pub nav: Nav<A>,
        /// Set by the stack layout on a row that sits flush against a rounded window, so the row
        /// follows the container's curve; `corners` names only the ones that touch it.
        pub corner_radius: u8,
        pub corners: u8,
        /// The button's SURFACE when unfocused: a gradient wash under the outline in place
        /// of the flat background. The focused fill is the UI-level style instead.
        pub shade: Option<Shade>,
}

#[derive(Clone, Copy, Debug)]
pub struct Label {
        pub text: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub enum Kind<A: 'static> {
        Window(Window),
        Button(Button<A>),
        Label(Label),
}

/// A vertical shade across a surface: `from` at the top edge, `to` at the bottom. An
/// RGB565 idea -- a mono canvas renders any shade as a solid fill -- carried by the
/// focused widget's fill ([`Ui::set_focus_shade`]) and by any button built
/// [`Desc::shaded`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shade {
        pub from: u16,
        pub to: u16,
}

/// Owned widget text: a small copy the application rewrites at runtime
/// ([`Ui::set_text`]) where the descriptor's `&'static str` cannot carry a formatted
/// value -- an elapsed time, a file name. Empty means unset: the static label shows.
#[derive(Clone, Copy, Debug)]
pub struct TextSlot {
        buf: [u8; Self::CAP],
        len: u8,
}

impl TextSlot {
        /// Sized for a row on a small panel; longer text truncates at a char boundary.
        pub const CAP: usize = 31;
        const EMPTY: Self = Self { buf: [0; Self::CAP], len: 0 };

        fn set(&mut self, s: &str) {
                let mut take = s.len().min(Self::CAP);
                while !s.is_char_boundary(take) {
                        take -= 1;
                }
                self.buf[..take].copy_from_slice(&s.as_bytes()[..take]);
                self.len = take as u8;
        }

        fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..usize::from(self.len)]).unwrap_or("")
        }
}

#[derive(Clone, Copy, Debug)]
pub struct Widget<A: 'static> {
        pub kind: Kind<A>,
        /// Runtime text override -- see [`Ui::set_text`].
        text: TextSlot,
        pub rect: Rect,
        pub visible: bool,
        pub focusable: bool,
        pub enabled: bool,
        /// Extra rows below `rect.y1` that count as a hit but are never drawn: the strip beneath
        /// a row laid out flush to a rounded container -- padding, border, safe inset -- has no
        /// widget of its own and reads as part of the row. A hit extension rather than a taller
        /// rect: what is wrong there is the target, not the picture.
        pub hit_slop_y1: i32,
        /// Bounds on what auto-layout may make of this widget, 0 for unconstrained. A minimum is
        /// what makes a stack OVERFLOW rather than shrink its rows without limit; min wins over
        /// max, on the grounds that a widget too small to use is the worse failure.
        pub min_w: i32,
        pub min_h: i32,
        pub max_w: i32,
        pub max_h: i32,
        /// Stretches to take the surplus in a horizontal layout, so pinned siblings can flank it.
        pub grow: bool,
        /// An application-chosen mark, for finding a widget again after a build; 0 = untagged.
        pub tag: u8,
        parent: Option<WidgetId>,
        next_sibling: Option<WidgetId>,
        first_child: Option<WidgetId>,
}

impl<A: Copy> Widget<A> {
        pub fn window(&self) -> Option<&Window> {
                match &self.kind {
                        Kind::Window(w) => Some(w),
                        _ => None,
                }
        }
        pub fn button(&self) -> Option<&Button<A>> {
                match &self.kind {
                        Kind::Button(b) => Some(b),
                        _ => None,
                }
        }
        fn window_mut(&mut self) -> Option<&mut Window> {
                match &mut self.kind {
                        Kind::Window(w) => Some(w),
                        _ => None,
                }
        }
        fn is_scrolling_window(&self) -> bool {
                matches!(&self.kind, Kind::Window(w) if w.scroll != 0)
        }
}

// --- declarative definitions ----------------------------------------------------------------

/// A page: a descriptor tree plus its place in the interface's structure.
///
/// PARENT, NOT HISTORY. `parent` describes where a page sits, the way a directory knows its
/// containing directory, so back goes somewhere predictable however the user arrived and costs
/// no stack -- a history list would have to be bounded, and the bound would be reached by
/// exactly the aimless wandering it exists to serve. Pages are `static` and reference each
/// other across a cycle (a child names its parent, the parent's button names the child).
pub struct Page<A: 'static> {
        pub content: &'static Desc<A>,
        pub parent: Option<&'static Page<A>>,
        /// This page's own navigation-descent direction, overriding the tree default and the
        /// layout-derived fallback. `None` defers to [`Ui::set_default_descent`], then to the
        /// layout seed. See [`Descent`].
        pub descend: Option<Descent>,
}

impl<A> Page<A> {
        pub const fn new(content: &'static Desc<A>, parent: Option<&'static Page<A>>) -> Self {
                Self { content, parent, descend: None }
        }

        /// Pin the direction this page's own arrival and departure flow, overriding the tree
        /// default. The page being entered decides going forward, the page being left coming
        /// back, so a page's opening and closing always mirror.
        pub const fn descend(mut self, descend: Descent) -> Self {
                self.descend = Some(descend);
                self
        }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DescKind {
        Window,
        Button,
        Label,
}

/// A widget tree as data, realised by [`Ui::build`]. The PARENT lists its children in the order
/// they appear -- sibling order is paint order, focus order and the top-to-bottom order of a
/// stack at once, and that is a fact of the source, not of linking. Descriptors are `const`,
/// hold no state, and live in flash: one can build the same subtree into two contexts.
///
/// ```ignore
/// static BTN_OK: Desc<AppEvent> = Desc::button("OK").emit(AppEvent::Ok);
/// static MAIN: Desc<AppEvent> = Desc::window("Title").rounded(8).stack(2).children(&[&BTN_OK]);
/// ```
pub struct Desc<A: 'static> {
        kind: DescKind,
        text: Option<&'static str>,
        emit: Option<A>,
        nav: Nav<A>,
        shade: Option<Shade>,
        corner_radius: u8,
        layout: Layout,
        scroll: u8,
        min_w: i32,
        min_h: i32,
        max_w: i32,
        max_h: i32,
        /// Only meaningful for a hand-placed widget under a `Layout::None` parent; a root's rect
        /// comes from the canvas.
        rect: Option<Rect>,
        tag: u8,
        /// A window only: reserve a second title row (see [`Window::subtitle`]).
        subtitle: bool,
        /// Take the surplus width in a horizontal layout, so pinned siblings can flank it (see
        /// [`Desc::grow`]).
        grow: bool,
        children: &'static [&'static Desc<A>],
}

impl<A: Copy> Desc<A> {
        const fn base(kind: DescKind, text: Option<&'static str>) -> Self {
                Self { kind, text, emit: None, nav: Nav::Stay, corner_radius: 0, layout: Layout::None, scroll: scroll::NONE, min_w: 0, min_h: 0, max_w: 0, max_h: 0, rect: None, tag: 0, subtitle: false, grow: false, children: &[], shade: None }
        }
        pub const fn window(title: &'static str) -> Self {
                Self::base(DescKind::Window, Some(title))
        }
        /// An untitled frame.
        pub const fn frame() -> Self {
                Self::base(DescKind::Window, None)
        }
        /// Give a titled window a second title row (see [`Window::subtitle`]), filled at
        /// runtime with [`Ui::set_subtitle`] -- for a narrow bar that splits a long status
        /// across two rows. The header band is two rows whether or not the lower one has text,
        /// so the content area never jumps.
        pub const fn subtitle(mut self) -> Self {
                self.subtitle = true;
                self
        }
        pub const fn button(label: &'static str) -> Self {
                Self::base(DescKind::Button, Some(label))
        }
        pub const fn label(text: &'static str) -> Self {
                Self::base(DescKind::Label, Some(text))
        }
        pub const fn emit(mut self, event: A) -> Self {
                self.emit = Some(event);
                self
        }
        pub const fn navigate(mut self, page: &'static Page<A>) -> Self {
                self.nav = Nav::To(page);
                self
        }
        pub const fn back(mut self) -> Self {
                self.nav = Nav::Back;
                self
        }
        /// Name a window's own corner radius, overriding the theme's -- which otherwise
        /// supplies [`Theme::screen_radius`] for the outer window and [`Theme::radius`]
        /// inside. 0 (the unset value) means themed, so a square frame under a rounded
        /// theme is expressed in the theme, not here.
        pub const fn rounded(mut self, radius: u8) -> Self {
                self.corner_radius = radius;
                self
        }
        /// A vertical gradient surface for a button, `from` at the top: shown while
        /// unfocused, under the outline, in place of the flat background.
        pub const fn shaded(mut self, from: u16, to: u16) -> Self {
                self.shade = Some(Shade { from, to });
                self
        }
        pub const fn stack(mut self, gap: u8) -> Self {
                self.layout = Layout::Stack { gap };
                self
        }
        pub const fn row(mut self, gap: u8) -> Self {
                self.layout = Layout::Row { gap };
                self
        }
        /// Lay children along the tree's [`Axis`] ([`Ui::set_layout_axis`]) rather than a
        /// fixed one -- a `Stack` in a vertical tree, a `Row` in a horizontal one. Authors a
        /// window once and lets the board choose portrait or landscape. See [`Layout::Linear`].
        pub const fn linear(mut self, gap: u8) -> Self {
                self.layout = Layout::Linear { gap };
                self
        }
        pub const fn scroll(mut self, flags: u8) -> Self {
                self.scroll = flags;
                self
        }
        pub const fn min_size(mut self, w: i32, h: i32) -> Self {
                self.min_w = w;
                self.min_h = h;
                self
        }
        pub const fn max_size(mut self, w: i32, h: i32) -> Self {
                self.max_w = w;
                self.max_h = h;
                self
        }
        pub const fn rect(mut self, x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
                self.rect = Some(Rect::new(x0, y0, x1, y1));
                self
        }
        pub const fn tag(mut self, tag: u8) -> Self {
                self.tag = tag;
                self
        }
        /// Claim the leftover space in a horizontal layout ([`Layout::Row`], or [`Layout::Linear`]
        /// in a horizontal tree). Without any grow child, the layout splits the width evenly and
        /// the last column absorbs the remainder -- so a fixed-width button can only sit at the
        /// end. Mark the child that should stretch, and the others keep their pinned widths
        /// wherever they are, letting pinned buttons flank a stretching one on both sides (the
        /// wide recordings list's paging buttons bracket the scrolling strip this way). Leftover
        /// splits evenly between multiple grow children. Honored by the horizontal layout only.
        pub const fn grow(mut self) -> Self {
                self.grow = true;
                self
        }
        pub const fn children(mut self, children: &'static [&'static Desc<A>]) -> Self {
                self.children = children;
                self
        }
}

// --- errors and outcomes ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
        /// The arena is full: the tree needs more widgets than the `Ui` was sized for.
        Full,
        /// A page whose descriptor could not be built is not shown; the previous page is gone.
        NoContent,
}

/// What [`Ui::touch`] did with the sample it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch<A> {
        None,
        /// A finger is down and the touch is undecided.
        Pending,
        /// Drag-scrolling, from the sample it engaged on. The caller should tell its gesture
        /// tracker the movement was consumed, or the release also classifies as a swipe.
        Drag,
        /// The release of a drag.
        DragEnd,
        /// The release of a tap: whether it landed on a widget, and what that widget emitted.
        Tap { hit: bool, emitted: Option<A> },
}

/// A swipe's direction in the LOGICAL frame -- the one the user is looking at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwipeDir {
        Up,
        Down,
        Left,
        Right,
}

// --- corner geometry ---------------------------------------------------------------------------
//
// There are two ways to keep something inside a corner arc, and which is right depends on the
// shape of the thing. A short string can simply start further RIGHT; a full-width row cannot,
// so it has to start further DOWN. The two helpers are the same circle solved for each axis,
// and using the wrong one makes a rounded frame either clip its content or waste a band the
// width of the radius.

fn isqrt(n: u32) -> u32 {
        if n == 0 {
                return 0;
        }
        let mut x = n;
        let mut y = (x + 1) / 2;
        while y < x {
                x = y;
                y = (x + n / x) / 2;
        }
        x
}

/// How far the arc still intrudes horizontally `dy` rows above its centre: `r - sqrt(r² - dy²)`.
/// What lets a title sit at the very TOP of a rounded frame, pushed sideways by as much as the
/// curve reaches in at its own row. Truncation errs on the safe side.
fn corner_indent(radius: u8, dy: i32) -> i32 {
        let r = i32::from(radius);
        if r <= 0 || dy <= 0 {
                return 0;
        }
        if dy >= r {
                return r;
        }
        r - isqrt((r * r - dy * dy) as u32) as i32
}

/// The mirror, for content that must span the full width: given the horizontal inset such
/// content sits at, how far down before the arc has come in that far: `sqrt(ix (2r - ix))`.
fn corner_drop(radius: u8, inset_x: i32) -> i32 {
        let r = i32::from(radius);
        let ix = inset_x;
        if r <= 0 || ix <= 0 || ix >= r {
                return 0;
        }
        r - isqrt((ix * (2 * r - ix)) as u32) as i32
}

/// What one pass of an animation did.
enum Step {
        Drew,
        Waiting,
        Finished,
}

fn quadrant(r: Rotation) -> i32 {
        match r {
                Rotation::R0 => 0,
                Rotation::R90 => 1,
                Rotation::R180 => 2,
                Rotation::R270 => 3,
        }
}

/// The shortest signed turn between two quadrants, in degrees: -90, 0, 90 or 180. Going the
/// long way round would animate three quarters of a turn to reach a neighbour.
fn rotation_delta_degrees(from: Rotation, to: Rotation) -> i32 {
        let mut q = (quadrant(to) + 4 - quadrant(from)) % 4;
        if q == 3 {
                q = -1;
        }
        q * 90
}

fn rect_empty(r: &Rect) -> bool {
        r.x1 < r.x0 || r.y1 < r.y0
}

fn rect_contains(r: &Rect, x: i32, y: i32) -> bool {
        x >= r.x0 && x <= r.x1 && y >= r.y0 && y <= r.y1
}

/// Shrinks `r` to its intersection with `clip`; false when nothing survives.
fn rect_intersect(r: &mut Rect, clip: &Rect) -> bool {
        r.x0 = r.x0.max(clip.x0);
        r.y0 = r.y0.max(clip.y0);
        r.x1 = r.x1.min(clip.x1);
        r.y1 = r.y1.min(clip.y1);
        !rect_empty(r)
}

// --- the context ---------------------------------------------------------------------------------

/// A widget tree, its focus and touch state, and which parts of it changed. Owns nothing about
/// the display: it draws through whatever frame layer it is handed, contributing the tree and
/// its dirty regions. `N` is the arena size -- the most widgets one page can have.
pub struct Ui<A: 'static, const N: usize> {
        widgets: Vec<Option<Widget<A>>, N>,
        root: Option<WidgetId>,
        focused: Option<WidgetId>,
        /// Set by any invalidation, cleared once a repaint has been pushed: "is there anything to
        /// draw", which decides whether to open a frame at all.
        dirty: bool,
        /// Regions to hand the frame layer at the next repaint, collapsing to the whole canvas
        /// past the list's capacity -- it can only push more than needed, never less.
        pending: Vec<Rect, MAX_REGIONS>,
        pending_all: bool,
        /// The canvas as the toolkit last saw it: logical size and the panel transform.
        width: i32,
        height: i32,
        transform: Transform,
        /// Each role's font cell, for layout and truncation; fonts are fixed-pitch. Indexed by
        /// [`FontRole`], set at [`set_style`](Self::set_style).
        cell_w: [i32; FontRole::COUNT],
        cell_h: [i32; FontRole::COUNT],
        /// Pixels kept clear on every edge, for glass that does not show the whole grid. Uniform
        /// rather than per-edge because the interface rotates while the corners are fixed in the
        /// panel's frame: a uniform inset is the only value invariant under rotation.
        safe_inset: i32,
        /// The look-and-feel every element paints with -- see [`theme`]. Starts as the
        /// pre-theme monochrome look; an application installs its own with
        /// [`set_theme`](Self::set_theme), typically parsed from an embedded LTH blob.
        theme: Theme,
        // --- touch tracking, owned by `touch()`; everything logical ---
        touch_down: bool,
        touch_dragging: bool,
        /// The scrollable window captured when the drag engaged: the drag stays with it even if
        /// the finger wanders off, as every scrolling surface behaves.
        drag_window: Option<WidgetId>,
        touch_start: (i32, i32),
        touch_last: (i32, i32),
        /// When the contact landed, and whether it has EVER strayed beyond the slop since --
        /// a tap must both rest long enough (see [`TAP_MIN_HOLD_US`]) and never have wandered.
        touch_start_us: u64,
        touch_moved: bool,
        pub drag_slop: i32,
        /// The button wearing the just-activated flash and when it lapses (see
        /// [`ACTIVATE_FLASH_US`]): instant feedback on a tap, independent of focus, cleared by
        /// the render loop when the deadline passes or the widget goes away.
        flash: Option<(WidgetId, u64)>,
        // --- navigation ---
        page: Option<&'static Page<A>>,
        /// Where back goes when it is not the current page's parent; set only by
        /// `navigate_returning`, cleared by every ordinary navigation.
        return_page: Option<&'static Page<A>>,
        /// The direction child pages open across this whole tree, unless a page pins its own
        /// with [`Page::descend`]. `None` -- the default -- keeps the historical
        /// layout-derived flow: a `Row` page enters from the bottom ([`Descent::FromBottom`]),
        /// everything else from the right ([`Descent::FromRight`]). An app or board sets this to point the whole interface
        /// one way, and may change it live, e.g. from an orientation sensor. See [`Descent`].
        default_descent: Option<Descent>,
        /// Whether [`set_default_descent`](Self::set_default_descent) was called: an explicit
        /// application choice a theme's descent seed must not overwrite.
        default_descent_explicit: bool,
        /// The axis every [`Layout::Linear`] window runs along. `Vertical` by default; a
        /// landscape tree sets `Horizontal`. `Stack`/`Row` windows ignore it. See [`Axis`].
        layout_axis: Axis,
        // --- the rotation animation, driven from `render` ---
        //   while active, frames show the pre-rotation image turning rather than the widget
        // tree; the real rotation is applied once, on the final step. The image is captured
        // into the display's back buffer at the first step (`Display::freeze`), which is why the
        // animation needs the display and `set_rotation` does not
        rotating: bool,
        rotate_started: bool,
        rotate_target: Rotation,
        /// Total turn in degrees, signed: the shortest route between the two quadrants.
        rotate_degrees: i32,
        rotate_start_us: u64,
        pub rotate_ms: u32,
        /// A rotation asked for while a transition was running, applied once it finishes. Both
        /// animations want the back buffer and the frame, so they cannot overlap; deferring
        /// rather than dropping either means the transition is seen through and the device
        /// still ends up the right way up.
        rotate_deferred: Option<Rotation>,
        // --- the page transition, likewise ---
        //   each frame draws the incoming page and then slides the image captured before the
        // transfer off it, so the outgoing page appears to move away and reveal the new one.
        // Only the outgoing image is stored: the incoming page is the live tree, redrawn each
        // step, which is why this costs one buffer and not two
        page_moving: bool,
        page_move_started: bool,
        /// Which way the outgoing image leaves: forward pushes it toward logical -x so the new
        /// page arrives from the right; a return sends it the other way.
        page_move_back: bool,
        /// A page laid out as a `Row` runs sideways, so its transition runs the OTHER axis:
        /// entering one drops it in from the top, leaving one reveals the parent upward.
        /// The child page of the pair decides -- the incoming page going forward, the
        /// outgoing one coming back -- so one page's arrival and departure mirror.
        page_move_descent: Descent,
        /// No back buffer to capture the outgoing page into: the incoming tree slides in
        /// OVER the old image, which survives in the live buffer wherever a step has not yet
        /// overdrawn it. Chosen at the first step, from what the display can hold.
        page_move_over: bool,
        /// Physical unit direction, derived from the logical one at the first step -- or the
        /// LOGICAL sign of the incoming page's offset in the slide-over mode, which draws
        /// through the canvas transform and never leaves logical space.
        page_move_dx: i32,
        page_move_dy: i32,
        /// The travel of the last DRAWN capture-mode step: the incoming page is static in
        /// that mode, so each step paints only the band uncovered since this mark.
        page_move_travel: i32,
        /// How far it travels to leave: the buffer's extent along that axis.
        page_move_span: i32,
        page_move_start_us: u64,
        pub page_move_ms: u32,
}

impl<A: Copy, const N: usize> Ui<A, N> {
        /// An empty toolkit. `const`, so it can be a `static` initialised in place -- the arena is
        /// the biggest object in an application, and a firmware's core 0 stack is 4 KB. Call
        /// [`set_font`](Self::set_font) and [`fit`](Self::fit) before the first build.
        pub const fn new() -> Self {
                Self {
                        widgets: Vec::new(),
                        root: None,
                        focused: None,
                        dirty: false,
                        pending: Vec::new(),
                        pending_all: false,
                        width: 0,
                        height: 0,
                        transform: Transform::IDENTITY,
                        cell_w: [0; FontRole::COUNT],
                        cell_h: [0; FontRole::COUNT],
                        safe_inset: 0,
                        theme: Theme::DEFAULT,
                        touch_down: false,
                        touch_dragging: false,
                        drag_window: None,
                        touch_start: (0, 0),
                        touch_last: (0, 0),
                        touch_start_us: 0,
                        touch_moved: false,
                        drag_slop: DRAG_SLOP,
                        flash: None,
                        page: None,
                        return_page: None,
                        default_descent: None,
                        default_descent_explicit: false,
                        layout_axis: Axis::Vertical,
                        rotating: false,
                        rotate_started: false,
                        rotate_target: Rotation::R0,
                        rotate_degrees: 0,
                        rotate_start_us: 0,
                        rotate_ms: ROTATE_MS,
                        rotate_deferred: None,
                        page_moving: false,
                        page_move_started: false,
                        page_move_back: false,
                        page_move_descent: Descent::FromRight,
                        page_move_over: false,
                        page_move_dx: 0,
                        page_move_dy: 0,
                        page_move_travel: 0,
                        page_move_span: 0,
                        page_move_start_us: 0,
                        page_move_ms: PAGE_MOVE_MS,
                }
        }

        pub fn is_animating(&self) -> bool {
                //   an active press flash keeps the loop rendering so the deadline is noticed and
                // the button reverts on its own
                self.rotating || self.page_moving || self.flash.is_some()
        }

        /// The union of everything invalidated since the last repaint, in LOGICAL
        /// coordinates -- what the next `render` will paint, known BEFORE it paints. `None`
        /// when the tree is clean or the WHOLE canvas is pending (`is_dirty` distinguishes
        /// the two). A single-buffered scanned panel schedules its draw against this: the
        /// bounds against the beam.
        pub fn dirty_bounds(&self) -> Option<Rect> {
                if self.pending_all {
                        return None;
                }
                let mut it = self.pending.iter();
                let first = *it.next()?;
                Some(it.fold(first, |a, r| Rect::new(a.x0.min(r.x0), a.y0.min(r.y0), a.x1.max(r.x1), a.y1.max(r.y1))))
        }

        /// The font's cell metrics, which layout and truncation need; fonts are fixed-pitch.
        /// Bind the look: the theme (its colours and metrics, via [`set_theme`](Self::set_theme))
        /// and each role's font cell metrics. The glyphs themselves stay transient -- pass the
        /// SAME [`Style`] to [`render`](Self::render). Re-lays-out, since a metric or the theme's
        /// radius may have moved.
        pub fn set_style(&mut self, style: &Style<'_>) {
                self.set_theme(style.theme);
                for role in FontRole::ALL {
                        let font = style.fonts.font(role);
                        self.cell_w[role as usize] = i32::from(font.cell_width());
                        self.cell_h[role as usize] = i32::from(font.cell_height());
                }
                self.relayout();
        }

        /// A role's font cell width, in logical pixels.
        fn role_cw(&self, role: FontRole) -> i32 {
                self.cell_w[role as usize]
        }

        /// A role's font cell height, in logical pixels.
        fn role_ch(&self, role: FontRole) -> i32 {
                self.cell_h[role as usize]
        }

        /// Take the canvas geometry from the layer: its logical size and transform. Re-lays-out and
        /// repaints everything when the size changed, which is what a rotation does.
        pub fn fit(&mut self, layer: &FrameLayer) {
                let (w, h) = layer.logical_size();
                let (w, h) = (i32::from(w), i32::from(h));
                let changed = w != self.width || h != self.height;
                self.width = w;
                self.height = h;
                self.transform = layer.transform();
                if changed {
                        self.relayout();
                        self.invalidate_all();
                }
        }

        pub fn get(&self, id: WidgetId) -> Option<&Widget<A>> {
                self.widgets.get(usize::from(id.0)).and_then(|s| s.as_ref())
        }

        fn w(&self, id: WidgetId) -> &Widget<A> {
                self.get(id).expect("a live widget")
        }

        fn w_mut(&mut self, id: WidgetId) -> &mut Widget<A> {
                self.widgets[usize::from(id.0)].as_mut().expect("a live widget")
        }

        pub fn root(&self) -> Option<WidgetId> {
                self.root
        }

        pub fn focused(&self) -> Option<WidgetId> {
                self.focused
        }

        pub fn page(&self) -> Option<&'static Page<A>> {
                self.page
        }

        /// The first live widget carrying `tag`.
        pub fn find(&self, tag: u8) -> Option<WidgetId> {
                self.widgets.iter().enumerate().find_map(|(i, s)| match s {
                        Some(w) if w.tag == tag && tag != 0 => Some(WidgetId(i as u8)),
                        _ => None,
                })
        }

        pub fn is_dirty(&self) -> bool {
                self.dirty
        }

        pub fn logical_size(&self) -> (i32, i32) {
                (self.width, self.height)
        }

        // --- tree ---

        fn alloc(&mut self, w: Widget<A>) -> Result<WidgetId, Error> {
                if let Some(i) = self.widgets.iter().position(|s| s.is_none()) {
                        self.widgets[i] = Some(w);
                        return Ok(WidgetId(i as u8));
                }
                if self.widgets.len() >= 255 {
                        return Err(Error::Full);
                }
                self.widgets.push(Some(w)).map_err(|_| Error::Full)?;
                Ok(WidgetId((self.widgets.len() - 1) as u8))
        }

        fn add(&mut self, parent: Option<WidgetId>, kind: Kind<A>, rect: Rect, focusable: bool) -> Result<WidgetId, Error> {
                let id = self.alloc(Widget { kind, text: TextSlot::EMPTY, rect, visible: true, focusable, enabled: true, hit_slop_y1: 0, min_w: 0, min_h: 0, max_w: 0, max_h: 0, grow: false, tag: 0, parent, next_sibling: None, first_child: None })?;
                match parent {
                        None => {
                                if let Some(old) = self.root {
                                        warn!("ui already has a root widget; replacing it");
                                        self.destroy(old);
                                }
                                self.root = Some(id);
                        }
                        Some(p) => {
                                // appended: sibling order is paint order and focus order, and
                                // neither reads correctly reversed
                                match self.w(p).first_child {
                                        None => self.w_mut(p).first_child = Some(id),
                                        Some(mut c) => {
                                                while let Some(n) = self.w(c).next_sibling {
                                                        c = n;
                                                }
                                                self.w_mut(c).next_sibling = Some(id);
                                        }
                                }
                        }
                }
                Ok(id)
        }

        /// `parent` may be `None` for the root. `rect` is absolute.
        ///
        /// The corner radius comes from the theme: the OUTER window wears the glass's own
        /// curvature ([`Theme::screen_radius`], zero on square screens) so the frame
        /// parallels the screen edge, and every interior container the house radius.
        pub fn create_window(&mut self, parent: Option<WidgetId>, rect: Rect, title: Option<&'static str>, subtitle: bool) -> Result<WidgetId, Error> {
                let corner_radius = if parent.is_none() { self.theme.screen_radius } else { self.theme.radius };
                let subtitle = if subtitle { Some(TextSlot::EMPTY) } else { None };
                let win = Window { title, padding: 2, border: true, corner_radius, layout: Layout::None, scroll: scroll::NONE, scroll_x: 0, scroll_y: 0, content_w: 0, content_h: 0, indicator: None, subtitle };
                self.add(parent, Kind::Window(win), rect, false)
        }

        pub fn create_button(&mut self, parent: Option<WidgetId>, rect: Rect, label: &'static str, emit: Option<A>, nav: Nav<A>) -> Result<WidgetId, Error> {
                let id = self.add(parent, Kind::Button(Button { label, emit, nav, corner_radius: 0, corners: light_draw::corner::NONE, shade: None }), rect, true)?;
                // the first focusable widget takes focus, so a two-button rig always has
                // somewhere to start cycling from
                if self.focused.is_none() {
                        self.focused = Some(id);
                }
                Ok(id)
        }

        pub fn create_label(&mut self, parent: Option<WidgetId>, rect: Rect, text: &'static str) -> Result<WidgetId, Error> {
                self.add(parent, Kind::Label(Label { text }), rect, false)
        }

        fn build_desc(&mut self, parent: Option<WidgetId>, desc: &'static Desc<A>) -> Result<WidgetId, Error> {
                let rect = desc.rect.unwrap_or(Rect::new(0, 0, 0, 0));
                let id = match desc.kind {
                        DescKind::Window => {
                                let id = self.create_window(parent, rect, desc.text, desc.subtitle)?;
                                //   before the children exist: the corner clearance is then already
                                // accounted for when the single layout pass runs; and scrolling
                                // changes how the stack treats rows that do not fit. An unset desc
                                // radius keeps the themed one create_window resolved
                                let win = self.w_mut(id).window_mut().expect("a window");
                                if desc.corner_radius != 0 {
                                        win.corner_radius = desc.corner_radius;
                                }
                                win.scroll = desc.scroll;
                                id
                        }
                        DescKind::Button => {
                                let id = self.create_button(parent, rect, desc.text.unwrap_or(""), desc.emit, desc.nav)?;
                                if let Kind::Button(b) = &mut self.w_mut(id).kind {
                                        b.shade = desc.shade;
                                }
                                id
                        }
                        DescKind::Label => self.create_label(parent, rect, desc.text.unwrap_or(""))?,
                };
                {
                        let w = self.w_mut(id);
                        w.min_w = desc.min_w;
                        w.min_h = desc.min_h;
                        w.max_w = desc.max_w;
                        w.max_h = desc.max_h;
                        w.grow = desc.grow;
                        w.tag = desc.tag;
                }
                for child in desc.children {
                        self.build_desc(Some(id), child)?;
                }
                // after the children, since a stack divides the content area between them
                match desc.layout {
                        Layout::Stack { gap } => self.layout_stack(id, gap),
                        Layout::Row { gap } => self.layout_row(id, gap),
                        Layout::Linear { gap } => self.layout_linear(id, gap),
                        Layout::None => {}
                }
                Ok(id)
        }

        /// Realise `desc` and its children under `parent` (`None` for the root). Building a ROOT
        /// also re-lays-out, sizing the tree to the canvas: a descriptor cannot carry the root's
        /// rect, which is what lets one serve a 64x128 OLED and a 240x280 panel unchanged. On
        /// `Error::Full` the partial subtree is torn down again.
        pub fn build(&mut self, parent: Option<WidgetId>, desc: &'static Desc<A>) -> Result<WidgetId, Error> {
                let before = self.widgets.len();
                match self.build_desc(parent, desc) {
                        Ok(id) => {
                                if parent.is_none() {
                                        self.relayout();
                                }
                                Ok(id)
                        }
                        Err(e) => {
                                // whatever got built is unreachable garbage otherwise
                                for i in before..self.widgets.len() {
                                        self.widgets[i] = None;
                                }
                                if parent.is_none() {
                                        self.root = None;
                                        self.focused = None;
                                }
                                error!("ui: building a page needs more than {} widgets", N);
                                Err(e)
                        }
                }
        }

        /// Free a widget and everything under it, unlinking it first. Focus and the drag target
        /// are cleared if they pointed inside. The only correct way to release a widget.
        pub fn destroy(&mut self, id: WidgetId) {
                if self.get(id).is_none() {
                        return;
                }
                // unlinked BEFORE anything is freed, so the tree never holds a stale handle
                match self.w(id).parent {
                        None => {
                                if self.root == Some(id) {
                                        self.root = None;
                                }
                        }
                        Some(p) => {
                                let next = self.w(id).next_sibling;
                                if self.w(p).first_child == Some(id) {
                                        self.w_mut(p).first_child = next;
                                } else {
                                        let mut c = self.w(p).first_child;
                                        while let Some(cid) = c {
                                                if self.w(cid).next_sibling == Some(id) {
                                                        self.w_mut(cid).next_sibling = next;
                                                        break;
                                                }
                                                c = self.w(cid).next_sibling;
                                        }
                                }
                        }
                }
                self.destroy_subtree(id);
                // whatever the subtree occupied has to be repainted; the widget that owned that
                // area no longer exists to invalidate it
                self.invalidate_all();
        }

        fn destroy_subtree(&mut self, id: WidgetId) {
                let mut c = self.w(id).first_child;
                while let Some(cid) = c {
                        c = self.w(cid).next_sibling;
                        self.destroy_subtree(cid);
                }
                if self.focused == Some(id) {
                        self.focused = None;
                }
                if self.drag_window == Some(id) {
                        self.drag_window = None;
                }
                if matches!(self.flash, Some((f, _)) if f == id) {
                        self.flash = None;
                }
                self.widgets[usize::from(id.0)] = None;
        }

        /// Next widget in depth-first pre-order -- paint order and focus order -- or `None` once
        /// the walk has left the subtree at `root`.
        fn next(&self, mut id: WidgetId, root: WidgetId) -> Option<WidgetId> {
                if let Some(c) = self.w(id).first_child {
                        return Some(c);
                }
                loop {
                        if id == root {
                                return None;
                        }
                        if let Some(n) = self.w(id).next_sibling {
                                return Some(n);
                        }
                        id = self.w(id).parent?;
                }
        }

        /// Every widget in pre-order from the root.
        fn walk(&self) -> impl Iterator<Item = WidgetId> + '_ {
                let root = self.root;
                let mut cur = root;
                core::iter::from_fn(move || {
                        let id = cur?;
                        cur = self.next(id, root?);
                        Some(id)
                })
        }

        fn children(&self, id: WidgetId) -> impl Iterator<Item = WidgetId> + '_ {
                let mut cur = self.w(id).first_child;
                core::iter::from_fn(move || {
                        let c = cur?;
                        cur = self.w(c).next_sibling;
                        Some(c)
                })
        }

        /// The direct children of `id`, in build order. For walking a built tree from outside -- an
        /// editor correlating each widget with the design node that produced it (the two are 1:1 and
        /// in the same order), to hit-test a point or outline a selection, frames included.
        pub fn child_ids(&self, id: WidgetId) -> impl Iterator<Item = WidgetId> + '_ {
                self.children(id)
        }

        // --- navigation ---

        fn show_page(&mut self, page: &'static Page<A>, return_page: Option<&'static Page<A>>, back: bool) -> Result<(), Error> {
                //   nothing to slide before the first page exists. A rotation in progress already
                // owns the back buffer and the frame, so a transfer during one simply snaps -- a
                // correct change beats two animations fighting over the same pixels. The image
                // itself is captured at the first render step, which is before anything of the
                // new tree has been drawn
                if self.root.is_some() && !self.rotating {
                        self.page_moving = true;
                        self.page_move_started = false;
                        self.page_move_back = back;
                        //   the CHILD of the pair decides the descent -- the page being entered
                        // going forward, the one being left coming back -- so a page's arrival
                        // and departure run the same axis and mirror. Precedence: the page's own
                        // override, then the tree default, then the layout seed that reproduces
                        // the historical flow. The seed follows the page's EFFECTIVE axis -- a
                        // horizontal page (a Row, or a Linear tree set horizontal) enters from
                        // the bottom, a vertical one from the right
                        let child = if back { self.page } else { Some(page) };
                        let axis = self.layout_axis;
                        let seed = || {
                                let horizontal = match child.map(|p| p.content.layout) {
                                        Some(Layout::Row { .. }) => true,
                                        Some(Layout::Linear { .. }) => axis == Axis::Horizontal,
                                        _ => false,
                                };
                                if horizontal { Descent::FromBottom } else { Descent::FromRight }
                        };
                        self.page_move_descent = child.and_then(|p| p.descend).or(self.default_descent).unwrap_or_else(seed);
                }
                // the old tree goes before the new one is built: only one page's widgets exist
                if let Some(root) = self.root {
                        self.destroy(root);
                }
                self.page = Some(page);
                self.return_page = return_page;
                self.build(None, page.content)?;
                self.invalidate_all();
                Ok(())
        }

        /// Build `page` in place of whatever is showing. Back from here goes to its parent. Safe
        /// to call from wherever an activation is handled -- the activating widget is gone
        /// afterwards, which is why activation returns before anything navigates.
        pub fn navigate(&mut self, page: &'static Page<A>) -> Result<(), Error> {
                self.show_page(page, None, false)
        }

        /// Rebuild `page` in place with NO transition -- for reflecting an in-place data change
        /// (a design editor re-materialising the page it just edited, a live-updated list). Unlike
        /// [`navigate`](Self::navigate) it does not slide: the point is to show the same page,
        /// changed, without a page-move animation.
        pub fn reload(&mut self, page: &'static Page<A>) -> Result<(), Error> {
                if let Some(root) = self.root {
                        self.destroy(root);
                }
                self.page = Some(page);
                self.return_page = None;
                self.build(None, page.content)?;
                self.invalidate_all();
                Ok(())
        }

        /// The same, but back from `page` goes to `return_page` -- for a cross-tree jump that
        /// should return to where it was reached from. The override lasts exactly one page.
        pub fn navigate_returning(&mut self, page: &'static Page<A>, return_page: &'static Page<A>) -> Result<(), Error> {
                self.show_page(page, Some(return_page), false)
        }

        /// Navigate to an explicit `page` with the BACK-direction transition -- the mirror of
        /// [`navigate`](Self::navigate). For a caller that keeps its own history (so the target is
        /// known) rather than relying on the parent link that [`navigate_back`](Self::navigate_back)
        /// follows.
        pub fn navigate_back_to(&mut self, page: &'static Page<A>) -> Result<(), Error> {
                self.show_page(page, None, true)
        }

        /// Go to the current page's return address if one was set, otherwise its parent. `false`,
        /// changing nothing, when there is nowhere to go -- a top-level page, or a tree built
        /// without pages -- so a caller can leave the gesture meaning nothing there.
        pub fn navigate_back(&mut self) -> bool {
                let Some(page) = self.page else { return false };
                let Some(target) = self.return_page.or(page.parent) else { return false };
                self.show_page(target, None, true).is_ok()
        }

        /// Point the whole tree's navigation flow one way: every child page opens in this
        /// direction unless it pins its own with [`Page::descend`]. `None` restores the
        /// historical layout-derived flow (a `Row` page rises, everything else slides left).
        /// Takes effect on the next navigation; an app may change it live, e.g. from an
        /// orientation sensor. See [`Descent`].
        pub fn set_default_descent(&mut self, descend: Option<Descent>) {
                self.default_descent = descend;
                self.default_descent_explicit = true;
        }

        /// The tree-wide default set by [`set_default_descent`](Self::set_default_descent),
        /// or `None` when navigation follows the layout-derived flow.
        pub fn default_descent(&self) -> Option<Descent> {
                self.default_descent
        }

        /// Set the axis every [`Layout::Linear`] window in this tree runs along -- the whole
        /// interface's portrait/landscape choice, made from the outside. Re-lays the current
        /// page so a change takes effect at once; `Stack` and `Row` windows are unaffected.
        pub fn set_layout_axis(&mut self, axis: Axis) {
                if self.layout_axis == axis {
                        return;
                }
                self.layout_axis = axis;
                if let Some(root) = self.root {
                        self.relayout_window(root);
                        self.invalidate_all();
                }
        }

        /// The tree's generic-layout axis, [`Axis::Vertical`] unless
        /// [`set_layout_axis`](Self::set_layout_axis) changed it.
        pub fn layout_axis(&self) -> Axis {
                self.layout_axis
        }

        // --- geometry ---

        fn inset_x(win: &Window) -> i32 {
                i32::from(win.padding) + if win.border { 1 } else { 0 }
        }

        /// Text rows in a window's title band: two when it carries a [`subtitle`](Window::subtitle),
        /// one otherwise. The single source `viewport` and `paint_window` share, so the reserved
        /// band and the painted band always agree.
        fn title_rows(win: &Window) -> i32 {
                if win.subtitle.is_some() { 2 } else { 1 }
        }

        /// A window's VIEWPORT: the area content shows through, inside border, padding, header
        /// band and corner clearance. One function so painting, hit-testing, the stack layout and
        /// the scroll clamp can never disagree about where content is allowed to be.
        ///
        /// A rounded corner is cleared vertically only as far as the arc reaches in at `inset_x`,
        /// not by the whole radius: at radius 40 with a 3 px inset that is 25 rows rather than 40.
        /// A titled window loses the header band, which is a lower bound on the content top
        /// rather than an addition -- adding would push content down twice for the same corner.
        fn viewport(&self, id: WidgetId) -> Rect {
                let w = self.w(id);
                let win = w.window().expect("a window");
                let inset_x = Self::inset_x(win);
                let drop = corner_drop(win.corner_radius, inset_x);
                let inset_y = drop.max(inset_x);
                let mut content = Rect::new(w.rect.x0 + inset_x, w.rect.y0 + inset_y, w.rect.x1 - inset_x, w.rect.y1 - inset_y);
                if win.title.is_some() {
                        //   a small breathing gap below the title bar before the content, so the top
                        // control does not butt against the separator. A rounded window already has
                        // it -- its corner_drop clears the curve by more than this -- so the max()
                        // leaves those unchanged and only gives a square window the same spacing.
                        const HEADER_GAP: i32 = 4;
                        let header_bottom = w.rect.y0 + if win.border { 1 } else { 0 } + Self::title_rows(win) * self.role_ch(FontRole::Title) + 2;
                        content.y0 = content.y0.max(header_bottom + HEADER_GAP);
                }
                content
        }

        /// Where a vertically scrolling stack's travel ends: the last row comes to rest at the
        /// flush edge (`rect.y1 - inset_x`), not the viewport's bottom. The plain viewport bottom
        /// for anything that earns no corner treatment.
        fn scroll_stop_y1(&self, id: WidgetId) -> i32 {
                let vp = self.viewport(id);
                let w = self.w(id);
                let win = w.window().expect("a window");
                if !matches!(win.layout, Layout::Stack { .. }) || win.scroll & scroll::VERTICAL == 0 {
                        return vp.y1;
                }
                let inset_x = Self::inset_x(win);
                if i32::from(win.corner_radius) - inset_x <= 0 {
                        return vp.y1;
                }
                (w.rect.y1 - inset_x).max(vp.y1)
        }

        fn last_visible_child(&self, id: WidgetId) -> Option<WidgetId> {
                self.children(id).filter(|c| self.w(*c).visible).last()
        }

        fn row_height(w: &Widget<A>, mut h: i32) -> i32 {
                if w.max_h != 0 && h > w.max_h {
                        h = w.max_h;
                }
                if w.min_h != 0 && h < w.min_h {
                        h = w.min_h;
                }
                h
        }

        fn row_width(w: &Widget<A>, mut width: i32) -> i32 {
                if w.max_w != 0 && width > w.max_w {
                        width = w.max_w;
                }
                if w.min_w != 0 && width < w.min_w {
                        width = w.min_w;
                }
                width
        }

        /// Divide the window's content area into equal-height rows, one per visible child, `gap`
        /// pixels apart. What makes a 64x128 OLED usable without hand-computing rects, and the
        /// companion to focus cycling: a stack has an obvious visual order.
        ///
        /// The last row of a NON-scrolling stack under a rounded frame runs all the way down and
        /// takes the container's curve as its own bottom corners: insetting a rounded rect by
        /// `inset_x` leaves a rounded rect of radius `R - inset_x` about the same arc centres, so
        /// a row ending at `y1 - inset_x` with that corner radius traces the container exactly --
        /// provided it is at least that tall, which is checked and withdrawn otherwise. A
        /// scrolling stack's rows move, so corners minted for one position are wrong the moment
        /// they do: its last row wears rounded bottom corners permanently instead, as the list's
        /// end cap, sitting flush when the clamp lands it at the stop.
        pub fn layout_stack(&mut self, id: WidgetId, gap: u8) {
                if let Some(win) = self.w_mut(id).window_mut() {
                        win.layout = Layout::Stack { gap };
                }
                self.lay_vertical(id, gap);
        }

        /// The vertical placement pass of [`layout_stack`], without recording the layout kind:
        /// shared with a [`Layout::Linear`] window in a vertical tree, which stays `Linear` so
        /// a later axis flip re-lays it the other way.
        fn lay_vertical(&mut self, id: WidgetId, gap: u8) {
                let gap = i32::from(gap);
                let mut content = self.viewport(id);
                let plain_y1 = content.y1;
                let (rect, inset_x, corner_radius, scroll_flags) = {
                        let w = self.w(id);
                        let win = w.window().expect("a window");
                        (w.rect, Self::inset_x(win), win.corner_radius, win.scroll)
                };
                let scroll_v = scroll_flags & scroll::VERTICAL != 0;

                let mut flush_r = if scroll_v { 0 } else { (i32::from(corner_radius) - inset_x).max(0) };
                if flush_r > 0 {
                        content.y1 = rect.y1 - inset_x;
                }
                let cap_r = if scroll_v { (i32::from(corner_radius) - inset_x).max(0) } else { 0 };

                let count = self.children(id).filter(|c| self.w(*c).visible).count() as i32;
                if count == 0 || rect_empty(&content) {
                        return;
                }

                let mut total_h = content.y1 - content.y0 + 1;
                let mut row_h = (total_h - gap * (count - 1)) / count;
                if row_h < 1 {
                        if !scroll_v {
                                warn!("ui: window content ({} px) too short for {} stacked rows", total_h, count);
                        }
                        row_h = 1;
                }
                // the withdrawal: rows too short to contain the curve pull the bottom back
                if flush_r > 0 && row_h < flush_r {
                        content.y1 = plain_y1;
                        flush_r = 0;
                        total_h = content.y1 - content.y0 + 1;
                        row_h = ((total_h - gap * (count - 1)) / count).max(1);
                }

                // the content's extent, measured before anything is placed, so the offset is
                // clamped against it FIRST and rows are laid out against a legal offset
                let viewport_w = content.x1 - content.x0 + 1;
                let mut content_h = 0;
                let mut content_w = 0;
                let kids: Vec<WidgetId, N> = self.children(id).filter(|c| self.w(*c).visible).collect();
                for &c in kids.iter() {
                        content_h += Self::row_height(self.w(c), row_h);
                        content_w = content_w.max(Self::row_width(self.w(c), viewport_w));
                }
                content_h += gap * (count - 1);
                let stop_y1 = self.scroll_stop_y1(id);
                let (scroll_x, scroll_y) = {
                        let win = self.w_mut(id).window_mut().expect("a window");
                        win.content_h = content_h;
                        win.content_w = content_w;
                        let mut max_sy = content_h - (stop_y1 - content.y0 + 1);
                        let mut max_sx = content_w - viewport_w;
                        if !scroll_v || max_sy < 0 {
                                max_sy = 0;
                        }
                        if win.scroll & scroll::HORIZONTAL == 0 || max_sx < 0 {
                                max_sx = 0;
                        }
                        win.scroll_y = win.scroll_y.clamp(0, max_sy);
                        win.scroll_x = win.scroll_x.clamp(0, max_sx);
                        (win.scroll_x, win.scroll_y)
                };

                let floor_y = if self.w(id).parent.is_some() { rect.y1 } else { self.height - 1 };
                let mut y = content.y0 - scroll_y;
                let x0 = content.x0 - scroll_x;
                for (index, &c) in kids.iter().enumerate() {
                        let last = index as i32 + 1 == count;
                        let mut h = Self::row_height(self.w(c), row_h);
                        let width = Self::row_width(self.w(c), viewport_w);
                        // the last row of a non-scrolling stack absorbs the division remainder
                        // as well as reaching the bottom edge
                        if last && !scroll_v {
                                h = Self::row_height(self.w(c), content.y1 - y + 1);
                        }
                        let cw = self.w_mut(c);
                        cw.rect = Rect::new(x0, y, x0 + width - 1, y + h - 1);
                        // a flush last row claims the strip beneath it for hit-testing: padding,
                        // border and (for the root) the safe inset, where a thumb reaching for
                        // the bottom button lands. Only when flush: a row that pulled back stops
                        // short on purpose
                        cw.hit_slop_y1 = 0;
                        if last && flush_r > 0 {
                                cw.hit_slop_y1 = (floor_y - cw.rect.y1).clamp(0, 255);
                        }
                        if let Kind::Button(b) = &mut cw.kind {
                                let r = if last && flush_r > 0 {
                                        flush_r
                                } else if last && cap_r > 0 && h >= cap_r {
                                        cap_r
                                } else {
                                        0
                                };
                                b.corner_radius = r as u8;
                                b.corners = if r > 0 { light_draw::corner::BOTTOM } else { light_draw::corner::NONE };
                        }
                        y += h + gap;
                }
                // a child that is itself a laid-out window was arranged against the rect it
                // had BEFORE this pass moved it: re-lay its interior against the new one
                for &c in kids.iter() {
                        self.relayout_window(c);
                }
                self.invalidate_widget(id);
        }

        /// Divide the window's content area into equal-width columns, one per visible
        /// child, `gap` pixels apart: [`layout_stack`](Self::layout_stack) turned on its
        /// side, for an interface on glass wider than it is tall. Columns take the full
        /// content height; a child's `min_w`/`max_w` pins its width. A non-scrolling row's
        /// last column absorbs the division remainder; a row marked
        /// [`scroll::HORIZONTAL`] instead lets its columns run past the frame and drags
        /// sideways, the clamp working exactly as the stack's vertical one does. No
        /// corner-flush treatment either way. Vertical scrolling is pinned off: a row is
        /// never taller than its window.
        pub fn layout_row(&mut self, id: WidgetId, gap: u8) {
                if let Some(win) = self.w_mut(id).window_mut() {
                        win.layout = Layout::Row { gap };
                }
                self.lay_horizontal(id, gap);
        }

        /// The horizontal placement pass of [`layout_row`], without recording the layout kind:
        /// shared with a [`Layout::Linear`] window in a horizontal tree, which stays `Linear`.
        fn lay_horizontal(&mut self, id: WidgetId, gap: u8) {
                let gap = i32::from(gap);
                let content = self.viewport(id);
                let scroll_h = {
                        let win = self.w(id).window().expect("a window");
                        win.scroll & scroll::HORIZONTAL != 0
                };
                let count = self.children(id).filter(|c| self.w(*c).visible).count() as i32;
                if count == 0 || rect_empty(&content) {
                        return;
                }
                let total_w = content.x1 - content.x0 + 1;
                let mut col_w = (total_w - gap * (count - 1)) / count;
                if col_w < 1 {
                        if !scroll_h {
                                warn!("ui: window content ({} px) too narrow for {} columns", total_w, count);
                        }
                        col_w = 1;
                }
                let viewport_h = content.y1 - content.y0 + 1;
                let kids: Vec<WidgetId, N> = self.children(id).filter(|c| self.w(*c).visible).collect();
                //   each column's width, settled before anything is placed so the extent is known
                // and the scroll offset can be clamped against it. With a child marked to stretch
                // (`grow`), pinned siblings keep their own widths wherever they sit and the surplus
                // is split between the grow children -- so a fixed button can flank a stretching one
                // on either side. Without one, the historical rule holds: equal columns, the last
                // absorbing the division remainder. A scrolling row has no surplus, so grow is moot.
                let grow_count = kids.iter().filter(|&&c| self.w(c).grow).count() as i32;
                let mut widths: Vec<i32, N> = Vec::new();
                if grow_count > 0 && !scroll_h {
                        let mut fixed = 0;
                        for &c in kids.iter() {
                                if !self.w(c).grow {
                                        fixed += Self::row_width(self.w(c), 0);
                                }
                        }
                        let surplus = (total_w - gap * (count - 1) - fixed).max(grow_count);
                        let each = surplus / grow_count;
                        let (mut given, mut seen) = (0, 0);
                        for &c in kids.iter() {
                                let w = if self.w(c).grow {
                                        seen += 1;
                                        // the last grow column takes the rounding slack, so the row fills exactly
                                        let w = if seen == grow_count { surplus - given } else { each };
                                        given += each;
                                        w
                                } else {
                                        Self::row_width(self.w(c), 0)
                                };
                                let _ = widths.push(w);
                        }
                } else {
                        for (index, &c) in kids.iter().enumerate() {
                                let last = index as i32 + 1 == count;
                                // the last column of a non-scrolling row absorbs the division remainder;
                                // a scrolling row's columns keep their measured widths
                                let w = if last && !scroll_h {
                                        let used: i32 = widths.iter().sum::<i32>() + gap * (count - 1);
                                        Self::row_width(self.w(c), total_w - used)
                                } else {
                                        Self::row_width(self.w(c), col_w)
                                };
                                let _ = widths.push(w);
                        }
                }
                let content_w = widths.iter().sum::<i32>() + gap * (count - 1);
                let scroll_x = {
                        let win = self.w_mut(id).window_mut().expect("a window");
                        win.content_w = content_w;
                        win.content_h = viewport_h;
                        let mut max_sx = content_w - total_w;
                        if !scroll_h || max_sx < 0 {
                                max_sx = 0;
                        }
                        win.scroll_x = win.scroll_x.clamp(0, max_sx);
                        win.scroll_y = 0;
                        win.scroll_x
                };
                let mut x = content.x0 - scroll_x;
                for (index, &c) in kids.iter().enumerate() {
                        let w = widths[index];
                        let h = Self::row_height(self.w(c), viewport_h);
                        let cw = self.w_mut(c);
                        cw.rect = Rect::new(x, content.y0, x + w - 1, content.y0 + h - 1);
                        cw.hit_slop_y1 = 0;
                        if let Kind::Button(b) = &mut cw.kind {
                                b.corner_radius = 0;
                                b.corners = light_draw::corner::NONE;
                        }
                        x += w + gap;
                }
                // a child that is itself a laid-out window was arranged against the rect it
                // had BEFORE this pass moved it: re-lay its interior against the new one
                for &c in kids.iter() {
                        self.relayout_window(c);
                }
                self.invalidate_widget(id);
        }

        /// Re-run whichever layout the window recorded. A no-op for hand-placed children.
        fn relayout_window(&mut self, id: WidgetId) {
                let Some(win) = self.w(id).window() else { return };
                match win.layout {
                        Layout::Stack { gap } => self.layout_stack(id, gap),
                        Layout::Row { gap } => self.layout_row(id, gap),
                        Layout::Linear { gap } => self.layout_linear(id, gap),
                        Layout::None => {}
                }
        }

        /// Lay a window along the tree's current [`Axis`] and record it as [`Layout::Linear`],
        /// so a later [`set_layout_axis`](Self::set_layout_axis) re-lays it the other way. The
        /// public entry for a generic layout.
        pub fn layout_linear(&mut self, id: WidgetId, gap: u8) {
                if let Some(win) = self.w_mut(id).window_mut() {
                        win.layout = Layout::Linear { gap };
                }
                match self.layout_axis {
                        Axis::Vertical => self.lay_vertical(id, gap),
                        Axis::Horizontal => self.lay_horizontal(id, gap),
                }
        }

        /// Round the window's frame and keep its content clear of the curve. The clearance is NOT
        /// uniform: a corner only eats into rows within the radius of the top and bottom edges,
        /// so content is pushed down and up by the radius while the horizontal inset stays at
        /// border + padding -- insetting all four sides by the radius would give back exactly the
        /// width this exists to recover. Re-lays-out.
        pub fn set_corner_radius(&mut self, id: WidgetId, radius: u8) {
                let Some(win) = self.w_mut(id).window_mut() else { return };
                if win.corner_radius == radius {
                        return;
                }
                win.corner_radius = radius;
                self.relayout_window(id);
                self.invalidate_widget(id);
        }

        /// Mark a window scrollable along the given axes. The layout pass this triggers is also
        /// what clamps the offset, putting content back inside the frame when an axis stops.
        pub fn set_scroll(&mut self, id: WidgetId, flags: u8) {
                let Some(win) = self.w_mut(id).window_mut() else { return };
                if win.scroll == flags {
                        return;
                }
                win.scroll = flags;
                self.relayout_window(id);
                self.invalidate_widget(id);
        }

        /// Content extents for a hand-placed window, from its children's rects with the offset
        /// added back -- the extent is a property of the content, not of where it is scrolled to.
        fn measure_content(&mut self, id: WidgetId) {
                let vp = self.viewport(id);
                let (sx, sy) = {
                        let win = self.w(id).window().expect("a window");
                        (win.scroll_x, win.scroll_y)
                };
                let mut w = 0;
                let mut h = 0;
                for c in self.children(id).collect::<Vec<_, N>>() {
                        let cw = self.w(c);
                        if !cw.visible {
                                continue;
                        }
                        w = w.max(cw.rect.x1 + sx - vp.x0 + 1);
                        h = h.max(cw.rect.y1 + sy - vp.y0 + 1);
                }
                let win = self.w_mut(id).window_mut().expect("a window");
                win.content_w = w;
                win.content_h = h;
        }

        /// Scroll to an absolute offset, clamped to `[0, content - viewport]`: the content's far
        /// edge never comes past the viewport's, and an axis without its flag never moves. That
        /// clamp is the entire safety argument for scrolling; everything else is a rect shift.
        /// Returns whether anything moved.
        pub fn scroll_to(&mut self, id: WidgetId, x: i32, y: i32) -> bool {
                if self.w(id).window().is_none() {
                        return false;
                }
                let vp = self.viewport(id);
                if rect_empty(&vp) {
                        return false;
                }
                if self.w(id).window().expect("a window").layout == Layout::None {
                        self.measure_content(id);
                }
                let stop_y1 = self.scroll_stop_y1(id);
                let (nx, ny, sx, sy) = {
                        let win = self.w(id).window().expect("a window");
                        let max_sx = if win.scroll & scroll::HORIZONTAL != 0 { (win.content_w - (vp.x1 - vp.x0 + 1)).max(0) } else { 0 };
                        let max_sy = if win.scroll & scroll::VERTICAL != 0 { (win.content_h - (stop_y1 - vp.y0 + 1)).max(0) } else { 0 };
                        let nx = x.clamp(0, max_sx);
                        let ny = y.clamp(0, max_sy);
                        // the content shifts OPPOSITE to the offset's change
                        (nx, ny, win.scroll_x - nx, win.scroll_y - ny)
                };
                if sx == 0 && sy == 0 {
                        return false;
                }
                {
                        let win = self.w_mut(id).window_mut().expect("a window");
                        win.scroll_x = nx;
                        win.scroll_y = ny;
                }
                // rects stay ABSOLUTE: scrolling shifts everything under the window
                let mut c = self.w(id).first_child;
                while let Some(cid) = c {
                        let cw = self.w_mut(cid);
                        cw.rect = Rect::new(cw.rect.x0 + sx, cw.rect.y0 + sy, cw.rect.x1 + sx, cw.rect.y1 + sy);
                        c = self.next(cid, id);
                }
                self.invalidate_widget(id);
                trace!("ui: window scrolled to ({nx}, {ny})");
                true
        }

        /// Scroll by `(dx, dy)` -- positive `dy` scrolls DOWN the content, i.e. the content moves
        /// up through the frame. Clamped as [`scroll_to`](Self::scroll_to).
        pub fn scroll_by(&mut self, id: WidgetId, dx: i32, dy: i32) -> bool {
                let Some(win) = self.w(id).window() else { return false };
                let (x, y) = (win.scroll_x + dx, win.scroll_y + dy);
                self.scroll_to(id, x, y)
        }

        /// Scroll every scrollable ancestor of `id` by as little as brings it into view --
        /// innermost first, since an outer window's decision has to see where the inner one left
        /// it. Called by [`set_focus`](Self::set_focus), so focus-driven navigation scrolls for free.
        pub fn scroll_into_view(&mut self, id: WidgetId) {
                let mut p = self.w(id).parent;
                while let Some(pid) = p {
                        p = self.w(pid).parent;
                        if !self.w(pid).is_scrolling_window() {
                                continue;
                        }
                        let mut vp = self.viewport(pid);
                        // the last row's "in view" reaches to the scroll stop, so focusing it
                        // rides it down flush against the frame, where a drag leaves it
                        if self.last_visible_child(pid) == Some(id) {
                                vp.y1 = self.scroll_stop_y1(pid);
                        }
                        let r = self.w(id).rect;
                        // as little as brings it in: far edge first, then the near edge overrides,
                        // so a widget taller than the viewport shows its top
                        let mut dx = 0;
                        let mut dy = 0;
                        if r.y1 > vp.y1 {
                                dy = r.y1 - vp.y1;
                        }
                        if r.y0 - dy < vp.y0 {
                                dy = r.y0 - vp.y0;
                        }
                        if r.x1 > vp.x1 {
                                dx = r.x1 - vp.x1;
                        }
                        if r.x0 - dx < vp.x0 {
                                dx = r.x0 - vp.x0;
                        }
                        if dx != 0 || dy != 0 {
                                self.scroll_by(pid, dx, dy);
                        }
                }
        }

        /// Keep `inset` pixels clear on every edge of the canvas. On rounded glass this is
        /// a small BREATHING MARGIN, not the corner radius: the curve is carried by the
        /// root window's own corner radius (the glass's measured radius minus this inset --
        /// insetting a rounded rectangle by d leaves a rounded rectangle of radius r - d).
        /// Setting the full glass radius here is the superseded approach that gave up a
        /// whole band on every edge to keep a SQUARE frame inside round glass.
        /// Install a look-and-feel; every element repaints with it. Install it BEFORE
        /// building pages: container corner radii resolve from the theme when a window is
        /// created, so a theme swapped under a live tree recolors it but keeps its
        /// geometry until the next page build. Typically parsed from
        /// an embedded LTH blob ([`Theme::parse`]) -- theming is a data change, never an
        /// edit to this crate.
        pub fn set_theme(&mut self, theme: Theme) {
                if self.theme == theme {
                        return;
                }
                self.theme = theme;
                //   a theme may seed the tree's descent -- but only as a starting value: an
                // application that called set_default_descent has spoken, and keeps its choice
                if !self.default_descent_explicit {
                        if let Some(d) = theme.descent {
                                self.default_descent = Some(d);
                        }
                }
                self.invalidate_all();
        }

        pub fn theme(&self) -> &Theme {
                &self.theme
        }

        /// Shade the focused widget's fill with a vertical gradient, or `None` for the
        /// solid inversion -- a live handle on the theme's [`theme::key::FOCUS_SURFACE`],
        /// which is where the value now lives.
        pub fn set_focus_shade(&mut self, shade: Option<Shade>) {
                if self.theme.focus_surface == shade {
                        return;
                }
                self.theme.focus_surface = shade;
                self.invalidate_all();
        }

        pub fn set_safe_inset(&mut self, inset: u8) {
                if self.safe_inset == i32::from(inset) {
                        return;
                }
                self.safe_inset = i32::from(inset);
                self.relayout();
                self.invalidate_all();
        }

        /// Re-run layout against the canvas as it is now: the root is resized to fill it (less the
        /// safe inset), then every window re-applies the arrangement it recorded, in pre-order so a
        /// window is resized by its parent before it lays out its own children.
        pub fn relayout(&mut self) {
                let Some(root) = self.root else { return };
                let inset = self.safe_inset;
                self.w_mut(root).rect = Rect::new(inset, inset, self.width - 1 - inset, self.height - 1 - inset);
                let mut cur = Some(root);
                while let Some(id) = cur {
                        cur = self.next(id, root);
                        self.relayout_window(id);
                }
        }

        /// Rotate the whole interface, keeping it upright as the device is turned. Re-orients the
        /// layer, re-lays-out (the canvas aspect has just flipped) and repaints everything. Safe
        /// between frames precisely because every frame is a full repaint. A no-op when unchanged.
        ///
        /// The toolkit knows nothing about orientation SENSORS: the application maps its IMU's
        /// orientation onto a rotation, because that depends on whether the panel is natively
        /// portrait or landscape, which is a board fact.
        pub fn set_rotation(&mut self, layer: &mut FrameLayer, rotation: Rotation) {
                //   compared against the target rather than the live rotation: mid-animation the
                // live one is still the OLD value, so a repeated orientation report would
                // otherwise restart the turn on every tick
                if self.rotating {
                        if self.rotate_target == rotation {
                                return;
                        }
                } else if layer.rotation() == rotation {
                        return;
                }
                if self.page_moving {
                        //   the target is recorded rather than the rotation restarted from
                        // scratch, so a board turned twice mid-transition settles where it ended up
                        self.rotate_deferred = Some(rotation);
                        debug!("ui: rotation to {rotation:?} deferred until the page transition finishes");
                        return;
                }
                let from = if self.rotating { self.rotate_target } else { layer.rotation() };
                self.rotating = true;
                self.rotate_started = false;
                self.rotate_target = rotation;
                self.rotate_degrees = rotation_delta_degrees(from, rotation);
                self.dirty = true;
                debug!("ui: rotating {} degrees to {rotation:?}", self.rotate_degrees);
        }

        /// Apply a rotation for real: re-orient the layer, drop the regions measured against the
        /// old geometry, re-lay-out and repaint everything -- the panel still shows the old
        /// arrangement in the old orientation, so nothing short of the whole canvas is safe.
        fn commit_rotation(&mut self, layer: &mut FrameLayer, rotation: Rotation) {
                layer.set_orientation(rotation, Flip::None);
                layer.invalidate_all();
                self.fit(layer);
                self.invalidate_all();
                let (w, h) = self.logical_size();
                debug!("ui rotation now {rotation:?}, canvas {w}x{h}");
        }

        /// One step of the rotation animation: a frame of the turn drawn, nothing drawn because
        /// the display was not ready, or the turn finished and the real rotation applied -- in
        /// which case the caller draws the settled tree without waiting a pass.
        fn rotation_step<D: DisplayDriver>(&mut self, layer: &mut FrameLayer, display: &mut Display<'_, D>, now_us: u64) -> Step {
                if !self.rotate_started {
                        if !display.is_double_buffered() || !display.format().is_rgb565() {
                                // nowhere to hold the image, or nothing to rotate it with: a
                                // correct snap beats a broken animation
                                let target = self.rotate_target;
                                self.rotating = false;
                                self.commit_rotation(layer, target);
                                return Step::Finished;
                        }
                        if !display.freeze() {
                                // the panel is still reading the front buffer; capture next pass
                                return Step::Waiting;
                        }
                        self.rotate_started = true;
                        self.rotate_start_us = now_us;
                }
                let elapsed = now_us.saturating_sub(self.rotate_start_us);
                if elapsed >= u64::from(self.rotate_ms) * 1000 {
                        // swapping back on before committing, so the repaint that follows runs
                        // against a normal double-buffered display again
                        display.thaw();
                        self.rotating = false;
                        let target = self.rotate_target;
                        self.commit_rotation(layer, target);
                        return Step::Finished;
                }
                let Some(c) = layer.frame_begin(display, now_us) else { return Step::Waiting };
                drop(c);
                // linear in time: at roughly ten frames for the whole turn an eased curve is
                // below what the eye picks out, and linear keeps the angle predictable.
                // Shrunk to whatever still fits, or the corners are sliced off mid-turn
                let angle = ((self.rotate_degrees as i64 * elapsed as i64) / (i64::from(self.rotate_ms) * 1000)) as i16;
                if let Some((front, captured)) = display.frame_and_capture() {
                        let mut c = layer.canvas(front);
                        let scale = c.scale_inscribed(angle);
                        c.blit_rotated(captured, angle, scale);
                }
                layer.invalidate_all();
                layer.frame_end(display);
                Step::Drew
        }

        /// One step of a page transition; the same contract as `rotation_step`.
        fn page_step<D: DisplayDriver>(&mut self, layer: &mut FrameLayer, display: &mut Display<'_, D>, style: &Style<'_>, now_us: u64) -> Step {
                if !self.page_move_started {
                        //   with a back buffer, the outgoing image is captured and slid off the
                        // incoming tree; without one (a single-buffered scanned panel), the
                        // roles swap -- the incoming tree draws OVER the old image at a
                        // shrinking offset, and the outgoing page survives in the live buffer
                        // wherever a step has not yet covered it. Only what the blit speaks
                        // can capture; the slide-over works in any format
                        self.page_move_over = !(display.is_double_buffered() && display.format().is_rgb565());
                        if self.page_move_over {
                                //   logical space end to end -- the canvas transform does the
                                // physical mapping -- so direction is just the arrival side:
                                // forward from logical +x, back from -x, like the capture mode.
                                // The vertical pair mirrors it a quarter-turn: forward drops in
                                // from logical -y, back rises from +y
                                let (w, h) = layer.logical_size();
                                //   over mode only ever slides the INCOMING in (no capture to move
                                // the outgoing), so it approximates the cover along the descent's
                                // axis. The offset shrinks from `span` to zero, so a positive unit
                                // starts the incoming past the far edge and brings it back this
                                // way: the vertical axis takes the descent's sign directly (down
                                // enters from the top), the horizontal one its negation (left
                                // enters from the right). Back reverses either.
                                let asign = self.page_move_descent.axis_sign();
                                let dir = if self.page_move_back { -1 } else { 1 };
                                if self.page_move_descent.vertical() {
                                        self.page_move_dx = 0;
                                        self.page_move_dy = asign * dir;
                                        self.page_move_span = i32::from(h);
                                } else {
                                        self.page_move_dx = -asign * dir;
                                        self.page_move_dy = 0;
                                        self.page_move_span = i32::from(w);
                                }
                        } else {
                                //   a forward vertical (Row-page) transition COVERS: the new page
                                // slides up over the old one, which stays put. Render the incoming
                                // into the back buffer ONCE and leave the front (the outgoing) as
                                // the static background; each step blits the incoming up over it.
                                // Every other capture-mode transition REVEALS: freeze the outgoing
                                // into the back and slide it off the live incoming.
                                let cover = self.page_move_descent.vertical() && !self.page_move_back;
                                if cover {
                                        let Some(back) = display.freeze_render() else {
                                                return Step::Waiting; // busy: try next pass
                                        };
                                        let mut c = layer.canvas(back);
                                        c.clear();
                                        self.paint(&mut c, style);
                                        drop(c);
                                } else if !display.freeze() {
                                        return Step::Waiting; // busy: capture next pass
                                }
                                //   the direction is chosen in LOGICAL terms and converted here,
                                // because the blit works in physical space: the transform's a and
                                // c are the physical components of logical +x (b and d of +y),
                                // exactly one of each pair non-zero for a pure rotation, so this
                                // picks the axis the viewer calls horizontal -- or, for a Row
                                // page's vertical transition, vertical -- whatever the board's
                                // orientation. Reveal-forward pushes the outgoing toward -x so the
                                // child arrives from the right; cover-forward's +y unit is the
                                // logical DOWN the incoming rises from, offset shrinking to zero.
                                let m = layer.transform();
                                let asign = self.page_move_descent.axis_sign();
                                let (ux, uy, sign) = if self.page_move_descent.vertical() {
                                        //   both directions run the descent's vertical axis: forward
                                        // the incoming rises into it (cover), back the outgoing
                                        // sinks off it (reveal) -- the one is the other reversed, so
                                        // the sign is the axis's alone and does not turn on `back`
                                        (m.b.signum(), m.d.signum(), asign)
                                } else {
                                        (m.a.signum(), m.c.signum(), asign * if self.page_move_back { -1 } else { 1 })
                                };
                                let (pw, ph) = layer.physical_size();
                                self.page_move_dx = sign * ux;
                                self.page_move_dy = sign * uy;
                                self.page_move_span = if ux != 0 { i32::from(pw) } else { i32::from(ph) };
                        }
                        self.page_move_started = true;
                        self.page_move_start_us = now_us;
                        self.page_move_travel = 0;
                }
                let elapsed = now_us.saturating_sub(self.page_move_start_us);
                if elapsed >= u64::from(self.page_move_ms) * 1000 {
                        if !self.page_move_over {
                                display.thaw();
                        }
                        self.page_moving = false;
                        self.invalidate_all();
                        //   a rotation that arrived mid-transition runs now. Taken BEFORE the
                        // call: set_rotation checks page_moving, already false, so it proceeds
                        if let Some(r) = self.rotate_deferred.take() {
                                self.set_rotation(layer, r);
                        }
                        return Step::Finished;
                }
                let travel = ((self.page_move_span as i64 * elapsed as i64) / (i64::from(self.page_move_ms) * 1000)) as i32;
                let cover = !self.page_move_over && self.page_move_descent.vertical() && !self.page_move_back;
                if self.page_move_over {
                        //   no clear: the outgoing image IS the ground the incoming page
                        // slides in over
                        let Some(mut c) = layer.frame_begin_over(display, now_us) else { return Step::Waiting };
                        c.set_offset(self.page_move_dx * (self.page_move_span - travel), self.page_move_dy * (self.page_move_span - travel));
                        self.paint(&mut c, style);
                        drop(c);
                } else if cover {
                        //   COVER: the incoming page (already rendered into the back buffer at
                        // setup) slides up over the static outgoing (the untouched front). Blit
                        // it at a SHRINKING offset along the logical-down axis: at full span it
                        // sits off-screen past the bottom, at zero it fully covers. The front
                        // keeps showing the outgoing wherever the incoming has not yet reached.
                        // Just a blit per step -- the incoming is drawn only once.
                        let Some(c) = layer.frame_begin_over(display, now_us) else { return Step::Waiting };
                        drop(c);
                        if let Some((front, incoming)) = display.frame_and_capture() {
                                let off = self.page_move_span - travel;
                                let mut c = layer.canvas(front);
                                c.blit_offset(incoming, self.page_move_dx * off, self.page_move_dy * off);
                        }
                } else {
                        //   over, not cleared: the buffer holds the previous step's frame, and
                        // the incoming page is STATIC in this mode -- only the strip the
                        // outgoing image has uncovered since the last drawn step needs
                        // painting, then the capture is re-blitted at its new offset. A full
                        // tree paint here cost ~70 ms a step on the big panels: a 300 ms
                        // transition landed in three visible jumps, and every one starved the
                        // audio ring into a click
                        let Some(c) = layer.frame_begin_over(display, now_us) else { return Step::Waiting };
                        drop(c);
                        if let Some((front, captured)) = display.frame_and_capture() {
                                let mut c = layer.canvas(front);
                                //   the outgoing image's LOGICAL motion sign along the axis (== the
                                // setup's `sign`): new rows of the incoming appear at the low end
                                // when it moves positive, the high end otherwise. The vertical
                                // axis takes the descent's sign alone (reveal here is always the
                                // back half of a cover, so it does not turn on `back`); the
                                // horizontal one reverses with it.
                                let vertical = self.page_move_descent.vertical();
                                let asign = self.page_move_descent.axis_sign();
                                let s = if vertical { asign } else { asign * if self.page_move_back { -1 } else { 1 } };
                                let prev = self.page_move_travel;
                                if travel > prev {
                                        let (lw, lh) = layer.logical_size();
                                        let (lw, lh) = (i32::from(lw), i32::from(lh));
                                        let extent = if vertical { lh } else { lw };
                                        let (b0, b1) = if s > 0 { (prev, travel - 1) } else { (extent - travel, extent - prev - 1) };
                                        let band = if vertical {
                                                Rect::new(0, b0, lw - 1, b1)
                                        } else {
                                                Rect::new(b0, 0, b1, lh - 1)
                                        };
                                        if let Some(root) = self.root {
                                                self.paint_clipped(&mut c, style, root, band);
                                        }
                                        c.clear_clip();
                                }
                                c.blit_offset(captured, self.page_move_dx * travel, self.page_move_dy * travel);
                                self.page_move_travel = travel;
                        }
                }
                layer.invalidate_all();
                layer.frame_end(display);
                Step::Drew
        }

        // --- invalidation ---

        /// Mark a widget's area as needing to reach the panel. Mutators call this; public for
        /// anything that changes what a custom widget draws.
        pub fn invalidate_widget(&mut self, id: WidgetId) {
                let r = self.w(id).rect;
                if !self.pending_all && self.pending.push(r).is_err() {
                        self.pending_all = true;
                }
                self.dirty = true;
        }

        /// Mark the whole canvas. Needed once at startup, before the first render.
        pub fn invalidate_all(&mut self) {
                self.pending_all = true;
                self.dirty = true;
        }

        // --- focus and activation ---

        fn is_focusable(&self, id: WidgetId) -> bool {
                let w = self.w(id);
                w.focusable && w.visible && w.enabled
        }

        /// One pass collecting the focusable before/after the focused one including both wrap
        /// cases; a backward pre-order traversal has no cheap formulation.
        fn focus_relative(&self, forward: bool) -> Option<WidgetId> {
                let mut first = None;
                let mut last = None;
                let mut before = None;
                let mut after = None;
                let mut seen = false;
                for id in self.walk() {
                        if !self.is_focusable(id) {
                                continue;
                        }
                        if first.is_none() {
                                first = Some(id);
                        }
                        if seen && after.is_none() {
                                after = Some(id);
                        }
                        if Some(id) == self.focused {
                                seen = true;
                                before = last;
                        }
                        last = Some(id);
                }
                first?;
                // no current focus, or it was hidden/disabled out of the cycle: start over
                if !seen {
                        return first;
                }
                if forward { after.or(first) } else { before.or(last) }
        }

        pub fn set_focus(&mut self, id: Option<WidgetId>) {
                if self.focused == id {
                        return;
                }
                // both the widget losing the highlight and the one gaining it change appearance
                if let Some(old) = self.focused {
                        if self.get(old).is_some() {
                                self.invalidate_widget(old);
                        }
                }
                self.focused = id;
                if let Some(id) = id {
                        // brought into its scrolling ancestor's viewport BEFORE the invalidation,
                        // so the rect invalidated is where the widget actually ended up
                        self.scroll_into_view(id);
                        self.invalidate_widget(id);
                }
        }

        pub fn focus_next(&mut self) {
                let id = self.focus_relative(true);
                self.set_focus(id);
        }

        pub fn focus_prev(&mut self) {
                let id = self.focus_relative(false);
                self.set_focus(id);
        }

        /// Fire a button: returns what it emits, then navigates if it says to. EVERYTHING is read
        /// before navigation, because navigation destroys the tree, the button included -- found
        /// the hard way in the C implementation this replaces, where reading a field after a
        /// navigating handler dereferenced freed memory and wedged the core.
        fn fire(&mut self, id: WidgetId) -> Option<A> {
                let (emit, nav, label) = match self.w(id).button() {
                        Some(b) => (b.emit, b.nav, b.label),
                        None => return None,
                };
                debug!("ui: button '{label}' activated");
                match nav {
                        Nav::Stay => {}
                        Nav::To(page) => {
                                if let Err(e) = self.navigate(page) {
                                        error!("ui: navigation from '{label}' failed: {e:?}");
                                }
                        }
                        Nav::Back => {
                                if !self.navigate_back() {
                                        debug!("ui: back from '{label}': nowhere to go");
                                }
                        }
                }
                emit
        }

        /// Activate the focused widget: what it emitted, if anything.
        pub fn activate(&mut self) -> Option<A> {
                let id = self.focused?;
                if !self.is_focusable(id) {
                        return None;
                }
                self.fire(id)
        }

        // --- input ---

        /// A panel point into logical coordinates, clamped into the canvas.
        fn untransform(&self, x: i32, y: i32) -> (i32, i32) {
                let m = &self.transform;
                let det = m.a * m.d - m.b * m.c;
                let px = x - m.tx;
                let py = y - m.ty;
                (((m.d * px - m.b * py) * det).clamp(0, (self.width - 1).max(0)), ((m.a * py - m.c * px) * det).clamp(0, (self.height - 1).max(0)))
        }

        fn canvas_rect(&self) -> Rect {
                Rect::new(0, 0, self.width - 1, self.height - 1)
        }

        fn widget_hit(&self, id: WidgetId, x: i32, y: i32) -> bool {
                let w = self.w(id);
                let mut r = w.rect;
                r.y1 += w.hit_slop_y1;
                rect_contains(&r, x, y)
        }

        /// Hit-testing with the same clipping the paint path applies: a widget scrolled out of its
        /// window's viewport is exactly as untouchable as it is invisible. Last match wins: deeper
        /// and later-drawn widgets are visited last, and those are on top where they overlap.
        fn hit_test(&self, id: WidgetId, mut clip: Rect, x: i32, y: i32, mut best: Option<WidgetId>) -> Option<WidgetId> {
                if !self.w(id).visible {
                        return best;
                }
                if self.is_focusable(id) && rect_contains(&clip, x, y) && self.widget_hit(id, x, y) {
                        best = Some(id);
                }
                if self.w(id).is_scrolling_window() {
                        let mut vp = self.viewport(id);
                        vp.y1 = vp.y1.max(self.scroll_stop_y1(id));
                        if !rect_intersect(&mut clip, &vp) {
                                return best;
                        }
                }
                for c in self.children(id) {
                        best = self.hit_test(c, clip, x, y, best);
                }
                best
        }

        /// The innermost scrollable window whose viewport contains the point: a drag belongs to
        /// the surface actually under the finger, not an ancestor that also scrolls.
        fn scroll_window_at(&self, id: WidgetId, mut clip: Rect, x: i32, y: i32, mut best: Option<WidgetId>) -> Option<WidgetId> {
                if !self.w(id).visible {
                        return best;
                }
                if self.w(id).is_scrolling_window() {
                        let mut vp = self.viewport(id);
                        vp.y1 = vp.y1.max(self.scroll_stop_y1(id));
                        if !rect_intersect(&mut vp, &clip) {
                                return best;
                        }
                        if rect_contains(&vp, x, y) {
                                best = Some(id);
                        }
                        clip = vp;
                }
                for c in self.children(id) {
                        best = self.scroll_window_at(c, clip, x, y, best);
                }
                best
        }

        /// Hit-test a PANEL point and, if it lands on an actionable widget, focus AND activate it
        /// -- a touch is a complete interaction. Returns `(hit, emitted)`.
        pub fn press_at(&mut self, x: u16, y: u16) -> (bool, Option<A>) {
                //   silently ignored while the interface is turning: the panel shows the
                // pre-rotation image being animated while the tree is laid out for the OLD
                // rotation and the transform is not yet the new one, so a tap would resolve
                // against a layout matching neither what is on screen nor where it will settle
                if self.rotating {
                        return (false, None);
                }
                let (lx, ly) = self.untransform(i32::from(x), i32::from(y));
                let Some(root) = self.root else { return (false, None) };
                let Some(hit) = self.hit_test(root, self.canvas_rect(), lx, ly, None) else { return (false, None) };
                self.set_focus(Some(hit));
                (true, self.fire(hit))
        }

        fn touch_reset(&mut self) {
                self.touch_down = false;
                self.touch_dragging = false;
                self.touch_moved = false;
                self.drag_window = None;
        }

        /// The stateful entry point: feed it the panel's CURRENT state every tick -- position and
        /// whether a finger is down -- and it runs the whole tap-versus-drag interaction.
        ///
        /// - A TAP fires on RELEASE only when the contact never strayed beyond `drag_slop` from
        ///   where it landed AND rested there at least [`TAP_MIN_HOLD_US`] -- a deliberate press,
        ///   not a brush or a jittered graze. It is delivered at the START point (with scrollable
        ///   content a down-edge fires on every drag's first contact, so the start is the intent).
        /// - A touch that moves beyond the slop over a scrollable window becomes a DRAG: the window
        ///   under the START point scrolls to follow the finger until release.
        /// - A touch that travels with nothing scrollable under it commits to neither, and the
        ///   release is left for the gesture pipeline -- a swipe on a non-scrolling page navigates.
        ///
        /// `now_us` is the current time (the same clock the animations run on); the caller passes
        /// it every sample so the hold can be measured without the toolkit owning a clock.
        pub fn touch(&mut self, x: u16, y: u16, touching: bool, now_us: u64) -> Touch<A> {
                // mid-rotation samples reset the tracker rather than being remembered: the
                // layout the touch began against is being replaced
                if self.rotating {
                        self.touch_reset();
                        return Touch::None;
                }
                if !touching {
                        if !self.touch_down {
                                return Touch::None;
                        }
                        let was_drag = self.touch_dragging;
                        let moved = self.touch_moved;
                        let held_us = now_us.saturating_sub(self.touch_start_us);
                        let (sx, sy) = self.touch_start;
                        let adx = (self.touch_last.0 - sx).abs();
                        let ady = (self.touch_last.1 - sy).abs();
                        self.touch_reset();
                        //   a tap must have stayed put (never beyond the slop, start to last) AND
                        // rested long enough. Too brief or wandered = intent unclear, dropped
                        let strayed = moved || adx > self.drag_slop || ady > self.drag_slop;
                        let too_brief = held_us < TAP_MIN_HOLD_US;
                        let verdict = if was_drag { "drag" } else if strayed { "strayed" } else if too_brief { "too brief" } else { "tap" };
                        // the verdict and the numbers, because "taps sometimes don't work" is
                        // otherwise undiagnosable; TRACE, since it fires on every touch
                        trace!("ui: release moved ({adx}, {ady}) slop {}, held {} us -> {}", self.drag_slop, held_us, verdict);
                        if was_drag {
                                return Touch::DragEnd;
                        }
                        if strayed || too_brief {
                                return Touch::None;
                        }
                        let hit = self.root.and_then(|root| self.hit_test(root, self.canvas_rect(), sx, sy, None));
                        return match hit {
                                Some(id) => {
                                        self.set_focus(Some(id));
                                        //   acknowledge the tap at once: a button wears its pressed
                                        // look for ACTIVATE_FLASH_US whether or not it was focused,
                                        // so a slow handler (a card open, a take starting) no longer
                                        // reads as a dropped tap. Set before firing, so a fire that
                                        // navigates clears it as the old tree is destroyed.
                                        if matches!(self.w(id).kind, Kind::Button(_)) {
                                                self.flash = Some((id, now_us + ACTIVATE_FLASH_US));
                                                self.invalidate_widget(id);
                                        }
                                        Touch::Tap { hit: true, emitted: self.fire(id) }
                                }
                                None => Touch::Tap { hit: false, emitted: None },
                        };
                }

                let (lx, ly) = self.untransform(i32::from(x), i32::from(y));
                if !self.touch_down {
                        self.touch_down = true;
                        self.touch_dragging = false;
                        self.touch_moved = false;
                        self.drag_window = None;
                        self.touch_start = (lx, ly);
                        self.touch_last = (lx, ly);
                        self.touch_start_us = now_us;
                        return Touch::Pending;
                }

                if !self.touch_dragging {
                        let adx = (lx - self.touch_start.0).abs();
                        let ady = (ly - self.touch_start.1).abs();
                        if adx > self.drag_slop || ady > self.drag_slop {
                                //   strayed beyond the slop: no longer a tap, whatever happens next
                                self.touch_moved = true;
                                let (sx, sy) = self.touch_start;
                                let target = self.root.and_then(|root| self.scroll_window_at(root, self.canvas_rect(), sx, sy, None));
                                if let Some(t) = target {
                                        self.touch_dragging = true;
                                        self.drag_window = Some(t);
                                        // engage with the full movement since the touch began, so
                                        // the content catches up rather than staying a slop behind;
                                        // the content follows the finger, hence the negation
                                        self.scroll_by(t, sx - lx, sy - ly);
                                }
                        }
                        self.touch_last = (lx, ly);
                        return if self.touch_dragging { Touch::Drag } else { Touch::Pending };
                }

                if let Some(t) = self.drag_window {
                        if self.get(t).is_some() {
                                let (px, py) = self.touch_last;
                                self.scroll_by(t, px - lx, py - ly);
                        }
                }
                self.touch_last = (lx, ly);
                Touch::Drag
        }

        /// Classify a swipe from its two endpoints in PANEL coordinates, in the LOGICAL frame. A
        /// controller classifies gestures in the panel's frame, fixed to the glass, while the user
        /// swipes relative to the interface, which rotates; at 90 or 270 the two are perpendicular.
        /// Both endpoints go through the same untransform a tap does, so only one place knows how
        /// the frames relate. `None` for no dominant axis, including both ends clamping to one edge.
        pub fn swipe_direction(&self, start: (u16, u16), end: (u16, u16)) -> Option<SwipeDir> {
                // discarded while turning, for the reason a tap is
                if self.rotating {
                        return None;
                }
                let (ax, ay) = self.untransform(i32::from(start.0), i32::from(start.1));
                let (bx, by) = self.untransform(i32::from(end.0), i32::from(end.1));
                let (dx, dy) = (bx - ax, by - ay);
                if dx == 0 && dy == 0 {
                        return None;
                }
                // the dominant axis decides; ties go to horizontal, consistently
                Some(if dx.abs() >= dy.abs() {
                        if dx > 0 { SwipeDir::Right } else { SwipeDir::Left }
                } else if dy > 0 {
                        SwipeDir::Down
                } else {
                        SwipeDir::Up
                })
        }

        // --- mutators ---

        pub fn set_visible(&mut self, id: WidgetId, visible: bool) {
                if self.w(id).visible == visible {
                        return;
                }
                self.invalidate_widget(id);
                self.w_mut(id).visible = visible;
                self.invalidate_widget(id);
                if !visible && self.focused == Some(id) {
                        self.focus_next();
                }
        }

        pub fn set_enabled(&mut self, id: WidgetId, enabled: bool) {
                if self.w(id).enabled == enabled {
                        return;
                }
                self.w_mut(id).enabled = enabled;
                self.invalidate_widget(id);
                if !enabled && self.focused == Some(id) {
                        self.focus_next();
                }
        }

        /// Constraints take effect on the next layout pass -- set them in a batch, then relayout.
        pub fn set_min_size(&mut self, id: WidgetId, w: i32, h: i32) {
                let x = self.w_mut(id);
                x.min_w = w;
                x.min_h = h;
        }

        pub fn set_max_size(&mut self, id: WidgetId, w: i32, h: i32) {
                let x = self.w_mut(id);
                x.max_w = w;
                x.max_h = h;
        }

        pub fn set_label(&mut self, id: WidgetId, label: &'static str) {
                match &mut self.w_mut(id).kind {
                        Kind::Button(b) => b.label = label,
                        Kind::Label(l) => l.text = label,
                        Kind::Window(w) => w.title = Some(label),
                }
                //   a static label supersedes any runtime text
                self.w_mut(id).text = TextSlot::EMPTY;
                self.invalidate_widget(id);
        }

        /// Runtime text for a widget: copied into the widget (up to [`TextSlot::CAP`] bytes,
        /// truncated at a char boundary), shown in place of the static label until
        /// [`set_label`](Self::set_label) or an empty string clears it. This is how a label
        /// carries a formatted value -- an elapsed time, a file name -- which a
        /// `&'static str` cannot. On a window it replaces the TITLE, and only on a window
        /// built with one: an untitled frame reserves no header band to draw into.
        pub fn set_text(&mut self, id: WidgetId, text: &str) {
                self.w_mut(id).text.set(text);
                self.invalidate_widget(id);
        }

        /// The text a widget currently SHOWS: its runtime text if [`set_text`](Self::set_text)
        /// gave it one, otherwise its static label/title (a window with no title reads empty).
        /// `None` for an id that no longer exists. Lets a caller read a list row or label back --
        /// what the viewer sees -- without reaching into the widget.
        pub fn widget_text(&self, id: WidgetId) -> Option<&str> {
                let w = self.get(id)?;
                if w.text.len > 0 {
                        return Some(w.text.as_str());
                }
                Some(match &w.kind {
                        Kind::Button(b) => b.label,
                        Kind::Label(l) => l.text,
                        Kind::Window(win) => win.title.unwrap_or(""),
                })
        }

        /// Set one row of a [`file_list!`](crate::file_list) by its tag (`tag_base + row`),
        /// showing `placeholder` for an empty string. This is the by-tag address and the
        /// empty-slot convention a selectable list needs in one place; a no-op if that row is
        /// not built (the list's page is not the one showing).
        pub fn set_list_text(&mut self, tag_base: u8, row: u8, text: &str, placeholder: &'static str) {
                if let Some(id) = self.find(tag_base.wrapping_add(row)) {
                        if text.is_empty() {
                                self.set_label(id, placeholder);
                        } else {
                                self.set_text(id, text);
                        }
                }
        }

        /// Set one row of a [`file_list!`](crate::file_list) by its tag, HIDING the row when `text`
        /// is empty instead of showing a placeholder -- so a partly-filled list collapses to just
        /// its entries. [`Relayout`](Self::relayout) after a batch of these for the freed space to
        /// close up. A no-op if that row is not built.
        pub fn set_list_row(&mut self, tag_base: u8, row: u8, text: &str) {
                if let Some(id) = self.find(tag_base.wrapping_add(row)) {
                        let filled = !text.is_empty();
                        self.set_visible(id, filled);
                        if filled {
                                self.set_text(id, text);
                        }
                }
        }

        /// Fill a whole [`file_list!`](crate::file_list) from `entries`, one per row from row 0,
        /// `placeholder` for an empty entry -- for a caller that holds the entire list rather
        /// than feeding rows one event at a time. Rows past `entries` are left as they are.
        pub fn fill_list(&mut self, tag_base: u8, entries: &[&str], placeholder: &'static str) {
                for (row, text) in entries.iter().enumerate() {
                        self.set_list_text(tag_base, row as u8, text, placeholder);
                }
        }

        /// The title-bar status symbol on a window: `Some((shape, lit, col))` places
        /// [`shape`](IndicatorShape) on title character cell `col`, `lit` choosing drawn or
        /// dark; `None` is off. A flashing light toggles `lit` at a fixed `col`, and since the
        /// symbol sits on a blank cell the title already holds the text never reflows. A no-op
        /// on a non-window.
        pub fn set_indicator(&mut self, id: WidgetId, state: Option<(IndicatorShape, bool, u16)>) {
                if let Kind::Window(w) = &mut self.w_mut(id).kind {
                        if w.indicator == state {
                                return;
                        }
                        w.indicator = state;
                        self.invalidate_widget(id);
                }
        }

        /// Set the second title row's text on a window built with [`Desc::subtitle`]. A no-op on
        /// a one-row window (or a non-window); an empty string draws a blank lower row without
        /// changing the band height, so a status that comes and goes never shifts the content.
        pub fn set_subtitle(&mut self, id: WidgetId, text: &str) {
                if let Kind::Window(w) = &mut self.w_mut(id).kind {
                        if let Some(slot) = w.subtitle.as_mut() {
                                slot.set(text);
                                self.invalidate_widget(id);
                        }
                }
        }

        // --- painting ---

        /// Draw `text` at `(x, y)` truncated to `max_width` and the canvas. The rasteriser clips
        /// per pixel; what this owes it is bounding the STRING so a long label is cut to its widget
        /// rather than painted across the clip, and refusing an origin the canvas cannot hold.
        fn draw_text_fitted(&self, c: &mut Canvas<'_>, font: &Font<'_>, x: i32, y: i32, text: &str, max_width: i32) {
                //   the truncation math uses the cell of the font actually being drawn, so it is
                // correct for any role without the toolkit knowing which one this is
                let cell_w = i32::from(font.cell_width());
                if cell_w == 0 || x < 0 || y < 0 || x >= self.width || y >= self.height {
                        return;
                }
                let fit_widget = (max_width.max(0) / cell_w) as usize;
                let fit_canvas = ((self.width - x) / cell_w) as usize;
                let mut len = text.len().min(fit_widget).min(fit_canvas).min(TEXT_MAX);
                while !text.is_char_boundary(len) {
                        len -= 1;
                }
                if len == 0 {
                        return;
                }
                c.text(font, Point::new(x, y), &text[..len]);
        }

        /// Horizontally centre `len` glyphs of cell width `cell_w` within `[x0, x1]`, never left
        /// of `x0`.
        fn centre_x(&self, x0: i32, x1: i32, len: usize, cell_w: i32) -> i32 {
                let avail = x1 - x0 + 1;
                let used = len as i32 * cell_w;
                if used >= avail { x0 } else { x0 + (avail - used) / 2 }
        }

        /// Clamp a widget's rect only as far as the rasteriser's coordinates demand; the SHAPE is
        /// drawn at its true geometry and the clip cuts it at its container's edge, so a
        /// half-scrolled widget is cropped rather than redrawn smaller.
        fn draw_rect_of(&self, r: Rect) -> Rect {
                Rect::new(r.x0.max(0), r.y0.max(0), r.x1.min(self.width - 1), r.y1.min(self.height - 1))
        }

        fn paint_window(&self, c: &mut Canvas<'_>, style: &Style<'_>, id: WidgetId, clip: &Rect) {
                let w = self.w(id);
                let win = w.window().expect("a window");
                let mut visible = w.rect;
                if !rect_intersect(&mut visible, clip) {
                        return;
                }
                let r = self.draw_rect_of(w.rect);
                //   the window owns every pixel of its rect, but its CHILDREN own theirs:
                // background goes only into the GAPS around them, so on a draw-over frame
                // (no clear -- the single-buffered scanout, where a cleared live buffer
                // flashes black under the beam) every pixel is written once, with its final
                // value. Filling the whole interior first read back as flicker on the glass:
                // the beam catching the focused button between the wash and its repaint.
                // Stacked children are in y order and disjoint; the canvas clip crops the
                // bands of scrolled-away children
                //   children own only the pixels they will PAINT: a scrolling window's
                // children clip to its viewport, so the part of a rect scrolled past it --
                // or the rows past the viewport bottom -- is background here, not child
                // territory. The first fit trusted raw rects, and the unowned strip below
                // the viewport kept every earlier frame: the jumble that taught this
                let mut owned = *clip;
                if w.is_scrolling_window() {
                        let mut vp = self.viewport(id);
                        vp.y1 = vp.y1.max(self.scroll_stop_y1(id));
                        if !rect_intersect(&mut owned, &vp) {
                                owned = Rect::new(0, 0, -1, -1);
                        }
                }
                let saved_fg = c.fg;
                //   the title band is painted once, in its own colour, BEFORE the gap loop, which
                // then starts below it -- so no pixel is written twice (a double-written pixel
                // reads as flicker on the single-buffered scanout). A titled window's band takes
                // the theme's bar colour, falling back to the background when the theme sets none
                let header_inset = if win.border { 1 } else { 0 };
                let header_bottom = if win.title.is_some() {
                        (r.y0 + header_inset + Self::title_rows(win) * self.role_ch(FontRole::Title) + 2).min(r.y1 + 1)
                } else {
                        r.y0
                };
                if header_bottom > r.y0 {
                        c.fg = self.theme.bar.unwrap_or(self.theme.bg);
                        c.rect(Point::new(r.x0, r.y0), Point::new(r.x1, header_bottom - 1), true);
                }
                c.fg = c.bg;
                let mut band_top = header_bottom;
                for child in self.children(id) {
                        let mut cr = self.w(child).rect;
                        if !rect_intersect(&mut cr, &owned) {
                                continue;
                        }
                        if cr.y0 > band_top {
                                c.rect(Point::new(r.x0, band_top), Point::new(r.x1, cr.y0 - 1), true);
                        }
                        let sy0 = cr.y0.max(band_top);
                        if sy0 <= cr.y1 {
                                if cr.x0 > r.x0 {
                                        c.rect(Point::new(r.x0, sy0), Point::new(cr.x0 - 1, cr.y1), true);
                                }
                                if cr.x1 < r.x1 {
                                        c.rect(Point::new(cr.x1 + 1, sy0), Point::new(r.x1, cr.y1), true);
                                }
                        }
                        band_top = band_top.max(cr.y1 + 1);
                }
                if band_top <= r.y1 {
                        c.rect(Point::new(r.x0, band_top), Point::new(r.x1, r.y1), true);
                }
                c.fg = self.theme.frame;
                if win.border {
                        if win.corner_radius != 0 {
                                c.rect_rounded(Point::new(r.x0, r.y0), Point::new(r.x1, r.y1), u16::from(win.corner_radius), light_draw::corner::ALL, false);
                        } else {
                                c.rect(Point::new(r.x0, r.y0), Point::new(r.x1, r.y1), false);
                        }
                }
                let Some(title) = win.title else {
                        c.fg = saved_fg;
                        return;
                };
                let title = if w.text.len > 0 { w.text.as_str() } else { title };
                // the header band the stack layout reserves: title at the very top, separator
                // under it; the two must agree on its height (cell_h + 2). A rounded corner is
                // cleared SIDEWAYS here, not downward: the title is one short string, and the
                // indent is taken at its TOP row, where the arc is furthest in
                let f = &w.rect;
                let inset = if win.border { 1 } else { 0 };
                let ty = f.y0 + inset;
                let indent = corner_indent(win.corner_radius, i32::from(win.corner_radius) - inset);
                let tx = f.x0 + indent + inset + 1;
                c.fg = self.theme.title;
                let text_right = f.x1 - indent - inset;
                self.draw_text_fitted(c, style.fonts.font(FontRole::Title), tx, ty, title, text_right - tx + 1);
                //   an INLINE status symbol, centred on the title cell the caller left blank
                // for it (e.g. the space in "REC 0:12"); sized to sit inside one cell so it
                // never touches the glyphs either side. Only drawn when lit -- the dark
                // phase shows the blank cell, so a flash never reflows the title
                if let Some((shape, true, col)) = win.indicator {
                        let (cw, ch) = (self.role_cw(FontRole::Title), self.role_ch(FontRole::Title));
                        let d = (ch * 3 / 5).clamp(3, cw);
                        let cx = tx + i32::from(col) * cw + cw / 2;
                        let cy = ty + ch / 2;
                        let (bx0, by0) = (cx - d / 2, cy - d / 2);
                        let (bx1, by1) = (bx0 + d - 1, by0 + d - 1);
                        if bx0 >= 0 && by0 >= 0 && bx1 <= text_right {
                                c.fg = self.theme.indicator;
                                match shape {
                                        IndicatorShape::Dot => {
                                                c.rect_rounded(Point::new(bx0, by0), Point::new(bx1, by1), (d / 2) as u16, light_draw::corner::ALL, true);
                                        }
                                        IndicatorShape::Play => {
                                                //   a right-pointing triangle: a vertical base at the
                                                // left, apex at the middle-right, each row a horizontal
                                                // span that tapers to the tip
                                                let mut dy = 0;
                                                while dy < d {
                                                        let wpx = d - 2 * (dy - d / 2).abs();
                                                        if wpx > 0 {
                                                                let y = by0 + dy;
                                                                c.line(Point::new(bx0, y), Point::new(bx0 + wpx - 1, y));
                                                        }
                                                        dy += 1;
                                                }
                                        }
                                }
                        }
                }
                //   the second title row, when the window carries one: a status too long for a
                // narrow bar sits here. Empty draws nothing but the band stays two rows tall
                if let Some(sub) = win.subtitle {
                        if sub.len > 0 {
                                c.fg = self.theme.title;
                                self.draw_text_fitted(c, style.fonts.font(FontRole::Title), tx, ty + self.role_ch(FontRole::Title), sub.as_str(), text_right - tx + 1);
                        }
                }
                // the separator sits below the title row(s), where the arc has come most of the way out
                c.fg = self.theme.frame;
                let sep_y = ty + Self::title_rows(win) * self.role_ch(FontRole::Title);
                let sep_indent = corner_indent(win.corner_radius, i32::from(win.corner_radius) - (sep_y - f.y0));
                let sep_x0 = f.x0 + sep_indent + inset;
                if sep_y <= f.y1 && sep_y >= 0 && sep_x0 >= 0 {
                        c.line(Point::new(sep_x0, sep_y), Point::new(f.x1 - sep_indent - inset, sep_y));
                }
                c.fg = saved_fg;
        }

        fn paint_button(&self, c: &mut Canvas<'_>, style: &Style<'_>, id: WidgetId, clip: &Rect) {
                let w = self.w(id);
                let btn = w.button().expect("a button");
                let mut visible = w.rect;
                if !rect_intersect(&mut visible, clip) {
                        return;
                }
                let r = self.draw_rect_of(w.rect);
                let focused = self.focused == Some(id);
                let pressed = matches!(self.flash, Some((f, _)) if f == id);
                let (p0, p1) = (Point::new(r.x0, r.y0), Point::new(r.x1, r.y1));
                //   a flush row's corners follow the CONTAINER'S OWN ARC -- equal radius,
                // same centre, so its curve parallels the frame's through the corner and
                // spans only the short segment inside the row -- rather than a tangent
                // quarter-circle of reduced radius, whose sweep climbed the row's sides
                // and met the frame at the corner apex (judged on the 1.69's glass)
                let flush = btn.corners == light_draw::corner::BOTTOM && btn.corner_radius != 0;
                let flush_drawn = flush && self.paint_flush_bottom(c, id, focused);
                if !flush_drawn {
                        //   a flush-marked row that is NOT docked at the frame (a scrolled
                        // list row in flight) is an ordinary square row: the frame-following
                        // corners belong to the docked position, and the old fallback --
                        // rect_rounded, whose clamp shrank and re-anchored the arc -- is the
                        // very picture this construction replaced. A button the layout gave
                        // no corner treatment wears the theme's radius on all four instead
                        let themed = btn.corners == light_draw::corner::NONE;
                        let radius = if flush {
                                0
                        } else if themed {
                                u16::from(self.theme.radius)
                        } else {
                                u16::from(btn.corner_radius)
                        };
                        let corners = if themed { light_draw::corner::ALL } else { btn.corners };
                        let saved_fg = c.fg;
                        if pressed {
                                //   the just-tapped acknowledgement: a solid inverted fill,
                                // distinct from both the resting and the focused looks (a
                                // record button is usually already focused, so reusing that
                                // would show nothing). Brief -- see ACTIVATE_FLASH_US
                                c.fg = self.theme.focus_text;
                                if radius != 0 {
                                        c.rect_rounded(p0, p1, radius, corners, true);
                                } else {
                                        c.rect(p0, p1, true);
                                }
                                c.fg = self.theme.button_outline;
                                if radius != 0 {
                                        c.rect_rounded(p0, p1, radius, corners, false);
                                } else {
                                        c.rect(p0, p1, false);
                                }
                        } else if focused {
                                //   the selection: shaded when the theme carries a focus
                                // surface -- the cell reads as LIT -- else a solid fill in
                                // the outline color
                                match self.theme.focus_surface {
                                        Some(s) if radius != 0 => c.rect_rounded_shaded(p0, p1, radius, corners, s.from, s.to),
                                        Some(s) => c.rect_shaded(p0, p1, s.from, s.to),
                                        None => {
                                                c.fg = self.theme.button_outline;
                                                if radius != 0 {
                                                        c.rect_rounded(p0, p1, radius, corners, true);
                                                } else {
                                                        c.rect(p0, p1, true);
                                                }
                                        }
                                }
                        } else {
                                //   an unfocused button owns its rect too: the interior takes
                                // its surface -- its own shade, the theme's button surface,
                                // or the flat background -- so a draw-over frame leaves
                                // nothing of the previous image inside the outline
                                match btn.shade.or(self.theme.button_surface) {
                                        Some(s) if radius != 0 => c.rect_rounded_shaded(p0, p1, radius, corners, s.from, s.to),
                                        Some(s) => c.rect_shaded(p0, p1, s.from, s.to),
                                        None => {
                                                c.fg = c.bg;
                                                if radius != 0 {
                                                        c.rect_rounded(p0, p1, radius, corners, true);
                                                } else {
                                                        c.rect(p0, p1, true);
                                                }
                                        }
                                }
                                c.fg = self.theme.button_outline;
                                if radius != 0 {
                                        c.rect_rounded(p0, p1, radius, corners, false);
                                } else {
                                        c.rect(p0, p1, false);
                                }
                        }
                        c.fg = saved_fg;
                }
                //   the label in the theme's voice: the focus text over the focused fill,
                // the button text otherwise. Uniform for 1 bpp and RGB565, since both go
                // through the same colour path
                let saved_fg = c.fg;
                c.fg = if pressed {
                        self.theme.button_outline
                } else if focused {
                        self.theme.focus_text
                } else {
                        self.theme.button_text
                };
                let label = if w.text.len > 0 { w.text.as_str() } else { btn.label };
                if !label.is_empty() {
                        // positioned from the TRUE rect, never the clamped one: centring against
                        // the clamp made a label creep as its row crossed the canvas edge
                        let f = &w.rect;
                        let (inner_x0, inner_x1) = (f.x0 + 1, f.x1 - 1);
                        let inner_h = f.y1 - f.y0 - 1;
                        let tx = self.centre_x(inner_x0, inner_x1, label.len(), self.role_cw(FontRole::Body));
                        let ty = (f.y0 + 1 + (inner_h - self.role_ch(FontRole::Body)) / 2).max(f.y0 + 1);
                        self.draw_text_fitted(c, style.fonts.font(FontRole::Body), tx, ty, label, inner_x1 - inner_x0 + 1);
                }
                c.fg = saved_fg;
                // TODO a distinct look for disabled buttons wants a colour model 1 bpp lacks
        }

        /// A flush bottom row drawn CONCENTRIC with its container's corner arcs: the same
        /// centres, radius reduced by exactly the row's inset, so the row's curve runs
        /// parallel to the frame's the whole way around the corner at a constant gap.
        /// This exists because `rect_rounded` cannot draw it: its safety clamp caps the
        /// radius at half the row's height and anchors the arc to the ROW's corner, which
        /// shrank the curve and pushed it through the frame at the apex. `false` when the
        /// geometry degenerates (no rounded parent, uneven insets, row too short) and the
        /// caller should draw the ordinary way.
        fn paint_flush_bottom(&self, c: &mut Canvas<'_>, id: WidgetId, focused: bool) -> bool {
                let w = self.w(id);
                let Some(parent) = w.parent else { return false };
                let pw = self.w(parent);
                let Some(pwin) = pw.window() else { return false };
                let pr = self.draw_rect_of(pw.rect);
                //   the clamp mirrors rect_rounded's, so these centres are the ones the
                // frame was actually drawn with
                let rad = i32::from(pwin.corner_radius).min((pr.x1 - pr.x0) / 2).min((pr.y1 - pr.y0) / 2);
                let row = self.draw_rect_of(w.rect);
                //   the parallel gap is the row's SIDE inset from the frame (exact by
                // construction); the bottom must be DOCKED at the flush edge, judged with
                // a pixel of tolerance because the scroll clamp's arithmetic may land the
                // rest position one off -- a strict test left a scrolled list's corners
                // square forever
                let side = row.x0 - pr.x0;
                if side <= 0 || pr.x1 - row.x1 != side || rad <= side {
                        return false;
                }
                let gap = pr.y1 - row.y1;
                if (gap - side).abs() > 1 || gap <= 0 {
                        return false;
                }
                let r_in = rad - side;
                let (cxl, cxr, cy) = (pr.x0 + rad, pr.x1 - rad, pr.y1 - rad);
                //   the inner arc is tangent to the row's bottom and side edges, so the
                // quadrant must fit above the row's top
                if row.y0 > cy || cxl > cxr {
                        return false;
                }

                //   the interior, span by span; doubles as the unfocused surface wash and
                // the focused fill -- below the centres the curve pulls the span ends in,
                // and a shade colors each span on its way down
                let btn_shade = match &w.kind {
                        Kind::Button(b) => b.shade,
                        _ => None,
                };
                let den = row.y1 - row.y0;
                let fill = |c: &mut Canvas<'_>, shade: Option<Shade>| {
                        let saved_fg = c.fg;
                        for y in row.y0..=row.y1 {
                                let (mut x0, mut x1) = (row.x0, row.x1);
                                if y > cy {
                                        let dy = y - cy;
                                        let s = isqrt((r_in * r_in - dy * dy).max(0) as u32) as i32;
                                        x0 = x0.max(cxl - s);
                                        x1 = x1.min(cxr + s);
                                }
                                if x0 <= x1 {
                                        if let Some(s) = shade {
                                                c.fg = lerp565(s.from, s.to, y - row.y0, den);
                                        }
                                        c.rect(Point::new(x0, y), Point::new(x1, y), true);
                                }
                        }
                        c.fg = saved_fg;
                };
                if focused {
                        if self.theme.focus_surface.is_none() {
                                let saved_fg = c.fg;
                                c.fg = self.theme.button_outline;
                                fill(c, None);
                                c.fg = saved_fg;
                        } else {
                                fill(c, self.theme.focus_surface);
                        }
                } else {
                        let wash = btn_shade.or(self.theme.button_surface);
                        if wash.is_none() {
                                let saved_fg = c.fg;
                                c.fg = c.bg;
                                fill(c, None);
                                c.fg = saved_fg;
                        } else {
                                fill(c, wash);
                        }
                        //   straight edges to the tangent points, then the concentric arcs.
                        // Inside a SCROLLING window the arcs are the corner MASK's to draw:
                        // it repaints after the children, and its erase spans (isqrt) and
                        // arc() disagree by the odd pixel -- an arc drawn here came back
                        // with bites taken out of it
                        let saved_fg = c.fg;
                        c.fg = self.theme.button_outline;
                        c.line(Point::new(row.x0, row.y0), Point::new(row.x1, row.y0));
                        c.line(Point::new(row.x0, row.y0), Point::new(row.x0, cy));
                        c.line(Point::new(row.x1, row.y0), Point::new(row.x1, cy));
                        c.line(Point::new(cxl, row.y1), Point::new(cxr, row.y1));
                        if !pw.is_scrolling_window() {
                                c.arc(Point::new(cxl, cy), r_in as u16, 90, 180);
                                c.arc(Point::new(cxr, cy), r_in as u16, 0, 90);
                        }
                        c.fg = saved_fg;
                }
                true
        }

        fn paint_label(&self, c: &mut Canvas<'_>, style: &Style<'_>, id: WidgetId, clip: &Rect) {
                let w = self.w(id);
                let Kind::Label(l) = &w.kind else { return };
                let mut visible = w.rect;
                if !rect_intersect(&mut visible, clip) {
                        return;
                }
                //   the label owns its rect: bg beneath the text, for the draw-over frames
                // where nothing else erases what was here last frame
                let r = self.draw_rect_of(w.rect);
                let saved_fg = c.fg;
                c.fg = c.bg;
                c.rect(Point::new(r.x0, r.y0), Point::new(r.x1, r.y1), true);
                c.fg = saved_fg;
                let text = if w.text.len > 0 { w.text.as_str() } else { l.text };
                let saved_fg = c.fg;
                c.fg = self.theme.text;
                self.draw_text_fitted(c, style.fonts.font(FontRole::Body), w.rect.x0, w.rect.y0, text, w.rect.x1 - w.rect.x0 + 1);
                c.fg = saved_fg;
        }

        /// `clip` is by value so each subtree narrows its own copy: a scrolling window's children
        /// paint only inside its viewport. The same narrowing happens in `hit_test`, and the two
        /// must agree: what cannot be seen must not respond.
        fn paint_clipped(&self, c: &mut Canvas<'_>, style: &Style<'_>, id: WidgetId, mut clip: Rect) {
                if !self.w(id).visible {
                        return;
                }
                // the walk's clip becomes the canvas's, so the primitives cut every shape at the
                // container edge; coordinates are safe -- the walk starts at the canvas and only
                // ever intersects
                c.set_clip(Region::new(clip.x0 as u16, clip.y0 as u16, clip.x1 as u16, clip.y1 as u16));
                match &self.w(id).kind {
                        Kind::Window(_) => self.paint_window(c, style, id, &clip),
                        Kind::Button(_) => self.paint_button(c, style, id, &clip),
                        Kind::Label(_) => self.paint_label(c, style, id, &clip),
                }
                // a scrolling window confines its children to its viewport; its own frame and
                // title were drawn against the wider clip, which keeps the frame visible while
                // content moves beneath it. The content region runs to the scroll STOP for every
                // row, so a row straddling the bottom paints into the corner band
                let scrolling = self.w(id).is_scrolling_window();
                let win_clip = clip;
                if scrolling {
                        let mut vp = self.viewport(id);
                        vp.y1 = vp.y1.max(self.scroll_stop_y1(id));
                        if !rect_intersect(&mut clip, &vp) {
                                return;
                        }
                }
                // children after the parent, in sibling order: later draws on top
                for child in self.children(id) {
                        self.paint_clipped(c, style, child, clip);
                }
                //   the curve belongs to the CONTAINER, not to whichever row is passing: a
                // rounded scrolling window re-masks its bottom corners after its children,
                // so content slides beneath a curve that never moves. Corner treatment that
                // rode the last row vanished the moment a scroll moved it off the stop
                if scrolling {
                        c.set_clip(Region::new(win_clip.x0.max(0) as u16, win_clip.y0.max(0) as u16, win_clip.x1.max(0) as u16, win_clip.y1.max(0) as u16));
                        self.paint_scroll_corner_mask(c, id);
                }
        }

        /// Erase whatever content reached outside the rounded viewport's bottom corners --
        /// the concentric curve the frame's own arcs imply at the content inset -- and
        /// redraw the border arcs over it. Runs AFTER a scrolling window's children.
        fn paint_scroll_corner_mask(&self, c: &mut Canvas<'_>, id: WidgetId) {
                let w = self.w(id);
                let Some(win) = w.window() else { return };
                let pr = self.draw_rect_of(w.rect);
                let rad = i32::from(win.corner_radius).min((pr.x1 - pr.x0) / 2).min((pr.y1 - pr.y0) / 2);
                let inset = Self::inset_x(win);
                let r_in = rad - inset;
                if r_in <= 0 {
                        return;
                }
                let (cxl, cxr, cy) = (pr.x0 + rad, pr.x1 - rad, pr.y1 - rad);
                let saved_fg = c.fg;
                c.fg = c.bg;
                for y in cy..=pr.y1 {
                        let dy = y - cy;
                        let s = if dy < r_in { isqrt((r_in * r_in - dy * dy) as u32) as i32 } else { 0 };
                        //   outside the content curve, inside the frame: erased. The border
                        // itself is repainted below
                        let left_edge = cxl - s;
                        if left_edge > pr.x0 {
                                c.rect(Point::new(pr.x0, y), Point::new(left_edge - 1, y), true);
                        }
                        let right_edge = cxr + s;
                        if right_edge < pr.x1 {
                                c.rect(Point::new(right_edge + 1, y), Point::new(pr.x1, y), true);
                        }
                }
                c.fg = self.theme.frame;
                if win.border {
                        c.arc(Point::new(cxl, cy), rad as u16, 90, 180);
                        c.arc(Point::new(cxr, cy), rad as u16, 0, 90);
                }
                //   the inner boundary -- the concentric arcs and the straight run between
                // them -- drawn HERE, after the erase, because the erase's isqrt spans and
                // arc()'s trig sampling disagree by the odd pixel and an arc drawn earlier
                // came back gap-toothed. Rendered whenever the container holds enough
                // content to scroll: the boundary belongs to the container, marking where
                // content ends against the curve, at every scroll position
                let vp = self.viewport(id);
                let scrollable = win.content_h > self.scroll_stop_y1(id) - vp.y0 + 1;
                if scrollable {
                        c.line(Point::new(cxl, cy + r_in), Point::new(cxr, cy + r_in));
                        c.arc(Point::new(cxl, cy), r_in as u16, 90, 180);
                        c.arc(Point::new(cxr, cy), r_in as u16, 0, 90);
                }
                c.fg = saved_fg;
        }

        /// Paint the whole tree onto a cleared canvas. The ENTIRE tree, not just the dirty
        /// widgets, because every frame is a full repaint; only the pushed REGION is optimised,
        /// which is where the cost that scales with panel size lives.
        pub fn paint(&self, c: &mut Canvas<'_>, style: &Style<'_>) {
                //   the theme's ground: every bg wash in the walk paints with this
                c.bg = self.theme.bg;
                if let Some(root) = self.root {
                        self.paint_clipped(c, style, root, self.canvas_rect());
                }
                // the clip is canvas state: left narrowed it would crop whatever draws next
                c.clear_clip();
        }

        /// Repaint and push, if anything is dirty. Call every pass; the layer's pacing decides
        /// when a frame happens, and this is a no-op on the passes in between. On a refused frame
        /// the dirty flag and the regions survive, so the repaint happens on a later pass.
        pub fn render<D: DisplayDriver>(&mut self, layer: &mut FrameLayer, display: &mut Display<'_, D>, style: &Style<'_>, now_us: u64) -> bool {
                //   a lapsed press flash reverts here: cleared and the button re-drawn normal.
                // Guarded against a widget the flash outlived (it should have been cleared on
                // destroy, but a stale id must never be dereferenced)
                if let Some((id, until)) = self.flash {
                        if now_us >= until {
                                self.flash = None;
                                if self.get(id).is_some() {
                                        self.invalidate_widget(id);
                                }
                        }
                }
                //   the two animations are mutually exclusive by construction, from both ends:
                // show_page declines while a rotation runs, and set_rotation defers while a
                // transition runs. The transition goes first only because it is the one that
                // hands a rotation on when it finishes; a step that finishes falls through to
                // draw the settled tree below rather than waiting a pass
                if self.page_moving {
                        match self.page_step(layer, display, style, now_us) {
                                Step::Drew => return true,
                                Step::Waiting => return false,
                                Step::Finished => {}
                        }
                }
                if self.rotating {
                        match self.rotation_step(layer, display, now_us) {
                                Step::Drew => return true,
                                Step::Waiting => return false,
                                Step::Finished => {}
                        }
                }
                if !self.dirty || self.root.is_none() {
                        return false;
                }
                let Some(mut c) = layer.frame_begin(display, now_us) else { return false };
                self.paint(&mut c, style);
                drop(c);
                self.commit(layer);
                layer.frame_end(display);
                true
        }

        /// Hand the regions invalidated since the last repaint to the layer and mark the tree
        /// clean. `render` does this; a caller running the frame itself (to time its phases, or
        /// to draw over the tree) calls it between `paint` and `frame_end`.
        pub fn commit(&mut self, layer: &mut FrameLayer) {
                if self.pending_all {
                        layer.invalidate_all();
                } else {
                        for r in self.pending.iter() {
                                layer.invalidate(*r);
                        }
                }
                self.pending.clear();
                self.pending_all = false;
                self.dirty = false;
        }
}

/// Building a page from an LUI blob into a widget tree of ANY event type. A button's emitted value
/// comes from the caller's `emit` closure, mapping the blob child (and its index) to the app's own
/// event -- so a typed app (`Ui<AppEvent>`) drives a design blob while keeping its events, and the
/// `Ui<u16>` [`build_lui`](Ui::build_lui) wrapper is just the identity mapping to the child index.
impl<A: Copy + 'static, const N: usize> Ui<A, N> {
        /// Build `page` (from a parsed [`Lui`] blob) into the widget tree, replacing any current
        /// page and laying it out to the canvas. Each button emits `emit(index, child)`. The blob
        /// must be `'static` -- its strings become the widgets' `&'static str`. There is no page
        /// transition; this is an in-place build.
        pub fn build_lui_with(&mut self, page: &LuiPage<'static>, emit: impl Fn(usize, &LuiChild<'static>) -> Option<A>) -> Result<(), Error> {
                if let Some(root) = self.root {
                        self.destroy(root);
                }
                let win = self.create_window(None, Rect::new(0, 0, 0, 0), Some(page.title()), page.subtitle())?;
                //   scroll before the children, since it changes how the stack lays out rows that
                // do not fit (mirrors build_desc)
                if page.scroll() {
                        if let Some(w) = self.w_mut(win).window_mut() {
                                w.scroll = scroll::VERTICAL;
                        }
                }
                //   a single running index over every widget built, so a button's emit value and a
                // default tag (index + 1, when the design gives none) stay unique across a frame's
                // children too; on a flat page it is just the child position, as before
                let mut index = 0usize;
                for child in page.children() {
                        if child.kind == lui::code::KIND_FRAME {
                                //   a frame is a titleless window with its own layout: create it,
                                // size it, fill it with its (leaf) children, then lay it out. It
                                // takes ONLY an explicit tag -- a structural container is never
                                // addressed by position, and an auto tag would collide with an
                                // app's own (a frame landing on TAG_PLAY once wore "Play last" as a
                                // title). It also consumes no emit index, for the same reason
                                let frame = self.create_window(Some(win), Rect::new(0, 0, 0, 0), None, false)?;
                                if child.scroll() != 0 {
                                        if let Some(w) = self.w_mut(frame).window_mut() {
                                                w.scroll = child.scroll();
                                        }
                                }
                                self.apply_lui_size(frame, &child);
                                self.w_mut(frame).tag = child.tag;
                                for sub in child.children() {
                                        let id = self.create_lui_leaf(frame, &sub, index, &emit)?;
                                        self.apply_lui_leaf_box(id, &sub, index);
                                        index += 1;
                                }
                                match child.layout() {
                                        lui::code::LAYOUT_ROW => self.layout_row(frame, child.gap()),
                                        lui::code::LAYOUT_LINEAR => self.layout_linear(frame, child.gap()),
                                        _ => self.layout_stack(frame, child.gap()),
                                }
                        } else {
                                let id = self.create_lui_leaf(win, &child, index, &emit)?;
                                self.apply_lui_leaf_box(id, &child, index);
                                index += 1;
                        }
                }
                match page.layout() {
                        lui::code::LAYOUT_ROW => self.layout_row(win, page.gap()),
                        lui::code::LAYOUT_LINEAR => self.layout_linear(win, page.gap()),
                        _ => self.layout_stack(win, page.gap()),
                }
                self.relayout();
                Ok(())
        }

        /// Create a leaf child (a button or label) under `parent`, its button emitting
        /// `emit(index, child)`.
        fn create_lui_leaf(&mut self, parent: WidgetId, child: &LuiChild<'static>, index: usize, emit: &impl Fn(usize, &LuiChild<'static>) -> Option<A>) -> Result<WidgetId, Error> {
                match child.kind {
                        lui::code::KIND_LABEL => self.create_label(Some(parent), Rect::new(0, 0, 0, 0), child.text),
                        //   a button, or any unknown kind treated as one; its emitted value is the
                        // caller's to decide from the child
                        _ => self.create_button(Some(parent), Rect::new(0, 0, 0, 0), child.text, emit(index, child), Nav::Stay),
                }
        }

        /// Apply a blob child's size metrics (min/max, grow) to a built widget.
        fn apply_lui_size(&mut self, id: WidgetId, child: &LuiChild<'static>) {
                let w = self.w_mut(id);
                w.min_w = i32::from(child.min_w);
                w.min_h = i32::from(child.min_h);
                w.max_w = i32::from(child.max_w);
                w.max_h = i32::from(child.max_h);
                w.grow = child.grow;
        }

        /// A leaf's size plus its tag -- the design's, or `index + 1` as a default (tag 0 stays
        /// "untagged"), so a caller can find a leaf by a stable id or by position. Only leaves get
        /// the positional default; a frame takes an explicit tag only (see `build_lui_with`).
        fn apply_lui_leaf_box(&mut self, id: WidgetId, child: &LuiChild<'static>, index: usize) {
                self.apply_lui_size(id, child);
                self.w_mut(id).tag = if child.tag != 0 { child.tag } else { (index + 1) as u8 };
        }

        /// Navigate to an LUI-blob `page` WITH the page-transition animation -- the blob analogue of
        /// [`navigate`](Self::navigate) (`back` false) and [`navigate_back`](Self::navigate_back)
        /// (`back` true). The blob carries no parent links, so the caller keeps its own history and
        /// names both the target page and the direction. Buttons emit `emit(index, child)`, exactly
        /// as [`build_lui_with`](Self::build_lui_with) builds them; the difference is the slide.
        ///
        /// The transition's axis is seeded from the entering page's layout and the tree's
        /// [`layout_axis`](Self::set_layout_axis) -- a `Row`, or a `Linear` tree set horizontal,
        /// rises from the bottom; anything else slides in from the right -- unless
        /// [`set_default_descent`](Self::set_default_descent) pins the flow. It mirrors forward and
        /// back correctly when a tree's pages share an axis, which is what the layout-axis model
        /// encourages (the same seed the const-`Page` path uses for a `Linear` tree).
        pub fn navigate_lui(&mut self, page: &LuiPage<'static>, back: bool, emit: impl Fn(usize, &LuiChild<'static>) -> Option<A>) -> Result<(), Error> {
                //   start the transition before the tree changes, as show_page does for a const
                // Page: the outgoing image is captured at the first render step, off the live panel,
                // so destroying the old widget tree now (inside build_lui_with) is fine
                if self.root.is_some() && !self.rotating {
                        self.page_moving = true;
                        self.page_move_started = false;
                        self.page_move_back = back;
                        let horizontal = match page.layout() {
                                lui::code::LAYOUT_ROW => true,
                                lui::code::LAYOUT_LINEAR => self.layout_axis == Axis::Horizontal,
                                _ => false,
                        };
                        let seed = if horizontal { Descent::FromBottom } else { Descent::FromRight };
                        self.page_move_descent = self.default_descent.unwrap_or(seed);
                }
                self.build_lui_with(page, emit)?;
                self.invalidate_all();
                Ok(())
        }
}

/// The `Ui<u16>` convenience over [`build_lui_with`](Ui::build_lui_with): a blob UI whose event type
/// is `u16` -- each button emits its child index, which [`LuiRuntime`](crate::LuiRuntime) resolves
/// back to the blob child's navigation and app event.
impl<const N: usize> Ui<u16, N> {
        pub fn build_lui(&mut self, page: &LuiPage<'static>) -> Result<(), Error> {
                self.build_lui_with(page, |i, _| Some(i as u16))
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use light_display::{DisplayDriver, Frame, Region};
        use light_draw::PixelFormat;
        use light_core::hal::Clock;
        use light_font::Encoder;
        extern crate std;
        use std::vec::Vec as StdVec;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Ev {
                Alpha,
                Beta,
                Item(u8),
        }

        static BTN_ALPHA: Desc<Ev> = Desc::button("Alpha").emit(Ev::Alpha);
        static BTN_BETA: Desc<Ev> = Desc::button("Beta").emit(Ev::Beta);
        static BTN_MORE: Desc<Ev> = Desc::button("More >").navigate(&PAGE_DETAIL);
        static MAIN: Desc<Ev> = Desc::window("Main").rounded(8).stack(2).children(&[&BTN_ALPHA, &BTN_BETA, &BTN_MORE]);
        static LBL: Desc<Ev> = Desc::label("swipe right to go back");
        static BTN_BACK: Desc<Ev> = Desc::button("< Back").back();
        static DETAIL: Desc<Ev> = Desc::window("More").stack(1).children(&[&LBL, &BTN_BACK]);
        static PAGE_MAIN: Page<Ev> = Page::new(&MAIN, None);
        static PAGE_DETAIL: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN));

        static ITEM_1: Desc<Ev> = Desc::button("Item 1").emit(Ev::Item(1)).min_size(0, 20);
        static ITEM_2: Desc<Ev> = Desc::button("Item 2").emit(Ev::Item(2)).min_size(0, 20);
        static ITEM_3: Desc<Ev> = Desc::button("Item 3").emit(Ev::Item(3)).min_size(0, 20);
        static ITEM_4: Desc<Ev> = Desc::button("Item 4").emit(Ev::Item(4)).min_size(0, 20);
        static ITEM_5: Desc<Ev> = Desc::button("Item 5").emit(Ev::Item(5)).min_size(0, 20);
        static LIST: Desc<Ev> = Desc::frame().stack(0).scroll(scroll::VERTICAL).children(&[&ITEM_1, &ITEM_2, &ITEM_3, &ITEM_4, &ITEM_5]);
        static PAGE_LIST: Page<Ev> = Page::new(&LIST, None);

        static COL_1: Desc<Ev> = Desc::button("C1").emit(Ev::Item(1)).min_size(20, 0);
        static COL_2: Desc<Ev> = Desc::button("C2").emit(Ev::Item(2)).min_size(20, 0);
        static COL_3: Desc<Ev> = Desc::button("C3").emit(Ev::Item(3)).min_size(20, 0);
        static COL_4: Desc<Ev> = Desc::button("C4").emit(Ev::Item(4)).min_size(20, 0);
        static COL_5: Desc<Ev> = Desc::button("C5").emit(Ev::Item(5)).min_size(20, 0);
        static ROW_LIST: Desc<Ev> = Desc::frame().row(0).scroll(scroll::HORIZONTAL).children(&[&COL_1, &COL_2, &COL_3, &COL_4, &COL_5]);
        static PAGE_ROW: Page<Ev> = Page::new(&ROW_LIST, None);

        static PIN: Desc<Ev> = Desc::button("<").emit(Ev::Beta).min_size(12, 0).max_size(12, 0);
        static STRIP: Desc<Ev> = Desc::frame().row(0).scroll(scroll::HORIZONTAL).children(&[&COL_1, &COL_2, &COL_3, &COL_4, &COL_5]);
        static PINNED: Desc<Ev> = Desc::frame().row(2).children(&[&PIN, &STRIP]);
        static PAGE_PINNED: Page<Ev> = Page::new(&PINNED, None);

        //   a grow child flanked by pinned columns on BOTH sides, and the same row without the
        // grow (the last-absorbs-remainder default) as its control
        static PIN_R: Desc<Ev> = Desc::button(">").emit(Ev::Alpha).min_size(12, 0).max_size(12, 0);
        static GROW_STRIP: Desc<Ev> = Desc::frame().grow().row(0).scroll(scroll::HORIZONTAL).children(&[&COL_1, &COL_2, &COL_3, &COL_4, &COL_5]);
        static GROW_ROW: Desc<Ev> = Desc::frame().row(2).children(&[&PIN, &GROW_STRIP, &PIN_R]);
        static PAGE_GROW: Page<Ev> = Page::new(&GROW_ROW, None);
        static NOGROW_STRIP: Desc<Ev> = Desc::frame().row(0).scroll(scroll::HORIZONTAL).children(&[&COL_1, &COL_2, &COL_3, &COL_4, &COL_5]);
        static NOGROW_ROW: Desc<Ev> = Desc::frame().row(2).children(&[&PIN, &NOGROW_STRIP, &PIN_R]);
        static PAGE_NOGROW: Page<Ev> = Page::new(&NOGROW_ROW, None);

        //   the same stack content (layout seed would be Left) pinned to each cardinal, to
        // prove a per-page override beats the seed and the tree default
        static PAGE_D_BOTTOM: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN)).descend(Descent::FromBottom);
        static PAGE_D_TOP: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN)).descend(Descent::FromTop);
        static PAGE_D_RIGHT: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN)).descend(Descent::FromRight);
        static PAGE_D_LEFT: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN)).descend(Descent::FromLeft);

        //   generic layouts: the same descriptor lays out along whichever axis the tree carries
        static LINEAR_WIN: Desc<Ev> = Desc::window("Lin").linear(2).children(&[&BTN_ALPHA, &BTN_BETA]);
        static PAGE_LINEAR: Page<Ev> = Page::new(&LINEAR_WIN, None);
        static LINEAR_WIN_2: Desc<Ev> = Desc::window("Lin2").linear(2).children(&[&BTN_ALPHA, &BTN_BETA]);
        static PAGE_LINEAR_2: Page<Ev> = Page::new(&LINEAR_WIN_2, None);

        //   the reusable selectable-list component: rows generated once, each tagged and
        // emitting its own index. PICK_ROWS_BACK also appends a back row.
        crate::file_list! {
                PICK_ROWS,
                event: Ev,
                tag_base: 0x30u8,
                min_size: (0, 20),
                select: |i| Ev::Item(i),
                indices: [0, 1, 2],
        }
        static PICK_WIN: Desc<Ev> = Desc::window("Pick").stack(0).children(PICK_ROWS);
        static PICK_PAGE: Page<Ev> = Page::new(&PICK_WIN, None);
        crate::file_list! {
                PICK_ROWS_BACK,
                event: Ev,
                tag_base: 0x40u8,
                min_size: (0, 20),
                select: |i| Ev::Item(i),
                indices: [0, 1],
                back: &BTN_BACK,
        }

        //   a two-row title bar (a subtitle) vs the ordinary one-row bar, same content
        static SUB_WIN: Desc<Ev> = Desc::window("Bar").subtitle().stack(0).children(&[&BTN_ALPHA]);
        static SUB_PAGE: Page<Ev> = Page::new(&SUB_WIN, None);
        static PLAIN_WIN: Desc<Ev> = Desc::window("Bar").stack(0).children(&[&BTN_ALPHA]);
        static PLAIN_PAGE: Page<Ev> = Page::new(&PLAIN_WIN, None);

        fn font_blob() -> StdVec<u8> {
                let mut e = Encoder::new(4, 6, 5, 6);
                for c in 0x20u8..0x7f {
                        e.add(c, &[0xF0; 6]).unwrap();
                }
                e.encode()
        }

        /// A style over one font for every role, on the default theme -- what these tests bind and
        /// render with; a test that wants a specific theme sets it separately with `set_theme`,
        /// which `render` reads from the [`Ui`], not from this style's theme field.
        fn styled<'a>(font: &'a Font<'a>) -> Style<'a> {
                Style::new(Theme::DEFAULT, Fonts::uniform(font))
        }

        struct Mock {
                pushed: StdVec<Region>,
        }
        impl DisplayDriver for Mock {
                fn init(&mut self, _: &mut dyn Clock, _: u16, _: u16) {}
                fn chunk_count(&self, _: &Region) -> u16 {
                        1
                }
                fn chunks_per_poll(&self, _: &Region) -> u16 {
                        0
                }
                fn kick(&mut self, _: &Frame<'_>, r: &Region, _: u16) {
                        self.pushed.push(*r);
                }
                fn chunk_complete(&mut self) -> bool {
                        true
                }
                fn chunk_timeout_ms(&self) -> u32 {
                        10
                }
        }

        fn now() -> u64 {
                0
        }

        /// A 64x48 mono rig, like a small OLED.
        fn rig(buf: &mut [u8]) -> (FrameLayer, Display<'_, Mock>) {
                let display = Display::new(Mock { pushed: StdVec::new() }, buf, 64, 48, PixelFormat::Mono1, now);
                (FrameLayer::new(64, 48, PixelFormat::Mono1), display)
        }

        fn flush(layer: &mut FrameLayer, display: &mut Display<'_, Mock>) -> StdVec<Region> {
                while layer.poll(display).unwrap() {}
                core::mem::take(&mut display.driver().pushed)
        }

        #[test]
        fn a_stack_divides_the_content_area_and_the_last_row_takes_the_remainder() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                let root = ui.root().unwrap();
                let rows: StdVec<Rect> = ui.children(root).map(|c| ui.get(c).unwrap().rect).collect();
                assert_eq!(rows.len(), 3);
                // rows abut with the gap between, span the content width, and are in order
                assert_eq!(rows[1].y0, rows[0].y1 + 3);
                assert_eq!(rows[2].y0, rows[1].y1 + 3);
                assert!(rows[0].x0 == rows[1].x0 && rows[0].x1 == rows[2].x1);
                // the last row reaches the flush edge of the rounded frame (y1 - inset_x)
                assert_eq!(rows[2].y1, 47 - 3);
                // and the whole thing sits below the header band: border + cell + 2
                assert!(rows[0].y0 >= 1 + 6 + 2);
                // the flush last row is a rounded-bottom button with hit slop to the canvas edge
                let last = ui.children(root).last().unwrap();
                let b = ui.get(last).unwrap();
                assert_eq!(b.button().unwrap().corners, light_draw::corner::BOTTOM);
                assert_eq!(b.hit_slop_y1, 3);
        }

        #[test]
        fn focus_cycles_through_buttons_in_order_and_wraps() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                // the first button took focus on creation
                assert_eq!(ui.activate(), Some(Ev::Alpha));
                ui.focus_next();
                assert_eq!(ui.activate(), Some(Ev::Beta));
                ui.focus_prev();
                ui.focus_prev();
                // wrapped from Alpha to the last button, which navigates
                assert_eq!(ui.activate(), None);
                assert!(core::ptr::eq(ui.page().unwrap(), &PAGE_DETAIL));
                // the new page's first button has focus; back returns to Main
                assert_eq!(ui.activate(), None);
                assert!(core::ptr::eq(ui.page().unwrap(), &PAGE_MAIN));
                assert!(!ui.navigate_back(), "top-level page: nowhere to go");
        }

        #[test]
        fn a_tap_activates_on_release_at_its_start_point_and_a_wander_does_not() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                let root = ui.root().unwrap();
                let beta = ui.children(root).nth(1).unwrap();
                let r = ui.get(beta).unwrap().rect;
                let (cx, cy) = (((r.x0 + r.x1) / 2) as u16, ((r.y0 + r.y1) / 2) as u16);
                assert_eq!(ui.touch(cx, cy, true, 0), Touch::Pending);
                // a wobble within the slop, held past the minimum, is still a tap
                assert_eq!(ui.touch(cx + 3, cy + 2, true, 10_000), Touch::Pending);
                assert_eq!(ui.touch(cx + 3, cy + 2, false, 60_000), Touch::Tap { hit: true, emitted: Some(Ev::Beta) });
                assert_eq!(ui.focused(), Some(beta));
                // travel beyond the slop with nothing scrollable underneath: neither tap nor drag
                assert_eq!(ui.touch(cx, cy, true, 100_000), Touch::Pending);
                assert_eq!(ui.touch(cx + 30, cy, true, 110_000), Touch::Pending);
                assert_eq!(ui.touch(cx + 30, cy, false, 200_000), Touch::None);
                // a press too brief to be deliberate is dropped, even dead on the widget
                assert_eq!(ui.touch(cx, cy, true, 300_000), Touch::Pending);
                assert_eq!(ui.touch(cx, cy, false, 300_000 + TAP_MIN_HOLD_US / 2), Touch::None);
                // and a real tap on empty space (the header) is reported, not swallowed
                assert_eq!(ui.touch(30, 2, true, 400_000), Touch::Pending);
                assert_eq!(ui.touch(30, 2, false, 460_000), Touch::Tap { hit: false, emitted: None });
        }

        #[test]
        fn a_scrolling_stack_overflows_and_a_drag_moves_it_within_the_clamp() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_LIST).unwrap();
                let root = ui.root().unwrap();
                let win = ui.get(root).unwrap().window().unwrap().clone();
                // five rows pinned at 20 px in a 48 px frame: the content overflows
                assert_eq!(win.content_h, 100);
                let first = ui.children(root).next().unwrap();
                let y_before = ui.get(first).unwrap().rect.y0;
                // a drag upward from the middle of the list
                assert_eq!(ui.touch(32, 30, true, 0), Touch::Pending);
                assert_eq!(ui.touch(32, 10, true, 10_000), Touch::Drag);
                assert_eq!(ui.get(first).unwrap().rect.y0, y_before - 20);
                assert_eq!(ui.touch(32, 10, false, 20_000), Touch::DragEnd);
                // scrolling past the end is clamped: the content's far edge never passes the stop
                assert!(ui.scroll_by(root, 0, 1000));
                let vp_y1 = ui.viewport(root).y1;
                let last = ui.children(root).last().unwrap();
                assert_eq!(ui.get(last).unwrap().rect.y1, vp_y1);
                assert!(!ui.scroll_by(root, 0, 1), "nothing left to scroll");
                // the part of a widget above the viewport is untouchable even though its rect
                // covers the point (row 3 spans y = -15..4 here; the viewport starts at 3), and
                // focusing a widget scrolled out brings it in
                assert_eq!(ui.touch(32, 2, true, 100_000), Touch::Pending);
                assert_eq!(ui.touch(32, 2, false, 160_000), Touch::Tap { hit: false, emitted: None });
                assert_eq!(ui.touch(32, 4, true, 200_000), Touch::Pending);
                assert_eq!(ui.touch(32, 4, false, 260_000), Touch::Tap { hit: true, emitted: Some(Ev::Item(3)) });
                ui.set_focus(Some(first));
                assert_eq!(ui.get(first).unwrap().rect.y0, ui.viewport(root).y0);
        }

        #[test]
        fn a_scrolling_row_overflows_sideways_and_a_drag_moves_it_within_the_clamp() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_ROW).unwrap();
                let root = ui.root().unwrap();
                let win = ui.get(root).unwrap().window().unwrap().clone();
                // five columns pinned at 20 px in a 64 px frame: the content overflows
                assert_eq!(win.content_w, 100);
                // a scrolling row's columns keep their measured widths: no absorbed remainder
                let last = ui.children(root).last().unwrap();
                let last_rect = ui.get(last).unwrap().rect;
                assert_eq!(last_rect.x1 - last_rect.x0 + 1, 20);
                let first = ui.children(root).next().unwrap();
                let x_before = ui.get(first).unwrap().rect.x0;
                // a drag leftward from the middle of the row pulls the content with it
                assert_eq!(ui.touch(40, 24, true, 0), Touch::Pending);
                assert_eq!(ui.touch(20, 24, true, 10_000), Touch::Drag);
                assert_eq!(ui.get(first).unwrap().rect.x0, x_before - 20);
                assert_eq!(ui.touch(20, 24, false, 20_000), Touch::DragEnd);
                // scrolling past the end is clamped: the last column rests at the frame's edge
                assert!(ui.scroll_by(root, 1000, 0));
                assert_eq!(ui.get(last).unwrap().rect.x1, ui.viewport(root).x1);
                assert!(!ui.scroll_by(root, 1, 0), "nothing left to scroll");
                // and the row never scrolls vertically, whatever a drag asks for
                assert!(!ui.scroll_by(root, 0, 10));
                // the part of a column left of the viewport is untouchable even though its
                // rect covers the point
                let vp_x0 = ui.viewport(root).x0;
                if vp_x0 > 0 {
                        assert_eq!(ui.touch((vp_x0 - 1) as u16, 24, true, 100_000), Touch::Pending);
                        assert_eq!(ui.touch((vp_x0 - 1) as u16, 24, false, 160_000), Touch::Tap { hit: false, emitted: None });
                }
        }

        #[test]
        fn a_column_pinned_beside_a_scrolling_strip_holds_still_while_the_strip_drags() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_PINNED).unwrap();
                let root = ui.root().unwrap();
                let (pin, strip) = {
                        let mut kids = ui.children(root);
                        (kids.next().unwrap(), kids.next().unwrap())
                };
                // the strip's interior was laid out against its PLACED rect, not the
                // placeholder it was built with: its first column starts inside it
                let strip_rect = ui.get(strip).unwrap().rect;
                let first = ui.children(strip).next().unwrap();
                assert!(ui.get(first).unwrap().rect.x0 > strip_rect.x0);
                assert!(strip_rect.x0 > ui.get(pin).unwrap().rect.x1);
                // the strip overflows and scrolls; the outer row does not
                assert!(ui.get(strip).unwrap().window().unwrap().content_w > strip_rect.x1 - strip_rect.x0 + 1);
                assert!(!ui.scroll_by(root, 10, 0));
                // a drag over the strip pulls its columns and leaves the pinned column alone
                let pin_rect = ui.get(pin).unwrap().rect;
                let x_before = ui.get(first).unwrap().rect.x0;
                let sx = (strip_rect.x0 + 4) as u16;
                assert_eq!(ui.touch(sx + 17, 24, true, 0), Touch::Pending);
                assert_eq!(ui.touch(sx, 24, true, 10_000), Touch::Drag);
                assert_eq!(ui.get(first).unwrap().rect.x0, x_before - 17);
                assert_eq!(ui.get(pin).unwrap().rect, pin_rect);
                assert_eq!(ui.touch(sx, 24, false, 20_000), Touch::DragEnd);
                // the pinned column still answers a tap
                let (px, py) = (pin_rect.x0 as u16, (pin_rect.y0 + 2) as u16);
                assert_eq!(ui.touch(px, py, true, 100_000), Touch::Pending);
                assert_eq!(ui.touch(px, py, false, 160_000), Touch::Tap { hit: true, emitted: Some(Ev::Beta) });
        }

        #[test]
        fn a_grow_child_takes_the_surplus_so_pinned_columns_flank_it() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 12> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let read = |ui: &Ui<Ev, 12>| {
                        let root = ui.root().unwrap();
                        let mut kids = ui.children(root);
                        let l = ui.get(kids.next().unwrap()).unwrap().rect;
                        let s = ui.get(kids.next().unwrap()).unwrap().rect;
                        let r = ui.get(kids.next().unwrap()).unwrap().rect;
                        (l, s, r)
                };
                let gap = 2;

                ui.navigate(&PAGE_GROW).unwrap();
                let (gl, gs, gr) = read(&ui);
                // both pinned columns keep their 12 px, wherever they sit
                assert_eq!(gl.x1 - gl.x0 + 1, 12);
                assert_eq!(gr.x1 - gr.x0 + 1, 12);
                // left pin, then the strip, then the right pin -- contiguous with the gap between
                assert_eq!(gs.x0, gl.x1 + 1 + gap);
                assert_eq!(gr.x0, gs.x1 + 1 + gap);

                ui.navigate(&PAGE_NOGROW).unwrap();
                let (_nl, ns, nr) = read(&ui);
                // grow gave the strip the surplus that the equal-split default strands after the
                // last column, so the strip is wider and the trailing pin sits flush at the edge
                // instead of floating mid-row (the bug this guards)
                assert!(gs.x1 - gs.x0 + 1 > ns.x1 - ns.x0 + 1, "grow widened the strip");
                assert!(gr.x1 > nr.x1, "grow pushed the trailing pin to the edge");
        }

        #[test]
        fn a_row_page_drops_in_vertically_and_lifts_out_while_a_stack_page_slides() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let mut now = 0u64;
                let mut settle = |ui: &mut Ui<Ev, 8>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, now: &mut u64| {
                        let mut frames = 0;
                        while ui.is_animating() {
                                *now += 50_000;
                                ui.render(layer, display, &styled(&font), *now);
                                while layer.poll(display).unwrap() {}
                                frames += 1;
                                assert!(frames < 100, "a transition that never ends");
                        }
                };
                ui.navigate(&PAGE_MAIN).unwrap();
                settle(&mut ui, &mut layer, &mut display, &mut now);
                // forward into a Row page: the incoming enters from logical DOWN (+y), cover-style
                ui.navigate_returning(&PAGE_PINNED, &PAGE_MAIN).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, 1));
                settle(&mut ui, &mut layer, &mut display, &mut now);
                // back OUT of the Row page: mirrored -- the motion reverses to the other end
                assert!(ui.navigate_back());
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, -1));
                settle(&mut ui, &mut layer, &mut display, &mut now);
                // forward into a Stack page: the horizontal slide as ever
                ui.navigate(&PAGE_DETAIL).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (1, 0));
        }

        //   a minimal LUI blob assembled by hand -- crush's compiler is std/heavy and lives in
        // another crate, so the reader/transition are tested off a literal blob in the v2 layout:
        // two Linear pages, one button each carrying an app-event id
        fn lui_blob() -> StdVec<u8> {
                fn put_str(b: &mut StdVec<u8>, s: &str) {
                        b.push(s.len() as u8);
                        b.extend_from_slice(s.as_bytes());
                }
                let page = |title: &str, btn: &str, event: u16| {
                        let mut b = StdVec::new();
                        put_str(&mut b, title);
                        b.push(crate::lui::code::LAYOUT_LINEAR);
                        b.push(2); // gap
                        b.push(0); // scroll
                        b.push(0); // subtitle
                        b.push(1); // one child
                        b.push(crate::lui::code::KIND_BUTTON);
                        b.push(crate::lui::code::NAV_NONE);
                        b.extend_from_slice(&0u16.to_le_bytes()); // nav_page
                        b.extend_from_slice(&event.to_le_bytes());
                        b.push(0); // tag (0 -> default index+1)
                        b.extend_from_slice(&0u16.to_le_bytes()); // min_w
                        b.extend_from_slice(&0u16.to_le_bytes()); // min_h
                        b.extend_from_slice(&0u16.to_le_bytes()); // max_w
                        b.extend_from_slice(&0u16.to_le_bytes()); // max_h
                        b.push(0); // grow
                        put_str(&mut b, btn);
                        b
                };
                let bodies = [page("One", "Go", 7), page("Two", "Back", 8)];
                let mut blob = StdVec::new();
                blob.extend_from_slice(b"LUI3");
                blob.extend_from_slice(&2u16.to_le_bytes()); // page_count
                blob.extend_from_slice(&0u16.to_le_bytes()); // root
                blob.extend_from_slice(&64u16.to_le_bytes()); // width
                blob.extend_from_slice(&48u16.to_le_bytes()); // height
                blob.extend_from_slice(&0u16.to_le_bytes()); // corner
                blob.extend_from_slice(&0u16.to_le_bytes()); // reserved
                let mut off = (16 + 4 * bodies.len()) as u32;
                for body in &bodies {
                        blob.extend_from_slice(&off.to_le_bytes());
                        off += body.len() as u32;
                }
                for body in &bodies {
                        blob.extend_from_slice(body);
                }
                blob
        }

        /// [`navigate_lui`](Ui::navigate_lui) slides between blob pages with the page transition
        /// (the first page snaps -- nothing to slide from), maps each button's blob event through the
        /// closure, and mirrors forward/back on the same axis -- the parity the dictaphone needs.
        #[test]
        fn navigate_lui_slides_between_blob_pages_and_mirrors_on_back() {
                let fb = font_blob();
                let font = Font::parse(&fb).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let data: &'static [u8] = StdVec::leak(lui_blob());
                let lui = crate::Lui::parse(data).unwrap();
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let map = |_i: usize, c: &crate::LuiChild<'static>| Some(Ev::Item(c.event as u8));
                let mut now = 0u64;
                let mut settle = |ui: &mut Ui<Ev, 8>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, now: &mut u64| {
                        let mut frames = 0;
                        while ui.is_animating() {
                                *now += 50_000;
                                ui.render(layer, display, &styled(&font), *now);
                                while layer.poll(display).unwrap() {}
                                frames += 1;
                                assert!(frames < 100, "a transition that never ends");
                        }
                };
                //   first page: nothing to slide from, so it snaps; the button maps its blob event
                ui.navigate_lui(&lui.page(0).unwrap(), false, map).unwrap();
                assert!(!ui.is_animating(), "the first page does not slide");
                assert_eq!(ui.widget_text(ui.find(1).unwrap()), Some("Go"));
                //   forward: a Linear tree on the default vertical axis slides on the horizontal
                ui.navigate_lui(&lui.page(1).unwrap(), false, map).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert!(ui.is_animating(), "forward navigation slides");
                let fwd = (ui.page_move_dx, ui.page_move_dy);
                assert_eq!(fwd.1, 0, "a vertical-axis Linear tree slides horizontally");
                settle(&mut ui, &mut layer, &mut display, &mut now);
                assert_eq!(ui.widget_text(ui.find(1).unwrap()), Some("Back"));
                //   back: the mirror -- same axis, reversed direction
                ui.navigate_lui(&lui.page(0).unwrap(), true, map).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (-fwd.0, 0), "back mirrors forward");
                settle(&mut ui, &mut layer, &mut display, &mut now);
                assert_eq!(ui.widget_text(ui.find(1).unwrap()), Some("Go"));
        }

        //   one page: a leaf button then a frame (horizontal-scroll linear strip) of two rows -- the
        // one-level nesting the wide dictaphone needs
        fn nested_lui_blob() -> StdVec<u8> {
                fn put_str(b: &mut StdVec<u8>, s: &str) {
                        b.push(s.len() as u8);
                        b.extend_from_slice(s.as_bytes());
                }
                fn prefix(b: &mut StdVec<u8>, kind: u8, tag: u8, grow: bool) {
                        b.push(kind);
                        b.push(crate::lui::code::NAV_NONE);
                        b.extend_from_slice(&0u16.to_le_bytes()); // nav_page
                        b.extend_from_slice(&0u16.to_le_bytes()); // event
                        b.push(tag);
                        b.extend_from_slice(&0u16.to_le_bytes()); // min_w
                        b.extend_from_slice(&0u16.to_le_bytes()); // min_h
                        b.extend_from_slice(&0u16.to_le_bytes()); // max_w
                        b.extend_from_slice(&0u16.to_le_bytes()); // max_h
                        b.push(grow as u8);
                }
                let mut body = StdVec::new();
                put_str(&mut body, "P");
                body.push(crate::lui::code::LAYOUT_LINEAR);
                body.push(2); // gap
                body.push(0); // scroll
                body.push(0); // subtitle
                body.push(2); // two top-level children
                prefix(&mut body, crate::lui::code::KIND_BUTTON, 5, false);
                put_str(&mut body, "top");
                //   the frame: UNtagged (like the real scrolling strip), grows; then
                // layout/gap/scroll/count, then two leaf rows
                prefix(&mut body, crate::lui::code::KIND_FRAME, 0, true);
                body.push(crate::lui::code::LAYOUT_LINEAR);
                body.push(2); // gap
                body.push(crate::scroll::HORIZONTAL);
                body.push(2); // two children
                prefix(&mut body, crate::lui::code::KIND_BUTTON, 0x30, false);
                put_str(&mut body, "R0");
                prefix(&mut body, crate::lui::code::KIND_BUTTON, 0x31, false);
                put_str(&mut body, "R1");

                let mut blob = StdVec::new();
                blob.extend_from_slice(b"LUI3");
                blob.extend_from_slice(&1u16.to_le_bytes()); // page_count
                blob.extend_from_slice(&0u16.to_le_bytes()); // root
                blob.extend_from_slice(&64u16.to_le_bytes()); // width
                blob.extend_from_slice(&48u16.to_le_bytes()); // height
                blob.extend_from_slice(&0u16.to_le_bytes()); // corner
                blob.extend_from_slice(&0u16.to_le_bytes()); // reserved
                blob.extend_from_slice(&((16 + 4) as u32).to_le_bytes()); // page 0 offset
                blob.extend_from_slice(&body);
                blob
        }

        /// build_lui_with realises a one-level frame: the frame's leaf rows are built under it and
        /// findable by tag, alongside the top-level leaf -- what puts the landscape dictaphone's
        /// scrolling strip on the data path.
        #[test]
        fn build_lui_with_builds_a_nested_frame() {
                let fb = font_blob();
                let font = Font::parse(&fb).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, _display) = rig(&mut buf);
                let data: &'static [u8] = StdVec::leak(nested_lui_blob());
                let page = crate::Lui::parse(data).unwrap().page(0).unwrap();
                let mut ui: Ui<Ev, 16> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let _ = &mut layer;
                ui.build_lui_with(&page, |_i, c: &crate::LuiChild<'static>| Some(Ev::Item(c.event as u8))).unwrap();
                assert_eq!(ui.widget_text(ui.find(5).expect("top button")), Some("top"));
                //   the strip's rows, built under the frame, are found by their own tags
                assert_eq!(ui.widget_text(ui.find(0x30).expect("row 0")), Some("R0"));
                assert_eq!(ui.widget_text(ui.find(0x31).expect("row 1")), Some("R1"));
        }

        /// A per-page [`Descent`] override and the tree-wide default both steer the transition,
        /// in that order of precedence, over the layout-derived seed; back mirrors. The rig is
        /// single-buffered mono, so the engine runs its logical over-mode and `page_move_dx`/`dy`
        /// are the logical unit: Up `(0,1)`, Down `(0,-1)`, Left `(1,0)`, Right `(-1,0)`.
        #[test]
        fn descent_overrides_and_the_tree_default_steer_the_transition() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let mut now = 0u64;
                let mut settle = |ui: &mut Ui<Ev, 8>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, now: &mut u64| {
                        let mut frames = 0;
                        while ui.is_animating() {
                                *now += 50_000;
                                ui.render(layer, display, &styled(&font), *now);
                                while layer.poll(display).unwrap() {}
                                frames += 1;
                                assert!(frames < 100, "a transition that never ends");
                        }
                };
                ui.navigate(&PAGE_MAIN).unwrap();
                settle(&mut ui, &mut layer, &mut display, &mut now);

                //   a per-page override beats the layout seed: DETAIL is a stack (seed FromRight),
                // yet each pinned copy flows the way it names, and back reverses it
                for (page, fwd, rev) in [
                        (&PAGE_D_BOTTOM, (0, 1), (0, -1)),
                        (&PAGE_D_TOP, (0, -1), (0, 1)),
                        (&PAGE_D_RIGHT, (1, 0), (-1, 0)),
                        (&PAGE_D_LEFT, (-1, 0), (1, 0)),
                ] {
                        ui.navigate_returning(page, &PAGE_MAIN).unwrap();
                        now += 50_000;
                        ui.render(&mut layer, &mut display, &styled(&font), now);
                        assert_eq!((ui.page_move_dx, ui.page_move_dy), fwd, "forward into an overridden page");
                        settle(&mut ui, &mut layer, &mut display, &mut now);
                        assert!(ui.navigate_back());
                        now += 50_000;
                        ui.render(&mut layer, &mut display, &styled(&font), now);
                        assert_eq!((ui.page_move_dx, ui.page_move_dy), rev, "back out mirrors the arrival");
                        settle(&mut ui, &mut layer, &mut display, &mut now);
                }

                //   the tree default applies to a page with no override (DETAIL, seed FromRight)...
                ui.set_default_descent(Some(Descent::FromTop));
                ui.navigate_returning(&PAGE_DETAIL, &PAGE_MAIN).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, -1), "tree default overrides the seed");
                settle(&mut ui, &mut layer, &mut display, &mut now);
                assert!(ui.navigate_back());
                settle(&mut ui, &mut layer, &mut display, &mut now);

                //   ...but a page's own override still wins over the tree default
                ui.navigate_returning(&PAGE_D_RIGHT, &PAGE_MAIN).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (1, 0), "page override beats the tree default");
                settle(&mut ui, &mut layer, &mut display, &mut now);
                assert!(ui.navigate_back());
                settle(&mut ui, &mut layer, &mut display, &mut now);

                //   clearing the default restores the layout-derived flow: a Row page rises
                ui.set_default_descent(None);
                assert_eq!(ui.default_descent(), None);
                ui.navigate_returning(&PAGE_PINNED, &PAGE_MAIN).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, 1), "cleared default falls back to the seed");
        }

        /// A theme seeds the tree's default descent, but only as a starting value: an
        /// application that set one explicitly keeps it across any theme change.
        #[test]
        fn a_theme_seeds_the_descent_but_an_explicit_choice_wins() {
                //   a theme with a descent seeds a tree that has not chosen
                let mut ui: Ui<Ev, 8> = Ui::new();
                assert_eq!(ui.default_descent(), None);
                let mut t = Theme::DEFAULT;
                t.descent = Some(Descent::FromBottom);
                ui.set_theme(t);
                assert_eq!(ui.default_descent(), Some(Descent::FromBottom));

                //   an explicit choice wins, and survives a later theme that also names one
                ui.set_default_descent(Some(Descent::FromLeft));
                let mut t2 = Theme::DEFAULT;
                t2.descent = Some(Descent::FromTop);
                t2.bg = 0x1234; // distinct, so set_theme does not early-return
                ui.set_theme(t2);
                assert_eq!(ui.default_descent(), Some(Descent::FromLeft), "the app's choice outlives a restyle");

                //   and an explicit choice made FIRST blocks the seed entirely
                let mut ui2: Ui<Ev, 8> = Ui::new();
                ui2.set_default_descent(None);
                let mut t3 = Theme::DEFAULT;
                t3.descent = Some(Descent::FromBottom);
                t3.bg = 0x0001;
                ui2.set_theme(t3);
                assert_eq!(ui2.default_descent(), None, "an explicit None is a choice, not an absence");
        }

        /// A generic `Linear` window lays out along the tree's `Axis`, and the SAME window
        /// re-flows the other way when the axis flips -- the descriptor never changes. The
        /// navigation seed follows the effective axis too: a Linear page in a horizontal tree
        /// enters from the bottom, as a `Row` page would.
        #[test]
        fn a_linear_window_follows_the_tree_axis_and_reflows_when_it_flips() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);

                // the default axis is vertical
                assert_eq!(ui.layout_axis(), Axis::Vertical);
                ui.navigate(&PAGE_LINEAR).unwrap();
                let root = ui.root().unwrap();
                let r: StdVec<Rect> = ui.children(root).map(|c| ui.get(c).unwrap().rect).collect();
                assert_eq!(r.len(), 2);
                // vertical: the second child sits below the first, sharing the left edge
                assert!(r[1].y0 > r[0].y1 && r[0].x0 == r[1].x0, "linear should stack vertically by default");

                // flip the tree axis: the same live window re-lays side by side
                ui.set_layout_axis(Axis::Horizontal);
                let r: StdVec<Rect> = ui.children(root).map(|c| ui.get(c).unwrap().rect).collect();
                assert!(r[1].x0 > r[0].x1 && r[0].y0 == r[1].y0, "flipping the axis should re-flow horizontally");

                //   and a Linear page now seeds its navigation like a Row (enters from the
                // bottom): the mono rig runs the logical over-mode, so that is (0, 1)
                let mut now = 0u64;
                ui.navigate(&PAGE_LINEAR_2).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, 1), "a horizontal-tree Linear page seeds from the bottom");
        }

        /// The `file_list!` component: it generates one tagged, index-emitting row per index
        /// (plus an optional back row), and the fill helpers address a row by tag and show the
        /// placeholder for an empty entry.
        #[test]
        fn file_list_rows_are_tagged_selectable_and_fillable() {
                // the generated slice has one entry per index, plus the back row when given
                assert_eq!(PICK_ROWS.len(), 3);
                assert_eq!(PICK_ROWS_BACK.len(), 3, "two rows and the appended back");

                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PICK_PAGE).unwrap();

                // each row carries tag_base + i and, activated, emits its own index
                for i in 0..3u8 {
                        let id = ui.find(0x30 + i).unwrap_or_else(|| panic!("row {i} present by tag"));
                        assert_eq!(ui.fire(id), Some(Ev::Item(i)), "row {i} emits its index");
                }

                // fill: a name shows as runtime text; an empty entry shows the placeholder label
                ui.fill_list(0x30, &["REC_0001.WAV", "", "keep"], "-");
                let row0 = ui.get(ui.find(0x30).unwrap()).unwrap();
                assert_eq!(row0.text.as_str(), "REC_0001.WAV");
                let row1 = ui.get(ui.find(0x31).unwrap()).unwrap();
                assert!(row1.text.as_str().is_empty(), "an empty entry clears the runtime text");
                if let Kind::Button(b) = &row1.kind {
                        assert_eq!(b.label, "-", "and falls back to the placeholder label");
                } else {
                        panic!("a row is a button");
                }
                // a row past the end is simply left alone (no panic)
                ui.set_list_text(0x30, 9, "ignored", "-");
        }

        /// A `.subtitle()` window reserves a second title row -- its content starts one text
        /// row lower than the same window without one -- and `set_subtitle` fills that row;
        /// on a one-row window it is a no-op.
        #[test]
        fn a_subtitle_reserves_a_second_title_row_and_fills_it() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let cell_h = i32::from(font.cell_height());
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);

                ui.navigate(&PLAIN_PAGE).unwrap();
                let proot = ui.root().unwrap();
                let plain_top = ui.get(ui.children(proot).next().unwrap()).unwrap().rect.y0;

                ui.navigate(&SUB_PAGE).unwrap();
                let sroot = ui.root().unwrap();
                let sub_top = ui.get(ui.children(sroot).next().unwrap()).unwrap().rect.y0;
                assert_eq!(sub_top - plain_top, cell_h, "the subtitle reserves one extra text row of header");

                ui.set_subtitle(sroot, "0:12 / 1:23");
                assert_eq!(ui.get(sroot).unwrap().window().unwrap().subtitle.unwrap().as_str(), "0:12 / 1:23");

                // a one-row window has no subtitle to set
                ui.navigate(&PLAIN_PAGE).unwrap();
                let proot = ui.root().unwrap();
                ui.set_subtitle(proot, "ignored");
                assert!(ui.get(proot).unwrap().window().unwrap().subtitle.is_none());
        }

        /// `set_indicator` carries the shape, lit flag and column for both a `Dot` and a `Play`.
        #[test]
        fn set_indicator_carries_the_shape() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PLAIN_PAGE).unwrap();
                let root = ui.root().unwrap();

                ui.set_indicator(root, Some((IndicatorShape::Play, true, 5)));
                assert_eq!(ui.get(root).unwrap().window().unwrap().indicator, Some((IndicatorShape::Play, true, 5)));
                ui.set_indicator(root, Some((IndicatorShape::Dot, false, 3)));
                assert_eq!(ui.get(root).unwrap().window().unwrap().indicator, Some((IndicatorShape::Dot, false, 3)));
                ui.set_indicator(root, None);
                assert_eq!(ui.get(root).unwrap().window().unwrap().indicator, None);
        }

        /// A tap on a button lights the press flash at once (so a slow handler still reads as
        /// acknowledged), and the flash lapses on its own once ACTIVATE_FLASH_US has passed.
        #[test]
        fn a_tap_flashes_the_button_then_lapses() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PLAIN_PAGE).unwrap();
                let btn = ui.children(ui.root().unwrap()).next().unwrap();
                let r = ui.get(btn).unwrap().rect;
                let (cx, cy) = (((r.x0 + r.x1) / 2) as u16, ((r.y0 + r.y1) / 2) as u16);

                // a tap: down, then up after a valid hold, on the same spot
                ui.touch(cx, cy, true, 0);
                let up = TAP_MIN_HOLD_US + 1;
                assert_eq!(ui.touch(cx, cy, false, up), Touch::Tap { hit: true, emitted: Some(Ev::Alpha) });
                assert!(ui.is_animating(), "the press flash keeps the loop rendering");

                // a render before the deadline keeps it; one after clears it
                ui.render(&mut layer, &mut display, &styled(&font), up + ACTIVATE_FLASH_US / 2);
                assert!(ui.is_animating(), "still flashing mid-window");
                ui.render(&mut layer, &mut display, &styled(&font), up + ACTIVATE_FLASH_US + 1);
                assert!(!ui.is_animating(), "the flash lapses on its own");
        }

        /// The COVER transition (forward into a Row page), at the rotation the landscape apps
        /// run: the incoming page is rendered into the back buffer and slid up over the static
        /// outgoing, which stays in the front. Mid-step, the covered region must be the incoming
        /// image displaced along the viewer's vertical, and the uncovered region the outgoing.
        #[test]
        fn a_row_page_covers_by_sliding_the_incoming_up_over_the_static_outgoing() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut front = std::vec![0u8; 64 * 48 * 2];
                let mut back = std::vec![0u8; 64 * 48 * 2];
                let mut display = Display::new(Mock { pushed: StdVec::new() }, &mut front, 64, 48, PixelFormat::Rgb565, now);
                display.set_back_buffer(&mut back);
                let mut layer = FrameLayer::new(64, 48, PixelFormat::Rgb565);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                let mut t = 0u64;
                let mut settle = |ui: &mut Ui<Ev, 8>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, t: &mut u64| loop {
                        ui.render(layer, display, &styled(&font), *t);
                        while layer.poll(display).unwrap() {}
                        *t += 50_000;
                        if !ui.is_animating() && !ui.is_dirty() {
                                break;
                        }
                };
                settle(&mut ui, &mut layer, &mut display, &mut t);
                ui.set_rotation(&mut layer, Rotation::R270);
                settle(&mut ui, &mut layer, &mut display, &mut t);
                //   keep a copy of the outgoing (PAGE_MAIN) as it stands in the front before the
                // transition -- the cover must preserve it in the uncovered region. Freeze to
                // reach the front, copy it, thaw; the transition below freezes for itself.
                let outgoing: StdVec<u8> = {
                        assert!(display.freeze());
                        let (f, _) = display.frame_and_capture().unwrap();
                        let v = f.to_vec();
                        display.thaw();
                        v
                };
                ui.navigate(&PAGE_PINNED).unwrap();
                // the first step freezes (render-mode) and renders the incoming into the back
                let t0 = t;
                assert!(ui.render(&mut layer, &mut display, &styled(&font), t0));
                assert!(display.is_frozen());
                while layer.poll(&mut display).unwrap() {}
                let m = layer.transform();
                let (ux, uy) = (m.b.signum(), m.d.signum());
                assert_ne!((ux, uy), (0, 0));
                let span = if ux != 0 { 64 } else { 48 };
                // at the midpoint the blit offset is span/2 (span - travel, travel = span/2)
                assert!(ui.render(&mut layer, &mut display, &styled(&font), t0 + u64::from(ui.page_move_ms) * 500));
                while layer.poll(&mut display).unwrap() {}
                let off = span / 2;
                let (off_x, off_y) = (ux * off, uy * off);
                let (f, incoming) = display.frame_and_capture().expect("frozen mid-transition");
                let mut covered = 0;
                for dy in 0..48i32 {
                        for dx in 0..64i32 {
                                let (sx, sy) = (dx - off_x, dy - off_y);
                                let d = ((dy * 64 + dx) * 2) as usize;
                                if !(0..64).contains(&sx) || !(0..48).contains(&sy) {
                                        //   uncovered: the front still holds the outgoing, untouched
                                        assert_eq!(&f[d..d + 2], &outgoing[d..d + 2], "uncovered ({dx},{dy}) is not the static outgoing");
                                        continue;
                                }
                                //   covered: the incoming (in the back buffer) displaced up
                                let s = ((sy * 64 + sx) * 2) as usize;
                                assert_eq!(&f[d..d + 2], &incoming[s..s + 2], "covered ({dx},{dy}) is not the incoming image displaced by ({off_x},{off_y})");
                                covered += 1;
                        }
                }
                assert_eq!(covered, 64 * 48 / 2);
                // run out: thawed, and the settled tree is the Row page
                settle(&mut ui, &mut layer, &mut display, &mut t);
                assert!(!display.is_frozen());
        }

        #[test]
        fn render_pushes_only_the_changed_widgets_after_the_first_full_frame() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                assert_eq!(flush(&mut layer, &mut display), [Region::full(64, 48)]);
                assert!(!ui.render(&mut layer, &mut display, &styled(&font), 0), "nothing dirty");
                // moving focus dirties exactly the two buttons
                let root = ui.root().unwrap();
                let rows: StdVec<Rect> = ui.children(root).map(|c| ui.get(c).unwrap().rect).collect();
                ui.focus_next();
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                let mut pushed = flush(&mut layer, &mut display);
                pushed.sort_by_key(|r| r.y0);
                // the previous frame's full-canvas invalidation carries forward once
                assert_eq!(pushed, [Region::full(64, 48)]);
                ui.focus_next();
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                let mut pushed = flush(&mut layer, &mut display);
                pushed.sort_by_key(|r| r.y0);
                // rows 0 and 1 from last frame's invalidation, rows 1 and 2 from this one: three
                // disjoint rows (the gap keeps them apart), row 1 merged with itself, and nothing
                // of the header or the frame
                assert_eq!(pushed.len(), 3);
                assert_eq!(pushed[0].y0, rows[0].y0 as u16);
                assert_eq!(pushed[2].y1, rows[2].y1 as u16);
        }

        #[test]
        fn rotation_relayouts_against_the_new_aspect_and_untransforms_taps() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                ui.set_rotation(&mut layer, Rotation::R90);
                // a mono, single-buffered panel cannot animate: the rotation snaps at the next
                // render, which is where it is applied
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                assert!(!ui.is_animating());
                assert_eq!(ui.logical_size(), (48, 64));
                let root = ui.root().unwrap();
                assert_eq!(ui.get(root).unwrap().rect, Rect::new(0, 0, 47, 63));
                // a panel point maps through the same transform the canvas draws with: logical
                // (10, 20) under R90 on a 64x48 panel is physical (63 - 20, 10)
                let (hit, _) = ui.press_at(43, 10);
                let _ = hit;
                let (lx, ly) = ui.untransform(43, 10);
                assert_eq!((lx, ly), (10, 20));
                // and a swipe along the panel's x reads as vertical to the user
                assert_eq!(ui.swipe_direction((10, 20), (50, 22)), Some(SwipeDir::Up));
        }

        static T_A: Desc<Ev> = Desc::button("Alpha").emit(Ev::Alpha).tag(1);
        static T_B: Desc<Ev> = Desc::button("Beta").emit(Ev::Beta).tag(2);
        static T_C: Desc<Ev> = Desc::button("Gamma").emit(Ev::Alpha).tag(3);
        static T_MORE: Desc<Ev> = Desc::button("More >").navigate(&T_PAGE_DETAIL);
        static T_LIST: Desc<Ev> = Desc::button("List >").navigate(&T_PAGE_LIST);
        static T_MAIN: Desc<Ev> = Desc::window("mk4 demo").rounded(24).stack(2).children(&[&T_A, &T_B, &T_C, &T_MORE, &T_LIST]);
        static T_LBL: Desc<Ev> = Desc::label("swipe right to go back");
        static T_BACK: Desc<Ev> = Desc::button("< Back").back();
        static T_DETAIL: Desc<Ev> = Desc::window("More").rounded(24).stack(2).children(&[&T_LBL, &T_A, &T_B, &T_BACK]);
        static T_I1: Desc<Ev> = Desc::button("Item 1").emit(Ev::Item(1)).min_size(0, 44);
        static T_I2: Desc<Ev> = Desc::button("Item 2").emit(Ev::Item(2)).min_size(0, 44);
        static T_I3: Desc<Ev> = Desc::button("Item 3").emit(Ev::Item(3)).min_size(0, 44);
        static T_I4: Desc<Ev> = Desc::button("Item 4").emit(Ev::Item(4)).min_size(0, 44);
        static T_I5: Desc<Ev> = Desc::button("Item 5").emit(Ev::Item(5)).min_size(0, 44);
        static T_I6: Desc<Ev> = Desc::button("Item 6").emit(Ev::Item(6)).min_size(0, 44);
        static T_I7: Desc<Ev> = Desc::button("Item 7").emit(Ev::Item(7)).min_size(0, 44);
        static T_LBACK: Desc<Ev> = Desc::button("< Back").back().min_size(0, 44);
        static T_LISTW: Desc<Ev> = Desc::window("List").rounded(24).stack(2).scroll(scroll::VERTICAL).children(&[&T_I1, &T_I2, &T_I3, &T_I4, &T_I5, &T_I6, &T_I7, &T_LBACK]);
        static T_PAGE_MAIN: Page<Ev> = Page::new(&T_MAIN, None);
        static T_PAGE_DETAIL: Page<Ev> = Page::new(&T_DETAIL, Some(&T_PAGE_MAIN));
        static T_PAGE_LIST: Page<Ev> = Page::new(&T_LISTW, Some(&T_PAGE_MAIN));

        /// The touch169 demo, end to end on the host: every page built, painted at every
        /// rotation, scrolled and navigated, with the real panel geometry and a 12x19 cell.
        #[test]
        fn the_touch169_demo_builds_paints_and_navigates_at_every_rotation() {
                let mut e = Encoder::new(12, 19, 15, 16);
                for c in 0x20u8..0x7f {
                        e.add(c, &[0xFF; 38]).unwrap();
                }
                let blob = e.encode();
                let font = Font::parse(&blob).unwrap();
                let mut front = std::vec![0u8; 240 * 280 * 2];
                let mut back = std::vec![0u8; 240 * 280 * 2];
                let mut display = Display::new(Mock { pushed: StdVec::new() }, &mut front, 240, 280, PixelFormat::Rgb565, now);
                display.set_back_buffer(&mut back);
                let mut layer = FrameLayer::new(240, 280, PixelFormat::Rgb565);
                let mut ui: Ui<Ev, 12> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&T_PAGE_MAIN).unwrap();
                // time advances 50 ms per pass, so the animations run their course
                let mut t: u64 = 0;
                let settle = |ui: &mut Ui<Ev, 12>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, t: &mut u64| {
                        let mut frames = 0;
                        loop {
                                if ui.render(layer, display, &styled(&font), *t) {
                                        frames += 1;
                                }
                                let _ = flush(layer, display);
                                *t += 50_000;
                                if !ui.is_animating() && !ui.is_dirty() {
                                        break;
                                }
                                assert!(frames < 100, "an animation that never ends");
                        }
                        frames
                };
                assert!(settle(&mut ui, &mut layer, &mut display, &mut t) >= 1);
                for rot in [Rotation::R90, Rotation::R180, Rotation::R270, Rotation::R0] {
                        ui.set_rotation(&mut layer, rot);
                        // a turn of 280 ms at 50 ms a pass: several animation frames, then the
                        // settled tree at the new orientation
                        assert!(settle(&mut ui, &mut layer, &mut display, &mut t) >= 5);
                        assert_eq!(layer.rotation(), rot);
                        for _ in 0..6 {
                                ui.focus_next();
                                assert_eq!(settle(&mut ui, &mut layer, &mut display, &mut t), 1);
                        }
                }
                assert_eq!(ui.logical_size(), (240, 280));
                // into the list -- a page transition -- then drag it to the end, tap the back row
                let root = ui.root().unwrap();
                let list_btn = ui.children(root).last().unwrap();
                ui.set_focus(Some(list_btn));
                assert_eq!(ui.activate(), None);
                assert!(core::ptr::eq(ui.page().unwrap(), &T_PAGE_LIST));
                assert!(ui.is_animating());
                assert!(settle(&mut ui, &mut layer, &mut display, &mut t) >= 3);
                assert_eq!(ui.touch(120, 200, true, t), Touch::Pending);
                let mut dragged = false;
                for y in (20..200).rev().step_by(10) {
                        // pending until the finger has travelled the slop, a drag from then on
                        match ui.touch(120, y, true, t) {
                                Touch::Pending => assert!(!dragged && 200 - y <= DRAG_SLOP as u16),
                                Touch::Drag => dragged = true,
                                other => panic!("unexpected {other:?}"),
                        }
                        settle(&mut ui, &mut layer, &mut display, &mut t);
                }
                assert!(dragged);
                assert_eq!(ui.touch(120, 20, false, t), Touch::DragEnd);
                let root = ui.root().unwrap();
                let _ = ui.scroll_by(root, 0, 1000);
                let back = ui.children(root).last().unwrap();
                let r = ui.get(back).unwrap().rect;
                let (cx, cy) = (((r.x0 + r.x1) / 2) as u16, ((r.y0 + r.y1) / 2) as u16);
                assert_eq!(ui.touch(cx, cy, true, t), Touch::Pending);
                assert_eq!(ui.touch(cx, cy, false, t + 100_000), Touch::Tap { hit: true, emitted: None });
                assert!(core::ptr::eq(ui.page().unwrap(), &T_PAGE_MAIN));
                // a tap during the return transition still lands: only rotation blocks input
                assert!(ui.is_animating());
                assert!(settle(&mut ui, &mut layer, &mut display, &mut t) >= 3);
                assert!(!display.is_frozen());
        }

        #[test]
        fn a_rotation_animates_from_a_captured_frame_and_ignores_taps_until_it_settles() {
                let mut e = Encoder::new(12, 19, 15, 16);
                for c in 0x20u8..0x7f {
                        e.add(c, &[0xFF; 38]).unwrap();
                }
                let blob = e.encode();
                let font = Font::parse(&blob).unwrap();
                let mut front = std::vec![0u8; 240 * 280 * 2];
                let mut back = std::vec![0u8; 240 * 280 * 2];
                let mut display = Display::new(Mock { pushed: StdVec::new() }, &mut front, 240, 280, PixelFormat::Rgb565, now);
                display.set_back_buffer(&mut back);
                let mut layer = FrameLayer::new(240, 280, PixelFormat::Rgb565);
                let mut ui: Ui<Ev, 12> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&T_PAGE_MAIN).unwrap();
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                let _ = flush(&mut layer, &mut display);
                ui.set_rotation(&mut layer, Rotation::R90);
                // the first step captures the frame and freezes swapping; the layer is still
                // at R0 and the tree still laid out for it
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 1_000), "an animation frame was drawn");
                assert!(display.is_frozen());
                assert_eq!(layer.rotation(), Rotation::R0);
                assert_eq!(flush(&mut layer, &mut display), [Region::full(240, 280)], "an animation frame is a whole-panel push");
                // taps are refused mid-turn
                assert_eq!(ui.press_at(120, 100), (false, None));
                assert_eq!(ui.touch(120, 100, true, 0), Touch::None);
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 150_000));
                let _ = flush(&mut layer, &mut display);
                // past the duration: thawed, committed, the settled tree drawn in one call
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 300_000));
                assert!(!display.is_frozen());
                assert!(!ui.is_animating());
                assert_eq!(layer.rotation(), Rotation::R90);
                assert_eq!(ui.logical_size(), (280, 240));
                assert_eq!(flush(&mut layer, &mut display), [Region::full(240, 280)]);
        }

        #[test]
        fn a_page_too_big_for_the_arena_is_refused_and_leaves_nothing_behind() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 3> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                assert_eq!(ui.navigate(&PAGE_MAIN), Err(Error::Full));
                assert!(ui.root().is_none());
                assert!(ui.widgets.iter().all(|s| s.is_none()));
        }
}
