#   the Waveshare RP2350-Touch-LCD-3.49 displaying a design compiled to an LUI blob -- the firmware
# consumer of the UI-as-data pipeline. The design is authored as JSON, compiled by crush to a binary
# blob, embedded, and read + displayed by light-ui's `lui` reader. No hand-written widget tree.
add_executable(light_mk4_lui_touch349
        ../light_mk4_shell/src/main.c
)

target_link_libraries(light_mk4_lui_touch349 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_lui_touch349
)

#   the font: the 3.49" bar's geometry (172x640 over 23.0x85.6 mm), 16 px
light_mk4_add_font(lui_touch349_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch349 WIDTH 172 HEIGHT 640 DIMENSION 23.0x85.6
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_lui_touch349 ENV LIGHT_FONT_LGF
)

#   the framework's default colour look (steel)
light_mk4_add_theme(lui_touch349_theme
        CRATE light_app_lui_touch349 ENV LIGHT_THEME_LTH
)

#   the UI itself, compiled from its JSON design to an LUI blob
light_mk4_add_ui(lui_touch349_ui
        UI ${CMAKE_CURRENT_LIST_DIR}/design.json
        CRATE light_app_lui_touch349 ENV LIGHT_UI_LUI
)

light_mk4_shell_configure(light_mk4_lui_touch349)
