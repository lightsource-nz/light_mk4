//! Real-time clocks behind [`light_core::hal::I2cBus`]. One part so far: the PCF85063A,
//! the battery-backed RTC on the Waveshare RP2350-Touch-LCD-3.49.

#![no_std]

pub mod pcf85063a;
pub use pcf85063a::{Datetime, Pcf85063a};
