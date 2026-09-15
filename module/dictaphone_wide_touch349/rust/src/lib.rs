//! The LANDSCAPE dictaphone executable for the Waveshare RP2350-Touch-LCD-3.49. All the board
//! wiring, the modules and the runtime are shared with the portrait build in
//! `light_dictaphone_touch349`; this crate contributes only what the two orientations differ by --
//! the embedded blobs (font, theme, and `light_app_dictaphone_wide/design.json` as an LUI blob) and
//! the landscape [`RunConfig`] -- plus the `#[no_mangle]`/`#[panic_handler]` entry points the shell
//! ABI requires. It starts sideways (R270) and follows the device between the two landscape poses.

#![no_std]

use light_board_touch349::{panic_report, service_core1, ShellInfo};
use light_dictaphone_touch349::{push_console_byte, run, Descent, RunConfig};
use light_draw::Rotation;
use light_input::imu::Orientation;

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));
static UI_BLOB: &[u8] = include_bytes!(env!("LIGHT_UI_LUI"));

/// Landscape only: the two horizontal poses follow the IMU end-for-end; the portrait poses are
/// ignored, so tilting the bar upright never leaves the sideways layout. The pairing is MEASURED on
/// the glass -- L->R90 rendered both poses upside down.
fn rotation_map(o: Orientation) -> Option<Rotation> {
        match o {
                Orientation::LandscapeL => Some(Rotation::R270),
                Orientation::LandscapeR => Some(Rotation::R90),
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
                        //   the resting pose is LandscapeL (R270): boot in it, or the first frames
                        // flash upside down until the IMU's first report
                        initial_rotation: Rotation::R270,
                        //   the landscape interface flows downward: a child page enters from the
                        // BOTTOM and rises into place, back sinks it back down. Logical, so it reads
                        // the same in both landscape poses.
                        default_descent: Some(Descent::FromBottom),
                        desc: "AXS15231B over PIO-QSPI, double-buffered, sideways",
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
