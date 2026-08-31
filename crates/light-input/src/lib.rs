//! Input: what mk3 split across `light_touch`, `light_imu` and their drivers. Gesture tracking
//! over a controller's samples (`touch`), the CST816T that produces them, the orientation model
//! (`imu`) and the QMI8658 behind it. Everything reaches hardware through
//! [`light_core::hal`]; the drivers' cadence rules were re-checked against mk4's loop rate
//! rather than copied, which is where the CST816T's minimum read gap came from.

#![no_std]

pub mod axs15231b;
pub mod cst328;
pub mod cst816t;
pub mod gt911;
pub mod imu;
pub mod qmi8658;
pub mod touch;

pub use touch::{Gesture, Swipe, Tracker};
