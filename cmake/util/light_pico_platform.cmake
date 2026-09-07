# light_pico_platform.cmake
#   resolves PICO_PLATFORM from the light board and arch selection.
#
#   this mapping is needed in two places that run at very different times -- once from
# external/light_preinit.cmake, before the consuming project even calls project(), and again
# from light_load_pico_sdk_support() in pico_sdk.cmake, right before the pico_sdk_init() call
# that actually consumes it. it used to be copy-pasted between them, with a comment in each
# noting that the two "have to agree"; a mapping that has to agree with its own duplicate is
# one edit away from not agreeing, so it lives here now and both call sites call this.
#
#   pico-sdk does NOT derive PICO_PLATFORM from PICO_BOARD on its own (its own default is
# unconditionally rp2040 -- see pico-sdk/cmake/pico_pre_load_platform.cmake), which is why
# this has to exist at all.
macro(light_resolve_pico_platform OUT_VAR BOARD ARCH)
        if("${BOARD}" STREQUAL pico2 OR "${BOARD}" STREQUAL pico2_w
                        OR "${BOARD}" STREQUAL waveshare_rp2350_touch_lcd_1.69
                        OR "${BOARD}" STREQUAL waveshare_rp2350_touch_lcd_2.8
                        OR "${BOARD}" STREQUAL waveshare_rp2350_touch_lcd_3.49
                        OR "${BOARD}" STREQUAL waveshare_rp2350_touch_lcd_4)
                if("${ARCH}" STREQUAL riscv32)
                        # the RP2350's Hazard3 cores. pico-sdk's own
                        # cmake/preload/platforms/rp2350-riscv.cmake selects the RISC-V
                        # toolchain from this alone, so nothing else has to be set to switch
                        # ISA -- but the toolchain has to be findable, which on this machine
                        # means PICO_TOOLCHAIN_PATH, since the arm-none-eabi one is picked up
                        # off PATH and there is only ever one PATH
                        set(${OUT_VAR} rp2350-riscv)
                else()
                        # rp2350-arm-s, not a generic "rp2350" -- matches what tinyusb's own
                        # hw/bsp/rp2040/boards/raspberry_pi_pico2/board.cmake sets
                        set(${OUT_VAR} rp2350-arm-s)
                endif()
        else()
                set(${OUT_VAR} rp2040)
        endif()
endmacro()
