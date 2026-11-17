//! Files to and from this host on byte streams ([`thurm_proto::CAP_TRANSFER`]).
//!
//! Uploads land in `<state>/drops/<pane>/<batch>/<name>`, written under `.parts/` and moved
//! into place once exactly the size announced has arrived. When the last file of a batch lands,
//! the daemon pastes their paths into the pane (bracketed paste applies) and only then closes
//! the batch's streams `Ok`. A file that fails, or the client going away, deletes the batch and
//! types nothing.
//!
//! Downloads send one regular file, resolved against the pane's working directory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use parking_lot::{Condvar, Mutex};
use thurm_proto::stream::{self, MAX_DATA, StreamClose, StreamFrame};
use thurm_proto::{DownloadInfo, PaneId, Principal, UploadBatch};

use crate::daemon::Daemon;
use crate::streams::{Kind, OpenStream};

/// The largest file one upload takes.
pub const MAX_FILE: u64 = 4 << 30;
/// The most files in one batch.
pub const MAX_BATCH: u32 = 256;
/// Files reserved and not landed yet, per pane.
pub const MAX_RESERVED_PER_PANE: usize = 64;
/// What an upload must leave free on the disk.
pub const MIN_FREE: u64 = 1 << 30;
/// Files the disk must still be able to take (inodes) after an upload.
pub const MIN_FREE_FILES: u64 = 10_000;
/// Batches not complete yet, per client and over all: each holds the streams of its landed
/// files until the last arrives.
pub const MAX_WAITING_BATCHES_PER_CLIENT: usize = 8;
pub const MAX_WAITING_BATCHES: usize = 32;
/// Landed files waiting for the rest of their batch, over all: a stream (and a thread) each.
pub const MAX_WAITING_FILES: usize = 512;
/// Batches kept, over all panes, however small.
pub const MAX_KEPT_BATCHES: usize = 1024;
/// What one file or directory takes on disk at least: an empty file is not free. 4 KiB, or
/// `THURM_TRANSFER_BLOCK` (tests, to count in bytes).
fn block() -> u64 {
    static BLOCK: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BLOCK.get_or_init(|| {
        std::env::var("THURM_TRANSFER_BLOCK")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|b| *b > 0)
            .unwrap_or(4096)
    })
}
/// Uploads kept, over all panes; the oldest batches go first.
pub const MAX_KEPT: u64 = 8 << 30;
/// How long uploads are kept.
pub const KEEP: Duration = Duration::from_secs(24 * 60 * 60);
/// How often kept uploads are pruned while nothing else asks for it.
pub const PRUNE_EVERY: Duration = Duration::from_secs(10 * 60);
/// Downloads sending at once, per pane.
pub const MAX_DOWNLOADS_PER_PANE: usize = 4;
/// The longest file name, in bytes.
const NAME_MAX: usize = 255;
/// Where files are written until whole, inside a batch's directory.
const PARTS: &str = ".parts";
/// When a batch was complete (milliseconds since the Unix epoch), inside its directory: what it
/// is aged from, by this daemon and the next.
const DONE: &str = ".done";

pub struct Transfers {
    root: PathBuf,
    state: Mutex<State>,
    changed: Condvar,
    /// A batch nothing happened to for this long has stalled (a ticket never used).
    idle: Duration,
    /// [`MAX_KEPT`], or `THURM_TRANSFER_MAX_KEPT` (tests).
    max_kept: u64,
    /// [`KEEP`], or `THURM_TRANSFER_KEEP_MS` (tests).
    keep: Duration,
    /// A pause between a batch's last file landing and its paths being typed:
    /// `THURM_TRANSFER_PASTE_DELAY_MS` (tests), none otherwise.
    paste_delay: Duration,
}

/// A duration in milliseconds from the environment variable `var`, else `default` (tests).
fn env_ms(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(default, Duration::from_millis)
}

#[derive(Default)]
struct State {
    batches: HashMap<u64, Batch>,
    next: u64,
    downloads: HashMap<PaneId, usize>,
}

struct Batch {
    pane: PaneId,
    client: u64,
    /// The client's name for it; `None` for a file sent alone.
    id: Option<String>,
    dir: PathBuf,
    count: u32,
    files: BTreeMap<u32, Slot>,
    /// Names taken in `dir`.
    names: HashSet<String>,
    /// Every file landed and the paths are being typed: it can no longer fail.
    pasting: bool,
    outcome: Option<Result<(), String>>,
    touched: Instant,
    /// The streams writing a file of the batch now, by its index: shut when the batch fails,
    /// and until they have let go, the batch's directory stays and its bytes count.
    writers: Vec<(u32, UnixStream)>,
}

struct Slot {
    path: PathBuf,
    size: u64,
    state: SlotState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Reserved,
    Writing,
    /// Of a failed batch, its writer gone: nothing of it is left to count.
    Gone,
    Landed,
}

impl Batch {
    fn waiting(&self) -> bool {
        self.outcome.is_none()
    }

    /// Its directory is in use: on its way, being typed, or still written to.
    fn busy(&self) -> bool {
        self.waiting() || self.pasting || !self.writers.is_empty()
    }

    /// Whether `f` takes room: every file of a batch still on its way, and a file of a failed
    /// one while it is written.
    fn occupies(&self, f: &Slot) -> bool {
        f.state != SlotState::Gone
            && (self.waiting() || self.pasting || f.state == SlotState::Writing)
    }
}

impl Transfers {
    pub fn new(root: PathBuf) -> Transfers {
        Transfers {
            root,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            idle: crate::streams::ticket_ttl().max(Duration::from_secs(1)) * 2,
            max_kept: std::env::var("THURM_TRANSFER_MAX_KEPT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_KEPT),
            keep: env_ms("THURM_TRANSFER_KEEP_MS", KEEP),
            paste_delay: env_ms("THURM_TRANSFER_PASTE_DELAY_MS", Duration::ZERO),
        }
    }

    /// Ends stalled batches and deletes kept uploads past their time or over the limit, those
    /// left by an earlier daemon included: what keeps uploads within their promise when no
    /// new one comes.
    pub fn prune(&self) {
        let mut st = self.state.lock();
        self.sweep(&mut st);
        self.make_room(&st, 0, 0);
    }

    /// [`thurm_proto::Request::UploadBegin`]: reserves the file's place in its batch and a
    /// stream to write it.
    #[allow(clippy::too_many_arguments)]
    pub fn upload_begin(
        &self,
        daemon: &Daemon,
        client: u64,
        principal: Principal,
        pane: PaneId,
        name: &str,
        size: u64,
        batch: Option<UploadBatch>,
    ) -> Result<(String, u32), String> {
        if size > MAX_FILE {
            return Err(format!(
                "{name} is too large to send (at most {} GiB)",
                MAX_FILE >> 30
            ));
        }
        let (id, index, count) = match batch {
            Some(b) => (Some(b.id), b.index, b.count),
            None => (None, 0, 1),
        };
        if count == 0 || count > MAX_BATCH {
            return Err(format!("at most {MAX_BATCH} files are sent at once"));
        }
        if index >= count {
            return Err(format!("file {index} of a batch of {count}"));
        }
        let mut st = self.state.lock();
        self.sweep(&mut st);
        let on_way = |b: &Batch, f: &Slot| b.occupies(f) && f.state != SlotState::Landed;
        let reserved: usize = st
            .batches
            .values()
            .filter(|b| b.pane == pane)
            .map(|b| b.files.values().filter(|f| on_way(b, f)).count())
            .sum();
        if reserved >= MAX_RESERVED_PER_PANE {
            return Err(format!(
                "too many files on their way to this pane (at most {MAX_RESERVED_PER_PANE})"
            ));
        }
        let incomplete = |b: &&Batch| b.waiting() && !b.pasting;
        let waiting_files = st
            .batches
            .values()
            .filter(incomplete)
            .flat_map(|b| b.files.values())
            .filter(|f| f.state == SlotState::Landed)
            .count();
        if waiting_files >= MAX_WAITING_FILES {
            return Err(format!(
                "too many sent files are waiting for the rest of their batch (at most {MAX_WAITING_FILES})"
            ));
        }
        let pending: u64 = st
            .batches
            .values()
            .flat_map(|b| b.files.values().filter(move |f| on_way(b, f)))
            .map(|f| charge(f.size))
            .sum();
        // The batch this file joins, checked before anything changes for it.
        let existing = id.as_ref().and_then(|id| {
            st.batches
                .iter()
                .filter(|(_, b)| b.client == client && b.pane == pane)
                .find(|(_, b)| b.id.as_ref() == Some(id))
                .map(|(serial, _)| *serial)
        });
        if existing.is_none() {
            let waiting = st.batches.values().filter(incomplete);
            if waiting.clone().filter(|b| b.client == client).count()
                >= MAX_WAITING_BATCHES_PER_CLIENT
            {
                return Err(format!(
                    "too many uploads from this app are not complete yet (at most {MAX_WAITING_BATCHES_PER_CLIENT})"
                ));
            }
            if waiting.count() >= MAX_WAITING_BATCHES {
                return Err(format!(
                    "too many uploads are not complete yet (at most {MAX_WAITING_BATCHES})"
                ));
            }
        }
        if let Some(b) = existing.and_then(|serial| st.batches.get(&serial)) {
            match &b.outcome {
                Some(Err(e)) => return Err(e.clone()),
                Some(Ok(())) => return Err("that batch was already sent".into()),
                None => {}
            }
            if b.count != count {
                return Err(format!("the batch has {} files, not {count}", b.count));
            }
            if b.files.contains_key(&index) || b.pasting {
                return Err(format!("file {index} of the batch is already on its way"));
            }
        }
        // Everything on its way or landed in a batch still open counts, with this file, against
        // what is kept: older drops make room, and what cannot fit is refused before a byte.
        let active: u64 = st
            .batches
            .values()
            .flat_map(|b| b.files.values().filter(move |f| b.occupies(f)))
            .map(|f| charge(f.size))
            .sum();
        // With its directories (the batch's and its parts'), should it be the batch's first.
        let dir_cost = if existing.is_none() { 2 * block() } else { 0 };
        let incoming = active.saturating_add(charge(size)).saturating_add(dir_cost);
        let too_much = || {
            format!(
                "{name} does not fit in what this host keeps of sent files ({} MiB)",
                self.max_kept >> 20
            )
        };
        if incoming > self.max_kept {
            return Err(too_much());
        }
        // The disk keeps its floor: kept drops may give way for it, but only if enough of them
        // can, and only once the file is admitted.
        make_private_dir(&self.root)?;
        let need = charge(size)
            .saturating_add(dir_cost)
            .saturating_add(pending)
            .saturating_add(MIN_FREE);
        // A batch's directory, its parts directory, the file and its part.
        let files = free_files(&self.root)?;
        if files < MIN_FREE_FILES + 4 {
            return Err(format!(
                "this host's disk can take too few more files ({files} left)"
            ));
        }
        let free = free_space(&self.root)?;
        let short = need.saturating_sub(free);
        let no_space = |free: u64| {
            format!(
                "not enough free space for {name}: {} MiB free, and {} GiB must stay free",
                free >> 20,
                MIN_FREE >> 30
            )
        };
        if short > 0 && self.kept(&st).iter().map(|(_, size, _)| size).sum::<u64>() < short {
            return Err(no_space(free));
        }
        // A new batch's number is used up here, whatever follows: a ticket left by a refusal
        // below names a file that does not exist (its stream is refused), never another batch's.
        let serial = existing.unwrap_or_else(|| {
            st.next += 1;
            st.next
        });
        let ticket = daemon.streams.reserve(
            Kind::Upload {
                batch: serial,
                index,
            },
            client,
            principal,
        )?;
        // A new batch's directory first: if it cannot be made, nothing kept was deleted for it.
        let created = existing.is_none();
        if created {
            let dir = self.new_batch_dir(pane)?;
            st.batches.insert(
                serial,
                Batch {
                    pane,
                    client,
                    id,
                    dir,
                    count,
                    files: BTreeMap::new(),
                    names: HashSet::from([PARTS.to_owned(), DONE.to_owned()]),
                    pasting: false,
                    outcome: None,
                    touched: Instant::now(),
                    writers: Vec::new(),
                },
            );
        }
        let undo = |st: &mut State| {
            if created && let Some(b) = st.batches.remove(&serial) {
                remove_batch(&b.dir);
            }
        };
        // Only an admitted file makes older drops give way, and only what was deleted counts.
        if !self.make_room(&st, incoming, short) {
            undo(&mut st);
            return Err(too_much());
        }
        if short > 0 {
            let free = match free_space(&self.root) {
                Ok(free) => free,
                Err(e) => {
                    undo(&mut st);
                    return Err(e);
                }
            };
            // The new batch's directories are on the disk already.
            if free.saturating_add(dir_cost) < need {
                undo(&mut st);
                return Err(no_space(free));
            }
        }
        let b = st.batches.get_mut(&serial).expect("batch just found");
        let name = unique_name(&mut b.names, &clean_name(name));
        b.files.insert(
            index,
            Slot {
                path: b.dir.join(&name),
                size,
                state: SlotState::Reserved,
            },
        );
        b.touched = Instant::now();
        Ok(ticket)
    }

    /// [`thurm_proto::Request::DownloadBegin`]: checks the file and reserves a stream to send
    /// it, unless the caller has this version already.
    #[allow(clippy::too_many_arguments)]
    pub fn download_begin(
        &self,
        daemon: &Daemon,
        client: u64,
        principal: Principal,
        pane: PaneId,
        path: &str,
        cwd: Option<&str>,
        home: Option<&str>,
        if_version: Option<&str>,
    ) -> Result<DownloadInfo, String> {
        let abs = resolve(path, cwd, home)?;
        let file = open_nonblocking(&abs).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
                format!("{path}: {}", thurm_proto::DOWNLOAD_NOT_FOUND)
            }
            _ => format!("cannot open {path}: {e}"),
        })?;
        let meta = file
            .metadata()
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{path} is not a regular file"));
        }
        let info = DownloadInfo {
            ticket: None,
            expires_ms: 0,
            name: abs
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: abs.display().to_string(),
            size: meta.len(),
            modified: modified_ms(&meta),
            version: version(&meta),
            not_modified: false,
        };
        if if_version.is_some() && info.version.as_deref() == if_version {
            return Ok(DownloadInfo {
                not_modified: true,
                ..info
            });
        }
        if self.state.lock().downloads.get(&pane).copied().unwrap_or(0) >= MAX_DOWNLOADS_PER_PANE {
            return Err(format!(
                "too many downloads from this pane at once (at most {MAX_DOWNLOADS_PER_PANE})"
            ));
        }
        let (ticket, expires_ms) = daemon.streams.reserve(
            Kind::Download {
                pane,
                path: abs,
                size: info.size,
                stamp: stamp(&meta),
            },
            client,
            principal,
        )?;
        Ok(DownloadInfo {
            ticket: Some(ticket),
            expires_ms,
            ..info
        })
    }

    /// The connection `client` is gone: its batches still on their way are dropped.
    pub fn client_gone(&self, client: u64) {
        let mut st = self.state.lock();
        let gone: Vec<u64> = st
            .batches
            .iter()
            .filter(|(_, b)| b.client == client && b.waiting())
            .map(|(serial, _)| *serial)
            .collect();
        for serial in gone {
            self.fail(
                &mut st,
                serial,
                "the app sending the files went away".into(),
            );
        }
    }

    /// Ends batch `serial` with `why`, deleting what landed; no-op once it is decided or its
    /// paths are being typed.
    fn fail(&self, st: &mut State, serial: u64, why: String) {
        let Some(b) = st.batches.get_mut(&serial) else {
            return;
        };
        if !b.waiting() || b.pasting {
            return;
        }
        log::info!("upload to pane {} failed: {why}", b.pane);
        b.outcome = Some(Err(why));
        b.touched = Instant::now();
        // Streams still writing stop taking bytes now; the directory goes when the last lets go
        // (`writer_done`), its bytes counted until then.
        for (_, socket) in &b.writers {
            let _ = socket.shutdown(std::net::Shutdown::Read);
        }
        if b.writers.is_empty() {
            remove_batch(&b.dir);
        }
        self.changed.notify_all();
    }

    /// Fails stalled batches and forgets decided ones nobody waits for any more.
    fn sweep(&self, st: &mut State) {
        let stalled: Vec<u64> = st
            .batches
            .iter()
            .filter(|(_, b)| b.waiting() && !b.pasting && b.touched.elapsed() > self.idle)
            .map(|(serial, _)| *serial)
            .collect();
        for serial in stalled {
            self.fail(st, serial, "the upload stalled".into());
        }
        st.batches
            .retain(|_, b| b.busy() || b.touched.elapsed() <= self.idle);
    }

    /// Marks batch `serial` as moving; whether it is still on its way (a writer of a failed
    /// batch stops).
    fn touch(&self, serial: u64) -> bool {
        match self.state.lock().batches.get_mut(&serial) {
            Some(b) => {
                b.touched = Instant::now();
                b.waiting()
            }
            None => false,
        }
    }

    /// The writer of file `index` of batch `serial` let go: a failed batch's file stops
    /// counting, and its directory goes with the last writer.
    fn writer_done(&self, serial: u64, index: u32) {
        let mut st = self.state.lock();
        let Some(b) = st.batches.get_mut(&serial) else {
            return;
        };
        b.writers.retain(|(i, _)| *i != index);
        if matches!(b.outcome, Some(Err(_))) {
            if let Some(f) = b.files.get_mut(&index) {
                f.state = SlotState::Gone;
            }
            if b.writers.is_empty() {
                remove_batch(&b.dir);
            }
        }
    }

    /// A new directory for a batch to `pane`.
    fn new_batch_dir(&self, pane: PaneId) -> Result<PathBuf, String> {
        self.new_batch_dir_with(pane, |parts| {
            std::fs::DirBuilder::new().mode(0o700).create(parts)
        })
    }

    /// [`Transfers::new_batch_dir`], making the parts directory with `make_parts` (tests).
    fn new_batch_dir_with(
        &self,
        pane: PaneId,
        make_parts: impl Fn(&Path) -> std::io::Result<()>,
    ) -> Result<PathBuf, String> {
        let pane_dir = self.root.join(pane.to_string());
        make_private_dir(&pane_dir)?;
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        for n in 0..1000u32 {
            let dir = pane_dir.join(format!("{stamp}-{n}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    // A batch directory without its parts is no batch: it goes, not kept to
                    // count against the drops that are.
                    if let Err(e) = make_parts(&dir.join(PARTS)) {
                        let _ = std::fs::remove_dir_all(&dir);
                        return Err(format!("cannot create {}: {e}", dir.display()));
                    }
                    return Ok(dir);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
            }
        }
        Err("no free directory name for the upload".into())
    }

    /// The kept batches on disk, oldest first, with their sizes; those past their time
    /// (`keep`) are deleted on the way. Batches in use are left out.
    fn kept(&self, st: &State) -> Vec<(SystemTime, u64, PathBuf)> {
        let busy: HashSet<&Path> = st
            .batches
            .values()
            .filter(|b| b.busy())
            .map(|b| b.dir.as_path())
            .collect();
        let mut kept = Vec::new();
        if let Ok(panes) = std::fs::read_dir(&self.root) {
            for pane in panes.flatten() {
                let Ok(batches) = std::fs::read_dir(pane.path()) else {
                    continue;
                };
                for batch in batches.flatten() {
                    let dir = batch.path();
                    if busy.contains(dir.as_path()) {
                        continue;
                    }
                    let since = aged_from(&dir);
                    let size = dir_size(&dir);
                    let old = since.elapsed().is_ok_and(|age| age > self.keep);
                    if !(old && remove_batch(&dir)) {
                        kept.push((since, size, dir));
                    }
                }
                // Gone once empty (a closed pane's).
                let _ = std::fs::remove_dir(pane.path());
            }
        }
        kept.sort_by_key(|(modified, _, _)| *modified);
        kept
    }

    /// Deletes the oldest kept batches until `incoming` more bytes stay within the kept limit
    /// and at least `free` bytes were freed. A batch that could not be deleted still counts.
    /// Whether both hold now.
    fn make_room(&self, st: &State, incoming: u64, free: u64) -> bool {
        let kept = self.kept(st);
        let mut total: u64 = kept.iter().map(|(_, size, _)| size).sum();
        let mut freed = 0u64;
        let mut count = kept.len();
        // Room for one more batch, however small its files.
        let fits = |total: u64, freed: u64, count: usize| {
            total.saturating_add(incoming) <= self.max_kept
                && freed >= free
                && count < MAX_KEPT_BATCHES
        };
        for (_, size, dir) in kept {
            if fits(total, freed, count) {
                break;
            }
            if remove_batch(&dir) {
                total = total.saturating_sub(size);
                freed += size;
                count -= 1;
            }
        }
        fits(total, freed, count)
    }

    /// File `index` of batch `serial` arrived whole at its place. When it is the last, types
    /// the batch's paths into its pane.
    fn landed(&self, daemon: &Daemon, serial: u64, index: u32) {
        {
            let mut st = self.state.lock();
            let Some(b) = st.batches.get_mut(&serial) else {
                return;
            };
            if let Some(f) = b.files.get_mut(&index) {
                f.state = SlotState::Landed;
            }
            b.touched = Instant::now();
        }
        if !self.paste_delay.is_zero() {
            std::thread::sleep(self.paste_delay);
        }
        // Decided and typed under the lock `client_gone` takes: a client that went before the
        // paths are written fails the batch, and nothing is typed for it.
        let mut st = self.state.lock();
        let Some(b) = st.batches.get_mut(&serial) else {
            return;
        };
        let all = b.files.len() == b.count as usize
            && b.files.values().all(|f| f.state == SlotState::Landed);
        if !all || !b.waiting() || b.pasting {
            return;
        }
        b.pasting = true;
        let paths: Vec<String> = b
            .files
            .values()
            .map(|f| crate::shell::shell_quote(&f.path.display().to_string()))
            .collect();
        let (client, pane, text) = (b.client, b.pane, paths.join(" ") + " ");
        let typed = daemon.paste_for(client, pane, &text);
        if let Some(b) = st.batches.get_mut(&serial) {
            b.pasting = false;
            b.touched = Instant::now();
            match typed {
                Ok(()) => {
                    record_done(&b.dir);
                    b.outcome = Some(Ok(()));
                    self.changed.notify_all();
                }
                Err(e) => self.fail(&mut st, serial, e),
            }
        }
    }

    /// Waits until batch `serial` is decided.
    fn outcome(&self, serial: u64) -> Result<(), String> {
        let mut st = self.state.lock();
        loop {
            let Some(b) = st.batches.get(&serial) else {
                return Err("the upload was cancelled".into());
            };
            if let Some(outcome) = &b.outcome {
                return outcome.clone();
            }
            if !b.pasting && b.touched.elapsed() > self.idle {
                self.fail(&mut st, serial, "the upload stalled".into());
                continue;
            }
            self.changed.wait_for(&mut st, Duration::from_millis(500));
        }
    }

    fn serve_upload<R: Read + Send>(
        &self,
        daemon: &Daemon,
        serial: u64,
        index: u32,
        mut reader: R,
        mut writer: UnixStream,
        open: OpenStream,
    ) {
        let slot = {
            let mut st = self.state.lock();
            match st.batches.get_mut(&serial) {
                None => Err("the upload was cancelled".to_owned()),
                Some(b) => match (&b.outcome, b.files.get_mut(&index)) {
                    (Some(Err(e)), _) => Err(e.clone()),
                    (None, Some(f)) if f.state == SlotState::Reserved => match writer.try_clone() {
                        Ok(socket) => {
                            f.state = SlotState::Writing;
                            b.touched = Instant::now();
                            b.writers.push((index, socket));
                            Ok((f.path.clone(), f.size))
                        }
                        Err(e) => Err(format!("cannot take the file: {e}")),
                    },
                    _ => Err("that file is not expected".to_owned()),
                },
            }
        };
        let (path, size) = match slot {
            Ok(slot) => slot,
            Err(e) => return close(&mut writer, StreamClose::Refused(e)),
        };
        let part = path
            .parent()
            .unwrap_or(Path::new("/"))
            .join(PARTS)
            .join(index.to_string());
        let received = receive(&mut reader, &part, size, || self.touch(serial))
            .and_then(|()| std::fs::rename(&part, &path).map_err(|e| e.to_string()));
        if received.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        // Waiting for the rest of the batch holds no stream slot: a batch may be as large as
        // the daemon's limit on open streams.
        drop(open);
        // From here on the sender may still give up: an abort on this stream ends the batch,
        // the last file's too while its paths wait to be typed (`landed` decides under the lock
        // `fail` takes). A connection that merely closes does not (the sender only waits on its
        // last file's stream).
        std::thread::scope(|scope| {
            let watching = std::thread::Builder::new()
                .name("upload-abort".into())
                .spawn_scoped(scope, || {
                    if let Ok(Some(StreamFrame::Close(_))) = stream::read_frame(&mut reader) {
                        let mut st = self.state.lock();
                        self.fail(&mut st, serial, "the upload was cancelled".into());
                    }
                });
            // Without a way to hear a cancel, the batch is not kept waiting on one.
            if let Err(e) = watching {
                let mut st = self.state.lock();
                self.fail(&mut st, serial, format!("cannot watch the upload: {e}"));
            }
            match received {
                Ok(()) => self.landed(daemon, serial, index),
                Err(e) => {
                    let mut st = self.state.lock();
                    self.fail(&mut st, serial, e);
                }
            }
            self.writer_done(serial, index);
            match self.outcome(serial) {
                Ok(()) => close(&mut writer, StreamClose::Ok),
                Err(e) => close(&mut writer, StreamClose::Internal(e)),
            }
        });
    }

    fn serve_download<R: Read>(
        &self,
        pane: PaneId,
        path: &Path,
        size: u64,
        stamp: u64,
        mut reader: R,
        mut writer: UnixStream,
    ) {
        {
            let mut st = self.state.lock();
            let n = st.downloads.entry(pane).or_default();
            if *n >= MAX_DOWNLOADS_PER_PANE {
                drop(st);
                return close(
                    &mut writer,
                    StreamClose::Refused(format!(
                        "too many downloads from this pane at once (at most {MAX_DOWNLOADS_PER_PANE})"
                    )),
                );
            }
            *n += 1;
        }
        let sent = send_file(&mut writer, path, size, stamp);
        {
            let mut st = self.state.lock();
            if let Some(n) = st.downloads.get_mut(&pane) {
                *n -= 1;
                if *n == 0 {
                    st.downloads.remove(&pane);
                }
            }
        }
        if let Err(e) = sent {
            return close(&mut writer, StreamClose::Internal(e));
        }
        // The caller says it has it all, or gives up.
        loop {
            match stream::read_frame(&mut reader) {
                Ok(Some(StreamFrame::End)) => return close(&mut writer, StreamClose::Ok),
                Ok(Some(StreamFrame::Data(_))) => {}
                Ok(Some(StreamFrame::Close(_))) | Ok(None) | Err(_) => {
                    let _ = writer.shutdown(std::net::Shutdown::Both);
                    return;
                }
            }
        }
    }
}

/// Prunes kept uploads now, and then every [`PRUNE_EVERY`] (`THURM_TRANSFER_PRUNE_MS` in
/// tests) for as long as the daemon runs.
pub fn start_pruning(daemon: &Arc<Daemon>) {
    let every = env_ms("THURM_TRANSFER_PRUNE_MS", PRUNE_EVERY);
    let weak = Arc::downgrade(daemon);
    let started = std::thread::Builder::new()
        .name("transfer-prune".into())
        .spawn(move || {
            while let Some(daemon) = weak.upgrade() {
                daemon.transfers.prune();
                drop(daemon);
                std::thread::sleep(every);
            }
        });
    if let Err(e) = started {
        log::warn!("kept uploads are pruned only when files are sent: {e}");
    }
}

/// Carries an opened upload or download stream (see [`crate::streams::serve`] for TCP).
pub fn serve<R: Read + Send>(
    daemon: &Arc<Daemon>,
    kind: Kind,
    reader: R,
    mut writer: UnixStream,
    open: OpenStream,
) {
    let t = &daemon.transfers;
    match kind {
        Kind::Upload { batch, index } => t.serve_upload(daemon, batch, index, reader, writer, open),
        Kind::Download {
            pane,
            path,
            size,
            stamp,
        } => t.serve_download(pane, &path, size, stamp, reader, writer),
        Kind::Tcp { .. } => close(
            &mut writer,
            StreamClose::Internal("not a file stream".into()),
        ),
    }
}

fn close(writer: &mut UnixStream, why: StreamClose) {
    let _ = stream::write_frame(writer, &StreamFrame::Close(why));
    let _ = writer.shutdown(std::net::Shutdown::Both);
}

/// Writes the stream's data to `part` (a new private file) until `End`: exactly `size` bytes.
fn receive<R: Read>(
    reader: &mut R,
    part: &Path,
    size: u64,
    mut progress: impl FnMut() -> bool,
) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(part)
        .map_err(|e| format!("cannot write {}: {e}", part.display()))?;
    let mut written = 0u64;
    loop {
        match stream::read_frame(reader) {
            Ok(Some(StreamFrame::Data(data))) => {
                written += data.len() as u64;
                if written > size {
                    return Err(format!("more than the {size} bytes announced arrived"));
                }
                file.write_all(&data)
                    .map_err(|e| format!("cannot write {}: {e}", part.display()))?;
                if !progress() {
                    return Err("the upload was cancelled".into());
                }
            }
            Ok(Some(StreamFrame::End)) => {
                if written != size {
                    return Err(format!("{written} of the {size} bytes announced arrived"));
                }
                return file
                    .sync_all()
                    .map_err(|e| format!("cannot write {}: {e}", part.display()));
            }
            Ok(Some(StreamFrame::Close(_))) => return Err("the upload was cancelled".into()),
            Ok(None) | Err(_) => return Err("the connection was lost".into()),
        }
    }
}

/// Sends the `size` bytes of `path`, then `End`: only while it is the file stamped `expected`
/// when asked for, before and after it is read, so new bytes never go out under the old
/// version.
fn send_file(writer: &mut UnixStream, path: &Path, size: u64, expected: u64) -> Result<(), String> {
    let file =
        open_nonblocking(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || stamp(&meta) != expected {
        return Err(format!("{} changed since it was asked for", path.display()));
    }
    let mut file = file.take(size);
    let mut buf = vec![0u8; MAX_DATA];
    let mut sent = 0u64;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        stream::write_data(writer, &buf[..n]).map_err(|e| e.to_string())?;
        sent += n as u64;
    }
    if sent != size {
        return Err(format!(
            "{} got shorter while it was sent ({sent} of {size} bytes)",
            path.display()
        ));
    }
    let after = file.get_ref().metadata().map_err(|e| e.to_string())?;
    if stamp(&after) != expected {
        return Err(format!("{} changed while it was sent", path.display()));
    }
    stream::write_frame(writer, &StreamFrame::End).map_err(|e| e.to_string())
}

/// Opens `path` to read without blocking on a FIFO or device put in its place.
fn open_nonblocking(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

/// `path` as the pane sees it: `~` is `home`, a relative path is under `cwd`.
fn resolve(path: &str, cwd: Option<&str>, home: Option<&str>) -> Result<PathBuf, String> {
    if path.is_empty() || path.contains('\0') {
        return Err("no file named".into());
    }
    let home = || {
        home.filter(|h| h.starts_with('/'))
            .ok_or("the home directory is not known")
    };
    if path == "~" {
        return Ok(PathBuf::from(home()?));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(Path::new(home()?).join(rest));
    }
    if path.starts_with('~') {
        return Err(format!("{path}: only your own home directory (~) is known"));
    }
    if path.starts_with('/') {
        return Ok(PathBuf::from(path));
    }
    let cwd = cwd
        .filter(|c| c.starts_with('/'))
        .ok_or("the pane's working directory is not known; use an absolute path")?;
    Ok(Path::new(cwd).join(path))
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

/// The file's device, inode, size, modification and change times, hashed. A write changes the
/// change time even when the modification time is set back afterwards.
pub(crate) fn stamp(meta: &std::fs::Metadata) -> u64 {
    // FNV-1a: stable across builds, unlike the standard library's hasher.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for word in [
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime() as u64,
        meta.mtime_nsec() as u64,
        meta.ctime() as u64,
        meta.ctime_nsec() as u64,
    ] {
        for b in word.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// [`stamp`] as a version; `None` when the file was modified or changed in the last second
/// (another write may follow within the clock's resolution).
fn version(meta: &std::fs::Metadata) -> Option<String> {
    let changed = SystemTime::UNIX_EPOCH
        + Duration::new(
            meta.ctime().max(0) as u64,
            meta.ctime_nsec().clamp(0, 999_999_999) as u32,
        );
    let recent = [meta.modified().ok(), Some(changed)].into_iter().any(|t| {
        t.and_then(|t| t.elapsed().ok())
            .is_none_or(|age| age < Duration::from_secs(1))
    });
    if recent {
        return None;
    }
    Some(format!("{:016x}", stamp(meta)))
}

/// The name a file gets: its last path component without control characters, at most
/// [`NAME_MAX`] bytes; `file` when nothing is left.
pub fn clean_name(name: &str) -> String {
    let last = name.rsplit('/').next().unwrap_or_default();
    let cleaned: String = last.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return "file".into();
    }
    let (stem, ext) = split_ext(cleaned);
    fit(stem, ext, "")
}

/// `name`, or with `-2`, `-3`… before its extension when taken; recorded in `taken`.
fn unique_name(taken: &mut HashSet<String>, name: &str) -> String {
    let (stem, ext) = split_ext(name);
    let mut candidate = name.to_owned();
    let mut n = 1u32;
    while taken.contains(&candidate) {
        n += 1;
        candidate = fit(stem, ext, &format!("-{n}"));
    }
    taken.insert(candidate.clone());
    candidate
}

/// `name` split before its extension (`.png`); a leading dot is part of the stem.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 16 => name.split_at(i),
        _ => (name, ""),
    }
}

/// `stem` + `suffix` + `ext` in at most [`NAME_MAX`] bytes, shortening the stem.
fn fit(stem: &str, ext: &str, suffix: &str) -> String {
    let room = NAME_MAX.saturating_sub(ext.len() + suffix.len());
    let mut end = stem.len().min(room);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}{ext}", &stem[..end])
}

/// Records that the batch in `dir` is complete now ([`DONE`]), for as long as it is kept.
fn record_done(dir: &Path) {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join(DONE))
        .and_then(|mut f| {
            f.write_all(now.to_string().as_bytes())?;
            f.sync_all()
        });
    if let Err(e) = written {
        log::warn!("{}: cannot record when it was complete: {e}", dir.display());
    }
}

/// What a kept batch is aged from: when it was complete ([`DONE`]), or, for one that never was
/// (a daemon stopped while it came) or whose record cannot be read, when anything in it was
/// last written.
fn aged_from(dir: &Path) -> SystemTime {
    let done = std::fs::read_to_string(dir.join(DONE))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok());
    match done {
        Some(ms) => SystemTime::UNIX_EPOCH + Duration::from_millis(ms),
        None => last_write(dir),
    }
}

/// The latest modification time of `dir` and of everything in it.
fn last_write(dir: &Path) -> SystemTime {
    let own = std::fs::metadata(dir)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return own;
    };
    entries.flatten().fold(own, |latest, e| {
        let t = match e.file_type() {
            Ok(t) if t.is_dir() => last_write(&e.path()),
            _ => e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH),
        };
        latest.max(t)
    })
}

/// Deletes a kept batch; whether it is gone. What could not be deleted is logged and kept.
fn remove_batch(dir: &Path) -> bool {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            log::warn!("cannot delete kept upload {}: {e}", dir.display());
            false
        }
    }
}

fn make_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let inside: u64 = entries
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some(if m.is_dir() {
                dir_size(&e.path())
            } else if e.file_name() == DONE {
                // A few bytes: a block.
                block()
            } else {
                // What the disk allocated, when it is more (counted in bytes in tests).
                let allocated = if block() >= 512 {
                    m.blocks().saturating_mul(512)
                } else {
                    0
                };
                charge(m.len()).max(allocated)
            })
        })
        .sum();
    inside.saturating_add(block())
}

/// What a file of `len` bytes is counted as taking: whole blocks, one at least.
fn charge(len: u64) -> u64 {
    len.div_ceil(block()).max(1).saturating_mul(block())
}

/// How many more files the disk holding `dir` can take; `THURM_TRANSFER_FREE_FILES` in tests
/// (the count with no uploads kept; what is kept takes from it).
fn free_files(dir: &Path) -> Result<u64, String> {
    if let Some(free) = std::env::var("THURM_TRANSFER_FREE_FILES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Ok(free.saturating_sub(count_entries(dir)));
    }
    #[allow(clippy::unnecessary_cast)]
    statvfs(dir).map(|s| s.f_favail as u64)
}

fn count_entries(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            1 + if e.path().is_dir() {
                count_entries(&e.path())
            } else {
                0
            }
        })
        .sum()
}

fn statvfs(dir: &Path) -> Result<libc::statvfs, String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(format!(
            "cannot tell the free space of {}: {}",
            dir.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(s)
}

/// Bytes free for this user on the disk holding `dir`; `THURM_TRANSFER_FREE_BYTES` in tests.
fn free_space(dir: &Path) -> Result<u64, String> {
    // Tests: the disk's free bytes with no uploads kept; what is kept takes from it.
    if let Some(free) = std::env::var("THURM_TRANSFER_FREE_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Ok(free.saturating_sub(dir_size(dir)));
    }
    let s = statvfs(dir)?;
    #[allow(clippy::unnecessary_cast)]
    Ok((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_directory_that_cannot_be_made_whole_is_not_left() {
        let root = std::env::temp_dir().join(format!("thurm-batch-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let t = Transfers::new(root.clone());
        let e = t
            .new_batch_dir_with(3, |_| Err(std::io::Error::other("no inodes left")))
            .unwrap_err();
        assert!(e.contains("no inodes left"), "{e}");
        let left: Vec<_> = std::fs::read_dir(root.join("3"))
            .unwrap()
            .flatten()
            .collect();
        assert!(left.is_empty(), "{left:?}");
        let dir = t.new_batch_dir(3).unwrap();
        assert!(dir.join(PARTS).is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_keep_their_last_component_without_control_characters() {
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name("a\nb\x1b[31m.txt"), "ab[31m.txt");
        assert_eq!(clean_name("dir/"), "file");
        assert_eq!(clean_name(".."), "file");
        assert_eq!(clean_name("  "), "file");
        assert_eq!(clean_name(".bashrc"), ".bashrc");
    }

    #[test]
    fn long_names_are_cut_on_a_character_keeping_the_extension() {
        let long = format!("{}.png", "é".repeat(200));
        let name = clean_name(&long);
        assert!(name.len() <= NAME_MAX, "{}", name.len());
        assert!(name.ends_with(".png"));
        let mut taken = HashSet::from([name.clone()]);
        let second = unique_name(&mut taken, &name);
        assert!(second.len() <= NAME_MAX);
        assert!(second.ends_with("-2.png"), "{second}");
    }

    #[test]
    fn colliding_names_get_a_number() {
        let mut taken = HashSet::from([PARTS.to_owned(), DONE.to_owned()]);
        assert_eq!(unique_name(&mut taken, "a.txt"), "a.txt");
        assert_eq!(unique_name(&mut taken, "a.txt"), "a-2.txt");
        assert_eq!(unique_name(&mut taken, "a.txt"), "a-3.txt");
        assert_eq!(unique_name(&mut taken, "Makefile"), "Makefile");
        assert_eq!(unique_name(&mut taken, "Makefile"), "Makefile-2");
        assert_eq!(unique_name(&mut taken, ".parts"), ".parts-2");
    }

    #[test]
    fn paths_resolve_as_the_pane_sees_them() {
        let (cwd, home) = (Some("/work"), Some("/home/me"));
        assert_eq!(
            resolve("a.txt", cwd, home).unwrap(),
            Path::new("/work/a.txt")
        );
        assert_eq!(
            resolve("/etc/hosts", None, None).unwrap(),
            Path::new("/etc/hosts")
        );
        assert_eq!(resolve("~", cwd, home).unwrap(), Path::new("/home/me"));
        assert_eq!(resolve("~/x", cwd, home).unwrap(), Path::new("/home/me/x"));
        assert!(resolve("~root/x", cwd, home).is_err());
        assert!(resolve("a.txt", None, home).is_err());
        assert!(resolve("~/x", cwd, None).is_err());
        assert!(resolve("", cwd, home).is_err());
    }
}
