// The C shell of the touch169 spike firmware.
//
// This file is deliberately everything pico-sdk needs to own and nothing else: the runtime comes
// up through the SDK's crt0 and runtime_init exactly as it does for any SDK program, stdio is
// USB CDC as on every touch169 build, and then control passes to Rust and does not come back.
// What the shell exports TO Rust is the handful of functions below; keeping them in one file
// makes the size of that surface -- one of the things the spike measures -- visible.
#include <stdio.h>
#include <stddef.h>

#include <pico/stdlib.h>

// the Rust side (module/light_mk4_touch169/rust)
extern void light_app_main(void) __attribute__((noreturn));

// a Rust panic, already formatted on the Rust side into msg[0..len). Routed through the SDK's
// panic() so it prints from whichever core died, the same path a C panic takes
void light_shell_panic(const char *msg, size_t len)
{
        panic("rust: %.*s", (int) len, msg);
}

// a line of text from Rust, formatted there, for the console. The spike's Rust side has no
// allocator and no stdio; this is the one hole through which it speaks until the log queue
// lands (milestone 2), when it will hand records across rather than text
void light_shell_log(const char *msg, size_t len)
{
        printf("%.*s\n", (int) len, msg);
}

int main(void)
{
        stdio_init_all();
        printf("light mk4 spike: shell up, entering rust\n");
        light_app_main();
}
