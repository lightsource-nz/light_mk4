//! Materialising a design into a live light-ui tree for the editor's preview.
//!
//! The design DATA model (`Design`, `PageDef`, `ChildDef`, `Device`, parse/to_json) lives in
//! crush-core, shared with crush's LUI compiler; it is re-exported here. This file adds the
//! host-only bits: the preview's event type and the leak-based materialisation that turns the model
//! into `'static` `Desc`/`Page` the `Ui` navigates (the trick being that light-ui's builders are
//! `const fn`, so callable at runtime, and `Box::leak` gives the `'static` they want).

pub use crush_core::design::*;

use light_ui::{Desc, Page};

/// What a button does when tapped, in the editor's live preview. `()`-free so the preview can act
/// without app logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesignEvent {
        /// Navigate to the page at this index.
        Goto(u16),
        /// Return to the previous page.
        Back,
}

/// Materialise every page into a leaked `'static` light-ui page the `Ui` can navigate. Each child
/// is tagged with its index + 1 (light-ui's `find` reserves tag 0 for "untagged"), so the editor
/// can hit-test a click to a selection.
pub fn materialize(design: &Design) -> Vec<&'static Page<DesignEvent>> {
        design.pages.iter().map(materialize_page).collect()
}

fn materialize_page(pd: &PageDef) -> &'static Page<DesignEvent> {
        let mut kids: Vec<&'static Desc<DesignEvent>> = Vec::new();
        for (i, c) in pd.children.iter().enumerate() {
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
                let pages = materialize(&d);
                assert_eq!(pages.len(), 2);
        }
}
