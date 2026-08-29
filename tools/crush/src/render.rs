//! The render job: FreeType rasterises each character of the set into a fixed cell, and the
//! result is written as an LGF blob and, for mk3's consumers, the same C pair mk3's crush wrote.
//!
//! Ported from mk3's `crush_render_backend`, including the parts it learned the hard way: the
//! cell comes from the font's nominal metrics rather than from scanning glyphs; an explicit pixel
//! size is the VERTICAL size and the horizontal one is derived through the display's pixel aspect
//! (FreeType's width=0 shorthand silently assumes square pixels); and glyph bitmaps are placed by
//! their bearings relative to the shared baseline, clipping pixel by pixel, since a glyph's
//! bitmap can be larger than the cell while its ink still lands inside it.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use freetype::face::LoadFlag;

use crate::context::Display;

/// The characters every render covers: mk3's set, unchanged.
pub const CHAR_SET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz`1234567890-=~!@#$%^&*()_+[]\\{}|;':\",./<>?";

pub struct Job {
        pub font_file: PathBuf,
        pub font_name: String,
        pub face_index: u32,
        pub display: Display,
        pub point_size: f64,
        pub pixel_size: u16,
        pub out_dir: PathBuf,
}

pub struct Outcome {
        pub pixel_size: u16,
        pub cell_width: u8,
        pub cell_height: u8,
        pub lgf_path: PathBuf,
}

pub fn run(job: &Job) -> Result<Outcome, String> {
        let lib = freetype::Library::init().map_err(|e| format!("FreeType init failed: {e}"))?;
        let face = lib
                .new_face(&job.font_file, job.face_index as isize)
                .map_err(|e| format!("FT_New_Face() failed for '{}': {e}", job.font_file.display()))?;

        if job.pixel_size > 0 {
                // vertical size as given; horizontal through the pixel aspect, so glyphs come
                // out the right shape on a display whose pixels are not square
                let pixel_width = (f64::from(job.pixel_size) * (job.display.ppi_h / job.display.ppi_v) + 0.5) as u32;
                face.set_pixel_sizes(pixel_width, u32::from(job.pixel_size)).map_err(|e| format!("FT_Set_Pixel_Sizes() failed: {e}"))?;
        } else {
                // 26.6 fixed point: 1/64 of a point
                let size = (job.point_size * 64.0).round() as isize;
                face.set_char_size(0, size, job.display.ppi_h.round() as u32, job.display.ppi_v.round() as u32)
                        .map_err(|e| format!("FT_Set_Char_Size() failed: {e}"))?;
        }
        let metrics = face.size_metrics().ok_or("the face reports no size metrics")?;
        //   what FreeType actually settled on, whichever call set it: it does not promise an
        // exact match to either input, and the output is named by the real size
        let pixel_size = metrics.y_ppem;
        let cell_width = (metrics.max_advance >> 6) as u8;
        let cell_height = (metrics.height >> 6) as u8;
        let cell_ascent = (metrics.ascender >> 6) as u8;
        if cell_width == 0 || cell_height == 0 {
                return Err(format!("degenerate cell {cell_width}x{cell_height} -- check the display's pixel density"));
        }
        let pitch = (cell_width as usize).div_ceil(8);

        let mut flags = LoadFlag::RENDER;
        if job.display.pixel_depth == 1 {
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

        let ident = format!("{}_{}px", sanitize_identifier(&job.font_name), pixel_size);
        let lgf_path = job.out_dir.join(format!("{ident}_font.lgf"));
        fs::write(&lgf_path, encoder.encode()).map_err(|e| format!("could not write '{}': {e}", lgf_path.display()))?;

        let (c_path, h_path) = (job.out_dir.join(format!("{ident}_font.c")), job.out_dir.join(format!("{ident}_font.h")));
        fs::write(&h_path, c_header(&ident)).map_err(|e| format!("could not write '{}': {e}", h_path.display()))?;
        fs::write(&c_path, c_source(&ident, &glyphs, cell_width, cell_height, pitch)).map_err(|e| format!("could not write '{}': {e}", c_path.display()))?;

        Ok(Outcome { pixel_size, cell_width, cell_height, lgf_path })
}

/// Copies a rendered glyph into a zeroed cell, MSB-first packed, placed by its bearings relative
/// to the baseline row `cell_ascent`, clipping pixel by pixel.
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

fn sanitize_identifier(name: &str) -> String {
        let mut out = String::with_capacity(name.len() + 1);
        if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                out.push('_');
        }
        out.extend(name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }));
        out
}

fn c_header(ident: &str) -> String {
        format!("#ifndef {ident}_FONT_H\n#define {ident}_FONT_H\n\n#include <light_draw.h>\n\nextern const light_draw_font_t {ident}_font;\n\n#endif\n")
}

/// The C pair mk3's light_draw consumes, byte-for-byte in the shape mk3's crush wrote it.
fn c_source(ident: &str, glyphs: &[(u8, Vec<u8>)], cell_width: u8, cell_height: u8, pitch: usize) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "#include \"{ident}_font.h\"\n");
        for (c, rows) in glyphs {
                let _ = writeln!(s, "// '{}' (0x{:02x}):", *c as char, c);
                // ASCII art of the rows that carry ink
                let inked: Vec<usize> = (0..cell_height as usize)
                        .filter(|&y| (0..cell_width as usize).any(|x| rows[y * pitch + x / 8] >> (7 - x % 8) & 1 != 0))
                        .collect();
                if let (Some(&first), Some(&last)) = (inked.first(), inked.last()) {
                        for y in first..=last {
                                s.push_str("// ");
                                for x in 0..cell_width as usize {
                                        s.push(if rows[y * pitch + x / 8] >> (7 - x % 8) & 1 != 0 { '*' } else { ' ' });
                                }
                                s.push('\n');
                        }
                }
                let _ = write!(s, "static const uint8_t glyph_0x{c:02x}[] = {{");
                for (i, b) in rows.iter().enumerate() {
                        let _ = write!(s, "{}0x{b:02x},", if i % 12 == 0 { "\n        " } else { " " });
                }
                s.push_str("\n};\n\n");
        }
        s.push_str("static const uint8_t *const glyph_table[LIGHT_DRAW_FONT_GLYPH_TABLE_SIZE] = {\n");
        for (c, _) in glyphs {
                let _ = writeln!(s, "        [0x{c:02x}] = glyph_0x{c:02x},");
        }
        s.push_str("};\n\n");
        let _ = write!(s, "const light_draw_font_t {ident}_font = {{\n        .glyphs = glyph_table,\n        .char_width = {cell_width},\n        .char_height = {cell_height},\n}};\n");
        s
}
