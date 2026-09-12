//! The device-UI preview and the design it edits.
//!
//! The preview DISPLAYS through the LUI binary path -- it compiles the design to a blob with
//! crush-core and renders it with `light_ui::Ui::build_lui`, exactly as firmware would. So what the
//! editor shows is what a device shows from the same blob; there is no separate host render path to
//! drift. The editable `Design` is kept alongside for editing, navigation resolution and the
//! inspector; every edit recompiles the blob and rebuilds. Buttons emit their child index, which the
//! preview resolves against the design's navigation (goto/back).

use std::path::PathBuf;

use light_core::hal::Clock;
use light_display::{Display, DisplayDriver, Frame, FrameLayer, Region};
use light_draw::PixelFormat;
use light_host_gui::now_us;
use light_ui::lui::code;
use light_ui::{Fonts, Lui, Rect, Style, Theme, Touch, Ui, WidgetId};

use crate::design::{self, ChildDef, Design};
use crate::font;

/// A selected node in the current page: a top-level child (`sub` = `None`), or the `sub`-th child
/// inside the frame at `top`. One level deep, matching the format.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sel {
        pub top: usize,
        pub sub: Option<usize>,
}

/// The preview font's pixel size -- the framework's common device size (16 px, what the touch
/// boards render at), so a label fits the preview exactly as it fits the glass. A design carries no
/// font, so this is a fixed default until a font picker exists.
const PIXEL_SIZE: u16 = 16;

/// Widget arena capacity per page.
const UI_WIDGETS: usize = 32;

/// The look, loaded from the framework's real steel theme.
const THEME_JSON: &str = include_str!("../../../themes/steel.json");

/// The design shipped as the default; a saved file beside the executable overrides it.
const DEFAULT_DESIGN_JSON: &str = include_str!("../design.json");

/// A do-nothing [`DisplayDriver`]: the preview renders into the [`Display`]'s own buffer and reads
/// it back with [`Display::front`], so nothing is ever pushed.
struct NullDriver;

impl DisplayDriver for NullDriver {
        fn init(&mut self, _clock: &mut dyn Clock, _width: u16, _height: u16) {}
        fn chunk_count(&self, _region: &Region) -> u16 {
                0
        }
        fn chunks_per_poll(&self, _region: &Region) -> u16 {
                0
        }
        fn kick(&mut self, _frame: &Frame<'_>, _region: &Region, _index: u16) {}
        fn chunk_complete(&mut self) -> bool {
                true
        }
        fn chunk_timeout_ms(&self) -> u32 {
                1000
        }
}

pub struct Preview {
        ui: Ui<u16, UI_WIDGETS>,
        display: Display<'static, NullDriver>,
        layer: FrameLayer,
        theme: Theme,
        font: light_font::Font<'static>,
        /// The editable model; the source of truth, saved back to [`path`](Self::path).
        design: Design,
        /// The design compiled to an LUI blob (leaked `'static`), reparsed on each recompile -- what
        /// the preview actually reads and displays.
        lui: Lui<'static>,
        /// The visited-page stack; its last entry is the page shown.
        history: Vec<usize>,
        /// The selected node in the current page (Edit mode): a top-level child or one inside a frame.
        selected: Option<Sel>,
        dev_w: u16,
        dev_h: u16,
        path: PathBuf,
        /// Write the compiled `.lui` beside the design too. On for the editor's own default file (a
        /// self-contained artifact); OFF when editing a named design (an app crate's), where the
        /// build system owns compilation and a stray blob would clutter the source tree.
        write_sidecar: bool,
}

impl Preview {
        /// Open a design. `path` names the file to edit (an app crate's `design.json`); `None` uses
        /// the editor's own file beside the executable.
        pub fn new(path: Option<PathBuf>) -> Self {
                let write_sidecar = path.is_none();
                let path = path.unwrap_or_else(save_path);
                let design = std::fs::read_to_string(&path)
                        .ok()
                        .as_deref()
                        .and_then(|j| design::parse(j).ok())
                        .unwrap_or_else(|| design::parse(DEFAULT_DESIGN_JSON).expect("the bundled design parses"));

                let (dev_w, dev_h) = (design.device.width.max(1), design.device.height.max(1));
                let font = font::load(PIXEL_SIZE);
                let lth = crush_core::theme::compile_flat(THEME_JSON).expect("the bundled steel theme compiles");
                let theme = Theme::parse(&lth).expect("the compiled theme parses");
                let buf: &'static mut [u8] = Vec::leak(vec![0u8; PixelFormat::Rgb565.buffer_len(dev_w, dev_h)]);
                let display = Display::new(NullDriver, buf, dev_w, dev_h, PixelFormat::Rgb565, now_us);
                let mut layer = FrameLayer::new(dev_w, dev_h, PixelFormat::Rgb565);
                layer.bg = theme.bg;
                let mut ui = Ui::new();
                ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
                ui.fit(&layer);

                let lui = compile_blob(&design);
                let root = lui.root().min(lui.page_count().saturating_sub(1));
                let mut this = Self { ui, display, layer, theme, font, design, lui, history: vec![root], selected: None, dev_w, dev_h, path, write_sidecar };
                this.build_current();
                this
        }

        /// Render a frame into the off-screen buffer if anything changed.
        pub fn render(&mut self, now_us: u64) -> bool {
                let style = Style::new(self.theme, Fonts::uniform(&self.font));
                let drew = self.ui.render(&mut self.layer, &mut self.display, &style, now_us);
                while self.layer.poll(&mut self.display).unwrap_or(false) {}
                drew || self.ui.is_animating()
        }

        // --- Run mode: taps navigate ---------------------------------------------------------

        /// Feed a touch that drives the UI as the device would. A tapped button's child index is
        /// resolved against the design's navigation (goto/back).
        pub fn interact(&mut self, x: u16, y: u16, touching: bool, now_us: u64) {
                if let Touch::Tap { emitted: Some(slot), .. } = self.ui.touch(x, y, touching, now_us) {
                        //   resolve the tapped child's navigation from the blob (the firmware path)
                        let nav = self.lui.page(self.current()).and_then(|p| p.children().nth(usize::from(slot)));
                        if let Some(child) = nav {
                                match child.nav {
                                        code::NAV_GOTO => self.goto(usize::from(child.nav_page)),
                                        code::NAV_BACK => self.back(),
                                        _ => {}
                                }
                        }
                }
        }

        fn goto(&mut self, idx: usize) {
                if idx < self.lui.page_count() {
                        self.history.push(idx);
                        self.selected = None;
                        self.build_current();
                }
        }

        fn back(&mut self) {
                if self.history.len() > 1 {
                        self.history.pop();
                        self.selected = None;
                        self.build_current();
                }
        }

        /// Start a fresh run from the root page.
        pub fn start_run(&mut self) {
                self.history = vec![self.lui.root().min(self.lui.page_count().saturating_sub(1))];
                self.selected = None;
                self.build_current();
        }

        // --- Edit mode: selection and structural edits --------------------------------------

        /// Select the widget at a point in device pixels, or clear the selection. Walks the built
        /// tree in step with the design (the two are 1:1 and in the same order): a hit inside a
        /// frame selects the child under the point, or the frame itself if the point is between its
        /// children.
        pub fn select_at(&mut self, x: i32, y: i32) {
                self.selected = None;
                let Some(root) = self.ui.root() else { return };
                let cur = self.current();
                let top_ids: Vec<_> = self.ui.child_ids(root).collect();
                for (i, &top_id) in top_ids.iter().enumerate() {
                        if !self.hit_widget(top_id, x, y) {
                                continue;
                        }
                        let is_frame = self.design.pages.get(cur).and_then(|p| p.children.get(i)).is_some_and(ChildDef::is_frame);
                        if is_frame {
                                let sub_ids: Vec<_> = self.ui.child_ids(top_id).collect();
                                for (j, &sub_id) in sub_ids.iter().enumerate() {
                                        if self.hit_widget(sub_id, x, y) {
                                                self.selected = Some(Sel { top: i, sub: Some(j) });
                                                return;
                                        }
                                }
                        }
                        self.selected = Some(Sel { top: i, sub: None });
                        return;
                }
        }

        /// Whether a built widget's rect contains a device point.
        fn hit_widget(&self, id: WidgetId, x: i32, y: i32) -> bool {
                self.ui.get(id).map(|w| w.rect).is_some_and(|r| x >= r.x0 && x <= r.x1 && y >= r.y0 && y <= r.y1)
        }

        /// Show a page for editing.
        pub fn show_page(&mut self, idx: usize) {
                if idx < self.lui.page_count() {
                        self.history = vec![idx];
                        self.selected = None;
                        self.build_current();
                }
        }

        /// Add a button. Into the selected frame when one (or a child of one) is selected, so a frame
        /// can be filled; otherwise at top level. Selects the new button.
        pub fn add_button(&mut self) {
                let cur = self.current();
                let into_frame = self.selected_frame_top();
                if let Some(page) = self.design.pages.get_mut(cur) {
                        if let Some(top) = into_frame {
                                if let Some(frame) = page.children.get_mut(top) {
                                        frame.children.push(ChildDef::new_button());
                                        self.selected = Some(Sel { top, sub: Some(frame.children.len() - 1) });
                                }
                        } else {
                                page.children.push(ChildDef::new_button());
                                self.selected = Some(Sel { top: page.children.len() - 1, sub: None });
                        }
                }
                self.recompile();
        }

        /// Add an empty frame at top level (frames do not nest) and select it, ready to fill.
        pub fn add_frame(&mut self) {
                let cur = self.current();
                if let Some(page) = self.design.pages.get_mut(cur) {
                        let mut frame = ChildDef::new_button();
                        frame.button = None;
                        frame.layout = Some("linear".to_owned());
                        frame.children.push(ChildDef::new_button());
                        page.children.push(frame);
                        self.selected = Some(Sel { top: page.children.len() - 1, sub: None });
                }
                self.recompile();
        }

        pub fn delete_selected(&mut self) {
                let cur = self.current();
                if let Some(sel) = self.selected {
                        if let Some(list) = self.container_mut(cur, sel) {
                                let idx = sel.sub.unwrap_or(sel.top);
                                if idx < list.len() {
                                        list.remove(idx);
                                        let len = list.len();
                                        self.selected = match sel.sub {
                                                _ if len == 0 && sel.sub.is_some() => Some(Sel { top: sel.top, sub: None }),
                                                Some(_) => Some(Sel { top: sel.top, sub: Some(idx.min(len - 1)) }),
                                                None if len == 0 => None,
                                                None => Some(Sel { top: idx.min(len - 1), sub: None }),
                                        };
                                }
                        }
                }
                self.recompile();
        }

        pub fn move_selected(&mut self, delta: i32) {
                let cur = self.current();
                if let Some(sel) = self.selected {
                        let idx = sel.sub.unwrap_or(sel.top);
                        if let Some(list) = self.container_mut(cur, sel) {
                                let target = idx as i32 + delta;
                                if target >= 0 && (target as usize) < list.len() {
                                        list.swap(idx, target as usize);
                                        self.selected = Some(match sel.sub {
                                                Some(_) => Sel { top: sel.top, sub: Some(target as usize) },
                                                None => Sel { top: target as usize, sub: None },
                                        });
                                }
                        }
                }
                self.recompile();
        }

        /// Set the selected widget's text (a no-op on a frame, which has none).
        pub fn set_selected_text(&mut self, s: &str) {
                if let Some(c) = self.selected_child_mut() {
                        if c.button.is_some() {
                                c.button = Some(s.to_owned());
                        } else if c.label.is_some() {
                                c.label = Some(s.to_owned());
                        }
                }
                self.recompile();
        }

        /// Cycle the selected button's action: none -> back -> goto(0..) -> none.
        pub fn cycle_selected_action(&mut self) {
                let pages = self.design.pages.len();
                {
                        let Some(c) = self.selected_child_mut() else {
                                return;
                        };
                        if c.button.is_none() {
                                return;
                        }
                        if c.back {
                                c.back = false;
                                c.goto = if pages > 0 { Some(0) } else { None };
                        } else if let Some(i) = c.goto {
                                c.goto = if i + 1 < pages { Some(i + 1) } else { None };
                        } else {
                                c.back = true;
                        }
                }
                self.recompile();
        }

        /// Cycle a selected frame's layout: stack -> row -> linear -> stack.
        pub fn cycle_frame_layout(&mut self) {
                if let Some(c) = self.selected_child_mut() {
                        if c.is_frame() {
                                c.layout = Some(match c.layout.as_deref() {
                                        Some("row") => "linear",
                                        Some("linear") => "stack",
                                        _ => "row",
                                }
                                .to_owned());
                        }
                }
                self.recompile();
        }

        /// Cycle a selected frame's scroll: none -> vertical -> horizontal -> none.
        pub fn cycle_frame_scroll(&mut self) {
                if let Some(c) = self.selected_child_mut() {
                        if c.is_frame() {
                                c.scroll = match c.scroll.as_deref() {
                                        None => Some("vertical".to_owned()),
                                        Some("vertical") => Some("horizontal".to_owned()),
                                        _ => None,
                                };
                        }
                }
                self.recompile();
        }

        /// Toggle whether the selected node grows to take a linear layout's surplus.
        pub fn toggle_selected_grow(&mut self) {
                if let Some(c) = self.selected_child_mut() {
                        c.grow = !c.grow;
                }
                self.recompile();
        }

        /// The top index of the frame the selection sits in or on, for adding into it.
        fn selected_frame_top(&self) -> Option<usize> {
                let sel = self.selected?;
                let page = self.design.pages.get(self.current())?;
                let top = page.children.get(sel.top)?;
                top.is_frame().then_some(sel.top)
        }

        /// The child list the selection lives in: a frame's children when `sub` is set, else the
        /// page's top-level children.
        fn container_mut(&mut self, cur: usize, sel: Sel) -> Option<&mut Vec<ChildDef>> {
                let page = self.design.pages.get_mut(cur)?;
                match sel.sub {
                        Some(_) => page.children.get_mut(sel.top).map(|f| &mut f.children),
                        None => Some(&mut page.children),
                }
        }

        /// Recompile the design to a fresh blob, rebuild the current page, and save.
        fn recompile(&mut self) {
                self.lui = compile_blob(&self.design);
                self.build_current();
                self.save();
        }

        /// Build the current page from the blob into the widget tree.
        fn build_current(&mut self) {
                let cur = self.current().min(self.lui.page_count().saturating_sub(1));
                let page = self.lui.page(cur);
                if let Some(page) = page {
                        let _ = self.ui.build_lui(&page);
                }
        }

        fn save(&self) {
                if let Err(e) = std::fs::write(&self.path, design::to_json(&self.design)) {
                        eprintln!("light-ui-editor: could not save '{}': {e}", self.path.display());
                }
                //   also the compiled blob beside it, so the editor's own file yields a usable
                // artifact; NOT for a named design, where the build compiles it and a stray blob
                // would dirty the source tree
                if self.write_sidecar {
                        if let Ok(blob) = crush_core::lui::compile(&self.design) {
                                let _ = std::fs::write(self.path.with_extension("lui"), blob);
                        }
                }
        }

        // --- accessors for the editor chrome ------------------------------------------------

        fn current(&self) -> usize {
                *self.history.last().unwrap_or(&0)
        }

        fn selected_child(&self) -> Option<&ChildDef> {
                let sel = self.selected?;
                let top = self.design.pages.get(self.current())?.children.get(sel.top)?;
                match sel.sub {
                        Some(j) => top.children.get(j),
                        None => Some(top),
                }
        }

        fn selected_child_mut(&mut self) -> Option<&mut ChildDef> {
                let sel = self.selected?;
                let cur = self.current();
                let top = self.design.pages.get_mut(cur)?.children.get_mut(sel.top)?;
                match sel.sub {
                        Some(j) => top.children.get_mut(j),
                        None => Some(top),
                }
        }

        /// The built widget for the selection, walking the tree in step with the design path.
        fn selected_widget(&self) -> Option<WidgetId> {
                let sel = self.selected?;
                let root = self.ui.root()?;
                let top_id = self.ui.child_ids(root).nth(sel.top)?;
                match sel.sub {
                        Some(j) => self.ui.child_ids(top_id).nth(j),
                        None => Some(top_id),
                }
        }

        pub fn page_count(&self) -> usize {
                self.design.pages.len()
        }

        pub fn page_title(&self, idx: usize) -> &str {
                self.design.pages.get(idx).map_or("", |p| p.title.as_str())
        }

        pub fn current_page(&self) -> usize {
                self.current()
        }

        pub fn selected(&self) -> Option<Sel> {
                self.selected
        }

        pub fn selected_is_frame(&self) -> bool {
                self.selected_child().is_some_and(ChildDef::is_frame)
        }

        /// A frame's layout name for the inspector (`None` off a frame).
        pub fn selected_layout_label(&self) -> Option<String> {
                let c = self.selected_child()?;
                c.is_frame().then(|| c.layout.clone().unwrap_or_else(|| "stack".to_owned()))
        }

        /// A frame's scroll name for the inspector (`None` off a frame).
        pub fn selected_scroll_label(&self) -> Option<String> {
                let c = self.selected_child()?;
                c.is_frame().then(|| c.scroll.clone().unwrap_or_else(|| "none".to_owned()))
        }

        pub fn selected_grow(&self) -> bool {
                self.selected_child().is_some_and(|c| c.grow)
        }

        pub fn selected_describe(&self) -> Option<String> {
                self.selected_child().map(ChildDef::describe)
        }

        pub fn selected_text(&self) -> Option<String> {
                let c = self.selected_child()?;
                c.button.clone().or_else(|| c.label.clone())
        }

        pub fn selected_is_button(&self) -> bool {
                self.selected_child().is_some_and(|c| c.button.is_some())
        }

        pub fn selected_action_label(&self) -> Option<String> {
                let c = self.selected_child()?;
                if c.button.is_none() {
                        return None;
                }
                Some(if let Some(g) = c.goto {
                        format!("goto {}", self.page_title(g))
                } else if c.back {
                        "back".to_owned()
                } else {
                        "none".to_owned()
                })
        }

        pub fn selected_rect(&self) -> Option<Rect> {
                let id = self.selected_widget()?;
                self.ui.get(id).map(|w| w.rect)
        }

        pub fn size(&self) -> (u16, u16) {
                (self.dev_w, self.dev_h)
        }

        pub fn corner_radius(&self) -> u16 {
                self.design.device.corner_radius.min(self.dev_w.min(self.dev_h) / 2)
        }

        pub fn pixels(&self) -> &[u8] {
                self.display.front().unwrap_or(&[])
        }
}

/// Compile a design to an LUI blob, leak it `'static`, and parse it -- the blob the preview reads.
fn compile_blob(design: &Design) -> Lui<'static> {
        let bytes = crush_core::lui::compile(design).expect("the design compiles to LUI");
        let leaked: &'static [u8] = Vec::leak(bytes);
        Lui::parse(leaked).expect("the compiled LUI blob parses")
}

/// The design's load/save path: `design.json` beside the executable.
fn save_path() -> PathBuf {
        std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("design.json")))
                .unwrap_or_else(|| PathBuf::from("design.json"))
}
