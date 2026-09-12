//! Rasterising a TrueType face into an LGF bitmap font -- the pure core of crush's render, with no
//! file I/O and no C output. Both crush (which wraps this with the `.lgf`/`.c` writers) and the host
//! editor (which rasterises its preview font) call it.
//!
//! Ported from the C implementation's `crush_render_backend`, including the parts it learned the
//! hard way: the cell comes from the font's nominal metrics rather than from scanning glyphs; an
//! explicit pixel size is the VERTICAL size and the horizontal one is derived through the display's
//! pixel aspect (FreeType's width=0 shorthand silently assumes square pixels); and glyph bitmaps are
//! placed by their bearings relative to the shared baseline, clipping pixel by pixel.

use std::rc::Rc;

use freetype::face::LoadFlag;

/// The characters a render covers: the C implementation's set, unchanged.
pub const CHAR_SET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz`1234567890-=~!@#$%^&*()_+[]\\{}|;':\",./<>?";

/// The result of a rasterisation: the cell metrics, the per-glyph packed bitmaps (for a C
/// emitter), and the LGF blob.
pub struct Rasterized {
        pub pixel_size: u16,
        pub cell_width: u8,
        pub cell_height: u8,
        /// `(char, packed 1-bpp rows)` for each glyph, in [`CHAR_SET`] order.
        pub glyphs: Vec<(u8, Vec<u8>)>,
        /// The encoded LGF blob.
        pub lgf: Vec<u8>,
}

/// Rasterise the TrueType face in `ttf` (face `face_index`). `pixel_size` is the vertical size in
/// pixels; if 0, `point_size` and the `ppi_h`/`ppi_v` densities are used instead. The horizontal
/// pixel size follows the `ppi_h`/`ppi_v` aspect so glyphs are the right shape on non-square pixels.
/// `mono` renders 1 bpp (what LGF stores); it must be set for the packed bitmaps to be read right.
pub fn rasterize(ttf: &[u8], face_index: u32, pixel_size: u16, point_size: f64, ppi_h: f64, ppi_v: f64, mono: bool) -> Result<Rasterized, String> {
        let lib = freetype::Library::init().map_err(|e| format!("FreeType init failed: {e}"))?;
        //   from memory: the Face keeps the buffer alive through the Rc
        let face = lib.new_memory_face(Rc::new(ttf.to_vec()), face_index as isize).map_err(|e| format!("FT_New_Memory_Face() failed: {e}"))?;

        if pixel_size > 0 {
                // vertical size as given; horizontal through the pixel aspect
                let pixel_width = (f64::from(pixel_size) * (ppi_h / ppi_v) + 0.5) as u32;
                face.set_pixel_sizes(pixel_width, u32::from(pixel_size)).map_err(|e| format!("FT_Set_Pixel_Sizes() failed: {e}"))?;
        } else {
                // 26.6 fixed point: 1/64 of a point
                let size = (point_size * 64.0).round() as isize;
                face.set_char_size(0, size, ppi_h.round() as u32, ppi_v.round() as u32).map_err(|e| format!("FT_Set_Char_Size() failed: {e}"))?;
        }
        let metrics = face.size_metrics().ok_or("the face reports no size metrics")?;
        //   what FreeType actually settled on, whichever call set it
        let pixel_size = metrics.y_ppem;
        let cell_width = (metrics.max_advance >> 6) as u8;
        let cell_height = (metrics.height >> 6) as u8;
        let cell_ascent = (metrics.ascender >> 6) as u8;
        if cell_width == 0 || cell_height == 0 {
                return Err(format!("degenerate cell {cell_width}x{cell_height} -- check the display's pixel density"));
        }

        let mut flags = LoadFlag::RENDER;
        if mono {
                flags |= LoadFlag::MONOCHROME;
        }

        let mut encoder = light_font::Encoder::new(cell_width, cell_height, cell_ascent, pixel_size);
        let mut glyphs: Vec<(u8, Vec<u8>)> = Vec::with_capacity(CHAR_SET.len());
        for c in CHAR_SET.bytes() {
                face.load_char(c as usize, flags).map_err(|e| format!("FT_Load_Char() failed for {:?}: {e}", c as char))?;
                let slot = face.glyph();
                let bitmap = slot.bitmap();
                let rows = copy_bitmap(
                        bitmap.buffer(),
                        bitmap.pitch(),
                        bitmap.width() as u32,
                        bitmap.rows() as u32,
                        slot.bitmap_left(),
                        slot.bitmap_top(),
                        cell_width,
                        cell_height,
                        cell_ascent,
                );
                encoder.add(c, &rows).map_err(|e| format!("{e:?}"))?;
                glyphs.push((c, rows));
        }

        Ok(Rasterized { pixel_size, cell_width, cell_height, lgf: encoder.encode(), glyphs })
}

/// Copies a rendered glyph into a zeroed cell, MSB-first packed, placed by its bearings relative to
/// the baseline row `cell_ascent`, clipping pixel by pixel -- a glyph's bitmap can exceed the cell
/// while its ink still lands inside it.
#[allow(clippy::too_many_arguments)]
fn copy_bitmap(buffer: &[u8], src_pitch: i32, width: u32, rows: u32, left: i32, top: i32, cell_width: u8, cell_height: u8, cell_ascent: u8) -> Vec<u8> {
        let dest_pitch = (cell_width as usize).div_ceil(8);
        let mut out = vec![0u8; dest_pitch * cell_height as usize];
        let origin_x = left;
        let origin_y = i32::from(cell_ascent) - top;
        let abs_pitch = src_pitch.unsigned_abs() as usize;
        for y in 0..rows {
                let dest_y = origin_y + y as i32;
                if dest_y < 0 || dest_y >= i32::from(cell_height) {
                        continue;
                }
                for x in 0..width {
                        let dest_x = origin_x + x as i32;
                        if dest_x < 0 || dest_x >= i32::from(cell_width) {
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
