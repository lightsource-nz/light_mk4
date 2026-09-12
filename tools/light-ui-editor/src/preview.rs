//! The device-UI preview.
//!
//! A real light-ui tree, rendered through the same rasteriser AND the same `Ui::render` path the
//! panels use, into an off-screen RGB565 buffer at a device resolution. The editor composites that
//! buffer into the stage and feeds it pointer input, so the previewed UI focuses, flashes and
//! navigates exactly as it would on the device. For now the tree is a fixed sample; a loaded
//! design comes next.

use light_core::hal::Clock;
use light_display::{Display, DisplayDriver, Frame, FrameLayer, Region};
use light_draw::PixelFormat;
use light_host_gui::now_us;
use light_ui::{Desc, Fonts, Page, Shade, Style, Theme, Touch, Ui};

use crate::font;

/// The previewed device screen, in pixels. A portrait panel; a stand-in until real designs load.
pub const DEV_W: u16 = 240;
pub const DEV_H: u16 = 400;

/// The preview font's pixel size.
const PIXEL_SIZE: u16 = 18;

/// Room for the sample tree: a window and its handful of children.
const UI_WIDGETS: usize = 8;

/// The preview's application event type. The sample tree navigates rather than emits, so nothing
/// is carried; a real loaded design will bring its own.
type Ev = ();

// The sample device UI: two pages, so a tap can navigate and the transition is exercised.
static REC: Desc<Ev> = Desc::button("Record");
static PLAY: Desc<Ev> = Desc::button("Play");
static FILES: Desc<Ev> = Desc::button("Files >").navigate(&FILES_PAGE);
static ROOT: Desc<Ev> = Desc::window("Dictaphone").stack(6).children(&[&REC, &PLAY, &FILES]);
static ROOT_PAGE: Page<Ev> = Page::new(&ROOT, None);

static TAKE_1: Desc<Ev> = Desc::button("REC_0001");
static TAKE_2: Desc<Ev> = Desc::button("REC_0002");
static BACK: Desc<Ev> = Desc::button("< Back").back();
static FILES_WIN: Desc<Ev> = Desc::window("Files").stack(6).children(&[&TAKE_1, &TAKE_2, &BACK]);
static FILES_PAGE: Page<Ev> = Page::new(&FILES_WIN, Some(&ROOT_PAGE));

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

/// A do-nothing [`DisplayDriver`]: the preview renders into the [`Display`]'s own buffer and reads
/// it back with [`Display::front`], so nothing is ever pushed. `chunk_count` 0 means every update
/// is a no-op and the display is never busy.
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

/// The preview pipeline: a light-ui `Ui` over a device-sized off-screen `Display`.
pub struct Preview {
        ui: Ui<Ev, UI_WIDGETS>,
        display: Display<'static, NullDriver>,
        layer: FrameLayer,
        theme: Theme,
        font: light_font::Font<'static>,
}

impl Preview {
        pub fn new() -> Self {
                let font = font::load(PIXEL_SIZE);
                let theme = preview_theme();
                //   the framebuffer lives for the program, like the window's; leak it once so the
                // Display is 'static without a self-referential struct
                let buf: &'static mut [u8] = Vec::leak(vec![0u8; PixelFormat::Rgb565.buffer_len(DEV_W, DEV_H)]);
                let display = Display::new(NullDriver, buf, DEV_W, DEV_H, PixelFormat::Rgb565, now_us);
                let mut layer = FrameLayer::new(DEV_W, DEV_H, PixelFormat::Rgb565);
                layer.bg = theme.bg;
                let mut ui = Ui::new();
                //   bind the look (theme + font metrics), take the canvas geometry, then build the
                // tree -- the same order a board's setup uses
                ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
                ui.fit(&layer);
                ui.navigate(&ROOT_PAGE).expect("the sample tree fits the arena");
                Self { ui, display, layer, theme, font }
        }

        /// Render a frame into the off-screen buffer if anything changed. Returns `true` while an
        /// animation is in flight (a press flash, a page transition), so the caller keeps redrawing.
        pub fn render(&mut self, now_us: u64) -> bool {
                let style = Style::new(self.theme, Fonts::uniform(&self.font));
                let drew = self.ui.render(&mut self.layer, &mut self.display, &style, now_us);
                //   drain the frame's queued regions; the null driver completes at once, so this is
                // what keeps the layer ready for the next frame
                while self.layer.poll(&mut self.display).unwrap_or(false) {}
                drew || self.ui.is_animating()
        }

        /// Feed a touch in the preview's own pixel space. Navigation, focus and the press flash are
        /// all handled inside `Ui::touch`; the result is reflected on the next [`render`](Self::render).
        pub fn touch(&mut self, x: u16, y: u16, touching: bool, now_us: u64) -> Touch<Ev> {
                self.ui.touch(x, y, touching, now_us)
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
