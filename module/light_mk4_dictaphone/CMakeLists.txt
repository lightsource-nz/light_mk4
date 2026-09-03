#   the dictaphone: a second application for the Waveshare RP2350-Touch-LCD-3.49 -- the
# same board and shell as light_mk4_touch349, built in the same tree, flashed as its own
# UF2. A voice recorder with a dedicated touch interface over the codec + TF-slot leg the
# demo brought up.
add_executable(light_mk4_dictaphone
        ../light_mk4_shell/src/main.c
)

target_link_libraries(light_mk4_dictaphone PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_dictaphone
)

#   the same font as the board's demo: the 3.49" bar's real geometry, 16 px
light_mk4_add_font(light_mk4_dictaphone_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch349 WIDTH 172 HEIGHT 640 DIMENSION 23.0x85.6
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_dictaphone ENV LIGHT_FONT_LGF
)

light_mk4_shell_configure(light_mk4_dictaphone)
