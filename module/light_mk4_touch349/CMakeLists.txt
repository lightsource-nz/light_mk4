#   the Waveshare RP2350-Touch-LCD-3.49: the widget demo on the 172x640 QSPI bar display --
# the AXS15231B's first bring-up, and the first board on the RP2350's upper GPIO bank.
add_executable(light_mk4_touch349
        ../light_mk4_shell/src/main.c
        src/psram_info.c
)

target_link_libraries(light_mk4_touch349 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        hardware_psram
        light_app_touch349
)

#   the 8 MB PSRAM on the XIP CS1 pin: the SDK's runtime init brings it up before main.
# The CS pin is the board's wiring; the SIZE is auto-detected (read from the chip's ID),
# so a board variant without the part degrades to "none" instead of bus-faulting
target_compile_definitions(light_mk4_touch349 PRIVATE
        PICO_PSRAM_CS_PIN=47
        PICO_AUTO_DETECT_PSRAM_SIZE=1
)

#   the demo's font: the 3.49" bar's real geometry (172x640 over 23.0x85.6 mm), 16 px
light_mk4_add_font(light_mk4_touch349_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch349 WIDTH 172 HEIGHT 640 DIMENSION 23.0x85.6
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_touch349 ENV LIGHT_FONT_LGF
)

#   the framework's default look-and-feel for a color board (steel); a board override
# would be a local THEME file extending "default"
light_mk4_add_theme(light_mk4_touch349_theme
        CRATE light_app_touch349 ENV LIGHT_THEME_LTH
)

light_mk4_shell_configure(light_mk4_touch349)
