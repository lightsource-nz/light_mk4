//! The port interface: what portable code asks of a board, and nothing more.
//!
//! Designed from the list of primitives the spike actually used rather than from what a HAL
//! happens to offer. The predecessor C framework's port surface grew to 4,500 lines across ten
//! port modules; this is the whole of it, and a port crate implements each trait once. The one primitive not here is
//! the critical section, which is the `critical-section` crate's `Impl`, supplied by the port.

/// Time, for the drivers that need to wait: init sequences and per-chunk deadlines.
pub trait Clock {
        /// Microseconds since boot. Monotonic.
        fn now_us(&self) -> u64;

        /// A blocking delay -- init-sequence territory only. Nothing polled from the runtime
        /// may call this: a touch driver doing so once stalled a drag for its whole 300 ms
        /// reset delay.
        fn delay_ms(&mut self, ms: u32) {
                let until = self.now_us() + ms as u64 * 1000;
                while self.now_us() < until {
                        core::hint::spin_loop();
                }
        }
}

/// What the runtime does between passes in which no module was busy.
pub trait Idle {
        /// Give the core a moment: a `wfe`, a `nop`, a host `yield`. Must return promptly --
        /// a poll-driven driver with a deadline is waiting.
        fn idle(&mut self);
}

/// A digital output.
pub trait OutputPin {
        fn set(&mut self, high: bool);
}

/// A synchronous-audio stream: a continuously running DAC output refilled by callback --
/// silence when the callback writes none -- and an optional capture path draining filled
/// buffers by callback. The port owns the buffers, their sizes and the transport (DMA
/// ping-pong, typically); the application owns only the samples.
///
/// Formats: the OUTPUT buffer is one `u32` word per frame -- the fill callback places a
/// 16-bit sample in both halves to play it on both slots. CAPTURE buffers are mono `u16`
/// samples in the machine's byte order.
pub trait AudioStream {
        /// Start the output stream. Called once, at the owning module's load.
        fn start(&mut self);

        /// Start (or resume) capture into the stream's own buffers.
        fn capture_start(&mut self);

        fn capture_stop(&mut self);

        /// Drain filled capture buffers, invoking `sink` once per buffer.
        fn capture_take(&mut self, sink: &mut dyn FnMut(&[u16]));

        /// Free words in the output PREFETCH ring right now. The producer tops the ring up
        /// toward full each poll; the transport drains it into its DMA buffers from an
        /// interrupt, so poll timing no longer bounds the audio -- only the ring's depth does.
        fn stream_free(&self) -> usize;

        /// Hand `fill` the ring's contiguous free region; it writes up to that many output
        /// words and returns how many, which the ring then commits. Call in a loop while
        /// [`stream_free`](Self::stream_free) is non-zero and there is data to play.
        fn stream_push(&mut self, fill: &mut dyn FnMut(&mut [u32]) -> usize);

        /// Whether the stream is meant to be producing sound: gates underrun accounting, so
        /// an idle ring draining to silence is not counted as starvation.
        fn set_active(&mut self, active: bool);

        /// Words still queued in the ring, not yet drained by the transport. Zero means the
        /// last sample has been handed to the DMA -- what tells a finishing track its tail
        /// has played before it stops.
        fn stream_pending(&self) -> usize;

        /// Discard everything queued in the ring at once, so the transport falls to silence
        /// immediately -- what makes a Stop stop now, rather than after the buffered lead.
        fn stream_clear(&mut self);

        /// Output buffers the transport played as silence for want of data, since the last
        /// reset -- genuine starvation, counted only while [`set_active`](Self::set_active).
        fn underruns(&self) -> u32;

        /// Capture buffers dropped for want of draining, since the last reset.
        fn cap_overruns(&self) -> u32;

        /// Zero both counters, so each reading covers the interval since the last.
        fn reset_stats(&mut self);

        /// A port-specific capture-path probe for bring-up diagnostics, logging whatever it
        /// finds; the default has nothing to say.
        fn debug_probe(&mut self) {}
}

/// A digital input, for interrupt/data-ready lines that are polled as levels.
pub trait InputPin {
        fn is_low(&self) -> bool;
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
///
/// The 16-bit-register operations exist for parts whose register addresses are two bytes
/// (big-endian on the wire) -- the CST328 touch controller is the first. They have default
/// implementations that answer [`I2cError::Bus`], so a bus that never meets such a part (and
/// every test fake) implements only the two byte-register operations.
pub trait I2cBus {
        /// Write `reg` under a held START, then read `out.len()` bytes with a STOP.
        fn read_register(&mut self, addr: u8, reg: u8, out: &mut [u8]) -> Result<(), I2cError>;

        /// `[reg, value]` as one transaction: S, addr+W, reg, value, P -- no repeated START.
        /// Some parts silently store nothing when the pair is split (the HUSB238 is one).
        fn write_register_byte(&mut self, addr: u8, reg: u8, value: u8) -> Result<(), I2cError>;

        /// Write the 16-bit `reg` big-endian under a held START, then read with a STOP.
        fn read_register16(&mut self, addr: u8, reg: u16, out: &mut [u8]) -> Result<(), I2cError> {
                let _ = (addr, reg, out);
                Err(I2cError::Bus)
        }

        /// The 16-bit register address alone, as a complete transaction WITH a STOP and no
        /// data byte: some parts (the CST328's mode switches) treat the bare address as a
        /// command.
        fn write_command16(&mut self, addr: u8, reg: u16) -> Result<(), I2cError> {
                let _ = (addr, reg);
                Err(I2cError::Bus)
        }

        /// Write `src` after the 16-bit `reg` (big-endian) as one transaction with a STOP --
        /// the GT911's status acknowledge is the first user. Default-implemented like the
        /// other 16-bit operations.
        fn write_register16(&mut self, addr: u8, reg: u16, src: &[u8]) -> Result<(), I2cError> {
                let _ = (addr, reg, src);
                Err(I2cError::Bus)
        }

        /// A raw write of `src` as one transaction with a STOP -- for parts whose protocol is
        /// a command blob rather than a register address (the AXS15231B's touch half sends an
        /// 11-byte command, then reads the answer). Default-implemented like the 16-bit
        /// operations, for the same reason.
        fn write_raw(&mut self, addr: u8, src: &[u8]) -> Result<(), I2cError> {
                let _ = (addr, src);
                Err(I2cError::Bus)
        }

        /// A raw read of `dst.len()` bytes with a STOP, no register address.
        fn read_raw(&mut self, addr: u8, dst: &mut [u8]) -> Result<(), I2cError> {
                let _ = (addr, dst);
                Err(I2cError::Bus)
        }
}

/// Two drivers on one bus -- the touch169's touch controller and IMU share I2C1 -- each take a
/// `&RefCell<bus>`. Both are polled from the same core, so the RefCell's exclusivity is enough,
/// and a driver holding the borrow across a call it does not make is impossible by
/// construction. Every transaction is one borrow.
impl<B: I2cBus> I2cBus for &core::cell::RefCell<B> {
        fn read_register(&mut self, addr: u8, reg: u8, out: &mut [u8]) -> Result<(), I2cError> {
                self.borrow_mut().read_register(addr, reg, out)
        }

        fn write_register_byte(&mut self, addr: u8, reg: u8, value: u8) -> Result<(), I2cError> {
                self.borrow_mut().write_register_byte(addr, reg, value)
        }

        fn read_register16(&mut self, addr: u8, reg: u16, out: &mut [u8]) -> Result<(), I2cError> {
                self.borrow_mut().read_register16(addr, reg, out)
        }

        fn write_command16(&mut self, addr: u8, reg: u16) -> Result<(), I2cError> {
                self.borrow_mut().write_command16(addr, reg)
        }

        fn write_register16(&mut self, addr: u8, reg: u16, src: &[u8]) -> Result<(), I2cError> {
                self.borrow_mut().write_register16(addr, reg, src)
        }

        fn write_raw(&mut self, addr: u8, src: &[u8]) -> Result<(), I2cError> {
                self.borrow_mut().write_raw(addr, src)
        }

        fn read_raw(&mut self, addr: u8, dst: &mut [u8]) -> Result<(), I2cError> {
                self.borrow_mut().read_raw(addr, dst)
        }
}

/// A full-duplex SPI master with no framing opinions: the caller owns chip select and
/// clocks bytes both ways. The SD card in SPI mode is the first consumer -- its protocol
/// interleaves command, response and data bytes under one held CS, which none of the
/// display-bus shapes can express.
pub trait SpiBus {
        /// Clock one byte out while clocking one in.
        fn transfer(&mut self, tx: u8) -> u8;

        /// Change the clock rate: SD initialization must run below 400 kHz, data runs at
        /// MHz. The achieved rate may be approximate.
        fn set_hz(&mut self, hz: u32);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
        /// The medium answered wrongly, or not at all.
        Io,
        /// The operation did not complete within its deadline.
        Timeout,
        /// The block index is beyond the medium.
        OutOfRange,
}

/// Block-addressed storage in 512-byte blocks -- the seam between a medium (an SD card,
/// a raw flash region, a test vector) and a filesystem. LBA addressing always; a medium
/// with other native addressing translates internally, the way the SD driver does for
/// byte-addressed cards.
pub trait BlockDevice {
        /// How many 512-byte blocks the medium holds.
        fn block_count(&self) -> u32;

        fn read_block(&mut self, lba: u32, out: &mut [u8; 512]) -> Result<(), BlockError>;

        fn write_block(&mut self, lba: u32, data: &[u8; 512]) -> Result<(), BlockError>;
}

//   a mutable borrow is a block device too, so a filesystem can be mounted OVER a device
// something else owns -- a board module mounting its card slot per command, say -- without
// giving the device up
impl<T: BlockDevice + ?Sized> BlockDevice for &mut T {
        fn block_count(&self) -> u32 {
                (**self).block_count()
        }

        fn read_block(&mut self, lba: u32, out: &mut [u8; 512]) -> Result<(), BlockError> {
                (**self).read_block(lba, out)
        }

        fn write_block(&mut self, lba: u32, data: &[u8; 512]) -> Result<(), BlockError> {
                (**self).write_block(lba, data)
        }
}

/// A QSPI display bus: four data lines, a clock, chip select -- and no D/C wire, so a
/// register write is ONE chip-select frame carrying a serial command header and its data,
/// which is why this is not [`SpiDisplayBus`] with more pins. The AXS15231B is the first
/// part: commands go out expanded onto D0 alone inside the 4-bit framing, pixel bursts open
/// with a header and then stream 4 bits per clock.
pub trait QspiDisplayBus {
        /// One register write, a single CS frame: command header, then `data`.
        fn write_register(&mut self, cmd: u8, data: &[u8]);

        /// Open a pixel burst: CS asserted, the pixel-write header sent for `cmd` (the
        /// panel's RAMWR-family command). Data follows through
        /// [`QspiDisplayBus::start_data`]; the frame stays open until
        /// [`QspiDisplayBus::is_complete`] answers true.
        fn begin_pixels(&mut self, cmd: u8);

        /// Stream data into the open pixel burst and return immediately.
        ///
        /// # Safety
        ///
        /// `bytes` is read asynchronously (by DMA) after this returns; the caller keeps it
        /// alive and unmodified until [`QspiDisplayBus::is_complete`] answers true -- the same
        /// contract as [`SpiDisplayBus::start_data`], upheld the same way by the display core.
        unsafe fn start_data(&mut self, bytes: &[u8]);

        /// Whether the transfer has fully left the wire. Closes the pixel frame (deasserts
        /// CS) the first time it answers true. True when nothing is in flight.
        fn is_complete(&mut self) -> bool;

        /// Pulse the panel's reset line. Blocking; init only.
        fn reset_pulse(&mut self, clock: &mut dyn Clock);
}
