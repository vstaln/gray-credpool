//! `gray-credpool` binary.
//!
//! Invocation vectors:
//! - bare (stdin piped) — the sidecar; this is what `gray plugin check`
//!   and the installed lock argv spawn (`resolve_argv` returns the bare
//!   path).
//! - `sidecar`          — explicit sidecar (manifest `sidecar_args`).
//! - `manifest`         — print the manifest JSON (install probe).
//! - anything else      — the `gray credpool …` CLI surface.

use clap::Parser;
use gray_credpool::cli::Cli;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdin_is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    match args.first().map(String::as_str) {
        Some("sidecar") => gray_credpool::sidecar::run(),
        Some("manifest") => println!("{}", gray_credpool::manifest::manifest()),
        None if !stdin_is_tty => gray_credpool::sidecar::run(),
        None => {
            let _ = Cli::try_parse_from(["credpool", "--help"]);
            eprintln!("usage: gray credpool <verb> — see `gray credpool help`");
        }
        _ => match Cli::try_parse() {
            Ok(cli) => match gray_credpool::cli::run(cli) {
                Ok(text) => {
                    if !text.is_empty() {
                        println!("{text}");
                    }
                }
                Err(e) => {
                    eprintln!("credpool: {e}");
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprint!("{e}");
                std::process::exit(if e.use_stderr() { 1 } else { 0 });
            }
        },
    }
}
