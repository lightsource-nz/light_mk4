# mk3 → mk4 migration ledger

What each mk3 consumer's modules became, module by module, so the state of the migration is
explicit rather than reconstructed from the README log. Three states:

- **ported** — exists in mk4 and has run on the hardware it was written for;
- **pending** — worth porting, not yet done, with the reason it waits;
- **retired** — not coming across, with the reason.

The rule from the assessment holds throughout: a driver is not ported until it has lit its
panel (or read its part) under mk3. Anything mk3 never verified is not framework code, it is
a guess, and mk4 does not inherit guesses.

## light_framework_mk3

| mk3 module | mk4 | state | note |
|---|---|---|---|
| light_core (objects, modules, tasks) | `light-core::module`, `events`, `mailbox` | ported | explicit registration, typed event bus; the kobject tree and refcounts are gone by design |
| light_core mqueue (logging) | `light-core::log` | ported | bounded, drop-with-counter, never blocks |
| light_cli | `light-core::cli` + `console::LineReader` | ported | one grammar: the `Cli` owns echo, help, loglevel, quit and usage-on-error; an app supplies a table of commands parsing into its event type. Not mk3's 26-command tree machinery — that died with decision 3, and nothing misses it |
| light_ioport (SPI, I2C, 3-wire, PIO-SPI) | `light-core::hal` traits + port crates | ported | SPI display bus and I2C only; PIO-SPI and 3-wire wait for a board that needs them |
| light_core_chip_rp2350 | `light-rp2` (feature `rp2350`; ARM + Hazard3) | ported | |
| light_core_chip_rp2040 | `light-rp2` (feature `rp2040`) | ported | one source with the RP2350 port; hw-verified in the po13 dock (demo + crossfire, DMA display, dual-core USB host). The one chip difference the port missed -- 12 DMA channels, not 16 -- was board wiring, and is chip-cfg'd there now |
| light_core_chip_rp2_common | `light-rp2` | ported | the common code is the whole crate; the chip is three `cfg` lines |
| light_core_chip_stm32h743 | `light-stm32h7` + CMSIS shell | ported | |
| light_core_chip_stm32f411 | `light-stm32f4` + CMSIS shell | ported | console verified by pin state only (no VCP wiring) |
| light_core_chip_stm32f446, stm32f103 | — | retired | no board on the bench; the F4 crate covers the F446 in an afternoon if one appears |
| light_core_arch_host_os | `cargo test` | ported | the host tree is the test tier |
| light_core_board_pico_hostmode | — | retired | pico-sdk hostmode existed to run C on the host; the mocked-hal tests do that job |
| platform/ (board headers) | each application's `src/board.rs` | ported | wiring is code, owned once, taken once -- and it is the application's, never the framework's: a port crate stops at the chip |
| scripts/, presets, CI workflow | unchanged, consumed | ported | `light-tools.ps1` adds cargo; nothing else changed |

## light_display

| mk3 module | mk4 | state | note |
|---|---|---|---|
| light_draw | `light-draw` | ported | |
| light_display (core, chunk model) | `light-display::display` | ported | the three documented bugs are tests |
| light_display_st7789 | `light-display::st7789` | ported | touch169 |
| light_display_sh1107 | `light-display::sh1107` | ported | po13 |
| light_display_st7735 | `light-display::st7735` | ported | MiniSTM32H7 |
| light_display_po13 (rig) | `light_mk4_pico2` | ported | |
| light_backlight | `light-rp2::pwm` | ported | a PWM level, not a switch |
| light_display_ssd1351 / light_display_ws15rgb | — | pending | hw-verified under mk3 (2026-08-23): port when the board is next on the bench. Needs the three-layer controller/panel/rig split kept |
| light_display_sh1106 | — | pending | two rigs built under mk3, neither hardware-verified; verify in C first |
| light_display_ssd1322 | — | retired (for now) | never lit a panel under mk3; three of five wires were never connected. Not framework code until it is |

## light_ui

| mk3 module | mk4 | state | note |
|---|---|---|---|
| light_ui | `light-ui` | ported | pages, stacks, buttons, scrolls, focus, touch, animations |
| light_canvas | `light-display::frames` | ported | the frame layer; carry-forward rule kept |
| light_touch | `light-input::touch` | ported | |
| light_touch_cst816t | `light-input::cst816t` | ported | with the wedge as an open hardware question |
| light_button | `light-core::button` | ported | |
| light_imu, light_imu_qmi8658 | `light-input::imu`, `qmi8658` | ported | |
| light_ui_demo_touch169, _po13 | `light_mk4_touch169`, `light_mk4_pico2` | ported | |
| light_ui_hw_ws_touch169, _po13 | the apps' `src/board.rs` | ported | board wiring lives with the application |
| light_audio | — | pending | hw-verified under mk3 (PCM + tone on a PWM pin); a `light-rp2::pwm` client. Port when a rig wants sound |
| light_touch_cst328, light_ui_demo_touch28, light_ui_hw_ws_touch28 | `light-input::cst328` + `light_mk4_touch28` (+ its `board.rs`) | ported | hw-verified 2026-08-31: taps hit, drags scroll, swipe-back works, IMU axis map measured -- and zero CST328 read failures under the load that wedges the 1.69's CST816T |
| light_ui_demo_ws15rgb, light_ui_hw_ws15rgb | — | pending | with the SSD1351 |

## new boards (no mk3 counterpart)

| board | mk4 | state | note |
|---|---|---|---|
| Waveshare RP2350-Touch-LCD-3.49 (AXS15231B QSPI) | `light-rp2::qspi` + `light-display::axs15231b` + `light-input::axs15231b` + `light_mk4_touch349` | ported | hw-verified 2026-08-31: panel lit first flash, taps/drags/swipe-back on the glass, IMU axis map measured. First QSPI panel, first RP2350B board, first upper-bank GPIO use. Two driver findings: the touch half is consume-on-read, so release is inferred from report silence, never read; and the panel ignores partial windowing (per-row RAMWR wedges it, RASET row offsets land at row 0), so every push is a full frame. Peripherals filled in 2026-08-31: battery ADC + the SYS_EN power latch (`light-rp2::adc`, first ADC use), PCF85063A RTC (new `light-rtc` crate, hw-verified keeping time across power loss), ES8311 audio (new `light-audio` crate + `light-rp2::i2s` -- codec as I2S master, ping-pong DMA stream after polled FIFO feeding chopped audibly), and the TF slot (new `light-sd` crate over the new `SpiBus` trait: SPI-mode identify + block reads, hw-verified against a 64 GB SDXC). PSRAM: MEASURED ABSENT -- the demo pack's library is family boilerplate, not fitment; the SDK auto-detect stays wired for a fitted variant. Filesystem landed 2026-09-03: `light-fs` (FAT16/32 over the new `BlockDevice` trait) hw-verified against a Raspberry Pi boot SD in this slot -- `fs info|ls|cat|write` on the console, read AND write (create/append/seek, write-through with FAT mirroring, LFN read). Remaining: the codec's microphone half |
| Waveshare RP2350-Touch-LCD-4 (ST7701S RGB, GT911) | `light-rp2::rgb` + `light-display::{scanout,st7701s}` + `light-input::gt911` + `light_mk4_touch4` | ported | hw-verified 2026-08-31: test pattern and widget demo on the glass, touch on target. The RGB scanout engine leg: four hand-assembled PIO programs (timing entirely in hardware) fed by a ZERO-IRQ two-channel DMA control-block loop -- deliberately not the vendor's IRQ-restarted chunks. 480x480 RGB565 single-buffered in SRAM (450 KB; the flip hook exists for a second buffer). GT911 grew `I2cBus::write_register16`. Two bring-up findings: the reference PIO programs carry a latent DE handshake race (a level-wait re-passes on the input synchronizer's stale high and DE collapses to a runt pulse the panel never sees; fixed with an edge wait), and 240 MHz is a REQUIREMENT (at stock 150 the fractional PCLK divider's stutter leaves the picture wavery -- measured on the glass). Battery divider measured ÷2 against the vendor's ÷3 formula. Single-buffer tearing solved 2026-09-01 by racing the beam instead of buying a buffer (900 KB does not fit a 520 KB chip): `rgb::beam_row()` + `Ui::dirty_bounds()` + `FrameLayer::to_physical()` gate every draw against the scan position, hw-verified clean. IMU axis map measured 2026-09-01 (three observations + one glass correction: the horizontal signs came out 180 degrees off because the poses were described in the holder's frame -- flip both, never one, to keep the determinant +1). Remaining: buzzer and TF slot undriven |

## light_usb

| mk3 module | mk4 | state | note |
|---|---|---|---|
| light_usbhost, light_usbhost_midi | `light-rp2::tinyusb_midi` + `light-midi` | ported | TinyUSB stays C; callbacks cross into Rust; the fork's four host fixes are local commits |
| light_usb (device), light_usb_midi | — | pending | the CDC console is in the C shell; a MIDI *device* role has no consumer yet |

## screen-test

| mk3 module | mk4 | state | note |
|---|---|---|---|
| screentest_ws_touch169, _po13, _mini_stm32h7 (+hw) | the three mk4 apps | ported | |
| screentest_common | — | retired | mk3-CLI scaffolding |
| light_power, light_power_husb238 | `light-power` (+ `husb238`) | ported (host-tested only) | in the framework proper, as decided; every mk3 bench finding is a test. Hardware verification waits for a rig powered through the part again |
| screentest_husb238 | — | retired | its job was corroborating the register map against light_power's view; the map's findings live in the driver's comments and tests now |
| screentest_calib169 | — | pending | touch calibration; wants the wedge resolved first |
| screentest_sh1106_i2c, _spi4 | — | pending | with the SH1106 |
| screentest_ws15rgb | — | pending | with the SSD1351 |
| screentest_ssd1322 | — | retired | with the SSD1322 |

## crossfire

| mk3 module | mk4 | state | note |
|---|---|---|---|
| crossfire_main (USB host side) | `light_mk4_crossfire` | ported | hub mode, reconnects, controller reset |
| SPI link (Pico 2 → H7) | — | pending | the engine models `Kind::Link`; needs the two boards wired together on the bench |
| the H7 half | — | pending | after the link |

## font-crusher

| mk3 module | mk4 | state | note |
|---|---|---|---|
| crush, crush_render_backend, libcrush_render/font | `tools/crush` + `light-font` (LGF) | ported | the generated-C coupling is gone |
| libcrush_common/context/display/module, mod_freetype, mod_jansson | — | retired | mk3-CLI scaffolding and vendored C; `freetype-sys` and `serde_json` do the job |

## What is not on any list

- Nothing. The RP2040 verification closed the last product-gating item; what remains pending
  in the tables above waits on specific hardware reaching the bench, not on the framework.
