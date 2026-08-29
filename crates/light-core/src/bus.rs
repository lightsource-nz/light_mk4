//! The transport traits drivers are written against.
//!
//! This is mk3's `light_ioport` contract, reduced to what the drivers actually call and typed so
//! the rules that were comments there are signatures here. Drivers never see a chip register;
//! that boundary is why the same driver ran on RP2 and STM32 in mk3, and it is kept.

/// Time, for the drivers that need to wait: init sequences and per-chunk deadlines.
pub trait Clock {
        /// Microseconds since boot. Monotonic.
        fn now_us(&self) -> u64;

        /// A blocking delay -- init-sequence territory only. Nothing polled from the runtime
        /// may call this: that was mk3's 300 ms touch-reset stall in the middle of a drag.
        fn delay_ms(&mut self, ms: u32) {
                let until = self.now_us() + ms as u64 * 1000;
                while self.now_us() < until {
                        core::hint::spin_loop();
                }
        }
}

/// A 4-wire SPI display bus: SCK, MOSI, chip select and data/command, plus an optional reset
/// line. The bus frames every transaction with CS itself.
pub trait SpiDisplayBus {
        /// One command byte, D/C low, CS framed.
        fn command(&mut self, cmd: u8);

        /// Data bytes, D/C high, CS framed, blocking until the last bit has left the shift
        /// register.
        fn data(&mut self, bytes: &[u8]);

        /// Start a data transfer and return immediately. CS stays asserted until
        /// [`is_complete`](Self::is_complete) reports the transfer has landed.
        ///
        /// # Safety
        ///
        /// `bytes` is read asynchronously -- by DMA, typically -- after this returns. The caller
        /// must keep it alive and unmodified until `is_complete` returns `true`. The display
        /// core upholds this by owning the frame buffer and refusing mutable access while an
        /// update is in flight; a driver calling this directly takes on the same obligation.
        unsafe fn start_data(&mut self, bytes: &[u8]);

        /// Whether the transfer started by `start_data` has fully left the wire. Deasserts CS
        /// the first time it answers `true`. Answers `true` when nothing is in flight.
        fn is_complete(&mut self) -> bool;

        /// Pulse the reset line, if there is one: high, low, high, with the delays a controller
        /// needs. Blocking; init only.
        fn reset_pulse(&mut self, clock: &mut dyn Clock);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum I2cError {
        /// The address or a data byte was not acknowledged. A sleeping controller looks like
        /// this, so it is a state, not necessarily a fault.
        Nack,
        /// The transfer did not progress within its deadline.
        Timeout,
        /// The peripheral reported an abort for some other reason.
        Bus,
}

/// A 7-bit-address I2C master.
pub trait I2cBus {
        /// Write `reg` under a held START, then read `out.len()` bytes with a STOP.
        fn read_register(&mut self, addr: u8, reg: u8, out: &mut [u8]) -> Result<(), I2cError>;

        /// `[reg, value]` as one transaction: S, addr+W, reg, value, P -- no repeated START.
        /// Some parts silently store nothing when the pair is split (mk3's HUSB238 finding).
        fn write_register_byte(&mut self, addr: u8, reg: u8, value: u8) -> Result<(), I2cError>;
}

/// A digital input, for interrupt/data-ready lines that are polled as levels.
pub trait InputPin {
        fn is_low(&self) -> bool;
}

/// A digital output, for reset lines a driver drives itself.
pub trait OutputPin {
        fn set(&mut self, high: bool);
}
