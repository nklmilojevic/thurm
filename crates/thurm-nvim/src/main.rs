//! `thurm-nvim [nvim args]`: Neovim with each of its windows in a Thurm pane.
//!
//! The launcher starts a daemon that runs Neovim headless, then hands it its pane's terminal.
//! Each window Neovim opens gets a Thurm split running `thurm-nvim pane`, which hands that
//! pane's terminal over too; the daemon draws every window into its pane.

mod cell;
mod daemon;
mod fdpass;
mod grid;
mod helper;
mod input;
mod launch;
#[cfg(test)]
mod live_tests;
mod nvim;
mod render;
mod tty;
mod view;

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        // Internal, behind flags no Neovim invocation starts with: a file named `daemon` or
        // `pane` is still a file to open.
        Some(launch::DAEMON) if args.len() >= 2 => {
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                .init();
            daemon::run(Path::new(&args[1]), &args[2..])
        }
        Some(launch::PANE) if (2..=3).contains(&args.len()) => {
            match args.get(2).map(|s| s.parse::<u32>()) {
                Some(Err(_)) => Err(std::io::Error::other("not a pane id")),
                id => helper::run(Path::new(&args[1]), id.map(Result::unwrap)),
            }
        }
        Some("--help" | "-h") if args.len() == 1 => {
            println!(
                "usage: thurm-nvim [nvim arguments]\n\nRuns Neovim with each of its windows in a Thurm pane."
            );
            Ok(())
        }
        _ => launch::run(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("thurm-nvim: {e}");
            ExitCode::FAILURE
        }
    }
}
