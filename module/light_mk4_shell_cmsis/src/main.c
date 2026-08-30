// The C shell for the bare-CMSIS STM32 targets: what pico-sdk's runtime does for the RP2 boards,
// done here by the CMSIS startup file, ST's system file and the three things this file adds --
// the caches, the clock tree and the console -- before control passes to Rust and does not come
// back. The same five functions cross the boundary as on the RP2 shell; there is no second core,
// so the log drain the RP2 shell runs on core 1 is the Rust side's own job here.
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>

#include <stm32h7xx.h>

#include "shell.h"

struct light_shell_info {
        uint32_t clk_sys_hz;
        uint32_t clk_apb2_hz;
        uint32_t clk_tim_hz;
};

extern void light_app_main(const struct light_shell_info *info) __attribute__((noreturn));

void light_shell_log(const char *msg, size_t len)
{
        printf("%.*s\n", (int) len, msg);
}

int light_shell_read_byte(void)
{
        return light_shell_console_read_byte();
}

#define PANIC_MESSAGE_MAX 256
static char panic_message[PANIC_MESSAGE_MAX];

//   one core, one console: print and halt where a debugger can read the message. There is no
// BOOTSEL to fall into; the ST-Link reflashes a halted part
static void __attribute__((noreturn)) shell_panic_finish(void)
{
        printf("\n*** PANIC ***\n%s\n", panic_message);
        __BKPT(0);
        while (1)
                __NOP();
}

void __attribute__((noreturn)) light_shell_panic(const char *msg, size_t len)
{
        snprintf(panic_message, sizeof panic_message, "rust: %.*s", (int) len, msg);
        shell_panic_finish();
}

//   the prescaler fields encode "divide at all" in the top bit and the power of two below it;
// SystemCoreClock is the CPU clock, already divided by D1CPRE, which has to be undone before HPRE
static uint32_t hclk_hz(void)
{
        static const uint8_t shift[16] = { 0,0,0,0,0,0,0,0, 1,2,3,4,6,7,8,9 };
        uint32_t d1cpre = (RCC->D1CFGR & RCC_D1CFGR_D1CPRE) >> RCC_D1CFGR_D1CPRE_Pos;
        uint32_t hpre = (RCC->D1CFGR & RCC_D1CFGR_HPRE) >> RCC_D1CFGR_HPRE_Pos;
        return (SystemCoreClock << shift[d1cpre & 0xF]) >> shift[hpre & 0xF];
}

static uint32_t apb_hz(uint32_t ppre_field)
{
        static const uint8_t shift[8] = { 0,0,0,0, 1,2,3,4 };
        return hclk_hz() >> shift[ppre_field & 7];
}

int main(void)
{
        //   the instruction cache before anything else: at 400 MHz flash is two wait states,
        // and fetch has no coherency problem to manage. The data cache stays OFF, as mk3's
        // default: the frame buffer is DMA territory one day, and a cached buffer handed to DMA
        // is silently wrong
        SCB_EnableICache();

        light_shell_clock_init();
        SystemCoreClockUpdate();

        //   keep the debug interface alive across WFI, or a running application becomes
        // unreachable over SWD ("Cortex-M CPUID: 0x0 is unrecognized")
        DBGMCU->CR |= DBGMCU_CR_DBG_SLEEPD1 | DBGMCU_CR_DBG_STOPD1 | DBGMCU_CR_DBG_STANDBYD1;

        light_shell_console_init();

        uint32_t pclk1 = apb_hz((RCC->D2CFGR & RCC_D2CFGR_D2PPRE1) >> RCC_D2CFGR_D2PPRE1_Pos);
        uint32_t pclk2 = apb_hz((RCC->D2CFGR & RCC_D2CFGR_D2PPRE2) >> RCC_D2CFGR_D2PPRE2_Pos);
        struct light_shell_info info = {
                .clk_sys_hz = SystemCoreClock,
                .clk_apb2_hz = pclk2,
                // the APB1 timers run at twice APB1 whenever APB1 is prescaled (TIMPRE clear)
                .clk_tim_hz = (pclk1 == hclk_hz()) ? pclk1 : pclk1 * 2,
        };
        printf("light_mk4 shell: sys %lu Hz, apb2 %lu Hz, timers %lu Hz%s\n",
                        (unsigned long) info.clk_sys_hz, (unsigned long) info.clk_apb2_hz,
                        (unsigned long) info.clk_tim_hz, light_shell_clock_status());
        light_app_main(&info);
}
