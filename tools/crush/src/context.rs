//! The context: fonts, displays and renders, as JSON files in a `.crush/` directory.
//!
//! File names and top-level keys follow the C implementation's (`font.json` with
//! `contextFonts`, and so on) so a context seeded from its template loads, and so the CMake
//! wrapper's dependency stamps keep pointing at real files. Object fields are this
//! implementation's own.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::log;
use crate::CmdResult;

pub const DEFAULT_PPI: f64 = 96.0;
pub const MM_PER_INCH: f64 = 25.4;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Font {
        pub name: String,
        /// Where the file(s) live: the font's own directory under the context.
        pub path: String,
        pub files: Vec<String>,
        pub target_file: usize,
        pub face_index: u32,
        pub source: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Display {
        pub name: String,
        pub res_h: u16,
        pub res_v: u16,
        pub dimension_h: f64,
        pub dimension_v: f64,
        pub ppi_h: f64,
        pub ppi_v: f64,
        pub pixel_depth: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Render {
        pub name: String,
        pub font: String,
        pub display: String,
        pub point_size: f64,
        pub pixel_size: u16,
        pub output: String,
}

/// One store file: `{ version, type, next_id, <key>: { name: object } }`.
#[derive(Serialize, Deserialize, Debug)]
struct Store<T> {
        version: u32,
        #[serde(rename = "type")]
        kind: String,
        next_id: u64,
        #[serde(flatten)]
        objects: BTreeMap<String, BTreeMap<String, T>>,
}

impl<T> Store<T> {
        fn empty(kind: &str, key: &str) -> Self {
                let mut objects = BTreeMap::new();
                objects.insert(key.to_string(), BTreeMap::new());
                Self { version: 0, kind: kind.into(), next_id: 1, objects }
        }

        fn entries(&self, key: &str) -> Option<&BTreeMap<String, T>> {
                self.objects.get(key)
        }

        fn entries_mut(&mut self, key: &str) -> &mut BTreeMap<String, T> {
                self.objects.entry(key.to_string()).or_default()
        }
}

pub struct Context {
        dir: PathBuf,
        fonts: Store<Font>,
        displays: Store<Display>,
        renders: Store<Render>,
        exists: bool,
}

const FONT_KEY: &str = "contextFonts";
const DISPLAY_KEY: &str = "contextDisplays";
const RENDER_KEY: &str = "contextRenders";

fn load_store<T: for<'de> Deserialize<'de>>(path: &Path, kind: &str, key: &str) -> Result<Store<T>, String> {
        if !path.exists() {
                return Ok(Store::empty(kind, key));
        }
        let text = fs::read_to_string(path).map_err(|e| format!("could not read '{}': {e}", path.display()))?;
        let store: Store<T> = serde_json::from_str(&text).map_err(|e| format!("json object decode failed for '{}': {e}", path.display()))?;
        if store.kind != kind {
                return Err(format!("'{}' holds an object store of type '{}' (expected '{kind}')", path.display(), store.kind));
        }
        Ok(store)
}

fn save_store<T: Serialize>(path: &Path, store: &Store<T>) -> Result<(), String> {
        let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
        fs::write(path, text + "\n").map_err(|e| format!("could not write '{}': {e}", path.display()))
}

impl Context {
        /// `$CRUSH_CONTEXT` if set and non-empty, else `.crush` in the working directory. A missing
        /// directory is not an error until something tries to store into it.
        pub fn open() -> Result<Self, String> {
                let dir = match std::env::var("CRUSH_CONTEXT") {
                        Ok(v) if !v.is_empty() => PathBuf::from(v),
                        _ => PathBuf::from(".crush"),
                };
                let exists = dir.join("context.json").exists();
                Ok(Self {
                        fonts: load_store(&dir.join("font.json"), "crush:font", FONT_KEY)?,
                        displays: load_store(&dir.join("display.json"), "crush:display", DISPLAY_KEY)?,
                        renders: load_store(&dir.join("render.json"), "crush:render", RENDER_KEY)?,
                        dir,
                        exists,
                })
        }

        pub fn dir(&self) -> &Path {
                &self.dir
        }

        pub fn create_new(&mut self) -> CmdResult {
                fs::create_dir_all(&self.dir).map_err(|e| format!("could not create '{}': {e}", self.dir.display()))?;
                let context = serde_json::json!({
                        "version": 0,
                        "type": "crush:context",
                        "contextObjects": {
                                "crush:display": "display.json",
                                "crush:font": "font.json",
                                "crush:render": "render.json"
                        }
                });
                fs::write(self.dir.join("context.json"), serde_json::to_string_pretty(&context).unwrap() + "\n")
                        .map_err(|e| format!("could not write context.json: {e}"))?;
                self.exists = true;
                self.save()?;
                log::info(&format!("created context at '{}'", self.dir.display()));
                Ok(())
        }

        /// Writes every store back. Called once at exit, so a session's changes land together.
        pub fn save(&self) -> Result<(), String> {
                if !self.exists {
                        // nothing was ever created here and nothing asked for it; leave the
                        // directory alone rather than sprinkle .crush/ wherever crush is run
                        return Ok(());
                }
                save_store(&self.dir.join("font.json"), &self.fonts)?;
                save_store(&self.dir.join("display.json"), &self.displays)?;
                save_store(&self.dir.join("render.json"), &self.renders)?;
                Ok(())
        }

        fn require(&self) -> Result<(), String> {
                if self.exists {
                        Ok(())
                } else {
                        Err(format!("no crush context at '{}' -- run 'crush context new' there first", self.dir.display()))
                }
        }

        // --- fonts --------------------------------------------------------------------------

        pub fn fonts(&self) -> Vec<&Font> {
                self.fonts.entries(FONT_KEY).map(|m| m.values().collect()).unwrap_or_default()
        }

        pub fn font(&self, name: &str) -> Option<&Font> {
                self.fonts.entries(FONT_KEY).and_then(|m| m.get(name))
        }

        pub fn font_file_path(&self, font: &Font) -> PathBuf {
                Path::new(&font.path).join(&font.files[font.target_file])
        }

        pub fn font_add(&mut self, local_file: &Path, face_index: u32) -> CmdResult {
                self.require()?;
                let name = local_file
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| format!("'{}' has no usable file name", local_file.display()))?
                        .to_string();
                //   opened before it is recorded: a font that FreeType cannot load is refused
                // here, where the message names the file, not later inside a render job
                let lib = freetype::Library::init().map_err(|e| format!("FreeType init failed: {e}"))?;
                lib.new_face(local_file, face_index as isize)
                        .map_err(|e| format!("could not load font file '{}': {e}", local_file.display()))?;
                let dir = self.dir.join("font").join(&name);
                fs::create_dir_all(&dir).map_err(|e| format!("could not create '{}': {e}", dir.display()))?;
                let stored = dir.join(&name);
                fs::copy(local_file, &stored).map_err(|e| format!("could not copy '{}' into the context: {e}", local_file.display()))?;
                let font = Font {
                        name: name.clone(),
                        path: dir.to_string_lossy().into_owned(),
                        files: vec![name.clone()],
                        target_file: 0,
                        face_index,
                        source: local_file.to_string_lossy().into_owned(),
                };
                self.fonts.entries_mut(FONT_KEY).insert(name, font);
                self.fonts.next_id += 1;
                log::info(&format!("loaded font file '{}' successfully", local_file.display()));
                Ok(())
        }

        pub fn font_info(&self, name: &str) -> CmdResult {
                let f = self.font(name).ok_or_else(|| format!("no font named '{name}'"))?;
                log::info(&format!("font '{}': {} (face {}), from '{}'", f.name, self.font_file_path(f).display(), f.face_index, f.source));
                Ok(())
        }

        pub fn font_remove(&mut self, name: &str) -> CmdResult {
                self.require()?;
                match self.fonts.entries_mut(FONT_KEY).remove(name) {
                        Some(f) => {
                                let _ = fs::remove_dir_all(&f.path);
                                log::info(&format!("removed font '{name}'"));
                                Ok(())
                        }
                        None => Err(format!("no font named '{name}'")),
                }
        }

        // --- displays -----------------------------------------------------------------------

        pub fn displays(&self) -> Vec<&Display> {
                self.displays.entries(DISPLAY_KEY).map(|m| m.values().collect()).unwrap_or_default()
        }

        pub fn display(&self, name: &str) -> Option<&Display> {
                self.displays.entries(DISPLAY_KEY).and_then(|m| m.get(name))
        }

        pub fn display_add(&mut self, name: &str, width: u16, height: u16, dimension: Option<&str>, pixel_depth: u8) -> CmdResult {
                self.require()?;
                if width == 0 || height == 0 {
                        return Err("a display needs a non-zero width and height".into());
                }
                let (dimension_h, dimension_v, ppi_h, ppi_v) = match dimension {
                        Some(d) => {
                                let (w, h) = parse_dimension(d)?;
                                (w, h, f64::from(width) * MM_PER_INCH / w, f64::from(height) * MM_PER_INCH / h)
                        }
                        None => {
                                log::warn(&format!("display '{name}' has no --dimension; assuming {DEFAULT_PPI} ppi, so point sizes will not match the glass"));
                                (
                                f64::from(width) * MM_PER_INCH / DEFAULT_PPI,
                                f64::from(height) * MM_PER_INCH / DEFAULT_PPI,
                                DEFAULT_PPI,
                                DEFAULT_PPI,
                        )
                        }
                };
                let d = Display { name: name.into(), res_h: width, res_v: height, dimension_h, dimension_v, ppi_h, ppi_v, pixel_depth };
                log::info(&format!("display '{name}': {width}x{height} px, {dimension_h:.1}x{dimension_v:.1} mm, {ppi_h:.1}x{ppi_v:.1} ppi, {pixel_depth} bpp"));
                self.displays.entries_mut(DISPLAY_KEY).insert(name.into(), d);
                self.displays.next_id += 1;
                Ok(())
        }

        pub fn display_info(&self, name: &str) -> CmdResult {
                let d = self.display(name).ok_or_else(|| format!("no display named '{name}'"))?;
                log::info(&format!("display '{}': {}x{} px, {:.1}x{:.1} mm, {:.2}x{:.2} ppi, {} bpp", d.name, d.res_h, d.res_v, d.dimension_h, d.dimension_v, d.ppi_h, d.ppi_v, d.pixel_depth));
                Ok(())
        }

        // --- renders ------------------------------------------------------------------------

        pub fn renders(&self) -> Vec<&Render> {
                self.renders.entries(RENDER_KEY).map(|m| m.values().collect()).unwrap_or_default()
        }

        pub fn render_new(&mut self, name: &str, point_size: f64, pixel_size: u16, font: &str, display: &str) -> CmdResult {
                self.require()?;
                let f = self.font(font).ok_or_else(|| format!("no font named '{font}' -- 'font add' it first"))?.clone();
                let d = self.display(display).ok_or_else(|| format!("no display named '{display}' -- 'display add' it first"))?.clone();
                let out_dir = self.dir.join("render").join(name);
                fs::create_dir_all(&out_dir).map_err(|e| format!("could not create '{}': {e}", out_dir.display()))?;
                let job = crate::render::Job {
                        font_file: self.font_file_path(&f),
                        font_name: f.name.clone(),
                        face_index: f.face_index,
                        display: d,
                        point_size,
                        pixel_size,
                        out_dir: out_dir.clone(),
                };
                let result = crate::render::run(&job).map_err(|e| format!("job '{name}' rendering failed: {e}"))?;
                log::info(&format!(
                        "job '{name}': {}px, cell {}x{}, {} glyphs -> {}",
                        result.pixel_size,
                        result.cell_width,
                        result.cell_height,
                        crate::render::CHAR_SET.len(),
                        result.lgf_path.display()
                ));
                let r = Render {
                        name: name.into(),
                        font: f.name,
                        display: display.into(),
                        point_size,
                        pixel_size: result.pixel_size,
                        output: result.lgf_path.to_string_lossy().into_owned(),
                };
                self.renders.entries_mut(RENDER_KEY).insert(name.into(), r);
                self.renders.next_id += 1;
                log::info(&format!("rendering complete for job '{name}'"));
                Ok(())
        }

        pub fn render_info(&self, name: &str) -> CmdResult {
                let r = self.renders.entries(RENDER_KEY).and_then(|m| m.get(name)).ok_or_else(|| format!("no render named '{name}'"))?;
                log::info(&format!("render '{}': font '{}' at {}pt / {}px on '{}' -> {}", r.name, r.font, r.point_size, r.pixel_size, r.display, r.output));
                Ok(())
        }
}

/// `WxH` in millimetres.
fn parse_dimension(s: &str) -> Result<(f64, f64), String> {
        let (w, h) = s.split_once(['x', 'X']).ok_or_else(|| format!("dimension '{s}' is not WxH"))?;
        let w: f64 = w.trim().parse().map_err(|_| format!("dimension width '{w}' is not a number"))?;
        let h: f64 = h.trim().parse().map_err(|_| format!("dimension height '{h}' is not a number"))?;
        if w <= 0.0 || h <= 0.0 {
                return Err(format!("dimension '{s}' must be positive"));
        }
        Ok((w, h))
}
