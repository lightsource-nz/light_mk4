//! The console: a script of command lines, one `--command`, or an interactive prompt.
//!
//! Behaviour pinned by the acceptance tests inherited from the C implementation: comments and
//! blank lines are not commands and are
//! not echoed; every command line is echoed as `crush> line` before its own output; a leading
//! `crush` token is tolerated; quoted arguments keep their spaces and lose their quotes, with no
//! backslash escapes (which is what lets a Windows path through); a script stops at the first
//! failure unless `--keep-going`, and the process exit status reports any failure either way;
//! `exit` ends a script early and successfully; `help [topic]` is a builtin that never fails;
//! an interactive session survives failures and exits 0; stdin that is not a terminal is a
//! script by another name.

use std::io::{self, BufRead, IsTerminal};

use clap::{CommandFactory, Parser};

use crate::context::Context;
use crate::{log, run_command, Cli, CmdResult, ConsoleArgs};

enum Line {
        /// Nothing to run: a comment or a blank line.
        Skip,
        Exit,
        Help(Vec<String>),
        Command(Vec<String>),
}

/// The C implementation's tokenizer rules: whitespace-separated, `"` groups and is stripped, no
/// escapes, `#` starts a comment outside quotes.
pub fn tokenize(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut have_token = false;
        for ch in line.chars() {
                match ch {
                        '"' => {
                                in_quotes = !in_quotes;
                                have_token = true;
                        }
                        '#' if !in_quotes => break,
                        c if c.is_whitespace() && !in_quotes => {
                                if have_token {
                                        out.push(std::mem::take(&mut cur));
                                        have_token = false;
                                }
                        }
                        c => {
                                cur.push(c);
                                have_token = true;
                        }
                }
        }
        if have_token {
                out.push(cur);
        }
        out
}

fn classify(line: &str) -> Line {
        let mut tokens = tokenize(line);
        if tokens.is_empty() {
                return Line::Skip;
        }
        if tokens[0] == "crush" {
                tokens.remove(0);
                if tokens.is_empty() {
                        return Line::Skip;
                }
        }
        match tokens[0].as_str() {
                "exit" | "quit" => Line::Exit,
                "help" => Line::Help(tokens[1..].to_vec()),
                _ => Line::Command(tokens),
        }
}

fn run_tokens(ctx: &mut Context, tokens: Vec<String>) -> CmdResult {
        let argv = std::iter::once("crush".to_string()).chain(tokens);
        match Cli::try_parse_from(argv) {
                Ok(cli) => run_command(ctx, cli.command),
                Err(e) => {
                        //   clap's own text names the option or subcommand; the log line is what a
                        // build reads, so it carries the same message
                        let text = e.to_string();
                        let first = text.lines().next().unwrap_or("could not parse the command line").trim();
                        Err(first.replace("error: ", ""))
                }
        }
}

fn help(topic: &[String]) {
        let mut cmd = Cli::command();
        if topic.is_empty() {
                println!("usage: crush <subcommand>\n");
                for sub in cmd.get_subcommands() {
                        println!("  {:<10} {}", sub.get_name(), sub.get_about().map(|a| a.to_string()).unwrap_or_default());
                }
                println!("\nconsole builtins: help [topic], exit");
                return;
        }
        let mut node = &mut cmd;
        for t in topic {
                match node.find_subcommand_mut(t) {
                        Some(n) => node = n,
                        None => {
                                log::error(&format!("no such command '{t}' under '{}'", topic.join(" ")));
                                return;
                        }
                }
        }
        println!("usage: crush {} <subcommand>\n", topic.join(" "));
        for sub in node.get_subcommands() {
                println!("  {:<10} {}", sub.get_name(), sub.get_about().map(|a| a.to_string()).unwrap_or_default());
        }
        if node.get_subcommands().next().is_none() {
                let _ = node.print_help();
        }
}

pub fn run(ctx: &mut Context, args: &ConsoleArgs) -> CmdResult {
        if let Some(cmd) = &args.command {
                return match classify(cmd) {
                        Line::Command(tokens) => run_tokens(ctx, tokens),
                        Line::Help(t) => {
                                help(&t);
                                Ok(())
                        }
                        _ => Ok(()),
                };
        }

        let stdin = io::stdin();
        let interactive = args.interactive || (args.script.is_none() && stdin.is_terminal());
        let reader: Box<dyn BufRead> = match &args.script {
                Some(path) => {
                        let f = std::fs::File::open(path).map_err(|e| format!("could not open script '{}': {e}", path.display()))?;
                        Box::new(io::BufReader::new(f))
                }
                None => Box::new(stdin.lock()),
        };

        if interactive {
                println!("crush {}\ntype 'help' for commands, 'exit' to leave.", env!("CARGO_PKG_VERSION"));
        }
        let mut failures = 0u32;
        let mut stopped_at_failure = false;
        for line in reader.lines() {
                let line = line.map_err(|e| format!("could not read the console input: {e}"))?;
                if interactive {
                        print!("crush> ");
                }
                let parsed = classify(&line);
                let result = match parsed {
                        Line::Skip => continue,
                        Line::Exit => {
                                if !interactive {
                                        println!("crush> {}", line.trim());
                                }
                                break;
                        }
                        Line::Help(t) => {
                                if !interactive {
                                        println!("crush> {}", line.trim());
                                }
                                help(&t);
                                Ok(())
                        }
                        Line::Command(tokens) => {
                                if !interactive {
                                        println!("crush> {}", line.trim());
                                }
                                run_tokens(ctx, tokens)
                        }
                };
                if let Err(e) = result {
                        log::error(&e);
                        failures += 1;
                        if !interactive && !args.keep_going {
                                log::error("stopping at the failed command (--keep-going to carry on)");
                                stopped_at_failure = true;
                                break;
                        }
                }
        }
        if interactive {
                log::info("console session ended");
                return Ok(());
        }
        if stopped_at_failure {
                return Err(String::new());
        }
        if failures > 0 {
                return Err(format!("{failures} command(s) failed"));
        }
        Ok(())
}

#[cfg(test)]
mod tests {
        use super::tokenize;

        #[test]
        fn quotes_group_and_vanish_and_hash_comments() {
                assert_eq!(tokenize(r#"font add --local-file "font dir/T.ttf" # note"#), ["font", "add", "--local-file", "font dir/T.ttf"]);
                assert_eq!(tokenize("   # only a comment"), Vec::<String>::new());
                assert_eq!(tokenize(r#"a "" b"#), ["a", "", "b"]);
                assert_eq!(tokenize(r#"p "C:\x y\z""#), ["p", r"C:\x y\z"]);
        }
}
