//! The design data model: the JSON a UI is authored as, shared by the editor (which edits and
//! previews it) and crush (which compiles it to an LUI blob). The model is pure data; turning it
//! into a live light-ui tree (the editor) or a binary blob (crush, see [`crate::lui`]) is done
//! elsewhere.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A whole design: the target device screen, a list of pages, and which one opens first.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Design {
        /// A parent design this one extends and overrides -- named either by the CRATE whose
        /// `design.json` is the parent, or by a relative PATH to the parent design file. The child's
        /// fields deep-merge over the parent's (objects by key, arrays element-wise by index, scalars
        /// replace), so one shared design takes per-board overrides (device size, titles, metrics).
        /// Resolved by [`resolve_file`]; a fully resolved design carries none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub extends: Option<String>,
        #[serde(default)]
        pub device: Device,
        /// How the interface is laid out and shown: `portrait` (the default) stacks a `linear`
        /// window top-to-bottom on the screen as authored; `landscape` runs it left-to-right and is
        /// previewed on the screen turned sideways. It is the design's copy of the toolkit's layout
        /// axis, so a preview matches the device without the firmware's rotation being guessed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub orientation: Option<String>,
        /// The named actions a button can take -- the design's mirror of the firmware's behaviour
        /// (e.g. `light_dictaphone_core::ui_event` plus the page each opens). A button names one
        /// action; crush expands it to that button's `event` (runtime behaviour) and `goto`/`back`
        /// (so the editor previews the navigation the same way). Binds the design to its app: a UI is
        /// authored in terms of actions, defined once here.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub actions: Vec<ActionDef>,
        #[serde(default)]
        pub root: usize,
        pub pages: Vec<PageDef>,
}

impl Design {
        /// Whether the interface is laid out sideways (a horizontal layout axis, previewed on the
        /// screen turned on its long edge).
        pub fn landscape(&self) -> bool {
                self.orientation.as_deref() == Some("landscape")
        }

        /// The named action's definition, if the design declares it.
        pub fn action(&self, name: &str) -> Option<&ActionDef> {
                self.actions.iter().find(|a| a.name == name)
        }
}

/// A named button action: the app `event` it emits (0 = none) and the navigation it performs, both
/// defined once so the editor's preview and the firmware behave alike. Mirrors the firmware's own
/// event/nav for the app.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDef {
        pub name: String,
        /// The app event id this action emits (0 = none).
        #[serde(default, skip_serializing_if = "is_zero_u16")]
        pub event: u16,
        /// Navigate to this page index when taken.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub goto: Option<usize>,
        /// Navigate back when taken.
        #[serde(default, skip_serializing_if = "is_false")]
        pub back: bool,
        /// The transition for the navigation this action performs: the edge the incoming page enters
        /// from -- `top`/`bottom`/`left`/`right` (same vocabulary as a theme's descent; `bottom`
        /// rises up to cover). Compiled onto the target page, so `back` navigation mirrors it. Absent
        /// leaves the toolkit's layout-derived default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub transition: Option<String>,
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

/// One widget in a page: a button (with an optional action), a label, or a FRAME -- a container
/// with its own layout, gap and scroll that groups a flat list of `children` (one level deep: a
/// frame's children are leaves, not frames). Flat fields keep the JSON terse. `max_w`/`max_h` pin a
/// size (equal min and max fixes it); `grow` takes a linear layout's surplus.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildDef {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub button: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub label: Option<String>,
        /// A named action from the design's [`actions`](Design::actions), supplying this button's
        /// event and navigation. When set, it takes precedence over the raw `goto`/`back`/`event`
        /// below (which stay for a one-off button that names no action).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub action: Option<String>,
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
        /// Maximum size in pixels (0 = unset). `min == max` fixes the size.
        #[serde(default, skip_serializing_if = "is_zero_u16")]
        pub max_w: u16,
        #[serde(default, skip_serializing_if = "is_zero_u16")]
        pub max_h: u16,
        /// Take the surplus a linear layout leaves after the fixed/min-sized siblings.
        #[serde(default, skip_serializing_if = "is_false")]
        pub grow: bool,
        /// A frame's child arrangement (`stack`/`row`/`linear`); defaults to `stack`. Only read
        /// when `children` is non-empty.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub layout: Option<String>,
        /// A frame's gap between children (defaults to 6). Only read when `children` is non-empty.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub gap: Option<u8>,
        /// A frame's scroll axis: `vertical`, `horizontal`, or absent for none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub scroll: Option<String>,
        /// A frame's children -- a flat list of leaves (not frames). A non-empty list makes this
        /// child a frame; `button`/`label` are then ignored.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub children: Vec<ChildDef>,
}

impl ChildDef {
        /// A fresh plain button, the default an "add" inserts.
        pub fn new_button() -> Self {
                Self {
                        button: Some("Button".to_owned()),
                        label: None,
                        action: None,
                        goto: None,
                        back: false,
                        event: 0,
                        tag: 0,
                        min_w: 0,
                        min_h: 0,
                        max_w: 0,
                        max_h: 0,
                        grow: false,
                        layout: None,
                        gap: None,
                        scroll: None,
                        children: Vec::new(),
                }
        }

        /// Whether this child is a frame (a container) rather than a leaf.
        pub fn is_frame(&self) -> bool {
                !self.children.is_empty()
        }

        /// A short human label for the inspector: the kind and its text.
        pub fn describe(&self) -> String {
                if self.is_frame() {
                        format!("frame: {} children", self.children.len())
                } else if let Some(t) = &self.button {
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

/// The deepest an `extends` chain may go before it is called a cycle.
const MAX_EXTENDS_DEPTH: u8 = 8;

/// Resolve a design FILE and its [`extends`](Design::extends) chain into one merged [`Design`]. A
/// child's fields deep-merge over its parent's: objects by key, arrays element-wise by index (an
/// empty `{}` leaves that element untouched, extra elements append), scalars replace. `crates_dir`
/// is where an `extends` given by CRATE NAME finds `<name>/design.json`; an `extends` given as a
/// path (it contains a slash or ends `.json`) resolves relative to the child's own directory.
pub fn resolve_file(path: &Path, crates_dir: Option<&Path>) -> Result<Design, String> {
        let merged = resolve_file_value(path, crates_dir, MAX_EXTENDS_DEPTH)?;
        serde_json::from_value(merged).map_err(|e| format!("'{}': {e}", path.display()))
}

/// Resolve a design file to its merged JSON value, following `extends`. Kept at the value level so a
/// child need not be a valid standalone design (it may carry only the fields it overrides).
fn resolve_file_value(path: &Path, crates_dir: Option<&Path>, depth: u8) -> Result<serde_json::Value, String> {
        if depth == 0 {
                return Err(format!("'{}': the extends chain is too deep (a cycle?)", path.display()));
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("could not read '{}': {e}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("'{}': {e}", path.display()))?;
        let extends = value.get("extends").and_then(serde_json::Value::as_str).map(str::to_owned);
        if let Some(base) = extends {
                let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
                let base_path = extends_path(&base, base_dir, crates_dir).map_err(|e| format!("'{}': {e}", path.display()))?;
                let mut merged = resolve_file_value(&base_path, crates_dir, depth - 1)?;
                merge_value(&mut merged, value);
                //   the merged design is fully resolved -- drop the chain marker so it is not read again
                if let Some(obj) = merged.as_object_mut() {
                        obj.remove("extends");
                }
                return Ok(merged);
        }
        Ok(value)
}

/// Where a design's `extends` points: a relative PATH (it contains a slash or ends `.json`) resolves
/// from the child's directory; a bare CRATE NAME resolves to `<crates_dir>/<name>/design.json`.
fn extends_path(base: &str, base_dir: &std::path::Path, crates_dir: Option<&std::path::Path>) -> Result<std::path::PathBuf, String> {
        if base.contains('/') || base.contains('\\') || base.ends_with(".json") {
                Ok(base_dir.join(base))
        } else {
                let dir = crates_dir.ok_or_else(|| format!("extends '{base}' by crate name, but no crates directory was given"))?;
                Ok(dir.join(base).join("design.json"))
        }
}

/// The `extends` target of the design at `path` and its resolved PARENT (the chain above it), or
/// `None` when it does not extend anything. An editor uses the name to re-emit `extends` on save, and
/// the resolved parent as the base a child is diffed against ([`diff_overlay`]).
pub fn resolve_parent(path: &Path, crates_dir: Option<&Path>) -> Result<Option<(String, Design)>, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("could not read '{}': {e}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("'{}': {e}", path.display()))?;
        let Some(base) = value.get("extends").and_then(serde_json::Value::as_str) else {
                return Ok(None);
        };
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let base_path = extends_path(base, base_dir, crates_dir).map_err(|e| format!("'{}': {e}", path.display()))?;
        let parent = resolve_file(&base_path, crates_dir)?;
        Ok(Some((base.to_owned(), parent)))
}

/// Deep-merge `overlay` into `base` (see [`resolve_file`]).
fn merge_value(base: &mut serde_json::Value, overlay: serde_json::Value) {
        use serde_json::Value;
        match (base, overlay) {
                (Value::Object(b), Value::Object(o)) => {
                        for (k, v) in o {
                                merge_value(b.entry(k).or_insert(Value::Null), v);
                        }
                }
                (Value::Array(b), Value::Array(o)) => {
                        for (i, v) in o.into_iter().enumerate() {
                                match b.get_mut(i) {
                                        Some(slot) => merge_value(slot, v),
                                        None => b.push(v),
                                }
                        }
                }
                (b, o) => *b = o,
        }
}

/// The minimal overlay that, deep-merged over `parent`, reproduces `child` -- the inverse of the
/// resolve merge. An editor uses it to save a design that `extends` a parent as just its overrides:
/// a field equal to the parent's is dropped; an object recurses; an array keeps its changed elements
/// by index (unchanged leading ones become `{}` placeholders to hold the index). Returns a JSON
/// object (empty when `child` equals `parent`), ready to receive an `"extends"` key.
///
/// One thing the merge cannot express, so nor can this: REMOVING an element the parent has (the
/// merge only overrides or appends). A child that drops an inherited page or widget cannot be saved
/// as an overlay -- the editor authors those on the parent.
pub fn diff_overlay(parent: &Design, child: &Design) -> serde_json::Value {
        let p = serde_json::to_value(parent).unwrap_or(serde_json::Value::Null);
        let c = serde_json::to_value(child).unwrap_or(serde_json::Value::Null);
        diff_value(&p, &c).unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()))
}

/// The JSON an editor writes for a design that `extends` `base`: its overrides against `parent` (see
/// [`diff_overlay`]) with the `extends` key restored at the front, so the file stays a minimal
/// override rather than a flattened copy.
pub fn overlay_json(base: &str, parent: &Design, child: &Design) -> String {
        let mut ordered = serde_json::Map::new();
        ordered.insert("extends".to_owned(), serde_json::Value::String(base.to_owned()));
        if let serde_json::Value::Object(obj) = diff_overlay(parent, child) {
                ordered.extend(obj);
        }
        serde_json::to_string_pretty(&serde_json::Value::Object(ordered)).unwrap_or_default()
}

/// The minimal value to overlay so that merging it over `parent` yields `child`; `None` when they are
/// already equal (nothing to override).
fn diff_value(parent: &serde_json::Value, child: &serde_json::Value) -> Option<serde_json::Value> {
        use serde_json::Value;
        if parent == child {
                return None;
        }
        match (parent, child) {
                (Value::Object(p), Value::Object(c)) => {
                        let mut out = serde_json::Map::new();
                        for (k, cv) in c {
                                match p.get(k) {
                                        Some(pv) => {
                                                if let Some(d) = diff_value(pv, cv) {
                                                        out.insert(k.clone(), d);
                                                }
                                        }
                                        None => {
                                                out.insert(k.clone(), cv.clone());
                                        }
                                }
                        }
                        (!out.is_empty()).then_some(Value::Object(out))
                }
                (Value::Array(p), Value::Array(c)) => {
                        //   the last index that differs bounds the overlay; unchanged leading elements
                        // become `{}` to hold their position, trailing unchanged ones are dropped
                        let last = (0..c.len()).rev().find(|&i| p.get(i).map_or(true, |pv| pv != &c[i]))?;
                        let out = (0..=last)
                                .map(|i| match p.get(i) {
                                        Some(pv) => diff_value(pv, &c[i]).unwrap_or_else(|| Value::Object(serde_json::Map::new())),
                                        None => c[i].clone(),
                                })
                                .collect();
                        Some(Value::Array(out))
                }
                _ => Some(child.clone()),
        }
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
        fn orientation_reads_landscape() {
                assert!(parse(r#"{ "orientation": "landscape", "pages": [] }"#).unwrap().landscape());
                assert!(!parse(r#"{ "pages": [] }"#).unwrap().landscape(), "default is portrait");
        }

        #[test]
        fn device_defaults_when_omitted() {
                let d = parse(r#"{ "pages": [ { "title": "P" } ] }"#).unwrap();
                assert_eq!((d.device.width, d.device.height, d.device.corner_radius), (240, 400, 0));
        }

        #[test]
        fn merge_replaces_scalars_recurses_objects_and_indexes_arrays() {
                let mut base = serde_json::json!({
                        "device": { "width": 172, "height": 640 },
                        "pages": [
                                { "title": "Main", "children": [ { "button": "A" }, { "button": "B" } ] },
                                { "title": "List", "children": [ { "button": "R", "min_h": 44 }, { "button": "R", "min_h": 44 } ] }
                        ]
                });
                //   change the device, the first page's title, and the second page's row heights,
                // leaving the first page's children and everything else as the base has them
                let overlay = serde_json::json!({
                        "orientation": "landscape",
                        "device": { "width": 480, "height": 480 },
                        "pages": [
                                { "title": "mk4 4.0" },
                                { "children": [ { "min_h": 64 }, { "min_h": 64 } ] }
                        ]
                });
                merge_value(&mut base, overlay);
                let d: Design = serde_json::from_value(base).unwrap();
                assert_eq!((d.device.width, d.device.height), (480, 480), "the overlay's device replaces the base's");
                assert!(d.landscape(), "a field only the overlay has is added");
                assert_eq!(d.pages[0].title, "mk4 4.0");
                assert_eq!(d.pages[0].children.len(), 2, "an untouched page keeps its children");
                assert_eq!(d.pages[0].children[0].button.as_deref(), Some("A"));
                assert_eq!((d.pages[1].children[0].min_h, d.pages[1].children[1].min_h), (64, 64));
                assert_eq!(d.pages[1].children[0].button.as_deref(), Some("R"), "the merged child keeps its base fields");
        }

        #[test]
        fn diff_overlay_round_trips_through_merge() {
                let parent: Design = serde_json::from_str(r#"{
                        "device": { "width": 172, "height": 640 },
                        "pages": [
                                { "title": "Main", "children": [ { "button": "A" }, { "button": "B" } ] },
                                { "title": "List", "children": [ { "button": "R", "min_h": 44 }, { "button": "R", "min_h": 44 } ] }
                        ]
                }"#).unwrap();
                //   the child overrides the first page title and the second page's row heights
                let mut child = parent.clone();
                child.pages[0].title = "mk4 4.0".to_owned();
                child.device.width = 480;
                child.pages[1].children[0].min_h = 64;
                child.pages[1].children[1].min_h = 64;

                let overlay = diff_overlay(&parent, &child);
                //   the overlay is minimal: it does not carry the untouched first page's children
                let obj = overlay.as_object().unwrap();
                assert!(obj.contains_key("device") && obj.contains_key("pages"));
                assert!(!obj.contains_key("root"), "an unchanged field is dropped");

                //   and merging it back over the parent reproduces the child exactly
                let mut merged = serde_json::to_value(&parent).unwrap();
                merge_value(&mut merged, overlay);
                let round: Design = serde_json::from_value(merged).unwrap();
                assert_eq!(serde_json::to_value(&round).unwrap(), serde_json::to_value(&child).unwrap());
        }

        #[test]
        fn diff_overlay_is_empty_when_equal() {
                let d: Design = serde_json::from_str(r#"{ "pages": [ { "title": "P" } ] }"#).unwrap();
                assert!(diff_overlay(&d, &d).as_object().unwrap().is_empty());
        }

        #[test]
        fn extends_resolves_by_path_and_by_crate_name() {
                //   a temp layout: crates/parent_demo/design.json is the base; a child extends it by
                // crate name, and another child extends the base file by relative path
                let root = std::env::temp_dir().join(format!("crush_extends_{}", std::process::id()));
                let crates = root.join("crates");
                let parent_dir = crates.join("parent_demo");
                std::fs::create_dir_all(&parent_dir).unwrap();
                std::fs::write(parent_dir.join("design.json"), r#"{ "device": { "width": 172, "height": 640 }, "pages": [ { "title": "Base", "children": [ { "button": "A" } ] } ] }"#).unwrap();

                let by_name = root.join("by_name.design.json");
                std::fs::write(&by_name, r#"{ "extends": "parent_demo", "pages": [ { "title": "Named" } ] }"#).unwrap();
                let d = resolve_file(&by_name, Some(&crates)).unwrap();
                assert_eq!(d.pages[0].title, "Named", "the child title wins");
                assert_eq!(d.device.width, 172, "the base device is inherited");
                assert_eq!(d.pages[0].children[0].button.as_deref(), Some("A"), "the base children are inherited");
                assert!(d.extends.is_none(), "a resolved design carries no chain marker");

                let by_path = root.join("by_path.design.json");
                std::fs::write(&by_path, r#"{ "extends": "crates/parent_demo/design.json", "device": { "width": 480 } }"#).unwrap();
                let d = resolve_file(&by_path, None).unwrap();
                assert_eq!(d.device.width, 480, "the path child overrides the device width");
                assert_eq!(d.pages[0].title, "Base", "and inherits the base pages");

                let _ = std::fs::remove_dir_all(&root);
        }
}
