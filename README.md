# light mk4 — the Light Framework

The current, primary version of the **Light Framework** by lightsource aotearoa. It began as the
feasibility spike from the 2026-08-29 assessment; that spike succeeded, and this is now the
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
    tools/crush             font-crusher in Rust: renders TrueType into LGF (and mk3's C pair)
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
- 2026-08-30 — **plan step 5: the widget toolkit.** `light_core::ui` is mk3's `light_ui`
  ported whole: a fixed-arena tree of windows, buttons and labels; `const` descriptors and
  `static` pages with parent-not-history navigation; the stack layout with min/max
  constraints and the flush-corner and end-cap geometry; scrolling with the clamp and
  scroll-into-view; focus cycling; the tap-versus-drag touch tracker; swipe classification in
  the logical frame; painting under per-subtree clips; and `render` over the frame layer. One
  design change: a button EMITS an application event (and optionally navigates) instead of
  carrying a callback and a command string, so a tap goes over the bus a console line does.
  8 host tests, one of them the touch169 demo painted at every rotation, dragged and
  navigated. Both apps run mk3's demo on it -- the touch169 with taps, drags, swipe-back and
  IMU rotation, the po13 as the two-key rig -- and both are hardware-verified, the touch169
  driven from the console (`ui activate`, `ui press 120 250` opening the list, `ui back`).
  What the bench cost: the first build overflowed core 0's 4 KB stack with the toolkit and
  frame-layer state built as temporaries, and since core 1's stack sits directly below,
  core 1 (USB) died silently while the UI kept answering taps -- diagnosed over SWD on the
  po13 as a hard fault with a garbage stack pointer. Those objects are `const`-constructed
  statics now. `PICO_USE_STACK_GUARDS=1` was tried and faults core 1 at boot on this SDK
  configuration (it died holding the log lock; core 0 then spun at its first `log::set_clock`),
  so it is off. Also learned: reading SIO spinlock 31 from the debugger ACQUIRES it. A full
  repaint costs 36 ms at opt-level 1; the release profile is still to come.
- 2026-08-30 — **the release profile, and where a frame's time actually went.** Release
  presets (`conf-light_mk4-<board>-release`, separate trees; Corrosion takes the cargo profile
  from CMAKE_BUILD_TYPE) took the 240x280 full repaint from 36 ms to 22 ms -- and opt-level 3
  against `s` made no difference, which said the compiler was not the problem. Timing the
  phases on the board did: the clear was 0.8 ms and the paint 20 ms, and the paint was
  `Font::pixel`, a presence-map popcount per PIXEL of every label. Fixed by looking a glyph up
  once per character; then horizontal and vertical runs stepping the buffer by the transform's
  stride instead of transforming every pixel, for spans, glyph rows and axis-aligned lines.
  The main page now paints in 5.8 ms and the eight-row list in 8 ms, the frame in 6.7 / 9.5 ms
  -- a fifth of a 30 fps period. `Ui::commit` splits out of `render` so an app can time or
  overdraw a frame it runs itself.
- 2026-08-30 — **the animations: the last of `light_ui`.** The two blits deferred since step 3
  (`Canvas::blit_rotated`, inverse-sampled with rounding for the reason mk3 found;
  `blit_offset`, a copy per row; `scale_inscribed`) work in physical space and ignore the
  transform, which is what lets them animate between two rotations. `Display::freeze` copies
  the panel's image into the back buffer and suspends swapping -- mk3's memcpy plus
  `set_double_buffer(false)` -- so drawing goes to the front while the back holds the capture;
  `thaw` puts swapping back. On that, `Ui` turns the captured frame through the shortest
  route over 280 ms and applies the real rotation on the final step, and slides the outgoing
  page off the incoming one over 180 ms, in the direction the VIEWER calls horizontal whatever
  the panel's orientation. Taps and swipes are refused mid-turn; a rotation asked for during a
  transition waits for it. A mono or single-buffered panel snaps, correctly. Tested on the
  host (a full turn at 50 ms a pass, taps refused, the settled tree at the new size) and on
  the touch169: four page transitions in 24 frames, none skipped, 6.8 ms worst draw.
- 2026-08-30 — **the CST816T wedge, bisected as far as software can.** Five 40-second runs of
  continuous tapping on the touch169, counting the controller's failed reads and recovery
  resets, all in the release build. Rendering on: 78 failed / 6 resets. Rendering paused
  (nothing drawn or pushed, controller polled identically): 26 / 1. SPI at 20 MHz instead of
  40: 84 / 6. The UNCHANGED frame re-pushed on every touch (all the bus and DMA activity,
  nothing changing on the glass): 88 / 7. I2C deadline raised from 2 ms to 10 ms: 74 / 8.
  Controller left unread while a push is in flight: 52 / 4. So: the failures track the
  SPI/DMA burst itself -- not the clock edge rate, not the picture changing, not a slow
  controller, and only partly a controller being read at a bad moment. The controller is
  disturbed by the burst and does not answer for tens of milliseconds afterwards, whatever the
  bus does. That is a supply, ground or coupling question on the board, and the next probe
  is a scope on the controller's VDD and INT during a push -- the analyser job it always was,
  now with the bus-side explanations eliminated. The console keeps the instruments:
  `render pause|resume|repush` and `touch hold|free`.
- 2026-08-30 — **plan step 6: crossfire.** mk3's USB-MIDI forwarder on the mk4 stack, on the
  po13 rig: the Pico 2's native USB port in the HOST role, the OLED as the status display, the
  console on the UART through the debug probe. `light_core::midi::Forwarder` is the engine --
  the one rule (cable C of any device to cable C of every other that has one), the padding
  drop, hub-port tracking learned from the devices behind a hub, the reset-only-when-the-bus-
  is-empty rule, the activity indicators -- portable, callback-free, and host-tested with
  eleven ports of mk3's `crossfire_forward_test`. The transport is TinyUSB through
  `light_rp2350::tinyusb_midi`, with the class callbacks implemented in Rust and handed to the
  module's own poll through a mailbox. The shell gained its host role
  (`light_mk4_shell_configure(... USB_HOST)`): the whole host stack runs on core 0 under the
  Rust runtime, since TinyUSB is not cross-core safe, and core 1 keeps the UART drain.
  Console-verified over the probe's UART: the stack up, the runtime polling, the OLED drawn.
  An instrument on the port is the next check.
  **A debugging lesson that rewrites an earlier one:** openocd's reset on this RP2350 config
  can leave a core parked in the bootrom's RAM helper at 0x2001xxxx with SIO spinlock 31
  held, and the next boot then spins at its first log lock on both cores. Every "hard fault
  at 0x200104xx" and "core 1 died holding the lock" this log recorded was that, including the
  one blamed on `PICO_USE_STACK_GUARDS`, which is unproven either way. Reading the lock from
  gdb acquires it too. The reliable sequence after a load is `monitor reset halt`, write 1 to
  0xd000017c, `monitor resume`.
- 2026-08-30 — **crossfire forwards.** Two instruments on a chained hub (addresses 5 and 6,
  the display's port map reading the inner chip's ports), 118 packets forwarded in 45 s of
  playing, the RX/TX indicators pushing 73 frames of their own band. Three faults on the way
  to that: the root-port-empty controller reset hung inside `tusb_deinit()`, which closed the
  device tree after tearing down the port's critical section (fixed in the pico-sdk TinyUSB
  fork, commit 676b027); without that reset the next enumeration panicked inside the USB IRQ,
  so the RP2350 needs mk3's workaround as much as the RP2040 did; and the shell's panic
  hand-off slept, which inside an interrupt handler raised the SDK's own panic over the one
  that mattered. In the host role the shell now halts on a panic where the debugger can read
  it rather than rebooting into a BOOTSEL nobody can see, and prints the message itself if
  core 1 never relays it. One earlier run died mid-play before that change and left no
  message; not seen since, and the halt-on-panic build is what will catch it if it returns.
  **It returned, and the halt caught it:** `buf_ctrl @ 0x50100080 already available`, TinyUSB's
  rp2040 host driver panicking in the USB IRQ on the stale-AVAILABLE hardware quirk -- on the
  in-IRQ round-robin switch of the shared EPX between pending endpoints, a path the rebased
  upstream driver added after the fork's fix for the same quirk on `hcd_edpt_xfer()`. Two
  instruments on a hub, three endpoints sharing EPX, hit it within a minute. The same
  force-clear on that path (fork commit bf8d79b) and a two-minute soak of playing on both
  instruments passed: 164 packets forwarded, no panic.
- 2026-08-30 — **the attach/detach hang, and a lesson about attaching.** Pulling an
  instrument from the hub "hung" the board: console silent, display frozen. A per-core
  heartbeat (`stats` reports passes for both cores, readable from memory without halting)
  and a two-core gdb session with openocd's flash probe disabled (`gdb_memory_map disable`,
  `gdb_flash_program disable` -- the probe runs a bootrom stub over whatever the halted core
  was doing, and had wrecked every earlier post-mortem) showed both cores alive but crawling:
  core 0 was inside the USB IRQ, in TinyUSB's MIDI host driver re-arming the pulled device's
  IN read from its own failed completion, timeout after timeout, and the hub's port-change
  report that would have unmounted it never got a turn. Fixed in the fork (a617677): a read
  is re-armed from the completion only on success; a failed one is marked stalled and re-armed
  by the application's next read, from thread context. A dozen attach/detach cycles across
  every port of a chained hub after that, heartbeats steady. Note the port map: a chained hub
  is two hub chips, and the display shows the port number whichever chip reported it.
- 2026-08-30 — **the remaining legs.** RISC-V: the same port crate on the RP2350's Hazard3
  cores, the ISA showing in exactly one place -- PRIMASK becomes `mstatus.MIE` through three
  lines of inline asm in the critical section -- with presets that pick LIGHT_ARCH=riscv32 the
  way screen-test's do; the whole po13 demo, keys and all, hardware-verified on the RISC-V
  cores. STM32H7: a second port crate, `light-stm32h7`, written against the reference manual
  rather than a pac (GPIO by port and pin, SPI4 with the H7 generation's rules mk3 found --
  TSIZE per transaction, the FIFO primed before CSTART, EOT not TX-empty, every wait bounded
  -- and a microsecond clock on the 32-bit TIM2), `light_core::st7735` for the MiniSTM32H7's
  160x80 panel, and a CMSIS shell in place of pico-sdk's runtime: the caches, mk3's 400 MHz
  clock tree, the ITM-and-USART console, CMSIS's startup and mk3's linker script, one core,
  the log drained by a module. Built for thumbv7em-none-eabihf (hard float, matching the
  shell's -mfloat-abi=hard; `use cortex_m as _` keeps the single-core critical section in the
  link) and hardware-verified over the ST-Link: the widget demo on the panel, the LED, and K1
  driving it -- a short press moves the focus, a hold activates the moment its interval
  expires. K1 is ACTIVE HIGH: mk3's board header said otherwise and nothing in mk3 ever read
  it; the pin sampled over SWD settled it. Two debugger notes for this board: connect under
  reset (`reset_config srst_only srst_nogate connect_assert_srst`) when a plain attach fails
  to examine the debug AP, and the SWO capture still needs the TPIU configured after the
  firmware has switched to 400 MHz. Not ported: the SPI-link peer transport, which the engine
  models and the H7 is now ready to carry.
- 2026-08-30 — **STM32F411.** The CMSIS shell's second chip: `light_mk4_shell_cmsis_configure`
  takes a CHIP argument that settles the device files, the startup file, the M4's fpv4-sp
  flags and the linker script, and `#if defined(STM32H743xx)` fences the three things the
  chips do differently -- caches and the clock tree (the F411 runs on HSI at reset defaults,
  as mk3's F4 ports did), the USART generation (ISR/TDR/RDR against SR/DR), and where the GPIO
  clocks live (AHB4 against AHB1). `light-stm32f4` is `light-stm32h7`'s shape at the F4's
  addresses, GPIO and TIM2 only, since the Blackpill carries a LED, a key and a console and
  nothing else; `light_mk4_f411` is mk3's demo for it on the mk4 runtime. Built, flashed over
  the ST-Link, and verified over SWD: TIM2 at 1 MHz, PC13 toggling at the blink rate, PA0 high
  under its pull-up, PA9 idling high with the USART running. The ST-Link's VCP (COM10) is
  not wired to PA9/PA10 on this board either, so the console has been exercised only through
  the pin states; a USB-serial on PA9/PA10 would finish that.
- 2026-08-30 — **the crate split: the spike becomes a layout.** `light-core` had grown to
  eight thousand lines holding the runtime, the rasteriser, the display stack, the toolkit,
  the MIDI engine and five drivers -- the spike's shape. It is now six crates along the lines
  the assessment's decision 10 drew (see Layout), `Region` moved from the display core into
  `light-draw` where the geometry lives, one workspace version, and a CI workflow calling the
  framework's shared host-test job. A `git mv` refactor and no behaviour change: the same 103
  host tests pass in their new crates and all six firmware trees build. Two things fell out of
  reconfiguring every tree at once. The host test suite had not compiled since the H7 commit:
  cortex-m's `critical-section-single-core` selects `restore-state-u32`, light-rp2350 selects
  `restore-state-bool`, and a workspace-wide `cargo test` unifies both -- the STM32 port crates
  are now excluded from it like the app crates. And the pico trees had silently lost their
  executables in the same commit: `if(LIGHT_SYSTEM STREQUAL PICO_SDK)` compared against a
  literal before `pico_sdk_init()` and against the variable `PICO_SDK` it defines after; every
  STREQUAL literal in the root CMakeLists is quoted now. The lesson from both is the same one:
  a change to the build has to reconfigure every tree, not the one being worked on.
- 2026-08-30 — **what a log record costs, measured; and the last item on the decision's debt
  list, re-scoped.** Sixteen `info!` calls timed on TIM2 on the Blackpill (16 MHz, opt-level
  1): 54 us each with no arguments, 78 us with one integer, 123 us with three arguments --
  roughly 6/8/13 us on the 150 MHz RP2350. The surprise is the floor: a message with nothing
  to format still paid for the `fmt` machinery, a copy into the 96-byte record and the lock,
  and that is the commonest kind of message. `log::Text` now keeps an argument-free message as
  the `&'static str` it already is (`Arguments::as_str()`), and that case measures 16 us --
  3.4x -- with the queue's contract, the drain API and the drop policy untouched. Full
  deferred formatting was the decision's "later optimisation"; with these numbers it is not
  an optimisation worth its cost -- what it would buy is the argument cases, at 8-13 us on the
  boards that matter -- and it is not a small change: `defmt`-style ids mean a binary stream
  over RTT and a host decoder, where every console here is text read by a person. It is a
  console-architecture decision to take if and when RTT logging is wanted, not framework
  debt. One trap for anyone testing this: rustc folds literal arguments (a string, an integer)
  into the format string, so `info!("{}", 1)` arrives as a static too.
- 2026-08-30 — **no `static mut` anywhere, and the migration ledger.** Every application had
  the same pattern for its frame buffers, frame layer and widget arena: a `static mut` in .bss
  and an `unsafe { &mut *addr_of_mut!(..) }` with a comment promising it was the only
  reference. `light_core::ConstStaticCell` (static_cell's, re-exported as the one blessed
  way) builds the object in place with a const initialiser and hands it out exactly once --
  a second `take()` panics instead of aliasing -- so the promise is enforced and the
  `unsafe` is gone. The two STM32 clocks' wrap trackers are relaxed atomics. The workspace
  has no `static mut` left. And `MIGRATION.md` lists every mk3 module of every consumer with
  what it became -- ported, pending with the reason it waits, or retired with the reason --
  so the state of the migration is a table, not a reading of this log. The one item on it
  that gates a product: RP2040, which crossfire's stock Pico needs and no mk4 port covers.
- 2026-08-30 — **RP2040: the port crate becomes `light-rp2`, one source for both chips.**
  The blocks the port touches -- SIO, pads, IO, SPI, I2C, DMA, PWM, the timer -- are the
  same IP on the RP2040 and the RP2350, and `rp2040-pac` 0.6 and `rp235x-pac` 0.2 come from
  the same svd2rust generation, so every accessor the crate uses is spelled the same in both.
  The chip is a feature (`rp2040` | `rp2350`, exactly one) and shows in three lines: which
  pac is aliased, `TIMER` against `TIMER0`, and the RP2350's pad `ISO` bit. The fourth
  difference is the Cortex-M0+ itself, which has no atomic read-modify-write: the framework's
  atomics are now `light_core::atomic` (portable-atomic, with its critical-section fallback),
  native instructions on the M33/M7/M4/Hazard3 and the port's spinlock section on the M0+.
  The po13 board module is `boards::po13` -- a Pico or a Pico 2 in the dock, pin-compatible
  -- and the two apps that run there forward the chip feature from the tree's PICO_PLATFORM
  through `corrosion_set_features`. Presets `conf-light_mk4-pico-debug` (the demo) and
  `conf-light_mk4-crossfire-pico-debug` (the product board), `openocd-rp2040.cfg`. Both build
  for thumbv6m-none-eabi (127 KB / 115 KB text); every other tree still builds. NOT yet on
  hardware: no RP2040 was on the bench. The first flash wants the crossfire tree on a stock
  Pico over a debugprobe, and the thing to watch is the spinlock critical section under
  portable-atomic's fallback -- every `fetch_add` on the M0+ now takes lock 31.
- 2026-08-31 — **`light_core::cli`: decision 6's last piece.** The bus made every input path
  publish the same typed events; what remained was five hand-written copies of the same
  word-matching. The shared `Cli` now owns echo, `help` (assembled from the table), the
  `loglevel` and `quit` built-ins, usage-on-bad-arguments and the unknown-command reply; an
  application hands it a `static` table of commands, each a name, a usage line and a parse
  function into the app's own event type. All five consoles converted -- the diff is almost
  entirely deletion -- and `loglevel` thereby arrived on the four boards that never had it.
  `stats` stays a table entry, not a built-in: every application's stats line is its own.
  One wrinkle worth recording: a table parse function cannot reach module state, so the
  touch169's per-module console counters are reported where the Stats event is published,
  not where it is parsed. The cli's tests include the decision-6 property directly: a console
  line and a test injection produce the same record on the same bus, indistinguishable to a
  subscriber.
- 2026-09-05 — **themes as data: a look-and-feel is a blob in the image, not code in the
  toolkit.** The font pipeline's arrangement, applied to visual style: a theme is JSON
  beside the application (`theme/steel.json`), compiled by crush (`crush theme compile`,
  via `light_mk4_add_theme` in CMake) into an LTH blob -- magic, entry count, then
  key/length/payload triples -- embedded with `include_bytes!`, parsed by light-ui's
  `Theme::parse` at boot and installed with `set_theme`. Restyling an interface, or
  shipping a themed variant, is a DATA change: no edit to the UI crate, ever. The
  contract that keeps it that way: strictness at authoring (crush rejects unknown JSON
  keys, so a typo stops the build), tolerance at runtime (the firmware SKIPS unknown
  binary keys, so a future theme still styles an old firmware with the shared subset).
  A theme carries the ground, frame, title, text, button outline/text, focus text, and
  the two shade surfaces; colors are raw RGB565 or `#RRGGBB` truncated on the way in;
  `Theme::DEFAULT` reproduces the pre-theme monochrome look exactly, so every unthemed
  application renders unchanged. Every paint site now draws through the theme, and the
  2.8's demo wears the first one -- steel: grey-blue frame, muted outlines, the
  steel-into-navy selection -- approved on the glass. Corrosion, it turns out, appends
  env vars across calls, so a crate embeds fonts and themes side by side.
- 2026-09-05 — **surfaces learn to carry a shade, and the selection learns to glow.**
  light-draw grows `lerp565` (component-wise RGB565 interpolation) and shaded fills --
  `rect_shaded` and `rect_rounded_shaded`, vertical gradients through the same span and
  corner geometry as the solid fills, degrading to a solid on a mono canvas. light-ui
  carries them as style: `Shade { from, to }`, `Ui::set_focus_shade` (the focused cell
  fills with a gradient and reads as LIT rather than inverted-flat -- one selection voice
  across the whole interface), and `Desc::shaded(from, to)` for a button's unfocused
  surface. Both routes run through the ordinary paint path AND the flush concentric
  construction, whose span fill lerps per row so a shaded row still hugs the frame's
  curve. First worn by the 2.8's demo: steel blue falling into deep navy on the selected
  cell, with a `shade FROM16 TO16 | off` console command for live tuning on the glass --
  approved on the first candidate, which is what live knobs are for.
- 2026-09-04 — **the touch169 demo learns its glass's corners, and the curve learns who
  owns it.** The 1.69's glass has a MEASURED corner radius of 42 (mk3's calibration
  sweep); the mk4 demo had guessed 24 -- the same class of invisible error as mk3's first
  guess of 20, with the frame's corners swallowed by the glass. Now: the root window draws
  at a 2 px breathing inset with the full measured radius, `set_safe_inset` finally has a
  caller (and its doc no longer teaches mk3's superseded radius-as-inset approach), and
  the corner geometry got rebuilt on three findings from the glass. One: a flush row's
  corners must be CONCENTRIC with the frame -- same centres, radius less the gap, curves
  parallel the whole way round -- and `rect_rounded` cannot draw that: its safety clamp
  caps the radius at half the row's height and re-anchors the arc to the row's own corner,
  which is why every radius ever tried poked through the frame at the apex
  (`paint_flush_bottom` now draws the true construction). Two: on a SCROLLING window the
  curve belongs to the CONTAINER, not to whichever row is passing -- corner treatment that
  rode the last row vanished the moment a scroll moved it -- so a rounded scrolling window
  re-masks its bottom corners after its children paint, and content slides beneath a curve
  that never moves. Three: the mask's isqrt erase spans and `arc()`'s trig sampling
  disagree by the odd pixel, so anything the erase might bite is drawn AFTER it; the inner
  boundary (arcs plus the straight run between) renders whenever the container holds
  enough content to scroll, a permanent fixture marking where content ends against the
  curve. All judged on the glass, iteration by iteration.
- 2026-09-04 — **PWM audio: the second provider, and the byte that never reached the
  compare register.** mk3's `light_audio` ports as `light-audio::pwm` (the PCM-to-duty
  conversion -- silence at MID-scale, volume attenuating toward it, because a piezo
  renders a DC step as a click -- with mk3's six mutation-hardened test groups) plus
  `light-rp2::pwm_audio` (the transport: a ~586 kHz DAC-mode carrier, a DMA pacing
  timer's DREQ delivering any sample rate for zero CPU, and the tone mode where the
  carrier IS the note, which is what a piezo is actually good at). Consumed by the
  touch169 app -- `tone`, `beep [HZ]`, `volume` -- on the piezo mk3's bench verified.
  The porting caught a real bug the original never knew it had: mk3 streamed single
  BYTES into the PWM compare register at the channel's byte offset, and the APB bridge
  upgrades narrow writes to word width by REPLICATING the byte across the lanes -- duty
  D arrived as D*257, past the 255 wrap, pinning the output at constant high. The DMA
  paced perfectly, moved every byte on time, and made no sound: the transfer-complete
  logs against total silence were the tell. mk4 streams channel-positioned WORDS.
  Verification honest to the transducer: the sample path is judged at the piezo's
  resonance (`beep 4000` beside `tone 4000`), because a piezo plays anything else
  near-silently however correct the stream is.
- 2026-09-04 — **the silent console was core 1 dying under core 0's stack, and the screen
  had to deliver the diagnosis.** For days, "the device wedged" during card-heavy commands:
  the console echoed one last line and went silent, later host writes timed out, and only a
  hard reset recovered it. Every layer was suspected in turn -- the dying SD card, the
  filesystem's chain walks, the SPI and I2C drivers, blocking stdout -- and each audit came
  back clean, until the glass broke the case: with the app running its own UI, the "wedged"
  device was FULLY RESPONSIVE to touch while the console was dead. The console is core 1.
  A core-1 heartbeat counter rendered by core 0 froze at the moment of death; a painted
  stack watermark hit zero at the same instant. Core 0's stack (SCRATCH_Y) sits directly
  above core 1's (SCRATCH_X), and a deep filesystem call chain -- a mounted `Fat` with its
  512-byte sector buffer stacked over another inside `open`'s directory walk, plus the
  formatting machinery of one log line -- dipped past core 0's floor and trampled core 1's
  live frames. Core 0 sailed on; core 1 died; the messenger was the casualty, which is why
  nothing could report it. FIX, in the shell for every device-role board: core 1's stack
  moves to ordinary RAM (`multicore_launch_core1_with_stack`), leaving SCRATCH_X as vacant
  runway a core-0 excursion overwrites harmlessly. Verified same day: the deterministic
  killer sequence (synth + play, 4/4 kills before) ran end to end with the console alive.
  Lessons carved in: when the console dies, CHECK THE GLASS before declaring a freeze; a
  diagnosis channel must not share fate with the failure it reports (the heartbeat + stack
  watermark now ride the dictaphone's diag row); and the two scratch banks are one bad
  frame away from being a shared fate machine -- separate them.
- 2026-09-04 — **the dictaphone: a second application on the 3.49, with its own interface.**
  `light_mk4_dictaphone` builds in the touch349 tree beside the demo -- two executables,
  two UF2s, one board and shell -- and replaces the widget pages with a recorder's: a live
  status line (elapsed time ticking through a take or playback), one big record/stop
  button, play-last, and a recordings list of the newest eight `REC_NNNN.WAV` takes, tap to
  play. Auto-numbered 8.3 names, ordinary WAV. For the dynamic text (elapsed seconds, file
  names) light-ui grew `set_text`: a small owned per-widget buffer shown in place of the
  static label, which descriptors alone could never carry. Playback stages its card reads
  OUTSIDE the DAC refill so a slow read gets a whole buffer period of slack, and `micgain`
  retunes the mic's two gain registers live over the console -- no reflash per step.
  light-fs also learned to refuse corruption: a directory entry's first cluster is
  validated before it reaches cluster arithmetic (a churned card's garbage entry was an
  arithmetic panic straight into the bootloader; now it is `BadChain`), with the dying-card
  scenario as a host test.
- 2026-09-04 — **all playback ever done on the 3.49 ran at half speed; the log timestamps
  convicted it.** Recorded speech played back "pitched down several octaves" from data that
  hex-dumped as a pristine normal-pitch waveform -- and "pitch is preserved by construction"
  (capture and playback share the codec's one LRCLK) said that was impossible. The device's
  own timestamps settled it: a 2.000 s sine file took 4.031 s from `play` to `play:
  finished`. Exactly 2x. The dout PIO program's header comment claims one `pull` per channel
  HALF-frame, but the program's steady-state loop (the wrap returns past the entry pull)
  consumes ONE 32-bit word per FRAME -- top 16 bits out the left half, low 16 the right --
  and both stream producers were written against the comment, pushing two words per sample:
  every sample played for two frames. Nobody could hear it in a bare tone (440 Hz at 220 is
  still "a clean tone" without a reference); a voice made it obvious. Fix: one word per
  sample with the sample in both halves, STREAM_WORDS halved to keep the same 53 ms of
  buffer (returning 10 KB of RAM), the comment corrected, and the rate now VERIFIED by
  timestamp: 2.000 s of sine in 1.975 s wall clock, a 3.8 s voice take in 3.79 s. Capture
  was independently re-verified at 47.9 KB/s over a 32 s soak. End-to-end voice loop
  confirmed on the glass at true pitch. Two lessons worth the price: comments describing
  hand-assembled PIO belong NEXT to the instruction words they describe, and when ears and
  theory disagree, measure with timestamps -- every earlier "distortion" verdict (and a
  gain retune based on one) was judged through this half-speed lens. Also new: `rec null`,
  a capture soak that drains the whole mic pipeline with the SD card out of the path -- it
  cleared the firmware when the bench's much-abused card (which no PC will mount any more)
  froze the device during sustained writes.
- 2026-09-03 — **the console must never block: non-blocking stdout.** Recording wedged the
  CDC console, and the cause was a blocking write, not the SD load: a `rec` emits ~7 log
  lines at start, and with the host not reading mid-capture they fill the CDC TX, whereupon
  the core-1 log drain BLOCKED in the stdio write (`PICO_STDIO_USB_STDOUT_TIMEOUT_US=5000`)
  waiting for space -- and while stuck there it stopped pumping `tud_task`, so the CDC's
  OUT endpoint went unserviced and the host's next write timed out. The drain runs on core
  1, but a blocking write lets it self-stall. Fix: `PICO_STDIO_USB_STDOUT_TIMEOUT_US=0` --
  stdout drops when the host isn't reading rather than stalling the core that runs USB,
  matching the log queue's own drop-with-counter policy on the push side. It's the
  framework's stated principle (logging never blocks the loop) applied at the transport,
  and it fixes recording on every device-role board at once. (Two wrong guesses preceded
  the fix -- "core-0 SD starves the CDC" and "no device hang at all" -- both retracted.)
- 2026-09-03 — **the dictaphone: the codec's encoder, and record/play through the
  filesystem.** The ES8311's capture half plus a recorder and player on the 3.49. New in
  `light-rp2::i2s`: a PIO capture machine (SM2 on PIO1) that samples the codec's SDOUT
  against its mastered BCLK/LRCLK, fed by its own ping-pong DMA into 200 ms buffers, with
  `attach_capture`/`capture_start`/`capture_take` mirroring the playback side. `light-audio`
  gained `mic_enable` (the vendor's analog-mic recipe verbatim) and an ADC->DAC monitor for
  bring-up. The app records mono 16-bit WAV straight to the card through `light-fs`
  (write-through, header patched on stop) and plays WAV back out the DAC, parsing the RIFF
  chunk walk and validating format. The bring-up was a five-bug gauntlet, each caught by a
  built-in diagnostic rather than a guess: (1) a ~1.2 KB `Recording` inline on core 0's
  4 KB SCRATCH_Y stack spilled into core 1's stack and wedged BOTH cores -- moved to .bss;
  (2) a hard reset mid-I2C left a codec driving SDA low and, on a battery-backed board, no
  reboot freed it -- every `I2c::new` now bus-clears with nine SCL pulses first; (3) the
  DIN pad was an SIO input, not routed to the PIO; (4) the capture SM shifted RIGHT, landing
  the 16-bit sample in the high half while the halfword DMA read the low half -- exact-zero
  captures from a live signal, found by draining the raw RX FIFO (0xa0000000 = real audio in
  the wrong half); (5) file playback glitched because 21 ms stream buffers could not ride
  out the 39 ms full-frame display push -- 53 ms buffers gave zero underruns. Also: the
  speaker amp is muted during capture (it clicked into the mic), and the ADC digital volume
  is the vendor's 0xFF (0xBF was ~32 dB too quiet). Capture verified by dumping real
  waveforms off the card; playback verified clean on a synthesized sine.
- 2026-09-03 — **light-fs rounds out: remove, truncate, rename, mkdir.** The directory
  operations, under the same write-through discipline. `remove` deletes the entry FIRST
  -- the commit point -- then frees the chain, treating a broken link as the end of the
  walk (a leaked tail is a checker's lint, not corruption; the LFN slots left behind are
  the orphans the read side's checksum gate already ignores). `truncate` frees the tail
  and re-marks the new last cluster end-of-chain. `rename` moves the raw 32-byte entry
  whole -- attributes and all -- across directories too, pointing a moved directory's
  ".." at its new parent, and refuses a move into the mover's own subtree. `mkdir` lays
  "." and ".." into one zeroed cluster; `rmdir` takes only empty directories (NotEmpty
  otherwise). create/mkdir/rename now share one insert_entry that owns the end-marker
  bookkeeping. Twenty host tests -- including "a moved directory's .. resolves to its new
  parent" exercised through a real `NEST/SUB2/..` path -- and a full console round trip
  on the Pi card: mkdir MK4, mv LIGHT.LOG into it, cat through the new path, trunc back
  to one line, rm the file, rm the directory, and the final ls answering NotFound.
- 2026-09-03 — **light-fs learns to write, seek, and read long names.** The staged second
  half. Writes are WRITE-THROUGH end to end: every mutated sector reaches the medium
  before the call returns, every FAT copy is kept in step (the fixture grew a second FAT
  to prove the mirroring), and the directory entry's size and first cluster are rewritten
  at the end of each `write` -- a pulled card loses at most the call in flight. `create`
  claims a directory slot (reusing deleted slots, walking the end marker forward, growing
  a chain directory by a zeroed cluster -- and answering DirFull for the FAT16 root,
  which cannot grow); the first cluster is claimed lazily by the first write; `append` is
  open-plus-seek; `seek` walks the chain, with the boundary subtlety that a position on a
  cluster edge belongs to the END of the previous cluster. Free clusters come from a
  rolling-hint scan that wraps once and answers NoSpace honestly. Long names are now READ
  (up to 64 ASCII chars): LFN chains are accumulated across sector and cluster edges,
  checksum-verified against their 8.3 entry -- orphaned slots attach to nothing -- and
  usable in listings and path lookup both; creation stays 8.3. Fifteen host tests, and
  the hardware pass wrote LIGHT.LOG onto the Pi boot card: create 15 B, append to 27 B,
  read both lines back. One fixture lesson: the test's hand-laid LFN split the name at
  the wrong character and blamed the decoder -- the failure named the fixture.
- 2026-09-03 — **the filesystem layer: FAT over anything block-shaped.** The framework's
  portable FS story lands in two seams and a crate. `light_core::hal::BlockDevice` is the
  bottom seam -- 512-byte LBA reads and writes plus a count, with a blanket impl for
  `&mut T` so a filesystem mounts OVER a borrowed device and the board keeps its card.
  `light-sd` implements it (and grew the CMD24 single-block write with its data-response
  and busy-wait). Above them, `light-fs` is our own FAT16/FAT32 -- no_std, no alloc, one
  owned 512-byte buffer -- read-side first: mount (superfloppy or through an MBR's first
  FAT partition), directory listing, case-insensitive 8.3 path descent, sequential file
  read through cluster chains, with a [`File`] that borrows nothing so any number
  interleave. The type decision follows the spec's one true rule (cluster COUNT, never
  the BPB's label string, which lies on real cards), and the two formats a card might
  actually carry but this crate does not speak -- exFAT, the SDXC factory format, and
  FAT12 -- are detected and NAMED in the error instead of misparsed. Seven host tests
  mount hand-laid FAT16/FAT32/MBR images; the hardware verification was better than any
  fixture: a Raspberry Pi boot SD in the 3.49's TF slot -- `fs ls` walked its root and
  overlays/, `fs cat overlays/README` read 274 KB through a path, and the log queue's
  drop-with-counter policy absorbed a 300-entry listing without blocking, exactly as
  designed. Writes are the staged next step; the trait already carries them.
- 2026-09-01 — **transitions without a capture, and the end of the clear.** Page
  transitions needed the outgoing page's image, which lives in a back buffer this board
  cannot afford -- so the roles swap: the incoming tree (the live one -- the outgoing tree
  is already destroyed) draws OVER the old image at a shrinking logical offset
  (`Canvas::set_offset`, its clip bounded to what lands on the buffer), and the old page
  survives in the live buffer wherever a step has not yet covered it. Works in any format;
  the capture path remains for double-buffered boards. Getting it clean on the glass
  killed the frame clear entirely, in three measured steps. First a black bar flickered
  atop the scrolling list: a cleared live buffer is black under the beam until the repaint
  reaches it, and the beam-gate's past-the-bottom arm had no concept of wrap runway (both
  fixed: `draw_over` frames, and the gate learned the trip time back to a region's top).
  Then the focused button flickered: the window filling its WHOLE interior before its
  children re-created the clear's race locally. Then the old menu survived inside the new
  page's widgets: outline-only buttons and bare labels had always been leaning on the
  clear for their backgrounds. The destination is one rule -- EVERY pixel is written once
  per frame, with its final value: windows fill only the gaps around what their children
  will actually paint (viewport-clipped, not raw rects -- the difference kept a jumble of
  old frames below the viewport), buttons and labels fill their own rects, and `run()`
  gained a grey fast path that makes those fills cost what the clear did. Hw-verified:
  transitions, scroll and toggles clean.
- 2026-09-01 — **tear-free single-buffering: racing the beam instead of buying a buffer.**
  The 4" board's deferred display expansion. A second 450 KB framebuffer does not exist on
  a 520 KB chip, but the scanout engine already knows what a back buffer would be standing
  in for: `light_rp2::rgb::beam_row()` reads the beam position straight off the data
  channel's remaining count (vblank deliberately answers "just wrapped"). Above it,
  `Ui::dirty_bounds()` exposes the union of pending invalidations BEFORE painting and
  `FrameLayer::to_physical()` maps it through the canvas transform to panel rows, so the
  app can gate each draw: a partial region waits until the beam is past its bottom row --
  it will not be back for most of a frame -- or far enough above that the draw finishes
  first; a full-canvas draw or animation step starts at the wrap and OUTRUNS the beam,
  painting rows ~3x faster than the 31.5 kHz scan, so the beam only ever reads finished
  rows. Zero bytes of RAM, one deferral counter in `stats` (`beam waits`), and the draw
  estimate is the measured worst case, not a guess. The `set_framebuffer` flip hook stays
  for a board with the memory to use it. Verified on the glass: transitions and toggles
  clean, 0 frames skipped.
- 2026-08-31 — **the 4" glass lights: a handshake race and a clock requirement.** The
  scanout's bring-up was a lesson in perfect-looking wires: DMA verifiably walking the
  framebuffer at 15 Mpix/s, no starvation, all four state machines running -- and a black
  panel. The `scan` command (PIO DBG_PADOUT/PADOE plus the four program counters, now part
  of the engine) named it: the DE machine parked at its hsync wait while the data machine
  streamed, meaning DE had collapsed to a runt pulse per line. The race is LATENT IN THE
  REFERENCE PROGRAMS: after `irq set 0` the data machine wraps to a level-wait on DE, and
  the input synchronizer still shows the high its partner has not yet dropped -- it sails
  through one line early, and from then on the partner's `wait irq` always finds the flag
  already set, so DE never spans a burst again. The fix is an EDGE wait (`wait 0 pin`
  then `wait 1 pin`): the falling edge always lands within cycles of the IRQ, and the
  handshake cannot re-enter stale. Second finding: the stock-150 MHz ambition died on the
  glass -- 150/32 = 4.6875 and the fractional divider's +-6.7 ns stutter leaves the picture
  wavery and distorted; 240 MHz (divider 7.5, the vendor's own clock) is rock solid and is
  now a documented board requirement, not an experiment. With both in, the panel shows the
  test pattern and the widget demo, touch lands on target, and the battery divider is
  measured ÷2 (the vendor's ÷3 formula read an impossible 6.6 V). Hw-verified 2026-08-31.
- 2026-08-31 — **the RGB scanout engine: the 4" board builds.** The RP2350-Touch-LCD-4's
  ST7701S has no GDDRAM -- every pixel of every frame streams over a 16-bit DPI bus
  forever -- and `light_rp2::rgb` makes that a hardware-only loop. Four hand-assembled PIO
  programs (ported from Waveshare's reference): hsync free-runs HSYNC+PCLK at 2x PCLK,
  vsync counts lines off its IRQ, and on the second PIO block -- IRQ flags do not cross
  blocks, so it WATCHES the sync pins -- rgb_de raises DE per active line while the data
  machine pulls one word per pixel onto 16 pins. Both blocks run with GPIOBASE=16, the
  RP2350B feature that fits sync pins in the 20s and data up to GPIO 39 into one window.
  The feeding deliberately rejects the reference's design: their chunked DMA is restarted
  from an interrupt handler (late IRQ = sheared frame; their examples overclock to 240 MHz
  for headroom). Here the data channel streams the WHOLE frame in one 230,400-transfer
  pass and chains to a one-word reprogram channel that copies the framebuffer address from
  a control word back into the data channel's read-address trigger -- a two-channel
  hardware loop with no interrupts and no deadline for software to miss, at the stock
  150 MHz. A buffer flip is one store into the control word. The framebuffer is the
  decision: 480x480 RGB565 is 450 KB of the 520 KB SRAM, single-buffered for bring-up
  (drawing races the scan; tearing accepted until measured), with `set_framebuffer` as the
  flip hook for whatever comes later. Above it the chunk model degenerates on purpose:
  `light-display::scanout` answers zero chunks for every region and the frame layer never
  notices the panel is self-refreshing. Around the engine: `st7701s` (the bit-banged 9-bit
  init, verbatim, ending in the SLPOUT/DISPON this panel genuinely wants), the GT911
  driver (16-bit registers; `I2cBus` grew write_register16 with a payload; explicit
  release reports, unlike the AXS), and the touch4 app -- touch, IMU, RTC and battery all
  on one shared i2c1. Builds and host-tests green; the glass will say the rest.
- 2026-08-31 — **the 3.49's backlight is a threshold drive, measured.** "Dim just blacks
  the screen" opened a hunt that first ACQUITTED the PWM: the slice registers read back
  correct at every level (top 1000, cc tracking, counter running, pin muxed, pad toggling)
  -- the framework's first upper-bank PWM pin works. The panel's response is the finding:
  a stepped duty sweep on the glass showed the backlight fully dark at or below 40% LED-on
  time, with all visible dimming compressed between ~45% and 100% -- an RC-filtered
  threshold drive, not a proportional switch. A gamma-2 curve (the usual perceptual fix)
  made it WORSE, mapping most of the scale below the cutoff. The board module now maps
  level 0 to off and every other level linearly onto the measured band above a 45% floor,
  so the console's whole 0..1000 scale lands on visible brightness; the demo's Dim landed
  clearly-dim-clearly-lit on the glass. The lesson for the next board: sweep the backlight
  on bring-up -- "full and off both work" proves only that the pin wiggles.
- 2026-08-31 — **the 3.49's last two peripherals: the TF slot reads, and the PSRAM turns
  out not to exist.** The TF slot is wired for SDIO (CLK 26, CMD 27, D0..D3 28..31), which
  maps exactly onto SPI1 with D3 as chip select -- so the classic SPI-mode fallback needed
  no PIO engine, just a shape the hal lacked: `SpiBus`, a full-duplex byte exchange with
  caller-owned CS and a rate change (SD init must run under 400 kHz, data runs at MHz),
  implemented as `light-rp2::spi_bus::Spi1Bus` beside the display-framed SPI. The new
  `light-sd` crate is the SPI-mode block layer -- CMD0/CMD8/ACMD41/CMD58/CSD and
  single-block reads, three host tests scripting the wire byte for byte -- and stops at
  "blocks read back": a filesystem is a separate decision, not a peripheral. On the glass:
  an empty slot answers NoCard cleanly, and a 64 GB card identified as SDHC/XC, decoded
  123,596,800 blocks from its CSD, and read block 0 with the boot signature present. The
  PSRAM story ended differently: GPIO 47 is the RP2350B's XIP CS1 and the vendor demo pack
  carries a whole PSRAM library, but the SDK's auto-detection (wired in through
  `hardware_psram` and a one-function C shim) reads no chip ID, and the wiki's spec list
  carries no PSRAM -- the library is family boilerplate, not evidence of fitment. MEASURED
  ABSENT; the auto-detect stays wired so a fitted variant lights up unchanged, and the
  `psram` console command reports whatever detection found.
- 2026-08-31 — **the 3.49 fills out: battery, power latch, RTC and audio.** Four of the
  board's six remaining peripherals, each hardware-verified as it landed. `light-rp2` grew
  `adc` (the framework's first ADC: one-shot blocking reads, ~2 us at the 48 MHz ADC clock;
  the RP2350B's channels start at GPIO 40) and the battery divider reads a plausible 4.2 V;
  the SYS_EN power latch is driven high as board::take()'s FIRST act -- on battery the
  board is only powered while the user holds the button until that line runs -- and the
  side button's 1.5 s hold flows through the runtime like the console's `quit`, every
  module unloading before the latch releases. The PCF85063A (new `light-rtc` crate, shared
  i2c1) keeps the reference's 1970 year base and surfaces the oscillator-stop flag as
  "trust me or not"; it read UNSET on first contact, took the bench clock, and has kept
  time through every reflash since -- the backup supply is real. The ES8311 (new
  `light-audio` crate) is configured as the I2S MASTER -- this side only feeds it a
  PIO-generated 256-Fs MCLK and answers its BCLK/LRCLK as a slave writer
  (`light-rp2::i2s`, both programs hand-assembled with the wait pins baked in from board
  wiring). The first cut fed the four-word TX FIFO from poll() and was audibly CHOPPY: 83
  us of FIFO headroom against 15 ms frame draws. The stream is ping-pong DMA now -- two 4
  KB buffers chained through two channels, ~21 ms each, refilled from poll, underruns
  counted not guessed at -- and a 4 s tone over six full-frame redraws played clean with
  zero underruns beyond the expected one at boot (display init's 600 ms reset drains the
  first ring; it restarts itself). One board fact with teeth: PA_CTRL and DOUT are GPIO 0
  and 1, so audio RETIRES THE UART CONSOLE on this board -- CDC only from here. Still
  pending: PSRAM on CS1, the TF slot, the microphone half of the codec.
- 2026-08-31 — **the 3.49's freeze hunt: the panel ignores its own windowing.** The bring-up
  session below ended with every counter clean; real use then showed "long gaps in touch
  response after every touch", and the hunt that followed is a lesson in symptom attribution:
  the logs cleared the touch driver (every tap registered instantly, 18 toggles in 9 s while
  the user was hammering a dead-looking button), cleared the runtime (console-injected
  presses fired and drew), and finally cornered the display: PARTIAL updates froze while
  full-page pushes kept landing. Two panel behaviours, measured on the glass, explain it.
  First, per-row RAMWR bursts (0x2C re-opened with CS cycled between rows) are accepted once
  after a full-window push and then silently ignored until the next one. Second, even a
  single full-width band with an honest RASET start lands at ROW 0 -- the chip takes the
  window write and ignores the row offset. The reference driver never uses its own partial
  path; the one push shape this panel has ever honoured is full-frame Display(). The driver
  now pushes the whole frame for any region (~12 ms at the PIO bus's 37.5 MHz -- inside the
  30 fps budget), and windowed partials wait for a bench session that finds the incantation
  the vendor never needed. Along the way the bar also lost auto-rotation into landscape:
  its resting pose sits at the classifier's margin, so ordinary handling flapped
  LandscapeL/R -- a 180-degree relayout per touch, with every next tap landing where a
  widget used to be. A 172 px-tall landscape canvas was never worth that; the demo now
  rotates only for the deliberate end-for-end flip.
- 2026-08-31 — **the 3.49 on the glass: the QSPI stack verified, and the touch protocol's
  one surprise.** First flash lit the panel outright -- the vendor init table without
  SLPOUT/DISPON was right as copied, the PIO-QSPI path pushed the 172x640 frame with zero
  chunk timeouts, and the AXS's touch half answered with zero failed reads. The surprise:
  the touch protocol is CONSUME-ON-READ. A report is handed over once; the next read answers
  zero fingers while the finger is still on the glass, so trusting that zero produced a
  down/up pair per poll (a tap became sixteen taps; a drag would have shredded). The driver
  now treats a zero-finger frame as silence and infers release from 60 ms without a report
  -- the CST drivers' quiet-path discipline, arrived at from the opposite direction -- and a
  measured swipe is one Down, 380 Moves, one Up. Axes measured on the glass: raw long runs
  from the USB end but row 0 is at the far end, so the long axis inverts (the reference's
  `640 - pointX` said so all along); the short axis matches the pixels. The IMU
  three-observation session gave display x = -raw_y, y = +raw_x, z = +raw_z, and the UI now
  follows the bar through every pose. Labelled-widget taps, list scrolling and swipe-back
  all land; every counter is zero after the full session. The bring-up also re-ran the
  predicted identity-map artifact on cue: before calibration the resting tilt read as
  landscape and rotated the UI, which is what made corner taps hit "wrong" widgets --
  consistent wrongness, exactly what an unmeasured map owes.
- 2026-08-31 — **two more Waveshare boards surveyed; the 3.49 built, the 4 scoped.** The
  RP2350-Touch-LCD-3.49 (AXS15231B, 172x640) and -4 (ST7701S RGB, 480x480, GT911) are both
  RP2350B parts, which grew `light-rp2` its upper-bank GPIO support (SIO GPIO_HI_*), a
  second I2C instance (one macro stamps I2c0 and I2c1), and PIO function selects. The 3.49
  is the first QSPI panel: `light_core::hal::QspiDisplayBus` exists because an AXS register
  write is ONE chip-select frame (no D/C wire), `light_rp2::qspi::PioQspiDisplayBus` drives
  it from a hand-assembled two-instruction PIO program (commands bit-expanded onto D0 inside
  the 4-bit framing, pixels DMA-fed raw -- Waveshare's reference structure, kept), the panel
  driver carries the vendor init table verbatim (deliberately no SLPOUT/DISPON -- the
  reference sends neither), and the touch half is a raw command-blob protocol behind
  `I2cBus`'s new write_raw/read_raw. `light_mk4_touch349` builds with double 215 KB frame
  buffers; the backlight is INVERTED (the reference writes 100-value); wiring provenance is
  the reference demo, so everything is unverified until the glass. Two build-system traps
  re-met and fixed: the new board names had to join mk3's RP2350 allow-list (the memory's
  "omission silently builds rp2040 code", exactly), and the board headers came from
  Waveshare's own demo into the pico-sdk fork. The 4" is scoped, not built: a continuous
  RGB scanout engine (four PIO SMs + DMA feeding scanlines forever, no GDDRAM) is a new
  display integration, not a driver, and comes as its own leg.
- 2026-08-31 — **the touch28 board: the CST328's first hardware.** mk3 authored this board's
  support without hardware (schematic + two reference drivers); the board arrived and the
  definition came across: `I2cBus` grew 16-bit register operations (default-implemented, so
  only buses that meet such a part carry them -- mk3's `read/write_register16`, in trait
  form), `light-input::cst328` mirrors the cst816t's entire hardened poll architecture over
  the new wire protocol (packed 12-bit coordinates, address-only mode commands, the 0xCACA
  probe that must ALWAYS switch back to normal mode, no gesture engine -- the software
  tracker classifies), and `module/light_mk4_touch28` is the touch169's demo on the 240x320
  glass: square corners, no GDDRAM offset, the IMU axis map declared IDENTITY-until-measured
  as mk3's header insists. First flash: the CST328 answers with ZERO failed reads -- the
  16-bit protocol is right -- and the QMI8658 reports live accel. Boot-time probe lines are
  lost to the CDC connect window (a known cost); the read counters carry the same news.
  THEN THE PREDICTED ARTIFACT ARRIVED ON CUE: the UI came up rotated 90 degrees, because
  the identity axis map read real-portrait as landscape -- which also verified the whole
  IMU-to-rotation pipeline end to end. The three-observation calibration over the live
  console settled it in minutes: up-the-screen = -chip X (upright: [-845,+422,+223]), out
  of the screen = -chip Z (flat: [+44,+88,-1004]), device X = -chip Y by right-handedness,
  confirmed by the on-edge pose reading LandscapeL with -1076 on device X. The measured map
  -- the 1.69's transposition and Z inversion plus a 180-degree twist -- is in board.rs
  with the observations recorded beside it. THE FINGER CLOSED THE CHECKLIST: 25 seconds of
  tapping and dragging -- every tap a hit on the widget under it (Items, toggles, Dim/Bright,
  Back), drags of up to 98 move samples scrolling the list, swipe-right returning a page --
  with 469 frames pushed and, the number that matters most on this stack: the CST328 at
  ZERO failed reads and zero resets through continuous rendering plus touching, the exact
  load that wedges the 1.69's CST816T every few taps. Same bus layout, same firmware, same
  poll architecture, different controller: strong evidence the 1.69's wedge is that board's
  electrical fact (supply/coupling under the SPI burst), not the framework's -- precisely
  what the still-open scope session was going to ask.
- 2026-08-31 — **touch28 headroom probed live: 75 MHz is the default.** The corner radius is
  0 by eye (square glass confirmed), and the SPI question turned out to be a one-point test:
  at a 150 MHz clk_peri the PL022's achievable rates are coarse -- 75, 37.5, 25 -- so the
  old "40 MHz" request actually ran 37.5 and the reference driver's 62.5 rounds to trying
  75. A `spi HZ` console command re-clocks the bus live (waiting the display out first, then
  invalidating everything so corruption would show immediately; `St7789::bus_mut` exists for
  exactly this kind of bring-up instrumentation). 75 MHz ran clean on the glass through full
  repaints, 0 chunk timeouts, and the CST328 stayed at zero failed reads with the burst
  twice as fast -- more of the 2.8's clean electrical story. Board default now 75 MHz; a
  full-frame push is ~16 ms, and a typical redraw lands in ~20 ms end to end. (Also
  re-checked while here: no port crate hardcodes a clock -- the shell measures
  clock_get_hz() at boot and everything derives dividers from what it is handed.)
- 2026-08-31 — **the RP2040 runs, and two findings paid for the trip.** The po13 demo and
  crossfire both hardware-verified on a Pico in the po13 dock: full CLI sessions over the
  probe UART, the OLED pushing frames over DMA with 0 chunk timeouts, crossfire's host stack
  pumping at ~600k core-1 passes/s -- the M0+ portable-atomic fallback carrying real traffic.
  FINDING ONE: the first boot panicked straight into BOOTSEL, and the panic path proved
  itself -- the message survived in RAM for probe-rs to read: rp2040-pac's bounds check on
  DMA channel 15, because **the RP2040 has 12 DMA channels where the RP2350 has 16** and the
  po13 wiring's "top of the range" was an RP2350 fact. The DMA channel in board.rs is now a
  chip-cfg'd constant (11 / 15). FINDING TWO: probe-rs `download` on this RP2040 (w25q16jv)
  reported success while `verify` said the flash did not match and the core sat in the
  bootrom with nothing to boot; with `--verify` the same download programs correctly. The
  shared script now passes `--verify` always -- a few seconds of readback against a silent
  misprogram. With this, every chip in the ledger has run mk4 on hardware.
- 2026-08-31 — **`debug.ps1 -ProbeRs`: the fast flash-and-run path the spike wanted.**
  `light-debug.ps1` (shared, in the framework repo) grows a probe-rs branch: download the
  ELF, reset, done -- no OpenOCD, no gdb, always batch. On the RP2350 that also sidesteps
  the two debugger contaminations this log has documented: the flash-probe ROM stub run over
  a halted core, and SIO spinlock 31 held by the debugger's own reads. Each Debug entry in
  project.config.ps1 now carries `Chip`, probe-rs's name for the part (RP235x, RP235x_riscv,
  RP2040, STM32H743VI, STM32F411CE -- all in probe-rs 0.32's registry). The gdb path is
  unchanged and remains the way to a `monitor` command or a breakpoint. Verified to the last
  step a probe-less bench allows: the refused-combination and missing-Chip errors, and the
  full wiring reaching `probe-rs download` with the right chip and ELF. HARDWARE-VERIFIED on
  the po13 the same day: download + reset in 4.56 s (the openocd sequence spends longer than
  that in adapter-speed retries alone), and the console's uptime immediately after read 45 s
  with the LED toggle count corroborating -- the reset genuinely rebooted into the new image.
  The fast path is now the recommended way to get an image running on a docked board; one
  wrinkle found on the way: a per-project wrapper
  forwards a NAMED parameter list, so a new shared-script switch reaches nobody until each
  wrapper forwards it -- light_mk4's does, the sibling projects' will when they take Chip
  entries.
- 2026-08-31 — **`light-power`: mk3's power layer joins the framework.** The model
  (`Power<S: PowerSource>`) carries every judgement mk3 made once: the SAFE-BY-DEFAULT
  request ceiling that starts at the USB-C 5V rail (raising it is a claim about the board's
  wiring, logged as a warning because the evidence of a wrong claim is a dead board), the
  request-as-state machine (a selection is a message to a negotiation, not a call that
  succeeds -- Pending until a poll sees the contract, Refused at 1.5 s, measured), stable
  profile indices with availability as a property, and find/select that can never disagree.
  The HUSB238 driver keeps the bench findings as structure: writes only through the strictly
  framed `write_register_byte` (the repeated-START path was measured storing nothing while
  acknowledging everything), SEL-then-GO ordering, the default-rail-is-not-a-contract
  distinction (PD_STATUS0 0x13 with SEL 0x00, observed), absent PDOs zeroed rather than
  decoded, silence as a resting state. Twelve host tests against a scripted source and a
  register-file fake, including the 45W arithmetic corroboration of the current table.
  Host-tested only: hardware verification waits for a rig powered through the part, and the
  ceiling warning from mk3's memory stands -- re-check it if the PD output is ever rewired.
- 2026-08-31 — **the touch169 verifies the refactors too.** BOOTSEL flash through the
  1200-baud reset, then the full CLI session over its CDC: the table-assembled help, stats
  answered by four modules (display frame timings, the touch controller's NACK counters, the
  IMU live at FaceUp), `ui press 120 140` landing on a button, backlight PWM levels, render
  pause/resume, and both error paths. With the po13's session below, every refactor since the
  crate split has now run on both RP2350 boards.
- 2026-08-31 — **the po13 verifies the refactors, and grows a UART console.** Everything since
  the crate split -- the split itself, `ConstStaticCell` in place of every `static mut`,
  portable-atomic, the board-wiring move, the shared CLI -- had been build-verified only on
  the RP2350 boards. The po13 now runs it all: the widget demo on the glass, and a full CLI
  session over the debug probe's UART -- help assembled from the table, stats answered by
  three modules over the bus, `ui activate` toggling a button, usage-on-error, unknown-command,
  loglevel round-trip. Two findings. FIRST: flashing over SWD failed with "[rp2350.cm0]
  Examination failed" because the board's last firmware was the RISC-V build -- the chip boots
  with the Hazard3 cores selected and the ARM debug config cannot examine cm0. The recovery is
  openocd's rescue reset (`-c "set RESCUE 1" -f target/rp2350.cfg`), which resets everything
  except the debug port, clearing ARCHSEL; the normal flash then goes through. Remember it
  whenever an RP2350 last ran the other ISA. SECOND: the device-role shell now enables the
  UART console alongside CDC -- a board in the SWD dock without its own USB cabled is served
  by the probe's CDC-UART bridge, and crossfire's host-role console had already retired the
  mk3-era doubt about that path. The port crates had grown `boards`
  modules -- the touch169, the po13 rig with its Pico-OLED-1.3 expansion board, the two WeAct
  STM32 boards -- which baked one bench's hardware combinations into the framework. Gone: a
  port crate now stops at the CHIP (gpio, buses, pwm, clock, critical section, and unsafe
  constructors with a construct-once contract), and each application carries `src/board.rs`
  -- its pins, its measured offsets, its taken-once peripheral set. Another user's Pico
  wearing different hardware writes their own forty lines of wiring and touches nothing in
  a crate. This is assessment decision 2's second half, done properly: the port is one axis,
  and the board layer only instantiates drivers -- and it dropped light-rp2's dependency on
  light-input, which existed only to name the touch169's IMU mounting.
