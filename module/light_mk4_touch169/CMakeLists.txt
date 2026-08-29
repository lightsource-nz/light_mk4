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

light_mk4_shell_configure(light_mk4_touch169)
