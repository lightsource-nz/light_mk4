//! The device-UI preview.
//!
//! A real light-ui tree -- now MATERIALISED FROM A LOADED DESIGN, not a hard-coded sample --
//! rendered through the same rasteriser and `Ui::render` path the panels use, into an off-screen
//! RGB565 buffer at a device resolution. The editor composites that buffer into the stage and feeds
//! it pointer input, so the previewed UI focuses, flashes and navigates exactly as it would on the
//! device. A button's navigation is an event the preview resolves against the loaded page list,
//! keeping its own history for `back`.

use light_core::hal::Clock;
use light_display::{Display, DisplayDriver, Frame, FrameLayer, Region};
use light_draw::PixelFormat;
use light_host_gui::now_us;
use light_ui::{Fonts, Page, Style, Theme, Touch, Ui};

use crate::design::{self, DesignEvent};
use crate::font;

/// The previewed device screen, in pixels. A portrait panel; a stand-in until the design carries
/// its own target size.
pub const DEV_W: u16 = 240;
pub const DEV_H: u16 = 400;

/// The preview font's pixel size.
const PIXEL_SIZE: u16 = 18;

/// Widget arena capacity per page. Generous; a page with more widgets than this fails to navigate
/// (and is left on the previous page) rather than drawing half a tree.
const UI_WIDGETS: usize = 32;

/// The look, loaded from the framework's real steel theme -- the same JSON the firmware compiles.
const THEME_JSON: &str = include_str!("../../../themes/steel.json");

/// The design previewed, loaded from data.
const DESIGN_JSON: &str = include_str!("../design.json");

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

/// The preview pipeline: a light-ui `Ui` over a device-sized off-screen `Display`, driving a design
/// loaded from data.
pub struct Preview {
        ui: Ui<DesignEvent, UI_WIDGETS>,
        display: Display<'static, NullDriver>,
        layer: FrameLayer,
        theme: Theme,
        font: light_font::Font<'static>,
        /// The loaded pages, materialised into `'static` trees the `Ui` navigates by index.
        pages: Vec<&'static Page<DesignEvent>>,
        /// The visited-page stack, its last entry the current page; `back` pops it.
        history: Vec<usize>,
}

impl Preview {
        pub fn new() -> Self {
                let font = font::load(PIXEL_SIZE);
                let theme = crate::theme::parse(THEME_JSON).expect("the bundled steel theme parses");
                let design = design::parse(DESIGN_JSON).expect("the bundled design parses");
                let pages = design::materialize(&design);
                let root = design.root.min(pages.len().saturating_sub(1));

                //   the framebuffer lives for the program, like the window's; leak it once so the
                // Display is 'static without a self-referential struct
                let buf: &'static mut [u8] = Vec::leak(vec![0u8; PixelFormat::Rgb565.buffer_len(DEV_W, DEV_H)]);
                let display = Display::new(NullDriver, buf, DEV_W, DEV_H, PixelFormat::Rgb565, now_us);
                let mut layer = FrameLayer::new(DEV_W, DEV_H, PixelFormat::Rgb565);
                layer.bg = theme.bg;
                let mut ui = Ui::new();
                //   bind the look (theme + font metrics), take the canvas geometry, then open the
                // root page -- the same order a board's setup uses
                ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
                ui.fit(&layer);
                let mut history = Vec::new();
                if let Some(&page) = pages.get(root) {
                        ui.navigate(page).expect("the root page fits the arena");
                        history.push(root);
                }
                Self { ui, display, layer, theme, font, pages, history }
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

        /// Feed a touch in the preview's own pixel space. Focus and the press flash are handled
        /// inside `Ui::touch`; a navigation event it returns is resolved here against the loaded
        /// pages, and reflected on the next [`render`](Self::render).
        pub fn touch(&mut self, x: u16, y: u16, touching: bool, now_us: u64) {
                if let Touch::Tap { emitted: Some(event), .. } = self.ui.touch(x, y, touching, now_us) {
                        match event {
                                DesignEvent::Goto(idx) => self.goto(idx as usize),
                                DesignEvent::Back => self.back(),
                        }
                }
        }

        fn goto(&mut self, idx: usize) {
                if let Some(&page) = self.pages.get(idx) {
                        if self.ui.navigate(page).is_ok() {
                                self.history.push(idx);
                        }
                }
        }

        fn back(&mut self) {
                if self.history.len() > 1 {
                        self.history.pop();
                        let idx = *self.history.last().expect("history is non-empty");
                        let _ = self.ui.navigate(self.pages[idx]);
                }
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
