//! A design loaded from data, materialised into a light-ui tree -- and now editable and saved
//! back.
//!
//! light-ui trees are compile-time `const Desc`/`Page`, but their builders are `const fn` -- which
//! means they are callable at runtime too -- and `.children()` takes `&'static [...]`, which
//! `Box::leak` satisfies. So a design authored as JSON becomes a real light-ui tree at runtime with
//! NO change to light-ui: each page is materialised into leaked `'static` values the `Ui` navigates
//! like any embedded tree, and re-materialised whenever an edit changes the model.
//!
//! Navigation is event-driven rather than baked into the descriptors: a button carries a
//! [`DesignEvent`] naming where to go, and the preview resolves it against the page list and keeps
//! its own history. That keeps every page independent -- no page references another -- so there is
//! no cyclic `'static` graph to construct.
//!
//! The leak is per-materialisation: fine for a design edited a handful of times. Reclaiming the old
//! tree would want an arena; a later concern.

use light_ui::{Desc, Page};
use serde::{Deserialize, Serialize};

/// What a button does when tapped. `()`-free so the preview can act without app logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesignEvent {
        /// Navigate to the page at this index.
        Goto(u16),
        /// Return to the previous page.
        Back,
}

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
        /// The screen's rounded-corner arc radius, in device pixels: 0 is square. What the preview
        /// rounds the screen and bezel by; clamped to half the shorter side.
        #[serde(default)]
        pub corner_radius: u16,
}

impl Default for Device {
        fn default() -> Self {
                Self { width: default_dev_w(), height: default_dev_h(), corner_radius: 0 }
        }
}

fn default_dev_w() -> u16 {
        240
}

fn default_dev_h() -> u16 {
        400
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageDef {
        pub title: String,
        #[serde(default = "default_layout")]
        pub layout: String,
        #[serde(default = "default_gap")]
        pub gap: u8,
        #[serde(default)]
        pub children: Vec<ChildDef>,
}

/// One widget in a page. A child is a button (with an optional action) or a label; the fields are
/// flat so the JSON stays terse.
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
}

impl ChildDef {
        /// A fresh plain button, the default an "add" inserts.
        pub fn new_button() -> Self {
                Self { button: Some("Button".to_owned()), label: None, goto: None, back: false }
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

fn is_false(b: &bool) -> bool {
        !*b
}

/// Parse a design JSON.
pub fn parse(json: &str) -> Result<Design, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
}

/// Serialise a design back to pretty JSON.
pub fn to_json(design: &Design) -> String {
        serde_json::to_string_pretty(design).unwrap_or_default()
}

/// Materialise every page into a leaked `'static` light-ui page the `Ui` can navigate. Each child
/// is tagged with its index in the page, so the editor can hit-test a click to a selection.
pub fn materialize(design: &Design) -> Vec<&'static Page<DesignEvent>> {
        design.pages.iter().map(materialize_page).collect()
}

fn materialize_page(pd: &PageDef) -> &'static Page<DesignEvent> {
        let mut kids: Vec<&'static Desc<DesignEvent>> = Vec::new();
        for (i, c) in pd.children.iter().enumerate() {
                //   tag = index + 1: light-ui's `find` treats tag 0 as "untagged", so children are
                // numbered from 1 and the editor maps a tag back to an index by subtracting one
                let tag = (i + 1) as u8;
                let desc = if let Some(text) = &c.button {
                        let base = Desc::button(leak_str(text)).tag(tag);
                        if let Some(g) = c.goto {
                                base.emit(DesignEvent::Goto(g as u16))
                        } else if c.back {
                                base.emit(DesignEvent::Back)
                        } else {
                                base
                        }
                } else if let Some(text) = &c.label {
                        Desc::label(leak_str(text)).tag(tag)
                } else {
                        continue;
                };
                //   the typed binding coerces the leaked `&mut` to the `&` the tree holds
                let leaked: &'static Desc<DesignEvent> = Box::leak(Box::new(desc));
                kids.push(leaked);
        }
        let children: &'static [&'static Desc<DesignEvent>] = Vec::leak(kids);
        let win = Desc::window(leak_str(&pd.title));
        let win = match pd.layout.as_str() {
                "row" => win.row(pd.gap),
                "linear" => win.linear(pd.gap),
                _ => win.stack(pd.gap),
        }
        .children(children);
        let win: &'static Desc<DesignEvent> = Box::leak(Box::new(win));
        let page: &'static Page<DesignEvent> = Box::leak(Box::new(Page::new(win, None)));
        page
}

/// Leak an owned copy of `s` as a `'static` string, for the descriptors that want one.
fn leak_str(s: &str) -> &'static str {
        Box::leak(s.to_owned().into_boxed_str())
}

#[cfg(test)]
mod tests {
        use super::*;

        #[test]
        fn a_two_page_design_materialises() {
                let d = parse(r#"{ "root": 0, "pages": [
                        { "title": "Main", "children": [ { "button": "Go", "goto": 1 } ] },
                        { "title": "Second", "children": [ { "button": "Back", "back": true }, { "label": "hi" } ] }
                ] }"#)
                .unwrap();
                assert_eq!(d.root, 0);
                let pages = materialize(&d);
                assert_eq!(pages.len(), 2);
        }

        #[test]
        fn unknown_fields_are_rejected() {
                assert!(parse(r#"{ "pages": [ { "title": "X", "widgets": [] } ] }"#).is_err());
        }

        #[test]
        fn round_trips_through_json_dropping_defaults() {
                let d = parse(r#"{ "pages": [ { "title": "P", "children": [ { "button": "A" }, { "label": "B" } ] } ] }"#).unwrap();
                let json = to_json(&d);
                //   a plain button serialises without null goto/label or a false back
                assert!(json.contains("\"button\": \"A\""));
                assert!(!json.contains("\"goto\""), "an absent action is not written");
                assert!(!json.contains("\"back\""));
                // and it parses back
                assert_eq!(parse(&json).unwrap().pages[0].children.len(), 2);
        }
}
