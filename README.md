# light mk4 — spike

The feasibility spike for the next iteration of the light framework, per the 2026-08-29
assessment: **Rust framework code linked as a `no_std` staticlib into a firmware executable that
pico-sdk's CMake still owns.** The C shell keeps crt0, boot2, the linker script, multicore launch,
PIO and TinyUSB; Rust owns everything above the runtime. CMake stays the outer build driver so the
shared `light-*.ps1` script layer keeps working unchanged.

This is a spike, not the framework. It exists to retire specific unknowns before any commitment:

1. Can a Rust staticlib be linked into a pico-sdk executable through Corrosion, driven by the
   existing presets and scripts? (milestone 1: this repo blinks the touch169 backlight from Rust)
2. Can Rust drive peripherals through `rp235x-pac` while pico-sdk owns the runtime, and how often
   is a C shim needed instead?
3. Does the host-first test story hold — `cargo test` on the same crates with a mocked board?
4. Later milestones: explicit module registration, a bounded log queue that never blocks, the
   ST7789 async chunk protocol over SPI+DMA, CST816T touch, TinyUSB CDC console crossing into Rust.

## Layout

    Cargo.toml              workspace
    crates/light-core       portable, no_std, host-testable framework code
    crates/light-rp2350     RP2350 board/peripheral access via rp235x-pac
    module/light_mk4_touch169/rust   the staticlib crate the firmware links (light_app_touch169)
    module/light_mk4_touch169        the C shell: main.c + pico-sdk executable (module/<target>/
                                     is where light-flash.ps1 looks for <target>.uf2)
    scripts/                the usual thin wrappers over $LIGHT_PATH/scripts

## Building

    scripts/build.ps1                 # default target, touch169
    scripts/flash.ps1                 # UF2 over BOOTSEL
    scripts/test.ps1                  # host tree; runs `cargo test` on the portable crates

Needs `rustup` with `thumbv8m.main-none-eabi` (soft-float ABI, to match pico-sdk's `-mfloat-abi=softfp`
on Cortex-M33 — NOT `eabihf`, which fails at link with a VFP-args mismatch) and the usual mk3
toolchain environment. `scripts/light-tools.ps1` puts `~/.cargo/bin` on PATH.

On Windows the Rust **host** toolchain must be the MSVC one (`stable-x86_64-pc-windows-msvc`).
The gnu host fails under `light-env.ps1`: w64devkit's gcc is first on PATH and has no
`libgcc_eh.a`, so the build scripts of the proc-macro and pac crates cannot link — and cargo
does not apply target rustflags to build scripts under `--target`, so no config.toml fixes it.

## Log

- 2026-08-29 — milestone 1 builds: `light_mk4_touch169.uf2`, 44.5 KB text, Rust entry point
  driving GPIO25 through the pac, host tests passing (one caught a scheduling bug in the blinker
  before it reached hardware).
- 2026-08-29 — **milestone 1 hardware-verified** on the touch169: `scripts/flash.ps1` over
  BOOTSEL, console shows `shell up, entering rust` then `rust: up, blinking backlight on GPIO25`
  and a toggle count advancing at the expected 2 Hz. Zero C shims for peripherals; the FFI
  surface is three functions (`light_app_main` in, `light_shell_panic` / `light_shell_log` out).
- 2026-08-29 — **milestone 2 hardware-verified**: explicit module registry + poll runtime
  (`light_core::module`), bounded never-blocking log queue (`light_core::log`), and a
  nesting-safe RP2350 critical section (PRIMASK + SIO spinlock 31) in Rust. 15 host tests.
  Still zero C shims for peripherals.
- 2026-08-29 — **milestone 3 hardware-verified**: ST7789 over SPI1 + DMA through the chunk
  protocol (`light_core::display`, host-tested against the three bugs that shaped it), CST816T
  over I2C1, all register-level through the pac (`light-rp2350::{spi,i2c,gpio}`). A red square
  bounces at 30 fps with region updates, zero chunk timeouts at 37.5 MHz; taps land with
  coordinates and move it. 25 host tests. The one wrinkle found: the touch controller
  auto-sleeps within a second of its reset, so the probe must follow the pulse immediately.
  Still zero C shims; FFI surface unchanged at three functions.
- 2026-08-29 — **touch wedge = mk3 parity, still open.** Under steady tapping the CST816T
  stops answering (I2C timeouts, then bus errors, INT still pulsing) every 4-8 taps; the
  non-blocking reset recovery ported from mk3 brings it back in ~2 s, which is the start/stop
  the user sees. mk3 has the identical open stall ("following heavy rendering"); this firmware
  renders continuously, so it shows the wedge more often. Tried: a 4 ms floor between INT-driven
  reads (no change), per-byte I2C deadlines (no change), SPI at 10 MHz (17 clean taps then a
  wedge -- suggestive, not decisive). Next step is a logic analyser on SCL/SDA/INT during a
  wedge, not more inference. Not a spike blocker: the Rust I2C/SPI/DMA paths reproduce mk3's
  behaviour, including its bug.
- 2026-08-29 — **milestone 4 hardware-verified: console and the core-1 worker.** TinyUSB lives on
  core 1 in the C shell (mk3's arrangement for this board: `tusb_init`/`tud_task` there, core 0
  never touches stdio), and core 1 calls a Rust service that drains the log queue and feeds CDC
  bytes into a `Mailbox<u8>`; core 0's console module turns them into lines and commands, and
  commands into events in typed mailboxes (`light_core::mailbox`) the display, touch and board
  modules consume -- the string front-end of the event bus, replacing the ad-hoc static. Every
  command round-trips; `quit` runs the orderly shutdown (display cleared, backlight off, "runtime
  stopped cleanly" logged by core 1 after core 0's loop has ended). Panics on either core are
  handed to core 1 to print, then the board drops into BOOTSEL so it stays flashable. FFI is
  now four functions (`light_app_main`, `light_app_core1_service` in; `light_shell_log`,
  `light_shell_read_byte`, `light_shell_panic` out -- five, counting the SDK panic hook). The
  spinlock critical section is now genuinely contended across cores and holds. 32 host tests.
  Lost an hour to a host-side artefact: .NET `SerialPort.Write(string)` turned every LF but the
  last into a space; byte writes do not. The device path was right all along.
