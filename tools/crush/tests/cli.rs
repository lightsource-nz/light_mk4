//! The acceptance suite: the C font-crusher's CTest cases, re-expressed against the Rust
//! crush.
//!
//! Every case runs the real binary in its own context directory, the way a build does. The
//! assertions are the C suite's -- the log lines a build reads, the exit codes, the echo rules
//! of the console, the glyph-level checks on the render -- plus what the C suite could not
//! check: that the LGF blob parses, and that the glyphs are byte-for-byte what the C crush
//! renders (when a C crush is available to compare against).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn crush() -> PathBuf {
        PathBuf::from(env!("CARGO_BIN_EXE_crush"))
}

fn resources() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources")
}

fn font_path() -> PathBuf {
        resources().join("fonts/TypeLightSans.ttf")
}

/// A fresh context directory per test, under the target dir so a failed run leaves evidence.
struct Rig {
        dir: PathBuf,
}

impl Rig {
        fn new(name: &str) -> Self {
                let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("crush-{name}"));
                let _ = fs::remove_dir_all(&dir);
                fs::create_dir_all(&dir).unwrap();
                let rig = Self { dir };
                rig.ok(&["context", "new"]);
                rig
        }

        fn run(&self, args: &[&str]) -> Output {
                self.run_with_stdin(args, None)
        }

        fn run_with_stdin(&self, args: &[&str], stdin: Option<&Path>) -> Output {
                let mut cmd = Command::new(crush());
                cmd.args(args).current_dir(&self.dir).env("CRUSH_CONTEXT", "");
                match stdin {
                        Some(p) => {
                                cmd.stdin(fs::File::open(p).unwrap());
                        }
                        None => {
                                cmd.stdin(Stdio::null());
                        }
                }
                cmd.output().unwrap()
        }

        fn ok(&self, args: &[&str]) -> String {
                let out = self.run(args);
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                assert!(out.status.success(), "crush {args:?} failed:\n{text}{}", String::from_utf8_lossy(&out.stderr));
                text
        }

        fn add_font(&self) {
                let text = self.ok(&["font", "add", "--local-file", font_path().to_str().unwrap()]);
                assert!(text.contains("loaded font file '") && text.contains("TypeLightSans.ttf' successfully"), "{text}");
        }

        fn render_dir(&self, name: &str) -> PathBuf {
                self.dir.join(".crush/render").join(name)
        }
}

fn code(out: &Output) -> i32 {
        out.status.code().unwrap_or(-1)
}

fn text(out: &Output) -> String {
        String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `.char_width = N,` from a generated C source.
fn c_field(source: &str, field: &str) -> u32 {
        let key = format!(".{field} = ");
        let start = source.find(&key).unwrap_or_else(|| panic!("no '{key}' in the generated source")) + key.len();
        source[start..].split(',').next().unwrap().trim().parse().unwrap()
}

fn c_glyph(source: &str, code: u8) -> Vec<u8> {
        let key = format!("glyph_0x{code:02x}[] = {{");
        let start = source.find(&key).unwrap_or_else(|| panic!("no glyph 0x{code:02x}")) + key.len();
        let end = source[start..].find('}').unwrap();
        source[start..start + end]
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| u8::from_str_radix(s.trim_start_matches("0x"), 16).unwrap())
                .collect()
}

// --- render -------------------------------------------------------------------------------

#[test]
fn render_new_writes_a_real_font_in_both_forms() {
        let rig = Rig::new("render");
        rig.add_font();
        rig.ok(&["display", "add", "test-display", "64", "128"]);
        // 19 is the y_ppem FreeType resolves 14pt at 96 ppi to for this font
        let out = rig.ok(&["render", "new", "test_render", "14", "19", "--font", "TypeLightSans.ttf", "--display", "test-display"]);
        assert!(out.contains("rendering complete for job"), "{out}");
        assert!(!out.contains("[  ERROR]"), "{out}");

        let dir = rig.render_dir("test_render");
        let h = fs::read_to_string(dir.join("TypeLightSans_ttf_19px_font.h")).unwrap();
        assert!(h.contains("extern const light_draw_font_t TypeLightSans_ttf_19px_font;"));
        let c = fs::read_to_string(dir.join("TypeLightSans_ttf_19px_font.c")).unwrap();
        assert!(c.contains("glyph_table["));
        assert!(c.len() > 200, "suspiciously small: {}", c.len());
        // visually distinct characters must render differently (a font/pipeline bug once
        // rendered every glyph identically)
        assert_ne!(c_glyph(&c, b'a'), c_glyph(&c, b'A'));

        let blob = fs::read(dir.join("TypeLightSans_ttf_19px_font.lgf")).unwrap();
        let font = light_font::Font::parse(&blob).expect("the blob parses");
        assert_eq!(u32::from(font.cell_width()), c_field(&c, "char_width"));
        assert_eq!(u32::from(font.cell_height()), c_field(&c, "char_height"));
        assert_eq!(font.pixel_size(), 19);
        assert_eq!(font.glyph_count(), 94);
        assert_eq!(font.glyph(b'a').unwrap(), c_glyph(&c, b'a').as_slice(), "the blob and the C pair carry the same bytes");
        assert!(font.glyph(b' ').is_none());
        // ink exists and sits inside the cell
        assert!((0..font.cell_height()).any(|y| (0..font.cell_width()).any(|x| font.pixel(b'A', x, y))));
}

#[test]
fn non_square_ppi_widens_the_cell_without_changing_its_height() {
        let rig = Rig::new("skewed");
        rig.add_font();
        rig.ok(&["display", "add", "test-display", "64", "128"]);
        // a square 1x1 inch panel with a 2:1 pixel grid: ppi_h is exactly double ppi_v
        rig.ok(&["display", "add", "test-display-skewed", "192", "96", "--dimension", "25.4x25.4"]);
        rig.ok(&["render", "new", "base", "14", "19", "--font", "TypeLightSans.ttf", "--display", "test-display"]);
        rig.ok(&["render", "new", "skewed", "14", "19", "--font", "TypeLightSans.ttf", "--display", "test-display-skewed"]);
        let base = fs::read_to_string(rig.render_dir("base").join("TypeLightSans_ttf_19px_font.c")).unwrap();
        let skew = fs::read_to_string(rig.render_dir("skewed").join("TypeLightSans_ttf_19px_font.c")).unwrap();
        let (bw, bh) = (c_field(&base, "char_width"), c_field(&base, "char_height"));
        let (sw, sh) = (c_field(&skew, "char_width"), c_field(&skew, "char_height"));
        assert!(sw * 10 >= bw * 13, "skewed width {sw} not meaningfully wider than {bw}");
        assert_eq!(sh, bh);
}

#[test]
fn a_real_panel_with_slightly_unequal_ppi_renders() {
        let rig = Rig::new("po13");
        rig.add_font();
        // the Pico-OLED-1.3: 64x128 over 17.2x32.3 mm, ppi_h ~94.5 and ppi_v ~100.7
        rig.ok(&["display", "add", "po13", "64", "128", "--dimension", "17.2x32.3"]);
        let out = rig.ok(&["render", "new", "po13r", "14", "16", "--font", "TypeLightSans.ttf", "--display", "po13"]);
        assert!(out.contains("rendering complete for job"));
        let c = fs::read_to_string(rig.render_dir("po13r").join("TypeLightSans_ttf_16px_font.c")).unwrap();
        assert_ne!(c_glyph(&c, b'a'), c_glyph(&c, b'A'));
}

#[test]
fn a_point_size_alone_resolves_through_the_display_density() {
        let rig = Rig::new("points");
        rig.add_font();
        rig.ok(&["display", "add", "d", "64", "128"]);
        rig.ok(&["render", "new", "r", "14", "--font", "TypeLightSans.ttf", "--display", "d"]);
        // 14 pt at 96 ppi is 18.67 px; FreeType rounds to 19 for this face
        assert!(rig.render_dir("r").join("TypeLightSans_ttf_19px_font.lgf").exists());
}

#[test]
fn rendering_needs_a_registered_font_and_display() {
        let rig = Rig::new("missing");
        let out = rig.run(&["render", "new", "r", "14", "19", "--font", "nope.ttf", "--display", "d"]);
        assert_eq!(code(&out), 1);
        assert!(text(&out).contains("[  ERROR]"));
}

/// Byte-for-byte against the C crush, when one is on this machine. The C build is not part of
/// this workspace, so the test is skipped -- loudly -- rather than failed when it is absent.
#[test]
fn glyphs_match_the_c_crush_byte_for_byte() {
        let c_crush = std::env::var("CRUSH_C_BINARY")
                .map(PathBuf::from)
                .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../font-crusher/build/bin/crush.exe"));
        let template = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../font-crusher/test_resource/test_context_default");
        if !c_crush.exists() || !template.exists() {
                eprintln!("SKIPPED: no C crush at {} (set CRUSH_C_BINARY)", c_crush.display());
                return;
        }
        let rig = Rig::new("parity-rs");
        rig.add_font();
        rig.ok(&["display", "add", "test-display", "64", "128"]);
        rig.ok(&["render", "new", "r", "14", "19", "--font", "TypeLightSans.ttf", "--display", "test-display"]);
        let rust_c = fs::read_to_string(rig.render_dir("r").join("TypeLightSans_ttf_19px_font.c")).unwrap();

        let cdir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("crush-parity-c");
        let _ = fs::remove_dir_all(&cdir);
        fs::create_dir_all(cdir.join(".crush")).unwrap();
        for entry in fs::read_dir(&template).unwrap() {
                let entry = entry.unwrap();
                fs::copy(entry.path(), cdir.join(".crush").join(entry.file_name())).unwrap();
        }
        let run = |args: &[&str]| {
                let out = Command::new(&c_crush).args(args).current_dir(&cdir).env("CRUSH_CONTEXT", "").output().unwrap();
                assert!(out.status.success(), "C crush {args:?} failed");
        };
        run(&["font", "add", "--local-file", font_path().to_str().unwrap()]);
        run(&["display", "add", "test-display", "64", "128"]);
        run(&["render", "new", "r", "14", "19", "--font", "TypeLightSans.ttf", "--display", "test-display"]);
        let c_c = fs::read_to_string(cdir.join(".crush/render/r/TypeLightSans_ttf_19px_font.c")).unwrap();
        for code in b'!'..=b'~' {
                if code == b' ' {
                        continue;
                }
                assert_eq!(c_glyph(&rust_c, code), c_glyph(&c_c, code), "glyph 0x{code:02x} differs from the C crush");
        }
        assert_eq!(rust_c, c_c, "the generated C source is identical");
}

// --- console ------------------------------------------------------------------------------

fn script(name: &str) -> PathBuf {
        resources().join("console").join(name)
}

/// Stages the font under a directory with a space in its name, as the C suite does.
fn stage_font(rig: &Rig) {
        let dir = rig.dir.join("font dir");
        fs::create_dir_all(&dir).unwrap();
        fs::copy(font_path(), dir.join("TypeLightSans.ttf")).unwrap();
}

#[test]
fn command_option_runs_one_line_and_reports_its_status() {
        let rig = Rig::new("console-c");
        let out = rig.run(&["console", "-c", "font list"]);
        assert_eq!(code(&out), 0);
        assert!(text(&out).contains("target command: 'crush font list'"));

        // a command that RAN and said no
        let out = rig.run(&["console", "-c", "font info"]);
        assert_eq!(code(&out), 1);
        assert!(text(&out).contains("[  ERROR]"), "{}", text(&out));

        // a line that will not parse fails identically as far as a build is concerned
        let out = rig.run(&["console", "-c", "font list --nosuch"]);
        assert_eq!(code(&out), 1);
        assert!(text(&out).contains("nosuch"), "{}", text(&out));
}

#[test]
fn a_script_echoes_commands_only_and_keeps_quoted_spaces() {
        let rig = Rig::new("console-script");
        stage_font(&rig);
        let out = rig.run(&["console", script("basic.crush").to_str().unwrap()]);
        let t = text(&out);
        assert_eq!(code(&out), 0, "{t}");
        let order = ["crush> font list", "crush> crush font list", "crush> font add --local-file"];
        let mut pos = 0;
        for needle in order {
                let at = t[pos..].find(needle).unwrap_or_else(|| panic!("'{needle}' missing or out of order in:\n{t}"));
                pos += at + needle.len();
        }
        assert!(t.contains("TypeLightSans.ttf' successfully"), "{t}");
        assert!(!t.contains("crush> #"));
        assert_eq!(t.matches("crush>").count(), 3, "exactly the three command lines are echoed:\n{t}");
}

#[test]
fn a_script_stops_at_the_first_failure_unless_told_to_keep_going() {
        let rig = Rig::new("console-stops");
        let out = rig.run(&["console", script("stops.crush").to_str().unwrap()]);
        let t = text(&out);
        assert_eq!(code(&out), 1);
        assert!(t.contains("stopping at the failed command"), "{t}");
        assert!(!t.contains("crush> font info tail-marker"), "{t}");

        let out = rig.run(&["console", "--keep-going", script("stops.crush").to_str().unwrap()]);
        let t = text(&out);
        assert_eq!(code(&out), 1);
        assert!(t.contains("crush> font info tail-marker"), "{t}");
        assert!(t.contains("command(s) failed"), "{t}");
        assert!(!t.contains("stopping at the failed command"));
}

#[test]
fn exit_ends_a_script_early_and_successfully() {
        let rig = Rig::new("console-exit");
        let out = rig.run(&["console", script("exits.crush").to_str().unwrap()]);
        let t = text(&out);
        assert_eq!(code(&out), 0, "{t}");
        assert!(t.contains("crush> exit"));
        assert!(!t.contains("crush> font info tail-marker"));
}

#[test]
fn a_missing_script_is_the_callers_mistake() {
        let rig = Rig::new("console-missing");
        let out = rig.run(&["console", rig.dir.join("no-such-script.crush").to_str().unwrap()]);
        assert_eq!(code(&out), 1);
        assert!(text(&out).contains("could not open script"), "{}", text(&out));
}

#[test]
fn piped_stdin_behaves_as_a_script() {
        let rig = Rig::new("console-stdin");
        let out = rig.run_with_stdin(&["console"], Some(&script("stops.crush")));
        let t = text(&out);
        assert_eq!(code(&out), 1, "{t}");
        assert!(t.contains("crush> font list"));
        assert!(!t.contains("crush> font info tail-marker"));
}

#[test]
fn help_is_a_builtin_that_never_fails_the_script() {
        let rig = Rig::new("console-help");
        let out = rig.run(&["console", script("help.crush").to_str().unwrap()]);
        let t = text(&out);
        assert_eq!(code(&out), 0, "{t}");
        assert!(t.contains("usage: crush <subcommand>"), "{t}");
        assert!(t.contains("usage: crush font <subcommand>"), "{t}");
        assert!(t.contains("no such command 'nosuch'"), "{t}");
}

#[test]
fn an_interactive_session_runs_every_line_and_survives_failures() {
        let rig = Rig::new("console-interactive");
        let out = rig.run_with_stdin(&["console", "--interactive"], Some(&script("session.crush")));
        let t = text(&out);
        assert_eq!(code(&out), 0, "{t}");
        assert!(t.contains("type 'help' for commands"));
        assert!(t.contains("crush>"));
        assert!(t.contains("console session ended"));
        assert_eq!(t.matches("[  ERROR]").count(), 3, "three failing lines, three errors:\n{t}");

        let out = rig.run_with_stdin(&["console", "-i"], Some(&script("session.crush")));
        assert_eq!(code(&out), 0);
}

#[test]
fn display_list_dispatches_as_a_command() {
        let rig = Rig::new("console-display-list");
        let out = rig.run(&["console", "-c", "display list"]);
        let t = text(&out);
        assert_eq!(code(&out), 0, "{t}");
        assert!(t.contains("target command: 'crush display list'"), "{t}");
        assert!(!t.contains("excess arguments"));
}

#[test]
fn the_font_target_pipeline_runs_in_one_session() {
        let rig = Rig::new("console-pipeline");
        stage_font(&rig);
        let out = rig.run(&["console", script("pipeline.crush").to_str().unwrap()]);
        let t = text(&out);
        assert_eq!(code(&out), 0, "{t}");
        for needle in ["crush> font add", "crush> display add pipeline_disp", "crush> render new pipeline_render"] {
                assert!(t.contains(needle), "{needle} missing:\n{t}");
        }
        assert!(!t.contains("rendering failed"));
        assert!(!t.contains("json object decode failed"));
        assert!(rig.render_dir("pipeline_render").join("TypeLightSans_ttf_8px_font.lgf").exists());
}

// --- context ------------------------------------------------------------------------------

#[test]
fn the_context_persists_across_processes() {
        let rig = Rig::new("context");
        rig.add_font();
        rig.ok(&["display", "add", "d", "10", "10"]);
        let t = rig.ok(&["font", "list"]);
        assert!(t.contains("TypeLightSans.ttf"), "{t}");
        let t = rig.ok(&["display", "info", "d"]);
        assert!(t.contains("10x10 px"), "{t}");
        for f in ["context.json", "font.json", "display.json", "render.json"] {
                assert!(rig.dir.join(".crush").join(f).exists(), "{f}");
        }
        rig.ok(&["font", "remove", "TypeLightSans.ttf"]);
        let out = rig.run(&["font", "info", "TypeLightSans.ttf"]);
        assert_eq!(code(&out), 1);
}

#[test]
fn without_a_context_nothing_is_created_by_accident() {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("crush-no-context");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let out = Command::new(crush()).args(["font", "list"]).current_dir(&dir).env("CRUSH_CONTEXT", "").output().unwrap();
        assert!(out.status.success());
        assert!(!dir.join(".crush").exists(), "listing must not create a context");
        let out = Command::new(crush()).args(["display", "add", "d", "1", "1"]).current_dir(&dir).env("CRUSH_CONTEXT", "").output().unwrap();
        assert_eq!(code(&out), 1);
        assert!(text(&out).contains("no crush context"));
}
