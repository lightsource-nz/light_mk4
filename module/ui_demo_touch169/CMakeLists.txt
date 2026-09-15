#   the firmware executable is a pico-sdk one in every respect -- runtime, linker script, boot2,
# UF2 output -- with the Rust staticlib (imported by the root CMakeLists through Corrosion) as
# just another library on its link line. The C shell is shared with every board's executable
add_executable(ui_demo_touch169
        ../light_mk4_shell/src/main.c
)

target_link_libraries(ui_demo_touch169 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_ui_demo_touch169
)

#   the demo's font: the face, size and panel geometry this board's demo renders
# (240x280 over 27.9x32.6 mm, 14 pt, 16 px). The Rust app embeds the blob through
# LIGHT_FONT_LGF -- see light_mk4_add_font for how the two builds are kept in step
light_mk4_add_font(ui_demo_touch169_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch169 WIDTH 240 HEIGHT 280 DIMENSION 27.9x32.6
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_ui_demo_touch169 ENV LIGHT_FONT_LGF
)

#   the demo's look-and-feel: the framework's default (colour) theme plus this GLASS's
# measured corner curvature -- see theme/round169.json and the THEME_BLOB note in the Rust app
light_mk4_add_theme(ui_demo_touch169_theme
        THEME ${CMAKE_CURRENT_LIST_DIR}/theme/round169.json
        CRATE light_app_ui_demo_touch169 ENV LIGHT_THEME_LTH
)

#   the demo interface as data: the app crate's design.json extends light_app_ui_demo's shared
# design (overriding device size, gap and list-row height) and compiles to an LUI blob.
light_mk4_add_ui(ui_demo_touch169_ui
        UI ${CMAKE_CURRENT_LIST_DIR}/../../crates/light_app_ui_demo_touch169/design.json
        CRATE light_app_ui_demo_touch169 ENV LIGHT_UI_LUI
)

light_mk4_shell_configure(ui_demo_touch169)
