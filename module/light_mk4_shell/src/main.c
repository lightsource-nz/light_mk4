// The C shell of the touch169 firmware.
//
// This file is deliberately everything pico-sdk needs to own and nothing else: the runtime comes
// up through the SDK's crt0 and runtime_init exactly as it does for any SDK program, and then
// control passes to Rust on core 0 and does not come back. What the shell exports TO Rust is the
// handful of functions below; keeping them in one file makes the size of that surface visible.
//
// USB LIVES ON CORE 1, the arrangement this board's bring-up settled on: tusb_init() and tud_task()
// here, on this core and no other, because dcd_int_enable() enables USBCTRL_IRQ on the CALLING
// core and TinyUSB guards its queues with per-core IRQ-disable sections that are not cross-core
// safe. Every stdio write and read therefore happens from core 1 -- the log drain and the console
// reads run in light_app_core1_service(), and core 0 never touches stdio. A panic on core 0 is
// formatted there (memory only) and handed to this core to print.
#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>

#include <hardware/clocks.h>
#include <pico/bootrom.h>
#include <pico/multicore.h>
#include <pico/stdlib.h>
#include <tusb.h>

// what the shell knows and Rust must not assume: the clocks the SDK runtime configured
struct light_shell_info {
        uint32_t clk_sys_hz;
        uint32_t clk_peri_hz;
};

// the Rust side (module/<board>/rust)
extern void light_app_main(const struct light_shell_info *info) __attribute__((noreturn));
extern void light_app_core1_service(void);

static volatile bool core1_ready = false;

//   the USB device enumeration state, updated on core 1 (which owns TinyUSB) each service pass and
// read from any core: the board's only "on external (USB) power" signal, since it carries no VBUS
// sense pin. Stays false on the host-role build, which runs no device stack.
static volatile bool usb_mounted_flag = false;

bool light_shell_usb_mounted(void)
{
        return usb_mounted_flag;
}

// a line of text from Rust, formatted there, for the console. Called from core 1 only
void light_shell_log(const char *msg, size_t len)
{
        printf("%.*s\n", (int) len, msg);
}

// one byte of console input, or -1 when none is waiting. Called from core 1 only
int light_shell_read_byte(void)
{
        int c = getchar_timeout_us(0);
        return c == PICO_ERROR_TIMEOUT ? -1 : c;
}

// panic hand-off: formatted on the dying core, printed by the core that owns USB, then into
// BOOTSEL so the board stays flashable -- a halted board no longer serves the 1200-baud reset
#define PANIC_MESSAGE_MAX 256
#define PANIC_HANDOFF_TIMEOUT_MS 2000
static char panic_message[PANIC_MESSAGE_MAX];
static volatile bool panic_pending = false;
static volatile bool panic_printed = false;

static void __attribute__((noreturn)) shell_panic_finish(void)
{
        if (get_core_num() == 1 || !core1_ready) {
                printf("\n*** PANIC (core %u) ***\n%s\n", (unsigned) get_core_num(), panic_message);
                stdio_flush();
        } else {
                panic_pending = true;
                //   busy_wait, never sleep: a panic raised inside an interrupt handler (TinyUSB's
                // host assertions fire from the USB IRQ) would otherwise hit the SDK's own
                // "attempted to sleep inside an exception handler" panic here, which re-enters
                // this function and overwrites the message that mattered
                for (uint32_t i = 0; i < PANIC_HANDOFF_TIMEOUT_MS && !panic_printed; i++)
                        busy_wait_us(1000);
                //   core 1 never got to it -- it is blocked on a lock this core died holding,
                // typically -- so the message is printed from here, which the UART allows and
                // the CDC does not: on the device-role builds it is simply lost, and the
                // message stays in panic_message for a debugger to read
#ifdef LIGHT_SHELL_USB_HOST
                if (!panic_printed) {
                        printf("\n*** PANIC (core %u, unrelayed) ***\n%s\n", (unsigned) get_core_num(), panic_message);
                        stdio_flush();
                }
#endif
        }
#ifdef LIGHT_SHELL_USB_HOST
        //   the host-role board is flashed over SWD and its USB port is a host port: BOOTSEL would
        // be invisible and would wipe the message. Halt where a debugger can read it
        __breakpoint();
        while (true)
                tight_loop_contents();
#else
        reset_usb_boot(0, 0);
        __breakpoint();
        while (true)
                tight_loop_contents();
#endif
}

// a Rust panic, already formatted on the Rust side into msg[0..len)
void __attribute__((noreturn)) light_shell_panic(const char *msg, size_t len)
{
        snprintf(panic_message, sizeof panic_message, "rust: %.*s", (int) len, msg);
        shell_panic_finish();
}

// the SDK's own panics -- assertions, spinlock misuse -- installed as PICO_PANIC_FUNCTION
void __attribute__((noreturn)) light_shell_panic_sdk(const char *fmt, ...)
{
        va_list args;
        va_start(args, fmt);
        vsnprintf(panic_message, sizeof panic_message, fmt ? fmt : "(no message)", args);
        va_end(args);
        shell_panic_finish();
}

#ifdef LIGHT_SHELL_USB_HOST
//   THE HOST ROLE, for crossfire: the native USB port is a HOST -- USB-MIDI instruments plug
// into it -- so there is no CDC console, and stdio is the UART (carried by the debug probe when
// one is attached). The whole host stack runs on CORE 0, driven from the Rust runtime through the three
// calls below: TinyUSB guards its queues with per-core IRQ-disable sections that are not
// cross-core safe, dcd/hcd_int_enable() enables the IRQ on the CALLING core, and the class
// callbacks (tuh_midi_mount_cb, implemented on the Rust side) then run in the same context as
// the packet reads and writes. Core 1 keeps the log drain and the console read, which the UART
// serves from either core without a USB stack to protect.
void light_shell_usb_host_init(void)
{
        tusb_rhport_init_t host_init = {
                .role = TUSB_ROLE_HOST,
                .speed = TUSB_SPEED_AUTO,
        };
        tusb_init(BOARD_TUH_RHPORT, &host_init);
}

void light_shell_usb_host_task(void)
{
        tuh_task();
}

//   the RP2 native host controller can leave stale buffer-control state behind across a
// disconnect (hathach/tinyusb#3533), which panics the next enumeration; the answer is a full
// teardown and re-init once the root port is EMPTY, from the main loop and never from inside a
// callback the stack is still unwinding. The settle delay matches TinyUSB's own dynamic_switch
// example
void light_shell_usb_host_reset(void)
{
        tusb_deinit(BOARD_TUH_RHPORT);
        sleep_ms(100);
        light_shell_usb_host_init();
}

static void core1_main(void)
{
        core1_ready = true;
        while (true) {
                if (panic_pending) {
                        printf("\n*** PANIC (core 0) ***\n%s\n", panic_message);
                        stdio_flush();
                        panic_printed = true;
                        while (true)
                                tight_loop_contents();
                }
                light_app_core1_service();
        }
}
#else
static void core1_main(void)
{
        tusb_init();
        core1_ready = true;
        while (true) {
                tud_task();
                usb_mounted_flag = tud_mounted();
                if (panic_pending) {
                        printf("\n*** PANIC (core 0) ***\n%s\n", panic_message);
                        // keep pumping so the message actually leaves the device
                        for (uint32_t i = 0; i < 1000; i++) {
                                tud_task();
                                sleep_ms(1);
                        }
                        panic_printed = true;
                        while (true)
                                tud_task();
                }
                light_app_core1_service();
        }
}
#endif

//   CORE 1'S STACK LIVES IN ORDINARY RAM, not the linker's SCRATCH_X default. SCRATCH_X
// sits directly below SCRATCH_Y (core 0's stack), and a deep core-0 call chain that dips
// past its own floor was found landing exactly on core 1's live frames: core 1 died
// alone, the application ran on, and the console -- which IS core 1 -- could not report
// its own death (diagnosed on the 3.49 with an on-screen core-1 heartbeat and a painted
// stack watermark: the heartbeat froze as the watermark hit zero). With core 1's stack
// here, SCRATCH_X is vacant runway: a core-0 excursion overwrites nothing that lives.
static uint32_t core1_stack[PICO_CORE1_STACK_SIZE / sizeof(uint32_t)];

int main(void)
{
        // core 1 first: with the SDK's IRQ background task disabled nothing else pumps
        // tud_task(), and stdio_init_all()'s connect wait would otherwise never see enumeration
        // complete. Reset before launch, or a warm restart of core 0 hangs in the FIFO handshake
        multicore_reset_core1();
        multicore_launch_core1_with_stack(core1_main, core1_stack, sizeof(core1_stack));
        while (!core1_ready)
                tight_loop_contents();
        //   stdio: USB CDC on the device-role builds (with the connect wait the SDK does for it),
        // the UART on the host-role build -- selected by pico_enable_stdio_* in the CMake
        stdio_init_all();
        struct light_shell_info info = {
                .clk_sys_hz = clock_get_hz(clk_sys),
                .clk_peri_hz = clock_get_hz(clk_peri),
        };
        light_app_main(&info);
}
