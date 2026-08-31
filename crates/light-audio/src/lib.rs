//! Audio codecs behind [`light_core::hal::I2cBus`]. One part so far: the ES8311, the
//! mono DAC/ADC on the Waveshare RP2350-Touch-LCD-3.49. The I2S transport is a port
//! crate's business; this crate only configures the codec's registers.

#![no_std]

pub mod es8311;
pub use es8311::Es8311;
