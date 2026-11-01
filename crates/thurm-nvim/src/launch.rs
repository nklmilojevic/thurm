//! `thurm-nvim [nvim args]`: starts the daemon in the background, then hands it this pane's
//! terminal and waits as its helper.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{helper, tty};

/// The internal subcommands, as the first argument: never a Neovim argument.
pub const DAEMON: &str = "--thurm-nvim-daemon";
pub const PANE: &str = "--thurm-nvim-pane";

/// A session's private directory: its helper socket, Neovim's socket and the log. Made fresh
/// (mode 0700, never an existing path) under the temp directory with a name nobody can guess,
/// as both sockets let whoever connects drive the editor.
pub fn session_dir() -> io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut seed = nanos ^ ((std::process::id() as u128) << 64);
    let mut random = [0u8; 8];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random))
        .is_ok()
    {
        seed ^= u64::from_ne_bytes(random) as u128;
    }
    let uid = unsafe { libc::getuid() };
    let dir = std::env::temp_dir().join(format!(
        "thurm-nvim-{uid}-{:x}",
        seed as u64 ^ (seed >> 64) as u64
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    if !thurm_config::private_dir_ok(&dir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not private", dir.display()),
        ));
    }
    Ok(dir)
}

pub fn helper_socket(dir: &Path) -> PathBuf {
    dir.join("sock")
}

pub fn nvim_socket(dir: &Path) -> PathBuf {
    dir.join("nvim")
}

pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("log")
}

pub fn run(nvim_args: &[String]) -> io::Result<()> {
    if !tty::is_tty(libc::STDIN_FILENO) {
        return Err(io::Error::other("run it in a terminal"));
    }
    // Each window opens a Thurm pane next to this one.
    if std::env::var_os(thurm_proto::ENV_PANE_ID).is_none() {
        return Err(io::Error::other("run it in a Thurm pane"));
    }
    let dir = session_dir()?;
    let log_path = log_path(&dir);
    let log: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log_path)?;
    let mut daemon = Command::new(std::env::current_exe()?)
        .arg(DAEMON)
        .arg(&dir)
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
    helper::run(&helper_socket(&dir), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn session_dirs_are_fresh_and_private() {
        let a = session_dir().unwrap();
        let b = session_dir().unwrap();
        assert_ne!(a, b);
        assert_eq!(
            std::fs::metadata(&a).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Never an existing directory: one made by someone else first is refused.
        assert!(std::fs::DirBuilder::new().mode(0o700).create(&a).is_err());
        std::fs::remove_dir(&a).unwrap();
        std::fs::remove_dir(&b).unwrap();
    }
}
