//! The touch and IMU runtime modules every touch349 app carries. The AXS15231B's touch half and the
//! QMI8658 IMU behave the same whatever the app is -- they read the hardware and publish the events a
//! touch panel and IMU raise -- so they live here, generic over the app's bus-event type through
//! [`light_input::BoardEvent`]. An app hands each its driver and its bus; the module names no app.

use core::cell::RefCell;

use light_core::{debug, info, warn, Bus, Module, Poll, Subscription};
use light_input::axs15231b::{self as axs, Axs15231bTouch};
use light_input::imu::Imu;
use light_input::qmi8658::Qmi8658;
use light_input::{BoardEvent, Tracker};
use light_rp2::gpio::Input;
use light_rp2::i2c::{I2c0, I2c1};

use crate::board::IMU_AXIS_MAP;

/// Owns the AXS15231B's touch half: no reset line of its own (the panel's reset is the chip's), so a
/// wedge is reported, never reset from here. Reads samples, publishes touch and gesture events, and
/// reports its diagnostics on `stats`.
pub struct TouchMod<A: BoardEvent + core::fmt::Debug + Send + 'static> {
        touch: Axs15231bTouch<I2c0, Input>,
        tracker: Tracker,
        bus: &'static dyn Bus<A>,
        sub: Subscription,
        //   the push-versus-touch bisect: whether touch reads should hold while the panel is being
        // pushed. The app owns the state (a console command toggles it); the module just asks.
        reads_held: fn() -> bool,
        moves: u32,
}

impl<A: BoardEvent + core::fmt::Debug + Send + 'static> TouchMod<A> {
        /// Wire the touch half to an app's `bus`. `reads_held` is the app's push/touch bisect gate.
        pub fn new(touch: Axs15231bTouch<I2c0, Input>, tracker: Tracker, bus: &'static dyn Bus<A>, reads_held: fn() -> bool) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { touch, tracker, bus, sub, reads_held, moves: 0 }
        }
}

impl<A: BoardEvent + core::fmt::Debug + Send + 'static> Module for TouchMod<A> {
        fn name(&self) -> &'static str {
                "touch"
        }
        fn load(&mut self) -> Result<(), ()> {
                //   after the display module's load has reset and initialised the shared chip; the
                // touch read is the only probe this protocol offers
                let mut clock = light_rp2::SysClock;
                let mut result = self.touch.probe();
                for _ in 0..2 {
                        if result.is_ok() {
                                break;
                        }
                        light_core::hal::Clock::delay_ms(&mut clock, 20);
                        result = self.touch.probe();
                }
                match result {
                        Ok(()) => info!("axs15231b touch answering on i2c0"),
                        Err(e) => warn!("axs15231b touch did not answer the probe: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = self.bus.poll(&self.sub) {
                        if ev.is_stats() {
                                info!("touch: {} failed reads ({} nack, {} timeout, {} bus)", self.touch.failures, self.touch.nacks, self.touch.timeouts, self.touch.bus_errors);
                        } else if ev.drag_consumed() {
                                self.tracker.suppress();
                        }
                }
                if (self.reads_held)() {
                        return Poll::Idle;
                }
                let now_ms = (light_rp2::now_us() / 1000) as u32;
                let Some(ev) = self.touch.poll(now_ms) else { return Poll::Idle };
                match ev {
                        axs::Event::Down { x, y } => {
                                self.moves = 0;
                                debug!("touch down at {x},{y}");
                        }
                        axs::Event::Up => debug!("touch up after {} moves at {},{}", self.moves, self.touch.x, self.touch.y),
                        axs::Event::Move { .. } => self.moves += 1,
                        axs::Event::Reset => {}
                }
                if let Err(e) = self.bus.publish(A::touch(ev)) {
                        warn!("event bus full; dropped {e:?}");
                }
                if let Some(g) = self.tracker.feed(ev, Some(&mut self.touch)) {
                        let _ = self.bus.publish(A::gesture(g));
                }
                Poll::Busy
        }
}

/// Owns the QMI8658 IMU on i2c1: reads acceleration, settles it into an orientation, and publishes a
/// change. Reports its diagnostics on `stats`.
pub struct ImuMod<A: BoardEvent + Send + 'static> {
        imu: Imu<Qmi8658<&'static RefCell<I2c1>>>,
        bus: &'static dyn Bus<A>,
        sub: Subscription,
}

impl<A: BoardEvent + Send + 'static> ImuMod<A> {
        /// Wire the IMU to an app's `bus`.
        pub fn new(imu: Imu<Qmi8658<&'static RefCell<I2c1>>>, bus: &'static dyn Bus<A>) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { imu, bus, sub }
        }
}

impl<A: BoardEvent + Send + 'static> Module for ImuMod<A> {
        fn name(&self) -> &'static str {
                "imu"
        }
        fn load(&mut self) -> Result<(), ()> {
                match self.imu.driver().probe() {
                        Ok(Some(id)) => info!("qmi8658 chip id confirmed: 0x{id:02x}"),
                        Ok(None) => warn!("qmi8658 answered with an unexpected chip id"),
                        Err(e) => warn!("qmi8658 did not answer the chip id read: {e:?}"),
                }
                if let Err(e) = self.imu.driver().configure() {
                        warn!("qmi8658 configuration failed: {e:?}");
                }
                self.imu.set_axis_map(IMU_AXIS_MAP);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = self.bus.poll(&self.sub) {
                        if ev.is_stats() {
                                let a = self.imu.accel_mg;
                                info!("imu: accel {} {} {} mg, {:?}, {} failed reads, {}.{} C", a[0], a[1], a[2], self.imu.orientation, self.imu.failures, self.imu.temperature_mc / 1000, (self.imu.temperature_mc % 1000).abs() / 100);
                        }
                }
                let now_ms = (light_rp2::now_us() / 1000) as u32;
                if !self.imu.poll(now_ms) {
                        return Poll::Idle;
                }
                if let Some(o) = self.imu.take_orientation() {
                        info!("orientation: {o:?}");
                        let _ = self.bus.publish(A::orientation(o));
                }
                Poll::Busy
        }
}
