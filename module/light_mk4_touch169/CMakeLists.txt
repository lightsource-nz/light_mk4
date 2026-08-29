#   the firmware executable is a pico-sdk one in every respect -- runtime, linker script, boot2,
# UF2 output -- with the Rust staticlib (imported by the root CMakeLists through Corrosion) as
# just another library on its link line
add_executable(light_mk4_touch169
        src/main.c
)

target_link_libraries(light_mk4_touch169 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_touch169
)

#   USB on core 1 -- see src/main.c. These are the knobs mk3's touch169 build settled on:
target_compile_definitions(light_mk4_touch169 PRIVATE
        #   the shell calls tusb_init() on core 1 itself, and pumps tud_task() there; either of
        # these left on would have the SDK do it from core 0
        PICO_STDIO_USB_ENABLE_TINYUSB_INIT=0
        PICO_STDIO_USB_ENABLE_IRQ_BACKGROUND_TASK=0
        #   the SDK's own panics go through the same hand-off as Rust's
        PICO_PANIC_FUNCTION=light_shell_panic_sdk
        PICO_PANIC_FUNCTION_DOES_NOT_RETURN=1
        #   long lines intact; and a 5 ms per-write bound rather than the SDK's 500 ms, which
        # only core 1's drain ever pays now
        CFG_TUD_CDC_TX_BUFSIZE=512
        PICO_STDIO_USB_STDOUT_TIMEOUT_US=5000
)

#   this board has no SWD pads exposed, so as with every touch169 build the console is USB CDC
# over the same cable that flashes it
pico_enable_stdio_usb(light_mk4_touch169 1)
pico_enable_stdio_uart(light_mk4_touch169 0)
pico_add_extra_outputs(light_mk4_touch169)
