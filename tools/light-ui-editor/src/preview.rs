//! The device-UI preview.
//!
//! A real light-ui tree, rendered through the same rasteriser the panels use, into an off-screen
//! RGB565 buffer at a device resolution. The editor composites that buffer into the stage. This is
//! the point of the whole tool: what shows here is exactly what the device would show. For now the
//! tree is a fixed sample and the preview is static (it paints, it does not yet take input); a
//! loaded design and interaction come next.

use light_display::FrameLayer;
use light_draw::{Canvas, PixelFormat};
use light_ui::{Desc, Fonts, Page, Shade, Style, Theme, Ui};

use crate::font;

/// The previewed device screen, in pixels. A portrait panel; a stand-in until real designs load.
pub const DEV_W: u16 = 240;
pub const DEV_H: u16 = 400;

/// The preview font's pixel size.
const PIXEL_SIZE: u16 = 18;

/// Room for the sample tree: a window and its handful of children.
const UI_WIDGETS: usize = 8;

// The sample device UI: an ordinary const descriptor tree, event type `()` since the preview does
// not emit yet.
static REC: Desc<()> = Desc::button("Record");
static PLAY: Desc<()> = Desc::button("Play");
static FILES: Desc<()> = Desc::button("Files");
static HINT: Desc<()> = Desc::label("a live light-ui tree");
static ROOT: Desc<()> = Desc::window("Dictaphone").stack(6).children(&[&REC, &PLAY, &FILES, &HINT]);
static ROOT_PAGE: Page<()> = Page::new(&ROOT, None);

/// Pack 8-bit RGB into RGB565.
const fn rgb(r: u8, g: u8, b: u8) -> u16 {
        (((r as u16) >> 3) << 11) | (((g as u16) >> 2) << 5) | ((b as u16) >> 3)
}

/// A steel-like look for the preview, so it reads as a real themed device rather than the bare
/// mono default: grey glass, black lines, a blue title bar and flat-blue controls.
fn preview_theme() -> Theme {
        let blue = rgb(0x22, 0x40, 0x5F);
        Theme {
                bg: rgb(0xB0, 0xB8, 0xC0),
                frame: 0x0000,
                bar: Some(blue),
                title: 0xFFFF,
                text: rgb(0x10, 0x14, 0x18),
                button_outline: rgb(0x78, 0x90, 0xA8),
                button_text: 0xFFFF,
                focus_text: 0xFFFF,
                button_surface: Some(Shade { from: blue, to: blue }),
                focus_surface: Some(Shade { from: rgb(0x4C, 0x5D, 0x8A), to: blue }),
                ..Theme::DEFAULT
        }
}

/// The preview pipeline: a light-ui `Ui` over a device-sized off-screen buffer.
pub struct Preview {
        ui: Ui<(), UI_WIDGETS>,
        buf: Vec<u8>,
        theme: Theme,
        font: light_font::Font<'static>,
}

impl Preview {
        pub fn new() -> Self {
                let font = font::load(PIXEL_SIZE);
                let layer = FrameLayer::new(DEV_W, DEV_H, PixelFormat::Rgb565);
                let theme = preview_theme();
                let mut ui = Ui::new();
                //   bind the look (theme + font metrics), take the canvas geometry, then build the
                // tree -- the same order a board's setup uses
                ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
                ui.fit(&layer);
                ui.navigate(&ROOT_PAGE).expect("the sample tree fits the arena");
                let buf = vec![0u8; PixelFormat::Rgb565.buffer_len(DEV_W, DEV_H)];
                Self { ui, buf, theme, font }
        }

        /// Repaint the tree into the off-screen buffer.
        pub fn paint(&mut self) {
                let style = Style::new(self.theme, Fonts::uniform(&self.font));
                let mut c = Canvas::new(&mut self.buf, PixelFormat::Rgb565, DEV_W, DEV_H);
                c.bg = self.theme.bg;
                c.clear();
                self.ui.paint(&mut c, &style);
        }

        /// The painted RGB565 image, `DEV_W * DEV_H` pixels, big-endian.
        pub fn pixels(&self) -> &[u8] {
                &self.buf
        }
}
