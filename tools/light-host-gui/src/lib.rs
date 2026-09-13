//! A host-side GUI runtime for the light framework.
//!
//! This is a HOST library, not the embedded shell: it links no C shell (`light_mk4_shell` is only
//! for firmware) and never reaches a device -- a desktop tool depends on this crate the way firmware
//! depends on the shell, and the two never meet.
//!
//! The window is an [`eframe`]/[`egui`] app, which owns the event loop and lets each platform's own
//! windowing system (Win32, AppKit, Wayland/X11) do what is native there -- the cross-platform
//! abstraction the host GUI asks for. The chrome (panels, controls) is egui; both are re-exported
//! here so a consumer depends only on this crate.
//!
//! The framework already renders a whole UI on the host: `light-ui`, `light-draw` and
//! `light-display` build and test off-device, and the one hardware-facing seam is the
//! [`DisplayDriver`] trait. This crate supplies the glue to drive that seam into an egui image: a
//! [`NullDriver`] that renders into the `Display`'s own buffer (nothing is pushed to a panel), and
//! [`rgb565_color_image`] to turn that buffer into a texture. So a tool renders a `light-ui` `Ui`
//! offscreen exactly as firmware does and shows it in a window, pixel-faithful, with egui around it.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use light_core::hal::Clock;
use light_display::{DisplayDriver, Frame, Region};

//   consumers write `light_host_gui::egui` / `light_host_gui::eframe` and never name the versions
pub use eframe;
pub use eframe::egui;

// --- the clock -----------------------------------------------------------------------------

//   `Display` takes a bare `fn() -> u64`, which cannot close over an `Instant`, so the host clock
// is a process-global monotonic base set on first read.
static START: OnceLock<Instant> = OnceLock::new();

/// Microseconds since the runtime first asked the time; monotonic, the host's `now_us`.
pub fn now_us() -> u64 {
        START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// A [`Clock`] over [`now_us`], for the one-time `Display::init`.
pub struct HostClock;

impl Clock for HostClock {
        fn now_us(&self) -> u64 {
                now_us()
        }

        fn delay_ms(&mut self, ms: u32) {
                std::thread::sleep(Duration::from_millis(u64::from(ms)));
        }
}

// --- the offscreen display driver ----------------------------------------------------------

/// A [`DisplayDriver`] that pushes nothing: the `Display` renders into its own buffer, which a host
/// reads back with [`Display::front`](light_display::Display::front) and uploads as a texture. There
/// is no transport, so it reports no chunks and completes instantly.
pub struct NullDriver;

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

// --- the egui bridge -----------------------------------------------------------------------

/// Expand a big-endian RGB565 framebuffer into an [`egui::ColorImage`] (RGBA8), replicating the top
/// bits into the low ones so full white stays full white. `px` is `width * height * 2` bytes; a
/// short slice yields a black image rather than panicking.
pub fn rgb565_color_image(width: usize, height: usize, px: &[u8]) -> egui::ColorImage {
        //   opaque to begin with, so an untouched pixel (a short buffer) is black, not transparent
        let mut rgba = vec![255u8; width * height * 4];
        if px.len() >= width * height * 2 {
                for i in 0..width * height {
                        let c = u16::from_be_bytes([px[2 * i], px[2 * i + 1]]);
                        let r = ((c >> 11) & 0x1F) as u8;
                        let g = ((c >> 5) & 0x3F) as u8;
                        let b = (c & 0x1F) as u8;
                        rgba[4 * i] = (r << 3) | (r >> 2);
                        rgba[4 * i + 1] = (g << 2) | (g >> 4);
                        rgba[4 * i + 2] = (b << 3) | (b >> 2);
                        rgba[4 * i + 3] = 255;
                }
        } else {
                //   a short buffer: fill black (the alpha above is already opaque)
                for p in rgba.chunks_exact_mut(4) {
                        p[0] = 0;
                        p[1] = 0;
                        p[2] = 0;
                }
        }
        egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba)
}

/// Open a native window titled `title`, sized `inner_size` logical pixels, and run `app` until it is
/// closed -- the boilerplate over [`eframe::run_native`] every host tool would otherwise repeat.
pub fn run(title: &str, inner_size: [f32; 2], app: impl eframe::App + 'static) -> eframe::Result {
        let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size(inner_size), ..Default::default() };
        eframe::run_native(title, options, Box::new(|_cc| Ok(Box::new(app) as Box<dyn eframe::App>)))
}

#[cfg(test)]
mod tests {
        use super::*;

        #[test]
        fn rgb565_endpoints_expand_to_full_range() {
                //   a 2x1 image: black then white, big-endian
                let px = [0x00, 0x00, 0xFF, 0xFF];
                let img = rgb565_color_image(2, 1, &px);
                assert_eq!(img.pixels[0], egui::Color32::from_rgb(0, 0, 0));
                assert_eq!(img.pixels[1], egui::Color32::from_rgb(255, 255, 255));
        }

        #[test]
        fn a_short_buffer_is_black_not_a_panic() {
                let img = rgb565_color_image(4, 4, &[0x12, 0x34]);
                assert!(img.pixels.iter().all(|p| *p == egui::Color32::from_rgb(0, 0, 0)));
        }
}
