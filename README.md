# light mk4 — the Light Framework

The current, primary version of the **Light Framework** by lightsource aotearoa. This is the
framework the lightsource projects build on.

The architecture it proved out and runs on: **Rust framework code linked as a `no_std` staticlib
into a firmware executable that pico-sdk's CMake still owns.** The C shell keeps crt0, boot2, the
linker script, multicore launch, PIO and TinyUSB; Rust owns everything above the runtime. CMake
stays the outer build driver so the shared `light-*.ps1` script layer keeps working unchanged. The
same shape carries the bare-CMSIS STM32 ports, where a small C shell stands in for pico-sdk.

It runs today on the RP2040 and the RP2350 (both its Arm and Hazard3 cores), and on the STM32H743
and STM32F411 over bare CMSIS; a host build exercises the portable crates under `cargo test`
against a mocked board. It is hardware-verified across the Waveshare RP2350 touch boards (1.69,
2.8, 3.49, 4.0), a Pico-OLED rig, and the crossfire USB-MIDI host.

## Licence

MIT — see [LICENSE](LICENSE). Every crate carries `license = "MIT"` from the workspace.

## Layout

One cargo workspace, one version for all of it (`[workspace.package].version`, bumped in the
commit that `light-release.ps1` tags). The portable crates are layered, each depending only on
the ones above it in this list and never on a port:

    Cargo.toml              workspace
    crates/light-core       the port interface (hal), module runtime, log queue, event bus, mailbox
    crates/light-font       the LGF bitmap font format: no_std reader, encoder behind `alloc`
    crates/light-draw       the rasteriser: canvas, transforms, pixel formats, regions, text
    crates/light-display    the chunked display core, the frame layer, ST7789 / SH1107 / ST7735
    crates/light-input      touch tracking and gestures, CST816T, the IMU model, QMI8658
    crates/light-ui         the widget toolkit
    crates/light-midi       the USB-MIDI forwarder engine and its transport trait
    crates/light-power      power supply management: operating points, contracts, the request
                            ceiling that keeps a live rail survivable; the HUSB238 PD sink
    crates/light-rp2        RP2 chip port: the RP2040 or the RP2350 (feature rp2040 | rp2350),
                            one source over the chip's pac; both RP2350 ISAs; TinyUSB host
                            (usb-host). The chip only -- no board knows it exists
    crates/light-stm32h7    STM32H743 chip port, raw registers over bare CMSIS
    crates/light-stm32f4    STM32F411 chip port, the same shape
    tools/crush             font-crusher in Rust: renders TrueType into the LGF bitmap font format
    tools/vendor            freetype-sys, vendored with a one-line build.rs fix (see Cargo.toml)
    module/light_mk4_shell        the pico-sdk C shell (device and USB-host roles)
    module/light_mk4_shell_cmsis  the bare-CMSIS C shell (H743, F411)
    module/light_mk4_<board>/rust the staticlib crate a board's firmware links (light_app_<board>);
                                  its src/board.rs is the wiring -- pins, offsets, the taken-once
                                  peripheral set. Board wiring is the application's, never a crate's
    module/light_mk4_<board>      the board's executable (module/<target>/ is where
                                  light-flash.ps1 looks for <target>.uf2)
    scripts/                the usual thin wrappers over $LIGHT_PATH/scripts
    .github/workflows       the host test suite through the framework's shared workflow

The port crates are target-only and are excluded from the host `cargo test` along with the
app crates: light-rp2 needs a chip chosen, and a workspace-wide invocation unifies features,
so the two `critical-section` flavours (cortex-m's single-core one in the STM32 ports,
light-rp2's own) cannot coexist.

## Building

    scripts/build.ps1                 # default target, touch169
    scripts/flash.ps1                 # UF2 over BOOTSEL
    scripts/test.ps1                  # host tree; runs `cargo test` on the portable crates
    cargo run -p crush -- help        # the font tool (host only; not part of a firmware build)

`cargo build --workspace --target thumbv8m...` will not work: `crush` is a std binary. Build the
firmware through the scripts, or `cargo build -p light_app_touch169 --target ...`.

Needs `rustup` with `thumbv8m.main-none-eabi` (soft-float ABI, to match pico-sdk's `-mfloat-abi=softfp`
on Cortex-M33 — NOT `eabihf`, which fails at link with a VFP-args mismatch) and the usual
framework toolchain environment. `scripts/light-tools.ps1` puts `~/.cargo/bin` on PATH.

On Windows the Rust **host** toolchain must be the MSVC one (`stable-x86_64-pc-windows-msvc`).
The gnu host fails under `light-env.ps1`: w64devkit's gcc is first on PATH and has no
`libgcc_eh.a`, so the build scripts of the proc-macro and pac crates cannot link — and cargo
does not apply target rustflags to build scripts under `--target`, so no config.toml fixes it.
