//! Reading an LUI binary UI blob -- UI as data, the zero-copy `'static`-blob arrangement fonts use
//! (see `light-font`). A design is authored, compiled by crush to this format, embedded or loaded,
//! and read here at runtime. The format is the contract with crush's `lui` compiler; the codes
//! below must match it.
//!
//! Zero-copy: every string is returned as a `&str` view into the blob, and a page is located
//! through the offset table without scanning. [`Ui::build_lui`](crate::Ui::build_lui) turns a page
//! into a live widget tree.

use crate::{Touch, Ui};

/// Format codes, shared with crush's `lui` compiler.
pub mod code {
        /// Window child arrangement.
        pub const LAYOUT_STACK: u8 = 0;
        pub const LAYOUT_ROW: u8 = 1;
        pub const LAYOUT_LINEAR: u8 = 2;
        /// Child kinds.
        pub const KIND_BUTTON: u8 = 0;
        pub const KIND_LABEL: u8 = 1;
        /// A button's navigation action.
        pub const NAV_NONE: u8 = 0;
        pub const NAV_BACK: u8 = 1;
        pub const NAV_GOTO: u8 = 2;
}

const MAGIC: [u8; 4] = *b"LUI2";
const HEADER_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LuiError {
        /// Not an LUI blob.
        BadMagic,
        /// An offset or field runs past the end of the blob.
        Truncated,
}

/// A parsed LUI blob: the header and page-offset table, over a borrowed byte slice.
#[derive(Clone, Copy)]
pub struct Lui<'a> {
        blob: &'a [u8],
}

impl<'a> Lui<'a> {
        /// Validate and wrap an LUI blob.
        pub fn parse(blob: &'a [u8]) -> Result<Self, LuiError> {
                if blob.len() < HEADER_LEN || blob[..4] != MAGIC {
                        return Err(LuiError::BadMagic);
                }
                let s = Self { blob };
                //   the offset table must fit
                if blob.len() < HEADER_LEN + 4 * s.page_count() {
                        return Err(LuiError::Truncated);
                }
                Ok(s)
        }

        fn u16(&self, at: usize) -> u16 {
                u16::from_le_bytes([self.blob[at], self.blob[at + 1]])
        }

        pub fn page_count(&self) -> usize {
                usize::from(self.u16(4))
        }

        /// The page shown first.
        pub fn root(&self) -> usize {
                usize::from(self.u16(6))
        }

        /// The target device: `(width, height, corner_radius)`.
        pub fn device(&self) -> (u16, u16, u16) {
                (self.u16(8), self.u16(10), self.u16(12))
        }

        /// The page at index `i`, located through the offset table.
        pub fn page(&self, i: usize) -> Option<LuiPage<'a>> {
                if i >= self.page_count() {
                        return None;
                }
                let at = HEADER_LEN + 4 * i;
                let off = u32::from_le_bytes([self.blob[at], self.blob[at + 1], self.blob[at + 2], self.blob[at + 3]]) as usize;
                LuiPage::parse(self.blob, off)
        }
}

/// A page view: a window (title, layout, gap, scroll, subtitle) and its children.
#[derive(Clone, Copy)]
pub struct LuiPage<'a> {
        blob: &'a [u8],
        title: &'a str,
        layout: u8,
        gap: u8,
        scroll: bool,
        subtitle: bool,
        child_count: usize,
        children_at: usize,
}

impl<'a> LuiPage<'a> {
        fn parse(blob: &'a [u8], off: usize) -> Option<Self> {
                let (title, at) = read_str(blob, off)?;
                let layout = *blob.get(at)?;
                let gap = *blob.get(at + 1)?;
                let scroll = *blob.get(at + 2)? != 0;
                let subtitle = *blob.get(at + 3)? != 0;
                let child_count = usize::from(*blob.get(at + 4)?);
                Some(Self { blob, title, layout, gap, scroll, subtitle, child_count, children_at: at + 5 })
        }

        pub fn title(&self) -> &'a str {
                self.title
        }
        pub fn layout(&self) -> u8 {
                self.layout
        }
        pub fn gap(&self) -> u8 {
                self.gap
        }
        pub fn scroll(&self) -> bool {
                self.scroll
        }
        pub fn subtitle(&self) -> bool {
                self.subtitle
        }

        /// The children in order.
        pub fn children(&self) -> LuiChildren<'a> {
                LuiChildren { blob: self.blob, at: self.children_at, remaining: self.child_count }
        }
}

/// One child widget: a button or a label, with its text, navigation, app event, tag and min-size.
#[derive(Clone, Copy)]
pub struct LuiChild<'a> {
        pub kind: u8,
        pub nav: u8,
        pub nav_page: u16,
        pub event: u16,
        pub tag: u8,
        pub min_w: u16,
        pub min_h: u16,
        pub text: &'a str,
}

/// Iterator over a page's children.
pub struct LuiChildren<'a> {
        blob: &'a [u8],
        at: usize,
        remaining: usize,
}

impl<'a> Iterator for LuiChildren<'a> {
        type Item = LuiChild<'a>;

        fn next(&mut self) -> Option<LuiChild<'a>> {
                if self.remaining == 0 {
                        return None;
                }
                let b = self.blob;
                let at = self.at;
                let kind = *b.get(at)?;
                let nav = *b.get(at + 1)?;
                let nav_page = u16::from_le_bytes([*b.get(at + 2)?, *b.get(at + 3)?]);
                let event = u16::from_le_bytes([*b.get(at + 4)?, *b.get(at + 5)?]);
                let tag = *b.get(at + 6)?;
                let min_w = u16::from_le_bytes([*b.get(at + 7)?, *b.get(at + 8)?]);
                let min_h = u16::from_le_bytes([*b.get(at + 9)?, *b.get(at + 10)?]);
                let (text, next) = read_str(b, at + 11)?;
                self.at = next;
                self.remaining -= 1;
                Some(LuiChild { kind, nav, nav_page, event, tag, min_w, min_h, text })
        }
}

/// How deep a navigation history the runtime keeps.
const HISTORY_DEPTH: usize = 16;

/// A ready runtime for displaying an LUI blob: it owns the widget tree and a navigation history,
/// resolving a tapped button's goto/back against the blob. Firmware wires its display, touch, style
/// and render loop around [`ui`](Self::ui); the editor's preview runs the same logic.
///
/// The blob must be `'static` -- `include_bytes!`'d on device, leaked on the host. The `Ui` is held
/// by `&'static mut` so it can live in a `ConstStaticCell` (a `Ui` of any size is too big to build
/// on the small firmware stack).
pub struct LuiRuntime<const N: usize> {
        /// The widget tree, for the caller to style, render and hit-test.
        pub ui: &'static mut Ui<u16, N>,
        lui: Lui<'static>,
        history: heapless::Vec<u16, HISTORY_DEPTH>,
}

impl<const N: usize> LuiRuntime<N> {
        /// Wrap a `'static` `Ui` and a parsed blob. Call [`start`](Self::start) after styling and
        /// fitting [`ui`](Self::ui).
        pub fn new(ui: &'static mut Ui<u16, N>, lui: Lui<'static>) -> Self {
                Self { ui, lui, history: heapless::Vec::new() }
        }

        /// Open the blob's root page.
        pub fn start(&mut self) {
                let root = self.lui.root().min(self.lui.page_count().saturating_sub(1)) as u16;
                self.history.clear();
                let _ = self.history.push(root);
                self.build_current();
        }

        /// Feed a touch, as [`Ui::touch`] takes it; a tapped button navigates per the blob and, if
        /// it carries an application event id, returns it for the caller to dispatch.
        pub fn touch(&mut self, x: u16, y: u16, touching: bool, now_us: u64) -> Option<u16> {
                if let Touch::Tap { emitted: Some(slot), .. } = self.ui.touch(x, y, touching, now_us) {
                        return self.activate(slot);
                }
                None
        }

        /// Resolve a child by its slot (index), as a tap would: perform its navigation and return
        /// its app event id (0 -> `None`). For a caller that wires input another way.
        pub fn activate(&mut self, slot: u16) -> Option<u16> {
                let cur = self.current_page();
                let child = self.lui.page(cur).and_then(|p| p.children().nth(usize::from(slot)))?;
                match child.nav {
                        code::NAV_GOTO => self.goto(child.nav_page),
                        code::NAV_BACK => self.back(),
                        _ => {}
                }
                (child.event != 0).then_some(child.event)
        }

        /// The index of the page currently shown.
        pub fn current_page(&self) -> usize {
                usize::from(*self.history.last().unwrap_or(&0))
        }

        fn goto(&mut self, idx: u16) {
                if usize::from(idx) < self.lui.page_count() {
                        let _ = self.history.push(idx);
                        self.build_current();
                }
        }

        fn back(&mut self) {
                if self.history.len() > 1 {
                        self.history.pop();
                        self.build_current();
                }
        }

        fn build_current(&mut self) {
                let cur = self.current_page();
                if let Some(page) = self.lui.page(cur) {
                        let _ = self.ui.build_lui(&page);
                }
        }
}

/// Read a `u8`-length-prefixed string; returns it and the offset just past it.
fn read_str(blob: &[u8], at: usize) -> Option<(&str, usize)> {
        let len = usize::from(*blob.get(at)?);
        let start = at + 1;
        let end = start + len;
        if end > blob.len() {
                return None;
        }
        core::str::from_utf8(&blob[start..end]).ok().map(|s| (s, end))
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::vec::Vec;

        fn put_str(b: &mut Vec<u8>, s: &str) {
                b.push(s.len() as u8);
                b.extend_from_slice(s.as_bytes());
        }

        //   a child in the v2 layout: kind, nav, nav_page, event, tag, min_w, min_h, text
        fn push_child(b: &mut Vec<u8>, kind: u8, nav: u8, nav_page: u16, event: u16, tag: u8, text: &str) {
                b.push(kind);
                b.push(nav);
                b.extend_from_slice(&nav_page.to_le_bytes());
                b.extend_from_slice(&event.to_le_bytes());
                b.push(tag);
                b.extend_from_slice(&0u16.to_le_bytes()); // min_w
                b.extend_from_slice(&0u16.to_le_bytes()); // min_h
                put_str(b, text);
        }

        fn header(b: &mut Vec<u8>, pages: u16) {
                b.extend_from_slice(&MAGIC);
                b.extend_from_slice(&pages.to_le_bytes());
                b.extend_from_slice(&0u16.to_le_bytes()); // root
                b.extend_from_slice(&172u16.to_le_bytes());
                b.extend_from_slice(&640u16.to_le_bytes());
                b.extend_from_slice(&0u16.to_le_bytes()); // corner
                b.extend_from_slice(&0u16.to_le_bytes()); // reserved
        }

        fn assemble(pages: &[Vec<u8>]) -> Vec<u8> {
                let mut b = Vec::new();
                header(&mut b, pages.len() as u16);
                let mut off = (HEADER_LEN + 4 * pages.len()) as u32;
                for p in pages {
                        b.extend_from_slice(&off.to_le_bytes());
                        off += p.len() as u32;
                }
                for p in pages {
                        b.extend_from_slice(p);
                }
                b
        }

        //   a hand-built blob: two pages, matching crush's v2 layout, so the reader is tested
        // without depending on the compiler (which is std/heavy)
        fn blob() -> Vec<u8> {
                let mut body0 = Vec::new();
                put_str(&mut body0, "Main");
                body0.push(code::LAYOUT_STACK);
                body0.push(6); // gap
                body0.push(0); // scroll
                body0.push(0); // subtitle
                body0.push(2); // children
                push_child(&mut body0, code::KIND_BUTTON, code::NAV_GOTO, 1, 0, 0, "Go");
                push_child(&mut body0, code::KIND_LABEL, code::NAV_NONE, 0, 0, 0, "hi");

                let mut body1 = Vec::new();
                put_str(&mut body1, "Second");
                body1.push(code::LAYOUT_STACK);
                body1.push(4); // gap
                body1.push(0); // scroll
                body1.push(0); // subtitle
                body1.push(0); // children

                assemble(&[body0, body1])
        }

        #[test]
        fn reads_header_pages_and_children() {
                let data = blob();
                let lui = Lui::parse(&data).unwrap();
                assert_eq!(lui.page_count(), 2);
                assert_eq!(lui.root(), 0);
                assert_eq!(lui.device(), (172, 640, 0));

                let p0 = lui.page(0).unwrap();
                assert_eq!(p0.title(), "Main");
                assert_eq!(p0.layout(), code::LAYOUT_STACK);
                assert_eq!(p0.gap(), 6);
                let kids: Vec<_> = p0.children().collect();
                assert_eq!(kids.len(), 2);
                assert_eq!(kids[0].kind, code::KIND_BUTTON);
                assert_eq!(kids[0].nav, code::NAV_GOTO);
                assert_eq!(kids[0].nav_page, 1);
                assert_eq!(kids[0].text, "Go");
                assert_eq!(kids[1].kind, code::KIND_LABEL);
                assert_eq!(kids[1].text, "hi");

                let p1 = lui.page(1).unwrap();
                assert_eq!(p1.title(), "Second");
                assert_eq!(p1.children().count(), 0);
                assert!(lui.page(2).is_none());
        }

        #[test]
        fn rejects_a_non_lui_blob() {
                assert!(matches!(Lui::parse(b"nope............"), Err(LuiError::BadMagic)));
                assert!(matches!(Lui::parse(&[]), Err(LuiError::BadMagic)));
        }

        //   two pages: page 0 has a button that goes to page 1, page 1 a button that goes back
        fn nav_blob() -> Vec<u8> {
                let page = |title: &str, btn: &str, nav: u8, nav_page: u16| {
                        let mut b = Vec::new();
                        put_str(&mut b, title);
                        b.push(code::LAYOUT_STACK);
                        b.push(6); // gap
                        b.push(0); // scroll
                        b.push(0); // subtitle
                        b.push(1); // children
                        push_child(&mut b, code::KIND_BUTTON, nav, nav_page, 0, 0, btn);
                        b
                };
                assemble(&[page("Main", "Go", code::NAV_GOTO, 1), page("Second", "Back", code::NAV_BACK, 0)])
        }

        #[test]
        fn runtime_navigates_goto_and_back() {
                let data: &'static [u8] = Vec::leak(nav_blob());
                let lui = Lui::parse(data).unwrap();
                let ui: &'static mut Ui<u16, 16> = std::boxed::Box::leak(std::boxed::Box::new(Ui::new()));
                let mut rt: LuiRuntime<16> = LuiRuntime::new(ui, lui);
                rt.start();
                assert_eq!(rt.current_page(), 0);
                rt.activate(0); // "Go" -> page 1
                assert_eq!(rt.current_page(), 1);
                rt.activate(0); // "Back" -> page 0
                assert_eq!(rt.current_page(), 0);
                rt.activate(0); // "Go" again -> page 1, history deepens without underflow
                assert_eq!(rt.current_page(), 1);
        }

        #[test]
        fn build_lui_with_drives_a_typed_ui() {
                use crate::Ui;
                //   a typed event Ui (not u16) built from a blob via the mapping closure
                let data: &'static [u8] = Vec::leak(blob());
                let page = Lui::parse(data).unwrap().page(0).unwrap();
                let ui: &'static mut Ui<i32, 16> = std::boxed::Box::leak(std::boxed::Box::new(Ui::new()));
                ui.build_lui_with(&page, |i, _child| Some(i as i32 + 100)).unwrap();
                assert_eq!(ui.widget_text(ui.find(1).expect("child 1")), Some("Go"));
                assert_eq!(ui.widget_text(ui.find(2).expect("child 2")), Some("hi"));
        }

        #[test]
        fn build_lui_builds_a_window_with_its_children() {
                use crate::Ui;
                //   a 'static blob (leaked once) so build_lui's `&'static str` requirement holds
                let data: &'static [u8] = Vec::leak(blob());
                let lui = Lui::parse(data).unwrap();
                let page = lui.page(0).unwrap();
                let mut ui: Ui<u16, 16> = Ui::new();
                ui.build_lui(&page).unwrap();
                //   the two children are tagged 1 and 2; the button carries "Go", the label "hi"
                let btn = ui.find(1).expect("child 1");
                assert_eq!(ui.widget_text(btn), Some("Go"));
                let lbl = ui.find(2).expect("child 2");
                assert_eq!(ui.widget_text(lbl), Some("hi"));
                assert!(ui.find(3).is_none(), "only two children");
        }
}
