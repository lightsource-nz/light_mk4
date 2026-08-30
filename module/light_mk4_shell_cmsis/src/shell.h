#ifndef LIGHT_MK4_SHELL_CMSIS_H
#define LIGHT_MK4_SHELL_CMSIS_H

// the clock tree: 400 MHz off the crystal, falling back to HSI if it does not start. Returns
// nothing; light_shell_clock_status() says afterwards what happened, once there is a console
void light_shell_clock_init(void);
const char *light_shell_clock_status(void);

// the console: USART1 on PA9/PA10 and the ITM stimulus port, both, since they fail in opposite
// ways. Neither may block forever
void light_shell_console_init(void);
int light_shell_console_read_byte(void);

#endif
