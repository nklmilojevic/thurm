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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::{Condvar, Mutex};
use thurm_proto::stream::{self, MAX_DATA, StreamClose, StreamFrame};
use thurm_proto::{DownloadInfo, PaneId, Principal, UploadBatch};

use crate::daemon::Daemon;
use crate::streams::{Kind, OpenStream};

/// The largest file one upload takes.
pub const MAX_FILE: u64 = 4 << 30;
/// The most files in one batch.
pub const MAX_BATCH: u32 = thurm_proto::MAX_UPLOAD_BATCH;
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
/// Files of batches not complete yet, over all, whether on their way or landed and waiting for
/// the rest: each takes a stream (and threads) once sent.
pub const MAX_WAITING_FILES: usize = 512;
/// Drops on disk, kept or on their way, over all panes, however small.
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
/// How long a download that sent its file waits for its caller to say it has it.
pub const ACK: Duration = Duration::from_secs(30);
/// Downloads sending at once, per pane.
pub const MAX_DOWNLOADS_PER_PANE: usize = 4;
/// The longest file name, in bytes.
const NAME_MAX: usize = 255;
/// Where files are written until whole, inside a batch's directory.
const PARTS: &str = ".parts";
/// When a batch was complete (milliseconds since the Unix epoch), inside its directory: what it
/// is aged from, by this daemon and the next.
const DONE: &str = ".done";

/// A kept batch: since when it is aged, what it takes on disk, and in files.
#[derive(Clone)]
struct Kept {
    since: SystemTime,
    size: u64,
    files: u64,
    dir: PathBuf,
}

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
    /// [`ACK`], or `THURM_TRANSFER_ACK_MS` (tests).
    ack: Duration,
    /// [`MAX_WAITING_FILES`], or `THURM_TRANSFER_MAX_WAITING_FILES` (tests).
    max_waiting_files: usize,
    /// [`MAX_KEPT_BATCHES`], or `THURM_TRANSFER_MAX_DROPS` (tests).
    max_drops: usize,
    /// How long a stream ticket lasts unused.
    ticket_ttl: Duration,
    /// What batches' pulses count from.
    epoch: Instant,
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
    /// The last walk of the kept drops, and when: reused for a moment (the files of one drop
    /// come one after the other), forgotten whenever the daemon changes a batch or deletes one.
    kept_seen: Option<(Instant, Vec<Kept>)>,
}

/// How long a walk of the kept drops is reused.
const KEPT_FRESH: Duration = Duration::from_secs(1);

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
    /// When anything last happened to it, and whether it is still on its way: shared with its
    /// receiving streams, which mark it on every frame without the transfers lock.
    pulse: Arc<Pulse>,
    /// The streams writing a file of the batch now, by its index: shut when the batch fails,
    /// and until they have let go, the batch's directory stays and its bytes count.
    writers: Vec<(u32, UnixStream)>,
}

/// A batch's last sign of life, kept without the transfers lock: an upload's every frame marks
/// it, and an admission walking or pruning the kept drops under that lock holds up none of them.
struct Pulse {
    /// Milliseconds since `Transfers::epoch`.
    at: AtomicU64,
    /// The batch is still on its way (it has not failed).
    on_way: AtomicBool,
}

impl Pulse {
    fn new(epoch: Instant) -> Arc<Pulse> {
        let p = Arc::new(Pulse {
            at: AtomicU64::new(0),
            on_way: AtomicBool::new(true),
        });
        p.touch(epoch);
        p
    }

    fn touch(&self, epoch: Instant) {
        self.at
            .store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    /// How long since it was last marked.
    fn idle(&self, epoch: Instant) -> Duration {
        let now = epoch.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.at.load(Ordering::Relaxed)))
    }
}

struct Slot {
    path: PathBuf,
    size: u64,
    state: SlotState,
    /// When its ticket was given: unused past the ticket's life, the file can never come.
    reserved: Instant,
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
            ack: env_ms("THURM_TRANSFER_ACK_MS", ACK),
            max_waiting_files: std::env::var("THURM_TRANSFER_MAX_WAITING_FILES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_WAITING_FILES),
            max_drops: std::env::var("THURM_TRANSFER_MAX_DROPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(MAX_KEPT_BATCHES),
            ticket_ttl: crate::streams::ticket_ttl(),
            epoch: Instant::now(),
        }
    }

    /// Ends stalled batches and deletes kept uploads past their time or over the limit, those
    /// left by an earlier daemon included: what keeps uploads within their promise when no
    /// new one comes.
    pub fn prune(&self) {
        let mut st = self.state.lock();
        self.sweep(&mut st);
        st.kept_seen = None;
        let kept = self.kept(&mut st);
        let busy = st.batches.values().filter(|b| b.busy()).count();
        self.make_room(kept, busy, 0, 0, 0);
        st.kept_seen = None;
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
        // Every file admitted will hold a stream until its batch is complete: they count from
        // their reservation, not from when they land.
        let waiting_files = st
            .batches
            .values()
            .filter(incomplete)
            .map(|b| b.files.len())
            .sum::<usize>();
        if waiting_files >= self.max_waiting_files {
            return Err(format!(
                "too many files are on their way in uploads not complete yet (at most {})",
                self.max_waiting_files
            ));
        }
        // What files on their way have yet to take: all of one not started, the rest of one
        // being written (what it wrote is out of the free space already).
        let pending: u64 = st
            .batches
            .values()
            .flat_map(|b| {
                b.files
                    .iter()
                    .filter(move |(_, f)| on_way(b, f))
                    .map(move |(index, f)| (b, *index, f))
            })
            .map(|(b, index, f)| match f.state {
                SlotState::Writing => {
                    let written = std::fs::metadata(b.dir.join(PARTS).join(index.to_string()))
                        .map_or(0, |m| m.len());
                    charge(f.size.saturating_sub(written))
                }
                _ => charge(f.size),
            })
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
        // A batch on its way is not among the drops kept: its directories (its own and its
        // parts') and the record it leaves once complete count here, beside its files.
        let on_way = st.batches.values().filter(|b| b.busy()).count() as u64;
        let on_way_dirs = on_way.saturating_mul(3 * block());
        // With its directories and its record, should it be the batch's first.
        let dir_cost = if existing.is_none() { 2 * block() } else { 0 };
        let record = if existing.is_none() { block() } else { 0 };
        let incoming = active
            .saturating_add(on_way_dirs)
            .saturating_add(charge(size))
            .saturating_add(dir_cost)
            .saturating_add(record);
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
        // Drops past their time go first (`kept` deletes them): the space and files they leave
        // count before any are found missing. What the rest would give back if deleted is what
        // admitting this file may still take from them.
        let kept_now = self.kept(&mut st);
        let recoverable: u64 = kept_now.iter().map(|k| k.size).sum();
        let recoverable_files: u64 = kept_now.iter().map(|k| k.files).sum();
        // A new batch's directory and its parts directory (and its pane's, for the pane's first),
        // and its record of being complete; the file and its part.
        let new_dirs = if existing.is_none() {
            2 + u64::from(std::fs::symlink_metadata(self.root.join(pane.to_string())).is_err())
        } else {
            0
        };
        let files_needed = new_dirs + u64::from(existing.is_none()) + 2;
        let files = free_files(&self.root)?;
        let files_short = (MIN_FREE_FILES + files_needed).saturating_sub(files);
        let too_few_files =
            |files: u64| format!("this host's disk can take too few more files ({files} left)");
        if files_short > recoverable_files {
            return Err(too_few_files(files));
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
        if short > recoverable {
            return Err(no_space(free));
        }
        // A new batch's number is used up here, whatever follows: a ticket left by a refusal
        // below names a file that does not exist (its stream is refused), never another batch's.
        let serial = existing.unwrap_or_else(|| {
            st.next += 1;
            st.next
        });
        let claimant = principal.clone();
        let ticket = daemon.streams.reserve(
            Kind::Upload {
                batch: serial,
                index,
            },
            client,
            principal,
        )?;
        // A refusal from here on gives the ticket back (claimed and dropped): refused uploads
        // retried do not use up the client's tickets.
        let release = || {
            let _ = daemon.streams.claim(&ticket.0, &claimant);
        };
        // A new batch's directory first: if it cannot be made, nothing kept was deleted for it.
        let created = existing.is_none();
        if created {
            let dir = match self.new_batch_dir(pane) {
                Ok(dir) => dir,
                Err(e) => {
                    release();
                    return Err(e);
                }
            };
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
                    pulse: Pulse::new(self.epoch),
                    writers: Vec::new(),
                },
            );
        }
        let undo = |st: &mut State| {
            if created && let Some(b) = st.batches.remove(&serial) {
                remove_batch(&b.dir);
            }
            release();
        };
        // Only an admitted file makes older drops give way, and only what was deleted counts.
        // The drops looked at above, not walked again: the new batch is not among them.
        // Drops on their way count toward the drops kept, this new one included.
        let busy = st.batches.values().filter(|b| b.busy()).count();
        let (room, deleted) = self.make_room(kept_now, busy, incoming, short, files_short);
        // The walk stands for the next file unless something in it went.
        if deleted {
            st.kept_seen = None;
        }
        if !room {
            undo(&mut st);
            return Err(too_much());
        }
        if files_short > 0 {
            match free_files(&self.root) {
                // The new batch's directories are made already: the rest remain.
                Ok(files) if files >= MIN_FREE_FILES + files_needed - new_dirs => {}
                Ok(files) => {
                    undo(&mut st);
                    return Err(too_few_files(files));
                }
                Err(e) => {
                    undo(&mut st);
                    return Err(e);
                }
            }
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
                reserved: Instant::now(),
            },
        );
        b.pulse.touch(self.epoch);
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
        st.kept_seen = None;
        let Some(b) = st.batches.get_mut(&serial) else {
            return;
        };
        log::info!("upload to pane {} failed: {why}", b.pane);
        b.outcome = Some(Err(why));
        b.pulse.on_way.store(false, Ordering::Relaxed);
        b.pulse.touch(self.epoch);
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
            .filter(|(_, b)| b.waiting() && !b.pasting && b.pulse.idle(self.epoch) > self.idle)
            .map(|(serial, _)| *serial)
            .collect();
        for serial in stalled {
            self.fail(st, serial, "the upload stalled".into());
        }
        // A file whose ticket ran out unused can never come (its stream never opened): its
        // batch goes now, not when it would stall.
        let lapsed = self.ticket_ttl + Duration::from_millis(200);
        let never: Vec<u64> = st
            .batches
            .iter()
            .filter(|(_, b)| b.waiting() && !b.pasting)
            .filter(|(_, b)| {
                b.files
                    .values()
                    .any(|f| f.state == SlotState::Reserved && f.reserved.elapsed() > lapsed)
            })
            .map(|(serial, _)| *serial)
            .collect();
        for serial in never {
            self.fail(st, serial, "a file of the upload never came".into());
        }
        st.batches
            .retain(|_, b| b.busy() || b.pulse.idle(self.epoch) <= self.idle);
    }

    /// Gives a file the name `taken` was refused for (the disk holds a file it takes for the
    /// same, by case or Unicode form): the next free one in its batch. Its new path, or `None`
    /// once the batch is gone.
    fn rename_slot(&self, serial: u64, index: u32, taken: &Path) -> Option<PathBuf> {
        let mut st = self.state.lock();
        let b = st.batches.get_mut(&serial)?;
        let name = taken.file_name()?.to_string_lossy().into_owned();
        let renamed = unique_name(&mut b.names, &name);
        let path = b.dir.join(renamed);
        b.files.get_mut(&index)?.path = path.clone();
        Some(path)
    }

    /// The writer of file `index` of batch `serial` let go: a failed batch's file stops
    /// counting, and its directory goes with the last writer.
    fn writer_done(&self, serial: u64, index: u32) {
        let mut st = self.state.lock();
        let Some(b) = st.batches.get_mut(&serial) else {
            return;
        };
        b.writers.retain(|(i, _)| *i != index);
        // A batch whose last writer left is no longer in use: a walk of the kept drops that
        // left it out is stale.
        st.kept_seen = None;
        let Some(b) = st.batches.get_mut(&serial) else {
            return;
        };
        let failed = matches!(b.outcome, Some(Err(_)));
        if failed {
            if let Some(f) = b.files.get_mut(&index) {
                f.state = SlotState::Gone;
            }
            if b.writers.is_empty() {
                remove_batch(&b.dir);
            }
        }
        if failed {
            st.kept_seen = None;
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
    fn kept(&self, st: &mut State) -> Vec<Kept> {
        if let Some((at, kept)) = &st.kept_seen
            && at.elapsed() < KEPT_FRESH
        {
            return kept.clone();
        }
        let kept = self.walk_kept(st);
        st.kept_seen = Some((Instant::now(), kept.clone()));
        kept
    }

    /// [`Transfers::kept`], walking the disk.
    fn walk_kept(&self, st: &State) -> Vec<Kept> {
        let busy: HashSet<&Path> = st
            .batches
            .values()
            .filter(|b| b.busy())
            .map(|b| b.dir.as_path())
            .collect();
        let mut kept = Vec::new();
        if let Ok(panes) = std::fs::read_dir(&self.root) {
            // Only the tree's own directories: a link put in a pane's or a batch's place is not
            // followed, so nothing outside it is walked or deleted.
            let is_dir = |e: &std::fs::DirEntry| e.file_type().is_ok_and(|t| t.is_dir());
            for pane in panes.flatten().filter(is_dir) {
                let Ok(batches) = std::fs::read_dir(pane.path()) else {
                    continue;
                };
                for batch in batches.flatten().filter(is_dir) {
                    let dir = batch.path();
                    if busy.contains(dir.as_path()) {
                        continue;
                    }
                    // One pass over it: what it takes, in bytes and files, and when it was last
                    // written; aged from when it was complete, if it was.
                    let seen = survey(&dir);
                    let since = done_at(&dir).unwrap_or(seen.latest);
                    let old = since.elapsed().is_ok_and(|age| age > self.keep);
                    if !(old && remove_batch(&dir)) {
                        kept.push(Kept {
                            since,
                            size: seen.size,
                            files: seen.files + 1,
                            dir,
                        });
                    }
                }
                // Gone once empty (a closed pane's).
                let _ = std::fs::remove_dir(pane.path());
            }
        }
        kept.sort_by_key(|k| k.since);
        kept
    }

    /// Deletes the oldest kept batches until `incoming` more bytes stay within the kept limit
    /// and at least `free` bytes and `files` files were freed. A batch that could not be deleted
    /// still counts. Whether all hold now.
    /// `busy` drops on their way (on disk, not among `kept`) count toward the drops kept.
    /// Returns whether all hold now, and whether any was deleted.
    fn make_room(
        &self,
        kept: Vec<Kept>,
        busy: usize,
        incoming: u64,
        free: u64,
        files: u64,
    ) -> (bool, bool) {
        let mut total: u64 = kept.iter().map(|k| k.size).sum();
        let (mut freed, mut freed_files) = (0u64, 0u64);
        let mut count = kept.len();
        let fits = |total: u64, freed: u64, freed_files: u64, count: usize| {
            total.saturating_add(incoming) <= self.max_kept
                && freed >= free
                && freed_files >= files
                && count + busy <= self.max_drops
        };
        // Planned first, oldest first, from the drops that can be deleted: if they are not
        // enough, nothing is deleted for a file that is refused all the same.
        let mut doomed = Vec::new();
        for k in kept {
            if fits(total, freed, freed_files, count) {
                break;
            }
            if deletable(&k.dir) {
                total = total.saturating_sub(k.size);
                freed += k.size;
                freed_files += k.files;
                count -= 1;
                doomed.push(k);
            }
        }
        if !fits(total, freed, freed_files, count) {
            return (false, false);
        }
        let mut deleted = false;
        let mut ok = true;
        for k in doomed {
            if remove_batch(&k.dir) {
                deleted = true;
            } else {
                // Deletable as far as could be told, and still not deleted: it counts.
                ok = false;
            }
        }
        (ok, deleted)
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
            b.pulse.touch(self.epoch);
        }
        if !self.paste_delay.is_zero() {
            std::thread::sleep(self.paste_delay);
        }
        // Decided under the lock `fail` takes: a cancel or a client gone before this point fails
        // the batch, and nothing is typed. Typed without it (a paste can set off anything,
        // ending a client, whose uploads then take this lock): once `pasting` is set, a
        // failure waits for the outcome, and a client leaving waits for the paste
        // (`Daemon::paste_for`), which refuses one already gone.
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
        drop(st);
        let typed = daemon.paste_for(client, pane, &text);
        let mut st = self.state.lock();
        if let Some(b) = st.batches.get_mut(&serial) {
            b.pasting = false;
            b.pulse.touch(self.epoch);
            match typed {
                Ok(()) => {
                    record_done(&b.dir);
                    b.outcome = Some(Ok(()));
                    st.kept_seen = None;
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
            if !b.pasting && b.pulse.idle(self.epoch) > self.idle {
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
                            b.pulse.touch(self.epoch);
                            b.writers.push((index, socket));
                            Ok((f.path.clone(), f.size, b.pulse.clone()))
                        }
                        Err(e) => Err(format!("cannot take the file: {e}")),
                    },
                    _ => Err("that file is not expected".to_owned()),
                },
            }
        };
        let (path, size, pulse) = match slot {
            Ok(slot) => slot,
            Err(e) => return close(&mut writer, StreamClose::Refused(e)),
        };
        let part = path
            .parent()
            .unwrap_or(Path::new("/"))
            .join(PARTS)
            .join(index.to_string());
        // A sender that stops mid-file is not waited on past the batch's idle time.
        let _ = writer.set_read_timeout(Some(self.idle));
        let alive = || {
            pulse.touch(self.epoch);
            pulse.on_way.load(Ordering::Relaxed)
        };
        let received = receive(&mut reader, &part, size, alive).and_then(|()| {
            // Into its place without replacing anything there: a name the disk takes for one
            // already kept (case, Unicode form) is numbered on, as names that match are.
            let mut path = path.clone();
            for _ in 0..100 {
                match std::fs::hard_link(&part, &path) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&part);
                        return Ok(());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        path = self
                            .rename_slot(serial, index, &path)
                            .ok_or("the upload was cancelled")?;
                    }
                    Err(e) => return Err(format!("cannot keep {}: {e}", path.display())),
                }
            }
            Err(format!("no free name for {}", path.display()))
        });
        let _ = writer.set_read_timeout(None);
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
                    // An abort, not a close a sender may send once all is said: only that ends
                    // the batch.
                    if let Ok(Some(StreamFrame::Close(why))) = stream::read_frame(&mut reader)
                        && why != StreamClose::Ok
                    {
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
        // The slot is the stream's until it ends, its caller's answer awaited included.
        struct Slot<'a>(&'a Transfers, PaneId);
        impl Drop for Slot<'_> {
            fn drop(&mut self) {
                let mut st = self.0.state.lock();
                if let Some(n) = st.downloads.get_mut(&self.1) {
                    *n -= 1;
                    if *n == 0 {
                        st.downloads.remove(&self.1);
                    }
                }
            }
        }
        let _slot = Slot(self, pane);
        let sent = send_file(&mut writer, path, size, stamp);
        if let Err(e) = sent {
            return close(&mut writer, StreamClose::Internal(e));
        }
        // The caller says it has it all, or gives up; one that says nothing is not waited on
        // for ever, holding the pane's download slot, nor one that keeps sending something
        // else, a frame at a time or a byte at a time: the wait has one deadline, not one per
        // frame or per read.
        let ended = {
            let mut until = Until {
                inner: &mut reader,
                socket: &writer,
                deadline: Instant::now() + self.ack,
            };
            loop {
                match stream::read_frame(&mut until) {
                    Ok(Some(StreamFrame::Data(_))) => {}
                    other => break matches!(other, Ok(Some(StreamFrame::End))),
                }
            }
        };
        if ended {
            close(&mut writer, StreamClose::Ok);
        } else {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Reads from `inner` only until `deadline`, however the bytes trickle in: each read waits on
/// `socket` (the one `inner` reads) for no more than what is left.
struct Until<'a, R> {
    inner: R,
    socket: &'a UnixStream,
    deadline: Instant,
}

impl<R: Read> Read for Until<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.socket.set_read_timeout(Some(left))?;
        self.inner.read(buf)
    }
}

/// Lets the daemon hold as many descriptors as its uploads may need: each file of a batch keeps
/// its stream until the batch is complete, and up to [`MAX_WAITING_FILES`] may wait, beyond the
/// 256 a process often starts with. The soft limit is raised as far as the hard one allows (at
/// most 10 240, what macOS takes).
pub fn raise_fd_limit() {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0 {
        return;
    }
    let wanted = rl.rlim_max.min(10_240);
    if rl.rlim_cur >= wanted {
        return;
    }
    rl.rlim_cur = wanted;
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &rl) } != 0 {
        log::warn!(
            "cannot raise the open-file limit: {}",
            std::io::Error::last_os_error()
        );
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
        Kind::Tcp { .. } | Kind::TcpAt { .. } => close(
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
///
/// Names are told apart without regard to case (`taken` holds them lowercased): a host's disk
/// may not tell `Photo.jpg` from `photo.jpg`, nor `.Done` from the batch's `.done`.
fn unique_name(taken: &mut HashSet<String>, name: &str) -> String {
    let (stem, ext) = split_ext(name);
    let mut candidate = name.to_owned();
    let mut n = 1u32;
    while taken.contains(&candidate.to_lowercase()) {
        n += 1;
        candidate = fit(stem, ext, &format!("-{n}"));
    }
    taken.insert(candidate.to_lowercase());
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

/// When the batch in `dir` was complete ([`DONE`]); `None` for one that never was (a daemon
/// stopped while it came) or whose record cannot be read: it is aged from its last write.
fn done_at(dir: &Path) -> Option<SystemTime> {
    std::fs::read_to_string(dir.join(DONE))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
}

/// What a directory holds, found in one pass: what it takes on disk (as [`dir_size`] counts),
/// the files and directories in it, and when it or anything in it was last written.
struct Survey {
    size: u64,
    files: u64,
    latest: SystemTime,
}

fn survey(dir: &Path) -> Survey {
    let mut seen = Survey {
        size: block(),
        files: 0,
        latest: std::fs::metadata(dir)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH),
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return seen;
    };
    for e in entries.flatten() {
        let Ok(m) = e.metadata() else {
            continue;
        };
        seen.files += 1;
        if m.is_dir() {
            let inner = survey(&e.path());
            seen.size += inner.size;
            seen.files += inner.files;
            seen.latest = seen.latest.max(inner.latest);
        } else {
            seen.size += file_charge(&e.file_name(), &m);
            seen.latest = seen
                .latest
                .max(m.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        }
    }
    seen
}

/// What a file counts as taking on disk: whole blocks of its length, or what the disk
/// allocated if more; the completion record, a few bytes, a block.
fn file_charge(name: &std::ffi::OsStr, m: &std::fs::Metadata) -> u64 {
    if name == DONE {
        return block();
    }
    // What the disk allocated, when it is more (counted in bytes in tests).
    let allocated = if block() >= 512 {
        m.blocks().saturating_mul(512)
    } else {
        0
    };
    charge(m.len()).max(allocated)
}

/// Whether this process can delete the batch in `dir`: write and search on it, on every
/// directory in it, and on the directory holding it.
fn deletable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let can = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) } == 0)
    };
    fn dirs_ok(dir: &Path, can: &dyn Fn(&Path) -> bool) -> bool {
        can(dir)
            && std::fs::read_dir(dir).is_ok_and(|entries| {
                entries
                    .flatten()
                    .all(|e| !e.file_type().is_ok_and(|t| t.is_dir()) || dirs_ok(&e.path(), can))
            })
    }
    dir.parent().is_some_and(can) && dirs_ok(dir, &can)
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

/// Makes `dir` (and what leads to it) private; `dir` itself must be a directory, not a link to
/// one: what goes in it stays in the tree.
fn make_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| {
            if std::fs::symlink_metadata(dir)?.is_dir() {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            } else {
                Err(std::io::Error::other("not a directory"))
            }
        })
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
            } else {
                file_charge(&e.file_name(), &m)
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
            1 + if e.file_type().is_ok_and(|t| t.is_dir()) {
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
    fn links_in_the_tree_are_not_followed_to_what_they_point_at() {
        let base = std::env::temp_dir().join(format!("thurm-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("drops");
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("victim")).unwrap();
        std::fs::write(outside.join("victim/file"), b"keep me").unwrap();
        std::fs::create_dir_all(root.join("8/old")).unwrap();
        // A pane's directory, and a batch's, put back as links to a folder elsewhere; and a
        // kept batch holding a link to itself.
        std::os::unix::fs::symlink(&outside, root.join("7")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("8/linked")).unwrap();
        std::os::unix::fs::symlink(".", root.join("8/old/loop")).unwrap();
        let mut t = Transfers::new(root.clone());
        t.keep = Duration::ZERO;
        t.prune();
        assert_eq!(
            std::fs::read(outside.join("victim/file")).unwrap(),
            b"keep me"
        );
        assert!(!root.join("8/old").exists());
        // Nor is a batch made through a pane's directory that is a link.
        let e = t.new_batch_dir(7).unwrap_err();
        assert!(e.contains("not a directory"), "{e}");
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            1,
            "a batch was made outside the tree"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn making_room_says_whether_anything_went() {
        let root = std::env::temp_dir().join(format!("thurm-make-room-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let t = Transfers::new(root.clone());
        let old = root.join("1/old");
        std::fs::create_dir_all(&old).unwrap();
        let kept = || {
            vec![Kept {
                since: SystemTime::UNIX_EPOCH,
                size: 10,
                files: 1,
                dir: old.clone(),
            }]
        };
        // Room already: nothing deleted, so a walk of the drops still stands.
        assert_eq!(t.make_room(kept(), 0, 10, 0, 0), (true, false));
        assert!(old.exists());
        // Room only once it goes.
        assert_eq!(t.make_room(kept(), 0, MAX_KEPT, 0, 0), (true, true));
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_differing_only_in_case_are_kept_apart() {
        let mut taken = HashSet::from([PARTS.to_owned(), DONE.to_owned()]);
        assert_eq!(unique_name(&mut taken, "Photo.jpg"), "Photo.jpg");
        assert_eq!(unique_name(&mut taken, "photo.jpg"), "photo-2.jpg");
        assert_eq!(unique_name(&mut taken, "PHOTO.JPG"), "PHOTO-3.JPG");
        assert_eq!(unique_name(&mut taken, ".Done"), ".Done-2");
    }

    #[test]
    fn the_open_file_limit_is_raised() {
        raise_fd_limit();
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) }, 0);
        assert!(
            rl.rlim_cur >= rl.rlim_max.min(10_240),
            "{} of {}",
            rl.rlim_cur,
            rl.rlim_max
        );
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
