//! QST QMI8658C 6-axis IMU over I2C, ported from mk3's `light_imu_qmi8658`.
//!
//! PROVENANCE: the register map is cross-referenced from open-source drivers, not a primary
//! datasheet -- the same footing the CST816T driver started on, where the vertical gesture
//! codes turned out inverted. The WHO_AM_I read at init is the first real evidence either way;
//! mk3 confirmed this configuration and the touch169's axis map on hardware.

use crate::hal::{I2cBus, I2cError};
use crate::imu::{scale_sample, ImuDriver, Sample, AXES};

/// SA0 pulled high, as strapped on the RP2350-Touch-LCD-1.69; 0x6A with SA0 low.
pub const I2C_ADDR: u8 = 0x6B;
pub const CHIP_ID: u8 = 0x05;

const REG_WHO_AM_I: u8 = 0x00;
const REG_CTRL1: u8 = 0x02;
const REG_CTRL2: u8 = 0x03;
const REG_CTRL3: u8 = 0x04;
const REG_CTRL7: u8 = 0x08;
const REG_STATUS0: u8 = 0x2E;
const REG_TEMP_L: u8 = 0x33;
const REG_AX_L: u8 = 0x35;
const REG_GZ_H: u8 = 0x40;

/// Status, temperature and all six axes are contiguous: one transaction from STATUS0 through
/// GZ_H fetches the lot, with five unused bytes in the middle cheaper than a second round trip
/// on a bus shared with the touch controller.
const FRAME_BASE: u8 = REG_STATUS0;
const FRAME_LEN: usize = (REG_GZ_H - FRAME_BASE + 1) as usize;
const FRAME_STATUS0: usize = 0;
const FRAME_TEMP: usize = (REG_TEMP_L - FRAME_BASE) as usize;
const FRAME_ACCEL: usize = (REG_AX_L - FRAME_BASE) as usize;
const FRAME_GYRO: usize = FRAME_ACCEL + 6;
/// Die temperature: signed, 8 fractional bits.
const TEMP_COUNTS_PER_DEGREE: i32 = 256;

const CTRL1_ADDR_AUTO_INC: u8 = 1 << 6;
const CTRL7_ACCEL_EN: u8 = 1 << 0;
const CTRL7_GYRO_EN: u8 = 1 << 1;
const STATUS0_ACCEL_READY: u8 = 1 << 0;
const STATUS0_GYRO_READY: u8 = 1 << 1;
const ACCEL_FS_8G: u8 = 2 << 4;
const GYRO_FS_512DPS: u8 = 5 << 4;
/// ~94.5 Hz in the normal-mode table.
const ODR_NORMAL: u8 = 0x03;
/// Matched to the ODR, rounded down so a ready sample never waits a whole extra interval.
const POLL_INTERVAL_MS: u32 = 10;

pub const ACCEL_FS_MG: u32 = 8_000;
pub const GYRO_FS_MDPS: u32 = 512_000;

pub struct Qmi8658<B: I2cBus> {
        bus: B,
}

impl<B: I2cBus> Qmi8658<B> {
        pub fn new(bus: B) -> Self {
                Self { bus }
        }

        /// Read WHO_AM_I: `Ok(Some(id))` when it answered with the expected value, `Ok(None)`
        /// when it answered with another, `Err` when the bus did not answer.
        pub fn probe(&mut self) -> Result<Option<u8>, I2cError> {
                let mut id = [0u8];
                self.bus.read_register(I2C_ADDR, REG_WHO_AM_I, &mut id)?;
                Ok(if id[0] == CHIP_ID { Some(id[0]) } else { None })
        }

        /// Program the ranges and rates, then enable: the ranges must be in place before the
        /// sensors produce samples against them.
        pub fn configure(&mut self) -> Result<(), I2cError> {
                // address auto-increment is what makes the frame burst work at all
                self.bus.write_register_byte(I2C_ADDR, REG_CTRL1, CTRL1_ADDR_AUTO_INC)?;
                self.bus.write_register_byte(I2C_ADDR, REG_CTRL2, ACCEL_FS_8G | ODR_NORMAL)?;
                self.bus.write_register_byte(I2C_ADDR, REG_CTRL3, GYRO_FS_512DPS | ODR_NORMAL)?;
                self.bus.write_register_byte(I2C_ADDR, REG_CTRL7, CTRL7_ACCEL_EN | CTRL7_GYRO_EN)
        }
}

fn le16(p: &[u8]) -> i16 {
        i16::from_le_bytes([p[0], p[1]])
}

impl<B: I2cBus> ImuDriver for Qmi8658<B> {
        fn sample(&mut self) -> Result<Option<Sample>, I2cError> {
                let mut frame = [0u8; FRAME_LEN];
                self.bus.read_register(I2C_ADDR, FRAME_BASE, &mut frame)?;
                // the status byte came from the same transaction, so it describes this frame
                if frame[FRAME_STATUS0] & (STATUS0_ACCEL_READY | STATUS0_GYRO_READY) == 0 {
                        return Ok(None);
                }
                let mut s = Sample { temperature_mc: i32::from(le16(&frame[FRAME_TEMP..])) * 1000 / TEMP_COUNTS_PER_DEGREE, ..Default::default() };
                for axis in 0..AXES {
                        s.accel_mg[axis] = scale_sample(le16(&frame[FRAME_ACCEL + axis * 2..]), ACCEL_FS_MG);
                        s.gyro_mdps[axis] = scale_sample(le16(&frame[FRAME_GYRO + axis * 2..]), GYRO_FS_MDPS);
                }
                Ok(Some(s))
        }

        fn sample_interval_ms(&self) -> u32 {
                POLL_INTERVAL_MS
        }
}
