//   a GUI binary: no console window when launched from Explorer or a shortcut. Debug builds keep
// the console so panics and logs are visible while developing; release builds are clean.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! A prototype desktop editor for light-ui embedded UIs.
//!
//! The window lays out the editor chrome -- a top bar with an Edit/Run toggle, a page list on the
//! left, an inspector on the right, and a central stage holding the device preview -- drawn through
//! the framework's own `light-draw` canvas. The stage previews a design loaded from data
//! (`design.json`); in Run mode taps drive it as the device would, and in Edit mode a tap selects a
//! widget and the inspector's buttons edit the design, saving the JSON back to disk.

use light_draw::{Canvas, Point};
use light_font::Font;
use light_host_gui::{HostApp, HostFrame, PointerEvent, PointerPhase};

mod design;
mod font;
mod preview;
mod theme;

use preview::Preview;

/// Pack 8-bit RGB into RGB565.
const fn rgb(r: u8, g: u8, b: u8) -> u16 {
        (((r as u16) >> 3) << 11) | (((g as u16) >> 2) << 5) | ((b as u16) >> 3)
}

// The editor's palette.
const DESK: u16 = rgb(0x1E, 0x22, 0x28); // window background behind the panels
const PANEL: u16 = rgb(0x26, 0x2C, 0x36); // side panels
const BAR: u16 = rgb(0x2E, 0x36, 0x42); // top bar
const STAGE: u16 = rgb(0x0E, 0x10, 0x13); // the preview stage, near-black
const LINE: u16 = rgb(0x3A, 0x42, 0x50); // dividers
const ACCENT: u16 = rgb(0x4C, 0x9A, 0xE0); // blue: active toggle, current page, selection
const CHIP: u16 = rgb(0x33, 0x3B, 0x47); // inactive chips / buttons
const TEXT: u16 = rgb(0xC8, 0xD0, 0xD8); // chrome text
const DIM: u16 = rgb(0x7A, 0x86, 0x94); // secondary chrome text
const BEZEL: u16 = rgb(0x05, 0x06, 0x08); // the preview device's body

/// The fixed editor canvas.
const CANVAS_W: u16 = 900;
const CANVAS_H: u16 = 560;
const W: i32 = CANVAS_W as i32;
const H: i32 = CANVAS_H as i32;

const BAR_H: i32 = 34;
const LEFT_W: i32 = 190;
const RIGHT_W: i32 = 230;

/// The device screen the stage previews; its size is the preview's own.
const DEV_W: i32 = preview::DEV_W as i32;
const DEV_H: i32 = preview::DEV_H as i32;

/// The chrome font's pixel size.
const CHROME_PX: u16 = 14;

/// An inclusive chrome rectangle: `(x0, y0, x1, y1)`.
type R = (i32, i32, i32, i32);

fn hit(ev: &PointerEvent, r: R) -> bool {
        ev.x >= r.0 && ev.x <= r.2 && ev.y >= r.1 && ev.y <= r.3
}

// --- fixed chrome geometry (canvas coordinates) --------------------------------------------

const fn run_toggle() -> R {
        (12, 7, 70, BAR_H - 8)
}
const fn edit_toggle() -> R {
        (78, 7, 136, BAR_H - 8)
}
/// The left-panel row for page `i`.
fn page_row(i: usize) -> R {
        let y0 = BAR_H + 28 + i as i32 * 32;
        (14, y0, LEFT_W - 14, y0 + 26)
}
/// The inspector's `n`th operation button (Move Up, Move Down, Delete, Add).
fn insp_button(n: i32) -> R {
        let y0 = BAR_H + 96 + n * 38;
        (W - RIGHT_W + 14, y0, W - 14, y0 + 30)
}
const INSP_LABELS: [&str; 4] = ["Move Up", "Move Down", "Delete", "Add Button"];

/// The device preview's top-left in canvas coordinates -- centred in the stage column. Shared by
/// the render (where the preview is composited and the selection outlined) and the pointer mapping.
const fn dev_x0() -> i32 {
        let stage_x0 = LEFT_W + 1;
        let stage_x1 = W - RIGHT_W - 2;
        stage_x0 + ((stage_x1 - stage_x0) - DEV_W) / 2
}
const fn dev_y0() -> i32 {
        BAR_H + ((H - BAR_H) - DEV_H) / 2
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
        Edit,
        Run,
}

struct Editor {
        preview: Preview,
        font: Font<'static>,
        mode: Mode,
        /// Whether a press began inside the stage, so a drag/release routes there.
        stage_press: bool,
}

/// Draw `s` at `(x, y)` in `color`.
fn text(c: &mut Canvas<'_>, font: &Font<'_>, x: i32, y: i32, color: u16, s: &str) {
        c.fg = color;
        c.text(font, Point::new(x, y), s);
}

/// Fill a chrome rectangle.
fn fill(c: &mut Canvas<'_>, r: R, color: u16) {
        c.fg = color;
        c.rect(Point::new(r.0, r.1), Point::new(r.2, r.3), true);
}

impl HostApp for Editor {
        fn title(&self) -> &str {
                "Light UI Editor"
        }

        fn canvas_size(&self) -> (u16, u16) {
                (CANVAS_W, CANVAS_H)
        }

        fn background(&self) -> u16 {
                DESK
        }

        fn on_pointer(&mut self, ev: PointerEvent) {
                let now = light_host_gui::now_us();
                match ev.phase {
                        PointerPhase::Pressed => {
                                // chrome first: toggles, page list, inspector ops
                                if hit(&ev, run_toggle()) {
                                        //   entering Run starts a fresh run from the root; the run
                                        // session, not the page list, owns navigation from here
                                        if self.mode != Mode::Run {
                                                self.mode = Mode::Run;
                                                self.preview.start_run();
                                        }
                                        return;
                                }
                                if hit(&ev, edit_toggle()) {
                                        self.mode = Mode::Edit;
                                        return;
                                }
                                //   the page list is an Edit control; in Run the run owns the page
                                if self.mode == Mode::Edit {
                                        for i in 0..self.preview.page_count() {
                                                if hit(&ev, page_row(i)) {
                                                        self.preview.show_page(i);
                                                        return;
                                                }
                                        }
                                }
                                if self.mode == Mode::Edit {
                                        if hit(&ev, insp_button(0)) {
                                                self.preview.move_selected(-1);
                                                return;
                                        }
                                        if hit(&ev, insp_button(1)) {
                                                self.preview.move_selected(1);
                                                return;
                                        }
                                        if hit(&ev, insp_button(2)) {
                                                self.preview.delete_selected();
                                                return;
                                        }
                                        if hit(&ev, insp_button(3)) {
                                                self.preview.add_button();
                                                return;
                                        }
                                }
                                // the stage
                                let (px, py) = (ev.x - dev_x0(), ev.y - dev_y0());
                                if px >= 0 && py >= 0 && px < DEV_W && py < DEV_H {
                                        self.stage_press = true;
                                        match self.mode {
                                                Mode::Run => self.preview.interact(px as u16, py as u16, true, now),
                                                Mode::Edit => self.preview.select_at(px, py),
                                        }
                                }
                        }
                        PointerPhase::Moved if self.stage_press && self.mode == Mode::Run => {
                                let (dw, dh) = self.preview.size();
                                let cx = (ev.x - dev_x0()).clamp(0, i32::from(dw) - 1) as u16;
                                let cy = (ev.y - dev_y0()).clamp(0, i32::from(dh) - 1) as u16;
                                self.preview.interact(cx, cy, true, now);
                        }
                        PointerPhase::Released if self.stage_press => {
                                self.stage_press = false;
                                if self.mode == Mode::Run {
                                        let (dw, dh) = self.preview.size();
                                        let cx = (ev.x - dev_x0()).clamp(0, i32::from(dw) - 1) as u16;
                                        let cy = (ev.y - dev_y0()).clamp(0, i32::from(dh) - 1) as u16;
                                        self.preview.interact(cx, cy, false, now);
                                }
                        }
                        _ => {}
                }
        }

        fn render(&mut self, frame: &mut HostFrame<'_>) -> bool {
                let animating = self.preview.render(frame.now_us);
                frame.layer.invalidate_all();
                let Some(mut c) = frame.layer.frame_begin(frame.display, frame.now_us) else {
                        return animating;
                };

                // panels over the stage ground, then the bar over both
                fill(&mut c, (0, 0, W - 1, H - 1), STAGE);
                fill(&mut c, (0, BAR_H, LEFT_W - 1, H - 1), PANEL);
                fill(&mut c, (W - RIGHT_W, BAR_H, W - 1, H - 1), PANEL);
                fill(&mut c, (0, 0, W - 1, BAR_H - 1), BAR);
                c.fg = LINE;
                c.line(Point::new(0, BAR_H), Point::new(W - 1, BAR_H));
                c.line(Point::new(LEFT_W, BAR_H), Point::new(LEFT_W, H - 1));
                c.line(Point::new(W - RIGHT_W - 1, BAR_H), Point::new(W - RIGHT_W - 1, H - 1));

                // top bar: the Edit / Run toggle
                let (run_on, edit_on) = (self.mode == Mode::Run, self.mode == Mode::Edit);
                fill(&mut c, run_toggle(), if run_on { ACCENT } else { CHIP });
                fill(&mut c, edit_toggle(), if edit_on { ACCENT } else { CHIP });
                text(&mut c, &self.font, run_toggle().0 + 14, 11, TEXT, "Run");
                text(&mut c, &self.font, edit_toggle().0 + 12, 11, TEXT, "Edit");

                // left panel: the page list -- interactive in Edit, a dimmed run-position indicator
                // in Run
                let editing = self.mode == Mode::Edit;
                text(&mut c, &self.font, 14, BAR_H + 8, DIM, if editing { "PAGES" } else { "PAGES (run)" });
                for i in 0..self.preview.page_count() {
                        let r = page_row(i);
                        let current = i == self.preview.current_page();
                        let bg = match (editing, current) {
                                (true, true) => ACCENT,
                                (true, false) => CHIP,
                                (false, true) => rgb(0x2A, 0x3A, 0x4C),
                                (false, false) => rgb(0x20, 0x25, 0x2D),
                        };
                        fill(&mut c, r, bg);
                        text(&mut c, &self.font, r.0 + 8, r.1 + 7, if editing { TEXT } else { DIM }, self.preview.page_title(i));
                }

                // right panel: the inspector
                let rx = W - RIGHT_W + 14;
                text(&mut c, &self.font, rx, BAR_H + 8, DIM, "INSPECTOR");
                match self.preview.selected_describe() {
                        Some(d) => text(&mut c, &self.font, rx, BAR_H + 34, TEXT, &d),
                        None => text(&mut c, &self.font, rx, BAR_H + 34, DIM, "no selection"),
                }
                let ops_live = self.mode == Mode::Edit;
                for (n, label) in INSP_LABELS.iter().enumerate() {
                        let r = insp_button(n as i32);
                        //   Add is always available in Edit; the others need a selection
                        let enabled = ops_live && (n == 3 || self.preview.selected().is_some());
                        fill(&mut c, r, if enabled { CHIP } else { rgb(0x20, 0x25, 0x2D) });
                        text(&mut c, &self.font, r.0 + 10, r.1 + 9, if enabled { TEXT } else { DIM }, label);
                }

                // the stage: device bezel, the composited preview, and the selection outline
                let (dx0, dy0) = (dev_x0(), dev_y0());
                let (dx1, dy1) = (dx0 + DEV_W - 1, dy0 + DEV_H - 1);
                c.fg = BEZEL;
                c.rect_rounded(Point::new(dx0 - 8, dy0 - 8), Point::new(dx1 + 8, dy1 + 8), 14, light_draw::corner::ALL, true);
                let px = self.preview.pixels();
                if px.len() >= (DEV_W * DEV_H * 2) as usize {
                        for cy in 0..DEV_H {
                                for cx in 0..DEV_W {
                                        let i = ((cy * DEV_W + cx) as usize) * 2;
                                        let color = u16::from_be_bytes([px[i], px[i + 1]]);
                                        c.set(dx0 + cx, dy0 + cy, color);
                                }
                        }
                }
                if self.mode == Mode::Edit {
                        if let Some(r) = self.preview.selected_rect() {
                                c.fg = ACCENT;
                                //   a two-pixel outline so it reads over any widget colour
                                c.rect(Point::new(dx0 + r.x0, dy0 + r.y0), Point::new(dx0 + r.x1, dy0 + r.y1), false);
                                c.rect(Point::new(dx0 + r.x0 - 1, dy0 + r.y0 - 1), Point::new(dx0 + r.x1 + 1, dy0 + r.y1 + 1), false);
                        }
                }

                drop(c);
                frame.layer.frame_end(frame.display);
                animating
        }
}

fn main() {
        let editor = Editor { preview: Preview::new(), font: font::load(CHROME_PX), mode: Mode::Edit, stage_press: false };
        if let Err(e) = light_host_gui::run(editor) {
                eprintln!("light-ui-editor: {e}");
                std::process::exit(1);
        }
}
