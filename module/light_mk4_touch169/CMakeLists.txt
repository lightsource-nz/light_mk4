#   the firmware executable is a pico-sdk one in every respect -- runtime, linker script, boot2,
# UF2 output -- with the Rust staticlib (imported by the root CMakeLists through Corrosion) as
# just another library on its link line. The C shell is shared with every board's executable
add_executable(light_mk4_touch169
        ../light_mk4_shell/src/main.c
)

target_link_libraries(light_mk4_touch169 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_touch169
)

#   the demo's font: the same face, size and panel geometry mk3's light_ui demo renders for this
# board (240x280 over 27.9x32.6 mm, 14 pt, 16 px). The Rust app embeds the blob through
# LIGHT_FONT_LGF -- see light_mk4_add_font for how the two builds are kept in step
light_mk4_add_font(light_mk4_touch169_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch169 WIDTH 240 HEIGHT 280 DIMENSION 27.9x32.6
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_touch169 ENV LIGHT_FONT_LGF
)

light_mk4_shell_configure(light_mk4_touch169)
