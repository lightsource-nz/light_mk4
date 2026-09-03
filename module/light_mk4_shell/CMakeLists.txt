#   the C shell every board's executable compiles: src/main.c plus the pico-sdk knobs it depends
# on. A function rather than a library because pico_enable_stdio_* and the panic hook are
# per-executable settings.
#
#   light_mk4_shell_configure(<target> [USB_HOST]): the default is the device role -- TinyUSB
# CDC on core 1 as the console. USB_HOST is crossfire's role: the native port hosts USB-MIDI
# instruments, the host stack runs on core 0 under the Rust runtime, and the console is the
# UART. The application supplies tusb_config.h on its include path either way.
function(light_mk4_shell_configure TARGET)
        cmake_parse_arguments(SHELL "USB_HOST" "" "" ${ARGN})
        #   USB on core 1 -- see src/main.c. These are the knobs mk3's touch169 build settled on:
        target_compile_definitions(${TARGET} PRIVATE
                #   the shell calls tusb_init() on core 1 itself, and pumps tud_task() there;
                # either of these left on would have the SDK do it from core 0
                PICO_STDIO_USB_ENABLE_TINYUSB_INIT=0
                PICO_STDIO_USB_ENABLE_IRQ_BACKGROUND_TASK=0
                #   the SDK's own panics go through the same hand-off as Rust's
                PICO_PANIC_FUNCTION=light_shell_panic_sdk
                PICO_PANIC_FUNCTION_DOES_NOT_RETURN=1
                #   long lines intact; and stdout NON-BLOCKING (timeout 0): a console write
                # drops when the host is not reading rather than stalling core 1's drain. A
                # blocking write here is a latent deadlock -- while core 1 sits in the drain
                # waiting for CDC TX space, it is not pumping tud_task, so the CDC's OUT
                # endpoint stops being serviced and the HOST's next write times out. This bit
                # recording on the 3.49: the ~7 log lines a `rec` emits, with the host not
                # reading mid-capture, stalled the drain and wedged the console. Dropping is
                # already the log queue's own policy on the push side; this matches it on the
                # transport side. The framework principle: logging never blocks the loop
                CFG_TUD_CDC_TX_BUFSIZE=512
                PICO_STDIO_USB_STDOUT_TIMEOUT_US=0
                #   core 0's stack: the SDK's default is 2 KB, which the widget toolkit's demo
                # overran the moment its module state was built on the stack -- the board
                # enumerated and then went silent, core 1 waiting on a lock core 0 died holding.
                # Module state lives in .bss now; this is headroom for what does not. 4 KB is
                # the ceiling: each core's stack lives in its own 4 KB scratch bank
                PICO_STACK_SIZE=0x1000
                PICO_CORE1_STACK_SIZE=0x1000
                #   an MPU guard region at the bottom of each stack, so an overflow is a hard
                # fault at the instruction that did it rather than core 1's stack (which sits
                # directly below core 0's) being silently overwritten -- which is what the first
                # toolkit build did: the UI kept answering taps while USB went dead
                PICO_USE_STACK_GUARDS=0
        )
        if(SHELL_USB_HOST)
                target_compile_definitions(${TARGET} PRIVATE LIGHT_SHELL_USB_HOST=1)
                #   the host stack and TinyUSB's BSP (board_init, which the SDK integration
                # calls for the clock); PIO-USB deliberately not -- see mk3's light_usb for why
                target_link_libraries(${TARGET} PRIVATE tinyusb_host tinyusb_board)
                pico_enable_stdio_usb(${TARGET} 0)
                pico_enable_stdio_uart(${TARGET} 1)
        else()
                #   the console is USB CDC on every device-role board (the touch169 has no SWD
                # pads and CDC is its only way in) -- AND the UART, for a board in the SWD dock
                # whose own USB port is not cabled: the probe's CDC carries it. mk3-era doubts
                # about the po13 UART through the probe were retired by crossfire's host-role
                # console running on exactly that path. stdio writes go to both; reads take
                # whichever arrives
                pico_enable_stdio_usb(${TARGET} 1)
                pico_enable_stdio_uart(${TARGET} 1)
        endif()
        pico_add_extra_outputs(${TARGET})
endfunction()
