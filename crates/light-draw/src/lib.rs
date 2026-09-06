//! The rasteriser: mk3's `light_draw`, ported.
//!
//! A [`Canvas`] draws in LOGICAL coordinates onto a PHYSICAL buffer through a 2x3 integer
//! affine transform composed from a rotation and a flip -- so an application can keep drawing
//! upright text however the panel is mounted -- and every primitive honours an inclusive clip
//! rectangle, so nothing can index outside the buffer and a widget's border stops exactly
//! where its fill does. Two pixel formats: 1 bpp packed eight-per-byte along a row with the
//! LEFTMOST pixel in bit 0 (what the SH1107 driver unpacks), and RGB565 big-endian (what the
//! ST7789 streams).
//!
//! Ported with its conventions intact, because each was fixed on hardware once already: 0
//! degrees points right and angles increase clockwise on screen; a rounded rectangle's arcs
//! are sampled at half a pixel so the joins never open; a disc is filled from the same spans
//! its outline would trace; Q15 rounds rather than truncates.

#![no_std]

use light_font::Font;


/// How pixels are packed in the physical buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
        /// One bit per pixel, eight per byte along a row, bit 0 = leftmost.
        Mono1,
        /// Sixteen bits per pixel, RGB565, big-endian: the byte order the push panels
        /// (SPI, QSPI) take on the wire, so their frame buffer IS the transfer.
        Rgb565,
        /// RGB565 in native little-endian halfwords: for a frame buffer a scanout engine
        /// reads as u16s by DMA rather than pushing byte-by-byte down a wire. The byte
        /// order is a property of who CONSUMES the buffer -- and it was invisible for a
        /// whole bring-up on white-on-black, whose colors are byte-swap invariant.
        Rgb565Le,
}

impl PixelFormat {
        /// Bytes per physical row.
        pub const fn stride(self, width: u16) -> usize {
                match self {
                        PixelFormat::Mono1 => (width as usize).div_ceil(8),
                        PixelFormat::Rgb565 | PixelFormat::Rgb565Le => width as usize * 2,
                }
        }

        pub const fn buffer_len(self, width: u16, height: u16) -> usize {
                self.stride(width) * height as usize
        }

        /// Whether this is a 16-bit color format, whichever byte order.
        pub const fn is_rgb565(self) -> bool {
                matches!(self, PixelFormat::Rgb565 | PixelFormat::Rgb565Le)
        }

        /// The two bytes of an RGB565 color in this format's memory order.
        #[inline]
        fn rgb565_bytes(self, color: u16) -> [u8; 2] {
                match self {
                        PixelFormat::Rgb565Le => color.to_le_bytes(),
                        _ => color.to_be_bytes(),
                }
        }
}

/// An inclusive rectangle in physical (panel) coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
        pub x0: u16,
        pub y0: u16,
        pub x1: u16,
        pub y1: u16,
}

impl Region {
        pub const fn new(x0: u16, y0: u16, x1: u16, y1: u16) -> Self {
                Self { x0, y0, x1, y1 }
        }

        pub const fn full(width: u16, height: u16) -> Self {
                Self { x0: 0, y0: 0, x1: width - 1, y1: height - 1 }
        }

        pub const fn width(&self) -> u16 {
                self.x1 - self.x0 + 1
        }

        pub const fn height(&self) -> u16 {
                self.y1 - self.y0 + 1
        }

        /// The smallest region covering both. This is how a caller honours the rule that a
        /// region update must cover what was drawn before as well as what is drawn now.
        pub fn union(&self, other: &Region) -> Region {
                Region {
                        x0: self.x0.min(other.x0),
                        y0: self.y0.min(other.y0),
                        x1: self.x1.max(other.x1),
                        y1: self.y1.max(other.y1),
                }
        }

        /// Clamp into the panel. Both corners against the same bound, so a region that was
        /// entirely off-panel collapses to an edge rather than inverting -- an inverted region
        /// has a meaningless chunk count and would never complete.
        pub fn clamped(&self, width: u16, height: u16) -> Region {
                Region {
                        x0: self.x0.min(width - 1),
                        y0: self.y0.min(height - 1),
                        x1: self.x1.min(width - 1),
                        y1: self.y1.min(height - 1),
                }
        }
}


#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Rotation {
        #[default]
        R0,
        /// Logical top edge lands on the physical right edge.
        R90,
        R180,
        /// Logical top edge lands on the physical left edge.
        R270,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Flip {
        #[default]
        None,
        Horizontal,
        Vertical,
        Both,
}

/// `phys_x = a*x + b*y + tx; phys_y = c*x + d*y + ty`. Entries in {-1, 0, 1}, determinant ±1,
/// so the inverse is exact integer arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transform {
        pub a: i32,
        pub b: i32,
        pub tx: i32,
        pub c: i32,
        pub d: i32,
        pub ty: i32,
}

impl Transform {
        pub const IDENTITY: Transform = Transform { a: 1, b: 0, tx: 0, c: 0, d: 1, ty: 0 };

        /// `second . first`: a point is mapped by `first`, then by `second`.
        fn compose(first: Transform, second: Transform) -> Transform {
                Transform {
                        a: second.a * first.a + second.b * first.c,
                        b: second.a * first.b + second.b * first.d,
                        tx: second.a * first.tx + second.b * first.ty + second.tx,
                        c: second.c * first.a + second.d * first.c,
                        d: second.c * first.b + second.d * first.d,
                        ty: second.c * first.tx + second.d * first.ty + second.ty,
                }
        }

        pub fn apply(&self, x: i32, y: i32) -> (i32, i32) {
                (self.a * x + self.b * y + self.tx, self.c * x + self.d * y + self.ty)
        }

        /// The logical-to-physical transform a canvas of this geometry and orientation uses.
        /// Flip is applied first, in logical space; then rotation maps onto the buffer.
        pub fn for_canvas(rotation: Rotation, flip: Flip, phys_w: u16, phys_h: u16) -> Transform {
                let (pw, ph) = (i32::from(phys_w), i32::from(phys_h));
                let (dx, dy) = match rotation {
                        Rotation::R90 | Rotation::R270 => (ph, pw),
                        _ => (pw, ph),
                };
                let flip = match flip {
                        Flip::None => Transform::IDENTITY,
                        Flip::Horizontal => Transform { a: -1, b: 0, tx: dx - 1, c: 0, d: 1, ty: 0 },
                        Flip::Vertical => Transform { a: 1, b: 0, tx: 0, c: 0, d: -1, ty: dy - 1 },
                        Flip::Both => Transform { a: -1, b: 0, tx: dx - 1, c: 0, d: -1, ty: dy - 1 },
                };
                let rotate = match rotation {
                        Rotation::R0 => Transform::IDENTITY,
                        Rotation::R90 => Transform { a: 0, b: -1, tx: pw - 1, c: 1, d: 0, ty: 0 },
                        Rotation::R180 => Transform { a: -1, b: 0, tx: pw - 1, c: 0, d: -1, ty: ph - 1 },
                        Rotation::R270 => Transform { a: 0, b: 1, tx: 0, c: -1, d: 0, ty: ph - 1 },
                };
                Transform::compose(flip, rotate)
        }

        /// A logical axis-aligned rectangle's physical bounds.
        pub fn rect(&self, r: &Region) -> Region {
                let (ax, ay) = self.apply(i32::from(r.x0), i32::from(r.y0));
                let (bx, by) = self.apply(i32::from(r.x1), i32::from(r.y1));
                Region::new(ax.min(bx) as u16, ay.min(by) as u16, ax.max(bx) as u16, ay.max(by) as u16)
        }
}

/// A point in logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
        pub x: i32,
        pub y: i32,
}

impl Point {
        pub const fn new(x: i32, y: i32) -> Self {
                Self { x, y }
        }
}

/// Which corners a rounded rectangle rounds.
pub mod corner {
        pub const NONE: u8 = 0;
        pub const TOP_LEFT: u8 = 1;
        pub const TOP_RIGHT: u8 = 2;
        pub const BOTTOM_RIGHT: u8 = 4;
        pub const BOTTOM_LEFT: u8 = 8;
        pub const TOP: u8 = TOP_LEFT | TOP_RIGHT;
        pub const BOTTOM: u8 = BOTTOM_LEFT | BOTTOM_RIGHT;
        pub const ALL: u8 = 0xF;
}

pub struct Canvas<'a> {
        buf: &'a mut [u8],
        format: PixelFormat,
        phys_w: u16,
        phys_h: u16,
        /// Logical dimensions: swapped from physical under a 90/270 rotation.
        dim_x: u16,
        dim_y: u16,
        rotation: Rotation,
        flip: Flip,
        /// Logical translation applied before the orientation transform -- a page
        /// transition's slide. Non-zero offsets shrink the clip to the window that still
        /// lands on the buffer, and set_clip/clear_clip preserve that bound.
        off_x: i32,
        off_y: i32,
        transform: Transform,
        /// Inclusive, logical, never larger than the canvas.
        clip: Region,
        pub fg: u16,
        pub bg: u16,
}

impl<'a> Canvas<'a> {
        /// `buf` must be at least `format.buffer_len(width, height)`.
        pub fn new(buf: &'a mut [u8], format: PixelFormat, width: u16, height: u16) -> Self {
                assert!(buf.len() >= format.buffer_len(width, height));
                let mut c = Self {
                        buf,
                        format,
                        phys_w: width,
                        phys_h: height,
                        dim_x: width,
                        dim_y: height,
                        rotation: Rotation::R0,
                        flip: Flip::None,
                        off_x: 0,
                        off_y: 0,
                        transform: Transform::IDENTITY,
                        clip: Region::full(width, height),
                        fg: 0xFFFF,
                        bg: 0,
                };
                c.recompute();
                c
        }

        pub fn width(&self) -> u16 {
                self.dim_x
        }

        pub fn height(&self) -> u16 {
                self.dim_y
        }

        pub fn format(&self) -> PixelFormat {
                self.format
        }

        pub fn transform(&self) -> Transform {
                self.transform
        }

        pub fn buffer(&self) -> &[u8] {
                self.buf
        }

        /// Existing content is not transformed; set up before drawing. Resets the clip.
        pub fn set_rotation(&mut self, rotation: Rotation) {
                self.rotation = rotation;
                self.recompute();
        }

        pub fn set_flip(&mut self, flip: Flip) {
                self.flip = flip;
                self.recompute();
        }

        fn recompute(&mut self) {
                if matches!(self.rotation, Rotation::R90 | Rotation::R270) {
                        self.dim_x = self.phys_h;
                        self.dim_y = self.phys_w;
                } else {
                        self.dim_x = self.phys_w;
                        self.dim_y = self.phys_h;
                }
                self.transform = Transform::for_canvas(self.rotation, self.flip, self.phys_w, self.phys_h);
                if self.off_x != 0 || self.off_y != 0 {
                        //   the translation is logical, so it goes FIRST; the orientation
                        // transform then maps the shifted coordinates onto the buffer
                        let translate = Transform { a: 1, b: 0, tx: self.off_x, c: 0, d: 1, ty: self.off_y };
                        self.transform = Transform::compose(translate, self.transform);
                }
                //   any clip was expressed in the logical space just redefined
                self.clip = self.offset_window();
        }

        /// The logical window that still lands on the buffer under the current offset -- the
        /// whole canvas at offset zero. Every clip is bounded by this: the primitives trust
        /// the clip to keep `run` on the buffer, so nothing may widen past it.
        fn offset_window(&self) -> Region {
                let (w, h) = (i32::from(self.dim_x), i32::from(self.dim_y));
                let x0 = (-self.off_x).max(0);
                let x1 = (w - 1 - self.off_x).min(w - 1);
                let y0 = (-self.off_y).max(0);
                let y1 = (h - 1 - self.off_y).min(h - 1);
                if x0 > x1 || y0 > y1 {
                        //   nothing lands on the buffer: a clip no span or cell passes
                        return Region::new(1, 1, 0, 0);
                }
                Region::new(x0 as u16, y0 as u16, x1 as u16, y1 as u16)
        }

        /// Translate everything drawn next by `(dx, dy)` LOGICAL pixels -- the page
        /// transition's slide. The clip shrinks to what still lands on the buffer, and any
        /// later `set_clip`/`clear_clip` stays inside that bound; reset with `(0, 0)`.
        pub fn set_offset(&mut self, dx: i32, dy: i32) {
                self.off_x = dx;
                self.off_y = dy;
                self.recompute();
        }

        /// Restrict drawing to a logical rectangle (inclusive, clamped to the canvas). Set it,
        /// draw what must not escape, then `clear_clip` -- the clip is canvas state.
        pub fn set_clip(&mut self, r: Region) {
                let w = self.offset_window();
                let r = r.clamped(self.dim_x, self.dim_y);
                let (x0, y0) = (r.x0.max(w.x0), r.y0.max(w.y0));
                let (x1, y1) = (r.x1.min(w.x1), r.y1.min(w.y1));
                self.clip = if x0 > x1 || y0 > y1 { Region::new(1, 1, 0, 0) } else { Region::new(x0, y0, x1, y1) };
        }

        pub fn clear_clip(&mut self) {
                self.clip = self.offset_window();
        }

        pub fn clip(&self) -> Region {
                self.clip
        }

        /// A logical axis-aligned rectangle's physical bounds. Every transform here maps
        /// axis-aligned rectangles to axis-aligned rectangles, so two opposite corners suffice.
        pub fn transform_rect(&self, r: &Region) -> Region {
                self.transform.rect(r)
        }

        /// A physical point back into logical space -- for input, which arrives in the panel's
        /// frame. Exact (determinant ±1), clamped into the canvas.
        pub fn untransform_point(&self, phys_x: i32, phys_y: i32) -> Point {
                let m = &self.transform;
                let det = m.a * m.d - m.b * m.c;
                let px = phys_x - m.tx;
                let py = phys_y - m.ty;
                let x = (m.d * px - m.b * py) * det;
                let y = (m.a * py - m.c * px) * det;
                Point::new(x.clamp(0, i32::from(self.dim_x) - 1), y.clamp(0, i32::from(self.dim_y) - 1))
        }

        // --- pixels -----------------------------------------------------------------------

        /// Unclipped physical write. Callers guarantee the logical point is on the canvas.
        fn set_phys(&mut self, x: u16, y: u16, color: u16) {
                match self.format {
                        PixelFormat::Mono1 => {
                                let stride = self.format.stride(self.phys_w);
                                let byte = &mut self.buf[y as usize * stride + x as usize / 8];
                                if color != 0 {
                                        *byte |= 1 << (x % 8);
                                } else {
                                        *byte &= !(1 << (x % 8));
                                }
                        }
                        PixelFormat::Rgb565 | PixelFormat::Rgb565Le => {
                                let i = (y as usize * self.phys_w as usize + x as usize) * 2;
                                let [b0, b1] = self.format.rgb565_bytes(color);
                                self.buf[i] = b0;
                                self.buf[i + 1] = b1;
                        }
                }
        }

        fn get_phys(&self, x: u16, y: u16) -> u16 {
                match self.format {
                        PixelFormat::Mono1 => {
                                let stride = self.format.stride(self.phys_w);
                                u16::from(self.buf[y as usize * stride + x as usize / 8] >> (x % 8) & 1)
                        }
                        PixelFormat::Rgb565 => {
                                let i = (y as usize * self.phys_w as usize + x as usize) * 2;
                                u16::from_be_bytes([self.buf[i], self.buf[i + 1]])
                        }
                        PixelFormat::Rgb565Le => {
                                let i = (y as usize * self.phys_w as usize + x as usize) * 2;
                                u16::from_le_bytes([self.buf[i], self.buf[i + 1]])
                        }
                }
        }

        fn in_clip(&self, x: i32, y: i32) -> bool {
                x >= i32::from(self.clip.x0) && y >= i32::from(self.clip.y0) && x <= i32::from(self.clip.x1) && y <= i32::from(self.clip.y1)
        }

        /// Set a logical pixel, clipped.
        pub fn set(&mut self, x: i32, y: i32, color: u16) {
                if !self.in_clip(x, y) {
                        return;
                }
                let (px, py) = self.transform.apply(x, y);
                self.set_phys(px as u16, py as u16, color);
        }

        /// Read a logical pixel; 0 outside the canvas.
        pub fn get(&self, x: i32, y: i32) -> u16 {
                if x < 0 || y < 0 || x >= i32::from(self.dim_x) || y >= i32::from(self.dim_y) {
                        return 0;
                }
                let (px, py) = self.transform.apply(x, y);
                self.get_phys(px as u16, py as u16)
        }

        /// A horizontal run, clipped.
        fn span(&mut self, x0: i32, x1: i32, y: i32, color: u16) {
                if y < i32::from(self.clip.y0) || y > i32::from(self.clip.y1) {
                        return;
                }
                let (mut x0, mut x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
                if x1 < i32::from(self.clip.x0) || x0 > i32::from(self.clip.x1) {
                        return;
                }
                x0 = x0.max(i32::from(self.clip.x0));
                x1 = x1.min(i32::from(self.clip.x1));
                self.run(x0, y, (x1 - x0 + 1) as usize, color);
        }

        /// A horizontal logical run of `len` pixels from `(x0, y)`, already clipped, written
        /// without transforming each pixel: every transform here maps logical +x onto one
        /// physical axis, so the run is a fixed stride through the buffer -- contiguous under
        /// R0/R180, a column under R90/R270. This is what makes a full repaint affordable:
        /// per-pixel transform and clip put a 240x280 frame at 22 ms, most of it in fills.
        fn run(&mut self, x0: i32, y: i32, len: usize, color: u16) {
                if len == 0 {
                        return;
                }
                let (px, py) = self.transform.apply(x0, y);
                // physical step per logical +x, in pixels
                let step = self.transform.a as isize + self.transform.c as isize * self.phys_w as isize;
                let mut i = py as isize * self.phys_w as isize + px as isize;
                match self.format {
                        PixelFormat::Rgb565 | PixelFormat::Rgb565Le => {
                                let [hi, lo] = self.format.rgb565_bytes(color);
                                if step == 1 {
                                        let start = i as usize * 2;
                                        if hi == lo {
                                                //   black, white and the greys: a fill, an
                                                // order of magnitude over the byte-pair loop.
                                                // Background fills live here, which is what
                                                // lets a window paint its own interior for
                                                // the cost of the clear it replaces
                                                self.buf[start..start + len * 2].fill(hi);
                                                return;
                                        }
                                        for p in self.buf[start..start + len * 2].chunks_exact_mut(2) {
                                                p[0] = hi;
                                                p[1] = lo;
                                        }
                                        return;
                                }
                                for _ in 0..len {
                                        let b = i as usize * 2;
                                        self.buf[b] = hi;
                                        self.buf[b + 1] = lo;
                                        i += step;
                                }
                        }
                        PixelFormat::Mono1 => {
                                // packed 8 to a byte along physical x; a stride other than ±1
                                // walks rows, so the general path stays per pixel
                                let stride = self.format.stride(self.phys_w);
                                let (mut x, mut yy) = (px as isize, py as isize);
                                let (dx, dy) = (self.transform.a as isize, self.transform.c as isize);
                                for _ in 0..len {
                                        let byte = &mut self.buf[yy as usize * stride + x as usize / 8];
                                        if color != 0 {
                                                *byte |= 1 << (x % 8);
                                        } else {
                                                *byte &= !(1 << (x % 8));
                                        }
                                        x += dx;
                                        yy += dy;
                                }
                        }
                }
        }

        // --- primitives -------------------------------------------------------------------

        /// Fill the whole physical buffer with the background colour, ignoring the clip.
        pub fn clear(&mut self) {
                match self.format {
                        PixelFormat::Mono1 => {
                                let v = if self.bg != 0 { 0xFF } else { 0x00 };
                                let n = self.format.buffer_len(self.phys_w, self.phys_h);
                                self.buf[..n].fill(v);
                        }
                        PixelFormat::Rgb565 | PixelFormat::Rgb565Le => {
                                let [hi, lo] = self.format.rgb565_bytes(self.bg);
                                let n = self.format.buffer_len(self.phys_w, self.phys_h);
                                if hi == lo {
                                        // black, white and the greys: one memset
                                        self.buf[..n].fill(hi);
                                        return;
                                }
                                for px in self.buf[..n].chunks_exact_mut(2) {
                                        px[0] = hi;
                                        px[1] = lo;
                                }
                        }
                }
        }

        /// A vertical run, clipped: the column counterpart of `span`, stepping the buffer by the
        /// transform's logical-+y stride rather than transforming each pixel.
        fn column(&mut self, x: i32, y0: i32, y1: i32, color: u16) {
                if x < i32::from(self.clip.x0) || x > i32::from(self.clip.x1) {
                        return;
                }
                let (y0, y1) = (y0.min(y1).max(i32::from(self.clip.y0)), y0.max(y1).min(i32::from(self.clip.y1)));
                if y1 < y0 {
                        return;
                }
                let (px, py) = self.transform.apply(x, y0);
                let step = self.transform.b as isize + self.transform.d as isize * self.phys_w as isize;
                let mut i = py as isize * self.phys_w as isize + px as isize;
                match self.format {
                        PixelFormat::Rgb565 | PixelFormat::Rgb565Le => {
                                let [hi, lo] = self.format.rgb565_bytes(color);
                                for _ in y0..=y1 {
                                        let b = i as usize * 2;
                                        self.buf[b] = hi;
                                        self.buf[b + 1] = lo;
                                        i += step;
                                }
                        }
                        PixelFormat::Mono1 => {
                                for y in y0..=y1 {
                                        self.set(x, y, color);
                                }
                        }
                }
        }

        /// Bresenham, clipped per pixel. Corners in any order. Axis-aligned lines take the run
        /// paths, which is what every frame and separator is.
        pub fn line(&mut self, p0: Point, p1: Point) {
                let color = self.fg;
                if p0.y == p1.y {
                        self.span(p0.x, p1.x, p0.y, color);
                        return;
                }
                if p0.x == p1.x {
                        self.column(p0.x, p0.y, p1.y, color);
                        return;
                }
                let dx = (p1.x - p0.x).abs();
                let sx = if p0.x < p1.x { 1 } else { -1 };
                let dy = -(p1.y - p0.y).abs();
                let sy = if p0.y < p1.y { 1 } else { -1 };
                let mut err = dx + dy;
                let (mut x, mut y) = (p0.x, p0.y);
                loop {
                        self.set(x, y, color);
                        let e2 = 2 * err;
                        if e2 >= dy {
                                if x == p1.x {
                                        break;
                                }
                                err += dy;
                                x += sx;
                        }
                        if e2 <= dx {
                                if y == p1.y {
                                        break;
                                }
                                err += dx;
                                y += sy;
                        }
                }
        }

        /// Axis-aligned rectangle between two corners in any order; outline or filled.
        pub fn rect(&mut self, p0: Point, p1: Point, fill: bool) {
                let (x0, x1) = (p0.x.min(p1.x), p0.x.max(p1.x));
                let (y0, y1) = (p0.y.min(p1.y), p0.y.max(p1.y));
                let color = self.fg;
                if fill {
                        for y in y0..=y1 {
                                self.span(x0, x1, y, color);
                        }
                        return;
                }
                self.line(Point::new(x0, y0), Point::new(x0, y1));
                self.line(Point::new(x0, y0), Point::new(x1, y0));
                self.line(Point::new(x0, y1), Point::new(x1, y1));
                self.line(Point::new(x1, y0), Point::new(x1, y1));
        }

        /// Fill a region with `color`, clipped. A convenience over `rect` for erasing.
        pub fn fill_region(&mut self, r: &Region, color: u16) {
                for y in i32::from(r.y0)..=i32::from(r.y1) {
                        self.span(i32::from(r.x0), i32::from(r.x1), y, color);
                }
        }

        /// Midpoint circle, outline or filled from the same spans the outline traces.
        pub fn circle(&mut self, centre: Point, radius: u16, fill: bool) {
                let color = self.fg;
                let (cx, cy) = (centre.x, centre.y);
                let r = i32::from(radius);
                let mut d = 3 - 2 * r;
                let (mut px, mut py) = (0i32, r);
                if fill {
                        while py >= px {
                                self.span(cx - px, cx + px, cy + py, color);
                                self.span(cx - px, cx + px, cy - py, color);
                                self.span(cx - py, cx + py, cy + px, color);
                                self.span(cx - py, cx + py, cy - px, color);
                                px += 1;
                                if d > 0 {
                                        py -= 1;
                                        d += 4 * (px - py) + 10;
                                } else {
                                        d += 4 * px + 6;
                                }
                        }
                        return;
                }
                self.octants(cx, cy, px, py, color);
                while py >= px {
                        px += 1;
                        if d > 0 {
                                py -= 1;
                                d += 4 * (px - py) + 10;
                        } else {
                                d += 4 * px + 6;
                        }
                        self.octants(cx, cy, px, py, color);
                }
        }

        fn octants(&mut self, cx: i32, cy: i32, px: i32, py: i32, color: u16) {
                self.set(cx + px, cy + py, color);
                self.set(cx + py, cy + px, color);
                self.set(cx + py, cy - px, color);
                self.set(cx + px, cy - py, color);
                self.set(cx - px, cy - py, color);
                self.set(cx - py, cy - px, color);
                self.set(cx - py, cy + px, color);
                self.set(cx - px, cy + py, color);
        }

        /// The part of a circle between two angles. 0 degrees points right, angles increase
        /// clockwise on screen (y grows downward); `end < start` sweeps the long way round.
        pub fn arc(&mut self, centre: Point, radius: u16, start_deg: i16, end_deg: i16) {
                let color = self.fg;
                if radius == 0 {
                        self.set(centre.x, centre.y, color);
                        return;
                }
                let start = i32::from(start_deg).rem_euclid(360);
                let end = i32::from(end_deg).rem_euclid(360);
                let mut span = end - start;
                if span <= 0 {
                        span += 360;
                }
                let start_q8 = start << 8;
                let span_q8 = span << 8;
                // arc length is r*theta, so a one-pixel step is 1/r radians; sampled at half
                // that spacing so rounding can never open a gap (1144/2^24 is pi/(180*256))
                let steps = ((i64::from(radius) * i64::from(span_q8) * 1144) >> 24) as i32 * 2 + 1;
                for i in 0..=steps {
                        let theta = start_q8 + ((i64::from(span_q8) * i64::from(i)) / i64::from(steps)) as i32;
                        let x = centre.x + ((i32::from(radius) * cos_deg_q8_q15(theta) + 16384) >> 15);
                        let y = centre.y + ((i32::from(radius) * sin_deg_q8_q15(theta) + 16384) >> 15);
                        self.set(x, y, color);
                }
        }

        /// A rectangle whose named corners are rounded to `radius` (clamped to half the shorter
        /// side, so an over-large radius degenerates into a stadium rather than nonsense).
        pub fn rect_rounded(&mut self, p0: Point, p1: Point, radius: u16, corners: u8, fill: bool) {
                let (x0, x1) = (p0.x.min(p1.x), p0.x.max(p1.x));
                let (y0, y1) = (p0.y.min(p1.y), p0.y.max(p1.y));
                let r = i32::from(radius).min((x1 - x0) / 2).min((y1 - y0) / 2);
                if r <= 0 || corners == corner::NONE {
                        self.rect(Point::new(x0, y0), Point::new(x1, y1), fill);
                        return;
                }
                let inset = |bit: u8| if corners & bit != 0 { r } else { 0 };
                let (tl, tr, br, bl) = (inset(corner::TOP_LEFT), inset(corner::TOP_RIGHT), inset(corner::BOTTOM_RIGHT), inset(corner::BOTTOM_LEFT));
                let color = self.fg;
                if fill {
                        for y in y0..=y1 {
                                let (mut li, mut ri) = (0, 0);
                                if y < y0 + r {
                                        let dy = y0 + r - y;
                                        let d = r - isqrt((r * r - dy * dy) as u32) as i32;
                                        if tl != 0 {
                                                li = d;
                                        }
                                        if tr != 0 {
                                                ri = d;
                                        }
                                } else if y > y1 - r {
                                        let dy = y - (y1 - r);
                                        let d = r - isqrt((r * r - dy * dy) as u32) as i32;
                                        if bl != 0 {
                                                li = d;
                                        }
                                        if br != 0 {
                                                ri = d;
                                        }
                                }
                                self.span(x0 + li, x1 - ri, y, color);
                        }
                        return;
                }
                self.line(Point::new(x0 + tl, y0), Point::new(x1 - tr, y0));
                self.line(Point::new(x0 + bl, y1), Point::new(x1 - br, y1));
                self.line(Point::new(x0, y0 + tl), Point::new(x0, y1 - bl));
                self.line(Point::new(x1, y0 + tr), Point::new(x1, y1 - br));
                let r16 = r as u16;
                if tl != 0 {
                        self.arc(Point::new(x0 + r, y0 + r), r16, 180, 270);
                }
                if tr != 0 {
                        self.arc(Point::new(x1 - r, y0 + r), r16, 270, 360);
                }
                if br != 0 {
                        self.arc(Point::new(x1 - r, y1 - r), r16, 0, 90);
                }
                if bl != 0 {
                        self.arc(Point::new(x0 + r, y1 - r), r16, 90, 180);
                }
        }

        /// A rectangle filled with a VERTICAL gradient: `from` at the top row, `to` at the
        /// bottom. On a Mono1 canvas the interpolated colors degenerate to set pixels --
        /// a shade is an RGB565 idea; mono callers get a solid fill.
        pub fn rect_shaded(&mut self, p0: Point, p1: Point, from: u16, to: u16) {
                let (x0, x1) = (p0.x.min(p1.x), p0.x.max(p1.x));
                let (y0, y1) = (p0.y.min(p1.y), p0.y.max(p1.y));
                let den = y1 - y0;
                for y in y0..=y1 {
                        self.span(x0, x1, y, lerp565(from, to, y - y0, den));
                }
        }

        /// [`rect_rounded`](Self::rect_rounded)'s fill with a vertical gradient: the same
        /// corner geometry, the row color interpolated from `from` to `to`.
        pub fn rect_rounded_shaded(&mut self, p0: Point, p1: Point, radius: u16, corners: u8, from: u16, to: u16) {
                let (x0, x1) = (p0.x.min(p1.x), p0.x.max(p1.x));
                let (y0, y1) = (p0.y.min(p1.y), p0.y.max(p1.y));
                let r = i32::from(radius).min((x1 - x0) / 2).min((y1 - y0) / 2);
                if r <= 0 || corners == corner::NONE {
                        self.rect_shaded(p0, p1, from, to);
                        return;
                }
                let inset = |bit: u8| if corners & bit != 0 { r } else { 0 };
                let (tl, tr, br, bl) = (inset(corner::TOP_LEFT), inset(corner::TOP_RIGHT), inset(corner::BOTTOM_RIGHT), inset(corner::BOTTOM_LEFT));
                let den = y1 - y0;
                for y in y0..=y1 {
                        let (mut li, mut ri) = (0, 0);
                        if y < y0 + r {
                                let dy = y0 + r - y;
                                let d = r - isqrt((r * r - dy * dy) as u32) as i32;
                                if tl != 0 {
                                        li = d;
                                }
                                if tr != 0 {
                                        ri = d;
                                }
                        } else if y > y1 - r {
                                let dy = y - (y1 - r);
                                let d = r - isqrt((r * r - dy * dy) as u32) as i32;
                                if bl != 0 {
                                        li = d;
                                }
                                if br != 0 {
                                        ri = d;
                                }
                        }
                        self.span(x0 + li, x1 - ri, y, lerp565(from, to, y - y0, den));
                }
        }

        // --- blits ------------------------------------------------------------------------
        //
        // Both work in PHYSICAL buffer space and ignore the transform entirely: they move an
        // image, they do not draw through the logical-to-physical mapping. That is what makes
        // them usable for animating between two rotations, where the transform is precisely the
        // thing in flux. `src` must be a buffer of exactly this canvas's geometry and format --
        // typically the display's captured back buffer. 16 bpp only; a 1 bpp path can follow if
        // a mono panel ever needs one (`false` says nothing was drawn).

        /// 1.0 in the Q15 scale `blit_rotated` takes.
        pub const SCALE_ONE: i32 = 32768;

        /// The largest scale at which a rotation by `angle_deg` still fits entirely inside the
        /// buffer -- `SCALE_ONE` at 0 and 180 degrees, smallest at 45. A w*h image rotated by
        /// 45 degrees needs a box of w|sin| + h|cos| on a side, so at 1:1 the corners fall
        /// outside a fixed buffer and are cut off part-way through a turn.
        pub fn scale_inscribed(&self, angle_deg: i16) -> i32 {
                let s = sin_deg_q15(i32::from(angle_deg)).abs();
                let c = sin_deg_q15(i32::from(angle_deg) + 90).abs();
                let (w, h) = (i32::from(self.phys_w), i32::from(self.phys_h));
                let need_w = w * c + h * s;
                let need_h = w * s + h * c;
                if need_w <= 0 || need_h <= 0 {
                        return Self::SCALE_ONE;
                }
                //   (dimension << 30) / need in 64 bits: shifting need down to whole pixels first
                // would round the divisor, and rounding a divisor down inflates the result --
                // an inscribed scale even slightly too large clips the corners it exists to keep
                let fit_w = ((i64::from(w) << 30) / i64::from(need_w)) as i32;
                let fit_h = ((i64::from(h) << 30) / i64::from(need_h)) as i32;
                fit_w.min(fit_h).min(Self::SCALE_ONE)
        }

        /// Sample `src` into this buffer rotated by `angle_deg` about the centre and scaled by
        /// `scale_q15`. INVERSE sampled: for every destination pixel the source is found and its
        /// nearest pixel taken -- mapping source pixels forward would scatter them and leave
        /// gaps that widen with the scale. Destination pixels whose source falls outside the
        /// buffer are left as they are, so clear first if that matters.
        pub fn blit_rotated(&mut self, src: &[u8], angle_deg: i16, scale_q15: i32) -> bool {
                //   byte pairs copied verbatim from a same-format snapshot: endian-agnostic
                if !self.format.is_rgb565() || scale_q15 <= 0 {
                        return false;
                }
                let (w, h) = (i32::from(self.phys_w), i32::from(self.phys_h));
                if src.len() < (w * h * 2) as usize {
                        return false;
                }
                let (cx, cy) = (w / 2, h / 2);
                // the inverse rotation folded with the inverse scale, so the inner loop is two
                // multiply-accumulates per axis
                let inv = ((i64::from(Self::SCALE_ONE) * i64::from(Self::SCALE_ONE)) / i64::from(scale_q15)) as i32;
                let cos_i = ((i64::from(sin_deg_q15(i32::from(angle_deg) + 90)) * i64::from(inv)) >> 15) as i32;
                let sin_i = ((i64::from(sin_deg_q15(i32::from(angle_deg))) * i64::from(inv)) >> 15) as i32;
                for dy in 0..h {
                        let ry = dy - cy;
                        // the row's source origin, stepped along x rather than recomputed
                        let mut sx = (-cx * cos_i + ry * sin_i) + (cx << 15);
                        let mut sy = (cx * sin_i + ry * cos_i) + (cy << 15);
                        let row = (dy * w * 2) as usize;
                        for dx in 0..w {
                                //   rounded, not truncated: Q15 cannot represent 1.0, so at 0
                                // degrees the step is one ulp short of a pixel and the deficit
                                // crosses a boundary at the centre, shifting half the image by
                                // one. Rounding absorbs it
                                let px = (sx + 16384) >> 15;
                                let py = (sy + 16384) >> 15;
                                if px >= 0 && py >= 0 && px < w && py < h {
                                        let s = ((py * w + px) * 2) as usize;
                                        let d = row + (dx * 2) as usize;
                                        self.buf[d] = src[s];
                                        self.buf[d + 1] = src[s + 1];
                                }
                                sx += cos_i;
                                sy -= sin_i;
                        }
                }
                true
        }

        /// Copy `src` into this buffer displaced by `(off_x, off_y)` in physical pixels. The
        /// band the displacement uncovers is left as it is, so whatever was drawn first shows
        /// through: draw the incoming image, then slide the outgoing one off it. A caller wanting
        /// to slide in a direction the VIEWER would name maps it through the transform's `a` and
        /// `c`, which give the physical direction logical +x points in.
        pub fn blit_offset(&mut self, src: &[u8], off_x: i32, off_y: i32) -> bool {
                if !self.format.is_rgb565() {
                        return false;
                }
                let (w, h) = (i32::from(self.phys_w), i32::from(self.phys_h));
                if src.len() < (w * h * 2) as usize {
                        return false;
                }
                if off_x <= -w || off_x >= w || off_y <= -h || off_y >= h {
                        return false;
                }
                // a translation: each destination row draws from one contiguous run of one
                // source row, a copy per row rather than a loop per pixel
                for dy in 0..h {
                        let sy = dy - off_y;
                        if sy < 0 || sy >= h {
                                continue;
                        }
                        let dx0 = off_x.max(0);
                        let dx1 = if off_x < 0 { w + off_x } else { w };
                        if dx1 <= dx0 {
                                continue;
                        }
                        let n = ((dx1 - dx0) * 2) as usize;
                        let s = ((sy * w + (dx0 - off_x)) * 2) as usize;
                        let d = ((dy * w + dx0) * 2) as usize;
                        self.buf[d..d + n].copy_from_slice(&src[s..s + n]);
                }
                true
        }

        /// Draw `text` with its cell's top-left at `origin`, ink in `fg`. Only ink is painted;
        /// clear the box first (or use [`text_boxed`](Self::text_boxed)) if the background
        /// matters. Returns the logical region the cells cover, clipped, if any of it landed.
        pub fn text(&mut self, font: &Font<'_>, origin: Point, text: &str) -> Option<Region> {
                self.text_impl(font, origin, text, None)
        }

        /// As [`text`](Self::text), painting every cell pixel: ink in `fg`, the rest in `bg`.
        pub fn text_boxed(&mut self, font: &Font<'_>, origin: Point, text: &str) -> Option<Region> {
                let bg = self.bg;
                self.text_impl(font, origin, text, Some(bg))
        }

        fn text_impl(&mut self, font: &Font<'_>, origin: Point, text: &str, bg: Option<u16>) -> Option<Region> {
                let (cw, ch) = (i32::from(font.cell_width()), i32::from(font.cell_height()));
                let fg = self.fg;
                let mut pen = origin.x;
                let mut touched: Option<Region> = None;
                let clip = self.clip;
                let pitch = usize::from(font.pitch());
                for c in text.bytes() {
                        //   the glyph looked up ONCE per character. `Font::pixel` is a lookup per
                        // pixel -- a popcount over the presence map each time -- and going through
                        // it cost a 240x280 frame 20 ms in labels alone. The cell is clipped once,
                        // then each row walked as runs of like pixels, so a glyph costs a few
                        // run() calls per row rather than a transform per pixel
                        let glyph = font.glyph(c);
                        let x_lo = pen.max(i32::from(clip.x0));
                        let x_hi = (pen + cw - 1).min(i32::from(clip.x1));
                        let y_lo = origin.y.max(i32::from(clip.y0));
                        let y_hi = (origin.y + ch - 1).min(i32::from(clip.y1));
                        if x_lo <= x_hi && y_lo <= y_hi {
                                for y in y_lo..=y_hi {
                                        let row = glyph.map(|g| &g[(y - origin.y) as usize * pitch..]);
                                        let bit = |gx: i32| -> bool {
                                                match row {
                                                        Some(r) => r[gx as usize / 8] >> (7 - gx % 8) & 1 != 0,
                                                        None => false,
                                                }
                                        };
                                        //   RGB565 ink-only text is the case every label is, and
                                        // it gets the tight loop: the row's physical start and
                                        // step computed once, two bytes written per ink pixel
                                        if bg.is_none() && self.format.is_rgb565() {
                                                if let Some(r) = row {
                                                        let (px, py) = self.transform.apply(x_lo, y);
                                                        let step = self.transform.a as isize + self.transform.c as isize * self.phys_w as isize;
                                                        let mut i = py as isize * self.phys_w as isize + px as isize;
                                                        let [hi, lo] = self.format.rgb565_bytes(fg);
                                                        for gx in (x_lo - pen)..=(x_hi - pen) {
                                                                if r[gx as usize / 8] >> (7 - gx % 8) & 1 != 0 {
                                                                        let b = i as usize * 2;
                                                                        self.buf[b] = hi;
                                                                        self.buf[b + 1] = lo;
                                                                }
                                                                i += step;
                                                        }
                                                }
                                                continue;
                                        }
                                        let mut x = x_lo;
                                        while x <= x_hi {
                                                let on = bit(x - pen);
                                                let mut end = x;
                                                while end + 1 <= x_hi && bit(end + 1 - pen) == on {
                                                        end += 1;
                                                }
                                                match (on, bg) {
                                                        (true, _) => self.run(x, y, (end - x + 1) as usize, fg),
                                                        (false, Some(b)) => self.run(x, y, (end - x + 1) as usize, b),
                                                        (false, None) => {}
                                                }
                                                x = end + 1;
                                        }
                                }
                        }
                        let cell = Region::new(pen.max(0) as u16, origin.y.max(0) as u16, (pen + cw - 1).max(0) as u16, (origin.y + ch - 1).max(0) as u16);
                        if pen + cw > 0 && origin.y + ch > 0 && pen < i32::from(self.dim_x) && origin.y < i32::from(self.dim_y) {
                                let cell = cell.clamped(self.dim_x, self.dim_y);
                                touched = Some(touched.map_or(cell, |t| t.union(&cell)));
                        }
                        pen += cw;
                }
                touched
        }
}

/// Sine of 0..90 degrees in Q15, one entry per degree. 182 bytes of rodata beats any runtime
/// approximation, and keeps this free of floating point.
const SIN_Q15: [i16; 91] = [
        0, 572, 1144, 1715, 2286, 2856, 3425, 3993, 4560, 5126, 5690, 6252, 6813, 7371, 7927, 8481, 9032, 9580, 10126, 10668, 11207, 11743,
        12275, 12803, 13328, 13848, 14365, 14876, 15384, 15886, 16384, 16877, 17364, 17847, 18324, 18795, 19261, 19720, 20174, 20622, 21063,
        21498, 21926, 22348, 22763, 23170, 23571, 23965, 24351, 24730, 25102, 25466, 25822, 26170, 26510, 26842, 27166, 27482, 27789, 28088,
        28378, 28660, 28932, 29197, 29452, 29698, 29935, 30163, 30382, 30592, 30792, 30983, 31164, 31336, 31499, 31651, 31795, 31928, 32052,
        32166, 32270, 32365, 32449, 32524, 32588, 32643, 32688, 32723, 32748, 32763, 32767,
];

fn sin_deg_q15(angle_deg: i32) -> i32 {
        let a = angle_deg.rem_euclid(360);
        let v = |i: i32| i32::from(SIN_Q15[i as usize]);
        if a <= 90 {
                v(a)
        } else if a <= 180 {
                v(180 - a)
        } else if a <= 270 {
                -v(a - 180)
        } else {
                -v(360 - a)
        }
}

/// Sine of a Q8 angle, linearly interpolated between whole degrees.
fn sin_deg_q8_q15(deg_q8: i32) -> i32 {
        let whole = deg_q8 >> 8;
        let frac = deg_q8 & 0xFF;
        let a = sin_deg_q15(whole);
        let b = sin_deg_q15(whole + 1);
        a + (((b - a) * frac) >> 8)
}

fn cos_deg_q8_q15(deg_q8: i32) -> i32 {
        sin_deg_q8_q15(deg_q8 + (90 << 8))
}

/// Newton's method on integers, a handful of iterations for anything a display holds.
/// Linear interpolation between two RGB565 colors at `num / den`, component-wise -- the
/// step gradients are built from. `num` is clamped into `0..=den`; a degenerate `den`
/// answers `from`.
pub fn lerp565(from: u16, to: u16, num: i32, den: i32) -> u16 {
        if den <= 0 {
                return from;
        }
        let num = num.clamp(0, den);
        let component = |shift: u16, mask: i32| {
                let a = i32::from(from >> shift) & mask;
                let b = i32::from(to >> shift) & mask;
                ((a + (b - a) * num / den) & mask) as u16
        };
        component(11, 0x1F) << 11 | component(5, 0x3F) << 5 | component(0, 0x1F)
}

fn isqrt(n: u32) -> u32 {
        if n == 0 {
                return 0;
        }
        let (mut x, mut y) = (n, n.div_ceil(2));
        while y < x {
                x = y;
                y = (x + n / x) / 2;
        }
        x
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::vec::Vec;

        fn mono(w: u16, h: u16) -> Vec<u8> {
                std::vec![0u8; PixelFormat::Mono1.buffer_len(w, h)]
        }

        fn ink(c: &Canvas<'_>) -> Vec<(i32, i32)> {
                let mut v = Vec::new();
                for y in 0..i32::from(c.height()) {
                        for x in 0..i32::from(c.width()) {
                                if c.get(x, y) != 0 {
                                        v.push((x, y));
                                }
                        }
                }
                v
        }

        #[test]
        fn mono_packs_leftmost_pixel_in_bit_zero() {
                let mut buf = mono(16, 2);
                let mut c = Canvas::new(&mut buf, PixelFormat::Mono1, 16, 2);
                c.set(0, 0, 1);
                c.set(9, 1, 1);
                assert_eq!(c.buffer()[0], 0x01);
                assert_eq!(c.buffer()[2], 0x00);
                assert_eq!(c.buffer()[3], 0x02, "x=9 is bit 1 of the second byte of row 1");
                assert_eq!(c.get(9, 1), 1);
                c.set(0, 0, 0);
                assert_eq!(c.buffer()[0], 0x00);
        }

        #[test]
        fn rotations_map_the_top_edge_where_they_say_and_invert_exactly() {
                let mut buf = std::vec![0u8; PixelFormat::Rgb565.buffer_len(4, 8)];
                let mut c = Canvas::new(&mut buf, PixelFormat::Rgb565, 4, 8);
                //   R90: logical is 8 wide x 4 tall; the logical top edge lands on the physical
                // right edge, so logical (0,0) is physical (3,0) and logical (7,0) is (3,7)
                c.set_rotation(Rotation::R90);
                assert_eq!((c.width(), c.height()), (8, 4));
                assert_eq!(c.transform().apply(0, 0), (3, 0));
                assert_eq!(c.transform().apply(7, 0), (3, 7));
                assert_eq!(c.transform().apply(0, 3), (0, 0));
                for r in [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270] {
                        for f in [Flip::None, Flip::Horizontal, Flip::Vertical, Flip::Both] {
                                c.set_rotation(r);
                                c.set_flip(f);
                                for y in 0..i32::from(c.height()) {
                                        for x in 0..i32::from(c.width()) {
                                                let (px, py) = c.transform().apply(x, y);
                                                assert!((0..4).contains(&px) && (0..8).contains(&py), "{r:?}/{f:?} maps ({x},{y}) off the buffer");
                                                assert_eq!(c.untransform_point(px, py), Point::new(x, y), "{r:?}/{f:?}");
                                        }
                                }
                        }
                }
        }

        #[test]
        fn transform_rect_gives_physical_bounds_under_rotation() {
                let mut buf = mono(8, 16);
                let mut c = Canvas::new(&mut buf, PixelFormat::Mono1, 8, 16);
                c.set_rotation(Rotation::R90);
                // logical 16x8; a logical box at the top-left becomes a physical box at the top-right
                let r = c.transform_rect(&Region::new(0, 0, 3, 1));
                assert_eq!(r, Region::new(6, 0, 7, 3));
        }

        #[test]
        fn everything_honours_the_clip_and_nothing_escapes_the_buffer() {
                let mut buf = mono(16, 16);
                let mut c = Canvas::new(&mut buf, PixelFormat::Mono1, 16, 16);
                c.set_clip(Region::new(4, 4, 11, 11));
                c.fg = 1;
                c.line(Point::new(-20, 8), Point::new(40, 8));
                c.circle(Point::new(8, 8), 7, true);
                c.rect(Point::new(0, 0), Point::new(15, 15), false);
                c.arc(Point::new(8, 8), 30, 0, 360);
                c.rect_rounded(Point::new(-5, -5), Point::new(20, 20), 6, corner::ALL, true);
                assert!(ink(&c).iter().all(|&(x, y)| (4..=11).contains(&x) && (4..=11).contains(&y)));
                c.clear_clip();
                assert_eq!(c.clip(), Region::full(16, 16));
        }

        #[test]
        fn a_disc_is_exactly_its_outline_filled() {
                let mut a = mono(32, 32);
                let mut outline = Canvas::new(&mut a, PixelFormat::Mono1, 32, 32);
                outline.fg = 1;
                outline.circle(Point::new(15, 15), 10, false);
                let mut b = mono(32, 32);
                let mut disc = Canvas::new(&mut b, PixelFormat::Mono1, 32, 32);
                disc.fg = 1;
                disc.circle(Point::new(15, 15), 10, true);
                // every outline pixel is in the disc, and the disc's extent equals the outline's
                for &(x, y) in &ink(&outline) {
                        assert_eq!(disc.get(x, y), 1, "({x},{y})");
                }
                let bound = |v: &Vec<(i32, i32)>| (v.iter().map(|p| p.0).min(), v.iter().map(|p| p.0).max(), v.iter().map(|p| p.1).min(), v.iter().map(|p| p.1).max());
                assert_eq!(bound(&ink(&outline)), bound(&ink(&disc)));
                assert_eq!(bound(&ink(&disc)), (Some(5), Some(25), Some(5), Some(25)));
        }

        #[test]
        fn a_rounded_rect_outline_is_closed() {
                let mut buf = mono(40, 24);
                let mut c = Canvas::new(&mut buf, PixelFormat::Mono1, 40, 24);
                c.fg = 1;
                c.rect_rounded(Point::new(2, 2), Point::new(37, 21), 6, corner::ALL, false);
                //   closed means: every ink pixel has an ink neighbour (8-connected) on each
                // side of it along the ring -- at least two ink neighbours
                let on = ink(&c);
                for &(x, y) in &on {
                        let n = (-1..=1).flat_map(|dy| (-1..=1).map(move |dx| (dx, dy))).filter(|&(dx, dy)| (dx, dy) != (0, 0) && c.get(x + dx, y + dy) != 0).count();
                        assert!(n >= 2, "gap at ({x},{y})");
                }
                //   the corners are cut: the very corner pixel is blank, the edge midpoints are ink
                assert_eq!(c.get(2, 2), 0);
                assert_eq!(c.get(20, 2), 1);
                assert_eq!(c.get(2, 12), 1);
        }

        #[test]
        fn text_paints_under_rotation_and_reports_its_logical_box() {
                let mut e = light_font::Encoder::new(8, 4, 3, 4);
                e.add(b'I', &[0x10, 0x10, 0x10, 0x10]).unwrap();
                let blob = e.encode();
                let font = Font::parse(&blob).unwrap();
                let mut buf = mono(8, 32);
                let mut c = Canvas::new(&mut buf, PixelFormat::Mono1, 8, 32);
                c.set_rotation(Rotation::R90); // logical 32x8
                c.fg = 1;
                c.bg = 0;
                let r = c.text_boxed(&font, Point::new(2, 1), "II").unwrap();
                assert_eq!(r, Region::new(2, 1, 17, 4));
                assert_eq!(c.get(5, 1), 1, "first bar at logical column 3 of its cell");
                assert_eq!(c.get(13, 3), 1, "second bar");
                assert_eq!(c.get(4, 1), 0, "background inside the cell painted");
                // and it physically landed rotated: logical (5,1) -> physical (6,5)
                assert_eq!(c.transform().apply(5, 1), (6, 5));
                assert_eq!(c.get_phys(6, 5), 1);
        }

        #[test]
        fn text_off_the_edge_clips_and_missing_glyphs_are_blank() {
                let mut e = light_font::Encoder::new(8, 4, 3, 4);
                e.add(b'X', &[0xFF, 0, 0, 0]).unwrap();
                let blob = e.encode();
                let font = Font::parse(&blob).unwrap();
                let mut buf = mono(12, 4);
                let mut c = Canvas::new(&mut buf, PixelFormat::Mono1, 12, 4);
                c.fg = 1;
                let r = c.text(&font, Point::new(8, 0), "?XX").unwrap();
                assert_eq!(r, Region::new(8, 0, 11, 3), "the second cell starts off-canvas");
                assert_eq!(ink(&c), Vec::<(i32, i32)>::new(), "'?' has no glyph; the X's are off the canvas");
        }

        #[test]
        fn fill_region_and_clear_use_the_named_colours() {
                let mut buf = std::vec![0u8; PixelFormat::Rgb565.buffer_len(4, 4)];
                let mut c = Canvas::new(&mut buf, PixelFormat::Rgb565, 4, 4);
                c.bg = 0x1234;
                c.clear();
                assert_eq!(c.get(3, 3), 0x1234);
                c.fill_region(&Region::new(1, 1, 2, 2), 0xABCD);
                assert_eq!(c.get(1, 1), 0xABCD);
                assert_eq!(c.get(0, 0), 0x1234);
        }
}
