#   the WeAct Blackpill (STM32F411CEU6) on bare CMSIS: the CMSIS shell's second chip, with the
# console-and-LED demo mk3 ran there. No panel, no font.
add_executable(light_mk4_f411)

target_link_libraries(light_mk4_f411 PRIVATE light_app_f411)

light_mk4_shell_cmsis_configure(light_mk4_f411 CHIP stm32f411)
