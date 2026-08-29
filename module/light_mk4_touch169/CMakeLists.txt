#   the firmware executable is a pico-sdk one in every respect -- runtime, linker script, boot2,
# UF2 output -- with the Rust staticlib (imported by the root CMakeLists through Corrosion) as
# just another library on its link line
add_executable(light_mk4_touch169
        src/main.c
)

target_link_libraries(light_mk4_touch169 PRIVATE
        pico_stdlib
        light_app_touch169
)

#   this board has no SWD pads exposed, so as with every touch169 build the console is USB CDC
# over the same cable that flashes it
pico_enable_stdio_usb(light_mk4_touch169 1)
pico_enable_stdio_uart(light_mk4_touch169 0)
pico_add_extra_outputs(light_mk4_touch169)
