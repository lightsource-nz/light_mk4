//! The framework's audio providers, transport-independent halves. Two so far: the ES8311
//! codec (register configuration behind [`light_core::hal::I2cBus`]; the I2S transport is
//! a port crate's business) and PWM audio for a passive piezo or buzzer ([`pwm`]: the
//! PCM-to-duty conversion; the carrier, pacing DMA and tone mode live in the port crate).

#![no_std]

pub mod es8311;
pub mod pwm;
pub use es8311::Es8311;
