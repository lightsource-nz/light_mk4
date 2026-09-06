#   the WeAct MiniSTM32H7xx on bare CMSIS: the CMSIS shell with a Rust app on the board's
# 160x80 ST7735, driven from its one key and the USART console. mk3's screentest_mini_stm32h7 on
# the mk4 stack, and the first target with no SDK under the shell at all.
add_executable(light_mk4_h7)

target_link_libraries(light_mk4_h7 PRIVATE light_app_h7)

#   the status font at the panel's real geometry: 160x80 over a 0.96" glass, 8 px
light_mk4_add_font(light_mk4_h7_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY mini_stm32h7 WIDTH 160 HEIGHT 80 DIMENSION 21.7x10.8
        POINT_SIZE 6 PIXEL_SIZE 8
        CRATE light_app_h7 ENV LIGHT_FONT_LGF
)

#   the framework's default look-and-feel for a color board (steel), plus this rig's
# decorative 6 px outer rounding -- carried over from the pre-theme demo
light_mk4_add_theme(light_mk4_h7_theme
        THEME ${CMAKE_CURRENT_LIST_DIR}/theme/h7.json
        CRATE light_app_h7 ENV LIGHT_THEME_LTH
)

light_mk4_shell_cmsis_configure(light_mk4_h7 CHIP stm32h743)
