//! Inertial sensing: axis mapping into the device frame, orientation from gravity, and the
//! polling throttle -- mk3's `light_imu`, ported.
//!
//! Readings are in engineering units (milli-g, milli-degrees per second), integer throughout.
//! The chip reports its own axes; the BOARD's axis map rotates them into the device frame --
//! +X right across the display, +Y up it, +Z out toward the viewer -- because a chip soldered
//! down rotated has no fixed relationship to the glass, and orientation codes are defined
//! against the glass.

use light_core::hal::I2cError;

pub const AXES: usize = 3;
pub const X: usize = 0;
pub const Y: usize = 1;
pub const Z: usize = 2;

/// How the chip's axes are mounted relative to the device frame: `source[i]` names the chip
/// axis that supplies device axis `i`, `sign[i]` negates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisMap {
        pub source: [usize; AXES],
        pub sign: [i32; AXES],
}

impl AxisMap {
        pub const IDENTITY: AxisMap = AxisMap { source: [X, Y, Z], sign: [1, 1, 1] };

        fn apply(&self, v: [i32; AXES]) -> [i32; AXES] {
                let mut out = [0; AXES];
                for i in 0..AXES {
                        let src = if self.source[i] < AXES { self.source[i] } else { i };
                        out[i] = v[src] * self.sign[i];
                }
                out
        }
}

/// Which way the device is held. "Portrait" means the board's +Y points up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Orientation {
        #[default]
        Unknown,
        Portrait,
        PortraitFlip,
        LandscapeL,
        LandscapeR,
        FaceUp,
        FaceDown,
}

/// A chip-frame sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
        pub accel_mg: [i32; AXES],
        pub gyro_mdps: [i32; AXES],
        pub temperature_mc: i32,
}

pub trait ImuDriver {
        /// A new sample if the sensor has one, `None` if nothing new, `Err` on a bus failure.
        fn sample(&mut self) -> Result<Option<Sample>, I2cError>;
        /// How often the chip can produce a sample; the core polls no faster.
        fn sample_interval_ms(&self) -> u32;
}

/// How long a new orientation must hold before adoption, and by how much its axis must beat
/// both others. Without the margin a board near 45 degrees flaps every sample; without the
/// hold a single knock re-orients the UI.
pub const ORIENT_HOLD_MS_DEFAULT: u32 = 250;
pub const ORIENT_MARGIN_MG_DEFAULT: i32 = 200;

pub struct Imu<D: ImuDriver> {
        driver: D,
        axis_map: AxisMap,
        /// In the device frame.
        pub accel_mg: [i32; AXES],
        pub gyro_mdps: [i32; AXES],
        pub temperature_mc: i32,
        poll_interval_ms: u32,
        last_poll_ms: u32,
        /// The first poll always samples; the throttle only applies between samples.
        polled: bool,
        pub orientation: Orientation,
        candidate: Orientation,
        candidate_ms: u32,
        pending: Option<Orientation>,
        hold_ms: u32,
        margin_mg: i32,
        pub failures: u32,
}

impl<D: ImuDriver> Imu<D> {
        pub fn new(driver: D) -> Self {
                let poll_interval_ms = driver.sample_interval_ms();
                Self {
                        driver,
                        axis_map: AxisMap::IDENTITY,
                        accel_mg: [0; AXES],
                        gyro_mdps: [0; AXES],
                        temperature_mc: 0,
                        poll_interval_ms,
                        last_poll_ms: 0,
                        polled: false,
                        orientation: Orientation::Unknown,
                        candidate: Orientation::Unknown,
                        candidate_ms: 0,
                        pending: None,
                        hold_ms: ORIENT_HOLD_MS_DEFAULT,
                        margin_mg: ORIENT_MARGIN_MG_DEFAULT,
                        failures: 0,
                }
        }

        pub fn driver(&mut self) -> &mut D {
                &mut self.driver
        }

        /// The board's mounting: set once, before anything reads a sample. The settled
        /// orientation was classified in the old frame, so it is forgotten.
        pub fn set_axis_map(&mut self, map: AxisMap) {
                self.axis_map = map;
                self.orientation = Orientation::Unknown;
                self.candidate = Orientation::Unknown;
        }

        pub fn set_poll_interval(&mut self, ms: u32) {
                self.poll_interval_ms = ms;
        }

        pub fn set_orientation_thresholds(&mut self, hold_ms: u32, margin_mg: i32) {
                self.hold_ms = hold_ms;
                self.margin_mg = margin_mg;
        }

        /// Sample if it is time to, and advance orientation tracking. Returns `true` when a
        /// new sample arrived. Every call in between costs nothing on the bus.
        pub fn poll(&mut self, now_ms: u32) -> bool {
                if self.polled && self.poll_interval_ms != 0 && now_ms.wrapping_sub(self.last_poll_ms) < self.poll_interval_ms {
                        return false;
                }
                self.polled = true;
                self.last_poll_ms = now_ms;
                let sample = match self.driver.sample() {
                        Ok(Some(s)) => s,
                        Ok(None) => return false,
                        Err(_) => {
                                self.failures = self.failures.wrapping_add(1);
                                return false;
                        }
                };
                self.accel_mg = self.axis_map.apply(sample.accel_mg);
                self.gyro_mdps = self.axis_map.apply(sample.gyro_mdps);
                self.temperature_mc = sample.temperature_mc;
                self.track_orientation(now_ms);
                true
        }

        /// The gravity vector alone: the dominant axis must beat BOTH others by the margin. The
        /// gyro is ignored on purpose -- integrated rate drifts, gravity never does.
        fn classify(&self) -> Orientation {
                let [x, y, z] = self.accel_mg;
                let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
                let m = self.margin_mg;
                if ax >= ay + m && ax >= az + m {
                        return if x > 0 { Orientation::LandscapeR } else { Orientation::LandscapeL };
                }
                if ay >= ax + m && ay >= az + m {
                        return if y > 0 { Orientation::Portrait } else { Orientation::PortraitFlip };
                }
                if az >= ax + m && az >= ay + m {
                        return if z > 0 { Orientation::FaceUp } else { Orientation::FaceDown };
                }
                // nothing dominant: hold what was settled rather than blink out
                self.orientation
        }

        fn track_orientation(&mut self, now_ms: u32) {
                let observed = self.classify();
                if observed != self.candidate {
                        self.candidate = observed;
                        self.candidate_ms = now_ms;
                        return;
                }
                if observed == self.orientation {
                        return;
                }
                if now_ms.wrapping_sub(self.candidate_ms) < self.hold_ms {
                        return;
                }
                self.orientation = observed;
                self.pending = Some(observed);
        }

        /// A pending orientation CHANGE, once. Read `orientation` for the settled value.
        pub fn take_orientation(&mut self) -> Option<Orientation> {
                self.pending.take()
        }
}

/// Raw signed 16-bit count into engineering units for a full-scale range. The 64-bit
/// intermediate is load-bearing (32767 * 2048000 overflows i32), and it is a real divide, not
/// a shift: a shift floors negatives and biases every negative reading a count low.
pub fn scale_sample(raw: i16, full_scale_units: u32) -> i32 {
        ((i64::from(raw) * i64::from(full_scale_units)) / 32768) as i32
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::Cell;

        struct Mock<'a> {
                accel: &'a Cell<[i32; 3]>,
                reads: &'a Cell<u32>,
        }
        impl ImuDriver for Mock<'_> {
                fn sample(&mut self) -> Result<Option<Sample>, I2cError> {
                        self.reads.set(self.reads.get() + 1);
                        Ok(Some(Sample { accel_mg: self.accel.get(), ..Default::default() }))
                }
                fn sample_interval_ms(&self) -> u32 {
                        10
                }
        }

        #[test]
        fn polling_is_throttled_to_the_sensor_rate() {
                let accel = Cell::new([0, 1000, 0]);
                let reads = Cell::new(0);
                let mut imu = Imu::new(Mock { accel: &accel, reads: &reads });
                for t in 0..100 {
                        imu.poll(t);
                }
                assert_eq!(reads.get(), 10);
        }

        #[test]
        fn orientation_needs_a_clear_axis_held_for_the_window_and_reports_changes_once() {
                let accel = Cell::new([0, 1000, 0]);
                let reads = Cell::new(0);
                let mut imu = Imu::new(Mock { accel: &accel, reads: &reads });
                let mut t = 0;
                let settle = |imu: &mut Imu<Mock>, t: &mut u32| {
                        for _ in 0..30 {
                                imu.poll(*t);
                                *t += 10;
                        }
                };
                settle(&mut imu, &mut t);
                assert_eq!(imu.orientation, Orientation::Portrait);
                assert_eq!(imu.take_orientation(), Some(Orientation::Portrait));
                assert_eq!(imu.take_orientation(), None);
                //   a knock: one sample of a different orientation does not re-orient
                accel.set([1000, 0, 0]);
                imu.poll(t);
                t += 10;
                accel.set([0, 1000, 0]);
                settle(&mut imu, &mut t);
                assert_eq!(imu.orientation, Orientation::Portrait);
                assert_eq!(imu.take_orientation(), None);
                //   near 45 degrees, nothing dominant: hold the settled orientation
                accel.set([700, 700, 0]);
                settle(&mut imu, &mut t);
                assert_eq!(imu.orientation, Orientation::Portrait);
                //   a real turn
                accel.set([-1000, 0, 0]);
                settle(&mut imu, &mut t);
                assert_eq!(imu.take_orientation(), Some(Orientation::LandscapeL));
                accel.set([0, 0, -1000]);
                settle(&mut imu, &mut t);
                assert_eq!(imu.take_orientation(), Some(Orientation::FaceDown));
        }

        #[test]
        fn the_axis_map_rotates_the_chip_into_the_device_frame() {
                let accel = Cell::new([1000, 0, 0]); // chip +X reads gravity
                let reads = Cell::new(0);
                let mut imu = Imu::new(Mock { accel: &accel, reads: &reads });
                // the touch169's mounting: device X from chip Y, device Y from chip X, Z inverted
                imu.set_axis_map(AxisMap { source: [Y, X, Z], sign: [1, 1, -1] });
                imu.poll(0);
                assert_eq!(imu.accel_mg, [0, 1000, 0], "chip +X is up the screen on that board");
                imu.poll(20);
                assert_eq!(imu.classify(), Orientation::Portrait);
        }

        #[test]
        fn scaling_is_symmetric_about_zero_and_does_not_overflow() {
                assert_eq!(scale_sample(32767, 8000), 7999);
                assert_eq!(scale_sample(-32768, 8000), -8000);
                assert_eq!(scale_sample(-1, 8000), 0, "truncates toward zero, not floor");
                assert_eq!(scale_sample(32767, 2_048_000), 2_047_937);
        }
}
