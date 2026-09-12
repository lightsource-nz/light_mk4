//! Compiling a theme to an LTH blob -- the pure core of crush's theme compiler, with no file I/O
//! and no logging. crush wraps this with the `extends` resolver (which walks files) and the file
//! writer; a host tool compiles a single flat theme with [`compile_flat`].
//!
//! Colors are written as strings, two spellings: four hex digits are a raw RGB565 value ("4C5D"),
//! and "#RRGGBB" is 24-bit truncated to 565. Unknown JSON keys are ERRORS: at compile time a typo
//! should stop the build, while at parse time on the firmware an unknown BINARY key is skipped for
//! forward compatibility. Strictness belongs at authoring, tolerance at runtime. The blob format
//! (magic "LTH1", an entry count, then `key, length, payload` triples, little-endian) is owned by
//! light-ui's `theme` module; this is its writer and must agree with that parser.

use std::collections::BTreeMap;

use serde::Deserialize;

const MAGIC: &[u8; 4] = b"LTH1";

// the key numbers light-ui's theme::key module assigns; one table, two homes, and the firmware's
// parse tests are the contract between them
const KEY_BG: u16 = 0x0001;
const KEY_FRAME: u16 = 0x0002;
const KEY_TITLE: u16 = 0x0003;
const KEY_TEXT: u16 = 0x0004;
const KEY_BUTTON_OUTLINE: u16 = 0x0005;
const KEY_BUTTON_TEXT: u16 = 0x0006;
const KEY_FOCUS_TEXT: u16 = 0x0007;
const KEY_INDICATOR: u16 = 0x0008;
const KEY_BAR: u16 = 0x0009;
const KEY_FOCUS_SURFACE: u16 = 0x0010;
const KEY_BUTTON_SURFACE: u16 = 0x0011;
const KEY_RADIUS: u16 = 0x0020;
const KEY_SCREEN_RADIUS: u16 = 0x0021;
const KEY_DESCENT: u16 = 0x0040;

/// A theme as authored: one level, before an `extends` chain is flattened. crush's resolver reads
/// the `extends` field; everything else is merged into a [`Resolved`].
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct ThemeSource {
        /// Documentation only; the blob carries no name.
        #[serde(default)]
        #[allow(dead_code)]
        pub name: Option<String>,
        /// A base theme this one overrides. Resolution is the caller's job (it walks files); this
        /// crate only records the field.
        #[serde(default)]
        pub extends: Option<String>,
        #[serde(default)]
        pub colors: BTreeMap<String, String>,
        #[serde(default)]
        pub surfaces: BTreeMap<String, Option<ShadeSource>>,
        #[serde(default)]
        pub metrics: BTreeMap<String, u16>,
        #[serde(default)]
        pub descent: Option<String>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ShadeSource {
        pub from: String,
        pub to: String,
}

/// The flattened result of an `extends` chain: base first, each level overriding. A child's
/// explicit `null` surface stays in the map and suppresses the base's shade.
#[derive(Default)]
pub struct Resolved {
        colors: BTreeMap<String, String>,
        surfaces: BTreeMap<String, Option<ShadeSource>>,
        metrics: BTreeMap<String, u16>,
        descent: Option<String>,
}

impl Resolved {
        /// Merge one authored level over this one, validating each entry so a typo names the level
        /// it is in. `extends`/`name` are ignored -- resolving the chain is the caller's job.
        pub fn apply(&mut self, src: ThemeSource) -> Result<(), String> {
                for (name, value) in src.colors {
                        color_key(&name)?;
                        parse_color(&value).map_err(|e| format!("color '{name}': {e}"))?;
                        self.colors.insert(name, value);
                }
                for (name, value) in src.surfaces {
                        surface_key(&name)?;
                        self.surfaces.insert(name, value);
                }
                for (name, value) in src.metrics {
                        metric_key(&name)?;
                        //   the toolkit's radii are u8; catch the impossible value at authoring
                        // rather than saturating quietly
                        if value > 255 {
                                return Err(format!("metric '{name}': {value} is past the toolkit's 255 px"));
                        }
                        self.metrics.insert(name, value);
                }
                if let Some(descent) = src.descent {
                        descent_value(&descent)?;
                        self.descent = Some(descent);
                }
                Ok(())
        }
}

/// Encode a resolved theme as an LTH blob.
pub fn emit(resolved: &Resolved) -> Result<Vec<u8>, String> {
        let mut entries: Vec<(u16, Vec<u8>)> = Vec::new();
        for (name, value) in &resolved.colors {
                let key = color_key(name)?;
                let color = parse_color(value).map_err(|e| format!("color '{name}': {e}"))?;
                entries.push((key, color.to_le_bytes().to_vec()));
        }
        for (name, value) in &resolved.surfaces {
                let key = surface_key(name)?;
                //   a null is a surface listed and left unset, or a child clearing its base's --
                // both emit nothing, and the resolved map is what makes the second one work
                let Some(shade) = value else { continue };
                let from = parse_color(&shade.from).map_err(|e| format!("surface '{name}' from: {e}"))?;
                let to = parse_color(&shade.to).map_err(|e| format!("surface '{name}' to: {e}"))?;
                let mut payload = from.to_le_bytes().to_vec();
                payload.extend_from_slice(&to.to_le_bytes());
                entries.push((key, payload));
        }
        for (name, value) in &resolved.metrics {
                let key = metric_key(name)?;
                entries.push((key, value.to_le_bytes().to_vec()));
        }
        if let Some(descent) = &resolved.descent {
                entries.push((KEY_DESCENT, descent_value(descent)?.to_le_bytes().to_vec()));
        }

        let mut blob = Vec::with_capacity(6 + entries.len() * 8);
        blob.extend_from_slice(MAGIC);
        blob.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (key, payload) in &entries {
                blob.extend_from_slice(&key.to_le_bytes());
                blob.extend_from_slice(&(payload.len() as u16).to_le_bytes());
                blob.extend_from_slice(payload);
        }
        Ok(blob)
}

/// Compile a single flat theme JSON (no `extends` resolution) to an LTH blob -- what a host tool
/// wants for a self-contained theme file.
pub fn compile_flat(json: &str) -> Result<Vec<u8>, String> {
        let src: ThemeSource = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let mut resolved = Resolved::default();
        resolved.apply(src)?;
        emit(&resolved)
}

/// "4C5D" as raw RGB565, or "#RRGGBB" truncated to 565.
pub fn parse_color(s: &str) -> Result<u16, String> {
        if let Some(rgb) = s.strip_prefix('#') {
                if rgb.len() != 6 {
                        return Err(format!("'{s}': #RRGGBB wants six hex digits"));
                }
                let v = u32::from_str_radix(rgb, 16).map_err(|_| format!("'{s}': not hex"))?;
                let (r, g, b) = (v >> 16 & 0xFF, v >> 8 & 0xFF, v & 0xFF);
                return Ok(((r >> 3) << 11 | (g >> 2) << 5 | b >> 3) as u16);
        }
        if s.len() != 4 {
                return Err(format!("'{s}': a raw RGB565 color wants four hex digits (or #RRGGBB)"));
        }
        u16::from_str_radix(s, 16).map_err(|_| format!("'{s}': not hex"))
}

fn color_key(name: &str) -> Result<u16, String> {
        Ok(match name {
                "bg" => KEY_BG,
                "frame" => KEY_FRAME,
                "title" => KEY_TITLE,
                "text" => KEY_TEXT,
                "button_outline" => KEY_BUTTON_OUTLINE,
                "button_text" => KEY_BUTTON_TEXT,
                "focus_text" => KEY_FOCUS_TEXT,
                "indicator" => KEY_INDICATOR,
                "bar" => KEY_BAR,
                _ => return Err(format!("unknown color '{name}' (bg, frame, title, text, button_outline, button_text, focus_text, indicator, bar)")),
        })
}

fn surface_key(name: &str) -> Result<u16, String> {
        Ok(match name {
                "focus" => KEY_FOCUS_SURFACE,
                "button" => KEY_BUTTON_SURFACE,
                _ => return Err(format!("unknown surface '{name}' (focus, button)")),
        })
}

fn metric_key(name: &str) -> Result<u16, String> {
        Ok(match name {
                "radius" => KEY_RADIUS,
                "screen_radius" => KEY_SCREEN_RADIUS,
                _ => return Err(format!("unknown metric '{name}' (radius, screen_radius)")),
        })
}

/// The wire number for a descent edge, matching light-ui's `Theme::descent_from_u16`.
fn descent_value(name: &str) -> Result<u16, String> {
        Ok(match name {
                "top" => 0,
                "bottom" => 1,
                "left" => 2,
                "right" => 3,
                _ => return Err(format!("unknown descent '{name}' (top, bottom, left, right)")),
        })
}

#[cfg(test)]
mod tests {
        use super::*;

        #[test]
        fn colors_parse_both_spellings() {
                assert_eq!(parse_color("4C5D").unwrap(), 0x4C5D);
                assert_eq!(parse_color("#FF0000").unwrap(), 0xF800);
                assert_eq!(parse_color("#FFFFFF").unwrap(), 0xFFFF);
                assert_eq!(parse_color("#000000").unwrap(), 0x0000);
                assert!(parse_color("nope").is_err());
                assert!(parse_color("#FFF").is_err());
        }

        #[test]
        fn compile_flat_writes_the_blob_the_firmware_parses() {
                let blob = compile_flat(r##"{ "name": "test", "colors": { "bg": "1082" }, "surfaces": { "focus": { "from": "4C5D", "to": "090E" }, "button": null } }"##).unwrap();
                assert_eq!(&blob[..4], b"LTH1");
                assert_eq!(u16::from_le_bytes([blob[4], blob[5]]), 2, "bg and the focus surface; the null surface emits nothing");
                assert_eq!(u16::from_le_bytes([blob[6], blob[7]]), KEY_BG);
                assert_eq!(u16::from_le_bytes([blob[10], blob[11]]), 0x1082);
        }

        #[test]
        fn typos_are_errors() {
                assert!(compile_flat(r##"{ "colors": { "backgroud": "0000" } }"##).is_err());
                assert!(compile_flat(r##"{ "metrics": { "radius": 300 } }"##).is_err());
        }

        #[test]
        fn apply_overrides_and_a_null_clears() {
                let mut r = Resolved::default();
                r.apply(serde_json::from_str(r##"{ "colors": { "bg": "1082", "text": "FFFF" }, "surfaces": { "focus": { "from": "4C5D", "to": "090E" } } }"##).unwrap()).unwrap();
                r.apply(serde_json::from_str(r##"{ "colors": { "bg": "2104" }, "surfaces": { "focus": null } }"##).unwrap()).unwrap();
                let blob = emit(&r).unwrap();
                //   bg overridden, text inherited, the focus surface cleared by the null: two colors
                assert_eq!(u16::from_le_bytes([blob[4], blob[5]]), 2);
        }
}
