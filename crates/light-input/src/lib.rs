//! Input: what the predecessor C framework split across separate touch and IMU modules. Gesture tracking
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

/// One raw touch sample from any controller: the shared shape every touch driver produces (each
/// re-exports it), so gesture tracking and the [`BoardEvent`] contract are controller-independent.
pub use cst816t::Event as TouchSample;

/// The contract a board's input modules need from an application's bus-event type: how to raise the
/// events a touch panel and IMU produce, and how to recognise the requests those modules react to.
/// An application implements this for its event enum, so a board's generic touch/IMU modules (the
/// same wiring on every app) publish onto its bus without naming the app.
pub trait BoardEvent: Copy {
        /// The event a raw touch sample raises.
        fn touch(sample: TouchSample) -> Self;
        /// The event a settled swipe gesture raises.
        fn gesture(gesture: Gesture) -> Self;
        /// The event a settled orientation raises.
        fn orientation(orientation: imu::Orientation) -> Self;
        /// Whether this event is the `stats` request, on which an input module logs its diagnostics.
        fn is_stats(&self) -> bool;
        /// Whether this event reports that a drag was consumed by scrolling -- the touch tracker then
        /// suppresses the release's swipe classification. Defaults to never.
        fn drag_consumed(&self) -> bool {
                false
        }
}
