#   the C shell every board's executable compiles: src/main.c plus the pico-sdk knobs it depends
# on. A function rather than a library because pico_enable_stdio_* and the panic hook are
# per-executable settings
function(light_mk4_shell_configure TARGET)
        #   USB on core 1 -- see src/main.c. These are the knobs mk3's touch169 build settled on:
        target_compile_definitions(${TARGET} PRIVATE
                #   the shell calls tusb_init() on core 1 itself, and pumps tud_task() there;
                # either of these left on would have the SDK do it from core 0
                PICO_STDIO_USB_ENABLE_TINYUSB_INIT=0
                PICO_STDIO_USB_ENABLE_IRQ_BACKGROUND_TASK=0
                #   the SDK's own panics go through the same hand-off as Rust's
                PICO_PANIC_FUNCTION=light_shell_panic_sdk
                PICO_PANIC_FUNCTION_DOES_NOT_RETURN=1
                #   long lines intact; and a 5 ms per-write bound rather than the SDK's 500 ms,
                # which only core 1's drain ever pays now
                CFG_TUD_CDC_TX_BUFSIZE=512
                PICO_STDIO_USB_STDOUT_TIMEOUT_US=5000
        )
        #   the console is USB CDC on every board: the touch169 has no SWD pads, and the po13
        # rig's UART has never been reliable through the probe
        pico_enable_stdio_usb(${TARGET} 1)
        pico_enable_stdio_uart(${TARGET} 0)
        pico_add_extra_outputs(${TARGET})
endfunction()
