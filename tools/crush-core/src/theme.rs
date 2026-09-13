//! Compiling a theme to an LTH blob. The colour/key logic and the blob writer are pure; the
//! `extends` resolver ([`resolve_file`]/[`resolve_source`]) walks theme files, so both crush's
//! compile and the editor's live preview resolve a hierarchy the same way. A host tool that wants a
//! single flat theme (no `extends`) uses [`compile_flat`].
//!
//! Colors are written as strings, two spellings: four hex digits are a raw RGB565 value ("4C5D"),
//! and "#RRGGBB" is 24-bit truncated to 565. Unknown JSON keys are ERRORS: at compile time a typo
//! should stop the build, while at parse time on the firmware an unknown BINARY key is skipped for
//! forward compatibility. Strictness belongs at authoring, tolerance at runtime. The blob format
//! (magic "LTH1", a u8 schema version, an entry count, then `key, length, payload` triples,
//! little-endian) is owned by light-ui's `theme` module; this is its writer and must agree with
//! that parser.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 4] = b"LTH1";
/// The schema version in the header, matching light-ui's `theme::VERSION` -- the shared blob-header
/// convention (see also LGF fonts and LUI UIs).
const VERSION: u8 = 1;

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
#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct ThemeSource {
        /// Documentation only; the blob carries no name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        /// A base theme this one overrides. Resolution is the caller's job (it walks files); this
        /// crate only records the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub extends: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pub colors: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pub surfaces: BTreeMap<String, Option<ShadeSource>>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pub metrics: BTreeMap<String, u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub descent: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
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

        let mut blob = Vec::with_capacity(7 + entries.len() * 8);
        blob.extend_from_slice(MAGIC);
        blob.push(VERSION);
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

/// Parse a theme JSON into its editable [`ThemeSource`] (a single flat theme; no `extends`
/// resolution) -- for a tool that edits the source and re-emits it, like the design model.
pub fn parse_source(json: &str) -> Result<ThemeSource, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
}

/// Compile an editable [`ThemeSource`] to an LTH blob (no `extends` resolution).
pub fn compile_source(src: &ThemeSource) -> Result<Vec<u8>, String> {
        let mut resolved = Resolved::default();
        resolved.apply(src.clone())?;
        emit(&resolved)
}

/// Serialise a [`ThemeSource`] back to pretty JSON, defaults omitted.
pub fn source_to_json(src: &ThemeSource) -> String {
        serde_json::to_string_pretty(src).unwrap_or_default()
}

//   the extends resolver, shared by crush's compile and the editor. Deepest base first, each level
// overriding; a child's explicit `null` surface suppresses the base's shade. `themes_dir` is where
// `extends: "name"` finds `name.json`; `default` is what the `"default"` alias resolves to.

/// The deepest an `extends` chain may go before it is called a cycle.
const MAX_EXTENDS_DEPTH: u8 = 8;

/// Resolve a theme FILE and its `extends` chain into a flat [`Resolved`].
pub fn resolve_file(path: &Path, themes_dir: Option<&Path>, default: Option<&str>) -> Result<Resolved, String> {
        resolve_file_depth(path, themes_dir, default, MAX_EXTENDS_DEPTH)
}

/// Resolve an in-memory [`ThemeSource`] and its `extends` chain. `base_dir` is where a relative
/// `extends` path is resolved from (the source file's own directory) -- for a tool editing a theme
/// in memory and previewing it, matching what [`resolve_file`] does on disk.
pub fn resolve_source(src: &ThemeSource, base_dir: &Path, themes_dir: Option<&Path>, default: Option<&str>) -> Result<Resolved, String> {
        resolve_source_depth(src, base_dir, themes_dir, default, MAX_EXTENDS_DEPTH)
}

/// Resolve a theme file's `extends` chain and compile the result to an LTH blob.
pub fn compile_file(path: &Path, themes_dir: Option<&Path>, default: Option<&str>) -> Result<Vec<u8>, String> {
        emit(&resolve_file(path, themes_dir, default)?)
}

fn resolve_file_depth(path: &Path, themes_dir: Option<&Path>, default: Option<&str>, depth: u8) -> Result<Resolved, String> {
        if depth == 0 {
                return Err(format!("'{}': the extends chain is too deep (a cycle?)", path.display()));
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("could not read '{}': {e}", path.display()))?;
        let src: ThemeSource = serde_json::from_str(&text).map_err(|e| format!("'{}': {e}", path.display()))?;
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        resolve_source_depth(&src, base_dir, themes_dir, default, depth).map_err(|e| format!("'{}': {e}", path.display()))
}

fn resolve_source_depth(src: &ThemeSource, base_dir: &Path, themes_dir: Option<&Path>, default: Option<&str>, depth: u8) -> Result<Resolved, String> {
        if depth == 0 {
                return Err("the extends chain is too deep (a cycle?)".to_owned());
        }
        let mut out = match &src.extends {
                None => Resolved::default(),
                Some(base) => {
                        //   the alias first: "default" is whatever the build declared for this board
                        let base: &str = if base == "default" {
                                default.ok_or_else(|| "extends 'default', but no default theme was given".to_owned())?
                        } else {
                                base
                        };
                        let base_path = if base.contains('/') || base.contains('\\') || base.ends_with(".json") {
                                base_dir.join(base)
                        } else {
                                let dir = themes_dir.ok_or_else(|| format!("extends '{base}' by name, but no themes directory was given"))?;
                                dir.join(format!("{base}.json"))
                        };
                        resolve_file_depth(&base_path, themes_dir, default, depth - 1)?
                }
        };
        out.apply(src.clone())?;
        Ok(out)
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
                assert_eq!(blob[4], VERSION, "schema version after the magic");
                assert_eq!(u16::from_le_bytes([blob[5], blob[6]]), 2, "bg and the focus surface; the null surface emits nothing");
                assert_eq!(u16::from_le_bytes([blob[7], blob[8]]), KEY_BG);
                assert_eq!(u16::from_le_bytes([blob[11], blob[12]]), 0x1082);
        }

        #[test]
        fn resolve_source_merges_an_extends_chain() {
                let dir = std::env::temp_dir().join("crush_core_theme_resolve");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("base.json"), r##"{ "colors": { "bg": "1082", "text": "FFFF" } }"##).unwrap();
                //   an in-memory source extending the base by name, overriding bg
                let src = parse_source(r##"{ "extends": "base", "colors": { "bg": "2104" } }"##).unwrap();
                let resolved = resolve_source(&src, &dir, Some(&dir), None).unwrap();
                let blob = emit(&resolved).unwrap();
                //   bg overridden (2104), text inherited (FFFF): two entries
                assert_eq!(u16::from_le_bytes([blob[5], blob[6]]), 2, "override + inherited");
                assert_eq!(u16::from_le_bytes([blob[7], blob[8]]), KEY_BG);
                assert_eq!(u16::from_le_bytes([blob[11], blob[12]]), 0x2104, "child bg wins");
                //   the 'default' alias resolves to the given default theme name
                let aliased = parse_source(r##"{ "extends": "default" }"##).unwrap();
                assert!(resolve_source(&aliased, &dir, Some(&dir), Some("base")).is_ok());
                assert!(resolve_source(&aliased, &dir, Some(&dir), None).is_err(), "no default given");
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
                assert_eq!(u16::from_le_bytes([blob[5], blob[6]]), 2);
        }
}
