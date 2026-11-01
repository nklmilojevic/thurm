//! `thurm-nvim [nvim args]`: starts the daemon in the background, then hands it this pane's
//! terminal and waits as its helper.

use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::{helper, tty};

/// Where the daemon keeps its sockets and log: short, as unix socket paths are.
pub fn base_path() -> PathBuf {
    std::env::temp_dir().join(format!("thurm-nvim-{}", std::process::id()))
}

pub fn with_ext(base: &std::path::Path, ext: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(ext);
    PathBuf::from(s)
}

pub fn run(nvim_args: &[String]) -> io::Result<()> {
    if !tty::is_tty(libc::STDIN_FILENO) {
        return Err(io::Error::other("run it in a terminal"));
    }
    // Each window opens a Thurm pane next to this one.
    if std::env::var_os(thurm_proto::ENV_PANE_ID).is_none() {
        return Err(io::Error::other("run it in a Thurm pane"));
    }
    let base = base_path();
    let log_path = with_ext(&base, ".log");
    let log = File::create(&log_path)?;
    let mut daemon = Command::new(std::env::current_exe()?)
        .arg("daemon")
        .arg(&base)
        .args(nvim_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()?;
    // One byte once it listens; nothing if it failed.
    let mut ready = [0u8; 1];
    let n = daemon
        .stdout
        .take()
        .map(|mut out| out.read(&mut ready).unwrap_or(0))
        .unwrap_or(0);
    if n != 1 {
        return Err(io::Error::other(format!(
            "the daemon didn't start; see {}",
            log_path.display()
        )));
    }
    // It outlives this process, which never waits for it.
    std::mem::forget(daemon);
    helper::run(&with_ext(&base, ".sock"), None)
}
