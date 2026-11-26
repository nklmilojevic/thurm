//! On-disk session snapshots: the layout and each pane's metadata, in
//! `<state_dir>/session.json`.
//!
//! Scrollback is not saved: whatever programs printed (tokens, keys, customer data) stays in
//! the daemon's memory. Older versions wrote it to `<state_dir>/scrollback/`, which
//! [`remove_old_scrollback`] removes at every start, whether or not sessions are saved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thurm_config::FlagArity;
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
    /// Program Resume Protocol (OSC 7577) record chosen for this pane. Never serialized as is:
    /// [`Store`] writes it sealed (see [`seal_record`]) and opens it on load.
    #[serde(skip)]
    pub program_resume: Option<SavedProgramResume>,
    pub size: PaneSizeSnap,
}

/// Persisted OSC 7577 program resume record for one pane.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SavedProgramResume {
    /// Reported command arguments (`argv[0]` + args).
    pub argv: Vec<String>,
    /// Working directory when the report was accepted (from `cwd` key or owning process cwd).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// True when `cwd` came from the report's `cwd` key: a missing directory skips resume.
    #[serde(default)]
    pub cwd_from_report: bool,
    /// True when the owning process was the first process on the PTY (no shell above it).
    #[serde(default)]
    pub first_process: bool,
    /// Effective UID of the daemon when the report was accepted; must match on restore.
    pub uid: u32,
    /// Unix timestamp (seconds) from the report's `time` key.
    pub reported_at: u64,
    /// Owning process metadata captured at accept time so trust rules re-evaluate on restore.
    /// The name is for diagnostics only; the executable is the OS's canonical path.
    #[serde(default)]
    pub process_name: String,
    #[serde(default)]
    pub process_exe: String,
    /// What restore needs of the owner's launch command line to re-check trust: see
    /// `program_resume::launch_evidence`. `None` (a snapshot from before it existed) fails
    /// every rule closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<SavedLaunchEvidence>,
    /// Whether an ancestor between the shell and the owner was not a shell/login/proxy.
    #[serde(default)]
    pub wrapped: bool,
    /// True once restore has attempted this record, so crashes or failures never retry.
    #[serde(default)]
    pub attempted: bool,
}

/// The part of an owner's launch command line that trust rules look at, as saved. Other launch
/// arguments may hold credentials and are never saved; the flag values saved are keyed digests
/// (`program_resume::digest_value`), never the values themselves.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SavedLaunchEvidence {
    /// The launch `argv[0]` (empty when the launch command line was unreadable).
    pub program: String,
    /// An interpreter's script operand, absolute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The canonical file `script` named when the report was accepted: restore pins the
    /// program to its directory (`program_resume::saved_pin_dirs`) without following the
    /// launcher again. `None` with a `script` pins nothing, so nothing is resumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_target: Option<String>,
    /// One entry per distinct set of flags (with arities) a rule for this command checks.
    #[serde(default)]
    pub flags: Vec<SavedFlagEvidence>,
}

/// The launch occurrences of exactly the flags in `checked`, parsed with those arities.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SavedFlagEvidence {
    /// The flags (and their arities) this evidence covers: a rule checking any other flag, or
    /// one of these with another arity, finds no evidence here.
    pub checked: BTreeMap<String, FlagArity>,
    /// Each occurrence in launch order: the flag and the digests of its values.
    pub occurrences: Vec<(String, Vec<String>)>,
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
    /// The evidence key, when not the daemon's own (tests).
    key: Option<[u8; 32]>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, key: None }
    }

    #[cfg(test)]
    fn with_key(dir: PathBuf, key: [u8; 32]) -> Self {
        Self {
            dir,
            key: Some(key),
        }
    }

    /// The key resume records are sealed under (derived from the evidence key, kept outside
    /// the state directory). `None` when it can be neither read nor made.
    fn record_key(&self) -> Option<[u8; 32]> {
        let evidence = self.key.or_else(crate::program_resume::evidence_key)?;
        Some(derive_record_key(&evidence))
    }

    fn session_path(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    pub fn load(&self) -> Option<SessionSnapshot> {
        // Saved resume records are as sensitive as a shell startup file: trust them only when
        // the directory and the snapshot are private to this user, checked on the very handles
        // the bytes are read through. Otherwise restore the layout without them.
        let (text, trust_osc) = match read_private_session(&self.dir) {
            Some(Ok(text)) => (text, true),
            Some(Err(())) => {
                let path = self.session_path();
                (std::fs::read_to_string(&path).ok()?, false)
            }
            None => return None,
        };
        let mut value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| log::warn!("ignoring corrupt session snapshot: {e}"))
            .ok()?;
        // Each pane's sealed record is taken out before the snapshot is decoded, so one that
        // does not open (or decode) costs only that record, never the panes and layout.
        let sealed: Vec<Option<serde_json::Value>> = value
            .get_mut("panes")
            .and_then(serde_json::Value::as_array_mut)
            .map(|panes| {
                panes
                    .iter_mut()
                    .map(|p| p.as_object_mut()?.remove("program_resume"))
                    .collect()
            })
            .unwrap_or_default();
        let mut snap: SessionSnapshot = serde_json::from_value(value)
            .map_err(|e| log::warn!("ignoring corrupt session snapshot: {e}"))
            .ok()?;
        if snap.version != SNAPSHOT_VERSION {
            return None;
        }
        let had_any = sealed.iter().any(Option::is_some);
        if had_any && !trust_osc {
            log::warn!(
                "discarding saved program resume records: {} is not private to current user",
                self.session_path().display()
            );
        } else if had_any {
            let key = self.record_key();
            if key.is_none() {
                log::warn!("discarding saved program resume records: no key to open them");
            }
            for (pane, record) in snap.panes.iter_mut().zip(sealed) {
                let (Some(key), Some(record)) = (key.as_ref(), record) else {
                    continue;
                };
                pane.program_resume = open_record(key, pane.id, &record)
                    .map_err(|e| {
                        log::warn!(
                            "discarding saved program resume record of pane {}: {e}",
                            pane.id
                        )
                    })
                    .ok();
            }
        }
        Some(snap)
    }

    /// Atomically write the snapshot.
    pub fn save(&self, snap: &SessionSnapshot) -> std::io::Result<()> {
        if !self.dir.exists() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.dir)?;
        } else {
            make_private(&self.dir);
        }
        let mut value = serde_json::to_value(snap).map_err(std::io::Error::other)?;
        if snap.panes.iter().any(|p| p.program_resume.is_some()) {
            let key = self.record_key();
            if key.is_none() {
                log::warn!("not saving program resume records: no key to seal them");
            }
            let panes = value
                .get_mut("panes")
                .and_then(serde_json::Value::as_array_mut)
                .ok_or_else(|| std::io::Error::other("snapshot has no panes"))?;
            for (pane, out) in snap.panes.iter().zip(panes) {
                let (Some(key), Some(record)) = (key.as_ref(), pane.program_resume.as_ref()) else {
                    continue;
                };
                // Never written unsealed: a record that cannot be sealed is left out.
                match seal_record(key, pane.id, record) {
                    Ok(sealed) => {
                        if let Some(obj) = out.as_object_mut() {
                            obj.insert("program_resume".into(), sealed.into());
                        }
                    }
                    Err(e) => log::warn!("not saving program resume record of {}: {e}", pane.id),
                }
            }
        }
        let json = serde_json::to_vec_pretty(&value).map_err(std::io::Error::other)?;
        write_atomic(&self.session_path(), &json)
    }

    #[cfg(test)]
    pub fn clear(&self) {
        let _ = std::fs::remove_file(self.session_path());
    }
}

/// What the record key is derived for, apart from the evidence digests under the same key.
const RECORD_KEY_PURPOSE: &str = "thurm program resume record seal v1";
/// What a sealed record starts with, followed by base64 of the nonce and the ciphertext.
const SEALED_RECORD_PREFIX: &str = "sealed-v1:";
const RECORD_NONCE_LEN: usize = 12;

/// HMAC-SHA256 of [`RECORD_KEY_PURPOSE`] under the evidence key: the encryption key is not the
/// key the launch evidence is digested with.
fn derive_record_key(evidence: &[u8; 32]) -> [u8; 32] {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(evidence).expect("any key length");
    mac.update(RECORD_KEY_PURPOSE.as_bytes());
    mac.finalize().into_bytes().into()
}

/// The data a pane's record is authenticated with: a record moved to another pane does not open.
fn record_aad(pane: PaneId) -> String {
    format!("thurm program resume record of pane {pane}")
}

/// `record` encrypted and authenticated (AES-256-GCM) under `key`, as saved in `session.json`:
/// reported arguments may carry credentials (an inline `--mcp-config`), and the snapshot and
/// its copies must not.
fn seal_record(
    key: &[u8; 32],
    pane: PaneId,
    record: &SavedProgramResume,
) -> std::io::Result<String> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use base64::Engine;
    use std::io::Read;
    let json = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let mut nonce = [0u8; RECORD_NONCE_LEN];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let aad = record_aad(pane);
    let sealed = aes_gcm::Aes256Gcm::new(&(*key).into())
        .encrypt(
            &nonce.into(),
            Payload {
                msg: &json,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| std::io::Error::other("cannot seal the record"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode([&nonce[..], &sealed].concat());
    Ok(format!("{SEALED_RECORD_PREFIX}{b64}"))
}

/// The record [`seal_record`] sealed for `pane` under `key`. Anything else (a plain record, one
/// sealed under another key or for another pane, a tampered one) is refused.
fn open_record(
    key: &[u8; 32],
    pane: PaneId,
    value: &serde_json::Value,
) -> Result<SavedProgramResume, String> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use base64::Engine;
    let data = value
        .as_str()
        .and_then(|s| s.strip_prefix(SEALED_RECORD_PREFIX))
        .ok_or("not sealed")?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "not sealed")?;
    if data.len() < RECORD_NONCE_LEN {
        return Err("sealed record is truncated".into());
    }
    let (nonce, sealed) = data.split_at(RECORD_NONCE_LEN);
    let nonce: [u8; RECORD_NONCE_LEN] = nonce.try_into().map_err(|_| "bad nonce")?;
    let aad = record_aad(pane);
    let json = aes_gcm::Aes256Gcm::new(&(*key).into())
        .decrypt(
            &nonce.into(),
            Payload {
                msg: sealed,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| "sealed record does not open with this key")?;
    serde_json::from_slice(&json).map_err(|e| format!("sealed record is invalid: {e}"))
}

/// Takes group and other access away from an existing directory this user owns (the state
/// directory made under a umask like 002), so the records saved from now on are trusted on the
/// next start. Those already there were written while others could change them, and stay
/// untrusted until this save replaces them.
fn make_private(dir: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = std::fs::metadata(dir) else {
        return;
    };
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 == 0 {
        return;
    }
    let mode = meta.mode() & 0o700;
    if let Err(e) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)) {
        log::warn!("cannot make {} private: {e}", dir.display());
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

/// Opens `dir`, then `session.json` relative to it without following a symlink, checks
/// ownership and private permissions on those exact file descriptors, and reads the bytes from
/// the verified one. (A symlink to the state directory is fine: the directory it opens is the
/// one checked.)
///
/// Returns:
/// - `None` if `session.json` does not exist;
/// - `Some(Ok(text))` if both directory and file are verified private to the current user;
/// - `Some(Err(()))` if the file exists or could not be verified private (caller may fall back to
///   layout-only restore with `program_resume` stripped).
fn read_private_session(dir: &Path) -> Option<Result<String, ()>> {
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let dir_c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let dir_fd = unsafe {
        libc::open(
            dir_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if dir_fd < 0 {
        let err = std::io::Error::last_os_error();
        return if err.kind() == std::io::ErrorKind::NotFound {
            None
        } else {
            Some(Err(()))
        };
    }
    let dir_owned = unsafe { OwnedFd::from_raw_fd(dir_fd) };
    let uid = unsafe { libc::geteuid() };
    let mut dir_st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(dir_fd, &mut dir_st) } != 0 {
        return Some(Err(()));
    }
    let dir_ok = (dir_st.st_mode & libc::S_IFMT) == libc::S_IFDIR
        && dir_st.st_uid == uid
        && (dir_st.st_mode & 0o022) == 0;

    let file_fd = unsafe {
        libc::openat(
            dir_fd,
            c"session.json".as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    drop(dir_owned);
    if file_fd < 0 {
        let err = std::io::Error::last_os_error();
        return if err.kind() == std::io::ErrorKind::NotFound {
            None
        } else {
            Some(Err(()))
        };
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(file_fd) };
    let mut file_st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(file_fd, &mut file_st) } != 0 {
        return Some(Err(()));
    }
    let file_ok = (file_st.st_mode & libc::S_IFMT) == libc::S_IFREG
        && file_st.st_uid == uid
        && (file_st.st_mode & 0o077) == 0;
    if !dir_ok || !file_ok {
        return Some(Err(()));
    }
    let mut text = String::new();
    match file.read_to_string(&mut text) {
        Ok(_) => Some(Ok(text)),
        Err(_) => Some(Err(())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    fn sample() -> SessionSnapshot {
        SessionSnapshot {
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
                program_resume: Some(SavedProgramResume {
                    argv: vec!["nvim".into(), "-S".into(), "/tmp/s.vim".into()],
                    cwd: Some("/tmp".into()),
                    cwd_from_report: true,
                    first_process: false,
                    uid: unsafe { libc::geteuid() },
                    reported_at: 1795543200,
                    process_name: "nvim".into(),
                    process_exe: "/usr/bin/nvim".into(),
                    launch: Some(SavedLaunchEvidence {
                        program: "nvim".into(),
                        script: None,
                        script_target: None,
                        flags: vec![SavedFlagEvidence {
                            checked: BTreeMap::from([("-S".to_owned(), FlagArity::One)]),
                            occurrences: vec![("-S".into(), vec!["0f".into()])],
                        }],
                    }),
                    wrapped: false,
                    attempted: false,
                }),
                size: PaneSize::default().into(),
            }],
            next_pane_id: 4,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("thurm-persist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn reported_arguments_are_sealed_on_disk() {
        let dir = temp_dir("sealed");
        let store = Store::with_key(dir.clone(), KEY);
        let mut snap = sample();
        let rec = snap.panes[0].program_resume.as_mut().unwrap();
        rec.argv = [
            "claude",
            "--resume",
            "x",
            "--mcp-config",
            r#"{"token":"sk-inline-secret"}"#,
        ]
        .map(String::from)
        .to_vec();
        rec.cwd = Some("/srv/private-project".into());
        store.save(&snap).unwrap();
        let bytes = std::fs::read_to_string(store.session_path()).unwrap();
        for plain in [
            "sk-inline-secret",
            "mcp-config",
            "private-project",
            "/usr/bin/nvim",
        ] {
            assert!(!bytes.contains(plain), "{plain} in {bytes}");
        }
        // The layout and the pane stay readable.
        assert!(bytes.contains("abc-123") && bytes.contains(SEALED_RECORD_PREFIX));
        assert_eq!(store.load().unwrap(), snap);

        // Under another key the record does not open: the panes and layout still restore.
        let other = Store::with_key(dir.clone(), [8; 32]);
        let loaded = other.load().unwrap();
        assert!(loaded.panes[0].program_resume.is_none());
        assert_eq!(loaded.layout, snap.layout);
        assert_eq!(loaded.panes[0].agent_session, snap.panes[0].agent_session);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_record_costs_only_that_record() {
        let dir = temp_dir("bad-record");
        let store = Store::with_key(dir.clone(), KEY);
        let snap = sample();
        let rec = snap.panes[0].program_resume.as_ref().unwrap();
        let write = |record: serde_json::Value| {
            let mut value = serde_json::to_value(&snap).unwrap();
            value["panes"][0]["program_resume"] = record;
            std::fs::create_dir_all(&dir).unwrap();
            write_atomic(&store.session_path(), value.to_string().as_bytes()).unwrap();
        };
        let mut no_uid = serde_json::to_value(rec).unwrap();
        no_uid.as_object_mut().unwrap().remove("uid");
        let mut plain_ok = serde_json::to_value(rec).unwrap();
        plain_ok["argv"] = serde_json::json!(["nvim"]);
        let other_pane = seal_record(&derive_record_key(&KEY), 9, rec).unwrap();
        let mut tampered = seal_record(&derive_record_key(&KEY), 3, rec).unwrap();
        let mid = tampered.len() / 2;
        let flip = if &tampered[mid..=mid] == "A" {
            "B"
        } else {
            "A"
        };
        tampered.replace_range(mid..=mid, flip);
        for record in [
            no_uid,
            plain_ok,
            serde_json::json!(42),
            serde_json::json!("sealed-v1:!!!"),
            other_pane.into(),
            tampered.into(),
        ] {
            write(record.clone());
            let loaded = store.load().unwrap_or_else(|| panic!("{record}"));
            assert!(loaded.panes[0].program_resume.is_none(), "{record}");
            assert_eq!(loaded.layout, snap.layout);
            assert_eq!(loaded.panes.len(), 1);
            assert_eq!(loaded.panes[0].title, "zsh");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_saves_all_succeed() {
        let dir = std::env::temp_dir().join(format!("thurm-persist-conc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = std::sync::Arc::new(Store::with_key(dir.clone(), KEY));
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
                                program_resume: None,
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
    fn record_saved_with_flat_launch_argv_loads_without_evidence() {
        let old = r#"{"argv":["nvim"],"uid":501,"reported_at":1,"process_argv":["nvim","-S","x"]}"#;
        let rec: SavedProgramResume = serde_json::from_str(old).unwrap();
        assert_eq!(rec.argv, ["nvim"]);
        assert!(rec.launch.is_none());
    }

    #[test]
    fn roundtrip_and_cleanup() {
        let dir = std::env::temp_dir().join(format!("thurm-persist-{}", std::process::id()));
        let store = Store::with_key(dir.clone(), KEY);
        let snap = sample();
        store.save(&snap).unwrap();
        assert_eq!(store.load().unwrap(), snap);

        // Non-private permissions or symlinked session.json keep layout but drop program_resume.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(store.session_path(), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.layout, snap.layout);
        assert!(loaded.panes[0].program_resume.is_none());

        // A symlinked session.json, even to a private snapshot in the same private directory.
        store.save(&snap).unwrap();
        let real = dir.join("real.json");
        std::fs::rename(store.session_path(), &real).unwrap();
        std::os::unix::fs::symlink(&real, store.session_path()).unwrap();
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = store.load().unwrap();
        assert_eq!(loaded.layout, snap.layout);
        assert!(loaded.panes[0].program_resume.is_none());
        std::fs::remove_file(&real).unwrap();

        store.clear();
        assert!(store.load().is_none());

        // A state directory others could write to (made under umask 002) drops the records
        // found in it, and the next save makes it private, so they hold from then on.
        store.save(&snap).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(store.load().unwrap().panes[0].program_resume.is_none());
        store.save(&snap).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert_eq!(store.load().unwrap(), snap);
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
