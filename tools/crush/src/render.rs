//! The render job: the file-and-C-output shell around [`crush_core::render`]. The rasterising
//! itself -- FreeType into a fixed cell, packed into an LGF -- lives in crush-core, shared with the
//! host tools; this reads the font file, then writes the LGF blob and, for the C consumers, the same
//! C pair the crush this replaces wrote.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

pub use crush_core::render::CHAR_SET;

use crate::context::Display;

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
        let ttf = fs::read(&job.font_file).map_err(|e| format!("could not read '{}': {e}", job.font_file.display()))?;
        //   pixel_depth 1 renders monochrome, the 1 bpp the LGF stores
        let mono = job.display.pixel_depth == 1;
        let r = crush_core::render::rasterize(&ttf, job.face_index, job.pixel_size, job.point_size, job.display.ppi_h, job.display.ppi_v, mono)?;

        let ident = format!("{}_{}px", sanitize_identifier(&job.font_name), r.pixel_size);
        let lgf_path = job.out_dir.join(format!("{ident}_font.lgf"));
        fs::write(&lgf_path, &r.lgf).map_err(|e| format!("could not write '{}': {e}", lgf_path.display()))?;

        let pitch = (r.cell_width as usize).div_ceil(8);
        let (c_path, h_path) = (job.out_dir.join(format!("{ident}_font.c")), job.out_dir.join(format!("{ident}_font.h")));
        fs::write(&h_path, c_header(&ident)).map_err(|e| format!("could not write '{}': {e}", h_path.display()))?;
        fs::write(&c_path, c_source(&ident, &r.glyphs, r.cell_width, r.cell_height, pitch)).map_err(|e| format!("could not write '{}': {e}", c_path.display()))?;

        Ok(Outcome { pixel_size: r.pixel_size, cell_width: r.cell_width, cell_height: r.cell_height, lgf_path })
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

/// The C pair `light_draw` consumes, byte-for-byte in the shape the C crush wrote it.
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
