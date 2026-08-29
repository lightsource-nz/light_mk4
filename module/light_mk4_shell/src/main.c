// The C shell of the touch169 spike firmware.
//
// This file is deliberately everything pico-sdk needs to own and nothing else: the runtime comes
// up through the SDK's crt0 and runtime_init exactly as it does for any SDK program, and then
// control passes to Rust on core 0 and does not come back. What the shell exports TO Rust is the
// handful of functions below; keeping them in one file makes the size of that surface -- one of
// the things the spike measures -- visible.
//
// USB LIVES ON CORE 1, the arrangement mk3 arrived at for this board: tusb_init() and tud_task()
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
                for (uint32_t i = 0; i < PANIC_HANDOFF_TIMEOUT_MS && !panic_printed; i++)
                        sleep_ms(1);
        }
        reset_usb_boot(0, 0);
        __breakpoint();
        while (true)
                tight_loop_contents();
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

static void core1_main(void)
{
        tusb_init();
        core1_ready = true;
        while (true) {
                tud_task();
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

int main(void)
{
        // core 1 first: with the SDK's IRQ background task disabled nothing else pumps
        // tud_task(), and stdio_init_all()'s connect wait would otherwise never see enumeration
        // complete. Reset before launch, or a warm restart of core 0 hangs in the FIFO handshake
        multicore_reset_core1();
        multicore_launch_core1(core1_main);
        while (!core1_ready)
                tight_loop_contents();
        stdio_init_all();
        struct light_shell_info info = {
                .clk_sys_hz = clock_get_hz(clk_sys),
                .clk_peri_hz = clock_get_hz(clk_peri),
        };
        light_app_main(&info);
}
