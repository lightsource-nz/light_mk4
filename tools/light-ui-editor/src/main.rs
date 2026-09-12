//   a GUI binary: no console window when launched from Explorer or a shortcut. Debug builds keep
// the console so panics and logs are visible while developing; release builds are clean.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! A prototype desktop editor for light-ui embedded UIs.
//!
//! First step: the base frame. It opens a native window through [`light_host_gui`] and lays out
//! the editor's chrome -- a top bar, a component palette on the left, an inspector on the right,
//! and a central stage holding a device-screen preview -- drawn entirely through the framework's
//! own `light-draw` canvas, the same rasteriser the panels use. Nothing is interactive yet and no
//! `light-ui` tree is loaded into the stage; this establishes the window, the render pipeline and
//! the layout the later editing surfaces will fill in.

use light_draw::Point;
use light_host_gui::{HostApp, HostFrame, PointerEvent, PointerPhase};

mod font;
mod preview;

use preview::Preview;

/// Pack 8-bit RGB into RGB565 (the canvas's colour type).
const fn rgb(r: u8, g: u8, b: u8) -> u16 {
        (((r as u16) >> 3) << 11) | (((g as u16) >> 2) << 5) | ((b as u16) >> 3)
}

// The editor's palette: a cool, low-key desktop so the device preview on the stage is what draws
// the eye.
const DESK: u16 = rgb(0x1E, 0x22, 0x28); // window background behind the panels
const PANEL: u16 = rgb(0x26, 0x2C, 0x36); // side panels
const BAR: u16 = rgb(0x2E, 0x36, 0x42); // top bar
const STAGE: u16 = rgb(0x0E, 0x10, 0x13); // the preview stage, near-black
const LINE: u16 = rgb(0x3A, 0x42, 0x50); // dividers
const ACCENT: u16 = rgb(0x4C, 0x9A, 0xE0); // blue, for the active hints
const CHIP: u16 = rgb(0x33, 0x3B, 0x47); // placeholder rows/menu chips
const BEZEL: u16 = rgb(0x05, 0x06, 0x08); // the preview device's body

/// The fixed editor canvas. A device-resolution UI renders inside the stage; this is the whole
/// editor window, not the device.
const CANVAS_W: u16 = 900;
const CANVAS_H: u16 = 560;

const BAR_H: i32 = 34;
const LEFT_W: i32 = 190;
const RIGHT_W: i32 = 230;

/// The device screen the stage previews, portrait; its size is the preview's own.
const DEV_W: i32 = preview::DEV_W as i32;
const DEV_H: i32 = preview::DEV_H as i32;

/// The device preview's top-left in canvas coordinates -- centred in the stage column. Shared by
/// the render (where the preview is composited) and the pointer mapping (where a click is turned
/// into a device coordinate), so the two cannot disagree.
const fn dev_x0() -> i32 {
        let stage_x0 = LEFT_W + 1;
        let stage_x1 = CANVAS_W as i32 - RIGHT_W - 2;
        stage_x0 + ((stage_x1 - stage_x0) - DEV_W) / 2
}
const fn dev_y0() -> i32 {
        BAR_H + ((CANVAS_H as i32 - BAR_H) - DEV_H) / 2
}

struct Editor {
        preview: Preview,
        /// Whether the primary button is down inside the preview -- so a drag tracks and a release
        /// completes the touch that a press began.
        pressed: bool,
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

        fn render(&mut self, frame: &mut HostFrame<'_>) -> bool {
                //   render the device UI into its own off-screen buffer first; it is composited
                // into the stage below. `animating` keeps the shell redrawing through a flash or
                // page transition
                let animating = self.preview.render(frame.now_us);
                //   full repaint every frame: invalidate the whole canvas so the flush pushes it
                frame.layer.invalidate_all();
                let Some(mut c) = frame.layer.frame_begin(frame.display, frame.now_us) else {
                        return animating;
                };
                let w = i32::from(CANVAS_W);
                let h = i32::from(CANVAS_H);

                // filled: a rectangle helper closes over the canvas below via direct calls.
                let fill = |c: &mut light_draw::Canvas<'_>, x0: i32, y0: i32, x1: i32, y1: i32, color: u16| {
                        c.fg = color;
                        c.rect(Point::new(x0, y0), Point::new(x1, y1), true);
                };

                // the stage fills the centre column; panels and bar are drawn over its edges
                fill(&mut c, 0, 0, w - 1, h - 1, STAGE);

                // left palette panel
                fill(&mut c, 0, BAR_H, LEFT_W - 1, h - 1, PANEL);
                // right inspector panel
                fill(&mut c, w - RIGHT_W, BAR_H, w - 1, h - 1, PANEL);
                // top bar over both
                fill(&mut c, 0, 0, w - 1, BAR_H - 1, BAR);

                // dividers
                c.fg = LINE;
                c.line(Point::new(0, BAR_H), Point::new(w - 1, BAR_H));
                c.line(Point::new(LEFT_W, BAR_H), Point::new(LEFT_W, h - 1));
                c.line(Point::new(w - RIGHT_W - 1, BAR_H), Point::new(w - RIGHT_W - 1, h - 1));

                // top-bar menu hints: a few chips, the first accented as "active"
                let chip_y0 = 8;
                let chip_y1 = BAR_H - 9;
                for i in 0..4 {
                        let x0 = 12 + i * 74;
                        let x1 = x0 + 62;
                        fill(&mut c, x0, chip_y0, x1, chip_y1, if i == 0 { ACCENT } else { CHIP });
                }

                // palette: a column of placeholder component rows
                for i in 0..8 {
                        let y0 = BAR_H + 14 + i * 40;
                        fill(&mut c, 14, y0, LEFT_W - 14, y0 + 28, CHIP);
                }

                // inspector: placeholder property rows
                for i in 0..6 {
                        let y0 = BAR_H + 14 + i * 46;
                        fill(&mut c, w - RIGHT_W + 14, y0, w - 14, y0 + 34, CHIP);
                }

                // the device preview, centred in the stage column (the origin is a shared const so
                // the composite and the pointer mapping agree)
                let (dev_x0, dev_y0) = (dev_x0(), dev_y0());
                let (dev_x1, dev_y1) = (dev_x0 + DEV_W - 1, dev_y0 + DEV_H - 1);
                // body bezel around the screen
                c.fg = BEZEL;
                c.rect_rounded(Point::new(dev_x0 - 8, dev_y0 - 8), Point::new(dev_x1 + 8, dev_y1 + 8), 14, light_draw::corner::ALL, true);
                //   the live device UI, composited pixel-for-pixel into the screen area: RGB565 to
                // RGB565, so each pixel is copied straight through
                let px = self.preview.pixels();
                if px.len() >= (DEV_W * DEV_H * 2) as usize {
                        for cy in 0..DEV_H {
                                for cx in 0..DEV_W {
                                        let i = ((cy * DEV_W + cx) as usize) * 2;
                                        let color = u16::from_be_bytes([px[i], px[i + 1]]);
                                        c.set(dev_x0 + cx, dev_y0 + cy, color);
                                }
                        }
                }

                drop(c);
                frame.layer.frame_end(frame.display);
                animating
        }

        fn on_pointer(&mut self, ev: PointerEvent) {
                let (dw, dh) = self.preview.size();
                //   canvas space to the device's own pixel space
                let px = ev.x - dev_x0();
                let py = ev.y - dev_y0();
                let inside = px >= 0 && py >= 0 && px < i32::from(dw) && py < i32::from(dh);
                let now = light_host_gui::now_us();
                //   a drag may wander off the screen; clamp so the touch keeps tracking a real cell
                let cx = px.clamp(0, i32::from(dw) - 1) as u16;
                let cy = py.clamp(0, i32::from(dh) - 1) as u16;
                match ev.phase {
                        PointerPhase::Pressed if inside => {
                                self.pressed = true;
                                self.preview.touch(cx, cy, true, now);
                        }
                        PointerPhase::Moved if self.pressed => {
                                self.preview.touch(cx, cy, true, now);
                        }
                        PointerPhase::Released if self.pressed => {
                                self.pressed = false;
                                self.preview.touch(cx, cy, false, now);
                        }
                        _ => {}
                }
        }
}

fn main() {
        let editor = Editor { preview: Preview::new(), pressed: false };
        if let Err(e) = light_host_gui::run(editor) {
                eprintln!("light-ui-editor: {e}");
                std::process::exit(1);
        }
}
