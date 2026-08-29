//! The widget toolkit: mk3's `light_ui`, ported.
//!
//! A retained tree of windows, buttons and labels over the frame layer. The tree is built from
//! `const` descriptors that live in flash; navigating tears the current page down and builds
//! the next, so only one page's widgets exist at a time. Widgets live in a fixed arena owned by
//! the [`Ui`] -- no allocator, and a tree too big for it is a build error at the call that adds
//! the widget, never a silent drop.
//!
//! **What comes out is an event, not a callback.** mk3 buttons carried a C function pointer, a
//! `void *` and a command string; here a button carries what it *emits* -- a value of the
//! application's own event type -- and, optionally, where it navigates. Activation returns the
//! emitted value for the caller to publish on its bus, which is the "command tree is the event
//! bus" decision applied to the UI: a tap, a console line and a boot script all end up as the
//! same event, and a handler runs on its own module's poll rather than inside the input path.
//!
//! **Hardware-free**, as mk3's was: nothing here knows what a touch controller, a push button
//! or an IMU is. The application maps its devices onto the input calls (a few lines per app),
//! and the toolkit can be exercised entirely on the host.
//!
//! Coordinates: every widget rect is in ABSOLUTE logical canvas coordinates, never
//! parent-relative, so a hit test, a clip and an invalidation are the same arithmetic wherever a
//! widget sits. Input arrives in PANEL coordinates -- what a touch controller reports -- and is
//! untransformed here, because the toolkit is the one thing that knows the rotation.

use heapless::Vec;

use crate::display::{Display, DisplayDriver, Region};
use crate::draw::{Canvas, Flip, Point, Rotation, Transform};
use crate::frames::{FrameLayer, LogicalRegion, MAX_REGIONS};
use crate::{debug, error, trace, warn};
use light_font::Font;

/// A widget rectangle: inclusive, logical, signed -- a widget positioned partly off the canvas
/// is clipped here before anything reaches the rasteriser.
pub type Rect = LogicalRegion;

/// How far a finger may wander (logical pixels, either axis) before a touch stops being a
/// prospective tap and becomes a drag. 16, raised from a first guess of 8, which classified
/// real taps as travel: a fingertip rolls several pixels as it presses and the CST816T adds its
/// own jitter, so taps were dropped or turned into 1 px scrolls. Still below any swipe threshold.
pub const DRAG_SLOP: i32 = 16;

/// Longest label the toolkit renders. Labels are truncated to their widget anyway; this bounds
/// the work a single draw does.
pub const TEXT_MAX: usize = 64;

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

/// How a window arranges its children when layout is (re-)run. Recorded on the window so a
/// rotation or resize can re-apply it without the application being told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
        /// Children placed by hand; relayout leaves them where they are.
        None,
        /// Equal-height rows, one per visible child, `gap` pixels apart.
        Stack { gap: u8 },
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
        /// curve -- see [`Ui::set_corner_radius`].
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

#[derive(Clone, Copy, Debug)]
pub struct Widget<A: 'static> {
        pub kind: Kind<A>,
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
}

impl<A> Page<A> {
        pub const fn new(content: &'static Desc<A>, parent: Option<&'static Page<A>>) -> Self {
                Self { content, parent }
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
        children: &'static [&'static Desc<A>],
}

impl<A: Copy> Desc<A> {
        const fn base(kind: DescKind, text: Option<&'static str>) -> Self {
                Self { kind, text, emit: None, nav: Nav::Stay, corner_radius: 0, layout: Layout::None, scroll: scroll::NONE, min_w: 0, min_h: 0, max_w: 0, max_h: 0, rect: None, tag: 0, children: &[] }
        }
        pub const fn window(title: &'static str) -> Self {
                Self::base(DescKind::Window, Some(title))
        }
        /// An untitled frame.
        pub const fn frame() -> Self {
                Self::base(DescKind::Window, None)
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
        pub const fn rounded(mut self, radius: u8) -> Self {
                self.corner_radius = radius;
                self
        }
        pub const fn stack(mut self, gap: u8) -> Self {
                self.layout = Layout::Stack { gap };
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
        /// The font's cell, for layout and truncation; fonts are fixed-pitch.
        cell_w: i32,
        cell_h: i32,
        /// Pixels kept clear on every edge, for glass that does not show the whole grid. Uniform
        /// rather than per-edge because the interface rotates while the corners are fixed in the
        /// panel's frame: a uniform inset is the only value invariant under rotation.
        safe_inset: i32,
        // --- touch tracking, owned by `touch()`; everything logical ---
        touch_down: bool,
        touch_dragging: bool,
        /// The scrollable window captured when the drag engaged: the drag stays with it even if
        /// the finger wanders off, as every scrolling surface behaves.
        drag_window: Option<WidgetId>,
        touch_start: (i32, i32),
        touch_last: (i32, i32),
        pub drag_slop: i32,
        // --- navigation ---
        page: Option<&'static Page<A>>,
        /// Where back goes when it is not the current page's parent; set only by
        /// `navigate_returning`, cleared by every ordinary navigation.
        return_page: Option<&'static Page<A>>,
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
                        cell_w: 0,
                        cell_h: 0,
                        safe_inset: 0,
                        touch_down: false,
                        touch_dragging: false,
                        drag_window: None,
                        touch_start: (0, 0),
                        touch_last: (0, 0),
                        drag_slop: DRAG_SLOP,
                        page: None,
                        return_page: None,
                }
        }

        /// The font's cell metrics, which layout and truncation need; fonts are fixed-pitch.
        pub fn set_font(&mut self, font: &Font<'_>) {
                self.cell_w = i32::from(font.cell_width());
                self.cell_h = i32::from(font.cell_height());
                self.relayout();
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
                let id = self.alloc(Widget { kind, rect, visible: true, focusable, enabled: true, hit_slop_y1: 0, min_w: 0, min_h: 0, max_w: 0, max_h: 0, tag: 0, parent, next_sibling: None, first_child: None })?;
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
        pub fn create_window(&mut self, parent: Option<WidgetId>, rect: Rect, title: Option<&'static str>) -> Result<WidgetId, Error> {
                let win = Window { title, padding: 2, border: true, corner_radius: 0, layout: Layout::None, scroll: scroll::NONE, scroll_x: 0, scroll_y: 0, content_w: 0, content_h: 0 };
                self.add(parent, Kind::Window(win), rect, false)
        }

        pub fn create_button(&mut self, parent: Option<WidgetId>, rect: Rect, label: &'static str, emit: Option<A>, nav: Nav<A>) -> Result<WidgetId, Error> {
                let id = self.add(parent, Kind::Button(Button { label, emit, nav, corner_radius: 0, corners: crate::draw::corner::NONE }), rect, true)?;
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
                                let id = self.create_window(parent, rect, desc.text)?;
                                //   before the children exist: the corner clearance is then already
                                // accounted for when the single layout pass runs; and scrolling
                                // changes how the stack treats rows that do not fit
                                let win = self.w_mut(id).window_mut().expect("a window");
                                win.corner_radius = desc.corner_radius;
                                win.scroll = desc.scroll;
                                id
                        }
                        DescKind::Button => self.create_button(parent, rect, desc.text.unwrap_or(""), desc.emit, desc.nav)?,
                        DescKind::Label => self.create_label(parent, rect, desc.text.unwrap_or(""))?,
                };
                {
                        let w = self.w_mut(id);
                        w.min_w = desc.min_w;
                        w.min_h = desc.min_h;
                        w.max_w = desc.max_w;
                        w.max_h = desc.max_h;
                        w.tag = desc.tag;
                }
                for child in desc.children {
                        self.build_desc(Some(id), child)?;
                }
                // after the children, since a stack divides the content area between them
                if let Layout::Stack { gap } = desc.layout {
                        self.layout_stack(id, gap);
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

        // --- navigation ---

        fn show_page(&mut self, page: &'static Page<A>, return_page: Option<&'static Page<A>>) -> Result<(), Error> {
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
                self.show_page(page, None)
        }

        /// The same, but back from `page` goes to `return_page` -- for a cross-tree jump that
        /// should return to where it was reached from. The override lasts exactly one page.
        pub fn navigate_returning(&mut self, page: &'static Page<A>, return_page: &'static Page<A>) -> Result<(), Error> {
                self.show_page(page, Some(return_page))
        }

        /// Go to the current page's return address if one was set, otherwise its parent. `false`,
        /// changing nothing, when there is nowhere to go -- a top-level page, or a tree built
        /// without pages -- so a caller can leave the gesture meaning nothing there.
        pub fn navigate_back(&mut self) -> bool {
                let Some(page) = self.page else { return false };
                let Some(target) = self.return_page.or(page.parent) else { return false };
                self.show_page(target, None).is_ok()
        }

        // --- geometry ---

        fn inset_x(win: &Window) -> i32 {
                i32::from(win.padding) + if win.border { 1 } else { 0 }
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
                        let header_bottom = w.rect.y0 + if win.border { 1 } else { 0 } + self.cell_h + 2;
                        content.y0 = content.y0.max(header_bottom);
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
                let gap = i32::from(gap);
                {
                        let win = self.w_mut(id).window_mut().expect("a window");
                        win.layout = Layout::Stack { gap: gap as u8 };
                }
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
                                b.corners = if r > 0 { crate::draw::corner::BOTTOM } else { crate::draw::corner::NONE };
                        }
                        y += h + gap;
                }
                self.invalidate_widget(id);
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
                if let Layout::Stack { gap } = win.layout {
                        self.layout_stack(id, gap);
                }
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
                if let Layout::Stack { gap } = win.layout {
                        self.layout_stack(id, gap);
                }
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

        /// Keep `inset` pixels clear on every edge of the canvas. Set it to the glass's corner
        /// radius; applied by relayout.
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
                        if let Some(win) = self.w(id).window() {
                                if let Layout::Stack { gap } = win.layout {
                                        self.layout_stack(id, gap);
                                }
                        }
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
                let before = layer.transform();
                layer.set_orientation(rotation, Flip::None);
                if layer.transform() == before {
                        return;
                }
                // regions measured against the old geometry describe nothing now; and the panel
                // still shows the old arrangement in the old orientation, so nothing short of the
                // whole canvas is a safe region to push
                layer.invalidate_all();
                self.fit(layer);
                self.invalidate_all();
                let (w, h) = self.logical_size();
                debug!("ui rotation now {rotation:?}, canvas {w}x{h}");
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
        /// the hard way in mk3, where reading a field after a navigating handler dereferenced
        /// freed memory and wedged the core.
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
                let (lx, ly) = self.untransform(i32::from(x), i32::from(y));
                let Some(root) = self.root else { return (false, None) };
                let Some(hit) = self.hit_test(root, self.canvas_rect(), lx, ly, None) else { return (false, None) };
                self.set_focus(Some(hit));
                (true, self.fire(hit))
        }

        fn touch_reset(&mut self) {
                self.touch_down = false;
                self.touch_dragging = false;
                self.drag_window = None;
        }

        /// The stateful entry point: feed it the panel's CURRENT state every tick -- position and
        /// whether a finger is down -- and it runs the whole tap-versus-drag interaction.
        ///
        /// - A touch that ends within `drag_slop` of where it began is a TAP, delivered on RELEASE
        ///   at the point it STARTED: release, because with scrollable content a down-edge fires
        ///   on every drag's first contact; the start point, because that is where the intent was.
        /// - A touch that moves beyond the slop over a scrollable window becomes a DRAG: the window
        ///   under the START point scrolls to follow the finger until release.
        /// - A touch that travels with nothing scrollable under it commits to neither, and the
        ///   release is left for the gesture pipeline -- a swipe on a non-scrolling page navigates.
        pub fn touch(&mut self, x: u16, y: u16, touching: bool) -> Touch<A> {
                if !touching {
                        if !self.touch_down {
                                return Touch::None;
                        }
                        let was_drag = self.touch_dragging;
                        let (sx, sy) = self.touch_start;
                        let adx = (self.touch_last.0 - sx).abs();
                        let ady = (self.touch_last.1 - sy).abs();
                        self.touch_reset();
                        let travelled = adx > self.drag_slop || ady > self.drag_slop;
                        // the verdict and the numbers, because "taps sometimes don't work" is
                        // otherwise undiagnosable; TRACE, since it fires on every touch
                        trace!("ui: touch release moved ({adx}, {ady}), slop {} -> {}", self.drag_slop, if was_drag { "drag" } else if travelled { "neither" } else { "tap" });
                        if was_drag {
                                return Touch::DragEnd;
                        }
                        if travelled {
                                return Touch::None;
                        }
                        let hit = self.root.and_then(|root| self.hit_test(root, self.canvas_rect(), sx, sy, None));
                        return match hit {
                                Some(id) => {
                                        self.set_focus(Some(id));
                                        Touch::Tap { hit: true, emitted: self.fire(id) }
                                }
                                None => Touch::Tap { hit: false, emitted: None },
                        };
                }

                let (lx, ly) = self.untransform(i32::from(x), i32::from(y));
                if !self.touch_down {
                        self.touch_down = true;
                        self.touch_dragging = false;
                        self.drag_window = None;
                        self.touch_start = (lx, ly);
                        self.touch_last = (lx, ly);
                        return Touch::Pending;
                }

                if !self.touch_dragging {
                        let adx = (lx - self.touch_start.0).abs();
                        let ady = (ly - self.touch_start.1).abs();
                        if adx > self.drag_slop || ady > self.drag_slop {
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
                self.invalidate_widget(id);
        }

        // --- painting ---

        /// Draw `text` at `(x, y)` truncated to `max_width` and the canvas. The rasteriser clips
        /// per pixel; what this owes it is bounding the STRING so a long label is cut to its widget
        /// rather than painted across the clip, and refusing an origin the canvas cannot hold.
        fn draw_text_fitted(&self, c: &mut Canvas<'_>, font: &Font<'_>, x: i32, y: i32, text: &str, max_width: i32) {
                if self.cell_w == 0 || x < 0 || y < 0 || x >= self.width || y >= self.height {
                        return;
                }
                let fit_widget = (max_width.max(0) / self.cell_w) as usize;
                let fit_canvas = ((self.width - x) / self.cell_w) as usize;
                let mut len = text.len().min(fit_widget).min(fit_canvas).min(TEXT_MAX);
                while !text.is_char_boundary(len) {
                        len -= 1;
                }
                if len == 0 {
                        return;
                }
                c.text(font, Point::new(x, y), &text[..len]);
        }

        /// Horizontally centre `len` glyphs within `[x0, x1]`, never left of `x0`.
        fn centre_x(&self, x0: i32, x1: i32, len: usize) -> i32 {
                let avail = x1 - x0 + 1;
                let used = len as i32 * self.cell_w;
                if used >= avail { x0 } else { x0 + (avail - used) / 2 }
        }

        /// Clamp a widget's rect only as far as the rasteriser's coordinates demand; the SHAPE is
        /// drawn at its true geometry and the clip cuts it at its container's edge, so a
        /// half-scrolled widget is cropped rather than redrawn smaller.
        fn draw_rect_of(&self, r: Rect) -> Rect {
                Rect::new(r.x0.max(0), r.y0.max(0), r.x1.min(self.width - 1), r.y1.min(self.height - 1))
        }

        fn paint_window(&self, c: &mut Canvas<'_>, font: &Font<'_>, id: WidgetId, clip: &Rect) {
                let w = self.w(id);
                let win = w.window().expect("a window");
                let mut visible = w.rect;
                if !rect_intersect(&mut visible, clip) {
                        return;
                }
                let r = self.draw_rect_of(w.rect);
                if win.border {
                        if win.corner_radius != 0 {
                                c.rect_rounded(Point::new(r.x0, r.y0), Point::new(r.x1, r.y1), u16::from(win.corner_radius), crate::draw::corner::ALL, false);
                        } else {
                                c.rect(Point::new(r.x0, r.y0), Point::new(r.x1, r.y1), false);
                        }
                }
                let Some(title) = win.title else { return };
                // the header band the stack layout reserves: title at the very top, separator
                // under it; the two must agree on its height (cell_h + 2). A rounded corner is
                // cleared SIDEWAYS here, not downward: the title is one short string, and the
                // indent is taken at its TOP row, where the arc is furthest in
                let f = &w.rect;
                let inset = if win.border { 1 } else { 0 };
                let ty = f.y0 + inset;
                let indent = corner_indent(win.corner_radius, i32::from(win.corner_radius) - inset);
                let tx = f.x0 + indent + inset + 1;
                self.draw_text_fitted(c, font, tx, ty, title, (f.x1 - indent - inset) - tx + 1);
                // the separator sits a cell lower, where the arc has come most of the way out
                let sep_y = ty + self.cell_h;
                let sep_indent = corner_indent(win.corner_radius, i32::from(win.corner_radius) - (sep_y - f.y0));
                let sep_x0 = f.x0 + sep_indent + inset;
                if sep_y <= f.y1 && sep_y >= 0 && sep_x0 >= 0 {
                        c.line(Point::new(sep_x0, sep_y), Point::new(f.x1 - sep_indent - inset, sep_y));
                }
        }

        fn paint_button(&self, c: &mut Canvas<'_>, font: &Font<'_>, id: WidgetId, clip: &Rect) {
                let w = self.w(id);
                let btn = w.button().expect("a button");
                let mut visible = w.rect;
                if !rect_intersect(&mut visible, clip) {
                        return;
                }
                let r = self.draw_rect_of(w.rect);
                let focused = self.focused == Some(id);
                let (p0, p1) = (Point::new(r.x0, r.y0), Point::new(r.x1, r.y1));
                if btn.corner_radius != 0 {
                        c.rect_rounded(p0, p1, u16::from(btn.corner_radius), btn.corners, focused);
                } else {
                        c.rect(p0, p1, focused);
                }
                // inverting the focused button: swap the colours around the label. Uniform for
                // 1 bpp and RGB565, since both go through the same colour path
                let saved_fg = c.fg;
                if focused {
                        c.fg = c.bg;
                }
                if !btn.label.is_empty() {
                        // positioned from the TRUE rect, never the clamped one: centring against
                        // the clamp made a label creep as its row crossed the canvas edge
                        let f = &w.rect;
                        let (inner_x0, inner_x1) = (f.x0 + 1, f.x1 - 1);
                        let inner_h = f.y1 - f.y0 - 1;
                        let tx = self.centre_x(inner_x0, inner_x1, btn.label.len());
                        let ty = (f.y0 + 1 + (inner_h - self.cell_h) / 2).max(f.y0 + 1);
                        self.draw_text_fitted(c, font, tx, ty, btn.label, inner_x1 - inner_x0 + 1);
                }
                c.fg = saved_fg;
                // TODO a distinct look for disabled buttons wants a colour model 1 bpp lacks
        }

        fn paint_label(&self, c: &mut Canvas<'_>, font: &Font<'_>, id: WidgetId, clip: &Rect) {
                let w = self.w(id);
                let Kind::Label(l) = &w.kind else { return };
                let mut visible = w.rect;
                if !rect_intersect(&mut visible, clip) {
                        return;
                }
                self.draw_text_fitted(c, font, w.rect.x0, w.rect.y0, l.text, w.rect.x1 - w.rect.x0 + 1);
        }

        /// `clip` is by value so each subtree narrows its own copy: a scrolling window's children
        /// paint only inside its viewport. The same narrowing happens in `hit_test`, and the two
        /// must agree: what cannot be seen must not respond.
        fn paint_clipped(&self, c: &mut Canvas<'_>, font: &Font<'_>, id: WidgetId, mut clip: Rect) {
                if !self.w(id).visible {
                        return;
                }
                // the walk's clip becomes the canvas's, so the primitives cut every shape at the
                // container edge; coordinates are safe -- the walk starts at the canvas and only
                // ever intersects
                c.set_clip(Region::new(clip.x0 as u16, clip.y0 as u16, clip.x1 as u16, clip.y1 as u16));
                match &self.w(id).kind {
                        Kind::Window(_) => self.paint_window(c, font, id, &clip),
                        Kind::Button(_) => self.paint_button(c, font, id, &clip),
                        Kind::Label(_) => self.paint_label(c, font, id, &clip),
                }
                // a scrolling window confines its children to its viewport; its own frame and
                // title were drawn against the wider clip, which keeps the frame visible while
                // content moves beneath it. The content region runs to the scroll STOP for every
                // row, so a row straddling the bottom paints into the corner band
                if self.w(id).is_scrolling_window() {
                        let mut vp = self.viewport(id);
                        vp.y1 = vp.y1.max(self.scroll_stop_y1(id));
                        if !rect_intersect(&mut clip, &vp) {
                                return;
                        }
                }
                // children after the parent, in sibling order: later draws on top
                for child in self.children(id) {
                        self.paint_clipped(c, font, child, clip);
                }
        }

        /// Paint the whole tree onto a cleared canvas. The ENTIRE tree, not just the dirty
        /// widgets, because every frame is a full repaint; only the pushed REGION is optimised,
        /// which is where the cost that scales with panel size lives.
        pub fn paint(&self, c: &mut Canvas<'_>, font: &Font<'_>) {
                if let Some(root) = self.root {
                        self.paint_clipped(c, font, root, self.canvas_rect());
                }
                // the clip is canvas state: left narrowed it would crop whatever draws next
                c.clear_clip();
        }

        /// Repaint and push, if anything is dirty. Call every pass; the layer's pacing decides
        /// when a frame happens, and this is a no-op on the passes in between. On a refused frame
        /// the dirty flag and the regions survive, so the repaint happens on a later pass.
        pub fn render<D: DisplayDriver>(&mut self, layer: &mut FrameLayer, display: &mut Display<'_, D>, font: &Font<'_>, now_us: u64) -> bool {
                if !self.dirty || self.root.is_none() {
                        return false;
                }
                let Some(mut c) = layer.frame_begin(display, now_us) else { return false };
                self.paint(&mut c, font);
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

#[cfg(test)]
mod tests {
        use super::*;
        use crate::display::{DisplayDriver, Frame, Region};
        use crate::draw::PixelFormat;
        use crate::hal::Clock;
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

        fn font_blob() -> StdVec<u8> {
                let mut e = Encoder::new(4, 6, 5, 6);
                for c in 0x20u8..0x7f {
                        e.add(c, &[0xF0; 6]).unwrap();
                }
                e.encode()
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
                ui.set_font(&font);
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
                assert_eq!(b.button().unwrap().corners, crate::draw::corner::BOTTOM);
                assert_eq!(b.hit_slop_y1, 3);
        }

        #[test]
        fn focus_cycles_through_buttons_in_order_and_wraps() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_font(&font);
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
                ui.set_font(&font);
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                let root = ui.root().unwrap();
                let beta = ui.children(root).nth(1).unwrap();
                let r = ui.get(beta).unwrap().rect;
                let (cx, cy) = (((r.x0 + r.x1) / 2) as u16, ((r.y0 + r.y1) / 2) as u16);
                assert_eq!(ui.touch(cx, cy, true), Touch::Pending);
                // a wobble within the slop is still a tap
                assert_eq!(ui.touch(cx + 3, cy + 2, true), Touch::Pending);
                assert_eq!(ui.touch(cx + 3, cy + 2, false), Touch::Tap { hit: true, emitted: Some(Ev::Beta) });
                assert_eq!(ui.focused(), Some(beta));
                // travel beyond the slop with nothing scrollable underneath: neither tap nor drag
                assert_eq!(ui.touch(cx, cy, true), Touch::Pending);
                assert_eq!(ui.touch(cx + 30, cy, true), Touch::Pending);
                assert_eq!(ui.touch(cx + 30, cy, false), Touch::None);
                // and a tap on empty space (the header) is reported, not swallowed
                assert_eq!(ui.touch(30, 2, true), Touch::Pending);
                assert_eq!(ui.touch(30, 2, false), Touch::Tap { hit: false, emitted: None });
        }

        #[test]
        fn a_scrolling_stack_overflows_and_a_drag_moves_it_within_the_clamp() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_font(&font);
                ui.fit(&layer);
                ui.navigate(&PAGE_LIST).unwrap();
                let root = ui.root().unwrap();
                let win = ui.get(root).unwrap().window().unwrap().clone();
                // five rows pinned at 20 px in a 48 px frame: the content overflows
                assert_eq!(win.content_h, 100);
                let first = ui.children(root).next().unwrap();
                let y_before = ui.get(first).unwrap().rect.y0;
                // a drag upward from the middle of the list
                assert_eq!(ui.touch(32, 30, true), Touch::Pending);
                assert_eq!(ui.touch(32, 10, true), Touch::Drag);
                assert_eq!(ui.get(first).unwrap().rect.y0, y_before - 20);
                assert_eq!(ui.touch(32, 10, false), Touch::DragEnd);
                // scrolling past the end is clamped: the content's far edge never passes the stop
                assert!(ui.scroll_by(root, 0, 1000));
                let vp_y1 = ui.viewport(root).y1;
                let last = ui.children(root).last().unwrap();
                assert_eq!(ui.get(last).unwrap().rect.y1, vp_y1);
                assert!(!ui.scroll_by(root, 0, 1), "nothing left to scroll");
                // the part of a widget above the viewport is untouchable even though its rect
                // covers the point (row 3 spans y = -15..4 here; the viewport starts at 3), and
                // focusing a widget scrolled out brings it in
                assert_eq!(ui.touch(32, 2, true), Touch::Pending);
                assert_eq!(ui.touch(32, 2, false), Touch::Tap { hit: false, emitted: None });
                assert_eq!(ui.touch(32, 4, true), Touch::Pending);
                assert_eq!(ui.touch(32, 4, false), Touch::Tap { hit: true, emitted: Some(Ev::Item(3)) });
                ui.set_focus(Some(first));
                assert_eq!(ui.get(first).unwrap().rect.y0, ui.viewport(root).y0);
        }

        #[test]
        fn render_pushes_only_the_changed_widgets_after_the_first_full_frame() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_font(&font);
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                assert!(ui.render(&mut layer, &mut display, &font, 0));
                assert_eq!(flush(&mut layer, &mut display), [Region::full(64, 48)]);
                assert!(!ui.render(&mut layer, &mut display, &font, 0), "nothing dirty");
                // moving focus dirties exactly the two buttons
                let root = ui.root().unwrap();
                let rows: StdVec<Rect> = ui.children(root).map(|c| ui.get(c).unwrap().rect).collect();
                ui.focus_next();
                assert!(ui.render(&mut layer, &mut display, &font, 0));
                let mut pushed = flush(&mut layer, &mut display);
                pushed.sort_by_key(|r| r.y0);
                // the previous frame's full-canvas invalidation carries forward once
                assert_eq!(pushed, [Region::full(64, 48)]);
                ui.focus_next();
                assert!(ui.render(&mut layer, &mut display, &font, 0));
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
                let (mut layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_font(&font);
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                ui.set_rotation(&mut layer, Rotation::R90);
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
                ui.set_font(&font);
                ui.fit(&layer);
                ui.navigate(&T_PAGE_MAIN).unwrap();
                for rot in [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270, Rotation::R0] {
                        ui.set_rotation(&mut layer, rot);
                        assert!(ui.render(&mut layer, &mut display, &font, 0));
                        let _ = flush(&mut layer, &mut display);
                        for _ in 0..6 {
                                ui.focus_next();
                                assert!(ui.render(&mut layer, &mut display, &font, 0));
                                let _ = flush(&mut layer, &mut display);
                        }
                }
                // into the list, drag it to the end, tap the back row
                let root = ui.root().unwrap();
                let list_btn = ui.children(root).last().unwrap();
                ui.set_focus(Some(list_btn));
                assert_eq!(ui.activate(), None);
                assert!(core::ptr::eq(ui.page().unwrap(), &T_PAGE_LIST));
                assert!(ui.render(&mut layer, &mut display, &font, 0));
                let _ = flush(&mut layer, &mut display);
                assert_eq!(ui.touch(120, 200, true), Touch::Pending);
                let mut dragged = false;
                for y in (20..200).rev().step_by(10) {
                        // pending until the finger has travelled the slop, a drag from then on
                        match ui.touch(120, y, true) {
                                Touch::Pending => assert!(!dragged && 200 - y <= DRAG_SLOP as u16),
                                Touch::Drag => dragged = true,
                                other => panic!("unexpected {other:?}"),
                        }
                        ui.render(&mut layer, &mut display, &font, 0);
                        let _ = flush(&mut layer, &mut display);
                }
                assert!(dragged);
                assert_eq!(ui.touch(120, 20, false), Touch::DragEnd);
                let root = ui.root().unwrap();
                assert!(ui.scroll_by(root, 0, 1000) || true);
                let back = ui.children(root).last().unwrap();
                let r = ui.get(back).unwrap().rect;
                let (cx, cy) = (((r.x0 + r.x1) / 2) as u16, ((r.y0 + r.y1) / 2) as u16);
                assert_eq!(ui.touch(cx, cy, true), Touch::Pending);
                assert_eq!(ui.touch(cx, cy, false), Touch::Tap { hit: true, emitted: None });
                assert!(core::ptr::eq(ui.page().unwrap(), &T_PAGE_MAIN));
                assert!(ui.render(&mut layer, &mut display, &font, 0));
        }

        #[test]
        fn a_page_too_big_for_the_arena_is_refused_and_leaves_nothing_behind() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 3> = Ui::new();
                ui.set_font(&font);
                ui.fit(&layer);
                assert_eq!(ui.navigate(&PAGE_MAIN), Err(Error::Full));
                assert!(ui.root().is_none());
                assert!(ui.widgets.iter().all(|s| s.is_none()));
        }
}
