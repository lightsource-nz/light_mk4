//! The theme compiler: a JSON look-and-feel to an LTH blob the firmware embeds -- the
//! same assets-as-data arrangement as fonts, so restyling an interface is a data change
//! that never touches the UI crate. The blob format (magic "LTH1", an entry count, then
//! `key, length, payload` triples, all little-endian) is owned by light-ui's `theme`
//! module; this file is its writer and must agree with that parser.
//!
//! Colors are written as strings, two spellings: four hex digits are a raw RGB565 value
//! ("4C5D"), and "#RRGGBB" is 24-bit truncated to 565 -- the spelling a designer's tool
//! hands over. Unknown JSON keys are ERRORS here, not skipped: at compile time a typo
//! should stop the build, while at parse time on the firmware an unknown BINARY key is
//! skipped for forward compatibility. Strictness belongs at authoring, tolerance at
//! runtime.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::log;
use crate::CmdResult;

const MAGIC: &[u8; 4] = b"LTH1";

// the key numbers light-ui's theme::key module assigns; one table, two homes, and the
// firmware's parse tests are the contract between them
const KEY_BG: u16 = 0x0001;
const KEY_FRAME: u16 = 0x0002;
const KEY_TITLE: u16 = 0x0003;
const KEY_TEXT: u16 = 0x0004;
const KEY_BUTTON_OUTLINE: u16 = 0x0005;
const KEY_BUTTON_TEXT: u16 = 0x0006;
const KEY_FOCUS_TEXT: u16 = 0x0007;
const KEY_FOCUS_SURFACE: u16 = 0x0010;
const KEY_BUTTON_SURFACE: u16 = 0x0011;

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct ThemeSource {
        /// Documentation only; the blob carries no name.
        #[serde(default)]
        #[allow(dead_code)]
        name: Option<String>,
        #[serde(default)]
        colors: BTreeMap<String, String>,
        #[serde(default)]
        surfaces: BTreeMap<String, Option<ShadeSource>>,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct ShadeSource {
        from: String,
        to: String,
}

/// "4C5D" as raw RGB565, or "#RRGGBB" truncated to 565.
fn parse_color(s: &str) -> Result<u16, String> {
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
                _ => return Err(format!("unknown color '{name}' (bg, frame, title, text, button_outline, button_text, focus_text)")),
        })
}

fn surface_key(name: &str) -> Result<u16, String> {
        Ok(match name {
                "focus" => KEY_FOCUS_SURFACE,
                "button" => KEY_BUTTON_SURFACE,
                _ => return Err(format!("unknown surface '{name}' (focus, button)")),
        })
}

/// Compile `input` (JSON) to `output` (LTH blob).
pub fn compile(input: &Path, output: &Path) -> CmdResult {
        let text = std::fs::read_to_string(input).map_err(|e| format!("could not read '{}': {e}", input.display()))?;
        let src: ThemeSource = serde_json::from_str(&text).map_err(|e| format!("'{}': {e}", input.display()))?;

        let mut entries: Vec<(u16, Vec<u8>)> = Vec::new();
        for (name, value) in &src.colors {
                let key = color_key(name)?;
                let color = parse_color(value).map_err(|e| format!("color '{name}': {e}"))?;
                entries.push((key, color.to_le_bytes().to_vec()));
        }
        for (name, value) in &src.surfaces {
                let key = surface_key(name)?;
                //   an explicit null clears nothing -- absence already means default --
                // but tolerating it lets a theme list every surface it considered
                let Some(shade) = value else { continue };
                let from = parse_color(&shade.from).map_err(|e| format!("surface '{name}' from: {e}"))?;
                let to = parse_color(&shade.to).map_err(|e| format!("surface '{name}' to: {e}"))?;
                let mut payload = from.to_le_bytes().to_vec();
                payload.extend_from_slice(&to.to_le_bytes());
                entries.push((key, payload));
        }

        let mut blob = Vec::with_capacity(6 + entries.len() * 8);
        blob.extend_from_slice(MAGIC);
        blob.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (key, payload) in &entries {
                blob.extend_from_slice(&key.to_le_bytes());
                blob.extend_from_slice(&(payload.len() as u16).to_le_bytes());
                blob.extend_from_slice(payload);
        }

        if let Some(dir) = output.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("could not create '{}': {e}", dir.display()))?;
        }
        std::fs::write(output, &blob).map_err(|e| format!("could not write '{}': {e}", output.display()))?;
        log::info(&format!("theme '{}': {} entries, {} bytes -> {}", input.display(), entries.len(), blob.len(), output.display()));
        Ok(())
}

#[cfg(test)]
mod tests {
        use super::*;

        #[test]
        fn colors_parse_both_spellings() {
                assert_eq!(parse_color("4C5D").unwrap(), 0x4C5D);
                //   pure red 888 -> 565: F8 >> 3 = 1F in the top five bits
                assert_eq!(parse_color("#FF0000").unwrap(), 0xF800);
                assert_eq!(parse_color("#FFFFFF").unwrap(), 0xFFFF);
                assert_eq!(parse_color("#000000").unwrap(), 0x0000);
                assert!(parse_color("nope").is_err());
                assert!(parse_color("#FFF").is_err());
        }

        #[test]
        fn compile_writes_the_blob_the_firmware_parses() {
                let dir = std::env::temp_dir().join("crush_theme_test");
                std::fs::create_dir_all(&dir).unwrap();
                let src = dir.join("t.json");
                let out = dir.join("t.lth");
                std::fs::write(
                        &src,
                        r##"{ "name": "test", "colors": { "bg": "1082" }, "surfaces": { "focus": { "from": "4C5D", "to": "090E" }, "button": null } }"##,
                )
                .unwrap();
                compile(&src, &out).unwrap();
                let blob = std::fs::read(&out).unwrap();
                assert_eq!(&blob[..4], b"LTH1");
                assert_eq!(u16::from_le_bytes([blob[4], blob[5]]), 2, "bg and the focus surface; the null surface emits nothing");
                //   first entry: bg
                assert_eq!(u16::from_le_bytes([blob[6], blob[7]]), KEY_BG);
                assert_eq!(u16::from_le_bytes([blob[8], blob[9]]), 2);
                assert_eq!(u16::from_le_bytes([blob[10], blob[11]]), 0x1082);
        }

        #[test]
        fn typos_stop_the_build() {
                let dir = std::env::temp_dir().join("crush_theme_test_typo");
                std::fs::create_dir_all(&dir).unwrap();
                let src = dir.join("t.json");
                std::fs::write(&src, r##"{ "colors": { "backgroud": "0000" } }"##).unwrap();
                assert!(compile(&src, &dir.join("t.lth")).is_err());
        }
}
