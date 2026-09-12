//! The dictaphone with a LANDSCAPE interface: the engine is `light_dictaphone_core`,
//! re-exported whole and identical to the portrait app's -- this crate owns only what the
//! interface looks like when the glass is held sideways. The main page runs LEFT TO RIGHT:
//! the record/stop button pinned at its text's width, then play-last and the recordings entry
//! sharing the rest. The recordings list is a horizontally scrolling strip of full-height
//! columns, with the back and paging buttons pinned outside it so a sideways drag scrolls the
//! strip rather than leaving the page.
//!
//! The interface is DATA: [`design.json`](../design.json) beside this file, compiled by crush to
//! an LUI blob and handed to the display as [`UiSource::Blob`](light_dictaphone_core::UiSource).
//! The scrolling strip is a FRAME (one-level nesting) holding the row leaves, with the pinned
//! buttons its siblings; `grow` gives it the surplus width and `max_w == min_w` fixes the pinned
//! buttons. Button `event` ids map to [`UiAction`](light_dictaphone_core::UiAction) through
//! [`ui_action`](light_dictaphone_core::ui_action) -- the same contract the portrait app uses
//! ([`ui_event`](light_dictaphone_core::ui_event)).
//!
//! Pair with [`DisplayConfig::initial_rotation`]` = Rotation::R270` and a landscape rotation map:
//! the interface starts sideways and follows the device between the two landscape poses.

#![no_std]

pub use light_dictaphone_core::*;
