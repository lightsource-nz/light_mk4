#   the po13 rig: a Pico 2 in the SWD dock wearing the Pico-OLED-1.3. The same shell, a Rust app
# with the LED, the OLED and the console. Flashed and halted over SWD, by openocd or probe-rs
add_executable(light_mk4_pico2
        ../light_mk4_shell/src/main.c
)

target_link_libraries(light_mk4_pico2 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_pico2
)

#   the OLED's font: the Pico-OLED-1.3's real geometry (64x128 over 17.2x32.3 mm, mk3's po13
# display), at 12 px -- the 128-pixel-wide logical row fits ten cells of it
light_mk4_add_font(light_mk4_pico2_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY po13 WIDTH 64 HEIGHT 128 DIMENSION 17.2x32.3
        POINT_SIZE 6 PIXEL_SIZE 8
        CRATE light_app_pico2 ENV LIGHT_FONT_LGF
)

#   the framework's default look-and-feel for a MONO board (steel's focused fill
# collapses illegibly at 1 bpp), plus this rig's decorative 6 px outer rounding
light_mk4_add_theme(light_mk4_pico2_theme MONO
        THEME ${CMAKE_CURRENT_LIST_DIR}/theme/po13.json
        CRATE light_app_pico2 ENV LIGHT_THEME_LTH
)

light_mk4_shell_configure(light_mk4_pico2)
