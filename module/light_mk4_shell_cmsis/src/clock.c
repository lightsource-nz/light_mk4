// clock.c -- the STM32H743's clock tree, ported from the predecessor C framework with its
// logging replaced by a status string the shell prints once the console exists.
//
// ST's SystemInit() does not configure the PLL on H7, so the part boots on HSI at 64 MHz with
// every prescaler at 1. This takes it to 400 MHz off the board's crystal and provides the 48 MHz
// USB reference as a side effect. 400 and not 480: 480 needs VOS0, which needs revision V silicon
// plus the overdrive sequence; 400 at VOS1 works on every H743.
//
// ORDER IS LOAD-BEARING: voltage scaling before frequency, flash wait states before the clock
// that needs them, prescalers before the switch. Each done late hangs the core rather than
// failing. Every wait is bounded so a missing crystal is a slow board, not a dead one.
#include <stdbool.h>
#include <stdint.h>

#include <stm32h7xx.h>

#include "shell.h"

#define PLL1_DIVM               5u
#define PLL1_DIVN               160u
#define PLL1_DIVP               2u
//   pll1_q_ck is NOT optional: RCC_D2CCIP1R.SPI123SEL resets to it, so with PLL1's Q output
// disabled SPI1/2/3 have no kernel clock and configure perfectly while transferring nothing
#define PLL1_DIVQ               8u
#define PLL3_DIVM               5u
#define PLL3_DIVN               96u
#define PLL3_DIVQ               10u
#define PLL_DIVN_FIELD(n)       ((n) - 1u)
#define PLL_DIVP_FIELD(p)       ((p) - 1u)
#define PLL_DIVQ_FIELD(q)       ((q) - 1u)
#define CLOCK_WAIT_SPINS        1000000u

static const char *clock_status = ": HSI 64 MHz (clock init did not run)";

const char *light_shell_clock_status(void)
{
        return clock_status;
}

static bool wait_for(volatile uint32_t *reg, uint32_t mask, uint32_t spins)
{
        while (spins--) {
                if (*reg & mask)
                        return true;
        }
        return false;
}

// every early return leaves PLL1 stopped, and SPI1/2/3 take their kernel clock from pll1_q_ck by
// default: repoint them at per_ck (hsi_ker_ck), which runs whenever the core does
static void spi123_kernel_clock_fallback(void)
{
        RCC->D2CCIP1R = (RCC->D2CCIP1R & ~RCC_D2CCIP1R_SPI123SEL_Msk) | (4u << RCC_D2CCIP1R_SPI123SEL_Pos);
}

void light_shell_clock_init(void)
{
        PWR->D3CR |= (3u << PWR_D3CR_VOS_Pos);          // Scale 1
        if (!wait_for(&PWR->D3CR, PWR_D3CR_VOSRDY, CLOCK_WAIT_SPINS)) {
                clock_status = ": VOS1 not ready, staying on HSI at 64 MHz";
                spi123_kernel_clock_fallback();
                return;
        }
        RCC->CR |= RCC_CR_HSEON;
        if (!wait_for(&RCC->CR, RCC_CR_HSERDY, CLOCK_WAIT_SPINS)) {
                clock_status = ": HSE did not start, staying on HSI at 64 MHz (USB out of spec)";
                spi123_kernel_clock_fallback();
                return;
        }
        RCC->PLLCKSELR = RCC_PLLCKSELR_PLLSRC_HSE
                        | (PLL1_DIVM << RCC_PLLCKSELR_DIVM1_Pos)
                        | (PLL3_DIVM << RCC_PLLCKSELR_DIVM3_Pos);
        RCC->PLL1DIVR = PLL_DIVN_FIELD(PLL1_DIVN)
                        | (PLL_DIVP_FIELD(PLL1_DIVP) << RCC_PLL1DIVR_P1_Pos)
                        | (PLL_DIVQ_FIELD(PLL1_DIVQ) << RCC_PLL1DIVR_Q1_Pos);
        RCC->PLL3DIVR = PLL_DIVN_FIELD(PLL3_DIVN)
                        | (PLL_DIVQ_FIELD(PLL3_DIVQ) << RCC_PLL3DIVR_Q3_Pos);
        RCC->PLLCFGR = (2u << RCC_PLLCFGR_PLL1RGE_Pos)
                        | (2u << RCC_PLLCFGR_PLL3RGE_Pos)
                        | RCC_PLLCFGR_DIVP1EN
                        | RCC_PLLCFGR_DIVQ1EN
                        | RCC_PLLCFGR_DIVQ3EN;
        RCC->CR |= RCC_CR_PLL1ON;
        if (!wait_for(&RCC->CR, RCC_CR_PLL1RDY, CLOCK_WAIT_SPINS)) {
                clock_status = ": PLL1 did not lock, staying on HSI at 64 MHz";
                spi123_kernel_clock_fallback();
                return;
        }
        RCC->CR |= RCC_CR_PLL3ON;
        bool pll3 = wait_for(&RCC->CR, RCC_CR_PLL3RDY, CLOCK_WAIT_SPINS);

        // flash wait states before the switch: AXI at 200 MHz needs 2WS at VOS1
        FLASH->ACR = FLASH_ACR_LATENCY_2WS | (2u << FLASH_ACR_WRHIGHFREQ_Pos);
        // prescalers before the switch: CPU 400 / AXI+AHB 200 / APB1-4 100
        RCC->D1CFGR = RCC_D1CFGR_HPRE_DIV2 | RCC_D1CFGR_D1PPRE_DIV2 | RCC_D1CFGR_D1CPRE_DIV1;
        RCC->D2CFGR = RCC_D2CFGR_D2PPRE1_DIV2 | RCC_D2CFGR_D2PPRE2_DIV2;
        RCC->D3CFGR = RCC_D3CFGR_D3PPRE_DIV2;
        RCC->CFGR = (RCC->CFGR & ~RCC_CFGR_SW) | RCC_CFGR_SW_PLL1;
        if (!wait_for(&RCC->CFGR, RCC_CFGR_SWS_PLL1, CLOCK_WAIT_SPINS)) {
                clock_status = ": system clock did not switch to PLL1";
                return;
        }
        clock_status = pll3 ? ", PLL1 400 MHz off HSE, PLL3 48 MHz for USB" : ", PLL1 400 MHz off HSE, PLL3 did not lock";
        SystemCoreClockUpdate();
}
