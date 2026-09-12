//! The LUI binary UI format: a design compiled to a flat, zero-copy blob that light-ui reads and
//! displays. UI as data, the same arrangement as fonts (LGF) and themes (LTH) -- authored, compiled
//! by crush, embedded or loaded, parsed at runtime. The format is owned here and by light-ui's
//! reader; the two must agree (the codes below are the contract).
//!
//! Layout, little-endian:
//! - Header (16 bytes): magic "LUI1", `page_count` u16, `root` u16, device `width`/`height`/
//!   `corner_radius` u16 each, then u16 reserved.
//! - Page-offset table: `page_count` * u32, each the byte offset of a page from the blob start.
//! - Pages: each is `title` (u8 len + bytes), `layout` u8, `gap` u8, `child_count` u8, then each
//!   child: `kind` u8, `nav` u8, `nav_page` u16, `text` (u8 len + bytes).
//!
//! Strings are inline and length-prefixed, so a reader returns `&str` views into the blob with no
//! copy -- the LGF pattern.

use crate::design::Design;

/// The blob magic: "LUI1", Light UI, format 1.
pub const MAGIC: &[u8; 4] = b"LUI1";
/// The fixed header length.
pub const HEADER_LEN: usize = 16;

// Layout codes (a window's child arrangement).
pub const LAYOUT_STACK: u8 = 0;
pub const LAYOUT_ROW: u8 = 1;
pub const LAYOUT_LINEAR: u8 = 2;

// Child kinds.
pub const KIND_BUTTON: u8 = 0;
pub const KIND_LABEL: u8 = 1;

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
                if page.children.len() > u8::MAX as usize {
                        return Err(format!("page '{}' has too many children for the format", page.title));
                }
                b.push(page.children.len() as u8);
                for c in &page.children {
                        let (kind, text) = if let Some(t) = &c.button {
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
                        put_str(&mut b, text)?;
                }
                bodies.push(b);
        }

        let table_len = 4 * design.pages.len();
        let mut blob = Vec::with_capacity(HEADER_LEN + table_len + bodies.iter().map(Vec::len).sum::<usize>());
        blob.extend_from_slice(MAGIC);
        blob.extend_from_slice(&(design.pages.len() as u16).to_le_bytes());
        blob.extend_from_slice(&(design.root.min(u16::MAX as usize) as u16).to_le_bytes());
        blob.extend_from_slice(&design.device.width.to_le_bytes());
        blob.extend_from_slice(&design.device.height.to_le_bytes());
        blob.extend_from_slice(&design.device.corner_radius.to_le_bytes());
        blob.extend_from_slice(&0u16.to_le_bytes()); // reserved

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
                                { "title": "Main", "layout": "stack", "gap": 6, "children": [ { "button": "Go", "goto": 1 }, { "label": "hi" } ] },
                                { "title": "Second", "children": [ { "button": "Back", "back": true } ] }
                        ] }"#,
                )
                .unwrap();
                let blob = compile(&d).unwrap();

                assert_eq!(&blob[..4], MAGIC);
                assert_eq!(u16::from_le_bytes([blob[4], blob[5]]), 2, "two pages");
                assert_eq!(u16::from_le_bytes([blob[6], blob[7]]), 0, "root 0");
                assert_eq!(u16::from_le_bytes([blob[8], blob[9]]), 172, "device width");
                assert_eq!(u16::from_le_bytes([blob[10], blob[11]]), 640, "device height");
                assert_eq!(u16::from_le_bytes([blob[12], blob[13]]), 8, "device corner");

                // page 0 body at its table offset: title "Main", stack, gap 6, 2 children
                let off0 = u32::from_le_bytes([blob[16], blob[17], blob[18], blob[19]]) as usize;
                assert_eq!(blob[off0], 4, "title length 'Main'");
                assert_eq!(&blob[off0 + 1..off0 + 5], b"Main");
                assert_eq!(blob[off0 + 5], LAYOUT_STACK);
                assert_eq!(blob[off0 + 6], 6, "gap");
                assert_eq!(blob[off0 + 7], 2, "child count");
                // first child: button "Go" goto 1
                let c0 = off0 + 8;
                assert_eq!(blob[c0], KIND_BUTTON);
                assert_eq!(blob[c0 + 1], NAV_GOTO);
                assert_eq!(u16::from_le_bytes([blob[c0 + 2], blob[c0 + 3]]), 1, "goto page 1");
                assert_eq!(blob[c0 + 4], 2, "text len 'Go'");
                assert_eq!(&blob[c0 + 5..c0 + 7], b"Go");
        }

        #[test]
        fn rejects_an_overlong_string() {
                let long = "x".repeat(300);
                let d = design::parse(&format!(r#"{{ "pages": [ {{ "title": "{long}", "children": [] }} ] }}"#)).unwrap();
                assert!(compile(&d).is_err());
        }
}
