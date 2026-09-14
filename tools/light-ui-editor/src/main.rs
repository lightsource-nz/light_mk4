//   a GUI binary: no console window when launched from Explorer or a shortcut. Debug builds keep
// the console so panics and logs are visible while developing; release builds are clean.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! A prototype desktop editor for light-ui embedded UIs.
//!
//! The chrome -- an Edit/Run toggle, a page list, and an inspector of real controls (text fields,
//! combo boxes, checkboxes, number spinners) -- is egui. The device PREVIEW in the centre is still
//! rendered by light-ui/light-draw into a pixel buffer (the `DisplayDriver` seam, in [`preview`])
//! and shown as an egui image, so it stays pixel-faithful to the firmware while the surrounding UI
//! gets proper OS-grade controls. Edits mutate the `Design`, which recompiles to an LUI blob and
//! saves the JSON, exactly as before.

mod design;
mod font;
mod preview;

use light_host_gui::{eframe, egui, now_us};
use preview::{Preview, Sel};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
        Edit,
        Run,
        Actions,
        Theme,
}

/// The editable theme colours: `(key, label)`, matching the preview's theme keys.
const COLOR_KEYS: &[(&str, &str)] = &[
        ("bg", "Background"),
        ("bar", "Title bar"),
        ("frame", "Frame / border"),
        ("title", "Title text"),
        ("text", "Label text"),
        ("button_outline", "Button outline"),
        ("button_text", "Button text"),
        ("focus_text", "Focus text"),
        ("indicator", "Indicator"),
];

fn rgb565_to_color32(c: u16) -> egui::Color32 {
        let r = ((c >> 11) & 0x1F) as u8;
        let g = ((c >> 5) & 0x3F) as u8;
        let b = (c & 0x1F) as u8;
        egui::Color32::from_rgb((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2))
}

fn color32_to_rgb565(c: egui::Color32) -> u16 {
        ((u16::from(c.r()) >> 3) << 11) | ((u16::from(c.g()) >> 2) << 5) | (u16::from(c.b()) >> 3)
}

struct EditorApp {
        preview: Preview,
        mode: Mode,
        tex: Option<egui::TextureHandle>,
        //   text-field buffers, resynced when the selection or page changes so a control edits the
        // right value without recompiling on every keystroke (committed on focus loss)
        label_buf: String,
        title_buf: String,
        last_sel: Option<Sel>,
        last_page: usize,
        //   one edit buffer per action name (committed on focus loss), kept in step with the action
        // count so the Actions view edits names without recompiling on every keystroke
        action_bufs: Vec<String>,
}

impl EditorApp {
        fn new(preview: Preview) -> Self {
                let title_buf = preview.page_title(preview.current_page()).to_owned();
                Self { preview, mode: Mode::Edit, tex: None, label_buf: String::new(), title_buf, last_sel: None, last_page: 0, action_bufs: Vec::new() }
        }

        /// Keep the text buffers in step with the current selection and page.
        fn sync_buffers(&mut self) {
                if self.preview.selected() != self.last_sel {
                        self.last_sel = self.preview.selected();
                        self.label_buf = self.preview.selected_text().unwrap_or_default();
                }
                if self.preview.current_page() != self.last_page {
                        self.last_page = self.preview.current_page();
                        self.title_buf = self.preview.page_title(self.last_page).to_owned();
                }
        }

        /// Upload the freshly rendered preview buffer as a texture (RGB565 -> RGBA via light-host-gui).
        fn upload_preview(&mut self, ctx: &egui::Context) {
                let (w, h) = self.preview.size();
                let img = light_host_gui::rgb565_color_image(w as usize, h as usize, self.preview.pixels());
                match &mut self.tex {
                        Some(t) => t.set(img, egui::TextureOptions::NEAREST),
                        None => self.tex = Some(ctx.load_texture("preview", img, egui::TextureOptions::NEAREST)),
                }
        }
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x4C, 0x9A, 0xE0);

impl eframe::App for EditorApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
                self.sync_buffers();
                let animating = self.preview.render(now_us());
                self.upload_preview(ctx);

                let compile_err = self.preview.compile_error().map(str::to_owned);
                egui::TopBottomPanel::top("bar").show(ctx, |ui| {
                        ui.horizontal(|ui| {
                                ui.selectable_value(&mut self.mode, Mode::Edit, "Edit");
                                if ui.selectable_value(&mut self.mode, Mode::Run, "Run").clicked() {
                                        self.preview.start_run();
                                }
                                ui.selectable_value(&mut self.mode, Mode::Actions, "Actions");
                                ui.selectable_value(&mut self.mode, Mode::Theme, "Theme");
                                ui.separator();
                                let (dw, dh) = self.preview.size();
                                ui.label(format!("{dw}x{dh}  {}", if self.preview.is_landscape() { "landscape" } else { "portrait" }));
                        });
                        if let Some(err) = &compile_err {
                                ui.colored_label(egui::Color32::from_rgb(0xE0, 0x60, 0x50), format!("⚠ does not compile: {err}  — file not saved"));
                        }
                });

                egui::SidePanel::left("pages").default_width(180.0).show(ctx, |ui| {
                        self.pages_panel(ui);
                });
                egui::SidePanel::right("inspector").default_width(240.0).show(ctx, |ui| {
                        self.inspector_panel(ui);
                });
                egui::CentralPanel::default().show(ctx, |ui| {
                        self.stage(ui);
                });

                if animating {
                        ctx.request_repaint();
                }
        }
}

impl EditorApp {
        fn pages_panel(&mut self, ui: &mut egui::Ui) {
                let edit = self.mode == Mode::Edit;
                //   the page list navigates in Edit and Theme (to preview the look on any page); in
                // Run the run session owns the page
                let can_nav = self.mode != Mode::Run;
                ui.add_space(4.0);
                ui.label(egui::RichText::new(if can_nav { "PAGES" } else { "PAGES (run)" }).weak());
                let cur = self.preview.current_page();
                for i in 0..self.preview.page_count() {
                        let title = self.preview.page_title(i).to_owned();
                        if ui.selectable_label(i == cur, format!("{i}  {title}")).clicked() && can_nav {
                                self.preview.show_page(i);
                        }
                }
                if edit {
                        ui.add_space(4.0);
                        if ui.button("+ Add page").clicked() {
                                self.preview.add_page();
                        }
                        ui.separator();
                        ui.label(egui::RichText::new("PAGE TITLE").weak());
                        if ui.text_edit_singleline(&mut self.title_buf).lost_focus() {
                                self.preview.set_page_title(&self.title_buf);
                        }
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new("WIDGETS").weak());
                        //   the current page's tree; selecting here beats hunting the tiny preview
                        let selected = self.preview.selected();
                        for (sel, indent, label) in self.preview.outline() {
                                let text = format!("{}{}", "    ".repeat(indent as usize), label);
                                if ui.selectable_label(Some(sel) == selected, text).clicked() {
                                        self.preview.select(Some(sel));
                                }
                        }
                }
        }

        fn inspector_panel(&mut self, ui: &mut egui::Ui) {
                if self.mode == Mode::Theme {
                        self.theme_panel(ui);
                        return;
                }
                if self.mode == Mode::Actions {
                        self.actions_panel(ui);
                        return;
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new("INSPECTOR").weak());
                if self.mode != Mode::Edit {
                        ui.label("run mode");
                        return;
                }
                if self.preview.selected().is_none() {
                        ui.label("no selection");
                } else {
                        //   snapshot the state, then render controls that mutate through setters --
                        // avoids borrowing the preview while a control also reads it
                        let describe = self.preview.selected_describe().unwrap_or_default();
                        let is_frame = self.preview.selected_is_frame();
                        let is_button = self.preview.selected_is_button();
                        let grow = self.preview.selected_grow();
                        let (min_w, min_h) = self.preview.selected_min().unwrap_or((0, 0));
                        let (max_w, max_h) = self.preview.selected_max().unwrap_or((0, 0));
                        let layout = self.preview.selected_layout_label();
                        let scroll = self.preview.selected_scroll_label();
                        let action = self.preview.selected_action();
                        let event = self.preview.selected_event().unwrap_or(0);
                        let has_actions = self.preview.has_actions();
                        let action_ref = self.preview.selected_action_ref();
                        let action_names = self.preview.action_names();
                        let pages = self.preview.page_count();
                        let page_titles: Vec<String> = (0..pages).map(|p| self.preview.page_title(p).to_owned()).collect();

                        ui.label(describe);
                        ui.separator();

                        if !is_frame {
                                ui.label("Text");
                                if ui.text_edit_singleline(&mut self.label_buf).lost_focus() {
                                        self.preview.set_selected_text(&self.label_buf);
                                }
                        }

                        if is_button {
                                if has_actions {
                                        //   the design declares its app's actions (event + navigation,
                                        // mirroring the firmware); a button just names one, so it
                                        // behaves right in the preview and on device alike
                                        let cur = action_ref.clone().unwrap_or_else(|| "none".to_owned());
                                        egui::ComboBox::from_label("Action").selected_text(&cur).show_ui(ui, |ui| {
                                                if ui.selectable_label(action_ref.is_none(), "none").clicked() {
                                                        self.preview.set_selected_action_ref(None);
                                                }
                                                for name in &action_names {
                                                        if ui.selectable_label(action_ref.as_deref() == Some(name), name).clicked() {
                                                                self.preview.set_selected_action_ref(Some(name));
                                                        }
                                                }
                                        });
                                } else {
                                        //   no action registry: edit the raw navigation and event id
                                        if let Some((goto, back)) = action {
                                                let cur = if let Some(g) = goto {
                                                        format!("goto {}", page_titles.get(g).map_or("?", |s| s.as_str()))
                                                } else if back {
                                                        "back".to_owned()
                                                } else {
                                                        "none".to_owned()
                                                };
                                                egui::ComboBox::from_label("Nav").selected_text(cur).show_ui(ui, |ui| {
                                                        if ui.selectable_label(goto.is_none() && !back, "none").clicked() {
                                                                self.preview.set_selected_action(None, false);
                                                        }
                                                        if ui.selectable_label(back, "back").clicked() {
                                                                self.preview.set_selected_action(None, true);
                                                        }
                                                        for (p, title) in page_titles.iter().enumerate() {
                                                                if ui.selectable_label(goto == Some(p), format!("goto {title}")).clicked() {
                                                                        self.preview.set_selected_action(Some(p), false);
                                                                }
                                                        }
                                                });
                                        }
                                        ui.horizontal(|ui| {
                                                let mut ev = event;
                                                if ui.add(egui::DragValue::new(&mut ev).range(0..=u16::MAX).prefix("event ")).changed() {
                                                        self.preview.set_selected_event(ev);
                                                }
                                                ui.label("(0 = none)");
                                        });
                                }
                        }

                        if is_frame {
                                let cur = layout.unwrap_or_else(|| "stack".to_owned());
                                egui::ComboBox::from_label("Layout").selected_text(&cur).show_ui(ui, |ui| {
                                        for opt in ["stack", "row", "linear"] {
                                                if ui.selectable_label(cur == opt, opt).clicked() {
                                                        self.preview.set_selected_layout(opt);
                                                }
                                        }
                                });
                                let cur = scroll.unwrap_or_else(|| "none".to_owned());
                                egui::ComboBox::from_label("Scroll").selected_text(&cur).show_ui(ui, |ui| {
                                        for opt in ["none", "vertical", "horizontal"] {
                                                if ui.selectable_label(cur == opt, opt).clicked() {
                                                        self.preview.set_selected_scroll(if opt == "none" { None } else { Some(opt) });
                                                }
                                        }
                                });
                        }

                        let mut g = grow;
                        if ui.checkbox(&mut g, "Grow to fill").changed() {
                                self.preview.set_selected_grow(g);
                        }

                        ui.separator();
                        ui.label("Min size (0 = auto)");
                        let (mut mw, mut mh) = (min_w, min_h);
                        ui.horizontal(|ui| {
                                let c = ui.add(egui::DragValue::new(&mut mw).range(0..=2000).prefix("w ")).changed();
                                let c2 = ui.add(egui::DragValue::new(&mut mh).range(0..=2000).prefix("h ")).changed();
                                if c || c2 {
                                        self.preview.set_selected_min(mw, mh);
                                }
                        });
                        ui.label("Max size (0 = none)");
                        let (mut xw, mut xh) = (max_w, max_h);
                        ui.horizontal(|ui| {
                                let c = ui.add(egui::DragValue::new(&mut xw).range(0..=2000).prefix("w ")).changed();
                                let c2 = ui.add(egui::DragValue::new(&mut xh).range(0..=2000).prefix("h ")).changed();
                                if c || c2 {
                                        self.preview.set_selected_max(xw, xh);
                                }
                        });
                }

                ui.separator();
                ui.horizontal(|ui| {
                        if ui.button("+ Button").clicked() {
                                self.preview.add_button();
                        }
                        if ui.button("+ Frame").clicked() {
                                self.preview.add_frame();
                        }
                });
                let has_sel = self.preview.selected().is_some();
                ui.horizontal(|ui| {
                        if ui.add_enabled(has_sel, egui::Button::new("Up")).clicked() {
                                self.preview.move_selected(-1);
                        }
                        if ui.add_enabled(has_sel, egui::Button::new("Down")).clicked() {
                                self.preview.move_selected(1);
                        }
                        if ui.add_enabled(has_sel, egui::Button::new("Delete")).clicked() {
                                self.preview.delete_selected();
                        }
                });
        }

        fn theme_panel(&mut self, ui: &mut egui::Ui) {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("THEME").weak());
                match self.preview.theme_base() {
                        Some(base) => ui.label(format!("extends {base} — the pickers show the resolved look; edits are overrides")),
                        None => ui.label("flat theme (self-contained)"),
                };
                ui.separator();
                ui.label("Colours");
                for (key, label) in COLOR_KEYS {
                        ui.horizontal(|ui| {
                                let mut col = rgb565_to_color32(self.preview.theme_color(key));
                                if ui.color_edit_button_srgba(&mut col).changed() {
                                        self.preview.set_theme_color(key, color32_to_rgb565(col));
                                }
                                ui.label(*label);
                        });
                }
                ui.separator();
                ui.label("Surfaces (button gradients)");
                for (key, label) in [("button", "Button"), ("focus", "Focused")] {
                        let surface = self.preview.theme_surface(key);
                        ui.horizontal(|ui| {
                                let mut on = surface.is_some();
                                if ui.checkbox(&mut on, label).changed() {
                                        if on {
                                                //   enable with a flat gradient at the button outline, ready to spread
                                                let seed = self.preview.theme_color("button_outline");
                                                self.preview.set_theme_surface(key, seed, seed);
                                        } else {
                                                self.preview.clear_theme_surface(key);
                                        }
                                }
                                if let Some((from, to)) = surface {
                                        let (mut cf, mut ct) = (rgb565_to_color32(from), rgb565_to_color32(to));
                                        let c1 = ui.color_edit_button_srgba(&mut cf).changed();
                                        let c2 = ui.color_edit_button_srgba(&mut ct).changed();
                                        if c1 || c2 {
                                                self.preview.set_theme_surface(key, color32_to_rgb565(cf), color32_to_rgb565(ct));
                                        }
                                }
                        });
                }
                ui.separator();
                ui.label("Metrics");
                for (key, label, max) in [("radius", "Corner radius", 64u16), ("screen_radius", "Screen radius", 128u16)] {
                        ui.horizontal(|ui| {
                                let mut v = self.preview.theme_metric(key);
                                if ui.add(egui::DragValue::new(&mut v).range(0..=max)).changed() {
                                        self.preview.set_theme_metric(key, v);
                                }
                                ui.label(label);
                        });
                }
                ui.horizontal(|ui| {
                        let cur = self.preview.theme_descent_label();
                        egui::ComboBox::from_label("Page descent").selected_text(cur).show_ui(ui, |ui| {
                                if ui.selectable_label(cur == "none", "none").clicked() {
                                        self.preview.set_theme_descent(None);
                                }
                                for opt in ["top", "bottom", "left", "right"] {
                                        if ui.selectable_label(cur == opt, opt).clicked() {
                                                self.preview.set_theme_descent(Some(opt));
                                        }
                                }
                        });
                });
                ui.separator();
                ui.small("Saved to theme.json beside the design.");
        }

        fn actions_panel(&mut self, ui: &mut egui::Ui) {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("ACTIONS").weak());
                ui.small("The app's event + navigation mappings a button names. Defined once here; the firmware mirrors them.");
                ui.separator();

                let actions = self.preview.actions();
                let pages = self.preview.page_count();
                let page_titles: Vec<String> = (0..pages).map(|p| self.preview.page_title(p).to_owned()).collect();
                //   keep one name buffer per action so a rename commits on focus loss, not per keystroke
                if self.action_bufs.len() != actions.len() {
                        self.action_bufs = actions.iter().map(|a| a.name.clone()).collect();
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                        let mut to_remove: Option<usize> = None;
                        for (i, a) in actions.iter().enumerate() {
                                ui.group(|ui| {
                                        ui.horizontal(|ui| {
                                                ui.label("Name");
                                                if ui.text_edit_singleline(&mut self.action_bufs[i]).lost_focus() {
                                                        self.preview.set_action_name(i, &self.action_bufs[i].clone());
                                                        //   reflect the applied name (a blank or duplicate is rejected)
                                                        self.action_bufs[i] = self.preview.action_name(i).unwrap_or_default();
                                                }
                                        });
                                        ui.horizontal(|ui| {
                                                let mut ev = a.event;
                                                if ui.add(egui::DragValue::new(&mut ev).range(0..=u16::MAX).prefix("event ")).changed() {
                                                        self.preview.set_action_event(i, ev);
                                                }
                                                ui.label("(0 = none)");
                                        });
                                        ui.horizontal(|ui| {
                                                ui.label("Nav");
                                                let cur = if let Some(g) = a.goto {
                                                        format!("goto {}", page_titles.get(g).map_or("?", |s| s.as_str()))
                                                } else if a.back {
                                                        "back".to_owned()
                                                } else {
                                                        "none".to_owned()
                                                };
                                                egui::ComboBox::from_id_salt((i, "nav")).selected_text(cur).show_ui(ui, |ui| {
                                                        if ui.selectable_label(a.goto.is_none() && !a.back, "none").clicked() {
                                                                self.preview.set_action_nav(i, None, false);
                                                        }
                                                        if ui.selectable_label(a.back, "back").clicked() {
                                                                self.preview.set_action_nav(i, None, true);
                                                        }
                                                        for (p, title) in page_titles.iter().enumerate() {
                                                                if ui.selectable_label(a.goto == Some(p), format!("goto {title}")).clicked() {
                                                                        self.preview.set_action_nav(i, Some(p), false);
                                                                }
                                                        }
                                                });
                                        });
                                        ui.horizontal(|ui| {
                                                ui.label("Transition");
                                                let cur = a.transition.clone().unwrap_or_else(|| "default".to_owned());
                                                egui::ComboBox::from_id_salt((i, "trans")).selected_text(cur).show_ui(ui, |ui| {
                                                        if ui.selectable_label(a.transition.is_none(), "default").clicked() {
                                                                self.preview.set_action_transition(i, None);
                                                        }
                                                        for opt in ["top", "bottom", "left", "right"] {
                                                                if ui.selectable_label(a.transition.as_deref() == Some(opt), opt).clicked() {
                                                                        self.preview.set_action_transition(i, Some(opt));
                                                                }
                                                        }
                                                });
                                        });
                                        if ui.button("Delete action").clicked() {
                                                to_remove = Some(i);
                                        }
                                });
                                ui.add_space(4.0);
                        }
                        if let Some(i) = to_remove {
                                self.preview.remove_action(i);
                        }
                        if ui.button("+ Add action").clicked() {
                                self.preview.add_action();
                        }
                });
        }

        fn stage(&mut self, ui: &mut egui::Ui) {
                let Some(tex) = self.tex.clone() else { return };
                let (dw, dh) = self.preview.size();
                let (dw, dh) = (dw as f32, dh as f32);
                let area = ui.available_rect_before_wrap();
                let scale = (area.width() / dw).min(area.height() / dh).max(0.01);
                let size = egui::vec2(dw * scale, dh * scale);
                //   centre the device in the available area, and make the INTERACTION area the same
                // rect the image is painted into -- otherwise a click on the visible device lands on
                // an interaction area placed elsewhere and nothing responds
                let rect = egui::Rect::from_center_size(area.center(), size);
                let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());
                let painter = ui.painter_at(rect);
                painter.image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);

                let to_device = |p: egui::Pos2| -> (i32, i32) { (((p.x - rect.left()) / scale) as i32, ((p.y - rect.top()) / scale) as i32) };
                let now = now_us();
                match self.mode {
                        Mode::Run => {
                                if resp.is_pointer_button_down_on() {
                                        if let Some(p) = resp.interact_pointer_pos() {
                                                let (x, y) = to_device(p);
                                                self.preview.interact(x.max(0) as u16, y.max(0) as u16, true, now);
                                        }
                                } else if resp.clicked() || resp.drag_stopped() {
                                        let (x, y) = resp.interact_pointer_pos().map(to_device).unwrap_or((0, 0));
                                        self.preview.interact(x.max(0) as u16, y.max(0) as u16, false, now);
                                }
                        }
                        Mode::Edit => {
                                if resp.clicked() {
                                        if let Some(p) = resp.interact_pointer_pos() {
                                                let (x, y) = to_device(p);
                                                self.preview.select_at(x, y);
                                        }
                                }
                                //   the selection outline, device rect scaled into the stage
                                if let Some(r) = self.preview.selected_rect() {
                                        let sel = egui::Rect::from_min_max(
                                                egui::pos2(rect.left() + r.x0 as f32 * scale, rect.top() + r.y0 as f32 * scale),
                                                egui::pos2(rect.left() + (r.x1 + 1) as f32 * scale, rect.top() + (r.y1 + 1) as f32 * scale),
                                        );
                                        painter.rect_stroke(sel, 0.0, egui::Stroke::new(2.0, ACCENT));
                                }
                        }
                        //   Theme and Actions modes just show the design; the editing is in the panels
                        Mode::Theme | Mode::Actions => {}
                }
        }
}

fn main() -> eframe::Result {
        //   an optional design file to edit; without one, the editor finds a design in the current
        // working directory (see resolve_design_path)
        let arg = std::env::args().nth(1).map(std::path::PathBuf::from);
        let path = preview::resolve_design_path(arg);
        let app = EditorApp::new(Preview::new(path));
        light_host_gui::run("Light UI Editor", [980.0, 640.0], app)
}
