#   the bare Pico 2 (the po13 rig, in its SWD dock): the same shell, a Rust app with the LED and
# the console and nothing else. Exists for what the touch169 cannot do -- be flashed and halted
# over SWD, by openocd or probe-rs
add_executable(light_mk4_pico2
        ../light_mk4_shell/src/main.c
)

target_link_libraries(light_mk4_pico2 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_pico2
)

light_mk4_shell_configure(light_mk4_pico2)
