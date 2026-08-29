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
    crates/light-font       the LGF bitmap font format: no_std reader, encoder behind `alloc`
    crates/light-rp2350     RP2350 board/peripheral access via rp235x-pac
    tools/crush             font-crusher in Rust: renders TrueType into LGF (and mk3's C pair)
    tools/vendor            freetype-sys, vendored with a one-line build.rs fix (see Cargo.toml)
    module/light_mk4_touch169/rust   the staticlib crate the firmware links (light_app_touch169)
    module/light_mk4_touch169        the C shell: main.c + pico-sdk executable (module/<target>/
                                     is where light-flash.ps1 looks for <target>.uf2)
    scripts/                the usual thin wrappers over $LIGHT_PATH/scripts

## Building

    scripts/build.ps1                 # default target, touch169
    scripts/flash.ps1                 # UF2 over BOOTSEL
    scripts/test.ps1                  # host tree; runs `cargo test` on the portable crates
    cargo run -p crush -- help        # the font tool (host only; not part of a firmware build)

`cargo build --workspace --target thumbv8m...` will not work: `crush` is a std binary. Build the
firmware through the scripts, or `cargo build -p light_app_touch169 --target ...`.

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
- 2026-08-29 — **the SWD questions, on the po13 rig (a bare Pico 2 in the debugprobe dock).** A
  second executable, `light_mk4_pico2`, shares the C shell with an LED-and-console Rust app.
  **probe-rs 0.32 works against the debugprobe**: `probe-rs download --chip RP235x` + `reset`
  flashed and started it, and `probe-rs read` samples memory (the LED's SIO bit toggling at 1 Hz)
  without halting the core. The openocd+gdb path via `scripts/debug.ps1 -Batch` also works.
  **The halt/resume artefact mk3 recorded reproduces exactly**: after `monitor halt`/`resume`
  through OpenOCD, CPACR reads `0x0000c000` (CP0 and the FPU denied) instead of the healthy
  `0x00f0c303` -- and the Rust firmware keeps blinking, because its GPIO goes through SIO
  registers, not the GPIO coprocessor. The image's only coprocessor instructions are in
  pico-sdk's `pico_double` wrappers, none of which run. A pac-driven Rust firmware is immune to
  that fault by construction; a C one using `gpio_put()` would have hard-faulted on the next
  write. Not yet seen: the Pico 2's own CDC console (no PID_0009 port enumerated -- check the
  board's USB cable), so its console was exercised only on the touch169.
- 2026-08-29 — **plan step 1: crush in Rust, and the LGF font format.** `tools/crush` reproduces
  mk3's command surface (`font add`, `display add`, `render new`, `console` with scripts, `-c`,
  `--interactive`, `--keep-going`, `help`/`exit` builtins) over a JSON context in `.crush/`,
  rendering through FreeType (bundled; `freetype-sys` vendored with a one-line fix because its
  crates.io package points at a zlib include path that only exists in its git checkout). Output
  is the **LGF blob** (`crates/light-font`: 48-byte header, presence bitmap, fixed-cell 1bpp
  glyphs, popcount lookup, zero-copy `no_std` reader) *and* mk3's C pair, so both stacks share
  one crush. 18 acceptance tests port the C suite's assertions -- and one more the C suite could
  not make: every glyph, and the whole generated `.c`, is **byte-identical to the C crush's**
  for the same font/display/size. The 93-case C suite's behaviours reduce to those 18 because
  the Rust tests assert directly instead of through CMake fixtures.
- 2026-08-29 — **the font in the firmware build.** `cmake/LightFont.cmake`'s
  `light_mk4_add_font()` is mk4's `crush_add_font_target()`: crush is built for the host inside
  the cross build (Corrosion's `hostbuild`, where mk3 needed an ExternalProject), renders the
  blob as a build step, and hands its path to the Rust crate through `corrosion_set_env_vars`
  for `include_bytes!(env!("LIGHT_FONT_LGF"))`, ordered ahead of cargo via `cargo-prebuild_`.
  Verified from a clean tree: render at step 9, link at 110. `light_core::draw` gained
  `Rgb565::{fill,text}` (host-tested), and the touch169 demo captions the panel in the 16 px
  face crush rendered for it. **Hardware-verified**: the caption reads correctly on the panel;
  the blob is 3,620 bytes for 94 glyphs where mk3 compiled ~20 KB of generated C.
- 2026-08-29 — **plan step 2, hardening the core from the spike.** `light_core::hal` is the port
  interface, written down from the primitives the spike actually used (Clock, Idle, OutputPin,
  InputPin, SpiDisplayBus, I2cBus, plus the `critical-section` impl) and nothing else.
  `light_core::events::EventBus` replaces the per-consumer static mailboxes: one app-defined
  event type, any producer, every subscriber sees every event in order, refuse-and-count when
  the slowest subscriber has not caught up (5 tests, including a 4-thread conservation check).
  The console publishes commands, the touch driver publishes touches, modules subscribe.
  `light_rp2350::boards::{touch169, pico2}::take()` hands each board's peripherals over once as
  an owned set -- the `steal()`-with-a-comment is gone -- and the clocks come from the shell that
  configured them (`light_shell_info`) instead of being assumed. `Blinker` takes a pin and a
  clock rather than a board. Hardware-verified on the touch169: commands fan out over the bus to
  three subscribers; `Breathe` is the RP2350 idle hook. Not done from the step-2 list:
  deferred (`defmt`-style) log formatting -- same queue contract, later.
- 2026-08-29 — **plan step 3, first half: the rasteriser and the second panel.**
  `light_core::draw::Canvas` is mk3's `light_draw` ported with its conventions intact: a 2x3
  integer transform from rotation and flip (exact inverse for touch input), an inclusive clip
  every primitive honours, 1 bpp (leftmost pixel in bit 0) and RGB565 formats, lines, rects,
  rounded rects with per-corner rounding, circles filled from their own outline spans, arcs
  sampled at half a pixel so joins never open, LGF text. 10 tests, including that every
  rotation/flip round-trips exactly and that a rounded outline is closed.
  `light_core::sh1107` is the Pico-OLED-1.3's driver (vertical addressing, one column per
  chunk, eight per poll), and the Pico 2 app draws the same demo as the touch169 on it through
  a 90-degree-rotated canvas, mapping logical regions to physical columns through the transform.
  The touch169 app moved onto `Canvas`. The OLED build is ready; its hardware check waits on
  the po13 rig being plugged back in.
- 2026-08-29 — **plan step 3, second half: the frame layer.** `light_core::frames::FrameLayer`
  is mk3's `light_canvas` ported with both its contracts -- every frame a full repaint,
  invalidate means "the panel is wrong here" -- and its subtle rule: what carries to the next
  frame is what the caller invalidated, never what was pushed. Regions merge where they overlap
  (transitively, restarting after each merge), stay separate where they do not, collapse to the
  whole canvas past eight, are re-clipped against the canvas as it is now, and are mapped to the
  panel through the canvas transform. Disjoint regions are queued and fed to the display one
  update at a time from `poll`. `Display` gained a back buffer and `swap`. 7 tests, including
  the carry-forward rule and a rotation forgetting stale regions. Both apps draw through it;
  the touch169 is double-buffered at 30 fps (hardware-verified: 187 frames in 10 s, 11 skipped,
  0 chunk timeouts at boot; 151 frames in 5.03 s once running, i.e. 30 fps). **The po13 OLED is
  hardware-verified** too: border, caption and bouncing square as designed, first try -- the
  SH1107 column addressing, the 1 bpp packing, the 90-degree rotation and the region-to-column
  mapping all correct together. Both bench boards now run the same demo through the same
  stack: rasteriser, frame layer, chunk protocol, driver.
- 2026-08-29 — **plan step 4: input.** `light_core::touch::Tracker` (swipes from down/move/up
  samples, hardware classification with first refusal, `suppress` for a consumed drag),
  `light_core::button::Button` (debounce that restarts on every raw change), `light_core::imu`
  (axis map into the device frame, orientation from gravity with a margin and a hold time,
  polling throttled to the sensor's rate) and `light_core::qmi8658` (one contiguous
  status+temperature+six-axis frame per read) -- all ports of mk3's modules with their
  hardware findings intact, 9 new tests. The CST816T driver latches the controller's gesture
  code with mk3's vertical inversion. Two drivers now share I2C1 through `&RefCell<I2c1>`
  (`I2cBus` is implemented for it). The touch169 demo rotates its canvas with the board's
  orientation and sends the square the way you swipe; the po13's keys hold the LED and
  reverse the square. po13 flashed; the touch169 dropped off USB mid-session, its flash pending.
- 2026-08-29 — **two bugs the bench found, both in the frame path.** The po13's caption froze:
  the demo consumed its "caption changed" flag before asking for a frame, and when the layer
  refused the pass the change was never invalidated -- a region is dirty until a frame has
  actually invalidated it, however many passes that takes. Then the touch169's square left a
  red trail: under double buffering the swap sat at the next `frame_begin`, after `frame_end`
  had queued the regions, so every push read the previous frame. The swap now closes the frame
  and the test asserts that shape. Step 4 hardware-verified on the touch169 after that:
  orientation Portrait → LandscapeR → Portrait with the canvas resizing, a hardware swipe
  steering the square, the QMI8658 reading gravity and 31 C on first contact. The controller
  still wedges and is reset every few seconds under tapping -- mk3 parity, the logic-analyser
  job. The touch demo now reports how many Move samples a touch produced on release.
