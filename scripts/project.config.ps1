# Per-project defaults for the mk4 spike.
#
# Two trees: the firmware for the one board the spike targets, and a host tree whose only job is
# to run `cargo test` under ctest so test.ps1 and CI need no Rust-specific path.
@{
        Name = 'light_mk4'

        Trees = @{
                'conf-light_mk4-host-debug'     = 'build-host'
                'conf-light_mk4-touch169-debug' = 'build-touch169'
        }

        Targets = @{
                # uf2 because the 1.69 exposes no SWD pads
                'light_mk4_touch169' = @{ Preset = 'conf-light_mk4-touch169-debug'; Flash = 'uf2' }
        }

        Expect = @{
                'conf-light_mk4-host-debug'     = @{ LIGHT_PLATFORM = 'HOST'; LIGHT_SYSTEM = 'HOST_OS' }
                'conf-light_mk4-touch169-debug' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_1.69'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
        }

        DefaultTarget = 'light_mk4_touch169'

        Test = @{
                Preset = 'conf-light_mk4-host-debug'
                Ctest  = $true
        }
}
