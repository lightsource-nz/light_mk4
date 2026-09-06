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
const KEY_RADIUS: u16 = 0x0020;
const KEY_SCREEN_RADIUS: u16 = 0x0021;

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct ThemeSource {
        /// Documentation only; the blob carries no name.
        #[serde(default)]
        #[allow(dead_code)]
        name: Option<String>,
        /// A base theme this one overrides: a path (relative to this file) when it
        /// contains a separator or ends in .json, otherwise a name resolved in the
        /// framework themes directory (`--themes`). The name `default` is an alias for
        /// whatever `--default` names -- the build system's per-board default theme --
        /// so a board override inherits the right base without pinning its name.
        /// Resolution happens HERE, at compile time, and the blob is flat -- which is
        /// what lets a child's explicit `null` surface REMOVE the base's, something a
        /// sparse blob could never express.
        #[serde(default)]
        extends: Option<String>,
        #[serde(default)]
        colors: BTreeMap<String, String>,
        #[serde(default)]
        surfaces: BTreeMap<String, Option<ShadeSource>>,
        /// Pixel measurements: `radius` (every container and control; the firmware's
        /// default is small, not square) and `screen_radius` (the glass's corner
        /// curvature, worn by the outer container -- zero means a square screen).
        #[serde(default)]
        metrics: BTreeMap<String, u16>,
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

fn metric_key(name: &str) -> Result<u16, String> {
        Ok(match name {
                "radius" => KEY_RADIUS,
                "screen_radius" => KEY_SCREEN_RADIUS,
                _ => return Err(format!("unknown metric '{name}' (radius, screen_radius)")),
        })
}

/// The flattened result of an `extends` chain: base first, each level overriding.
#[derive(Default)]
struct Resolved {
        colors: BTreeMap<String, String>,
        surfaces: BTreeMap<String, Option<ShadeSource>>,
        metrics: BTreeMap<String, u16>,
}

/// Load `path` and everything it extends, deepest base first, child entries overriding.
/// A child's explicit `null` surface stays in the map and suppresses the base's shade.
fn resolve(path: &Path, themes_dir: Option<&Path>, default: Option<&str>, depth: u8) -> Result<Resolved, String> {
        if depth == 0 {
                return Err(format!("'{}': the extends chain is too deep (a cycle?)", path.display()));
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("could not read '{}': {e}", path.display()))?;
        let src: ThemeSource = serde_json::from_str(&text).map_err(|e| format!("'{}': {e}", path.display()))?;
        let mut out = match &src.extends {
                None => Resolved::default(),
                Some(base) => {
                        //   the alias first: "default" is whatever the build declared for
                        // this board, so board themes need never pin a base by name
                        let base: &str = if base == "default" {
                                match default {
                                        Some(d) => d,
                                        None => return Err(format!("'{}' extends 'default', but no default theme was given (--default)", path.display())),
                                }
                        } else {
                                base
                        };
                        let base_path = if base.contains('/') || base.contains('\\') || base.ends_with(".json") {
                                path.parent().unwrap_or(Path::new(".")).join(base)
                        } else {
                                let Some(dir) = themes_dir else {
                                        return Err(format!("'{}' extends '{base}' by name, but no themes directory was given (--themes)", path.display()));
                                };
                                dir.join(format!("{base}.json"))
                        };
                        resolve(&base_path, themes_dir, default, depth - 1)?
                }
        };
        //   validate at every level, so a typo in a base names the base
        for (name, value) in src.colors {
                color_key(&name)?;
                parse_color(&value).map_err(|e| format!("'{}' color '{name}': {e}", path.display()))?;
                out.colors.insert(name, value);
        }
        for (name, value) in src.surfaces {
                surface_key(&name)?;
                out.surfaces.insert(name, value);
        }
        for (name, value) in src.metrics {
                metric_key(&name)?;
                //   the toolkit's radii are u8; the strict side of the contract catches
                // the impossible value at authoring time rather than saturating quietly
                if value > 255 {
                        return Err(format!("'{}' metric '{name}': {value} is past the toolkit's 255 px", path.display()));
                }
                out.metrics.insert(name, value);
        }
        Ok(out)
}

/// Compile `input` (JSON, possibly extending a base) to `output` (a flat LTH blob).
/// `default` names the theme `extends: "default"` resolves to -- the board's default.
pub fn compile(input: &Path, output: &Path, themes_dir: Option<&Path>, default: Option<&str>) -> CmdResult {
        let src = resolve(input, themes_dir, default, 8)?;

        let mut entries: Vec<(u16, Vec<u8>)> = Vec::new();
        for (name, value) in &src.colors {
                let key = color_key(name)?;
                let color = parse_color(value).map_err(|e| format!("color '{name}': {e}"))?;
                entries.push((key, color.to_le_bytes().to_vec()));
        }
        for (name, value) in &src.surfaces {
                let key = surface_key(name)?;
                //   a null here is either a surface the theme listed and left unset, or a
                // child clearing its base's -- both simply emit nothing, and the resolved
                // map is what makes the second one work
                let Some(shade) = value else { continue };
                let from = parse_color(&shade.from).map_err(|e| format!("surface '{name}' from: {e}"))?;
                let to = parse_color(&shade.to).map_err(|e| format!("surface '{name}' to: {e}"))?;
                let mut payload = from.to_le_bytes().to_vec();
                payload.extend_from_slice(&to.to_le_bytes());
                entries.push((key, payload));
        }
        for (name, value) in &src.metrics {
                let key = metric_key(name)?;
                entries.push((key, value.to_le_bytes().to_vec()));
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
                compile(&src, &out, None, None).unwrap();
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
                assert!(compile(&src, &dir.join("t.lth"), None, None).is_err());
        }

        #[test]
        fn a_child_overrides_and_clears_its_base() {
                let dir = std::env::temp_dir().join("crush_theme_test_extends");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                        dir.join("base.json"),
                        r##"{ "colors": { "bg": "1082", "text": "FFFF" }, "surfaces": { "focus": { "from": "4C5D", "to": "090E" } } }"##,
                )
                .unwrap();
                std::fs::write(
                        dir.join("child.json"),
                        r##"{ "extends": "base", "colors": { "bg": "2104" }, "surfaces": { "focus": null } }"##,
                )
                .unwrap();
                let out = dir.join("child.lth");
                compile(&dir.join("child.json"), &out, Some(&dir), None).unwrap();
                let blob = std::fs::read(&out).unwrap();
                //   bg overridden, text inherited, the base's focus surface CLEARED by the
                // child's null: two color entries, no surface entry
                assert_eq!(u16::from_le_bytes([blob[4], blob[5]]), 2);
                let entry = |i: usize| {
                        let at = 6 + i * 6;
                        (u16::from_le_bytes([blob[at], blob[at + 1]]), u16::from_le_bytes([blob[at + 4], blob[at + 5]]))
                };
                assert_eq!(entry(0), (KEY_BG, 0x2104), "the child's bg wins");
                assert_eq!(entry(1), (KEY_TEXT, 0xFFFF), "the base's text is inherited");
        }

        #[test]
        fn metrics_compile_and_inherit() {
                let dir = std::env::temp_dir().join("crush_theme_test_metrics");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("base.json"), r##"{ "metrics": { "radius": 5, "screen_radius": 42 } }"##).unwrap();
                std::fs::write(dir.join("child.json"), r##"{ "extends": "base", "metrics": { "radius": 2 } }"##).unwrap();
                let out = dir.join("child.lth");
                compile(&dir.join("child.json"), &out, Some(&dir), None).unwrap();
                let blob = std::fs::read(&out).unwrap();
                assert_eq!(u16::from_le_bytes([blob[4], blob[5]]), 2);
                let entry = |i: usize| {
                        let at = 6 + i * 6;
                        (u16::from_le_bytes([blob[at], blob[at + 1]]), u16::from_le_bytes([blob[at + 4], blob[at + 5]]))
                };
                assert_eq!(entry(0), (KEY_RADIUS, 2), "the child's radius wins");
                assert_eq!(entry(1), (KEY_SCREEN_RADIUS, 42), "the base's glass curve is inherited");
                //   strict authoring: a typoed metric and an impossible value both stop the build
                std::fs::write(dir.join("typo.json"), r##"{ "metrics": { "corner": 3 } }"##).unwrap();
                assert!(compile(&dir.join("typo.json"), &dir.join("typo.lth"), None, None).is_err());
                std::fs::write(dir.join("huge.json"), r##"{ "metrics": { "radius": 300 } }"##).unwrap();
                assert!(compile(&dir.join("huge.json"), &dir.join("huge.lth"), None, None).is_err());
        }

        #[test]
        fn extends_cycles_stop_the_build() {
                let dir = std::env::temp_dir().join("crush_theme_test_cycle");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("a.json"), r##"{ "extends": "b" }"##).unwrap();
                std::fs::write(dir.join("b.json"), r##"{ "extends": "a" }"##).unwrap();
                assert!(compile(&dir.join("a.json"), &dir.join("a.lth"), Some(&dir), None).is_err());
        }

        #[test]
        fn extends_by_name_without_a_themes_dir_is_an_error() {
                let dir = std::env::temp_dir().join("crush_theme_test_nodir");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("t.json"), r##"{ "extends": "steel" }"##).unwrap();
                assert!(compile(&dir.join("t.json"), &dir.join("t.lth"), None, None).is_err());
        }

        #[test]
        fn extends_default_is_the_boards_declared_base() {
                let dir = std::env::temp_dir().join("crush_theme_test_default");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("steel.json"), r##"{ "colors": { "frame": "4C5D" } }"##).unwrap();
                std::fs::write(dir.join("board.json"), r##"{ "extends": "default", "metrics": { "screen_radius": 42 } }"##).unwrap();
                let out = dir.join("board.lth");
                compile(&dir.join("board.json"), &out, Some(&dir), Some("steel")).unwrap();
                let blob = std::fs::read(&out).unwrap();
                assert_eq!(u16::from_le_bytes([blob[4], blob[5]]), 2, "the aliased base's color plus the board's metric");
                //   and without a declared default the alias is an authoring error
                assert!(compile(&dir.join("board.json"), &dir.join("x.lth"), Some(&dir), None).is_err());
        }
}
