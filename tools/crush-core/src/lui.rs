//! The LUI binary UI format: a design compiled to a flat, zero-copy blob that light-ui reads and
//! displays. UI as data, the same arrangement as fonts (LGF) and themes (LTH) -- authored, compiled
//! by crush, embedded or loaded, parsed at runtime. The format is owned here and by light-ui's
//! reader; the two must agree (the codes below are the contract).
//!
//! Layout, little-endian:
//! - Header (16 bytes): magic "LUI3", `version` u8 (the schema version -- the shared blob-header
//!   convention, see LGF fonts and LTH themes), `reserved` u8, `page_count` u16, `root` u16, device
//!   `width`/`height`/`corner_radius` u16 each.
//! - Page-offset table: `page_count` * u32, each the byte offset of a page from the blob start.
//! - Pages: each is `title` (u8 len + bytes), `layout` u8, `gap` u8, `scroll` u8, `subtitle` u8,
//!   `child_count` u8, then each child.
//! - Child: a common prefix -- `kind` u8, `nav` u8, `nav_page` u16, `event` u16, `tag` u8, `min_w`
//!   u16, `min_h` u16, `max_w` u16, `max_h` u16, `grow` u8 -- then, by kind:
//!   - a FRAME: `layout` u8, `gap` u8, `scroll` u8 (flags), `child_count` u8, then that many LEAF
//!     children (a frame's children are leaves -- nesting is one level deep).
//!   - a BUTTON/LABEL (leaf): `text` (u8 len + bytes).
//!
//! Strings are inline and length-prefixed, so a reader returns `&str` views into the blob with no
//! copy -- the LGF pattern.

use crate::design::ChildDef;
use crate::design::Design;

/// The blob magic: "LUI3", Light UI. The magic is now the frozen format-family tag; the schema
/// revision is carried in the [`VERSION`] header byte, not by bumping the magic.
pub const MAGIC: &[u8; 4] = b"LUI3";
/// The schema version in the header, matching light-ui's `lui::VERSION`. Version 1 is the layout
/// historically called LUIv3 (one-level frame nesting, per-child max-size/grow); the shared
/// blob-header convention with LGF fonts and LTH themes.
pub const VERSION: u8 = 1;
/// The fixed header length.
pub const HEADER_LEN: usize = 16;

// Layout codes (a window's child arrangement).
pub const LAYOUT_STACK: u8 = 0;
pub const LAYOUT_ROW: u8 = 1;
pub const LAYOUT_LINEAR: u8 = 2;

// Child kinds.
pub const KIND_BUTTON: u8 = 0;
pub const KIND_LABEL: u8 = 1;
pub const KIND_FRAME: u8 = 2;

// Scroll flags (a frame's scroll axis), matching light-ui's `scroll` module.
pub const SCROLL_NONE: u8 = 0;
pub const SCROLL_VERTICAL: u8 = 1 << 0;
pub const SCROLL_HORIZONTAL: u8 = 1 << 1;

// Navigation actions a button carries.
pub const NAV_NONE: u8 = 0;
pub const NAV_BACK: u8 = 1;
pub const NAV_GOTO: u8 = 2;

fn layout_code(s: &str) -> u8 {
        match s {
                "row" => LAYOUT_ROW,
                "linear" => LAYOUT_LINEAR,
                _ => LAYOUT_STACK,
        }
}

fn scroll_code(s: Option<&str>) -> u8 {
        match s {
                Some("vertical") => SCROLL_VERTICAL,
                Some("horizontal") => SCROLL_HORIZONTAL,
                _ => SCROLL_NONE,
        }
}

/// Write one child (its common prefix, then its kind-specific body). `depth` guards the one-level
/// rule: a frame's children must be leaves.
fn put_child(b: &mut Vec<u8>, c: &ChildDef, depth: u8) -> Result<(), String> {
        let is_frame = c.is_frame();
        let (kind, text) = if is_frame {
                (KIND_FRAME, "")
        } else if let Some(t) = &c.button {
                (KIND_BUTTON, t.as_str())
        } else if let Some(t) = &c.label {
                (KIND_LABEL, t.as_str())
        } else {
                (KIND_LABEL, "")
        };
        b.push(kind);
        let (nav, nav_page) = match (c.goto, c.back) {
                (Some(g), _) => (NAV_GOTO, g.min(u16::MAX as usize) as u16),
                (None, true) => (NAV_BACK, 0),
                (None, false) => (NAV_NONE, 0),
        };
        b.push(nav);
        b.extend_from_slice(&nav_page.to_le_bytes());
        b.extend_from_slice(&c.event.to_le_bytes());
        b.push(c.tag);
        b.extend_from_slice(&c.min_w.to_le_bytes());
        b.extend_from_slice(&c.min_h.to_le_bytes());
        b.extend_from_slice(&c.max_w.to_le_bytes());
        b.extend_from_slice(&c.max_h.to_le_bytes());
        b.push(c.grow as u8);
        if is_frame {
                if depth > 0 {
                        return Err(format!("frame nesting is one level deep; a frame with {} children nests further", c.children.len()));
                }
                b.push(layout_code(c.layout.as_deref().unwrap_or("stack")));
                b.push(c.gap.unwrap_or(6));
                b.push(scroll_code(c.scroll.as_deref()));
                if c.children.len() > u8::MAX as usize {
                        return Err("a frame has too many children for the format".to_owned());
                }
                b.push(c.children.len() as u8);
                for sub in &c.children {
                        put_child(b, sub, depth + 1)?;
                }
        } else {
                put_str(b, text)?;
        }
        Ok(())
}

/// Compile a design into an LUI blob.
pub fn compile(design: &Design) -> Result<Vec<u8>, String> {
        if design.pages.len() > u16::MAX as usize {
                return Err("too many pages for the format".to_owned());
        }

        //   each page's body is built first, so the offset table can point at them
        let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(design.pages.len());
        for page in &design.pages {
                let mut b = Vec::new();
                put_str(&mut b, &page.title)?;
                b.push(layout_code(&page.layout));
                b.push(page.gap);
                b.push(page.scroll as u8);
                b.push(page.subtitle as u8);
                if page.children.len() > u8::MAX as usize {
                        return Err(format!("page '{}' has too many children for the format", page.title));
                }
                b.push(page.children.len() as u8);
                for c in &page.children {
                        put_child(&mut b, c, 0)?;
                }
                bodies.push(b);
        }

        let table_len = 4 * design.pages.len();
        let mut blob = Vec::with_capacity(HEADER_LEN + table_len + bodies.iter().map(Vec::len).sum::<usize>());
        blob.extend_from_slice(MAGIC);
        blob.push(VERSION);
        blob.push(0); // reserved
        blob.extend_from_slice(&(design.pages.len() as u16).to_le_bytes());
        blob.extend_from_slice(&(design.root.min(u16::MAX as usize) as u16).to_le_bytes());
        blob.extend_from_slice(&design.device.width.to_le_bytes());
        blob.extend_from_slice(&design.device.height.to_le_bytes());
        blob.extend_from_slice(&design.device.corner_radius.to_le_bytes());

        let mut offset = (HEADER_LEN + table_len) as u32;
        for body in &bodies {
                blob.extend_from_slice(&offset.to_le_bytes());
                offset += body.len() as u32;
        }
        for body in &bodies {
                blob.extend_from_slice(body);
        }
        Ok(blob)
}

fn put_str(b: &mut Vec<u8>, s: &str) -> Result<(), String> {
        if s.len() > u8::MAX as usize {
                return Err(format!("string too long for the format: '{s}'"));
        }
        b.push(s.len() as u8);
        b.extend_from_slice(s.as_bytes());
        Ok(())
}

#[cfg(test)]
mod tests {
        use super::*;
        use crate::design;

        #[test]
        fn compiles_a_two_page_design() {
                let d = design::parse(
                        r#"{ "device": { "width": 172, "height": 640, "corner_radius": 8 }, "root": 0, "pages": [
                                { "title": "Main", "layout": "stack", "gap": 6, "subtitle": true, "children": [ { "button": "Go", "goto": 1, "event": 5, "tag": 9 }, { "label": "hi" } ] },
                                { "title": "Second", "scroll": true, "children": [ { "button": "Back", "back": true } ] }
                        ] }"#,
                )
                .unwrap();
                let blob = compile(&d).unwrap();

                assert_eq!(&blob[..4], MAGIC);
                assert_eq!(blob[4], VERSION, "schema version after the magic");
                assert_eq!(u16::from_le_bytes([blob[6], blob[7]]), 2, "two pages");
                assert_eq!(u16::from_le_bytes([blob[8], blob[9]]), 0, "root 0");
                assert_eq!(u16::from_le_bytes([blob[10], blob[11]]), 172, "device width");
                assert_eq!(u16::from_le_bytes([blob[12], blob[13]]), 640, "device height");
                assert_eq!(u16::from_le_bytes([blob[14], blob[15]]), 8, "device corner");

                // page 0 body: title "Main", stack, gap 6, scroll 0, subtitle 1, 2 children
                let off0 = u32::from_le_bytes([blob[16], blob[17], blob[18], blob[19]]) as usize;
                assert_eq!(blob[off0], 4, "title length 'Main'");
                assert_eq!(&blob[off0 + 1..off0 + 5], b"Main");
                assert_eq!(blob[off0 + 5], LAYOUT_STACK);
                assert_eq!(blob[off0 + 6], 6, "gap");
                assert_eq!(blob[off0 + 7], 0, "no scroll");
                assert_eq!(blob[off0 + 8], 1, "subtitle");
                assert_eq!(blob[off0 + 9], 2, "child count");
                // first child: button "Go" goto 1, event 5, tag 9; the common prefix is 16 bytes then text
                let c0 = off0 + 10;
                assert_eq!(blob[c0], KIND_BUTTON);
                assert_eq!(blob[c0 + 1], NAV_GOTO);
                assert_eq!(u16::from_le_bytes([blob[c0 + 2], blob[c0 + 3]]), 1, "goto page 1");
                assert_eq!(u16::from_le_bytes([blob[c0 + 4], blob[c0 + 5]]), 5, "event id");
                assert_eq!(blob[c0 + 6], 9, "tag");
                assert_eq!(blob[c0 + 16], 2, "text len 'Go'");
                assert_eq!(&blob[c0 + 17..c0 + 19], b"Go");
        }

        #[test]
        fn compiles_a_frame_with_leaf_children() {
                //   a page with one frame (a horizontal-scrolling linear strip) holding two rows
                let d = design::parse(
                        r#"{ "pages": [ { "title": "P", "children": [
                                { "layout": "linear", "gap": 4, "scroll": "horizontal", "grow": true, "max_w": 50,
                                  "children": [ { "button": "A", "event": 1 }, { "button": "B", "event": 2 } ] }
                        ] } ] }"#,
                )
                .unwrap();
                let blob = compile(&d).unwrap();
                let off = u32::from_le_bytes([blob[16], blob[17], blob[18], blob[19]]) as usize;
                // page body: title "P" (2), layout/gap/scroll/subtitle (4), child_count (1)
                let c = off + 2 + 4 + 1;
                assert_eq!(blob[c], KIND_FRAME);
                // common prefix: max_w at +11, grow at +15
                assert_eq!(u16::from_le_bytes([blob[c + 11], blob[c + 12]]), 50, "frame max_w");
                assert_eq!(blob[c + 15], 1, "frame grows");
                // frame body follows the 16-byte prefix: layout, gap, scroll, child_count
                assert_eq!(blob[c + 16], LAYOUT_LINEAR);
                assert_eq!(blob[c + 17], 4, "gap");
                assert_eq!(blob[c + 18], SCROLL_HORIZONTAL);
                assert_eq!(blob[c + 19], 2, "two leaf children");
                // first sub-child: button "A" event 1, its own 16-byte prefix then text
                let s0 = c + 20;
                assert_eq!(blob[s0], KIND_BUTTON);
                assert_eq!(u16::from_le_bytes([blob[s0 + 4], blob[s0 + 5]]), 1, "sub event");
                assert_eq!(blob[s0 + 16], 1, "text len 'A'");
                assert_eq!(blob[s0 + 17], b'A');
        }

        #[test]
        fn rejects_a_frame_nested_in_a_frame() {
                let d = design::parse(
                        r#"{ "pages": [ { "title": "P", "children": [
                                { "children": [ { "children": [ { "button": "deep" } ] } ] }
                        ] } ] }"#,
                )
                .unwrap();
                assert!(compile(&d).is_err(), "nesting past one level is rejected");
        }

        #[test]
        fn rejects_an_overlong_string() {
                let long = "x".repeat(300);
                let d = design::parse(&format!(r#"{{ "pages": [ {{ "title": "{long}", "children": [] }} ] }}"#)).unwrap();
                assert!(compile(&d).is_err());
        }
}
