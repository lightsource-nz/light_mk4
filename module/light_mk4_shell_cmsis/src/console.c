// console.c -- USART1 on PA9/PA10 and the ITM stimulus port, mk3's H743 console.
//
// Both backends, because they fail in opposite ways: SWO needs a debugger attached and one wire
// already on the SWD header; the USART needs no debugger but a wire to something. Neither may
// block forever: CMSIS's ITM_SendChar() spins on FIFO room with no timeout, and a debugger that
// has enabled ITM without draining SWO -- OpenOCD does exactly that after flashing -- then stops
// the firmware dead inside a printf. Both waits are bounded; a lost log line is a log line.
#include <stdint.h>
#include <stdio.h>

#include <stm32h7xx.h>

#include "shell.h"

#ifndef LIGHT_CONSOLE_BAUD
#define LIGHT_CONSOLE_BAUD              115200
#endif
#define CONSOLE_TX_SPINS                100000u

static uint32_t pclk2_hz(void)
{
        static const uint8_t ahb_shift[16] = { 0,0,0,0,0,0,0,0, 1,2,3,4,6,7,8,9 };
        static const uint8_t apb_shift[8]  = { 0,0,0,0, 1,2,3,4 };
        uint32_t d1cpre = (RCC->D1CFGR & RCC_D1CFGR_D1CPRE) >> RCC_D1CFGR_D1CPRE_Pos;
        uint32_t hpre = (RCC->D1CFGR & RCC_D1CFGR_HPRE) >> RCC_D1CFGR_HPRE_Pos;
        uint32_t ppre2 = (RCC->D2CFGR & RCC_D2CFGR_D2PPRE2) >> RCC_D2CFGR_D2PPRE2_Pos;
        uint32_t hclk = (SystemCoreClock << ahb_shift[d1cpre & 0xF]) >> ahb_shift[hpre & 0xF];
        return hclk >> apb_shift[ppre2 & 7];
}

void light_shell_console_init(void)
{
        // AHB4, not AHB1: the GPIO ports are in the D3 domain on the H7
        RCC->AHB4ENR |= RCC_AHB4ENR_GPIOAEN;
        RCC->APB2ENR |= RCC_APB2ENR_USART1EN;
        GPIOA->MODER &= ~((3U << (9 * 2)) | (3U << (10 * 2)));
        GPIOA->MODER |= ((2U << (9 * 2)) | (2U << (10 * 2)));
        GPIOA->AFR[1] &= ~((0xFU << ((9 - 8) * 4)) | (0xFU << ((10 - 8) * 4)));
        GPIOA->AFR[1] |= ((7U << ((9 - 8) * 4)) | (7U << ((10 - 8) * 4)));
        GPIOA->OSPEEDR |= ((2U << (9 * 2)) | (2U << (10 * 2)));
        // the USART's kernel clock (rcc_pclk2 by default), not the CPU clock: the two differ by
        // four once clock.c has run, which would put the console out at 28800 baud
        USART1->BRR = (pclk2_hz() + (LIGHT_CONSOLE_BAUD / 2)) / LIGHT_CONSOLE_BAUD;
        USART1->CR1 = USART_CR1_TE | USART_CR1_RE | USART_CR1_UE;
        setvbuf(stdout, NULL, _IONBF, 0);
        setvbuf(stderr, NULL, _IONBF, 0);
}

int light_shell_console_read_byte(void)
{
        if (USART1->ISR & USART_ISR_RXNE_RXFNE)
                return (int) (USART1->RDR & 0xFF);
        // an overrun or framing error latches and stops reception until cleared
        if (USART1->ISR & (USART_ISR_ORE | USART_ISR_FE))
                USART1->ICR = USART_ICR_ORECF | USART_ICR_FECF;
        return -1;
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
        while (!(USART1->ISR & USART_ISR_TXE_TXFNF)) {
                if (!--spins)
                        return;
        }
        USART1->TDR = c;
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
