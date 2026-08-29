# mk4 gate decision — 2026-08-29

**Decision: proceed with Rust for light framework mk4, in the hybrid form the spike built —
Rust `no_std` crates linked as a staticlib into an executable that pico-sdk's CMake owns.**
C++ is released as the fallback. C is not revisited.

This closes the spike opened on 2026-08-29 under the assessment in
`~/.claude/plans/i-m-considering-rewriting-the-unified-squirrel.md`. The gate it set was:
*items 1–4 pass and the C shim count in item 2 is small.* All five items are answered and the
shim count is zero.

## The gate, item by item

| # | Item | Result | Evidence |
|---|---|---|---|
| 1 | Rust staticlib through Corrosion, driven by the existing `light-*.ps1` presets and scripts; flash via `light-flash.ps1` unchanged | **Pass** | `scripts/build.ps1`, `flash.ps1`, `debug.ps1`, `test.ps1` are the same thin wrappers every mk3 project has; the only addition is `light-tools.ps1` putting `~/.cargo/bin` on PATH. `project.config.ps1` declares three trees. |
| 2 | Rust owns: explicit module registration, a task runner, a bounded never-blocking log queue, ST7789 over SPI+DMA with the async chunk protocol, CST816T touch; peripherals via `rp235x-pac`, shim count measured | **Pass, zero shims** | `light_core::{module, log, display, st7789, cst816t}`; `light_rp2350::{gpio, spi, i2c, critical}` — every register access is the pac. Hardware-verified on the touch169: 30 fps region updates, 0 chunk timeouts at 37.5 MHz, taps with coordinates. |
| 3 | C shell owns crt0/boot2/linker script, multicore launch, TinyUSB CDC console; console lines cross into Rust | **Pass** | `module/light_mk4_shell/src/main.c`: 130 lines, USB on core 1 as mk3 arranged it. Console round-trips every command; `quit` runs the orderly unload. |
| 4 | `cargo test` on the host with a mocked bus; Loom on the concurrent code | **Pass (Loom not needed yet)** | 32 tests. Three caught real bugs before hardware: the blinker's catch-up arithmetic, the display core's deadline semantics, the touch backoff cadence. The log queue has a 4-producer conservation test. Loom deferred: the critical section is the only shared-state primitive and it is exercised across two real cores. |
| 5 | Retire the unknowns: debug-halt NOCP, core-1 worker from Rust, probe-rs vs openocd | **All three answered** | Core 1 runs a Rust service under the C USB loop; the spinlock critical section holds under real two-core contention. probe-rs 0.32 flashes, resets and reads a running RP2350 through the debugprobe. OpenOCD halt/resume corrupts CPACR exactly as mk3 recorded (`0x00f0c303 → 0x0000c000`) and the Rust firmware is unaffected, because pac GPIO uses SIO registers, not the CP0 coprocessor. |

**FFI surface after the whole spike: five functions**, all in one C file —
`light_app_main`, `light_app_core1_service` (C→Rust); `light_shell_log`, `light_shell_read_byte`,
`light_shell_panic` (Rust→C). Nothing about a peripheral crosses it.

## What the spike found that the assessment did not predict

These are facts, each learned by breaking something, and each now recorded in the code.

1. **The Rust/pico-sdk seam itself was uneventful.** Once the cross target was
   `thumbv8m.main-none-eabi` (soft-float ABI, matching the SDK's `-mfloat-abi=softfp`; `eabihf`
   fails at link with a VFP-args mismatch), Corrosion just worked. The predicted pain — bindgen
   against `static inline` SDK headers — never arose because the pac made SDK calls unnecessary.
2. **All the friction was on the host.** The Rust host toolchain on Windows must be MSVC: with
   w64devkit's gcc first on PATH (which `light-env.ps1` guarantees) the gnu host cannot link
   proc-macro build scripts (`-lgcc_eh` missing), and cargo applies no rustflags to build
   scripts under `--target`, so no config fixes it. Also `pwsh -File` mangles array arguments,
   and .NET `SerialPort.Write(string)` turned every LF but the last into a space. None of this
   is Rust's; all of it is now written down.
3. **The host tests earned their keep on day one**, and the specific way they did — a mock that
   made the wrong assumption visible — is the argument for keeping the mocked-bus discipline as
   the primary test tier, with hardware as confirmation.
4. **mk3's source comments were the requirements spec, exactly as the assessment said.** The
   touch driver was ported first *without* the reset-recovery state machine and the panel went
   deaf after four taps — the failure mk3's comments describe, reproduced on the first run. The
   chunk protocol's three documented bugs became three tests. Nothing in mk4 should be designed
   without first reading what mk3 wrote about it.
5. **A polling loop 200× faster than mk3's changes driver behaviour.** The CST816T "read on the
   spot while INT is asserted" rule, written for a ~1 kHz loop, issued back-to-back reads for
   the whole INT pulse at ~236 k polls/s. A minimum gap between reads is now part of the
   driver. Any mk3 driver ported to mk4 needs its cadence assumptions checked, not copied.
6. **The CST816T wedge is a hardware/bus issue, not a port issue.** Timeouts under continuous
   rendering, INT still pulsing, recovery by reset every 4–8 taps — mk3's open stall, seen more
   often because this firmware never stops rendering. SPI at 10 MHz reduced it (17 clean taps
   vs 4–8) in one run. It needs a logic analyser on SCL/SDA/INT. It is not a reason to change
   the language decision, and it is not to be inferred at any further.

## What the spike is not

It is a spike. Before any of it becomes mk4:

- `light_rp2350` hardcodes clk_sys/clk_peri at 150 MHz, takes its DMA channel by convention,
  and steals peripherals with a doc comment as the only ownership rule. The real port needs a
  peripheral ownership story (one-shot constructors returning owned handles is the shape).
- `Board` is a two-method trait that grew to fit one board. The port interface needs designing
  from the list of primitives the spike actually used: output, input, SPI display bus, I2C,
  clock, delay, critical section, DMA-backed burst — and no more.
- Module-to-module communication is static `Mailbox`es. The typed event bus (assessment
  decision 6) is the next core design, and the console module is its first front-end.
- Logging formats at the producer into 96-byte records. Deferred formatting (`defmt`-style)
  is a later optimisation with the same queue contract.
- There is no `Board`/HAL for the STM32 targets, no RISC-V build, no font, no `light_draw`,
  no `light_ui`. Those come in the migration order below.
- `light_mk4` is one repo with two apps. The workspace/group layout (assessment decision 10)
  is a separate decision to make once font-crusher and the core exist.

## Plan forward

Order is by leverage and by how much each retires a risk, as in the assessment.

1. **font-crusher in Rust** — host-only, self-contained, proves the host-side story, and
   defines the **binary glyph blob** (assessment decision 8) that removes the generated-C
   coupling. Its 93 command-invocation tests are the acceptance suite. First real deliverable.
2. **`light-core` hardened from the spike** — port interface, event bus, deferred logging,
   the module runtime with a scheduler idle hook, and a `Board` for each of the two boards on
   the bench. The spike's tests carry over; the mutation-check discipline applies.
3. **Display stack**: `light_draw` (rasteriser, transforms, the Q15 rounding lesson) →
   `light_display` core from the spike's chunk protocol → ST7789 and SH1107 → `light_canvas`.
   Verified panels only; SSD1322 is not ported until it has lit a panel in C.
4. **Input**: `light_touch` + CST816T (with the logic-analyser session on the wedge as a
   parallel task), `light_button`, `light_imu` + QMI8658.
5. **`light_ui`**, on top of 3 and 4, then the screen-test rigs as its consumers.
6. **`light_usb`** last — the thinnest layer over TinyUSB, and the one that stays closest to C
   (TinyUSB callbacks are `extern "C"` into Rust; the host stack stays TinyUSB's).
7. **crossfire**, once 3–6 exist. It is the product; it moves when the platform is proven.

Throughout: `light_power`/HUSB238 move from screen-test into the framework proper, and the
script layer grows a `-Send` option on `light-console.ps1` and a probe-rs path in
`light-debug.ps1`, both of which the spike wanted.

## Toolchain baseline

Recorded so a second machine can be set up without rediscovery:

- rustup, **stable, MSVC host** (`stable-x86_64-pc-windows-msvc`; VS 2026 Community with the
  C++ toolset and Windows SDK), targets `thumbv8m.main-none-eabi` (+ `thumbv6m-none-eabi`,
  `riscv32imac-unknown-none-elf` for later), components `llvm-tools rust-src clippy rustfmt`.
- probe-rs 0.32 via the official installer; chip `RP235x`.
- Everything mk3 already needed: arm-none-eabi-gcc 14.2, w64devkit, pico-sdk 2.3, openocd
  0.12 xpack, PowerShell 7 — through `light-env.ps1` unchanged.
- Corrosion v0.6.1 by FetchContent; `Rust_CARGO_TARGET` from the preset.

## Rules that came out of the spike

Short, because each is already a comment at the place it applies:

- soft-float target; MSVC host; `~/.cargo/bin` via `light-tools.ps1`; delete the build tree
  after a toolchain change (FindRust caches it).
- pac write-1-to-clear bits are `clear_bit_by_one()`, not `set_bit()`.
- RP2350 pads power up isolated: clear `ISO` or the pin does nothing.
- probe the CST816T immediately after its reset pulse; it sleeps within a second.
- USB on core 1; core 0 never touches stdio; panics hand off to core 1 then drop to BOOTSEL.
- a region update must cover the union of what was drawn before and what is drawn now.
- a driver's cadence assumptions are checked against the new loop rate, not copied.
