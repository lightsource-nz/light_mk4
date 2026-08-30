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
    crates/light-rp2350     RP2350 port: rp235x-pac, both ISAs, TinyUSB host transport (usb-host)
    crates/light-stm32h7    STM32H743 port, raw registers over bare CMSIS
    crates/light-stm32f4    STM32F411 port, the same shape
    tools/crush             font-crusher in Rust: renders TrueType into LGF (and mk3's C pair)
    tools/vendor            freetype-sys, vendored with a one-line build.rs fix (see Cargo.toml)
    module/light_mk4_shell        the pico-sdk C shell (device and USB-host roles)
    module/light_mk4_shell_cmsis  the bare-CMSIS C shell (H743, F411)
    module/light_mk4_<board>/rust the staticlib crate a board's firmware links (light_app_<board>)
    module/light_mk4_<board>      the board's executable (module/<target>/ is where
                                  light-flash.ps1 looks for <target>.uf2)
    scripts/                the usual thin wrappers over $LIGHT_PATH/scripts
    .github/workflows       the host test suite through the framework's shared workflow

The port crates are target-only and are excluded from the host `cargo test` along with the
app crates: a workspace-wide invocation unifies features, and the two `critical-section`
flavours (cortex-m's single-core one in the STM32 ports, light-rp2350's own) cannot coexist.

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
