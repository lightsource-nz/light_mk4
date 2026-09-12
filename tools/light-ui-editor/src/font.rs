//! The preview font.
//!
//! crush-core rasterises the bundled TTF at runtime into an LGF, which is parsed into a light-ui
//! [`Font`] -- the same rasteriser crush runs at build time on device, so the preview's glyphs match
//! the firmware's, and the editor mirrors none of that logic. Later this is the seam a font picker
//! grows from (any TTF, any size).

use light_font::Font;

/// The bundled preview face: the same TTF the firmware fonts are rendered from.
const TTF: &[u8] = include_bytes!("../../crush/tests/resources/fonts/TypeLightSans.ttf");

/// Rasterise the bundled face at `pixel_size` (square pixels, monochrome) and parse it. The LGF is
/// leaked to `'static` -- one font for the program's life, like the framebuffer -- so the `Font`
/// borrows it without a lifetime to thread.
pub fn load(pixel_size: u16) -> Font<'static> {
        //   square pixels (equal ppi); monochrome, the 1 bpp LGF stores; face 0; point size unused
        // when a pixel size is given
        let rendered = crush_core::render::rasterize(TTF, 0, pixel_size, 0.0, 96.0, 96.0, true).expect("rasterise the bundled font");
        let lgf: &'static [u8] = Vec::leak(rendered.lgf);
        Font::parse(lgf).expect("the rasterised LGF parses")
}
