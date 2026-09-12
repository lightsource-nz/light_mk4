//! The pure asset-compilation core shared by crush and the host tools.
//!
//! crush the binary is CLI, context and logging around two jobs: turning a TrueType face into an
//! LGF bitmap font, and turning a theme JSON into an LTH blob. Those two jobs are pure -- bytes in,
//! bytes out, no files or logging -- and are also exactly what a host tool wants (the light-ui
//! editor rasterises its preview font and compiles a theme the same way). They live here so there
//! is one implementation, not a mirrored copy per consumer.

pub mod design;
pub mod lui;
pub mod render;
pub mod theme;
