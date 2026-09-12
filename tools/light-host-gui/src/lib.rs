//! A host-side GUI shell for the light framework.
//!
//! The framework already renders a whole UI on the host: `light-ui`, `light-draw` and
//! `light-display` all build and test off-device, and the one hardware-facing seam is the
//! [`DisplayDriver`] trait -- how a frame's chunks get pushed to a panel. This crate implements
//! that seam against a native desktop window instead of an SPI bus, so the exact on-device render
//! path (`FrameLayer` -> `Display` -> driver) drives a window on Windows, macOS or Linux.
//!
//! The native windowing itself is delegated to [`winit`], which is the abstraction the request
//! asks for: it owns the event loop and the window and lets each platform's own windowing system
//! (Win32, AppKit, Wayland/X11) do what is native there. We own only the pixels -- a CPU
//! framebuffer presented through [`softbuffer`] -- which is how the rest of the framework works
//! too (a [`Canvas`](light_draw::Canvas) over a byte buffer).
//!
//! An application implements [`HostApp`] and hands it to [`run`]. Each frame it is given a
//! [`HostFrame`] -- the live `FrameLayer` and `Display` -- and draws through the ordinary
//! toolkit API; the shell flushes the frame into the window and presents it. This first cut
//! establishes that whole pipeline; input and a `light-ui` event loop layer over it next.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use light_core::hal::Clock;
use light_draw::PixelFormat;
use light_display::{Display, DisplayDriver, FrameLayer, Frame, Region};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

/// The frame buffer format the shell presents. RGB565 is what the panels take, so the host path
/// stays byte-for-byte the same as a device's -- the only extra step is expanding to 8888 at the
/// window, where the desktop wants full-width color.
const FORMAT: PixelFormat = PixelFormat::Rgb565;

/// The un-covered border around a canvas smaller than its window: a neutral desk, not black, so
/// the canvas edge reads as an edge.
const MARGIN_XRGB: u32 = 0x0014_181C;

// --- the clock -----------------------------------------------------------------------------

//   `Display` takes a bare `fn() -> u64`, which cannot close over an `Instant`, so the host
// clock is a process-global monotonic base set on first read.
static START: OnceLock<Instant> = OnceLock::new();

/// Microseconds since the shell first asked the time; monotonic, the host's `now_us`.
pub fn now_us() -> u64 {
        START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// A [`Clock`] over [`now_us`], for the one-time `Display::init`.
struct HostClock;

impl Clock for HostClock {
        fn now_us(&self) -> u64 {
                now_us()
        }

        fn delay_ms(&mut self, ms: u32) {
                std::thread::sleep(Duration::from_millis(u64::from(ms)));
        }
}

// --- the display driver: a window instead of a panel ---------------------------------------

/// Expand one big-endian RGB565 pixel to `0x00RRGGBB`, replicating the top bits into the low
/// ones so full white stays full white rather than 0xF8.
#[inline]
fn rgb565_to_xrgb(c: u16) -> u32 {
        let r = u32::from((c >> 11) & 0x1F);
        let g = u32::from((c >> 5) & 0x3F);
        let b = u32::from(c & 0x1F);
        let r8 = (r << 3) | (r >> 2);
        let g8 = (g << 2) | (g >> 4);
        let b8 = (b << 3) | (b >> 2);
        (r8 << 16) | (g8 << 8) | b8
}

/// A [`DisplayDriver`] whose "panel" is an in-memory `0x00RRGGBB` image the window presents.
/// `kick` converts the region it is handed straight into that image; there is no transport to
/// wait on, so every chunk completes at once.
pub struct WindowDriver {
        pixels: Vec<u32>,
        width: u16,
}

impl WindowDriver {
        fn new(width: u16, height: u16) -> Self {
                Self { pixels: vec![0u32; usize::from(width) * usize::from(height)], width }
        }

        /// The presentable `0x00RRGGBB` image, row-major, `width * height` long.
        pub fn pixels(&self) -> &[u32] {
                &self.pixels
        }
}

impl DisplayDriver for WindowDriver {
        fn init(&mut self, _clock: &mut dyn Clock, _width: u16, _height: u16) {}

        fn chunk_count(&self, _region: &Region) -> u16 {
                //   the whole region in one memory blit; no wire to chunk for
                1
        }

        fn chunks_per_poll(&self, _region: &Region) -> u16 {
                0
        }

        fn kick(&mut self, frame: &Frame<'_>, region: &Region, _index: u16) {
                let width = usize::from(self.width);
                for y in region.y0..=region.y1 {
                        let row = frame.row(region, y);
                        let base = usize::from(y) * width + usize::from(region.x0);
                        for (i, px) in row.chunks_exact(2).enumerate() {
                                let c = u16::from_be_bytes([px[0], px[1]]);
                                self.pixels[base + i] = rgb565_to_xrgb(c);
                        }
                }
        }

        fn chunk_complete(&mut self) -> bool {
                true
        }

        fn chunk_timeout_ms(&self) -> u32 {
                1000
        }
}

// --- the application seam ------------------------------------------------------------------

/// What one frame's render is handed: the live frame layer and display, plus the host time.
/// Draw through them exactly as an on-device module does -- `layer.frame_begin(display, now_us)`
/// for a canvas, or `ui.render(layer, display, font, now_us)` once a `light-ui` `Ui` is wired.
pub struct HostFrame<'a> {
        pub layer: &'a mut FrameLayer,
        pub display: &'a mut Display<'static, WindowDriver>,
        pub now_us: u64,
}

/// A pointer (mouse) gesture phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPhase {
        /// The pointer moved (a button may or may not be held).
        Moved,
        /// The primary button went down.
        Pressed,
        /// The primary button came up.
        Released,
}

/// A pointer event delivered in CANVAS space (the app's logical canvas, origin top-left). The
/// shell has already undone the window centring, so `(0,0)` is the canvas's top-left however the
/// window is sized; coordinates may fall outside `0..canvas_size` when the pointer is on the
/// margin.
#[derive(Clone, Copy, Debug)]
pub struct PointerEvent {
        pub x: i32,
        pub y: i32,
        pub phase: PointerPhase,
}

/// A key press, reduced to what a text field needs: a typed character, or an editing key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
        /// A printable character was typed.
        Text(char),
        Backspace,
        Enter,
        Escape,
}

/// A host GUI application. Implement it and pass it to [`run`].
pub trait HostApp {
        /// The window title.
        fn title(&self) -> &str;

        /// The fixed logical canvas size in pixels -- the UI's own coordinate space, which is a
        /// device resolution when editing a device UI. The window opens at this size; resizing
        /// the window centres this canvas rather than rescaling it.
        fn canvas_size(&self) -> (u16, u16);

        /// The colour the frame layer clears to each frame (RGB565).
        fn background(&self) -> u16 {
                0x0000
        }

        /// A pointer event in canvas space. Default: ignored.
        fn on_pointer(&mut self, _event: PointerEvent) {}

        /// A key press. Default: ignored.
        fn on_key(&mut self, _key: Key) {}

        /// Draw one frame. Returns `true` to ask for another redraw -- an animation is in flight
        /// and the frame after this one will differ.
        fn render(&mut self, frame: &mut HostFrame<'_>) -> bool;
}

// --- the shell -----------------------------------------------------------------------------

/// Everything a live window owns. Built on `resumed`; the display borrows a leaked framebuffer,
/// so it is `'static` -- one buffer for the window's life, allocated once.
struct Gfx {
        window: Rc<Window>,
        _context: softbuffer::Context<Rc<Window>>,
        surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
        display: Display<'static, WindowDriver>,
        layer: FrameLayer,
        canvas_w: u16,
        canvas_h: u16,
        /// The last pointer position in physical window pixels, for the button events winit
        /// reports without one.
        last_cursor: (f64, f64),
        /// Whether the primary button is down, so a move is a drag worth a redraw (a hover is not).
        pointer_down: bool,
}

impl Gfx {
        /// The canvas's top-left in physical window pixels -- the centring the pointer mapping and
        /// [`present`](Self::present) share.
        fn canvas_origin(&self) -> (i32, i32) {
                let size = self.window.inner_size();
                let ox = (size.width as i32 - i32::from(self.canvas_w)) / 2;
                let oy = (size.height as i32 - i32::from(self.canvas_h)) / 2;
                (ox, oy)
        }

        /// Copy the driver's image into the window surface, the canvas centred and the rest the
        /// margin colour, then present.
        fn present(&mut self) {
                let size = self.window.inner_size();
                let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
                        return;
                };
                if self.surface.resize(w, h).is_err() {
                        return;
                }
                //   computed before the surface buffer is borrowed: both read `self`
                let (ox, oy) = self.canvas_origin();
                let Ok(mut buffer) = self.surface.buffer_mut() else {
                        return;
                };
                buffer.fill(MARGIN_XRGB);

                let (sw, sh) = (size.width as i32, size.height as i32);
                let (cw, ch) = (i32::from(self.canvas_w), i32::from(self.canvas_h));
                let pixels = self.display.driver().pixels();
                let dx0 = ox.max(0);
                let dx1 = (ox + cw).min(sw);
                if dx1 <= dx0 {
                        let _ = buffer.present();
                        return;
                }
                for cy in 0..ch {
                        let dy = oy + cy;
                        if dy < 0 || dy >= sh {
                                continue;
                        }
                        let dst = (dy * sw) as usize;
                        let src = (cy * cw) as usize;
                        for dx in dx0..dx1 {
                                buffer[dst + dx as usize] = pixels[src + (dx - ox) as usize];
                        }
                }
                let _ = buffer.present();
        }
}

/// The winit application: an app plus its window state.
struct Shell<A: HostApp> {
        app: A,
        gfx: Option<Gfx>,
}

impl<A: HostApp> ApplicationHandler for Shell<A> {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                if self.gfx.is_some() {
                        return;
                }
                let (cw, ch) = self.app.canvas_size();
                let attrs = Window::default_attributes()
                        .with_title(self.app.title())
                        .with_inner_size(LogicalSize::new(f64::from(cw), f64::from(ch)));
                let window = Rc::new(event_loop.create_window(attrs).expect("create window"));
                let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
                let surface = softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");

                //   the framebuffer outlives every frame and is borrowed by the Display for the
                // window's whole life; leaking it once is how the on-device code's `static`
                // buffer becomes a host `'static` without a self-referential struct
                let buf: &'static mut [u8] = Vec::leak(vec![0u8; FORMAT.buffer_len(cw, ch)]);
                let mut display = Display::new(WindowDriver::new(cw, ch), buf, cw, ch, FORMAT, now_us);
                display.init(&mut HostClock);
                let mut layer = FrameLayer::new(cw, ch, FORMAT);
                layer.bg = self.app.background();

                window.request_redraw();
                self.gfx = Some(Gfx { window, _context: context, surface, display, layer, canvas_w: cw, canvas_h: ch, last_cursor: (0.0, 0.0), pointer_down: false });
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
                let Shell { app, gfx } = self;
                let Some(gfx) = gfx.as_mut() else {
                        return;
                };
                match event {
                        WindowEvent::CloseRequested => event_loop.exit(),
                        WindowEvent::Resized(_) => gfx.window.request_redraw(),
                        WindowEvent::CursorMoved { position, .. } => {
                                gfx.last_cursor = (position.x, position.y);
                                let (ox, oy) = gfx.canvas_origin();
                                app.on_pointer(PointerEvent { x: position.x as i32 - ox, y: position.y as i32 - oy, phase: PointerPhase::Moved });
                                //   a hover changes nothing; only redraw when the move is a drag
                                if gfx.pointer_down {
                                        gfx.window.request_redraw();
                                }
                        }
                        WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                                gfx.pointer_down = state == ElementState::Pressed;
                                let (ox, oy) = gfx.canvas_origin();
                                let (cx, cy) = gfx.last_cursor;
                                let phase = if gfx.pointer_down { PointerPhase::Pressed } else { PointerPhase::Released };
                                app.on_pointer(PointerEvent { x: cx as i32 - ox, y: cy as i32 - oy, phase });
                                gfx.window.request_redraw();
                        }
                        WindowEvent::KeyboardInput { event, .. } => {
                                if event.state == ElementState::Pressed {
                                        let named = match event.logical_key {
                                                WinitKey::Named(NamedKey::Backspace) => Some(Key::Backspace),
                                                WinitKey::Named(NamedKey::Enter) => Some(Key::Enter),
                                                WinitKey::Named(NamedKey::Escape) => Some(Key::Escape),
                                                _ => None,
                                        };
                                        if let Some(key) = named {
                                                app.on_key(key);
                                                gfx.window.request_redraw();
                                        } else if let Some(text) = &event.text {
                                                //   the character(s) this press produced, honouring the
                                                // layout and modifiers; control chars are the editing
                                                // keys handled above
                                                for ch in text.chars().filter(|c| !c.is_control()) {
                                                        app.on_key(Key::Text(ch));
                                                }
                                                gfx.window.request_redraw();
                                        }
                                }
                        }
                        WindowEvent::RedrawRequested => {
                                let mut frame = HostFrame { layer: &mut gfx.layer, display: &mut gfx.display, now_us: now_us() };
                                let again = app.render(&mut frame);
                                //   flush the frame's queued regions through the driver into the
                                // window image; the driver completes every chunk at once
                                while gfx.layer.poll(&mut gfx.display).unwrap_or(false) {}
                                gfx.present();
                                //   an animation (a press flash, a page transition) wants the next
                                // frame; a static UI goes back to waiting for input
                                if again {
                                        gfx.window.request_redraw();
                                }
                        }
                        _ => {}
                }
        }
}

/// Open a native window and run `app` until it is closed.
pub fn run<A: HostApp + 'static>(app: A) -> Result<(), Box<dyn std::error::Error>> {
        let event_loop = EventLoop::new()?;
        event_loop.set_control_flow(ControlFlow::Wait);
        let mut shell = Shell { app, gfx: None };
        event_loop.run_app(&mut shell)?;
        Ok(())
}

#[cfg(test)]
mod tests {
        use super::*;

        #[test]
        fn rgb565_endpoints_expand_to_full_range() {
                assert_eq!(rgb565_to_xrgb(0x0000), 0x0000_0000, "black");
                assert_eq!(rgb565_to_xrgb(0xFFFF), 0x00FF_FFFF, "white saturates every channel");
                assert_eq!(rgb565_to_xrgb(0xF800), 0x00FF_0000, "pure red");
                assert_eq!(rgb565_to_xrgb(0x07E0), 0x0000_FF00, "pure green");
                assert_eq!(rgb565_to_xrgb(0x001F), 0x0000_00FF, "pure blue");
        }

        #[test]
        fn the_driver_blits_a_region_into_its_image() {
                let mut driver = WindowDriver::new(4, 2);
                //   a 4x2 RGB565 buffer, big-endian: fill row 1 with white, leave row 0 black
                let mut buf = [0u8; 4 * 2 * 2];
                for px in buf[4 * 2..].chunks_exact_mut(2) {
                        px[0] = 0xFF;
                        px[1] = 0xFF;
                }
                let frame = Frame { buf: &buf, width: 4, height: 2, format: FORMAT, stride: FORMAT.stride(4) };
                driver.kick(&frame, &Region::full(4, 2), 0);
                assert_eq!(driver.pixels()[0], 0x0000_0000, "row 0 stays black");
                assert_eq!(driver.pixels()[4], 0x00FF_FFFF, "row 1 is white");
        }
}
