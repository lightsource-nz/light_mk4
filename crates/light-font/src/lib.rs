//! LGF: the light glyph font format.
//!
//! A rendered bitmap font as DATA, embedded in firmware with `include_bytes!` and parsed in
//! place, instead of the generated C that mk3's crush emitted. That C hardcoded the field names
//! of `light_draw_font_t` as printf strings, made light_display build crush as an ExternalProject,
//! and produced files that did not exist until crush had run. A blob has a header, a version, and
//! a reader that says no to what it does not understand.
//!
//! Layout (all little-endian):
//!
//! ```text
//!  0  "LGF1"                magic
//!  4  u8   version          1
//!  5  u8   flags            bit 0: 1 bpp, MSB-first, row-major (the only encoding so far)
//!  6  u8   cell_width       every glyph is the same cell, in pixels
//!  7  u8   cell_height
//!  8  u8   ascent           the row the baseline sits on, from the top of the cell
//!  9  u8   pitch            bytes per row = (cell_width + 7) / 8
//! 10  u16  glyph_count      how many glyphs follow
//! 12  u16  pixel_size       the vertical pixel size the rasteriser resolved (y_ppem)
//! 14  u16  reserved         0
//! 16  [u8; 32] present      bit c (byte c/8, bit c%8) set when char code c has a glyph
//! 48  glyphs                glyph_count * pitch * cell_height bytes, ascending char code
//! ```
//!
//! A glyph's index is the number of present codes below it, so lookup is a popcount over at
//! most 32 bytes -- no table of offsets to keep in step with the data.

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

pub const MAGIC: [u8; 4] = *b"LGF1";
pub const VERSION: u8 = 1;
pub const FLAG_MONO_MSB: u8 = 0x01;
pub const HEADER_LEN: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
        TooShort,
        BadMagic,
        UnsupportedVersion(u8),
        UnsupportedFlags(u8),
        /// The pitch does not match the cell width, or the data does not match the count.
        Inconsistent,
}

/// A parsed font, borrowing the blob. Copy, so passing it around costs a pointer and a length.
#[derive(Clone, Copy)]
pub struct Font<'a> {
        blob: &'a [u8],
}

impl<'a> Font<'a> {
        pub fn parse(blob: &'a [u8]) -> Result<Self, Error> {
                if blob.len() < HEADER_LEN {
                        return Err(Error::TooShort);
                }
                if blob[0..4] != MAGIC {
                        return Err(Error::BadMagic);
                }
                if blob[4] != VERSION {
                        return Err(Error::UnsupportedVersion(blob[4]));
                }
                if blob[5] != FLAG_MONO_MSB {
                        return Err(Error::UnsupportedFlags(blob[5]));
                }
                let font = Self { blob };
                if font.pitch() as usize != (font.cell_width() as usize).div_ceil(8) {
                        return Err(Error::Inconsistent);
                }
                let present: u32 = font.present_map().iter().map(|b| b.count_ones()).sum();
                if present != u32::from(font.glyph_count()) {
                        return Err(Error::Inconsistent);
                }
                let need = HEADER_LEN + font.glyph_count() as usize * font.glyph_len();
                if blob.len() < need {
                        return Err(Error::TooShort);
                }
                Ok(font)
        }

        pub fn cell_width(&self) -> u8 {
                self.blob[6]
        }

        pub fn cell_height(&self) -> u8 {
                self.blob[7]
        }

        pub fn ascent(&self) -> u8 {
                self.blob[8]
        }

        pub fn pitch(&self) -> u8 {
                self.blob[9]
        }

        pub fn glyph_count(&self) -> u16 {
                u16::from_le_bytes([self.blob[10], self.blob[11]])
        }

        pub fn pixel_size(&self) -> u16 {
                u16::from_le_bytes([self.blob[12], self.blob[13]])
        }

        fn present_map(&self) -> &'a [u8] {
                &self.blob[16..48]
        }

        /// Bytes per glyph.
        pub fn glyph_len(&self) -> usize {
                self.pitch() as usize * self.cell_height() as usize
        }

        pub fn has(&self, c: u8) -> bool {
                self.present_map()[c as usize / 8] & (1 << (c % 8)) != 0
        }

        /// The packed rows of `c`'s glyph, or `None` when the font has no glyph for it.
        pub fn glyph(&self, c: u8) -> Option<&'a [u8]> {
                if !self.has(c) {
                        return None;
                }
                let map = self.present_map();
                let full_bytes = c as usize / 8;
                let mut index: usize = map[..full_bytes].iter().map(|b| b.count_ones() as usize).sum();
                let partial = map[full_bytes] & ((1u16 << (c % 8)) - 1) as u8;
                index += partial.count_ones() as usize;
                let len = self.glyph_len();
                let start = HEADER_LEN + index * len;
                Some(&self.blob[start..start + len])
        }

        /// One pixel of `c`'s glyph; false outside the cell or for a missing glyph.
        pub fn pixel(&self, c: u8, x: u8, y: u8) -> bool {
                if x >= self.cell_width() || y >= self.cell_height() {
                        return false;
                }
                match self.glyph(c) {
                        Some(g) => g[y as usize * self.pitch() as usize + x as usize / 8] >> (7 - x % 8) & 1 != 0,
                        None => false,
                }
        }

        /// Every char code that has a glyph, ascending.
        pub fn chars(&self) -> impl Iterator<Item = u8> + 'a {
                let map = self.present_map();
                (0..=255u8).filter(move |&c| map[c as usize / 8] & (1 << (c % 8)) != 0)
        }
}

/// Builds a blob. Glyphs may be added in any order; the encoder sorts them.
#[cfg(feature = "alloc")]
pub struct Encoder {
        cell_width: u8,
        cell_height: u8,
        ascent: u8,
        pixel_size: u16,
        glyphs: alloc::collections::BTreeMap<u8, alloc::vec::Vec<u8>>,
}

#[cfg(feature = "alloc")]
impl Encoder {
        pub fn new(cell_width: u8, cell_height: u8, ascent: u8, pixel_size: u16) -> Self {
                Self { cell_width, cell_height, ascent, pixel_size, glyphs: Default::default() }
        }

        pub fn pitch(&self) -> usize {
                (self.cell_width as usize).div_ceil(8)
        }

        /// `rows` must be exactly `pitch * cell_height` bytes, MSB-first packed.
        pub fn add(&mut self, c: u8, rows: &[u8]) -> Result<(), Error> {
                if rows.len() != self.pitch() * self.cell_height as usize {
                        return Err(Error::Inconsistent);
                }
                self.glyphs.insert(c, rows.to_vec());
                Ok(())
        }

        pub fn encode(&self) -> alloc::vec::Vec<u8> {
                let mut out = alloc::vec::Vec::with_capacity(HEADER_LEN + self.glyphs.len() * self.pitch() * self.cell_height as usize);
                out.extend_from_slice(&MAGIC);
                out.push(VERSION);
                out.push(FLAG_MONO_MSB);
                out.push(self.cell_width);
                out.push(self.cell_height);
                out.push(self.ascent);
                out.push(self.pitch() as u8);
                out.extend_from_slice(&(self.glyphs.len() as u16).to_le_bytes());
                out.extend_from_slice(&self.pixel_size.to_le_bytes());
                out.extend_from_slice(&0u16.to_le_bytes());
                let mut map = [0u8; 32];
                for &c in self.glyphs.keys() {
                        map[c as usize / 8] |= 1 << (c % 8);
                }
                out.extend_from_slice(&map);
                for rows in self.glyphs.values() {
                        out.extend_from_slice(rows);
                }
                out
        }
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
        use super::*;
        extern crate std;

        fn sample() -> alloc::vec::Vec<u8> {
                // 10x3 cells: pitch 2
                let mut e = Encoder::new(10, 3, 2, 12);
                e.add(b'b', &[0x80, 0x00, 0x40, 0x00, 0x20, 0x00]).unwrap();
                e.add(b'A', &[0xFF, 0xC0, 0x00, 0x00, 0x01, 0x40]).unwrap();
                e.add(b'a', &[0x00, 0x00, 0xFF, 0xC0, 0x00, 0x00]).unwrap();
                e.encode()
        }

        #[test]
        fn round_trips_and_looks_up_by_popcount() {
                let blob = sample();
                let f = Font::parse(&blob).unwrap();
                assert_eq!((f.cell_width(), f.cell_height(), f.ascent(), f.pitch()), (10, 3, 2, 2));
                assert_eq!(f.glyph_count(), 3);
                assert_eq!(f.pixel_size(), 12);
                assert_eq!(f.chars().collect::<alloc::vec::Vec<_>>(), [b'A', b'a', b'b']);
                assert_eq!(f.glyph(b'A').unwrap(), &[0xFF, 0xC0, 0x00, 0x00, 0x01, 0x40]);
                assert_eq!(f.glyph(b'a').unwrap(), &[0x00, 0x00, 0xFF, 0xC0, 0x00, 0x00]);
                assert_eq!(f.glyph(b'b').unwrap(), &[0x80, 0x00, 0x40, 0x00, 0x20, 0x00]);
                assert!(f.glyph(b'c').is_none());
                assert!(f.pixel(b'A', 0, 0));
                assert!(f.pixel(b'A', 9, 0), "10th pixel is bit 6 of the second byte");
                assert!(!f.pixel(b'A', 0, 1));
                // row 2 is 0x01 0x40: bit 0 of byte 0 is x=7, bit 6 of byte 1 is x=9
                assert!(f.pixel(b'A', 7, 2));
                assert!(!f.pixel(b'A', 8, 2));
                assert!(f.pixel(b'A', 9, 2));
                assert!(!f.pixel(b'A', 10, 0), "outside the cell");
                assert!(!f.pixel(b'z', 0, 0), "missing glyph");
        }

        #[test]
        fn rejects_what_it_does_not_understand() {
                let blob = sample();
                assert_eq!(Font::parse(&blob[..10]).map(|_| ()), Err(Error::TooShort));
                let mut bad = blob.clone();
                bad[0] = b'X';
                assert_eq!(Font::parse(&bad).map(|_| ()), Err(Error::BadMagic));
                let mut v2 = blob.clone();
                v2[4] = 2;
                assert_eq!(Font::parse(&v2).map(|_| ()), Err(Error::UnsupportedVersion(2)));
                let mut short = blob.clone();
                short.truncate(short.len() - 1);
                assert_eq!(Font::parse(&short).map(|_| ()), Err(Error::TooShort));
                let mut wrong_pitch = blob.clone();
                wrong_pitch[9] = 3;
                assert_eq!(Font::parse(&wrong_pitch).map(|_| ()), Err(Error::Inconsistent));
        }

        #[test]
        fn encoder_refuses_a_glyph_of_the_wrong_size() {
                let mut e = Encoder::new(8, 2, 1, 8);
                assert_eq!(e.add(b'x', &[0, 0, 0]), Err(Error::Inconsistent));
                assert_eq!(e.add(b'x', &[0, 0]), Ok(()));
        }
}
