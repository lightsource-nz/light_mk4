//! The 3.49 firmware displaying an LUI blob -- the on-device consumer of the UI-as-data pipeline.
//!
//! A design authored as JSON is compiled by crush to a binary LUI blob (embedded here with
//! `include_bytes!`), and light-ui's `lui` reader + [`LuiRuntime`] display and navigate it -- no
//! hand-written widget tree. This is the same path a font (LGF) or theme (LTH) blob takes. The
//! board wiring (QSPI panel, AXS15231B touch, backlight) is shared with the other 3.49 apps via
//! [`light_board_touch349`]; this app skips the audio/SD/RTC/power legs it does not need.

#![no_std]

use core::fmt::Write;

use light_board_touch349::board::{self, DISPLAY_HEIGHT, DISPLAY_WIDTH, TOUCH_MAP};
use light_core::{info, log, warn, ConstStaticCell, Module, Poll, Runtime};
use light_display::axs15231b::Axs15231b;
use light_display::{Display, FrameLayer};
use light_draw::PixelFormat;
use light_font::Font;
use light_input::axs15231b::{self as axs, Axs15231bTouch};
use light_rp2::gpio::Input;
use light_rp2::i2c::I2c0;
use light_rp2::pwm::PwmOutput;
use light_rp2::qspi::PioQspiDisplayBus;
use light_rp2::{Breathe, Clocks, SysClock};
use light_ui::{Fonts, Lui, LuiRuntime, Style, Theme, Ui};

unsafe extern "C" {
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        fn light_shell_log(msg: *const u8, len: usize);
        fn light_shell_read_byte() -> i32;
}

#[repr(C)]
pub struct ShellInfo {
        clk_sys_hz: u32,
        clk_peri_hz: u32,
}

/// The compiled assets, embedded: the font, the theme, and the UI design as an LUI blob.
static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));
static UI_BLOB: &[u8] = include_bytes!(env!("LIGHT_UI_LUI"));

/// The widget arena capacity per page.
const UI_WIDGETS: usize = 32;
const FPS: u32 = 30;

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);

//   single-buffered: this UI is static (no page-transition animation on the blob path), so one
// 215 KB buffer is enough and leaves the RAM a second would cost
static FRAME: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565));
static UI: ConstStaticCell<Ui<u16, UI_WIDGETS>> = ConstStaticCell::new(Ui::new());

/// The one module: it owns the panel, the touch input and the blob runtime, and each poll drives
/// the display, reads touch into the runtime, and renders.
struct LuiMod {
        display: Display<'static, Axs15231b<PioQspiDisplayBus>>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        theme: Theme,
        rt: LuiRuntime<UI_WIDGETS>,
        touch: Axs15231bTouch<I2c0, Input>,
        //   held so the backlight PWM keeps driving; set bright at load
        _backlight: PwmOutput,
        clock: SysClock,
}

impl Module for LuiMod {
        fn name(&self) -> &'static str {
                "lui"
        }

        fn load(&mut self) -> Result<(), ()> {
                self.display.init(&mut self.clock);
                self.layer.set_frame_rate(FPS);
                self.layer.bg = self.theme.bg;
                self.rt.ui.set_style(&Style::new(self.theme, Fonts::uniform(&self.font)));
                self.rt.ui.fit(self.layer);
                self.rt.start();
                self.rt.ui.invalidate_all();
                //   the shared chip answers touch only after the panel's init reset it
                for _ in 0..3 {
                        if self.touch.probe().is_ok() {
                                break;
                        }
                        light_core::hal::Clock::delay_ms(&mut self.clock, 20);
                }
                info!("lui: displaying an LUI blob on the 3.49");
                self.render();
                Ok(())
        }

        fn poll(&mut self) -> Poll {
                let _ = self.layer.poll(&mut self.display);
                //   read touch into the runtime; a tapped button navigates per the blob
                let now_ms = (light_rp2::now_us() / 1000) as u32;
                if let Some(ev) = self.touch.poll(now_ms) {
                        let now = light_rp2::now_us();
                        match ev {
                                axs::Event::Down { x, y } | axs::Event::Move { x, y } => self.rt.touch(x, y, true, now),
                                axs::Event::Up => self.rt.touch(0, 0, false, now),
                                axs::Event::Reset => {}
                        }
                }
                self.render();
                let busy = self.layer.busy(&self.display);
                if self.rt.ui.is_dirty() || self.rt.ui.is_animating() || busy {
                        Poll::Busy
                } else {
                        Poll::Idle
                }
        }
}

impl LuiMod {
        fn render(&mut self) {
                if !self.rt.ui.is_dirty() && !self.rt.ui.is_animating() {
                        return;
                }
                //   the AXS15231B takes full-frame pushes (it ignores partial windowing), so the
                // whole canvas is invalidated before each repaint
                self.rt.ui.invalidate_all();
                let now = light_rp2::now_us();
                let style = Style::new(self.theme, Fonts::uniform(&self.font));
                self.rt.ui.render(self.layer, &mut self.display, &style, now);
        }
}

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        log::set_clock(light_rp2::now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = board::take(&clocks).expect("the board's peripherals are taken once");

        let mut backlight = p.backlight;
        //   LOW duty is bright on this board (see board::BACKLIGHT_INVERTED); 0 is fully on
        backlight.set_duty(0);

        let buf: &'static mut [u8] = FRAME.take();
        let display = Display::new(Axs15231b::new(p.display_bus), buf, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        let theme = match Theme::parse(THEME_BLOB) {
                Ok(t) => t,
                Err(e) => panic!("the embedded theme does not parse: {e:?}"),
        };
        let lui = match Lui::parse(UI_BLOB) {
                Ok(l) => l,
                Err(e) => panic!("the embedded LUI blob does not parse: {e:?}"),
        };
        let touch = Axs15231bTouch::new(p.touch_bus, p.touch_int, TOUCH_MAP, (light_rp2::now_us() / 1000) as u32);

        //   a stack local: light_app_main never returns, so it outlives the runtime. The two big
        // objects (the framebuffer and the widget arena) are `static`s it only references
        let mut lui_mod = LuiMod {
                display,
                layer: LAYER.take(),
                font,
                theme,
                rt: LuiRuntime::new(UI.take(), lui),
                touch,
                _backlight: backlight,
                clock: SysClock,
        };

        let mut rt: Runtime<1> = Runtime::new();
        rt.add(&mut lui_mod).expect("capacity");
        rt.start().expect("start");
        info!("runtime started");
        let mut idle = Breathe;
        let result = rt.run(|| light_core::Idle::idle(&mut idle));
        match result {
                Ok(()) => info!("runtime stopped cleanly"),
                Err(e) => warn!("runtime stopped with {e:?}"),
        }
        loop {
                core::hint::spin_loop();
        }
}

// --- shell glue: logging on core 1, panic handoff ------------------------------------------

struct StackString<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> StackString<N> {
        fn new() -> Self {
                Self { buf: [0; N], len: 0 }
        }
}

impl<const N: usize> Write for StackString<N> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let room = N - self.len;
                let take = room.min(s.len());
                self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
                self.len += take;
                Ok(())
        }
}

fn log_sink(record: &log::Record) {
        let mut line = StackString::<160>::new();
        let _ = write!(line, "{record}");
        unsafe { light_shell_log(line.buf.as_ptr(), line.len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_service() {
        log::drain(4, log_sink);
        //   no console on this demo; drain any input so the shell's buffer never backs up
        for _ in 0..32 {
                if unsafe { light_shell_read_byte() } < 0 {
                        break;
                }
        }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        let mut msg = StackString::<160>::new();
        let _ = write!(msg, "{info}");
        unsafe { light_shell_panic(msg.buf.as_ptr(), msg.len) }
}
