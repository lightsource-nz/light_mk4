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
                'conf-light_mk4-pico2-debug'    = 'build-pico2'
        }

        Targets = @{
                # uf2 because the 1.69 exposes no SWD pads
                'light_mk4_touch169' = @{ Preset = 'conf-light_mk4-touch169-debug'; Flash = 'uf2' }
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
                'conf-light_mk4-pico2-debug'    = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'pico2'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
        }

        # which OpenOCD config and SVD belong to which board -- see screen-test's config for
        # why getting this pairing wrong misbehaves rather than erroring
        Debug = @{
                'conf-light_mk4-pico2-debug' = @{
                        Config = 'openocd-rp2350.cfg'
                        Svd    = '../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                }
        }

        DefaultTarget = 'light_mk4_touch169'

        Test = @{
                Preset = 'conf-light_mk4-host-debug'
                Ctest  = $true
        }
}
