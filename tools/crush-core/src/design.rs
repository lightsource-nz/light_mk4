//! The design data model: the JSON a UI is authored as, shared by the editor (which edits and
//! previews it) and crush (which compiles it to an LUI blob). The model is pure data; turning it
//! into a live light-ui tree (the editor) or a binary blob (crush, see [`crate::lui`]) is done
//! elsewhere.

use serde::{Deserialize, Serialize};

/// A whole design: the target device screen, a list of pages, and which one opens first.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Design {
        #[serde(default)]
        pub device: Device,
        #[serde(default)]
        pub root: usize,
        pub pages: Vec<PageDef>,
}

/// The target device screen -- what the preview renders at, and its physical shape.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
        #[serde(default = "default_dev_w")]
        pub width: u16,
        #[serde(default = "default_dev_h")]
        pub height: u16,
        /// The screen's rounded-corner arc radius, in device pixels: 0 is square. Clamped to half
        /// the shorter side when used.
        #[serde(default)]
        pub corner_radius: u16,
}

impl Default for Device {
        fn default() -> Self {
                Self { width: default_dev_w(), height: default_dev_h(), corner_radius: 0 }
        }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageDef {
        pub title: String,
        #[serde(default = "default_layout")]
        pub layout: String,
        #[serde(default = "default_gap")]
        pub gap: u8,
        /// The window scrolls vertically (its content can exceed the screen).
        #[serde(default, skip_serializing_if = "is_false")]
        pub scroll: bool,
        /// The window reserves a second title row (for a runtime-set status/subtitle).
        #[serde(default, skip_serializing_if = "is_false")]
        pub subtitle: bool,
        #[serde(default)]
        pub children: Vec<ChildDef>,
}

/// One widget in a page: a button (with an optional action) or a label. Flat fields keep the JSON
/// terse.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildDef {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub button: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub label: Option<String>,
        /// A button that navigates to the page at this index.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub goto: Option<usize>,
        /// A button that goes back.
        #[serde(default, skip_serializing_if = "is_false")]
        pub back: bool,
        /// An application event id the button emits when tapped (0 = none). The app owns the
        /// meaning; navigation (goto/back) still applies alongside it.
        #[serde(default, skip_serializing_if = "is_zero_u16")]
        pub event: u16,
        /// A stable tag the app finds this widget by (to set its text at runtime); 0 = auto
        /// (the child's index + 1).
        #[serde(default, skip_serializing_if = "is_zero_u8")]
        pub tag: u8,
        /// Minimum size in pixels (0 = unset).
        #[serde(default, skip_serializing_if = "is_zero_u16")]
        pub min_w: u16,
        #[serde(default, skip_serializing_if = "is_zero_u16")]
        pub min_h: u16,
}

impl ChildDef {
        /// A fresh plain button, the default an "add" inserts.
        pub fn new_button() -> Self {
                Self { button: Some("Button".to_owned()), label: None, goto: None, back: false, event: 0, tag: 0, min_w: 0, min_h: 0 }
        }

        /// A short human label for the inspector: the kind and its text.
        pub fn describe(&self) -> String {
                if let Some(t) = &self.button {
                        let action = if self.goto.is_some() {
                                " -> goto"
                        } else if self.back {
                                " -> back"
                        } else {
                                ""
                        };
                        format!("button: {t}{action}")
                } else if let Some(t) = &self.label {
                        format!("label: {t}")
                } else {
                        "empty".to_owned()
                }
        }
}

fn default_layout() -> String {
        "stack".to_owned()
}

fn default_gap() -> u8 {
        6
}

fn default_dev_w() -> u16 {
        240
}

fn default_dev_h() -> u16 {
        400
}

fn is_false(b: &bool) -> bool {
        !*b
}

fn is_zero_u16(v: &u16) -> bool {
        *v == 0
}

fn is_zero_u8(v: &u8) -> bool {
        *v == 0
}

/// Parse a design JSON.
pub fn parse(json: &str) -> Result<Design, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
}

/// Serialise a design back to pretty JSON.
pub fn to_json(design: &Design) -> String {
        serde_json::to_string_pretty(design).unwrap_or_default()
}

#[cfg(test)]
mod tests {
        use super::*;

        #[test]
        fn unknown_fields_are_rejected() {
                assert!(parse(r#"{ "pages": [ { "title": "X", "widgets": [] } ] }"#).is_err());
        }

        #[test]
        fn round_trips_dropping_defaults() {
                let d = parse(r#"{ "pages": [ { "title": "P", "children": [ { "button": "A" }, { "label": "B" } ] } ] }"#).unwrap();
                let json = to_json(&d);
                assert!(json.contains("\"button\": \"A\""));
                assert!(!json.contains("\"goto\""), "an absent action is not written");
                assert!(!json.contains("\"back\""));
                assert_eq!(parse(&json).unwrap().pages[0].children.len(), 2);
        }

        #[test]
        fn device_defaults_when_omitted() {
                let d = parse(r#"{ "pages": [ { "title": "P" } ] }"#).unwrap();
                assert_eq!((d.device.width, d.device.height, d.device.corner_radius), (240, 400, 0));
        }
}
