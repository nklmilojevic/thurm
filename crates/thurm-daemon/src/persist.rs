//! On-disk session snapshots: the layout and each pane's metadata, in
//! `<state_dir>/session.json`.
//!
//! Scrollback is not saved: whatever programs printed (tokens, keys, customer data) stays in
//! the daemon's memory. Older versions wrote it to `<state_dir>/scrollback/`, which
//! [`remove_old_scrollback`] removes at every start, whether or not sessions are saved.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thurm_proto::{PaneId, PaneSize};

pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SessionSnapshot {
    pub version: u32,
    pub saved_at: u64,
    pub layout: Option<String>,
    pub panes: Vec<PaneSnapshot>,
    pub next_pane_id: PaneId,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PaneSnapshot {
    pub id: PaneId,
    pub cwd: Option<String>,
    pub title: String,
    /// Explicit command the pane was created with (`None` = shell).
    pub command: Option<Vec<String>>,
    /// Kind of the agent that was in the foreground when saved.
    pub agent: Option<String>,
    /// Its session id (from hooks), for an exact resume.
    #[serde(default)]
    pub agent_session: Option<String>,
    /// Exact executable and arguments supplied by the session owner.
    #[serde(default)]
    pub agent_resume: Option<Vec<String>>,
    pub size: PaneSizeSnap,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct PaneSizeSnap {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl From<PaneSize> for PaneSizeSnap {
    fn from(s: PaneSize) -> Self {
        Self {
            cols: s.cols,
            rows: s.rows,
            cell_width: s.cell_width,
            cell_height: s.cell_height,
        }
    }
}

impl From<PaneSizeSnap> for PaneSize {
    fn from(s: PaneSizeSnap) -> Self {
        Self {
            cols: s.cols,
            rows: s.rows,
            cell_width: s.cell_width,
            cell_height: s.cell_height,
        }
    }
}

/// Deletes the plain-text scrollback an older version saved in `state_dir`.
pub fn remove_old_scrollback(state_dir: &Path) {
    match std::fs::remove_dir_all(state_dir.join("scrollback")) {
        Ok(()) => log::info!("removed the scrollback an older version saved"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("cannot remove saved scrollback: {e}"),
    }
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn session_path(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    pub fn load(&self) -> Option<SessionSnapshot> {
        let text = std::fs::read_to_string(self.session_path()).ok()?;
        let snap: SessionSnapshot = serde_json::from_str(&text)
            .map_err(|e| log::warn!("ignoring corrupt session snapshot: {e}"))
            .ok()?;
        (snap.version == SNAPSHOT_VERSION).then_some(snap)
    }

    /// Atomically write the snapshot.
    pub fn save(&self, snap: &SessionSnapshot) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec_pretty(snap).map_err(std::io::Error::other)?;
        write_atomic(&self.session_path(), &json)
    }

    #[cfg(test)]
    pub fn clear(&self) {
        let _ = std::fs::remove_file(self.session_path());
    }
}

/// Writes `data` to a new private temporary file next to `path`, flushes it to disk, renames
/// it over `path` and flushes the directory, so `path` holds either the old or the new
/// contents, even after a crash or power loss. The temporary name is unique per call.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{seq}", std::process::id()));
    let result = (|| {
        // Titles and working directories are private too; the file is private from the start.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_saves_all_succeed() {
        let dir = std::env::temp_dir().join(format!("thurm-persist-conc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = std::sync::Arc::new(Store::new(dir.clone()));
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let store = store.clone();
                std::thread::spawn(move || {
                    let mut failures = 0;
                    for i in 0..30u64 {
                        let snap = SessionSnapshot {
                            version: SNAPSHOT_VERSION,
                            saved_at: t * 1000 + i,
                            panes: vec![PaneSnapshot {
                                id: 1,
                                cwd: None,
                                title: String::new(),
                                command: None,
                                agent: None,
                                agent_session: None,
                                agent_resume: None,
                                size: PaneSize::default().into(),
                            }],
                            ..Default::default()
                        };
                        if store.save(&snap).is_err() {
                            failures += 1;
                        }
                    }
                    failures
                })
            })
            .collect();
        let failures: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(failures, 0);
        // Whatever won, the file is whole, private, and no temporary file is left.
        assert!(store.load().is_some());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.session_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_errors_are_reported() {
        let file = std::env::temp_dir().join(format!("thurm-persist-file-{}", std::process::id()));
        std::fs::write(&file, b"not a directory").unwrap();
        let store = Store::new(file.clone());
        assert!(store.save(&SessionSnapshot::default()).is_err());
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn roundtrip_and_cleanup() {
        let dir = std::env::temp_dir().join(format!("thurm-persist-{}", std::process::id()));
        let store = Store::new(dir.clone());
        let snap = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at: 1,
            layout: Some("{}".into()),
            panes: vec![PaneSnapshot {
                id: 3,
                cwd: Some("/tmp".into()),
                title: "zsh".into(),
                command: None,
                agent: Some("claude".into()),
                agent_session: Some("abc-123".into()),
                agent_resume: Some(vec!["claude".into(), "--resume".into(), "abc-123".into()]),
                size: PaneSize::default().into(),
            }],
            next_pane_id: 4,
        };
        store.save(&snap).unwrap();
        assert_eq!(store.load().unwrap(), snap);
        store.clear();
        assert!(store.load().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scrollback_an_older_version_saved_is_removed() {
        let dir = std::env::temp_dir().join(format!("thurm-persist-old-{}", std::process::id()));
        let old = dir.join("scrollback");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("3.ansi"), b"export TOKEN=secret\r\n").unwrap();
        remove_old_scrollback(&dir);
        assert!(!old.exists());
        let store = Store::new(dir.clone());
        // Saving writes no scrollback either.
        store.save(&SessionSnapshot::default()).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["session.json"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
