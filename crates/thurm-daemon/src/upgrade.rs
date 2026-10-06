//! In-place upgrade: on SIGUSR2 the daemon replaces its own image with the new `thurmd` (named
//! in `<socket>.upgrade`, else the executable it was started from, which an app update has
//! replaced). Nothing a pane runs notices:
//!
//! * `exec` keeps the pid, so every shell stays our child (`waitpid` still works);
//! * the PTY masters and the listening socket are inherited (their `FD_CLOEXEC` is cleared
//!   first), so the programs keep their terminals and clients reconnect to the same socket;
//! * each terminal's complete state ([`Terminal::serialize_state`]) goes through a hand-off
//!   file, and the new image rebuilds the terminals from it before accepting clients.
//!
//! Before exec'ing, the old image asks the new binary (`--handoff-check`) which hand-off
//! versions it reads; a binary that can't answer is never exec'd, and the daemon keeps running.

use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use serde::{Deserialize, Serialize};
use thurm_proto::PaneId;

use crate::daemon::Daemon;
use crate::persist::PaneSizeSnap;
use crate::pty;

/// Bumped when [`Handoff`] changes incompatibly. A new daemon reads every version up to its own.
pub const HANDOFF_VERSION: u32 = 1;

/// How long the old image waits for a reader to stop, or for a parser that makes no progress.
const QUIESCE_STALL: std::time::Duration = std::time::Duration::from_secs(3);
/// The most it waits for its parsers to catch up (a debug build parses slowly).
const QUIESCE_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Serialize, Deserialize, Debug)]
pub struct Handoff {
    pub version: u32,
    pub listener_fd: RawFd,
    /// The single-instance lock (`<socket>.lock`), held across the exec.
    #[serde(default)]
    pub lock_fd: Option<RawFd>,
    /// The old image logged to the state dir's log file (it was daemonized), not stderr.
    pub log_to_file: bool,
    pub layout: Option<String>,
    pub next_pane_id: PaneId,
    pub restored: bool,
    pub panes: Vec<PaneHandoff>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PaneHandoff {
    pub id: PaneId,
    pub fd: RawFd,
    pub pid: u32,
    pub size: PaneSizeSnap,
    pub title: String,
    pub cwd: Option<String>,
    pub command: Option<Vec<String>>,
    pub hold: bool,
    pub restored: bool,
    /// `Some(code)` once the process has exited (a held pane).
    pub exited: Option<Option<i32>>,
    pub osc_cwd: Option<String>,
    pub shell_integration_seen: bool,
    /// Base64 of the terminal's serialized state.
    pub state: String,
    /// Base64 of the unterminated end of the output the old image parsed, fed to the new
    /// terminal after `state` (empty from images that didn't write it).
    #[serde(default)]
    pub pending: String,
    /// The token the shell integration sends with `$PATH` (from images that wrote it).
    #[serde(default)]
    pub shell_token: Option<String>,
    /// The `$PATH` the shell reported (it only reports it again when it changes).
    #[serde(default)]
    pub shell_path: Option<String>,
    /// Ordered public reporting state for agent processes that remain alive.
    #[serde(default)]
    pub agent_report: Option<crate::agents::reporting::ReportHandoff>,
    /// A submitted prompt that has not started a turn.
    #[serde(default)]
    pub agent_prompt: Option<crate::agents::prompt_handoff::PendingPromptHandoff>,
    /// Restore this size because exec closes the attached terminal connection.
    #[serde(default)]
    pub terminal_size_after_detach: Option<PaneSizeSnap>,
    /// What programs reported about themselves (OSC 7501).
    #[serde(default)]
    pub programs: Vec<thurm_proto::ProgramRecord>,
}

impl PaneHandoff {
    pub fn state_bytes(&self) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.state)
            .unwrap_or_default()
    }

    pub fn pending_bytes(&self) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.pending)
            .unwrap_or_default()
    }

    pub fn encode_state(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}

/// `<socket>.upgrade`: the path of the `thurmd` to become, written by whoever sends SIGUSR2.
pub fn request_path(socket: &Path) -> PathBuf {
    let mut p = socket.as_os_str().to_owned();
    p.push(".upgrade");
    p.into()
}

fn handoff_path(state_dir: &Path) -> PathBuf {
    state_dir.join(format!("handoff-{}.json", std::process::id()))
}

/// Output of `thurmd --handoff-check`: the newest hand-off version this binary reads.
pub fn print_check() {
    println!(
        "handoff {HANDOFF_VERSION} protocol {}",
        thurm_proto::PROTOCOL_VERSION
    );
}

/// The binary to become: the requested one, else our own path (updated in place by the app).
fn target(socket: &Path) -> PathBuf {
    let req = request_path(socket);
    let named = std::fs::read_to_string(&req)
        .ok()
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty());
    let _ = std::fs::remove_file(&req);
    named
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("thurmd"))
}

/// Asks `exe` which hand-off versions it reads; an error when it can't take ours.
fn check(exe: &Path) -> Result<(), String> {
    // A binary that hangs must not hold up the daemon (the upgrade runs on its main thread).
    let mut child = crate::pty::spawn_locked(
        std::process::Command::new(exe)
            .arg("--handoff-check")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
    )
    .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    // Its output is read on a thread: a process it left behind could keep the pipe open.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut out = Vec::new();
            let _ = (&mut stdout).take(64 * 1024).read_to_end(&mut out);
            let _ = tx.send(out);
        });
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} --handoff-check did not answer", exe.display()));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let stdout = rx
        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        .map_err(|_| format!("{} --handoff-check did not answer", exe.display()))?;
    let out = std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let theirs = text
        .split_whitespace()
        .skip_while(|w| *w != "handoff")
        .nth(1)
        .and_then(|v| v.parse::<u32>().ok());
    match theirs {
        _ if !out.status.success() => Err(format!("{} --handoff-check failed", exe.display())),
        Some(v) if v >= HANDOFF_VERSION => Ok(()),
        Some(v) => Err(format!(
            "{} reads hand-off version {v}, we write {HANDOFF_VERSION}",
            exe.display()
        )),
        None => Err(format!(
            "{} does not support in-place upgrades",
            exe.display()
        )),
    }
}

/// Replaces this process with the new daemon. Returns only when that was not possible, with
/// everything restored as it was.
pub fn perform(
    daemon: &Arc<Daemon>,
    listener_fd: RawFd,
    lock_fd: Option<RawFd>,
    socket: &Path,
    state_dir: &Path,
    log_to_file: bool,
) {
    let exe = target(socket);
    if let Err(e) = check(&exe) {
        log::warn!("upgrade refused: {e}");
        return;
    }
    // Stop reading the PTYs and parse what was read: the terminals are then serialized with
    // nothing lost, and the output that follows stays in the kernel for the new image. Readers
    // resume if we return (the upgrade didn't happen).
    struct Resume<'a>(&'a Daemon);
    impl Drop for Resume<'_> {
        fn drop(&mut self) {
            self.0.resume_readers();
        }
    }
    let _resume = Resume(daemon);
    if !daemon.quiesce_readers(QUIESCE_STALL, QUIESCE_LIMIT) {
        // Going on would drop what they read; the next upgrade request can try again.
        log::warn!("upgrade aborted: pane readers did not stop in time");
        return;
    }
    // If the new image dies before adopting the panes, their shells are gone, but the session
    // (layout, scrollback, cwd) comes back from this snapshot on the next start.
    if let Err(e) = daemon.save_session() {
        // The handoff carries the panes; the snapshot is only the fallback after a crash.
        log::warn!("upgrade: session snapshot not saved: {e}");
    }
    let mut handoff = daemon.handoff(listener_fd, log_to_file);
    handoff.lock_fd = lock_fd;
    let path = handoff_path(state_dir);
    let written = serde_json::to_vec(&handoff)
        .map_err(std::io::Error::other)
        .and_then(|json| write_private(&path, &json));
    if let Err(e) = written {
        log::warn!("upgrade aborted: cannot write {}: {e}", path.display());
        return;
    }
    let fds: Vec<RawFd> = std::iter::once(listener_fd)
        .chain(lock_fd)
        .chain(handoff.panes.iter().map(|p| p.fd))
        .collect();
    for &fd in &fds {
        if let Err(e) = pty::set_cloexec(fd, false) {
            log::warn!("upgrade aborted: fd {fd}: {e}");
            restore_cloexec(&fds);
            let _ = std::fs::remove_file(&path);
            return;
        }
    }
    log::info!(
        "upgrading in place to {} ({} panes)",
        exe.display(),
        handoff.panes.len()
    );
    let mut args = vec![
        "--adopt".to_owned(),
        path.display().to_string(),
        "--socket".to_owned(),
        socket.display().to_string(),
    ];
    if !log_to_file {
        args.push("--foreground".to_owned());
    }
    let err = exec(&exe, &args);
    log::warn!("upgrade failed: exec {}: {err}", exe.display());
    restore_cloexec(&fds);
    let _ = std::fs::remove_file(&path);
}

fn restore_cloexec(fds: &[RawFd]) {
    for &fd in fds {
        let _ = pty::set_cloexec(fd, true);
    }
}

fn exec(exe: &Path, args: &[String]) -> std::io::Error {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(prog) = CString::new(exe.as_os_str().as_bytes()) else {
        return std::io::Error::other("path contains NUL");
    };
    let mut argv: Vec<CString> = vec![prog.clone()];
    argv.extend(args.iter().filter_map(|a| CString::new(a.as_str()).ok()));
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { libc::execv(prog.as_ptr(), ptrs.as_ptr()) };
    std::io::Error::last_os_error()
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)
}

/// Reads (and deletes) the hand-off file given to `--adopt`.
pub fn load(path: &Path) -> Result<Handoff, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let _ = std::fs::remove_file(path);
    let h: Handoff =
        serde_json::from_slice(&data).map_err(|e| format!("{}: {e}", path.display()))?;
    if h.version > HANDOFF_VERSION {
        return Err(format!(
            "hand-off version {} is newer than {HANDOFF_VERSION}",
            h.version
        ));
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hanging_binary_is_refused_in_time() {
        let dir = std::env::temp_dir().join(format!("thurm-upgrade-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("thurmd");
        std::fs::write(&exe, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        assert!(check(&exe).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(dir);
    }
}
