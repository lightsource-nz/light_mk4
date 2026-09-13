//! The device-UI preview and the design it edits.
//!
//! The preview DISPLAYS through the LUI binary path -- it compiles the design to a blob with
//! crush-core and renders it with `light_ui::Ui::build_lui`, exactly as firmware would. So what the
//! editor shows is what a device shows from the same blob; there is no separate host render path to
//! drift. The editable `Design` is kept alongside for editing, navigation resolution and the
//! inspector; every edit recompiles the blob and rebuilds. Buttons emit their child index, which the
//! preview resolves against the design's navigation (goto/back).

use std::path::PathBuf;

use light_display::{Display, FrameLayer};
use light_draw::PixelFormat;
use light_host_gui::{now_us, NullDriver};
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

/// The starting content for a design file that does not exist yet: one page, one button.
const STARTER_JSON: &str = r#"{ "pages": [ { "title": "Page", "children": [ { "button": "Button" } ] } ] }"#;

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
        /// The editable theme, and where it saves: `theme.json` beside the design. The preview
        /// styles the design with it, so editing the look is live.
        theme_src: crush_core::theme::ThemeSource,
        theme_path: PathBuf,
}

impl Preview {
        /// Open the design at `path` -- the file the editor edits and saves back to. A path that does
        /// not exist yet starts from [`STARTER_JSON`] and is created on the first save. The build
        /// compiles the design to a blob, so the editor writes only the JSON.
        pub fn new(path: PathBuf) -> Self {
                let design = std::fs::read_to_string(&path)
                        .ok()
                        .as_deref()
                        .and_then(|j| design::parse(j).ok())
                        .unwrap_or_else(|| design::parse(STARTER_JSON).expect("the starter design parses"));

                //   a landscape design is authored against the panel turned onto its long edge:
                // swap the panel dimensions for the preview and lay the tree out along the
                // horizontal axis, exactly as the firmware does with its rotation + layout axis, so
                // the fixed widths (sized for the long edge) read right instead of half-screen
                let landscape = design.landscape();
                let (pw, ph) = (design.device.width.max(1), design.device.height.max(1));
                let (dev_w, dev_h) = if landscape { (ph, pw) } else { (pw, ph) };
                let font = font::load(PIXEL_SIZE);
                //   the theme is edited too: load theme.json beside the design if present, else start
                // from the framework's steel defaults. The preview styles the design with it.
                let theme_path = path.parent().map(|d| d.join("theme.json")).unwrap_or_else(|| PathBuf::from("theme.json"));
                let theme_src = std::fs::read_to_string(&theme_path)
                        .ok()
                        .as_deref()
                        .and_then(|j| crush_core::theme::parse_source(j).ok())
                        .unwrap_or_else(|| crush_core::theme::parse_source(THEME_JSON).expect("the bundled steel theme parses"));
                let lth = crush_core::theme::compile_source(&theme_src).expect("the theme compiles");
                let theme = Theme::parse(&lth).expect("the compiled theme parses");
                let buf: &'static mut [u8] = Vec::leak(vec![0u8; PixelFormat::Rgb565.buffer_len(dev_w, dev_h)]);
                let display = Display::new(NullDriver, buf, dev_w, dev_h, PixelFormat::Rgb565, now_us);
                let mut layer = FrameLayer::new(dev_w, dev_h, PixelFormat::Rgb565);
                layer.bg = theme.bg;
                let mut ui = Ui::new();
                ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
                if landscape {
                        ui.set_layout_axis(light_ui::Axis::Horizontal);
                }
                ui.fit(&layer);

                let lui = compile_blob(&design);
                let root = lui.root().min(lui.page_count().saturating_sub(1));
                let mut this = Self { ui, display, layer, theme, font, design, lui, history: vec![root], selected: None, dev_w, dev_h, path, theme_src, theme_path };
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

        // --- direct setters, bound to real controls (egui) rather than cycled -----------------

        /// Select a node, or clear the selection -- from a preview click or the outline list.
        pub fn select(&mut self, sel: Option<Sel>) {
                self.selected = sel;
        }

        /// Set the selected node's grow flag.
        pub fn set_selected_grow(&mut self, grow: bool) {
                if let Some(c) = self.selected_child_mut() {
                        c.grow = grow;
                }
                self.recompile();
        }

        /// Set the selected node's minimum size (0 = unset).
        pub fn set_selected_min(&mut self, w: u16, h: u16) {
                if let Some(c) = self.selected_child_mut() {
                        c.min_w = w;
                        c.min_h = h;
                }
                self.recompile();
        }

        /// Set the selected node's maximum size (0 = unset; equal to min pins the size).
        pub fn set_selected_max(&mut self, w: u16, h: u16) {
                if let Some(c) = self.selected_child_mut() {
                        c.max_w = w;
                        c.max_h = h;
                }
                self.recompile();
        }

        /// Set a selected frame's layout (`stack`/`row`/`linear`).
        pub fn set_selected_layout(&mut self, layout: &str) {
                if let Some(c) = self.selected_child_mut() {
                        if c.is_frame() {
                                c.layout = Some(layout.to_owned());
                        }
                }
                self.recompile();
        }

        /// Set a selected frame's scroll axis (`vertical`/`horizontal`, or `None` for no scroll).
        pub fn set_selected_scroll(&mut self, scroll: Option<&str>) {
                if let Some(c) = self.selected_child_mut() {
                        if c.is_frame() {
                                c.scroll = scroll.map(str::to_owned);
                        }
                }
                self.recompile();
        }

        /// Set a selected button's navigation: `goto` a page, or `back`, or neither.
        pub fn set_selected_action(&mut self, goto: Option<usize>, back: bool) {
                if let Some(c) = self.selected_child_mut() {
                        if c.button.is_some() {
                                c.goto = goto;
                                c.back = back && goto.is_none();
                        }
                }
                self.recompile();
        }

        /// Rename the current page.
        pub fn set_page_title(&mut self, title: &str) {
                let cur = self.current();
                if let Some(p) = self.design.pages.get_mut(cur) {
                        p.title = title.to_owned();
                }
                self.recompile();
        }

        /// Append a page (one empty stack) and show it.
        pub fn add_page(&mut self) {
                self.design.pages.push(design::PageDef {
                        title: "Page".to_owned(),
                        layout: "stack".to_owned(),
                        gap: 6,
                        scroll: false,
                        subtitle: false,
                        children: Vec::new(),
                });
                let idx = self.design.pages.len() - 1;
                self.recompile();
                self.show_page(idx);
        }

        // --- theme editing -------------------------------------------------------------------

        /// Recompile the edited theme, restyle the preview live, and save `theme.json`.
        fn apply_theme(&mut self) {
                if let Ok(lth) = crush_core::theme::compile_source(&self.theme_src) {
                        if let Ok(theme) = Theme::parse(&lth) {
                                self.theme = theme;
                                self.layer.bg = theme.bg;
                                self.ui.set_style(&Style::new(theme, Fonts::uniform(&self.font)));
                                //   rebuild so windows re-resolve their corner radius from the new
                                // theme metrics (resolved at creation, not per frame)
                                self.build_current();
                        }
                }
                if let Err(e) = std::fs::write(&self.theme_path, crush_core::theme::source_to_json(&self.theme_src)) {
                        eprintln!("light-ui-editor: could not save '{}': {e}", self.theme_path.display());
                }
        }

        /// A theme colour by key, as the effective RGB565 (defaults resolved) -- for a colour picker.
        pub fn theme_color(&self, key: &str) -> u16 {
                match key {
                        "bg" => self.theme.bg,
                        "bar" => self.theme.bar.unwrap_or(self.theme.bg),
                        "frame" => self.theme.frame,
                        "title" => self.theme.title,
                        "text" => self.theme.text,
                        "button_outline" => self.theme.button_outline,
                        "button_text" => self.theme.button_text,
                        "focus_text" => self.theme.focus_text,
                        "indicator" => self.theme.indicator,
                        _ => 0,
                }
        }

        /// Set a theme colour (RGB565), restyle live and save.
        pub fn set_theme_color(&mut self, key: &str, rgb565: u16) {
                self.theme_src.colors.insert(key.to_owned(), format!("{rgb565:04X}"));
                self.apply_theme();
        }

        /// A theme metric by key (`radius`/`screen_radius`), effective value.
        pub fn theme_metric(&self, key: &str) -> u16 {
                match key {
                        "radius" => u16::from(self.theme.radius),
                        "screen_radius" => u16::from(self.theme.screen_radius),
                        _ => 0,
                }
        }

        /// Set a theme metric, restyle live and save.
        pub fn set_theme_metric(&mut self, key: &str, value: u16) {
                self.theme_src.metrics.insert(key.to_owned(), value);
                self.apply_theme();
        }

        /// A theme surface (`focus`/`button`) as its gradient `(from, to)` RGB565, or `None` when the
        /// surface is flat (no gradient).
        pub fn theme_surface(&self, key: &str) -> Option<(u16, u16)> {
                let s = match key {
                        "focus" => self.theme.focus_surface,
                        "button" => self.theme.button_surface,
                        _ => None,
                }?;
                Some((s.from, s.to))
        }

        /// Set a surface's gradient endpoints (RGB565), restyle live and save.
        pub fn set_theme_surface(&mut self, key: &str, from: u16, to: u16) {
                self.theme_src.surfaces.insert(key.to_owned(), Some(crush_core::theme::ShadeSource { from: format!("{from:04X}"), to: format!("{to:04X}") }));
                self.apply_theme();
        }

        /// Remove a surface's gradient (it paints flat), restyle live and save.
        pub fn clear_theme_surface(&mut self, key: &str) {
                self.theme_src.surfaces.remove(key);
                self.apply_theme();
        }

        /// The theme's page-descent edge as a label: `none`/`top`/`bottom`/`left`/`right`.
        pub fn theme_descent_label(&self) -> &'static str {
                match self.theme.descent {
                        None => "none",
                        Some(light_ui::Descent::FromTop) => "top",
                        Some(light_ui::Descent::FromBottom) => "bottom",
                        Some(light_ui::Descent::FromLeft) => "left",
                        Some(light_ui::Descent::FromRight) => "right",
                }
        }

        /// Set the theme's page-descent edge (`None` clears it), restyle live and save.
        pub fn set_theme_descent(&mut self, descent: Option<&str>) {
                self.theme_src.descent = descent.map(str::to_owned);
                self.apply_theme();
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
                //   only the JSON: the build compiles it to a blob, so a stray .lui beside the
                // source would just be clutter
                if let Err(e) = std::fs::write(&self.path, design::to_json(&self.design)) {
                        eprintln!("light-ui-editor: could not save '{}': {e}", self.path.display());
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

        /// The selected node's min size `(w, h)`.
        pub fn selected_min(&self) -> Option<(u16, u16)> {
                self.selected_child().map(|c| (c.min_w, c.min_h))
        }

        /// The selected node's max size `(w, h)`.
        pub fn selected_max(&self) -> Option<(u16, u16)> {
                self.selected_child().map(|c| (c.max_w, c.max_h))
        }

        /// The selected button's navigation `(goto page, back)`; `None` off a button.
        pub fn selected_action(&self) -> Option<(Option<usize>, bool)> {
                let c = self.selected_child()?;
                c.button.is_some().then_some((c.goto, c.back))
        }

        /// The current page's widgets as a selectable tree: `(path, indent, label)`, a frame's
        /// children indented under it. For an inspector list that selects without hunting the preview.
        pub fn outline(&self) -> Vec<(Sel, u8, String)> {
                let mut out = Vec::new();
                if let Some(page) = self.design.pages.get(self.current()) {
                        for (i, c) in page.children.iter().enumerate() {
                                out.push((Sel { top: i, sub: None }, 0, c.describe()));
                                if c.is_frame() {
                                        for (j, sc) in c.children.iter().enumerate() {
                                                out.push((Sel { top: i, sub: Some(j) }, 1, sc.describe()));
                                        }
                                }
                        }
                }
                out
        }

        /// Whether the design is laid out landscape (read-only info for the inspector).
        pub fn is_landscape(&self) -> bool {
                self.design.landscape()
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

/// The design file to open: the given `arg`, or one found in the current working directory. With no
/// argument the editor prefers `./design.json`, else the first `*.json` there that parses as a
/// design; with none, it falls back to `./design.json` (created on the first save). There is no
/// path baked into the binary -- the editor edits designs where you run it, or where you point it.
pub fn resolve_design_path(arg: Option<PathBuf>) -> PathBuf {
        if let Some(p) = arg {
                return p;
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let preferred = cwd.join("design.json");
        if preferred.exists() {
                return preferred;
        }
        if let Ok(entries) = std::fs::read_dir(&cwd) {
                let mut designs: Vec<PathBuf> = entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                        .filter(|p| std::fs::read_to_string(p).ok().and_then(|j| design::parse(&j).ok()).is_some())
                        .collect();
                designs.sort();
                if let Some(first) = designs.into_iter().next() {
                        return first;
                }
        }
        preferred
}
