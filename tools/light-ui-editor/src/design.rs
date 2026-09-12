//! The design data model, re-exported from crush-core (shared with crush's LUI compiler).
//!
//! The editor edits this model and compiles it to an LUI blob that the preview reads and displays
//! (see [`crate::preview`]) -- the same binary path firmware uses. There is no separate host
//! tree-building; the leak-based `materialize` this file once held is gone.

pub use crush_core::design::*;
