// TinyUSB configuration for crossfire's host role: mk3's crossfire_main/include/tusb_config.h
// with its findings kept and its device-role remnants dropped.
#ifndef _TUSB_CONFIG_H_
#define _TUSB_CONFIG_H_

#ifdef __cplusplus
extern "C" {
#endif

//   the native controller only, never PIO-USB: Pico-PIO-USB is a submodule of nothing and
// tinyusb fetches it from a third-party repository, so a build depending on it reproduces on no
// clone -- mk3's light_usb learned that from CI
#if CFG_TUSB_MCU == OPT_MCU_RP2040
#define CFG_TUH_RPI_PIO_USB   0
#define BOARD_TUH_RHPORT      CFG_TUH_RPI_PIO_USB
#endif

#ifndef BOARD_TUH_RHPORT
#define BOARD_TUH_RHPORT      0
#endif

#ifndef BOARD_TUH_MAX_SPEED
#define BOARD_TUH_MAX_SPEED   OPT_MODE_DEFAULT_SPEED
#endif

#ifndef CFG_TUSB_MCU
#error CFG_TUSB_MCU must be defined
#endif

#ifndef CFG_TUSB_OS
#define CFG_TUSB_OS           OPT_OS_NONE
#endif

#ifndef CFG_TUSB_DEBUG
#define CFG_TUSB_DEBUG        0
#endif

#define CFG_TUH_ENABLED       1
#define CFG_TUH_MAX_SPEED     BOARD_TUH_MAX_SPEED

#ifndef CFG_TUSB_MEM_SECTION
#define CFG_TUSB_MEM_SECTION
#endif
#ifndef CFG_TUSB_MEM_ALIGN
#define CFG_TUSB_MEM_ALIGN    __attribute__((aligned(4)))
#endif

//   256 was too small for a composite audio+MIDI device: its config descriptor (audio control
// plus MIDI streaming plus jacks) exceeds it, and usbh.c's "total_len <= BUFSIZE" assert then
// bails out of enumeration silently -- no mount callback, no error, CFG_TUSB_DEBUG being 0
#define CFG_TUH_ENUMERATION_BUFSIZE 512

//   HUBS IN THE WHOLE TREE, not "hubs you may plug in". A seven-port hub is usually two hub
// chips in series, and with this at 1 the second one exhausts the hub address window and
// enumeration retries forever with nothing mounting and nothing said -- a full session in mk3.
// 2 covers one chained hub
#define CFG_TUH_HUB                 2

//   excluding the hub: TinyUSB sizes its table as DEVICE_MAX + HUB, so 4 means four
// instruments plus the hub. Equal to the forwarding engine's USB slot count by construction
// (LIGHT_MIDI_SLOTS on the Rust side), because the engine indexes its table with the mount
// index directly
#define CFG_TUH_DEVICE_MAX          4
#define CFG_TUH_MIDI                CFG_TUH_DEVICE_MAX
#define CFG_TUH_ENDPOINT_MAX        8
#define CFG_TUH_API_EDPT_XFER       1

#ifdef __cplusplus
}
#endif

#endif
