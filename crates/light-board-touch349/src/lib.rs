//! The shared board port for the Waveshare RP2350-Touch-LCD-3.49. Every touch349 firmware builds
//! against this: [`board`] is the pin wiring and peripheral hand-over, and [`PowerManager`] is the
//! default power behaviour they all carry -- the screen dims after a spell without a touch, and on
//! battery (never on external power) the board powers itself off after a longer idle.
//!
//! The board module in each firmware owns a `PowerManager` and drives it: it routes user activity,
//! backlight commands and a busy flag in, and reads a [`Poll`](light_core::Poll) out that says when
//! to shut down. Everything the power behaviour needs -- the backlight PWM, the power latch, the
//! side button and the battery ADC -- lives here, so the behaviour is defined once.

#![no_std]

pub mod board;
mod power;

pub use power::PowerManager;
