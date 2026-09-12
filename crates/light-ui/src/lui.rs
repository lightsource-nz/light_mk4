//! Reading an LUI binary UI blob -- UI as data, the zero-copy `'static`-blob arrangement fonts use
//! (see `light-font`). A design is authored, compiled by crush to this format, embedded or loaded,
//! and read here at runtime. The format is the contract with crush's `lui` compiler; the codes
//! below must match it.
//!
//! Zero-copy: every string is returned as a `&str` view into the blob, and a page is located
//! through the offset table without scanning. [`Ui::build_lui`](crate::Ui::build_lui) turns a page
//! into a live widget tree.

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

const MAGIC: [u8; 4] = *b"LUI1";
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

/// A page view: a window (title, layout, gap) and its children.
#[derive(Clone, Copy)]
pub struct LuiPage<'a> {
        blob: &'a [u8],
        title: &'a str,
        layout: u8,
        gap: u8,
        child_count: usize,
        children_at: usize,
}

impl<'a> LuiPage<'a> {
        fn parse(blob: &'a [u8], off: usize) -> Option<Self> {
                let (title, at) = read_str(blob, off)?;
                let layout = *blob.get(at)?;
                let gap = *blob.get(at + 1)?;
                let child_count = usize::from(*blob.get(at + 2)?);
                Some(Self { blob, title, layout, gap, child_count, children_at: at + 3 })
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

        /// The children in order.
        pub fn children(&self) -> LuiChildren<'a> {
                LuiChildren { blob: self.blob, at: self.children_at, remaining: self.child_count }
        }
}

/// One child widget: a button or a label, with its text and (for a button) its navigation.
#[derive(Clone, Copy)]
pub struct LuiChild<'a> {
        pub kind: u8,
        pub nav: u8,
        pub nav_page: u16,
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
                let kind = *self.blob.get(self.at)?;
                let nav = *self.blob.get(self.at + 1)?;
                let nav_page = u16::from_le_bytes([*self.blob.get(self.at + 2)?, *self.blob.get(self.at + 3)?]);
                let (text, next) = read_str(self.blob, self.at + 4)?;
                self.at = next;
                self.remaining -= 1;
                Some(LuiChild { kind, nav, nav_page, text })
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

        //   a hand-built blob: two pages, matching crush's layout, so the reader is tested without
        // depending on the compiler (which is std/heavy)
        fn blob() -> Vec<u8> {
                let mut body0 = Vec::new();
                put_str(&mut body0, "Main");
                body0.push(code::LAYOUT_STACK);
                body0.push(6);
                body0.push(2); // children
                                // button "Go" goto 1
                body0.push(code::KIND_BUTTON);
                body0.push(code::NAV_GOTO);
                body0.extend_from_slice(&1u16.to_le_bytes());
                put_str(&mut body0, "Go");
                // label "hi"
                body0.push(code::KIND_LABEL);
                body0.push(code::NAV_NONE);
                body0.extend_from_slice(&0u16.to_le_bytes());
                put_str(&mut body0, "hi");

                let mut body1 = Vec::new();
                put_str(&mut body1, "Second");
                body1.push(code::LAYOUT_STACK);
                body1.push(4);
                body1.push(0);

                let mut b = Vec::new();
                b.extend_from_slice(&MAGIC);
                b.extend_from_slice(&2u16.to_le_bytes()); // pages
                b.extend_from_slice(&0u16.to_le_bytes()); // root
                b.extend_from_slice(&172u16.to_le_bytes());
                b.extend_from_slice(&640u16.to_le_bytes());
                b.extend_from_slice(&8u16.to_le_bytes());
                b.extend_from_slice(&0u16.to_le_bytes()); // reserved
                let mut off = (HEADER_LEN + 8) as u32;
                b.extend_from_slice(&off.to_le_bytes());
                off += body0.len() as u32;
                b.extend_from_slice(&off.to_le_bytes());
                b.extend_from_slice(&body0);
                b.extend_from_slice(&body1);
                b
        }

        fn put_str(b: &mut Vec<u8>, s: &str) {
                b.push(s.len() as u8);
                b.extend_from_slice(s.as_bytes());
        }

        #[test]
        fn reads_header_pages_and_children() {
                let data = blob();
                let lui = Lui::parse(&data).unwrap();
                assert_eq!(lui.page_count(), 2);
                assert_eq!(lui.root(), 0);
                assert_eq!(lui.device(), (172, 640, 8));

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
