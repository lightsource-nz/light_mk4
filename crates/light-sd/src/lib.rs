//! SD/TF cards behind [`light_core::hal::SpiBus`]: the SPI-mode block layer -- card
//! identification and 512-byte block reads. A filesystem is a separate decision; this
//! crate stops at "the card answers and blocks read back".

#![no_std]

pub mod spi_card;
pub use spi_card::{CardInfo, SdError, SpiSd};
