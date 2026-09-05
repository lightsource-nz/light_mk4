//! Themes as data: a look-and-feel is a binary blob rolled into the firmware image, the
//! same shape as the font pipeline -- authored as JSON beside the application, compiled
//! by crush (`crush theme compile`) into an LTH blob at build time, embedded with
//! `include_bytes!(env!(...))`, and parsed here at boot. The point of the arrangement:
//! a new theme, or a themed variant of an application, is a DATA change -- no edit to
//! this crate, ever, is needed to restyle an interface.
//!
//! The blob is a tagged list -- magic, an entry count, then `key, length, payload`
//! triples -- and the parser SKIPS entries it does not know, which is the whole
//! forward-compatibility story: a theme compiled by a newer crush still styles an older
//! firmware with the subset both sides understand, and an old blob leaves the newer
//! defaults standing. Every field starts from [`Theme::DEFAULT`], which reproduces the
//! monochrome white-on-black look the demos had before themes existed.

use crate::Shade;

/// The blob's magic: "LTH1", Light THeme, format 1.
pub const MAGIC: [u8; 4] = *b"LTH1";

/// Entry keys. u16, with the payload length carried beside them so unknown keys skip.
pub mod key {
        /// The background everything paints over (u16 RGB565).
        pub const BG: u16 = 0x0001;
        /// Window borders, separators and the scroll mask's boundary (u16).
        pub const FRAME: u16 = 0x0002;
        /// Window title text (u16).
        pub const TITLE: u16 = 0x0003;
        /// Label text (u16).
        pub const TEXT: u16 = 0x0004;
        /// Button outlines, and the focused fill when no focus surface is set (u16).
        pub const BUTTON_OUTLINE: u16 = 0x0005;
        /// Button label text, unfocused (u16).
        pub const BUTTON_TEXT: u16 = 0x0006;
        /// Button label text on the focused fill (u16).
        pub const FOCUS_TEXT: u16 = 0x0007;
        /// The focused widget's fill, a vertical shade (u16 from, u16 to).
        pub const FOCUS_SURFACE: u16 = 0x0010;
        /// Every button's unfocused surface, a vertical shade (u16 from, u16 to); a
        /// widget's own [`crate::Desc::shaded`] overrides it.
        pub const BUTTON_SURFACE: u16 = 0x0011;
}

/// A complete look-and-feel: what every element paints with. Colors are RGB565; on a
/// mono canvas nonzero renders as set, which is why the default reads correctly there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
        pub bg: u16,
        pub frame: u16,
        pub title: u16,
        pub text: u16,
        pub button_outline: u16,
        pub button_text: u16,
        pub focus_text: u16,
        pub focus_surface: Option<Shade>,
        pub button_surface: Option<Shade>,
}

impl Theme {
        /// The pre-theme look, exactly: white on black, solid inverted focus.
        pub const DEFAULT: Theme = Theme {
                bg: 0x0000,
                frame: 0xFFFF,
                title: 0xFFFF,
                text: 0xFFFF,
                button_outline: 0xFFFF,
                button_text: 0xFFFF,
                focus_text: 0x0000,
                focus_surface: None,
                button_surface: None,
        };

        /// Parse an LTH blob. Unknown keys are skipped; known keys with the wrong length
        /// are an error (a corrupt blob, not a future format).
        pub fn parse(blob: &[u8]) -> Result<Theme, ThemeError> {
                if blob.len() < 6 || blob[..4] != MAGIC {
                        return Err(ThemeError::BadMagic);
                }
                let count = u16::from_le_bytes([blob[4], blob[5]]);
                let mut at = 6usize;
                let mut theme = Theme::DEFAULT;
                let u16le = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
                for _ in 0..count {
                        if at + 4 > blob.len() {
                                return Err(ThemeError::Truncated);
                        }
                        let k = u16le(blob, at);
                        let len = usize::from(u16le(blob, at + 2));
                        at += 4;
                        if at + len > blob.len() {
                                return Err(ThemeError::Truncated);
                        }
                        let payload = &blob[at..at + len];
                        at += len;
                        let color = |field: &mut u16| -> Result<(), ThemeError> {
                                if payload.len() != 2 {
                                        return Err(ThemeError::BadEntry(k));
                                }
                                *field = u16le(payload, 0);
                                Ok(())
                        };
                        let shade = |field: &mut Option<Shade>| -> Result<(), ThemeError> {
                                if payload.len() != 4 {
                                        return Err(ThemeError::BadEntry(k));
                                }
                                *field = Some(Shade { from: u16le(payload, 0), to: u16le(payload, 2) });
                                Ok(())
                        };
                        match k {
                                key::BG => color(&mut theme.bg)?,
                                key::FRAME => color(&mut theme.frame)?,
                                key::TITLE => color(&mut theme.title)?,
                                key::TEXT => color(&mut theme.text)?,
                                key::BUTTON_OUTLINE => color(&mut theme.button_outline)?,
                                key::BUTTON_TEXT => color(&mut theme.button_text)?,
                                key::FOCUS_TEXT => color(&mut theme.focus_text)?,
                                key::FOCUS_SURFACE => shade(&mut theme.focus_surface)?,
                                key::BUTTON_SURFACE => shade(&mut theme.button_surface)?,
                                //   a key from a future format: skipped, styled by default
                                _ => {}
                        }
                }
                Ok(theme)
        }
}

impl Default for Theme {
        fn default() -> Self {
                Theme::DEFAULT
        }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeError {
        /// Not an LTH blob at all.
        BadMagic,
        /// An entry runs past the end of the blob.
        Truncated,
        /// A KNOWN key with the wrong payload length: corruption, not a future format.
        BadEntry(u16),
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::vec::Vec;

        fn blob(entries: &[(u16, &[u8])]) -> Vec<u8> {
                let mut b = Vec::new();
                b.extend_from_slice(&MAGIC);
                b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
                for (k, payload) in entries {
                        b.extend_from_slice(&k.to_le_bytes());
                        b.extend_from_slice(&(payload.len() as u16).to_le_bytes());
                        b.extend_from_slice(payload);
                }
                b
        }

        #[test]
        fn defaults_are_the_pre_theme_look() {
                let t = Theme::parse(&blob(&[])).unwrap();
                assert_eq!(t, Theme::DEFAULT);
                assert_eq!(t.bg, 0x0000);
                assert_eq!(t.frame, 0xFFFF);
                assert_eq!(t.focus_surface, None);
        }

        #[test]
        fn entries_apply_over_the_defaults() {
                let t = Theme::parse(&blob(&[
                        (key::BG, &0x1082u16.to_le_bytes()),
                        (key::FOCUS_SURFACE, &[0x5D, 0x4C, 0x0E, 0x09]),
                ]))
                .unwrap();
                assert_eq!(t.bg, 0x1082);
                assert_eq!(t.focus_surface, Some(Shade { from: 0x4C5D, to: 0x090E }));
                assert_eq!(t.frame, 0xFFFF, "untouched fields keep the default");
        }

        #[test]
        fn unknown_keys_are_skipped_not_errors() {
                //   the forward-compatibility contract: a future crush may emit keys this
                // firmware has never heard of, and the theme still applies
                let t = Theme::parse(&blob(&[
                        (0x7F01, &[1, 2, 3, 4, 5]),
                        (key::TEXT, &0x07E0u16.to_le_bytes()),
                ]))
                .unwrap();
                assert_eq!(t.text, 0x07E0);
        }

        #[test]
        fn corruption_is_an_error_not_a_guess() {
                assert_eq!(Theme::parse(b"LTHX\x00\x00"), Err(ThemeError::BadMagic));
                assert_eq!(Theme::parse(&MAGIC[..3]), Err(ThemeError::BadMagic));
                //   a known key with a wrong length is corruption
                assert_eq!(Theme::parse(&blob(&[(key::BG, &[1, 2, 3])])), Err(ThemeError::BadEntry(key::BG)));
                //   an entry running past the end
                let mut b = blob(&[]);
                b[4] = 1;
                assert_eq!(Theme::parse(&b), Err(ThemeError::Truncated));
        }
}
