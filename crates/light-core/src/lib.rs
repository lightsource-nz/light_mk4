//! Portable core of the light framework, mk4 spike.
//!
//! Nothing in this crate touches hardware. Everything it needs from the world comes through
//! traits a board crate implements, plus one `critical_section` implementation from the port,
//! which is what lets the same code run under `cargo test` on the host with a mocked board --
//! the host-first rule mk3 has, made structural.

#![no_std]

pub mod blink;
pub mod bus;
pub mod console;
pub mod cst816t;
pub mod display;
pub mod draw;
pub mod log;
pub mod mailbox;
pub mod module;
pub mod st7789;

pub use blink::Blinker;
pub use bus::{Clock, I2cBus, I2cError, InputPin, OutputPin, SpiDisplayBus};
pub use console::LineReader;
pub use display::{Display, DisplayDriver, Region, UpdateError};
pub use mailbox::Mailbox;
pub use module::{Error, Module, Poll, Runtime};

/// What the spike needs of a board so far: one output and a clock.
///
/// Deliberately minimal. The real port interface grows from here one primitive at a time, each
/// added when a piece of portable code needs it, so the boundary records what the framework
/// actually depends on rather than what a HAL happens to offer.
pub trait Board {
        /// Drive the panel backlight enable line.
        fn set_backlight(&mut self, on: bool);
        /// Microseconds since boot. Monotonic; wraps only after ~584,000 years.
        fn now_us(&self) -> u64;
}
