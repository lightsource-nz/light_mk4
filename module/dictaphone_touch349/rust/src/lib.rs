//! The PORTRAIT dictaphone executable for the Waveshare RP2350-Touch-LCD-3.49. All the board wiring,
//! the modules and the runtime are shared with the landscape build in `light_dictaphone_touch349`;
//! this crate contributes only what the two orientations differ by -- the embedded blobs (font,
//! theme, and `light_app_dictaphone/design.json` as an LUI blob) and the portrait [`RunConfig`] --
//! plus the `#[no_mangle]`/`#[panic_handler]` entry points the shell ABI requires.

#![no_std]

use light_board_touch349::{panic_report, service_core1, ShellInfo};
use light_dictaphone_touch349::{push_console_byte, run, RunConfig};
use light_draw::Rotation;
use light_input::imu::Orientation;

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));
static UI_BLOB: &[u8] = include_bytes!(env!("LIGHT_UI_LUI"));

/// Portrait only, MEASURED: the bar rests near-landscape on its long edge, so ordinary handling
/// flapped LandscapeL/R -- a 180-degree relayout per touch, and every tap then landed where a widget
/// used to be. The bar rotates end-for-end (a deliberate gesture, nowhere near the resting pose) and
/// otherwise holds still.
fn rotation_map(o: Orientation) -> Option<Rotation> {
        match o {
                Orientation::Portrait => Some(Rotation::R0),
                Orientation::PortraitFlip => Some(Rotation::R180),
                _ => None,
        }
}

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        run(
                info,
                RunConfig {
                        font: FONT_BLOB,
                        theme: THEME_BLOB,
                        ui: UI_BLOB,
                        rotation_map,
                        initial_rotation: Rotation::R0,
                        //   the portrait interface keeps the toolkit's layout-derived flow; its
                        // layout axis comes from the design's orientation (portrait -> vertical)
                        default_descent: None,
                        desc: "AXS15231B over PIO-QSPI, double-buffered",
                },
        )
}

//   the shell ABI glue is shared in light_board_touch349::shell; core 1's pump feeds the engine's
// console mailbox
#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_service() {
        service_core1(push_console_byte);
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        panic_report(info)
}
