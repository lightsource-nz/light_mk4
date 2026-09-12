//! A design loaded from data, materialised into a light-ui tree.
//!
//! light-ui trees are compile-time `const Desc`/`Page`, but their builders are `const fn` -- which
//! means they are callable at runtime too -- and `.children()` takes `&'static [...]`, which
//! `Box::leak` satisfies. So a design authored as JSON becomes a real light-ui tree at runtime with
//! NO change to light-ui: each page is materialised into leaked `'static` values the `Ui` navigates
//! like any embedded tree.
//!
//! Navigation is event-driven rather than baked into the descriptors: a button carries a
//! [`DesignEvent`] naming where to go, and the preview resolves it against the page list and keeps
//! its own history. That keeps every page independent -- no page references another -- so there is
//! no cyclic `'static` graph to construct.
//!
//! The leak is for the program's life: fine for a design loaded once. Reloading would leak the old
//! tree; an arena that reclaims it is a later concern.

use light_ui::{Desc, Page};
use serde::Deserialize;

/// What a button does when tapped. `()`-free so the preview can act without app logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesignEvent {
        /// Navigate to the page at this index.
        Goto(u16),
        /// Return to the previous page.
        Back,
}

/// A whole design: a list of pages and which one opens first.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Design {
        #[serde(default)]
        pub root: usize,
        pub pages: Vec<PageDef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageDef {
        title: String,
        #[serde(default = "default_layout")]
        layout: String,
        #[serde(default = "default_gap")]
        gap: u8,
        #[serde(default)]
        children: Vec<ChildDef>,
}

/// One widget in a page. A child is a button (with an optional action) or a label; the fields are
/// flat so the JSON stays terse.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildDef {
        #[serde(default)]
        button: Option<String>,
        #[serde(default)]
        label: Option<String>,
        /// A button that navigates to the page at this index.
        #[serde(default)]
        goto: Option<usize>,
        /// A button that goes back.
        #[serde(default)]
        back: bool,
}

fn default_layout() -> String {
        "stack".to_owned()
}

fn default_gap() -> u8 {
        6
}

/// Parse a design JSON.
pub fn parse(json: &str) -> Result<Design, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
}

/// Materialise every page into a leaked `'static` light-ui page the `Ui` can navigate.
pub fn materialize(design: &Design) -> Vec<&'static Page<DesignEvent>> {
        design.pages.iter().map(materialize_page).collect()
}

fn materialize_page(pd: &PageDef) -> &'static Page<DesignEvent> {
        let mut kids: Vec<&'static Desc<DesignEvent>> = Vec::new();
        for c in &pd.children {
                let desc = if let Some(text) = &c.button {
                        let base = Desc::button(leak_str(text));
                        if let Some(g) = c.goto {
                                base.emit(DesignEvent::Goto(g as u16))
                        } else if c.back {
                                base.emit(DesignEvent::Back)
                        } else {
                                base
                        }
                } else if let Some(text) = &c.label {
                        Desc::label(leak_str(text))
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
}
