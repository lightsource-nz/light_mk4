//! Loading a theme from data.
//!
//! The firmware's look is authored as JSON, compiled by crush to an LTH blob, and parsed by
//! light-ui at boot (see [[light_ui_theme_system]] in the design notes). The editor loads the same
//! JSON directly into a [`Theme`] -- the colours, surfaces and metrics a designer edits, driving
//! the preview. This mirrors crush's theme schema (`tools/crush/src/theme.rs`), flat: it does not
//! resolve `extends`, so it takes a self-contained theme file. (A shared crush library would let
//! both sides share this parser instead of mirroring it; a later cleanup.)

use std::collections::BTreeMap;

use light_ui::{Descent, Shade, Theme};
use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ThemeSource {
        #[serde(default)]
        #[allow(dead_code)]
        name: Option<String>,
        #[serde(default)]
        colors: BTreeMap<String, String>,
        #[serde(default)]
        surfaces: BTreeMap<String, Option<ShadeSource>>,
        #[serde(default)]
        metrics: BTreeMap<String, u16>,
        #[serde(default)]
        descent: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShadeSource {
        from: String,
        to: String,
}

/// Parse a flat theme JSON into a [`Theme`], starting from [`Theme::DEFAULT`] and applying every
/// key present -- the same fields, spellings and colour rules crush compiles.
pub fn parse(json: &str) -> Result<Theme, String> {
        let src: ThemeSource = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let mut theme = Theme::DEFAULT;
        for (name, value) in &src.colors {
                let c = parse_color(value)?;
                match name.as_str() {
                        "bg" => theme.bg = c,
                        "frame" => theme.frame = c,
                        "title" => theme.title = c,
                        "text" => theme.text = c,
                        "button_outline" => theme.button_outline = c,
                        "button_text" => theme.button_text = c,
                        "focus_text" => theme.focus_text = c,
                        "indicator" => theme.indicator = c,
                        "bar" => theme.bar = Some(c),
                        _ => return Err(format!("unknown color '{name}'")),
                }
        }
        for (name, value) in &src.surfaces {
                //   a null surface clears it (stays None); a present one is a shade
                let shade = match value {
                        Some(s) => Some(Shade { from: parse_color(&s.from)?, to: parse_color(&s.to)? }),
                        None => None,
                };
                match name.as_str() {
                        "focus" => theme.focus_surface = shade,
                        "button" => theme.button_surface = shade,
                        _ => return Err(format!("unknown surface '{name}'")),
                }
        }
        for (name, value) in &src.metrics {
                let m = (*value).min(255) as u8;
                match name.as_str() {
                        "radius" => theme.radius = m,
                        "screen_radius" => theme.screen_radius = m,
                        _ => return Err(format!("unknown metric '{name}'")),
                }
        }
        if let Some(d) = &src.descent {
                theme.descent = Some(match d.as_str() {
                        "top" => Descent::FromTop,
                        "bottom" => Descent::FromBottom,
                        "left" => Descent::FromLeft,
                        "right" => Descent::FromRight,
                        _ => return Err(format!("unknown descent '{d}'")),
                });
        }
        Ok(theme)
}

/// "4C5D" as a raw RGB565 value, or "#RRGGBB" truncated to 565 -- crush's two spellings.
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

#[cfg(test)]
mod tests {
        use super::*;

        #[test]
        fn steel_loads_with_its_blue_bar_and_flat_button_surface() {
                let t = parse(include_str!("../../../themes/steel.json")).unwrap();
                assert_eq!(t.bar, Some(parse_color("#22405F").unwrap()), "the title bar is blue");
                assert_eq!(t.frame, 0x0000, "black lines");
                let s = t.button_surface.expect("a flat button surface");
                assert_eq!(s.from, s.to, "steel's buttons are flat, not gradient");
        }

        #[test]
        fn colors_take_both_spellings_and_typos_are_errors() {
                assert_eq!(parse_color("4C5D").unwrap(), 0x4C5D);
                assert_eq!(parse_color("#FF0000").unwrap(), 0xF800);
                assert!(parse(r#"{ "colors": { "backgroud": "0000" } }"#).is_err(), "an unknown key is rejected");
        }
}
