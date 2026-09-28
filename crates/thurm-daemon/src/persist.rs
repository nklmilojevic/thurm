//! On-disk session snapshots: layout, per-pane metadata and scrollback.
//!
//! ```text
//! <state_dir>/session.json          layout + pane list
//! <state_dir>/scrollback/<id>.ansi  ANSI-escaped scrollback per pane
//! ```

use std::path::{Path, PathBuf};

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
        scrollbacks: &[(PaneId, Vec<u8>)],
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(self.scrollback_dir())?;
        for (id, data) in scrollbacks {
            write_atomic(&self.scrollback_path(*id), data)?;
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

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Scrollback may contain secrets; keep it private.
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(tmp, path)
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
