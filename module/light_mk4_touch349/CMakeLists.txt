#   the Waveshare RP2350-Touch-LCD-3.49: the widget demo on the 172x640 QSPI bar display --
# the AXS15231B's first bring-up, and the first board on the RP2350's upper GPIO bank.
add_executable(light_mk4_touch349
        ../light_mk4_shell/src/main.c
)

target_link_libraries(light_mk4_touch349 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_touch349
)

#   the demo's font: the 3.49" bar's real geometry (172x640 over 23.0x85.6 mm), 16 px
light_mk4_add_font(light_mk4_touch349_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch349 WIDTH 172 HEIGHT 640 DIMENSION 23.0x85.6
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_touch349 ENV LIGHT_FONT_LGF
)

light_mk4_shell_configure(light_mk4_touch349)
