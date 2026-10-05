//! On-disk session snapshots: layout, per-pane metadata and scrollback.
//!
//! ```text
//! <state_dir>/session.json          layout + pane list
//! <state_dir>/scrollback/<id>.ansi  ANSI-escaped scrollback per pane
//! ```

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

    fn scrollback_dir(&self) -> PathBuf {
        self.dir.join("scrollback")
    }

    pub fn scrollback_path(&self, id: PaneId) -> PathBuf {
        self.scrollback_dir().join(format!("{id}.ansi"))
    }

    pub fn load(&self) -> Option<SessionSnapshot> {
        let text = std::fs::read_to_string(self.session_path()).ok()?;
        let snap: SessionSnapshot = serde_json::from_str(&text)
            .map_err(|e| log::warn!("ignoring corrupt session snapshot: {e}"))
            .ok()?;
        (snap.version == SNAPSHOT_VERSION).then_some(snap)
    }

    pub fn load_scrollback(&self, id: PaneId) -> Option<Vec<u8>> {
        std::fs::read(self.scrollback_path(id)).ok()
    }

    /// Atomically write the snapshot plus changed scrollbacks, and delete scrollback files of
    /// panes that no longer exist.
    pub fn save(
        &self,
        snap: &SessionSnapshot,
        scrollbacks: &[(PaneId, impl AsRef<[u8]>)],
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(self.scrollback_dir())?;
        for (id, data) in scrollbacks {
            write_atomic(&self.scrollback_path(*id), data.as_ref())?;
        }
        let json = serde_json::to_vec_pretty(snap).map_err(std::io::Error::other)?;
        write_atomic(&self.session_path(), &json)?;
        let keep: Vec<String> = snap
            .panes
            .iter()
            .map(|p| format!("{}.ansi", p.id))
            .collect();
        if let Ok(entries) = std::fs::read_dir(self.scrollback_dir()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".ansi") && !keep.contains(&name) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn clear(&self) {
        let _ = std::fs::remove_file(self.session_path());
        let _ = std::fs::remove_dir_all(self.scrollback_dir());
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
        // Scrollback may contain secrets; the file is private from the start.
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
                        let data = format!("thread {t} save {i}\r\n").repeat(200);
                        if store.save(&snap, &[(1, data.into_bytes())]).is_err() {
                            failures += 1;
                        }
                    }
                    failures
                })
            })
            .collect();
        let failures: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(failures, 0);
        // Whatever won, both files are whole, private, and no temporary file is left.
        assert!(store.load().is_some());
        let scrollback = String::from_utf8(store.load_scrollback(1).unwrap()).unwrap();
        let first = scrollback.lines().next().unwrap().to_owned();
        assert!(scrollback.lines().all(|l| l == first));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.scrollback_path(1))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .chain(std::fs::read_dir(dir.join("scrollback")).unwrap())
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
        assert!(
            store
                .save(&SessionSnapshot::default(), &[(1, b"x".to_vec())])
                .is_err()
        );
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
        store
            .save(&snap, &[(3, b"hello\r\n".to_vec()), (9, b"stale".to_vec())])
            .unwrap();
        assert_eq!(store.load().unwrap(), snap);
        assert_eq!(store.load_scrollback(3).unwrap(), b"hello\r\n");
        // Pane 9 is not in the snapshot, so its file is removed.
        assert!(store.load_scrollback(9).is_none());
        store.clear();
        assert!(store.load().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
