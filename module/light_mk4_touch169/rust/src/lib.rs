//! The Rust side of the touch169 spike firmware.
//!
//! The C shell (`../src/main.c`) brings the pico-sdk runtime up and calls `light_app_main`,
//! which never returns. Everything the shell provides to Rust is declared in the one `extern`
//! block below, so the size of the FFI surface -- one of the things the spike measures -- can be
//! read off this file.
//!
//! Milestone 3: the panel and the touch controller, through the pac. A square bounces around
//! the screen, pushed as region updates over SPI+DMA through the chunk protocol; a tap moves it
//! to the finger. The backlight is simply on.

#![no_std]

use core::cell::Cell;
use core::fmt::Write;
use critical_section::Mutex;
use light_core::cst816t::{self, Cst816t, Event};
use light_core::st7789::St7789;
use light_core::{info, log, warn, Board, Display, Module, Poll, Region, Runtime, UpdateError};
use light_rp2350::gpio::Input;
use light_rp2350::i2c::I2c1;
use light_rp2350::spi::Spi1Display;
use light_rp2350::touch169::*;
use light_rp2350::{Backlight, SysClock};

unsafe extern "C" {
        /// Hands a Rust panic to pico-sdk's `panic()`, which knows how to print from whichever
        /// core died. Never returns.
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        /// Prints one line on the shell's stdio: the log sink, and nothing else's.
        fn light_shell_log(msg: *const u8, len: usize);
}

const BYTES_PER_PIXEL: usize = 2;
const FRAME_BYTES: usize = DISPLAY_WIDTH as usize * DISPLAY_HEIGHT as usize * BYTES_PER_PIXEL;

/// The frame buffer: 134 KB, so it lives in .bss rather than on the 2 KB main stack. Handed
/// out exactly once, in `light_app_main`.
static mut FRAME: [u8; FRAME_BYTES] = [0; FRAME_BYTES];

/// The last tap, from the touch module to the display module. A static mailbox is the spike's
/// stand-in for the typed event bus the assessment calls for; the runtime has no channel yet.
static TAP: Mutex<Cell<Option<(u16, u16)>>> = Mutex::new(Cell::new(None));

const BG: u16 = 0x0000;
const FG: u16 = 0xF800; // red, RGB565
const SQUARE: u16 = 24;
const FRAME_INTERVAL_US: u64 = 33_333;

fn fill_rect(buf: &mut [u8], width: u16, r: &Region, color: u16) {
        let hi = (color >> 8) as u8;
        let lo = color as u8;
        for y in r.y0..=r.y1 {
                let start = (y as usize * width as usize + r.x0 as usize) * BYTES_PER_PIXEL;
                let end = start + r.width() as usize * BYTES_PER_PIXEL;
                for px in buf[start..end].chunks_exact_mut(2) {
                        px[0] = hi;
                        px[1] = lo;
                }
        }
}

/// Owns the panel. Paints a bouncing square with region updates, honouring the rule that a
/// region must cover what was drawn before as well as what is drawn now.
struct DisplayMod {
        display: Display<'static, St7789<Spi1Display>>,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        prev: Option<Region>,
        next_frame_us: u64,
        frames: u32,
        last_report_us: u64,
}

impl DisplayMod {
        fn square(&self) -> Region {
                Region::new(self.x as u16, self.y as u16, self.x as u16 + SQUARE - 1, self.y as u16 + SQUARE - 1)
        }

        fn step(&mut self) {
                if let Some((tx, ty)) = critical_section::with(|cs| TAP.borrow(cs).take()) {
                        self.x = (i32::from(tx) - i32::from(SQUARE) / 2).clamp(0, i32::from(DISPLAY_WIDTH - SQUARE));
                        self.y = (i32::from(ty) - i32::from(SQUARE) / 2).clamp(0, i32::from(DISPLAY_HEIGHT - SQUARE));
                }
                self.x += self.dx;
                self.y += self.dy;
                if self.x <= 0 || self.x >= i32::from(DISPLAY_WIDTH - SQUARE) {
                        self.dx = -self.dx;
                        self.x = self.x.clamp(0, i32::from(DISPLAY_WIDTH - SQUARE));
                }
                if self.y <= 0 || self.y >= i32::from(DISPLAY_HEIGHT - SQUARE) {
                        self.dy = -self.dy;
                        self.y = self.y.clamp(0, i32::from(DISPLAY_HEIGHT - SQUARE));
                }
        }
}

impl Module for DisplayMod {
        fn name(&self) -> &'static str {
                "display"
        }
        fn deps(&self) -> &'static [&'static str] {
                &["log_drain"]
        }
        fn load(&mut self) -> Result<(), ()> {
                let mut clock = SysClock;
                self.display.init(&mut clock);
                self.display.driver().set_offset(0, DISPLAY_ROW_OFFSET);
                // after the offset, so the band the offset moves the window over is blanked too
                self.display.driver().clear(BG);
                let square = self.square();
                let frame = self.display.frame_mut().ok_or(())?;
                fill_rect(frame, DISPLAY_WIDTH, &Region::full(DISPLAY_WIDTH, DISPLAY_HEIGHT), BG);
                fill_rect(frame, DISPLAY_WIDTH, &square, FG);
                // the first push is the whole frame: one full-width chunk, the yield-per-poll path
                self.display.update_async(Region::full(DISPLAY_WIDTH, DISPLAY_HEIGHT)).map_err(|_| ())?;
                self.prev = Some(square);
                info!("display up: {}x{}, first frame in flight", DISPLAY_WIDTH, DISPLAY_HEIGHT);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.display.poll() {
                        Ok(true) => return Poll::Busy,
                        Ok(false) => {}
                        Err(UpdateError::Timeout) => warn!("display chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                let now = light_rp2350::now_us();
                if now < self.next_frame_us {
                        return Poll::Idle;
                }
                self.next_frame_us = now + FRAME_INTERVAL_US;
                let old = self.prev.unwrap_or_else(|| self.square());
                self.step();
                let new = self.square();
                let Some(frame) = self.display.frame_mut() else { return Poll::Busy };
                fill_rect(frame, DISPLAY_WIDTH, &old, BG);
                fill_rect(frame, DISPLAY_WIDTH, &new, FG);
                // union of where it was and where it is: a narrow region, so row-chunked with
                // the spin budget -- the other path through the chunk protocol
                let region = old.union(&new);
                if self.display.update_async(region).is_ok() {
                        self.prev = Some(new);
                        self.frames += 1;
                }
                if now - self.last_report_us >= 5_000_000 {
                        self.last_report_us = now;
                        info!("display: {} frames, {} chunk timeouts", self.frames, self.display.timeouts);
                }
                Poll::Busy
        }
}

/// Owns the touch controller; reports taps to the display module through the mailbox.
struct TouchMod {
        touch: Cst816t<I2c1, Input>,
        last_report_us: u64,
        /// Polls that saw INT asserted -- diagnostic: does the line move at all under a finger?
        int_low_polls: u32,
        polls: u32,
}

impl Module for TouchMod {
        fn name(&self) -> &'static str {
                "touch"
        }
        fn deps(&self) -> &'static [&'static str] {
                &["log_drain"]
        }
        fn load(&mut self) -> Result<(), ()> {
                //   reset immediately before the probe: the controller auto-sleeps within about
                // a second of being left alone, and anything that runs between the pulse and
                // the first read -- the display's init did, at first -- can use that second up
                // SAFETY: the reset pin is touched here and nowhere else
                unsafe { light_rp2350::touch_reset_pulse(&mut SysClock) };
                match self.touch.probe() {
                        Ok(Some(id)) => info!("cst816t chip id confirmed: 0x{id:02x}"),
                        Ok(None) => warn!("cst816t answered with an unexpected chip id"),
                        Err(e) => warn!("cst816t did not answer the chip id read: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let now = light_rp2350::now_us();
                self.polls = self.polls.wrapping_add(1);
                if self.touch.int_asserted() {
                        self.int_low_polls = self.int_low_polls.wrapping_add(1);
                }
                match self.touch.poll((now / 1000) as u32) {
                        Some(Event::Down { x, y }) => {
                                info!("touch down at {x},{y}");
                                critical_section::with(|cs| TAP.borrow(cs).set(Some((x, y))));
                                Poll::Busy
                        }
                        Some(Event::Up) => {
                                info!("touch up");
                                Poll::Busy
                        }
                        Some(Event::Move { .. }) => Poll::Busy,
                        None => {
                                if now - self.last_report_us >= 10_000_000 {
                                        self.last_report_us = now;
                                        //   no re-probe here: a read against a sleeping controller
                                        // is an aborted transfer, and mk3 measured what a cadence of
                                        // those does to a bus the IMU shares
                                        info!(
                                                "touch: {} failed reads, INT low on {}/{} polls",
                                                self.touch.failures,
                                                self.int_low_polls,
                                                self.polls
                                        );
                                        self.int_low_polls = 0;
                                        self.polls = 0;
                                }
                                Poll::Idle
                        }
                }
        }
}

/// Moves log records from the queue to the shell's stdio, a bounded number per poll so a burst
/// of logging cannot monopolise a pass.
struct LogDrain;

impl LogDrain {
        const PER_POLL: usize = 4;

        fn sink(record: &log::Record) {
                let mut line = StackBuf::<160> { buf: [0; 160], len: 0 };
                let _ = write!(line, "{record}");
                unsafe { light_shell_log(line.buf.as_ptr(), line.len) }
        }
}

impl Module for LogDrain {
        fn name(&self) -> &'static str {
                "log_drain"
        }
        fn poll(&mut self) -> Poll {
                if log::drain(Self::PER_POLL, Self::sink) == 0 { Poll::Idle } else { Poll::Busy }
        }
        fn unload(&mut self) {
                //   whatever was said on the way down still gets out
                while log::drain(usize::MAX, Self::sink) > 0 {}
        }
}

/// Entry point called by the C shell once the runtime is up.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_main() -> ! {
        log::set_clock(light_rp2350::now_us);

        // SAFETY: each peripheral is constructed exactly once, here, and the shell touches none
        // of them after handing over
        let mut backlight = unsafe { Backlight::new() };
        backlight.set_backlight(true);
        let spi = unsafe {
                Spi1Display::new(
                        PIN_DISPLAY_SCK,
                        PIN_DISPLAY_MOSI,
                        PIN_DISPLAY_CS,
                        PIN_DISPLAY_DC,
                        Some(PIN_DISPLAY_RESET),
                        DISPLAY_SPI_HZ,
                        DISPLAY_DMA_CH,
                )
        };
        info!("spi1 at {} Hz", spi.actual_hz);
        // SAFETY: the one and only reference to FRAME, taken before anything can alias it
        let frame: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(FRAME) };
        let display = Display::new(St7789::new(spi), frame, DISPLAY_WIDTH, DISPLAY_HEIGHT, BYTES_PER_PIXEL as u16, light_rp2350::now_us);

        let i2c = unsafe { I2c1::new(PIN_TOUCH_SCL, PIN_TOUCH_SDA, TOUCH_I2C_HZ) };
        info!("i2c1 at {} Hz", i2c.actual_hz);
        let int = Input::new_pull_up(PIN_TOUCH_INT);
        let touch = Cst816t::new(i2c, int, (light_rp2350::now_us() / 1000) as u32);
        let _ = cst816t::I2C_ADDR;

        let mut drain = LogDrain;
        let mut display_mod = DisplayMod {
                display,
                x: 40,
                y: 60,
                dx: 3,
                dy: 2,
                prev: None,
                next_frame_us: 0,
                frames: 0,
                last_report_us: 0,
        };
        let mut touch_mod = TouchMod { touch, last_report_us: 0, int_low_polls: 0, polls: 0 };

        let mut rt: Runtime<4> = Runtime::new();
        rt.add(&mut display_mod).expect("capacity");
        rt.add(&mut touch_mod).expect("capacity");
        rt.add(&mut drain).expect("capacity");
        rt.start().expect("start");
        let result = rt.run(|| {});
        panic!("runtime exited: {result:?}");
}

/// A `core::fmt::Write` over a fixed stack buffer, so text can be formatted with no allocator
/// and handed across the FFI as a pointer and length.
struct StackBuf<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> Write for StackBuf<N> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let room = N - self.len;
                let take = s.len().min(room);
                self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
                self.len += take;
                // truncating is the right failure for a line; report success so the formatter
                // keeps going rather than abandoning the message at the first overflow
                Ok(())
        }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        let mut msg = StackBuf::<160> { buf: [0; 160], len: 0 };
        let _ = write!(msg, "{info}");
        unsafe { light_shell_panic(msg.buf.as_ptr(), msg.len) }
}
