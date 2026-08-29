//! Drawing into a frame buffer: the first pieces of what will become `light_draw`.
//!
//! Only what the spike's demo needs -- rectangles and text from an LGF font into an RGB565
//! buffer -- with the coordinate handling that mk3's rasteriser learned to get right: every
//! primitive clips to the buffer, and text reports the region it touched so the caller can push
//! exactly that.

use light_font::Font;

use crate::display::Region;

/// An RGB565, row-major frame buffer of known width and height. Big-endian pixel bytes, which is
/// what the ST7789 streams natively.
pub struct Rgb565<'a> {
        pub buf: &'a mut [u8],
        pub width: u16,
        pub height: u16,
}

impl Rgb565<'_> {
        #[inline]
        pub fn set(&mut self, x: u16, y: u16, color: u16) {
                if x >= self.width || y >= self.height {
                        return;
                }
                let i = (y as usize * self.width as usize + x as usize) * 2;
                self.buf[i] = (color >> 8) as u8;
                self.buf[i + 1] = color as u8;
        }

        /// Fills an inclusive region, clipped to the buffer.
        pub fn fill(&mut self, r: &Region, color: u16) {
                if r.x0 >= self.width || r.y0 >= self.height {
                        return;
                }
                let r = r.clamped(self.width, self.height);
                let hi = (color >> 8) as u8;
                let lo = color as u8;
                for y in r.y0..=r.y1 {
                        let start = (y as usize * self.width as usize + r.x0 as usize) * 2;
                        let end = start + r.width() as usize * 2;
                        for px in self.buf[start..end].chunks_exact_mut(2) {
                                px[0] = hi;
                                px[1] = lo;
                        }
                }
        }

        /// Draws `text` with its cell's top-left at (x, y), painting every cell pixel -- ink in
        /// `fg`, the rest in `bg` -- so the text box needs no separate clear. Characters the
        /// font lacks paint as blank cells. Returns the region touched, or `None` when nothing
        /// landed on the buffer.
        pub fn text(&mut self, font: &Font<'_>, x: u16, y: u16, text: &str, fg: u16, bg: u16) -> Option<Region> {
                let (cw, ch) = (u16::from(font.cell_width()), u16::from(font.cell_height()));
                let mut pen = x;
                let mut touched: Option<Region> = None;
                for c in text.bytes() {
                        if pen >= self.width || y >= self.height {
                                break;
                        }
                        let cell = Region::new(pen, y, pen.saturating_add(cw - 1), y.saturating_add(ch - 1)).clamped(self.width, self.height);
                        for dy in 0..ch {
                                for dx in 0..cw {
                                        let on = font.pixel(c, dx as u8, dy as u8);
                                        self.set(pen + dx, y + dy, if on { fg } else { bg });
                                }
                        }
                        touched = Some(match touched {
                                Some(t) => t.union(&cell),
                                None => cell,
                        });
                        pen = pen.saturating_add(cw);
                }
                touched
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;

        fn font() -> std::vec::Vec<u8> {
                // 8x4 cells; 'I' is a vertical bar in column 3, 'X' a full first row
                let mut e = light_font::Encoder::new(8, 4, 3, 4);
                e.add(b'I', &[0x10, 0x10, 0x10, 0x10]).unwrap();
                e.add(b'X', &[0xFF, 0x00, 0x00, 0x00]).unwrap();
                e.encode()
        }

        fn px(buf: &[u8], width: u16, x: u16, y: u16) -> u16 {
                let i = (y as usize * width as usize + x as usize) * 2;
                u16::from_be_bytes([buf[i], buf[i + 1]])
        }

        #[test]
        fn text_paints_ink_and_background_and_reports_its_box() {
                let blob = font();
                let f = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 32 * 8 * 2];
                let mut fb = Rgb565 { buf: &mut buf, width: 32, height: 8 };
                let r = fb.text(&f, 2, 1, "IX", 0xF800, 0x001F).unwrap();
                assert_eq!(r, Region::new(2, 1, 17, 4));
                assert_eq!(px(fb.buf, 32, 5, 1), 0xF800, "I's bar at column 3 of its cell");
                assert_eq!(px(fb.buf, 32, 4, 1), 0x001F, "background painted inside the cell");
                assert_eq!(px(fb.buf, 32, 10, 1), 0xF800, "X's top row");
                assert_eq!(px(fb.buf, 32, 10, 2), 0x001F);
                assert_eq!(px(fb.buf, 32, 1, 1), 0x0000, "outside the box untouched");
        }

        #[test]
        fn text_clips_at_the_right_edge_and_missing_glyphs_are_blank() {
                let blob = font();
                let f = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 12 * 4 * 2];
                let mut fb = Rgb565 { buf: &mut buf, width: 12, height: 4 };
                let r = fb.text(&f, 8, 0, "?XX", 0xFFFF, 0x0000).unwrap();
                assert_eq!(r, Region::new(8, 0, 11, 3), "only the first cell fits, clipped");
                assert!((8..12).all(|x| px(fb.buf, 12, x, 0) == 0x0000), "'?' has no glyph: blank");
        }

        #[test]
        fn fill_clips_and_ignores_off_buffer_regions() {
                let mut buf = [0u8; 4 * 4 * 2];
                let mut fb = Rgb565 { buf: &mut buf, width: 4, height: 4 };
                fb.fill(&Region::new(2, 2, 9, 9), 0xABCD);
                assert_eq!(px(fb.buf, 4, 3, 3), 0xABCD);
                assert_eq!(px(fb.buf, 4, 1, 1), 0);
                fb.fill(&Region::new(7, 7, 9, 9), 0x1111);
                assert!(fb.buf.iter().all(|&b| b != 0x11));
        }
}
