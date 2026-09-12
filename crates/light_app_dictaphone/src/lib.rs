//! The dictaphone with its PORTRAIT interface: the engine is `light_dictaphone_core`,
//! re-exported whole; this crate owns what the interface looks like -- a vertically stacked
//! main page (status line, one big record/stop button, play-last, the recordings entry) and a
//! scrolling recordings list. Held upright; the sibling `light_app_dictaphone_wide` lays the
//! same engine out for glass held sideways.
//!
//! The interface is DATA, not code: [`design.json`](../design.json) beside this file, compiled
//! by crush to an LUI blob (`light_mk4_add_ui` in the board module) and handed to the display as
//! [`UiSource::Blob`](light_dictaphone_core::UiSource). A button's `event` id maps to the app's
//! [`UiAction`](light_dictaphone_core::UiAction) through
//! [`ui_action`](light_dictaphone_core::ui_action); the ids and tags the design uses are the
//! contract documented in [`ui_event`](light_dictaphone_core::ui_event) (JSON carries no comments,
//! so the numbers live there). The flat scrolling list is what lets this interface be a blob --
//! the landscape one nests a scrolling strip and stays a const-`Page` tree for now.

#![no_std]

pub use light_dictaphone_core::*;
