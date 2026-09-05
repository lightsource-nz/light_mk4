#   the Waveshare RP2350-Touch-LCD-2.8: the same shell and demo as the touch169, on the
# bigger glass with the CST328 touch controller -- this firmware is that part's bring-up.
add_executable(light_mk4_touch28
        ../light_mk4_shell/src/main.c
)

target_link_libraries(light_mk4_touch28 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_touch28
)

#   the demo's font: the 2.8" panel's real geometry (240x320 over 43.2x57.6 mm), 16 px --
# lower DPI than the 1.69, so the same pixel size reads a little larger on the glass
light_mk4_add_font(light_mk4_touch28_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch28 WIDTH 240 HEIGHT 320 DIMENSION 43.2x57.6
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_touch28 ENV LIGHT_FONT_LGF
)

#   the demo's look-and-feel: a data file compiled to a blob the firmware embeds --
# restyling this app is an edit to theme/steel.json, nothing else
light_mk4_add_theme(light_mk4_touch28_theme
        THEME ${CMAKE_CURRENT_LIST_DIR}/theme/steel.json
        CRATE light_app_touch28 ENV LIGHT_THEME_LTH
)

light_mk4_shell_configure(light_mk4_touch28)
