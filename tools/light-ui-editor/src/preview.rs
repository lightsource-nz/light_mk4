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
use light_ui::{Fonts, Lui, Rect, Style, Theme, Touch, Ui};

use crate::design::{self, ChildDef, Design};
use crate::font;

/// The preview font's pixel size.
const PIXEL_SIZE: u16 = 18;

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
        /// The selected child's index in the current page (Edit mode).
        selected: Option<usize>,
        dev_w: u16,
        dev_h: u16,
        path: PathBuf,
}

impl Preview {
        pub fn new() -> Self {
                let path = save_path();
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
                let mut this = Self { ui, display, layer, theme, font, design, lui, history: vec![root], selected: None, dev_w, dev_h, path };
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

        /// Select the widget at a point in device pixels, or clear the selection.
        pub fn select_at(&mut self, x: i32, y: i32) {
                let count = self.design.pages.get(self.current()).map_or(0, |p| p.children.len());
                self.selected = None;
                for i in 0..count {
                        if let Some(id) = self.ui.find((i + 1) as u8) {
                                if let Some(w) = self.ui.get(id) {
                                        let r = w.rect;
                                        if x >= r.x0 && x <= r.x1 && y >= r.y0 && y <= r.y1 {
                                                self.selected = Some(i);
                                                break;
                                        }
                                }
                        }
                }
        }

        /// Show a page for editing.
        pub fn show_page(&mut self, idx: usize) {
                if idx < self.lui.page_count() {
                        self.history = vec![idx];
                        self.selected = None;
                        self.build_current();
                }
        }

        pub fn add_button(&mut self) {
                let cur = self.current();
                if let Some(page) = self.design.pages.get_mut(cur) {
                        page.children.push(ChildDef::new_button());
                        self.selected = Some(page.children.len() - 1);
                }
                self.recompile();
        }

        pub fn delete_selected(&mut self) {
                let cur = self.current();
                if let Some(sel) = self.selected {
                        if let Some(page) = self.design.pages.get_mut(cur) {
                                if sel < page.children.len() {
                                        page.children.remove(sel);
                                        self.selected = if page.children.is_empty() { None } else { Some(sel.min(page.children.len() - 1)) };
                                }
                        }
                }
                self.recompile();
        }

        pub fn move_selected(&mut self, delta: i32) {
                let cur = self.current();
                if let Some(sel) = self.selected {
                        if let Some(page) = self.design.pages.get_mut(cur) {
                                let target = sel as i32 + delta;
                                if target >= 0 && (target as usize) < page.children.len() {
                                        page.children.swap(sel, target as usize);
                                        self.selected = Some(target as usize);
                                }
                        }
                }
                self.recompile();
        }

        /// Set the selected widget's text.
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
                //   also the compiled blob beside it, so a design yields a usable artifact
                if let Ok(blob) = crush_core::lui::compile(&self.design) {
                        let _ = std::fs::write(self.path.with_extension("lui"), blob);
                }
        }

        // --- accessors for the editor chrome ------------------------------------------------

        fn current(&self) -> usize {
                *self.history.last().unwrap_or(&0)
        }

        fn selected_child(&self) -> Option<&ChildDef> {
                self.design.pages.get(self.current())?.children.get(self.selected?)
        }

        fn selected_child_mut(&mut self) -> Option<&mut ChildDef> {
                let cur = self.current();
                let sel = self.selected?;
                self.design.pages.get_mut(cur)?.children.get_mut(sel)
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

        pub fn selected(&self) -> Option<usize> {
                self.selected
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
                let id = self.ui.find((self.selected? + 1) as u8)?;
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
