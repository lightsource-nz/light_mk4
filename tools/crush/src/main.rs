//! crush: font-crusher in Rust.
//!
//! The command surface is mk3's -- `font add`, `display add`, `render new`, `console` -- because
//! that is what `crush_add_font_target()` generates, what the 93 acceptance tests exercise, and
//! what people have in their shell history. What changes is the OUTPUT: a render now produces an
//! LGF blob (see `light-font`) beside the C pair mk3's consumers still link, so the two stacks can
//! share one crush during the migration.
//!
//! The context is a directory of JSON files, `.crush/` in the working directory (or
//! `$CRUSH_CONTEXT`), with the same file names and top-level shape as mk3's so an existing
//! context template still seeds it.

mod console;
mod context;
mod log;
mod render;
mod theme;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use context::Context;

#[derive(Parser, Debug)]
#[command(name = "crush", version, about = "renders TrueType fonts into bitmap fonts for light firmware", disable_help_subcommand = true)]
pub struct Cli {
        #[command(subcommand)]
        pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
        /// Fonts registered with this context
        Font {
                #[command(subcommand)]
                cmd: FontCmd,
        },
        /// Displays a font can be rendered against
        Display {
                #[command(subcommand)]
                cmd: DisplayCmd,
        },
        /// Renders: a font at a size against a display
        Render {
                #[command(subcommand)]
                cmd: RenderCmd,
        },
        /// The context itself
        Context {
                #[command(subcommand)]
                cmd: ContextCmd,
        },
        /// Look-and-feel definitions: compile a JSON theme to an LTH blob
        Theme {
                #[command(subcommand)]
                cmd: ThemeCmd,
        },
        /// Run commands from a script, a single --command, or an interactive prompt
        Console(ConsoleArgs),
}

#[derive(Subcommand, Debug)]
pub enum ThemeCmd {
        /// Compile a JSON theme into the binary blob the firmware embeds
        Compile {
                /// The theme source (JSON)
                input: PathBuf,
                /// Where the LTH blob goes
                output: PathBuf,
        },
}

#[derive(Subcommand, Debug)]
pub enum FontCmd {
        /// Register a font file with the context
        Add {
                /// A font file on this machine
                #[arg(long = "local-file")]
                local_file: PathBuf,
                /// Which face of a collection to use
                #[arg(long = "face-index", default_value_t = 0)]
                face_index: u32,
        },
        /// List registered fonts
        List,
        /// Describe one font
        Info { name: String },
        /// Forget a font
        Remove { name: String },
}

#[derive(Subcommand, Debug)]
pub enum DisplayCmd {
        /// Describe a display by resolution and, optionally, physical size
        Add {
                name: String,
                width: u16,
                height: u16,
                /// Physical size in millimetres, WxH, e.g. 27.9x32.6 -- sets the pixel density
                #[arg(long)]
                dimension: Option<String>,
                /// Bits per pixel
                #[arg(long = "pixel-depth", short = 'p', default_value_t = 1)]
                pixel_depth: u8,
        },
        List,
        Info { name: String },
}

#[derive(Subcommand, Debug)]
pub enum RenderCmd {
        /// Render a font at a size against a display
        New {
                name: String,
                /// Point size, resolved through the display's pixel density
                point_size: f64,
                /// Explicit vertical pixel size; 0 derives it from the point size
                #[arg(default_value_t = 0)]
                pixel_size: u16,
                #[arg(long)]
                font: String,
                #[arg(long)]
                display: String,
        },
        List,
        Info { name: String },
}

#[derive(Subcommand, Debug)]
pub enum ContextCmd {
        /// Create an empty context in the working directory
        New,
        /// Show where the context is and what it holds
        Info,
}

#[derive(Args, Debug)]
pub struct ConsoleArgs {
        /// A script of commands, one per line; stdin when omitted and not a terminal
        pub script: Option<PathBuf>,
        /// Run one command line and exit with its status
        #[arg(short = 'c', long = "command")]
        pub command: Option<String>,
        /// Read one line at a time from stdin, and keep going after failures
        #[arg(short = 'i', long = "interactive")]
        pub interactive: bool,
        /// Run every line of a script even after one fails; the exit status still reports it
        #[arg(long = "keep-going")]
        pub keep_going: bool,
}

/// What a command run inside the console reports: the console decides what to do with it.
pub type CmdResult = Result<(), String>;

fn main() -> ExitCode {
        let cli = Cli::parse();
        let mut ctx = match Context::open() {
                Ok(c) => c,
                Err(e) => {
                        log::error(&format!("{e}"));
                        return ExitCode::from(1);
                }
        };
        let result = match cli.command {
                Command::Console(args) => console::run(&mut ctx, &args),
                other => run_command(&mut ctx, other),
        };
        let saved = ctx.save();
        if let Err(e) = &saved {
                log::error(&format!("could not save the context: {e}"));
        }
        match (result, saved) {
                (Ok(()), Ok(())) => ExitCode::SUCCESS,
                (Err(e), _) => {
                        if !e.is_empty() {
                                log::error(&e);
                        }
                        ExitCode::from(1)
                }
                (_, Err(_)) => ExitCode::from(1),
        }
}

/// Runs one parsed command against the context. Shared by `main` and the console.
pub fn run_command(ctx: &mut Context, command: Command) -> CmdResult {
        match command {
                Command::Font { cmd } => match cmd {
                        FontCmd::Add { local_file, face_index } => ctx.font_add(&local_file, face_index),
                        FontCmd::List => {
                                log::debug("finished parsing command line, target command: 'crush font list'");
                                for f in ctx.fonts() {
                                        log::info(&format!("font '{}' ({} file(s), face {})", f.name, f.files.len(), f.face_index));
                                }
                                Ok(())
                        }
                        FontCmd::Info { name } => ctx.font_info(&name),
                        FontCmd::Remove { name } => ctx.font_remove(&name),
                },
                Command::Display { cmd } => match cmd {
                        DisplayCmd::Add { name, width, height, dimension, pixel_depth } => {
                                ctx.display_add(&name, width, height, dimension.as_deref(), pixel_depth)
                        }
                        DisplayCmd::List => {
                                log::debug("finished parsing command line, target command: 'crush display list'");
                                for d in ctx.displays() {
                                        log::info(&format!("display '{}' {}x{} @ {:.1}x{:.1} ppi, {} bpp", d.name, d.res_h, d.res_v, d.ppi_h, d.ppi_v, d.pixel_depth));
                                }
                                Ok(())
                        }
                        DisplayCmd::Info { name } => ctx.display_info(&name),
                },
                Command::Render { cmd } => match cmd {
                        RenderCmd::New { name, point_size, pixel_size, font, display } => {
                                ctx.render_new(&name, point_size, pixel_size, &font, &display)
                        }
                        RenderCmd::List => {
                                for r in ctx.renders() {
                                        log::info(&format!("render '{}': {} @ {}pt/{}px on {}", r.name, r.font, r.point_size, r.pixel_size, r.display));
                                }
                                Ok(())
                        }
                        RenderCmd::Info { name } => ctx.render_info(&name),
                },
                Command::Context { cmd } => match cmd {
                        ContextCmd::New => ctx.create_new(),
                        ContextCmd::Info => {
                                log::info(&format!("context at '{}': {} fonts, {} displays, {} renders", ctx.dir().display(), ctx.fonts().len(), ctx.displays().len(), ctx.renders().len()));
                                Ok(())
                        }
                },
                Command::Theme { cmd } => match cmd {
                        ThemeCmd::Compile { input, output } => theme::compile(&input, &output),
                },
                Command::Console(_) => Err("console cannot be nested".into()),
        }
}
