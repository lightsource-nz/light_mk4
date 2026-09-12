//! The preview font.
//!
//! light-ui renders text from an LGF bitmap-font blob (see `light-font`); on device that blob is
//! produced at build time by crush from a TTF. The editor does the same rasterisation at runtime
//! instead -- FreeType into a fixed cell, then `light_font::Encoder` -- so it needs no committed
//! binary and, later, can load any font a design targets. This first cut bundles one face and one
//! size; it mirrors crush's render (`tools/crush/src/render.rs`), trimmed to the LGF path with
//! square pixels.

use std::rc::Rc;

use freetype::face::LoadFlag;
use light_font::{Encoder, Font};

/// The bundled preview face: the same TTF the firmware fonts are rendered from.
const TTF: &[u8] = include_bytes!("../../crush/tests/resources/fonts/TypeLightSans.ttf");

/// The characters a render covers -- crush's set, so the preview has the same glyph coverage as
/// the device.
const CHAR_SET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz`1234567890-=~!@#$%^&*()_+[]\\{}|;':\",./<>?";

/// Rasterise the bundled face at `pixel_size` and return a parsed [`Font`]. The LGF blob is leaked
/// to `'static` -- one font for the program's life, like the framebuffer -- so the `Font` borrows
/// it without a lifetime to thread.
pub fn load(pixel_size: u16) -> Font<'static> {
        let blob: &'static [u8] = Vec::leak(rasterise(pixel_size));
        Font::parse(blob).expect("the rasterised LGF parses")
}

fn rasterise(pixel_size: u16) -> Vec<u8> {
        let lib = freetype::Library::init().expect("FreeType init");
        //   from memory: the Face keeps the buffer alive through the Rc
        let face = lib.new_memory_face(Rc::new(TTF.to_vec()), 0).expect("load the bundled face");
        //   width 0 -> square pixels (the desktop's aspect); vertical size as asked
        face.set_pixel_sizes(0, u32::from(pixel_size)).expect("set the pixel size");
        let m = face.size_metrics().expect("the face reports size metrics");
        //   the cell comes from the font's nominal metrics, not from scanning glyphs (crush's
        // hard-won rule); 26.6 fixed point, so >> 6 is whole pixels
        let cell_w = (m.max_advance >> 6) as u8;
        let cell_h = (m.height >> 6) as u8;
        let ascent = (m.ascender >> 6) as u8;
        let ppem = m.y_ppem;
        let mut enc = Encoder::new(cell_w, cell_h, ascent, ppem);
        for c in CHAR_SET.bytes() {
                face.load_char(c as usize, LoadFlag::RENDER | LoadFlag::MONOCHROME).expect("load a glyph");
                let slot = face.glyph();
                let bm = slot.bitmap();
                let rows = copy_bitmap(bm.buffer(), bm.pitch(), bm.width() as u32, bm.rows() as u32, slot.bitmap_left(), slot.bitmap_top(), cell_w, cell_h, ascent);
                enc.add(c, &rows).expect("encode a glyph");
        }
        enc.encode()
}

/// Copy a rendered glyph into a zeroed cell, MSB-first packed, placed by its bearings relative to
/// the baseline row `ascent`, clipping pixel by pixel -- a glyph's bitmap can exceed the cell while
/// its ink still lands inside it.
#[allow(clippy::too_many_arguments)]
fn copy_bitmap(buffer: &[u8], src_pitch: i32, width: u32, rows: u32, left: i32, top: i32, cell_w: u8, cell_h: u8, ascent: u8) -> Vec<u8> {
        let dest_pitch = (cell_w as usize).div_ceil(8);
        let mut out = vec![0u8; dest_pitch * cell_h as usize];
        let origin_x = left;
        let origin_y = i32::from(ascent) - top;
        let abs_pitch = src_pitch.unsigned_abs() as usize;
        for y in 0..rows {
                let dest_y = origin_y + y as i32;
                if dest_y < 0 || dest_y >= i32::from(cell_h) {
                        continue;
                }
                for x in 0..width {
                        let dest_x = origin_x + x as i32;
                        if dest_x < 0 || dest_x >= i32::from(cell_w) {
                                continue;
                        }
                        let byte = buffer[y as usize * abs_pitch + x as usize / 8];
                        if byte >> (7 - x % 8) & 1 != 0 {
                                out[dest_y as usize * dest_pitch + dest_x as usize / 8] |= 1 << (7 - dest_x % 8);
                        }
                }
        }
        out
}
