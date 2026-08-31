#   the Waveshare RP2350-Touch-LCD-4: the widget demo on the 480x480 RGB (DPI) panel -- the
# ST7701S's first bring-up, fed by the pure-hardware PIO scanout engine.
add_executable(light_mk4_touch4
        ../light_mk4_shell/src/main.c
        src/psram_info.c
)

target_link_libraries(light_mk4_touch4 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        hardware_psram
        light_app_touch4
)

#   PSRAM on the XIP CS1 pin, if this board variant carries one: the SDK's runtime init
# probes the chip ID before main and a board without the part degrades to "none" instead
# of bus-faulting. The `psram` console command reports what detection found.
target_compile_definitions(light_mk4_touch4 PRIVATE
        PICO_PSRAM_CS_PIN=47
        PICO_AUTO_DETECT_PSRAM_SIZE=1
)

#   the demo's font: the 4" square glass (480x480 over ~71.9x71.9 mm), 16 px
light_mk4_add_font(light_mk4_touch4_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch4 WIDTH 480 HEIGHT 480 DIMENSION 71.9x71.9
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_touch4 ENV LIGHT_FONT_LGF
)

#   the scanout NEEDS 240 MHz, like the vendor's examples: at stock 150 the PCLK divider is
# 150/32 = 4.6875 and the fractional stutter (+-6.7 ns on a 62.5 ns period) leaves the
# panel a wavery, distorted picture -- measured on the glass. 240/32 = 7.5 jitters less
# in absolute terms and the shorter cycles buy the data-to-PCLK setup margin back.
target_compile_definitions(light_mk4_touch4 PRIVATE
        SYS_CLK_KHZ=240000
        # 12 MHz XOSC * 120 = 1440 MHz VCO, / 6 / 1 = 240 MHz (vcocalc's answer)
        PLL_SYS_VCO_FREQ_HZ=1440000000
        PLL_SYS_POSTDIV1=6
        PLL_SYS_POSTDIV2=1
)

light_mk4_shell_configure(light_mk4_touch4)
