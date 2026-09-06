#   the dictaphone on the Waveshare RP2350-Touch-LCD-3.49: a voice recorder with a
# dedicated touch interface over the board's ES8311 codec, microphone and TF slot. The
# application is the hardware-independent light_app_dictaphone crate; this module is its
# 3.49 instantiation. Same board and shell configuration as light_mk4_touch349, built in
# the same tree, flashed as its own UF2.
#
#   the executable name matches the module directory: light-flash.ps1 and light-debug.ps1
# resolve module/<target>/<target>.{uf2,elf}
add_executable(dictaphone_touch349
        ../light_mk4_shell/src/main.c
)

target_link_libraries(dictaphone_touch349 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        dictaphone_touch349_app
)

#   the same font as the board's demo: the 3.49" bar's real geometry, 16 px
light_mk4_add_font(dictaphone_touch349_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch349 WIDTH 172 HEIGHT 640 DIMENSION 23.0x85.6
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE dictaphone_touch349_app ENV LIGHT_FONT_LGF
)

#   the framework's default look-and-feel for a color board (steel); a board override
# would be a local THEME file extending "default"
light_mk4_add_theme(dictaphone_touch349_theme
        CRATE dictaphone_touch349_app ENV LIGHT_THEME_LTH
)

light_mk4_shell_configure(dictaphone_touch349)
