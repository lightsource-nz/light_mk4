//! A scanned panel: the framebuffer IS the display. An RGB/DPI panel has no GDDRAM and no
//! transfer to schedule -- a hardware engine (the RP2350's `light_rp2::rgb`) streams the
//! buffer to the glass continuously -- so the chunk model degenerates on purpose: every
//! region has zero chunks and every update completes the moment it starts. The frame layer
//! and UI above notice nothing; "pushing" a region costs a function call.
//!
//! Panel bring-up (the ST7701S's bit-banged init) and starting the scanout engine are the
//! BOARD's work, done before the [`crate::Display`] is constructed over the live buffer --
//! which is also why this driver's `init` has nothing to do.

use light_core::hal::Clock;
use crate::display::{DisplayDriver, Frame, Region};

pub struct Scanout;

impl DisplayDriver for Scanout {
        fn init(&mut self, _clock: &mut dyn Clock, _width: u16, _height: u16) {}

        fn chunk_count(&self, _region: &Region) -> u16 {
                // nothing to send: the engine is already reading the buffer
                0
        }

        fn chunks_per_poll(&self, _region: &Region) -> u16 {
                0
        }

        fn kick(&mut self, _frame: &Frame<'_>, _region: &Region, _index: u16) {
                unreachable!("a zero-chunk update never kicks");
        }

        fn chunk_complete(&mut self) -> bool {
                true
        }

        fn chunk_timeout_ms(&self) -> u32 {
                1
        }
}
