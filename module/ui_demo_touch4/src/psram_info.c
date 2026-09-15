//   the one board-specific C file: pico-sdk's hardware_psram owns detection and QMI setup
// (it runs during runtime init, before main); Rust only needs the answer.
#include "hardware/psram.h"

uint32_t light_board_psram_size(void)
{
	return (uint32_t)psram_get_size();
}
