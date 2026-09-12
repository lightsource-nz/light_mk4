//! The device-UI preview and the design it edits.
//!
//! A real light-ui tree, materialised from a loaded design and rendered through the same rasteriser
//! and `Ui::render` path the panels use, into an off-screen RGB565 buffer. The editor composites it
//! into the stage. The preview owns the editable [`Design`]: it can be interacted with (Run mode:
//! taps navigate) or edited (Edit mode: a tap selects a widget, and structural operations mutate
//! the model, re-materialise the tree with `Ui::reload`, and save the JSON back to disk).

use std::path::PathBuf;

use light_core::hal::Clock;
use light_display::{Display, DisplayDriver, Frame, FrameLayer, Region};
use light_draw::PixelFormat;
use light_host_gui::now_us;
use light_ui::{Fonts, Page, Rect, Style, Theme, Touch, Ui};

use crate::design::{self, ChildDef, Design, DesignEvent};
use crate::font;

/// The previewed device screen, in pixels. A portrait panel; a stand-in until the design carries
/// its own target size.
pub const DEV_W: u16 = 240;
pub const DEV_H: u16 = 400;

/// The preview font's pixel size.
const PIXEL_SIZE: u16 = 18;

/// Widget arena capacity per page. Generous; a page with more widgets than this fails to build
/// (and is left as it was) rather than drawing half a tree.
const UI_WIDGETS: usize = 32;

/// The look, loaded from the framework's real steel theme -- the same JSON the firmware compiles.
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
        ui: Ui<DesignEvent, UI_WIDGETS>,
        display: Display<'static, NullDriver>,
        layer: FrameLayer,
        theme: Theme,
        font: light_font::Font<'static>,
        /// The editable model; the source of truth, saved back to [`path`](Self::path).
        design: Design,
        /// The materialised pages, regenerated whenever the model changes.
        pages: Vec<&'static Page<DesignEvent>>,
        /// The visited-page stack; its last entry is the page shown. Run-mode `back` pops it, an
        /// Edit-mode page switch resets it.
        history: Vec<usize>,
        /// The selected child's index in the current page (Edit mode).
        selected: Option<usize>,
        /// Where the design is loaded from and saved to.
        path: PathBuf,
}

impl Preview {
        pub fn new() -> Self {
                let path = save_path();
                //   a saved file beside the exe wins; otherwise the bundled default. A corrupt
                // saved file falls back rather than refusing to open
                let json = std::fs::read_to_string(&path).ok();
                let design = json
                        .as_deref()
                        .and_then(|j| design::parse(j).ok())
                        .unwrap_or_else(|| design::parse(DEFAULT_DESIGN_JSON).expect("the bundled design parses"));

                let font = font::load(PIXEL_SIZE);
                //   compile the theme JSON to an LTH blob with crush-core (the firmware's path) and
                // parse it, rather than mirroring the theme schema here
                let lth = crush_core::theme::compile_flat(THEME_JSON).expect("the bundled steel theme compiles");
                let theme = Theme::parse(&lth).expect("the compiled theme parses");
                let buf: &'static mut [u8] = Vec::leak(vec![0u8; PixelFormat::Rgb565.buffer_len(DEV_W, DEV_H)]);
                let display = Display::new(NullDriver, buf, DEV_W, DEV_H, PixelFormat::Rgb565, now_us);
                let mut layer = FrameLayer::new(DEV_W, DEV_H, PixelFormat::Rgb565);
                layer.bg = theme.bg;
                let mut ui = Ui::new();
                ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
                ui.fit(&layer);

                let pages = design::materialize(&design);
                let root = design.root.min(pages.len().saturating_sub(1));
                let mut history = vec![root];
                if let Some(&page) = pages.get(root) {
                        let _ = ui.reload(page);
                } else {
                        history.clear();
                }
                Self { ui, display, layer, theme, font, design, pages, history, selected: None, path }
        }

        /// Render a frame into the off-screen buffer if anything changed. Returns `true` while an
        /// animation is in flight, so the caller keeps redrawing.
        pub fn render(&mut self, now_us: u64) -> bool {
                let style = Style::new(self.theme, Fonts::uniform(&self.font));
                let drew = self.ui.render(&mut self.layer, &mut self.display, &style, now_us);
                while self.layer.poll(&mut self.display).unwrap_or(false) {}
                drew || self.ui.is_animating()
        }

        // --- Run mode: taps navigate --------------------------------------------------------

        /// Feed a touch that drives the UI as the device would: focus, flash, and navigation.
        pub fn interact(&mut self, x: u16, y: u16, touching: bool, now_us: u64) {
                if let Touch::Tap { emitted: Some(event), .. } = self.ui.touch(x, y, touching, now_us) {
                        match event {
                                DesignEvent::Goto(idx) => self.run_goto(idx as usize),
                                DesignEvent::Back => self.run_back(),
                        }
                }
        }

        fn run_goto(&mut self, idx: usize) {
                if let Some(&page) = self.pages.get(idx) {
                        if self.ui.navigate(page).is_ok() {
                                self.history.push(idx);
                                self.selected = None;
                        }
                }
        }

        fn run_back(&mut self) {
                if self.history.len() > 1 {
                        self.history.pop();
                        let idx = *self.history.last().expect("history is non-empty");
                        //   the mirror of the forward slide, so back retraces the way in
                        let _ = self.ui.navigate_back_to(self.pages[idx]);
                        self.selected = None;
                }
        }

        /// Start a fresh run from the root page -- the run session owns navigation from here.
        pub fn start_run(&mut self) {
                let root = self.design.root.min(self.pages.len().saturating_sub(1));
                if let Some(&page) = self.pages.get(root) {
                        let _ = self.ui.reload(page);
                        self.history = vec![root];
                }
                self.selected = None;
        }

        // --- Edit mode: selection and structural edits --------------------------------------

        /// Select the widget at a point in the device's own pixel space, or clear the selection if
        /// none is there. Hit-tests the current page's children by their materialised tags.
        pub fn select_at(&mut self, x: i32, y: i32) {
                let cur = self.current();
                let count = self.design.pages.get(cur).map_or(0, |p| p.children.len());
                self.selected = None;
                for i in 0..count {
                        //   children are tagged from 1 (tag 0 is light-ui's "untagged")
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

        /// Show a page for editing: an instant rebuild (no transition), selection cleared.
        pub fn show_page(&mut self, idx: usize) {
                if let Some(&page) = self.pages.get(idx) {
                        let _ = self.ui.reload(page);
                        self.history = vec![idx];
                        self.selected = None;
                }
        }

        /// Append a fresh button to the current page and select it.
        pub fn add_button(&mut self) {
                let cur = self.current();
                if let Some(page) = self.design.pages.get_mut(cur) {
                        page.children.push(ChildDef::new_button());
                        self.selected = Some(page.children.len() - 1);
                }
                self.rebuild();
        }

        /// Delete the selected widget.
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
                self.rebuild();
        }

        /// Move the selected widget by `delta` places within its page (clamped).
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
                self.rebuild();
        }

        /// Set the selected widget's text (its button or label string).
        pub fn set_selected_text(&mut self, s: &str) {
                if let Some(c) = self.selected_child_mut() {
                        if c.button.is_some() {
                                c.button = Some(s.to_owned());
                        } else if c.label.is_some() {
                                c.label = Some(s.to_owned());
                        }
                }
                self.rebuild();
        }

        /// Cycle the selected button's action: none -> back -> goto(0) -> ... -> goto(last) -> none.
        /// A no-op on a label.
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
                self.rebuild();
        }

        fn selected_child(&self) -> Option<&ChildDef> {
                self.design.pages.get(self.current())?.children.get(self.selected?)
        }

        fn selected_child_mut(&mut self) -> Option<&mut ChildDef> {
                let cur = self.current();
                let sel = self.selected?;
                self.design.pages.get_mut(cur)?.children.get_mut(sel)
        }

        /// The selected widget's text, for the inspector's editable field.
        pub fn selected_text(&self) -> Option<String> {
                let c = self.selected_child()?;
                c.button.clone().or_else(|| c.label.clone())
        }

        /// Whether the selected widget is a button (so an action applies).
        pub fn selected_is_button(&self) -> bool {
                self.selected_child().is_some_and(|c| c.button.is_some())
        }

        /// A label for the selected button's action ("none" / "back" / "goto <page>"), or `None`
        /// for a label widget or no selection.
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

        /// Re-materialise after a model change, reload the current page in place, and save.
        fn rebuild(&mut self) {
                self.pages = design::materialize(&self.design);
                let cur = self.current().min(self.pages.len().saturating_sub(1));
                if let Some(&page) = self.pages.get(cur) {
                        let _ = self.ui.reload(page);
                }
                self.save();
        }

        fn save(&self) {
                if let Err(e) = std::fs::write(&self.path, design::to_json(&self.design)) {
                        eprintln!("light-ui-editor: could not save '{}': {e}", self.path.display());
                }
        }

        // --- accessors for the editor chrome ------------------------------------------------

        fn current(&self) -> usize {
                *self.history.last().unwrap_or(&0)
        }

        pub fn page_count(&self) -> usize {
                self.design.pages.len()
        }

        pub fn page_title(&self, idx: usize) -> &str {
                self.design.pages.get(idx).map_or("", |p| p.title.as_str())
        }

        /// The page currently shown (its index).
        pub fn current_page(&self) -> usize {
                self.current()
        }

        pub fn selected(&self) -> Option<usize> {
                self.selected
        }

        /// A short description of the selected widget, for the inspector.
        pub fn selected_describe(&self) -> Option<String> {
                let sel = self.selected?;
                self.design.pages.get(self.current())?.children.get(sel).map(ChildDef::describe)
        }

        /// The selected widget's rectangle in device pixels, for the selection outline.
        pub fn selected_rect(&self) -> Option<Rect> {
                let id = self.ui.find((self.selected? + 1) as u8)?;
                self.ui.get(id).map(|w| w.rect)
        }

        /// The device screen size.
        pub fn size(&self) -> (u16, u16) {
                (DEV_W, DEV_H)
        }

        /// The rendered RGB565 image, `DEV_W * DEV_H` pixels big-endian, or empty if unavailable.
        pub fn pixels(&self) -> &[u8] {
                self.display.front().unwrap_or(&[])
        }
}

/// The design's load/save path: `design.json` beside the executable, so a save is discoverable and
/// does not depend on the working directory.
fn save_path() -> PathBuf {
        std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("design.json")))
                .unwrap_or_else(|| PathBuf::from("design.json"))
}
