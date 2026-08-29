#   crossfire on the po13 rig: a Pico 2 whose native USB port HOSTS USB-MIDI instruments (through
# an OTG adapter, one device or a hub), forwarding every instrument's MIDI to every other, with
# the Pico-OLED-1.3 as the status display and the console on the UART through the debug probe.
# mk3's crossfire_main pico2 configuration, on the same shell as the other boards in its host
# role -- see light_mk4_shell_configure.
add_executable(light_mk4_crossfire
        ../light_mk4_shell/src/main.c
)

#   tusb_config.h: the host stack's configuration, on the include path for the shell (which
# initialises the stack) and for TinyUSB itself
target_include_directories(light_mk4_crossfire PRIVATE ${CMAKE_CURRENT_LIST_DIR}/include)

target_link_libraries(light_mk4_crossfire PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_crossfire
)

#   the status font: 16 px on the po13's real geometry, as mk3's crossfire used -- two lines
# of ten cells across the rotated 128-pixel row
light_mk4_add_font(light_mk4_crossfire_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY po13 WIDTH 64 HEIGHT 128 DIMENSION 17.2x32.3
        POINT_SIZE 12 PIXEL_SIZE 16
        CRATE light_app_crossfire ENV LIGHT_FONT_LGF
)

light_mk4_shell_configure(light_mk4_crossfire USB_HOST)
