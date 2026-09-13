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
}

impl EditorApp {
        fn new(preview: Preview) -> Self {
                let title_buf = preview.page_title(preview.current_page()).to_owned();
                Self { preview, mode: Mode::Edit, tex: None, label_buf: String::new(), title_buf, last_sel: None, last_page: 0 }
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

                egui::TopBottomPanel::top("bar").show(ctx, |ui| {
                        ui.horizontal(|ui| {
                                ui.selectable_value(&mut self.mode, Mode::Edit, "Edit");
                                if ui.selectable_value(&mut self.mode, Mode::Run, "Run").clicked() {
                                        self.preview.start_run();
                                }
                                ui.separator();
                                let (dw, dh) = self.preview.size();
                                ui.label(format!("{dw}x{dh}  {}", if self.preview.is_landscape() { "landscape" } else { "portrait" }));
                        });
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
                ui.add_space(4.0);
                ui.label(egui::RichText::new(if edit { "PAGES" } else { "PAGES (run)" }).weak());
                let cur = self.preview.current_page();
                for i in 0..self.preview.page_count() {
                        let title = self.preview.page_title(i).to_owned();
                        //   the page list navigates in Edit; in Run the run session owns the page
                        if ui.selectable_label(i == cur, format!("{i}  {title}")).clicked() && edit {
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
                                if let Some((goto, back)) = action {
                                        let cur = if let Some(g) = goto {
                                                format!("goto {}", page_titles.get(g).map_or("?", |s| s.as_str()))
                                        } else if back {
                                                "back".to_owned()
                                        } else {
                                                "none".to_owned()
                                        };
                                        egui::ComboBox::from_label("Action").selected_text(cur).show_ui(ui, |ui| {
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

        fn stage(&mut self, ui: &mut egui::Ui) {
                let Some(tex) = self.tex.clone() else { return };
                let (dw, dh) = self.preview.size();
                let (dw, dh) = (dw as f32, dh as f32);
                let avail = ui.available_size();
                let scale = (avail.x / dw).min(avail.y / dh).max(0.01);
                let size = egui::vec2(dw * scale, dh * scale);
                //   centre the device in the stage
                let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
                //   allocate_exact_size lays out top-left; offset into the centre of what is available
                let offset = egui::vec2((avail.x - size.x).max(0.0) / 2.0, (avail.y - size.y).max(0.0) / 2.0);
                let rect = rect.translate(offset);
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
