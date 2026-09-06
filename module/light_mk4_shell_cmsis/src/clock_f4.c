// clock_f4.c -- the F411 runs on its reset defaults: HSI at 16 MHz with
// every prescaler at 1. Correct, crystal-free, and slow; a PLL is a later slice, and the shell
// reports the clocks it hands over either way.
#include "shell.h"

void light_shell_clock_init(void)
{
}

const char *light_shell_clock_status(void)
{
        return " (HSI, reset defaults)";
}
