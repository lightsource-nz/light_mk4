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
