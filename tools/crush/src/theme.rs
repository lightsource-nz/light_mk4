//! The theme compiler's file-and-`extends` shell around [`crush_core::theme`]. The blob writer and
//! the pure color/key logic live in crush-core, shared with the host tools; this resolves an
//! `extends` chain by walking files (with the `--themes`/`--default` policy) and writes the result.
//!
//! Unknown JSON keys are ERRORS (see crush-core): a typo stops the build, while the firmware skips
//! an unknown BINARY key for forward compatibility. Strictness at authoring, tolerance at runtime.

use std::path::Path;

use crush_core::theme::{emit, Resolved, ThemeSource};

use crate::log;
use crate::CmdResult;

/// Load `path` and everything it extends, deepest base first, child entries overriding. A child's
/// explicit `null` surface suppresses the base's shade -- resolution happens here, at compile time,
/// and the blob is flat, which is what lets a child REMOVE a base's surface.
fn resolve(path: &Path, themes_dir: Option<&Path>, default: Option<&str>, depth: u8) -> Result<Resolved, String> {
        if depth == 0 {
                return Err(format!("'{}': the extends chain is too deep (a cycle?)", path.display()));
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("could not read '{}': {e}", path.display()))?;
        let src: ThemeSource = serde_json::from_str(&text).map_err(|e| format!("'{}': {e}", path.display()))?;
        let mut out = match &src.extends {
                None => Resolved::default(),
                Some(base) => {
                        //   the alias first: "default" is whatever the build declared for this
                        // board, so board themes need never pin a base by name
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
        //   validate + merge this level over the resolved base; a bad entry names the file it is in
        out.apply(src).map_err(|e| format!("'{}' {e}", path.display()))?;
        Ok(out)
}

/// Compile `input` (JSON, possibly extending a base) to `output` (a flat LTH blob). `default` names
/// the theme `extends: "default"` resolves to -- the board's default.
pub fn compile(input: &Path, output: &Path, themes_dir: Option<&Path>, default: Option<&str>) -> CmdResult {
        let resolved = resolve(input, themes_dir, default, 8)?;
        let blob = emit(&resolved)?;
        if let Some(dir) = output.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("could not create '{}': {e}", dir.display()))?;
        }
        std::fs::write(output, &blob).map_err(|e| format!("could not write '{}': {e}", output.display()))?;
        log::info(&format!("theme '{}': {} bytes -> {}", input.display(), blob.len(), output.display()));
        Ok(())
}

#[cfg(test)]
mod tests {
        use super::*;

        // the light-ui key numbers, inlined (crush-core keeps them private); these tests read the
        // blob's entries by (key, value)
        const KEY_BG: u16 = 0x0001;
        const KEY_TEXT: u16 = 0x0004;
        const KEY_RADIUS: u16 = 0x0020;
        const KEY_SCREEN_RADIUS: u16 = 0x0021;
        const KEY_DESCENT: u16 = 0x0040;

        //   the header is magic(4) + version(1) + count(2); entries follow at 7
        fn count(b: &[u8]) -> u16 {
                u16::from_le_bytes([b[5], b[6]])
        }
        fn entry(b: &[u8], i: usize) -> (u16, u16) {
                let at = 7 + i * 6;
                (u16::from_le_bytes([b[at], b[at + 1]]), u16::from_le_bytes([b[at + 4], b[at + 5]]))
        }

        #[test]
        fn a_child_overrides_and_clears_its_base() {
                let dir = std::env::temp_dir().join("crush_theme_test_extends");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("base.json"), r##"{ "colors": { "bg": "1082", "text": "FFFF" }, "surfaces": { "focus": { "from": "4C5D", "to": "090E" } } }"##).unwrap();
                std::fs::write(dir.join("child.json"), r##"{ "extends": "base", "colors": { "bg": "2104" }, "surfaces": { "focus": null } }"##).unwrap();
                let out = dir.join("child.lth");
                compile(&dir.join("child.json"), &out, Some(&dir), None).unwrap();
                let blob = std::fs::read(&out).unwrap();
                //   bg overridden, text inherited, the base's focus surface CLEARED by the null
                assert_eq!(count(&blob), 2);
                assert_eq!(entry(&blob, 0), (KEY_BG, 0x2104), "the child's bg wins");
                assert_eq!(entry(&blob, 1), (KEY_TEXT, 0xFFFF), "the base's text is inherited");
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
                assert_eq!(count(&blob), 2);
                assert_eq!(entry(&blob, 0), (KEY_RADIUS, 2), "the child's radius wins");
                assert_eq!(entry(&blob, 1), (KEY_SCREEN_RADIUS, 42), "the base's glass curve is inherited");
                std::fs::write(dir.join("huge.json"), r##"{ "metrics": { "radius": 300 } }"##).unwrap();
                assert!(compile(&dir.join("huge.json"), &dir.join("huge.lth"), None, None).is_err());
        }

        #[test]
        fn descent_inherits_and_overrides() {
                let dir = std::env::temp_dir().join("crush_theme_test_descent");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("base.json"), r##"{ "descent": "bottom" }"##).unwrap();
                std::fs::write(dir.join("over.json"), r##"{ "extends": "base", "descent": "top" }"##).unwrap();
                compile(&dir.join("over.json"), &dir.join("over.lth"), Some(&dir), None).unwrap();
                let b = std::fs::read(dir.join("over.lth")).unwrap();
                assert_eq!(count(&b), 1);
                assert_eq!(entry(&b, 0), (KEY_DESCENT, 0), "top=0 overrides the base's bottom");
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
                compile(&dir.join("board.json"), &dir.join("board.lth"), Some(&dir), Some("steel")).unwrap();
                let blob = std::fs::read(dir.join("board.lth")).unwrap();
                assert_eq!(count(&blob), 2, "the aliased base's color plus the board's metric");
                assert!(compile(&dir.join("board.json"), &dir.join("x.lth"), Some(&dir), None).is_err());
        }
}
