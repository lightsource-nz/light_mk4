//! The display stack: the chunked display core (`display`), the frame layer that paces,
//! double-buffers and region-flushes above it (`frames`), and the drivers for the panels that
//! have lit under this code. A driver sees a [`light_core::hal::SpiDisplayBus`] and nothing
//! about the chip behind it -- the rule that lets the same driver run on the RP2350 and the H7.

#![no_std]

pub mod display;
pub mod frames;
pub mod sh1107;
pub mod st7735;
pub mod st7789;

pub use display::{Display, DisplayDriver, Frame, Region, UpdateError};
pub use frames::{FrameLayer, LogicalRegion};
