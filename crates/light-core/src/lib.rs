//! Portable core of the light framework, mk4: the runtime and what every other crate needs
//! from it. The port interface ([`hal`]), the module runtime, the bounded log queue, the typed
//! event bus and the mailbox, a line reader for consoles, and the two hal-level helpers small
//! enough to live here (a blinker, a debounced button).
//!
//! Nothing in this crate touches hardware. Everything it needs from the world comes through the
//! traits in [`hal`], which a port crate implements, plus one `critical_section` implementation
//! from the port -- which is what lets the same code run under `cargo test` on the host with a
//! mocked board. The host-first rule mk3 had, made structural.
//!
//! The rest of the framework is layered above: `light-draw` (the rasteriser), `light-display`
//! (the chunked display core, the frame layer and the panel drivers), `light-input` (touch,
//! gestures, the IMU), `light-ui` (the widget toolkit) and `light-midi` (the USB-MIDI
//! forwarder engine). Each depends on this crate; none depends on a port.

#![no_std]

pub mod blink;
pub mod button;
pub mod console;
pub mod events;
pub mod hal;
pub mod log;
pub mod mailbox;
pub mod module;

pub use blink::Blinker;
pub use console::LineReader;
pub use events::{EventBus, Subscription};
pub use hal::{Clock, I2cBus, I2cError, Idle, InputPin, OutputPin, SpiDisplayBus};
pub use mailbox::Mailbox;
pub use module::{Error, Module, Poll, Runtime};
