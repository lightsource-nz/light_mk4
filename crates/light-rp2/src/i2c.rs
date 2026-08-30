//! I2C1 master. Register-level port of pico-sdk's `hardware_i2c` init and blocking transfers,
//! with the per-byte timeout mk3's ioport layered on top and the one fix it found necessary:
//! a held-START write that fails must clear the "restart next" flag, or the failure leaks
//! into the next device on the bus.

use light_core::{I2cBus, I2cError};
use crate::pac;

use crate::gpio;

/// TX FIFO depth on this block.
const TX_FIFO_DEPTH: u32 = 16;
/// Per-transfer deadline: a base plus a per-byte allowance, the figures mk3 settled on.
const TIMEOUT_BASE_US: u64 = 2000;
const TIMEOUT_PER_BYTE_US: u64 = 100;

// IC_DATA_CMD bits
const DATA_CMD_RESTART: u32 = 1 << 10;
const DATA_CMD_STOP: u32 = 1 << 9;
const DATA_CMD_READ: u32 = 1 << 8;
// IC_TX_ABRT_SOURCE bits
const ABRT_7B_ADDR_NOACK: u32 = 1 << 0;
const ABRT_TXDATA_NOACK: u32 = 1 << 3;

pub struct I2c1 {
        restart_on_next: bool,
        /// clk_sys, from the shell that configured it: the I2C block is clocked from it.
        clk_sys_hz: u32,
        pub actual_hz: u32,
}

impl I2c1 {
        /// # Safety
        ///
        /// Takes I2C1, which nothing else may use while this lives. Construct once.
        pub unsafe fn new(clk_sys_hz: u32, scl: usize, sda: usize, hz: u32) -> Self {
                gpio::set_function(scl, gpio::FUNC_I2C);
                gpio::set_function(sda, gpio::FUNC_I2C);
                // internal pull-ups, for breakouts that carry none; harmless where the board has
                // its own
                gpio::set_pull_up(scl);
                gpio::set_pull_up(sda);

                let resets = unsafe { &*pac::RESETS::ptr() };
                resets.reset().modify(|_, w| w.i2c1().set_bit());
                resets.reset().modify(|_, w| w.i2c1().clear_bit());
                while resets.reset_done().read().i2c1().bit_is_clear() {}

                let i2c = unsafe { &*pac::I2C1::ptr() };
                i2c.ic_enable().write(|w| unsafe { w.bits(0) });
                // fast-mode master with repeated-START support, 7-bit addresses, TX_EMPTY when
                // the shift register is done rather than when the FIFO is
                i2c.ic_con().write(|w| {
                        w.speed().fast().master_mode().set_bit().ic_slave_disable().set_bit().ic_restart_en().set_bit().tx_empty_ctrl().set_bit()
                });
                i2c.ic_tx_tl().write(|w| unsafe { w.bits(0) });
                i2c.ic_rx_tl().write(|w| unsafe { w.bits(0) });
                i2c.ic_dma_cr().write(|w| unsafe { w.bits(3) });

                let mut bus = Self { restart_on_next: false, clk_sys_hz, actual_hz: 0 };
                bus.set_baudrate(hz);
                bus
        }

        pub fn set_baudrate(&mut self, hz: u32) -> u32 {
                let i2c = unsafe { &*pac::I2C1::ptr() };
                let freq_in = self.clk_sys_hz;
                let period = (freq_in + hz / 2) / hz;
                let lcnt = period * 3 / 5;
                let hcnt = period - lcnt;
                // 300 ns SDA hold below 1 MHz, 120 ns above
                let hold = if hz < 1_000_000 { freq_in * 3 / 10_000_000 + 1 } else { freq_in * 3 / 25_000_000 + 1 };
                i2c.ic_enable().write(|w| unsafe { w.bits(0) });
                i2c.ic_fs_scl_hcnt().write(|w| unsafe { w.bits(hcnt) });
                i2c.ic_fs_scl_lcnt().write(|w| unsafe { w.bits(lcnt) });
                i2c.ic_fs_spklen().write(|w| unsafe { w.bits(if lcnt < 16 { 1 } else { lcnt / 16 }) });
                i2c.ic_sda_hold().write(|w| unsafe { w.bits(hold) });
                i2c.ic_enable().write(|w| unsafe { w.bits(1) });
                self.actual_hz = freq_in / period;
                self.actual_hz
        }

        fn set_target(addr: u8) {
                let i2c = unsafe { &*pac::I2C1::ptr() };
                i2c.ic_enable().write(|w| unsafe { w.bits(0) });
                i2c.ic_tar().write(|w| unsafe { w.bits(u32::from(addr)) });
                i2c.ic_enable().write(|w| unsafe { w.bits(1) });
        }

        /// Abort whatever the block is doing and clear its FIFOs, for a transfer that timed out
        /// mid-flight: left alone, the next transfer starts behind a half-finished one and fails
        /// for a different reason -- the Timeout-then-Bus sequence seen on the touch169 when the
        /// controller stopped answering under a finger.
        fn abort_transfer() {
                let i2c = unsafe { &*pac::I2C1::ptr() };
                i2c.ic_enable().modify(|_, w| w.abort().set_bit());
                let deadline = crate::now_us() + TIMEOUT_BASE_US;
                while i2c.ic_enable().read().abort().bit_is_set() {
                        if crate::now_us() > deadline {
                                break;
                        }
                }
                let _ = i2c.ic_clr_tx_abrt().read();
                let _ = i2c.ic_clr_stop_det().read();
        }

        /// Re-armed per byte, as pico-sdk's per-iteration timeout is: the deadline bounds how
        /// long ONE byte may take, not the whole transfer, so a controller stretching the clock
        /// on wake gets the full allowance for every byte
        fn deadline(len: usize) -> u64 {
                crate::now_us() + TIMEOUT_BASE_US + len as u64 * TIMEOUT_PER_BYTE_US
        }

        fn write(&mut self, addr: u8, src: &[u8], nostop: bool) -> Result<(), I2cError> {
                let i2c = unsafe { &*pac::I2C1::ptr() };
                Self::set_target(addr);
                let mut result = Ok(());
                for (i, &b) in src.iter().enumerate() {
                        let deadline = Self::deadline(1);
                        let first = i == 0;
                        let last = i == src.len() - 1;
                        let cmd = u32::from(b)
                                | if first && self.restart_on_next { DATA_CMD_RESTART } else { 0 }
                                | if last && !nostop { DATA_CMD_STOP } else { 0 };
                        i2c.ic_data_cmd().write(|w| unsafe { w.bits(cmd) });
                        // wait for the byte to leave the shift register (TX_EMPTY_CTRL)
                        let mut timed_out = false;
                        while i2c.ic_raw_intr_stat().read().tx_empty().bit_is_clear() {
                                if crate::now_us() > deadline {
                                        timed_out = true;
                                        break;
                                }
                        }
                        if timed_out {
                                result = Err(I2cError::Timeout);
                                break;
                        }
                        let abort = i2c.ic_tx_abrt_source().read().bits();
                        if abort != 0 {
                                let _ = i2c.ic_clr_tx_abrt().read();
                                result = Err(if abort & (ABRT_7B_ADDR_NOACK | ABRT_TXDATA_NOACK) != 0 { I2cError::Nack } else { I2cError::Bus });
                        }
                        if result.is_err() || (last && !nostop) {
                                // the hardware issues a STOP on abort too; wait for it
                                while i2c.ic_raw_intr_stat().read().stop_det().bit_is_clear() {
                                        if crate::now_us() > deadline {
                                                break;
                                        }
                                }
                                let _ = i2c.ic_clr_stop_det().read();
                        }
                        if result.is_err() {
                                break;
                        }
                }
                if result == Err(I2cError::Timeout) {
                        Self::abort_transfer();
                }
                //   a held START that FAILED must not leave the flag set: the next transfer on
                // this peripheral may be another device's, and it would open with a spurious
                // RESTART on an idle bus -- one device's nap taking down its bus-mate
                self.restart_on_next = nostop && result.is_ok();
                result
        }

        fn read(&mut self, addr: u8, dst: &mut [u8], nostop: bool) -> Result<(), I2cError> {
                let i2c = unsafe { &*pac::I2C1::ptr() };
                Self::set_target(addr);
                let mut result = Ok(());
                let len = dst.len();
                for (i, out) in dst.iter_mut().enumerate() {
                        let deadline = Self::deadline(1);
                        let first = i == 0;
                        let last = i == len - 1;
                        while TX_FIFO_DEPTH - i2c.ic_txflr().read().bits() == 0 {
                                if crate::now_us() > deadline {
                                        break;
                                }
                        }
                        let cmd = DATA_CMD_READ
                                | if first && self.restart_on_next { DATA_CMD_RESTART } else { 0 }
                                | if last && !nostop { DATA_CMD_STOP } else { 0 };
                        i2c.ic_data_cmd().write(|w| unsafe { w.bits(cmd) });
                        loop {
                                let abort = i2c.ic_tx_abrt_source().read().bits();
                                if i2c.ic_raw_intr_stat().read().tx_abrt().bit_is_set() {
                                        let _ = i2c.ic_clr_tx_abrt().read();
                                        result = Err(if abort == 0 || abort & ABRT_7B_ADDR_NOACK != 0 { I2cError::Nack } else { I2cError::Bus });
                                        break;
                                }
                                if i2c.ic_rxflr().read().bits() > 0 {
                                        break;
                                }
                                if crate::now_us() > deadline {
                                        result = Err(I2cError::Timeout);
                                        break;
                                }
                        }
                        if result.is_err() {
                                break;
                        }
                        *out = i2c.ic_data_cmd().read().bits() as u8;
                }
                if result == Err(I2cError::Timeout) {
                        Self::abort_transfer();
                }
                self.restart_on_next = nostop && result.is_ok();
                result
        }
}

impl I2cBus for I2c1 {
        fn read_register(&mut self, addr: u8, reg: u8, out: &mut [u8]) -> Result<(), I2cError> {
                self.write(addr, &[reg], true)?;
                self.read(addr, out, false)
        }

        fn write_register_byte(&mut self, addr: u8, reg: u8, value: u8) -> Result<(), I2cError> {
                self.write(addr, &[reg, value], false)
        }
}
