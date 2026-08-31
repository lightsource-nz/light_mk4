//! Sitronix ST7701S bring-up over its 9-bit control SPI, bit-banged: the panel is then fed
//! over the 16-bit RGB (DPI) interface by a scanout engine, and this bus is never touched
//! again. 9-bit frames -- a D/C bit ahead of each byte -- fit no hardware SPI block worth
//! configuring for a one-shot init, so four GPIOs and a delay do it.
//!
//! The init table is Waveshare's reference for the RP2350-Touch-LCD-4's panel, byte for
//! byte: the two-page register soup, IPS mode, RGB666 pixel format on the panel side (the
//! wire carries RGB565 into the top bits), then SLPOUT and DISPON -- which this panel,
//! unlike the AXS15231B, genuinely wants.

use light_core::hal::{Clock, OutputPin};

struct Cmd {
        reg: u8,
        data: &'static [u8],
        delay_ms: u32,
}

static INIT: &[Cmd] = &[
        Cmd { reg: 0xFF, data: &[0x77, 0x01, 0x00, 0x00, 0x10], delay_ms: 0 },
        Cmd { reg: 0xC0, data: &[0x3B, 0x00], delay_ms: 0 },
        Cmd { reg: 0xC1, data: &[0x0D, 0x02], delay_ms: 0 },
        Cmd { reg: 0xC2, data: &[0x31, 0x05], delay_ms: 0 },
        Cmd { reg: 0xCD, data: &[0x08], delay_ms: 0 },
        Cmd { reg: 0xB0, data: &[0x00, 0x11, 0x18, 0x0E, 0x11, 0x06, 0x07, 0x08, 0x07, 0x22, 0x04, 0x12, 0x0F, 0xAA, 0x31, 0x18], delay_ms: 0 },
        Cmd { reg: 0xB1, data: &[0x00, 0x11, 0x19, 0x0E, 0x12, 0x07, 0x08, 0x08, 0x08, 0x22, 0x04, 0x11, 0x11, 0xA9, 0x32, 0x18], delay_ms: 0 },
        Cmd { reg: 0xFF, data: &[0x77, 0x01, 0x00, 0x00, 0x11], delay_ms: 0 },
        Cmd { reg: 0xB0, data: &[0x60], delay_ms: 0 },
        Cmd { reg: 0xB1, data: &[0x32], delay_ms: 0 },
        Cmd { reg: 0xB2, data: &[0x07], delay_ms: 0 },
        Cmd { reg: 0xB3, data: &[0x80], delay_ms: 0 },
        Cmd { reg: 0xB5, data: &[0x49], delay_ms: 0 },
        Cmd { reg: 0xB7, data: &[0x85], delay_ms: 0 },
        Cmd { reg: 0xB8, data: &[0x21], delay_ms: 0 },
        Cmd { reg: 0xC1, data: &[0x78], delay_ms: 0 },
        Cmd { reg: 0xC2, data: &[0x78], delay_ms: 0 },
        Cmd { reg: 0xE0, data: &[0x00, 0x1B, 0x02], delay_ms: 0 },
        Cmd { reg: 0xE1, data: &[0x08, 0xA0, 0x00, 0x00, 0x07, 0xA0, 0x00, 0x00, 0x00, 0x44, 0x44], delay_ms: 0 },
        Cmd { reg: 0xE2, data: &[0x11, 0x11, 0x44, 0x44, 0xED, 0xA0, 0x00, 0x00, 0xEC, 0xA0, 0x00, 0x00], delay_ms: 0 },
        Cmd { reg: 0xE3, data: &[0x00, 0x00, 0x11, 0x11], delay_ms: 0 },
        Cmd { reg: 0xE4, data: &[0x44, 0x44], delay_ms: 0 },
        Cmd { reg: 0xE5, data: &[0x0A, 0xE9, 0xD8, 0xA0, 0x0C, 0xEB, 0xD8, 0xA0, 0x0E, 0xED, 0xD8, 0xA0, 0x10, 0xEF, 0xD8, 0xA0], delay_ms: 0 },
        Cmd { reg: 0xE6, data: &[0x00, 0x00, 0x11, 0x11], delay_ms: 0 },
        Cmd { reg: 0xE7, data: &[0x44, 0x44], delay_ms: 0 },
        Cmd { reg: 0xE8, data: &[0x09, 0xE8, 0xD8, 0xA0, 0x0B, 0xEA, 0xD8, 0xA0, 0x0D, 0xEC, 0xD8, 0xA0, 0x0F, 0xEE, 0xD8, 0xA0], delay_ms: 0 },
        Cmd { reg: 0xEB, data: &[0x02, 0x00, 0xE4, 0xE4, 0x88, 0x00, 0x40], delay_ms: 0 },
        Cmd { reg: 0xEC, data: &[0x3C, 0x00], delay_ms: 0 },
        Cmd { reg: 0xED, data: &[0xAB, 0x89, 0x76, 0x54, 0x02, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x20, 0x45, 0x67, 0x98, 0xBA], delay_ms: 0 },
        Cmd { reg: 0xFF, data: &[0x77, 0x01, 0x00, 0x00, 0x13], delay_ms: 0 },
        Cmd { reg: 0xE5, data: &[0xE4], delay_ms: 0 },
        Cmd { reg: 0xFF, data: &[0x77, 0x01, 0x00, 0x00, 0x00], delay_ms: 0 },
        // IPS inversion on, RGB666 interface format
        Cmd { reg: 0x21, data: &[], delay_ms: 0 },
        Cmd { reg: 0x3A, data: &[0x60], delay_ms: 0 },
        // sleep out, display on -- this panel wants both
        Cmd { reg: 0x11, data: &[], delay_ms: 120 },
        Cmd { reg: 0x29, data: &[], delay_ms: 120 },
];

/// Half a bit period. The reference uses 100 us; the part takes MHz-class clocks, and 5 us
/// keeps the whole table under 100 ms while staying far from any margin.
const HALF_BIT_US_AS_SPINS: u32 = 5;

fn delay_short(clock: &mut dyn Clock) {
        let until = clock.now_us() + u64::from(HALF_BIT_US_AS_SPINS);
        while clock.now_us() < until {
                core::hint::spin_loop();
        }
}

fn write9(sck: &mut dyn OutputPin, sda: &mut dyn OutputPin, clock: &mut dyn Clock, word: u16) {
        for i in (0..9).rev() {
                sda.set(word & (1 << i) != 0);
                delay_short(clock);
                sck.set(true);
                delay_short(clock);
                sck.set(false);
        }
}

/// Reset and run the whole init table. `cs`/`sck` idle as constructed (high/low); blocking,
/// init only.
pub fn init(cs: &mut dyn OutputPin, sck: &mut dyn OutputPin, sda: &mut dyn OutputPin, reset: &mut dyn OutputPin, clock: &mut dyn Clock) {
        reset.set(true);
        clock.delay_ms(20);
        reset.set(false);
        clock.delay_ms(20);
        reset.set(true);
        clock.delay_ms(200);

        for c in INIT {
                cs.set(false);
                // D/C bit 0: command
                write9(sck, sda, clock, u16::from(c.reg));
                for &b in c.data {
                        // D/C bit 1: data
                        write9(sck, sda, clock, 0x0100 | u16::from(b));
                }
                cs.set(true);
                if c.delay_ms > 0 {
                        clock.delay_ms(c.delay_ms);
                }
        }
}
