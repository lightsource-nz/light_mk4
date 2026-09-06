#   crossfire on the po13 rig: a Pico 2 whose native USB port HOSTS USB-MIDI instruments
# (through an OTG adapter, one device or a hub), forwarding every instrument's MIDI to every
# other, with the Pico-OLED-1.3 as the status display and the console on the UART through the
# debug probe. The application is the hardware-independent light_app_crossfire crate; this
# module is its Pico 2 instantiation -- see rust/src/board.rs and include/tusb_config.h.
#   the executable name matches the module directory: light-flash.ps1 and light-debug.ps1
# resolve module/<target>/<target>.{uf2,elf}
add_executable(crossfire_pico2
        ../light_mk4_shell/src/main.c
)

#   tusb_config.h: the host stack's configuration, on the include path for the shell (which
# initialises the stack) and for TinyUSB itself. This module's OWN copy: host-stack tuning
# is per tangible board, never shared
target_include_directories(crossfire_pico2 PRIVATE ${CMAKE_CURRENT_LIST_DIR}/include)

target_link_libraries(crossfire_pico2 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        crossfire_pico2_app
)

#   the status font: 16 px on the po13's real geometry, as mk3's crossfire used -- two lines
# of ten cells across the rotated 128-pixel row
light_mk4_add_font(crossfire_pico2_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY po13 WIDTH 64 HEIGHT 128 DIMENSION 17.2x32.3
        POINT_SIZE 12 PIXEL_SIZE 16
        CRATE crossfire_pico2_app ENV LIGHT_FONT_LGF
)

light_mk4_shell_configure(crossfire_pico2 USB_HOST)
