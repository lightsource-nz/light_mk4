// console.c -- USART1 on PA9/PA10 and the ITM stimulus port, mk3's STM32 consoles.
//
// Both backends, because they fail in opposite ways: SWO needs a debugger attached and one wire
// already on the SWD header; the USART needs no debugger but a wire to something. Neither may
// block forever: CMSIS's ITM_SendChar() spins on FIFO room with no timeout, and a debugger that
// has enabled ITM without draining SWO -- OpenOCD does exactly that after flashing -- then stops
// the firmware dead inside a printf. Both waits are bounded; a lost log line is a log line.
//
// The USART is a different generation on the two chips, and every difference fails silently if
// carried across: ISR/TDR/RDR with FIFO flags on the H7, SR/DR on the F4; GPIO clocks on AHB4
// on the H7, AHB1 on the F4; the kernel clock a quarter of the core clock on the H7 once its PLL
// runs, equal to it on the F4 at reset.
#include <stdint.h>
#include <stdio.h>

#if defined(STM32H743xx)
#include <stm32h7xx.h>
#else
#include <stm32f4xx.h>
#endif

#include "shell.h"

#ifndef LIGHT_CONSOLE_BAUD
#define LIGHT_CONSOLE_BAUD              115200
#endif
#define CONSOLE_TX_SPINS                100000u

#if defined(STM32H743xx)
static uint32_t usart_kernel_hz(void)
{
        static const uint8_t ahb_shift[16] = { 0,0,0,0,0,0,0,0, 1,2,3,4,6,7,8,9 };
        static const uint8_t apb_shift[8]  = { 0,0,0,0, 1,2,3,4 };
        uint32_t d1cpre = (RCC->D1CFGR & RCC_D1CFGR_D1CPRE) >> RCC_D1CFGR_D1CPRE_Pos;
        uint32_t hpre = (RCC->D1CFGR & RCC_D1CFGR_HPRE) >> RCC_D1CFGR_HPRE_Pos;
        uint32_t ppre2 = (RCC->D2CFGR & RCC_D2CFGR_D2PPRE2) >> RCC_D2CFGR_D2PPRE2_Pos;
        uint32_t hclk = (SystemCoreClock << ahb_shift[d1cpre & 0xF]) >> ahb_shift[hpre & 0xF];
        return hclk >> apb_shift[ppre2 & 7];
}
#else
static uint32_t usart_kernel_hz(void)
{
        // APB2 at the core clock: the reset prescalers, which this shell leaves alone
        return SystemCoreClock;
}
#endif

void light_shell_console_init(void)
{
#if defined(STM32H743xx)
        RCC->AHB4ENR |= RCC_AHB4ENR_GPIOAEN;
#else
        RCC->AHB1ENR |= RCC_AHB1ENR_GPIOAEN;
#endif
        RCC->APB2ENR |= RCC_APB2ENR_USART1EN;
        GPIOA->MODER &= ~((3U << (9 * 2)) | (3U << (10 * 2)));
        GPIOA->MODER |= ((2U << (9 * 2)) | (2U << (10 * 2)));
        GPIOA->AFR[1] &= ~((0xFU << ((9 - 8) * 4)) | (0xFU << ((10 - 8) * 4)));
        GPIOA->AFR[1] |= ((7U << ((9 - 8) * 4)) | (7U << ((10 - 8) * 4)));
        GPIOA->OSPEEDR |= ((2U << (9 * 2)) | (2U << (10 * 2)));
        USART1->BRR = (usart_kernel_hz() + (LIGHT_CONSOLE_BAUD / 2)) / LIGHT_CONSOLE_BAUD;
        USART1->CR1 = USART_CR1_TE | USART_CR1_RE | USART_CR1_UE;
        setvbuf(stdout, NULL, _IONBF, 0);
        setvbuf(stderr, NULL, _IONBF, 0);
}

int light_shell_console_read_byte(void)
{
#if defined(STM32H743xx)
        if (USART1->ISR & USART_ISR_RXNE_RXFNE)
                return (int) (USART1->RDR & 0xFF);
        // an overrun or framing error latches and stops reception until cleared
        if (USART1->ISR & (USART_ISR_ORE | USART_ISR_FE))
                USART1->ICR = USART_ICR_ORECF | USART_ICR_FECF;
        return -1;
#else
        if (USART1->SR & USART_SR_RXNE)
                return (int) (USART1->DR & 0xFF);
        // on the F4 an overrun is cleared by reading SR then DR, which this just did if it was
        // set; reading DR once more is the documented sequence
        if (USART1->SR & USART_SR_ORE)
                (void) USART1->DR;
        return -1;
#endif
}

static void console_putc(uint8_t c)
{
        if ((ITM->TCR & ITM_TCR_ITMENA_Msk) && (ITM->TER & 1uL)) {
                uint32_t spins = CONSOLE_TX_SPINS;
                while (ITM->PORT[0].u32 == 0uL) {
                        if (!--spins)
                                break;
                }
                if (spins)
                        ITM->PORT[0].u8 = c;
        }
        uint32_t spins = CONSOLE_TX_SPINS;
#if defined(STM32H743xx)
        while (!(USART1->ISR & USART_ISR_TXE_TXFNF)) {
                if (!--spins)
                        return;
        }
        USART1->TDR = c;
#else
        while (!(USART1->SR & USART_SR_TXE)) {
                if (!--spins)
                        return;
        }
        USART1->DR = c;
#endif
}

// newlib's _write, a strong symbol beating libnosys's stub: every printf lands here
int _write(int fd, const char *buf, int len)
{
        if (fd != 1 && fd != 2)
                return -1;
        for (int i = 0; i < len; i++) {
                if (buf[i] == '\n')
                        console_putc('\r');
                console_putc((uint8_t) buf[i]);
        }
        return len;
}
