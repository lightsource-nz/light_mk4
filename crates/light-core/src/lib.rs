//! Portable core of the light framework, mk4.
//!
//! Nothing in this crate touches hardware. Everything it needs from the world comes through the
//! traits in [`hal`], which a port crate implements, plus one `critical_section` implementation
//! from the port -- which is what lets the same code run under `cargo test` on the host with a
//! mocked board. The host-first rule mk3 had, made structural.

#![no_std]

pub mod blink;
pub mod button;
pub mod console;
pub mod cst816t;
pub mod display;
pub mod draw;
pub mod events;
pub mod frames;
pub mod hal;
pub mod imu;
pub mod log;
pub mod mailbox;
pub mod module;
pub mod qmi8658;
pub mod sh1107;
pub mod st7789;
pub mod touch;

pub use blink::Blinker;
pub use console::LineReader;
pub use display::{Display, DisplayDriver, Region, UpdateError};
pub use draw::{Canvas, Flip, PixelFormat, Point, Rotation};
pub use events::{EventBus, Subscription};
pub use frames::{FrameLayer, LogicalRegion};
pub use hal::{Clock, I2cBus, I2cError, Idle, InputPin, OutputPin, SpiDisplayBus};
pub use mailbox::Mailbox;
pub use module::{Error, Module, Poll, Runtime};
