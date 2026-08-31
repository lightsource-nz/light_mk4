# Per-project defaults for the mk4 spike.
#
# Three trees: the touch169 firmware (BOOTSEL-flashed, the board has no SWD pads), the bare
# Pico 2 in the po13 rig's SWD dock (flashed over SWD by debug.ps1 -Batch), and a host tree whose
# only job is to run `cargo test` under ctest so test.ps1 and CI need no Rust-specific path.
@{
        Name = 'light_mk4'

        Trees = @{
                'conf-light_mk4-host-debug'     = 'build-host'
                'conf-light_mk4-touch169-debug' = 'build-touch169'
                'conf-light_mk4-touch28-debug'  = 'build-touch28'
                'conf-light_mk4-touch349-debug' = 'build-touch349'
                'conf-light_mk4-pico2-debug'    = 'build-pico2'
                # the release profile (opt-level s, fat LTO), as separate trees so the debug
                # ones stay warm: build/flash with -Preset conf-light_mk4-<board>-release
                'conf-light_mk4-touch169-release' = 'build-touch169-release'
                'conf-light_mk4-pico2-release'    = 'build-pico2-release'
                # crossfire: the Pico 2 with its USB port in the host role, own tree
                'conf-light_mk4-crossfire-debug'  = 'build-crossfire'
                # the RP2040 Pico: the po13 demo and crossfire (the product board), each
                # its own tree; build/flash with -Preset
                'conf-light_mk4-pico-debug'           = 'build-pico'
                'conf-light_mk4-crossfire-pico-debug' = 'build-crossfire-pico'
                # the same boards on the RP2350's Hazard3 cores: build/flash with -Preset
                'conf-light_mk4-pico2-riscv-debug'    = 'build-pico2-riscv'
                'conf-light_mk4-touch169-riscv-debug' = 'build-touch169-riscv'
                # the MiniSTM32H7 on bare CMSIS, flashed over its ST-Link
                'conf-light_mk4-mini-stm32h7-debug'   = 'build-mini-stm32h7'
                'conf-light_mk4-blackpill-debug'      = 'build-blackpill'
        }

        Targets = @{
                'light_mk4_h7'   = @{ Preset = 'conf-light_mk4-mini-stm32h7-debug'; Flash = 'swd' }
                'light_mk4_f411' = @{ Preset = 'conf-light_mk4-blackpill-debug'; Flash = 'swd' }
                # the po13 rig again, flashed over SWD like the pico2 demo
                'light_mk4_crossfire' = @{ Preset = 'conf-light_mk4-crossfire-debug'; Flash = 'swd' }
                # uf2 because the 1.69 exposes no SWD pads
                'light_mk4_touch169' = @{ Preset = 'conf-light_mk4-touch169-debug'; Flash = 'uf2' }
                'light_mk4_touch28'  = @{ Preset = 'conf-light_mk4-touch28-debug'; Flash = 'uf2' }
                'light_mk4_touch349' = @{ Preset = 'conf-light_mk4-touch349-debug'; Flash = 'uf2' }
                # the po13 rig flashes over SWD; Flash='swd' records that light-flash.ps1's
                # BOOTSEL path is not how an image reaches it
                'light_mk4_pico2'    = @{ Preset = 'conf-light_mk4-pico2-debug'; Flash = 'swd' }
        }

        Expect = @{
                'conf-light_mk4-host-debug'     = @{ LIGHT_PLATFORM = 'HOST'; LIGHT_SYSTEM = 'HOST_OS' }
                'conf-light_mk4-touch169-debug' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_1.69'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
                'conf-light_mk4-touch28-debug'  = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_2.8'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
                'conf-light_mk4-touch349-debug' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_3.49'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
                'conf-light_mk4-pico2-debug'    = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'pico2'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
                'conf-light_mk4-touch169-release' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_1.69'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                        CMAKE_BUILD_TYPE  = 'Release'
                }
                'conf-light_mk4-pico2-release'    = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'pico2'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                        CMAKE_BUILD_TYPE  = 'Release'
                }
                'conf-light_mk4-crossfire-debug'  = @{
                        LIGHT_PLATFORM      = 'TARGET'
                        LIGHT_BOARD         = 'pico2'
                        PICO_PLATFORM       = 'rp2350-arm-s'
                        Rust_CARGO_TARGET   = 'thumbv8m.main-none-eabi'
                        LIGHT_MK4_PICO2_APP = 'crossfire'
                }
                'conf-light_mk4-pico2-riscv-debug'    = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'pico2'
                        PICO_PLATFORM     = 'rp2350-riscv'
                        Rust_CARGO_TARGET = 'riscv32imac-unknown-none-elf'
                }
                'conf-light_mk4-pico-debug'           = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'pico'
                        PICO_PLATFORM     = 'rp2040'
                        Rust_CARGO_TARGET = 'thumbv6m-none-eabi'
                }
                'conf-light_mk4-crossfire-pico-debug' = @{
                        LIGHT_PLATFORM      = 'TARGET'
                        LIGHT_BOARD         = 'pico'
                        PICO_PLATFORM       = 'rp2040'
                        Rust_CARGO_TARGET   = 'thumbv6m-none-eabi'
                        LIGHT_MK4_PICO2_APP = 'crossfire'
                }
                'conf-light_mk4-touch169-riscv-debug' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_1.69'
                        PICO_PLATFORM     = 'rp2350-riscv'
                        Rust_CARGO_TARGET = 'riscv32imac-unknown-none-elf'
                }
                'conf-light_mk4-mini-stm32h7-debug'   = @{
                        LIGHT_SYSTEM      = 'CMSIS'
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'mini_stm32h7'
                        Rust_CARGO_TARGET = 'thumbv7em-none-eabihf'
                }
                'conf-light_mk4-blackpill-debug'      = @{
                        LIGHT_SYSTEM      = 'CMSIS'
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'blackpill'
                        Rust_CARGO_TARGET = 'thumbv7em-none-eabihf'
                }
        }

        # which OpenOCD config and SVD belong to which board -- see screen-test's config for
        # why getting this pairing wrong misbehaves rather than erroring
        Debug = @{
                # Chip is probe-rs's name for the part, for debug.ps1 -ProbeRs (fast
                # flash-and-run; no openocd, no spinlock-31 contamination)
                'conf-light_mk4-pico2-debug' = @{
                        Config = 'openocd-rp2350.cfg'
                        Svd    = '../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x'
                }
                'conf-light_mk4-pico2-release' = @{
                        Config = 'openocd-rp2350.cfg'
                        Svd    = '../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x'
                }
                'conf-light_mk4-crossfire-debug' = @{
                        Config = 'openocd-rp2350.cfg'
                        Svd    = '../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x'
                }
                'conf-light_mk4-pico2-riscv-debug' = @{
                        Config = 'openocd-rp2350.cfg'
                        Svd    = '../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x_riscv'
                }
                'conf-light_mk4-pico-debug' = @{
                        Config = 'openocd-rp2040.cfg'
                        Svd    = '../pico-sdk/src/rp2040/hardware_regs/RP2040.svd'
                        Chip   = 'RP2040'
                }
                'conf-light_mk4-crossfire-pico-debug' = @{
                        Config = 'openocd-rp2040.cfg'
                        Svd    = '../pico-sdk/src/rp2040/hardware_regs/RP2040.svd'
                        Chip   = 'RP2040'
                }
                # over the ST-Link; no SVD vendored for this part
                'conf-light_mk4-mini-stm32h7-debug' = @{
                        Config = 'openocd-stm32h7.cfg'
                        Chip   = 'STM32H743VI'
                }
                'conf-light_mk4-blackpill-debug' = @{
                        Config = 'openocd-stm32f4.cfg'
                        Chip   = 'STM32F411CE'
                }
        }

        DefaultTarget = 'light_mk4_touch169'

        Test = @{
                Preset = 'conf-light_mk4-host-debug'
                Ctest  = $true
        }
}
