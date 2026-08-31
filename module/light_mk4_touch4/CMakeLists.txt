#   the Waveshare RP2350-Touch-LCD-4: the widget demo on the 480x480 RGB (DPI) panel -- the
# ST7701S's first bring-up, fed by the pure-hardware PIO scanout engine.
add_executable(light_mk4_touch4
        ../light_mk4_shell/src/main.c
)

target_link_libraries(light_mk4_touch4 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_touch4
)

#   the demo's font: the 4" square glass (480x480 over ~71.9x71.9 mm), 16 px
light_mk4_add_font(light_mk4_touch4_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch4 WIDTH 480 HEIGHT 480 DIMENSION 71.9x71.9
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_touch4 ENV LIGHT_FONT_LGF
)

light_mk4_shell_configure(light_mk4_touch4)
