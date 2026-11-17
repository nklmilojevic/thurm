//! Files to and from a host's daemon ([`thurm_proto::CAP_TRANSFER`]), each on a byte stream of
//! its own: dropped and pasted files go up as one batch the daemon types the paths of once all
//! have landed; a file in a remote pane comes back to read here.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use thurm_client::stream::{self, StreamReader, StreamWriter};
use thurm_client::{Client, ClientError};
use thurm_proto::stream::{MAX_DATA, StreamClose};
use thurm_proto::{CAP_COPY, CAP_TRANSFER, DownloadInfo, PaneId, UploadBatch};

/// A host's daemon: the connection that asks, and the socket streams open on (the tunnel's
/// local end for a remote host).
#[derive(Clone, Copy)]
pub struct Host<'a> {
    pub client: &'a Client,
    pub socket: &'a Path,
}

/// Stops a transfer from another thread.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A file to send.
#[derive(Clone, Debug)]
pub enum Upload {
    /// A file on this machine, sent under its own name.
    Path(PathBuf),
    /// A file on this machine, sent as `name` (a pasted image saved under a temporary name).
    Named { name: String, path: PathBuf },
    /// Bytes, sent as `name`.
    Data { name: String, data: Vec<u8> },
}

/// How far a transfer got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}

#[derive(Debug)]
pub enum TransferError {
    /// The host's daemon cannot take or send files (an older version).
    Unsupported,
    Cancelled,
    /// A folder was given; only files are sent.
    Folder(String),
    /// The file is larger than the caller takes.
    TooLarge {
        size: u64,
        max: u64,
    },
    /// Reading or writing here.
    Local(String),
    /// The daemon refused, or the stream failed.
    Remote(String),
    /// Nothing is at the path asked for ([`thurm_proto::DOWNLOAD_NOT_FOUND`]).
    NotFound(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferError::Unsupported => f.write_str(thurm_client::NO_TRANSFER),
            TransferError::Cancelled => f.write_str("cancelled"),
            TransferError::Folder(name) => write!(f, "{name} is a folder; only files are sent"),
            TransferError::TooLarge { size, max } => write!(
                f,
                "the file is {} MiB, more than the {} MiB allowed",
                size.div_ceil(1 << 20),
                max >> 20
            ),
            TransferError::Local(e) | TransferError::Remote(e) | TransferError::NotFound(e) => {
                f.write_str(e)
            }
        }
    }
}

impl std::error::Error for TransferError {}

impl From<ClientError> for TransferError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Daemon(why) if why.ends_with(thurm_proto::DOWNLOAD_NOT_FOUND) => {
                TransferError::NotFound(why)
            }
            e => TransferError::Remote(e.to_string()),
        }
    }
}

/// An [`Upload`] opened to read.
struct Source<'a> {
    name: String,
    size: u64,
    reader: Box<dyn Read + Send + 'a>,
}

/// An [`Upload`] checked before the batch starts: its name and size, with nothing held open,
/// so a batch of many files needs one descriptor at a time.
struct Planned<'a> {
    upload: &'a Upload,
    name: String,
    size: u64,
}

fn shown_and_name(upload: &Upload, path: &Path) -> (String, String) {
    let shown = path.display().to_string();
    let name = match upload {
        Upload::Named { name, .. } => name.clone(),
        _ => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| shown.clone()),
    };
    (shown, name)
}

/// Checks `upload` without opening it (a FIFO is not waited on): a folder or anything but a
/// regular file is refused.
fn plan(upload: &Upload) -> Result<Planned<'_>, TransferError> {
    match upload {
        Upload::Data { name, data } => Ok(Planned {
            upload,
            name: name.clone(),
            size: data.len() as u64,
        }),
        Upload::Path(path) | Upload::Named { path, .. } => {
            let (shown, name) = shown_and_name(upload, path);
            let meta = std::fs::metadata(path)
                .map_err(|e| TransferError::Local(format!("{shown}: {e}")))?;
            if meta.is_dir() {
                return Err(TransferError::Folder(name));
            }
            if !meta.is_file() {
                return Err(TransferError::Local(format!(
                    "{shown} is not a regular file"
                )));
            }
            Ok(Planned {
                upload,
                name,
                size: meta.len(),
            })
        }
    }
}

/// Opens a planned upload to send it: still a regular file of the size planned.
fn open_planned<'a>(planned: &Planned<'a>) -> Result<Source<'a>, TransferError> {
    open_planned_with_mode(planned).map(|(source, _)| source)
}

/// [`open_planned`], with the permission bits of the file that was opened (none for bytes):
/// read from the open file, so they are the bits of the bytes sent whatever its name leads to
/// later.
fn open_planned_with_mode<'a>(
    planned: &Planned<'a>,
) -> Result<(Source<'a>, Option<u32>), TransferError> {
    let path = match planned.upload {
        // Read where they are, not copied.
        Upload::Data { data, .. } => {
            return Ok((
                Source {
                    name: planned.name.clone(),
                    size: planned.size,
                    reader: Box::new(std::io::Cursor::new(data.as_slice())),
                },
                None,
            ));
        }
        Upload::Path(path) | Upload::Named { path, .. } => path,
    };
    let shown = path.display().to_string();
    // Without blocking: a FIFO put in its place is refused below, not waited on.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| TransferError::Local(format!("{shown}: {e}")))?;
    let meta = file
        .metadata()
        .map_err(|e| TransferError::Local(format!("{shown}: {e}")))?;
    if !meta.is_file() || meta.len() != planned.size {
        return Err(TransferError::Local(format!(
            "{shown} changed since it was chosen"
        )));
    }
    let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777;
    Ok((
        Source {
            name: planned.name.clone(),
            size: planned.size,
            reader: Box::new(file),
        },
        Some(mode),
    ))
}

/// A batch name unlikely to meet another of this client's.
fn batch_id() -> String {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "{}-{nanos:x}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Sends `files` to `pane`'s host as one batch. Once all have landed, the daemon types their
/// paths into the pane; if one fails or this is cancelled, nothing is typed and nothing kept.
/// `progress` hears the bytes sent of the batch's total.
pub fn upload(
    host: Host,
    pane: PaneId,
    files: &[Upload],
    cancel: &Cancel,
    mut progress: impl FnMut(Progress),
) -> Result<(), TransferError> {
    if !host.client.has_capability(CAP_TRANSFER) {
        return Err(TransferError::Unsupported);
    }
    if files.is_empty() {
        return Ok(());
    }
    // Every file is checked before the first goes: a folder refuses the batch up front. Each
    // is opened only when its turn comes.
    // More than a batch holds is refused before any file goes, not at the first one over.
    if files.len() > thurm_proto::MAX_UPLOAD_BATCH as usize {
        return Err(TransferError::Local(format!(
            "at most {} files are sent at once",
            thurm_proto::MAX_UPLOAD_BATCH
        )));
    }
    let planned = files.iter().map(plan).collect::<Result<Vec<_>, _>>()?;
    let count = planned.len() as u32;
    let id = batch_id();
    let mut p = Progress {
        done: 0,
        total: planned.iter().map(|s| s.size).sum(),
    };
    progress(p);
    // The last file's stream, sent and finished: kept until the next one opens, so a failure
    // to start that one can still end the batch on the daemon (an aborted stream does; a
    // closed one does not, as the earlier files' streams close).
    let mut last: Option<(StreamReader, StreamWriter, UnixStream)> = None;
    for (index, planned) in planned.iter().enumerate() {
        if cancel.is_cancelled() {
            if let Some((_, w, h)) = last.take() {
                abort(w, &h);
            }
            return Err(TransferError::Cancelled);
        }
        let next = open_planned(planned).and_then(|source| {
            host.client
                .upload_begin(
                    pane,
                    &source.name,
                    source.size,
                    Some(UploadBatch {
                        id: id.clone(),
                        index: index as u32,
                        count,
                    }),
                )
                .map_err(TransferError::from)
                .and_then(|ticket| open_watched(host.socket, &ticket.ticket, cancel))
                .map(|opened| (source, opened))
        });
        let (mut source, (reader, writer, watch, handle)) = match next {
            Ok(next) => next,
            Err(e) => {
                if let Some((_, w, handle)) = last.take() {
                    abort(w, &handle);
                }
                return Err(e);
            }
        };
        // A cancel between files still has to reach the daemon: it ends the batch on this
        // stream, which is aborted unfinished.
        let sent = send_one(&mut source, writer, watch, &handle, cancel, |n| {
            p.done += n;
            progress(p);
        });
        let writer = match sent {
            Ok(writer) => writer,
            // This stream was aborted; the last one is too, so the batch ends now whichever
            // the daemon hears first.
            Err(e) => {
                if let Some((_, w, h)) = last.take() {
                    abort(w, &h);
                }
                return Err(e);
            }
        };
        // Every stream of the batch closes with the same outcome: waiting on the last one is
        // enough, and the others need no connection meanwhile.
        last = Some((reader, writer, handle));
    }
    let (reader, writer, handle) = last.expect("at least one file");
    wait_cancellable(reader, writer, handle, cancel)
}

/// Opens the stream `ticket` reserved, with a watch that cuts its connection once `cancel` is
/// set: a read or write blocked on a host that stopped then returns.
fn open_watched(
    socket: &Path,
    ticket: &str,
    cancel: &Cancel,
) -> Result<(StreamReader, StreamWriter, CancelWatch, UnixStream), TransferError> {
    let conn =
        thurm_client::connect_daemon(socket).map_err(|e| ClientError::Connect(e.to_string()))?;
    let clone = || {
        conn.try_clone()
            .map_err(|e| TransferError::Local(format!("cannot watch the transfer: {e}")))
    };
    let (shutdown, handle) = (clone()?, clone()?);
    open_watched_on(conn, shutdown, handle, ticket, cancel)
}

/// [`open_watched`] on a connection made: watched from before the handshake, so Cancel stops a
/// daemon slow to open the stream too.
fn open_watched_on(
    conn: UnixStream,
    shutdown: UnixStream,
    handle: UnixStream,
    ticket: &str,
    cancel: &Cancel,
) -> Result<(StreamReader, StreamWriter, CancelWatch, UnixStream), TransferError> {
    let watch = CancelWatch::start(cancel.clone(), shutdown)
        .map_err(|e| TransferError::Local(format!("cannot watch the transfer for Cancel: {e}")))?;
    match stream::open_on(conn, ticket) {
        Ok((reader, writer)) => Ok((reader, writer, watch, handle)),
        Err(_) if cancel.is_cancelled() => Err(TransferError::Cancelled),
        Err(e) => Err(e.into()),
    }
}

/// How long an abort may wait for a host that stopped reading before the connection is cut.
const ABORT_WRITE: std::time::Duration = std::time::Duration::from_secs(1);
/// How long, once aborted, the daemon's answer is waited for.
const ABORT_ANSWER: std::time::Duration = std::time::Duration::from_secs(5);

/// Aborts a stream (`Close(Aborted)`, which ends its batch on the daemon), without waiting on a
/// host that stopped reading: if the frame cannot go out soon, the connection (`handle`, the
/// stream's socket) is cut, which ends the batch all the same.
fn abort(writer: StreamWriter, handle: &UnixStream) {
    let _ = handle.set_write_timeout(Some(ABORT_WRITE));
    if writer.close(StreamClose::Aborted).is_err() {
        let _ = handle.shutdown(std::net::Shutdown::Both);
    }
}

/// Waits for the batch's outcome on its last stream, everything sent. A cancel meanwhile
/// aborts the stream, which ends the batch on the daemon unless it is typing the paths
/// already; the daemon's answer then says which.
fn wait_cancellable(
    reader: StreamReader,
    writer: StreamWriter,
    handle: UnixStream,
    cancel: &Cancel,
) -> Result<(), TransferError> {
    wait_cancellable_with(reader, writer, handle, cancel, |wait| {
        std::thread::Builder::new()
            .name("upload-wait".into())
            .spawn(wait)
    })
}

/// [`wait_cancellable`], its waiting thread started by `spawn` (tests).
fn wait_cancellable_with(
    reader: StreamReader,
    writer: StreamWriter,
    handle: UnixStream,
    cancel: &Cancel,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<std::thread::JoinHandle<()>>,
) -> Result<(), TransferError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let started = spawn(Box::new(move || {
        let _ = tx.send(reader.wait_closed());
    }));
    // Without a way to hear its outcome the upload is reported failed: it must be, on the host
    // too, or its paths would be typed all the same.
    if let Err(e) = started {
        abort(writer, &handle);
        return Err(TransferError::Local(format!(
            "cannot wait for the upload: {e}"
        )));
    }
    let mut writer = Some(writer);
    let mut aborted: Option<std::time::Instant> = None;
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => {
                return Err(if writer.is_none() || cancel.is_cancelled() {
                    TransferError::Cancelled
                } else {
                    remote_error(&e)
                });
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.is_cancelled()
                    && let Some(w) = writer.take()
                {
                    abort(w, &handle);
                    aborted = Some(std::time::Instant::now());
                }
                // A host that takes the abort but never answers is not waited on for ever.
                if aborted.is_some_and(|at| at.elapsed() > ABORT_ANSWER) {
                    let _ = handle.shutdown(std::net::Shutdown::Both);
                    return Err(TransferError::Cancelled);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(TransferError::Remote("the upload's stream ended".into()));
            }
        }
    }
}

/// Copies `source` to `writer`, checking `cancel` between frames; drops (aborts) nothing
/// itself: on an error the caller's writer goes unfinished.
fn send(
    source: &mut Source<'_>,
    writer: &mut StreamWriter,
    cancel: &Cancel,
    mut sent: impl FnMut(u64),
) -> Result<(), TransferError> {
    let mut buf = vec![0u8; MAX_DATA];
    let mut done = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Err(TransferError::Cancelled);
        }
        let n = match source.reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(TransferError::Local(format!("{}: {e}", source.name))),
        };
        if done + n as u64 > source.size {
            return Err(TransferError::Local(format!(
                "{} grew while it was sent",
                source.name
            )));
        }
        // A host that stopped taking bytes blocks this write; Cancel cuts the connection under
        // it (see `open_watched`).
        writer.write_all(&buf[..n]).map_err(|e| {
            if cancel.is_cancelled() {
                TransferError::Cancelled
            } else {
                TransferError::Remote(e.to_string())
            }
        })?;
        done += n as u64;
        sent(n as u64);
    }
    if done != source.size {
        return Err(TransferError::Local(format!(
            "{} got shorter while it was sent",
            source.name
        )));
    }
    Ok(())
}

/// Sends `source` on its stream ([`send_and_finish`]); if that fails, aborts the stream within
/// bounds ([`abort`]) rather than leaving it to a writer's drop, which would wait on a host
/// that stopped reading.
fn send_one(
    source: &mut Source<'_>,
    mut writer: StreamWriter,
    watch: CancelWatch,
    handle: &UnixStream,
    cancel: &Cancel,
    sent: impl FnMut(u64),
) -> Result<StreamWriter, TransferError> {
    match send_and_finish(source, &mut writer, watch, cancel, sent) {
        Ok(()) => Ok(writer),
        Err(e) => {
            abort(writer, handle);
            Err(e)
        }
    }
}

/// Sends `source` and its `End` with `watch` cutting the connection on Cancel, the `End` too
/// (a host that stopped reading blocks it as well); then lets the watch go, so a Cancel while
/// the batch waits aborts the stream rather than cutting it.
fn send_and_finish(
    source: &mut Source<'_>,
    writer: &mut StreamWriter,
    watch: CancelWatch,
    cancel: &Cancel,
    sent: impl FnMut(u64),
) -> Result<(), TransferError> {
    send(source, writer, cancel, sent)?;
    writer.finish().map_err(|e| {
        if cancel.is_cancelled() {
            TransferError::Cancelled
        } else {
            TransferError::Remote(e.to_string())
        }
    })?;
    drop(watch);
    Ok(())
}

/// Waits for the stream's close: `Ok` once the daemon has the batch and typed it.
fn wait(reader: StreamReader, cancel: &Cancel) -> Result<(), TransferError> {
    reader.wait_closed().map_err(|e| {
        if cancel.is_cancelled() {
            TransferError::Cancelled
        } else {
            remote_error(&e)
        }
    })
}

/// A stream's error as the daemon put it.
fn remote_error(e: &std::io::Error) -> TransferError {
    let e = e.to_string();
    let why = ["stream failed: ", "stream refused: "]
        .iter()
        .find_map(|p| e.strip_prefix(p))
        .unwrap_or(&e);
    TransferError::Remote(why.to_owned())
}

/// What [`download`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Downloaded {
    /// The file is still `if_version`: nothing was written.
    NotModified(DownloadInfo),
    /// The file is at `dest`.
    Saved(DownloadInfo),
}

/// Fetches the file at `path`, as `pane` sees it, into the folder `dir` (one others write to,
/// like Downloads) as `name`, or `name 2`, `name 3`… when that is taken. The file is written in
/// a new private folder of its own inside `dir`, then moved into `dir` by a rename that never
/// replaces anything: no file already in `dir` is written into, moved or deleted, whatever
/// else writes there meanwhile. Returns the file's details and where it was saved.
pub fn download_to_folder(
    host: Host,
    pane: PaneId,
    path: &str,
    dir: &Path,
    name: &str,
    cancel: &Cancel,
    mut progress: impl FnMut(Progress),
) -> Result<(DownloadInfo, PathBuf), TransferError> {
    if !host.client.has_capability(CAP_TRANSFER) {
        return Err(TransferError::Unsupported);
    }
    let name = if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        "file"
    } else {
        name
    };
    let shown = dir.join(name);
    let local = |e: std::io::Error| TransferError::Local(format!("{}: {e}", shown.display()));
    // `dir`, and the private folder made in it, held open: everything after is done through
    // them, not their names, which another writer could point elsewhere meanwhile.
    let folder =
        Dir::open(dir).map_err(|e| TransferError::Local(format!("{}: {e}", dir.display())))?;
    let (private_name, private) = loop {
        let private_name = format!(".thurm-{}", batch_id());
        match folder.make_private(&private_name) {
            Ok(private) => break (private_name, private),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(TransferError::Local(format!("{}: {e}", dir.display()))),
        }
    };
    let got = (|| {
        let file = private.create_new("part").map_err(local)?;
        let made = file_id(&file.metadata().map_err(local)?);
        let info = host.client.download_begin(pane, path, None)?;
        let ticket = info
            .ticket
            .as_deref()
            .ok_or_else(|| TransferError::Remote("the daemon sent no ticket".into()))?;
        fetch_into(host, ticket, file, &shown, info.size, cancel, &mut progress)?;
        if !private
            .stat("part")
            .is_ok_and(|(id, kind, len)| id == made && kind == libc::S_IFREG && len == info.size)
        {
            return Err(TransferError::Local(format!(
                "{} changed while it came; nothing was saved",
                shown.display()
            )));
        }
        // Into `dir` under the first of its names that is free, taken in the same step.
        for n in 1..=10_000 {
            let dest = numbered(name, n);
            match private.rename_noreplace("part", &folder, &dest) {
                Ok(()) => return Ok((info, dir.join(dest))),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(TransferError::Local(format!(
                        "{}: {e}",
                        dir.join(dest).display()
                    )));
                }
            }
        }
        Err(TransferError::Local(format!(
            "no free name for {name} in {}",
            dir.display()
        )))
    })();
    // The file, if it was not saved, and then the private folder, while its name is still the
    // folder made (empty, it is all that goes).
    private.unlink("part");
    if let Ok(ours) = private.id() {
        folder.remove_dir_if(&private_name, ours);
    }
    got
}

fn file_id(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}

/// A folder held open, so what is done in it is done there whatever its name comes to name.
struct Dir(std::os::fd::OwnedFd);

impl Dir {
    fn c(name: &str) -> std::io::Result<std::ffi::CString> {
        Ok(std::ffi::CString::new(name)?)
    }

    fn check(r: libc::c_int) -> std::io::Result<libc::c_int> {
        if r < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(r)
        }
    }

    fn open(path: &Path) -> std::io::Result<Dir> {
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        let p = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let fd = Self::check(unsafe {
            libc::open(
                p.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        })?;
        Ok(Dir(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }))
    }

    fn raw(&self) -> libc::c_int {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }

    /// Makes the folder `name` here, readable by this user only, and holds it open: the one
    /// made, as long as it is a folder of this user's, private, when opened.
    fn make_private(&self, name: &str) -> std::io::Result<Dir> {
        use std::os::fd::FromRawFd;
        let n = Self::c(name)?;
        Self::check(unsafe { libc::mkdirat(self.raw(), n.as_ptr(), 0o700) })?;
        let fd = Self::check(unsafe {
            libc::openat(
                self.raw(),
                n.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })?;
        let dir = Dir(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        Self::check(unsafe { libc::fstat(dir.raw(), &mut st) })?;
        if st.st_uid != unsafe { libc::getuid() } || st.st_mode & 0o777 != 0o700 {
            return Err(std::io::Error::other(
                "the folder made for the download was replaced",
            ));
        }
        Ok(dir)
    }

    fn id(&self) -> std::io::Result<(u64, u64)> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        Self::check(unsafe { libc::fstat(self.raw(), &mut st) })?;
        #[allow(clippy::unnecessary_cast)]
        Ok((st.st_dev as u64, st.st_ino as u64))
    }

    /// A new file `name` here for this user only, never through a link.
    fn create_new(&self, name: &str) -> std::io::Result<File> {
        use std::os::fd::FromRawFd;
        let n = Self::c(name)?;
        let fd = Self::check(unsafe {
            libc::openat(
                self.raw(),
                n.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        })?;
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// What `name` here is, not following a link: its device and inode, kind, and length.
    fn stat(&self, name: &str) -> std::io::Result<((u64, u64), libc::mode_t, u64)> {
        let n = Self::c(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        Self::check(unsafe {
            libc::fstatat(self.raw(), n.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW)
        })?;
        #[allow(clippy::unnecessary_cast)]
        Ok((
            (st.st_dev as u64, st.st_ino as u64),
            st.st_mode & libc::S_IFMT,
            st.st_size as u64,
        ))
    }

    fn unlink(&self, name: &str) {
        if let Ok(n) = Self::c(name) {
            unsafe { libc::unlinkat(self.raw(), n.as_ptr(), 0) };
        }
    }

    /// Removes the empty folder `name` here only while the name is the folder `ours`.
    fn remove_dir_if(&self, name: &str, ours: (u64, u64)) {
        if self
            .stat(name)
            .is_ok_and(|(id, kind, _)| id == ours && kind == libc::S_IFDIR)
            && let Ok(n) = Self::c(name)
        {
            unsafe { libc::unlinkat(self.raw(), n.as_ptr(), libc::AT_REMOVEDIR) };
        }
    }

    /// Gives `name` here the name `to` in `dest` only if nothing has that name, in one step.
    fn rename_noreplace(&self, name: &str, dest: &Dir, to: &str) -> std::io::Result<()> {
        let (a, b) = (Self::c(name)?, Self::c(to)?);
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let r = unsafe {
            libc::renameatx_np(
                self.raw(),
                a.as_ptr(),
                dest.raw(),
                b.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        // The system call itself: not every C library has a wrapper for it (musl's older ones).
        #[cfg(target_os = "linux")]
        let r = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                self.raw(),
                a.as_ptr(),
                dest.raw(),
                b.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "linux")))]
        let r = {
            let _ = (&a, &b, dest);
            return Err(std::io::ErrorKind::Unsupported.into());
        };
        if r == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

/// `name`, numbered `n` past the first (`a 2.txt`, as Finder names copies), within the 255
/// bytes a name may take: the part before the extension is cut, by whole characters, to make
/// room, and an extension too long by itself is cut too, keeping a character before it.
fn numbered(name: &str, n: u32) -> String {
    let (base, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    };
    let number = if n == 1 {
        String::new()
    } else {
        format!(" {n}")
    };
    let mut ext = ext.to_owned();
    while !ext.is_empty() && 1 + number.len() + 1 + ext.len() > 255 {
        ext.pop();
    }
    let tail = if ext.is_empty() {
        number
    } else {
        format!("{number}.{ext}")
    };
    let mut stem = if base.is_empty() { "file" } else { base }.to_owned();
    while stem.len() + tail.len() > 255 && !stem.is_empty() {
        stem.pop();
    }
    stem + &tail
}

/// Fetches the file at `path`, as `pane` sees it, into `dest` (written beside it first, then
/// moved into place, replacing whatever `dest` is: for a destination this caller owns, like a
/// cache's; [`download_to_folder`] saves into a folder others write to). With `if_version` still the file's version, writes nothing. A file over
/// `max_bytes` is refused before anything is sent.
#[allow(clippy::too_many_arguments)]
pub fn download(
    host: Host,
    pane: PaneId,
    path: &str,
    dest: &Path,
    if_version: Option<&str>,
    max_bytes: Option<u64>,
    cancel: &Cancel,
    mut progress: impl FnMut(Progress),
) -> Result<Downloaded, TransferError> {
    if !host.client.has_capability(CAP_TRANSFER) {
        return Err(TransferError::Unsupported);
    }
    let info = host.client.download_begin(pane, path, if_version)?;
    if info.not_modified {
        return Ok(Downloaded::NotModified(info));
    }
    if let Some(max) = max_bytes.filter(|max| info.size > *max) {
        // Its ticket is used up (opened and dropped at once) rather than left to lapse, where
        // it would count against this client's tickets for a minute.
        if let Some(ticket) = info.ticket.as_deref()
            && let Ok(opened) = stream::open(host.socket, ticket)
        {
            drop(opened);
        }
        return Err(TransferError::TooLarge {
            size: info.size,
            max,
        });
    }
    let ticket = info
        .ticket
        .as_deref()
        .ok_or_else(|| TransferError::Remote("the daemon sent no ticket".into()))?;
    // A read blocks until the host sends; Cancel ends the connection under it.
    // Where it goes first: a destination that cannot be written fails before any stream.
    let (part, file) = create_part(dest)?;
    if let Err(e) = fetch_into(host, ticket, file, &part, info.size, cancel, &mut progress) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dest).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        TransferError::Local(format!("{}: {e}", dest.display()))
    })?;
    Ok(Downloaded::Saved(info))
}

/// What a copy says of a file it would have to replace, and was not told to.
pub const COPY_EXISTS: &str = "is there already (--force replaces it)";

/// Copies the file `src` to `dest` on the host ([`thurm_proto::CAP_COPY`]): a path of the
/// host's user, `~` and relative ones under its home, the file's own name inside it when it is
/// a folder. A file already there is replaced only with `replace`. The file is whole there or
/// not there at all. Returns where it is on the host, and its size.
pub fn copy_to(
    host: Host,
    src: &Path,
    dest: &str,
    replace: bool,
    cancel: &Cancel,
    mut progress: impl FnMut(Progress),
) -> Result<(String, u64), TransferError> {
    if !host.client.has_capability(CAP_COPY) {
        return Err(TransferError::Unsupported);
    }
    let upload = Upload::Path(src.to_owned());
    let planned = plan(&upload)?;
    let (mut source, mode) = open_planned_with_mode(&planned)?;
    let (ticket, landed) = host.client.copy_to_begin(
        dest,
        &source.name,
        source.size,
        mode.unwrap_or(0o644),
        replace,
    )?;
    let (reader, writer, watch, handle) = open_watched(host.socket, &ticket.ticket, cancel)?;
    let mut p = Progress {
        done: 0,
        total: source.size,
    };
    progress(p);
    let writer = send_one(&mut source, writer, watch, &handle, cancel, |n| {
        p.done += n;
        progress(p);
    })?;
    wait_cancellable(reader, writer, handle, cancel)?;
    Ok((landed, p.total))
}

/// Copies the file at `path` on the host ([`thurm_proto::CAP_COPY`], read as [`copy_to`] reads
/// its destination) to `dest` here, under its own name inside `dest` when that is a folder. A
/// file already there is replaced only with `replace`, and only a regular file is. Written
/// beside its place first, then moved there. Returns where it is and its size.
pub fn copy_from(
    host: Host,
    path: &str,
    dest: &Path,
    replace: bool,
    cancel: &Cancel,
    mut progress: impl FnMut(Progress),
) -> Result<(PathBuf, u64), TransferError> {
    if !host.client.has_capability(CAP_COPY) {
        return Err(TransferError::Unsupported);
    }
    let info = host.client.copy_from_begin(path)?;
    let ticket = info
        .ticket
        .as_deref()
        .ok_or_else(|| TransferError::Remote("the daemon sent no ticket".into()))?;
    // A copy refused here uses its ticket up (opened and dropped at once) rather than leave it
    // to lapse, counted against this client's tickets for a minute.
    let refuse = |e: TransferError| {
        if let Ok(opened) = stream::open(host.socket, ticket) {
            drop(opened);
        }
        e
    };
    let dest = if dest.is_dir() {
        let name = Path::new(&info.name)
            .file_name()
            .ok_or_else(|| refuse(TransferError::Remote("the host's file has no name".into())))?;
        dest.join(name)
    } else {
        dest.to_owned()
    };
    // Its mode once in place: the one of the file it replaces, else what a new file gets.
    let mode = match std::fs::symlink_metadata(&dest) {
        Ok(m) if !m.is_file() => {
            return Err(refuse(TransferError::Local(format!(
                "{} is not a regular file; it is not replaced",
                dest.display()
            ))));
        }
        Ok(_) if !replace => {
            return Err(refuse(TransferError::Local(format!(
                "{} {COPY_EXISTS}",
                dest.display()
            ))));
        }
        Ok(m) => std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0o644,
        Err(e) => {
            return Err(refuse(TransferError::Local(format!(
                "{}: {e}",
                dest.display()
            ))));
        }
    };
    let (part, file) = create_part(&dest).map_err(refuse)?;
    // The part itself, kept beside the handle that writes it: its bits are set through it, and
    // it tells whether the part's name is still this file.
    let ours = match file.try_clone() {
        Ok(ours) => ours,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(refuse(TransferError::Local(format!(
                "{}: {e}",
                part.display()
            ))));
        }
    };
    let placed = fetch_into(host, ticket, file, &part, info.size, cancel, &mut progress)
        .and_then(|()| land_copy(&part, &ours, &dest, mode, replace));
    // Moved into place, the part's name is gone; linked there, or failed, it is not. Only
    // while the name is still this copy's file: what someone put in its place is theirs.
    if names(&part, &ours) {
        let _ = std::fs::remove_file(&part);
    }
    placed.map(|()| (dest, info.size))
}

/// Whether `path` is still the name of `file` (not a link, nor another file, put in its place).
fn names(path: &Path, file: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(path), file.metadata()) {
        (Ok(at), Ok(ours)) => at.is_file() && (at.dev(), at.ino()) == (ours.dev(), ours.ino()),
        _ => false,
    }
}

/// Gives the whole file `ours`, written as `part`, the permission bits `mode` and puts it at
/// `dest` ([`place_copy`]). The bits are set through the open file, not its name: in a folder
/// others can change, the name may lead elsewhere by now, and a file of the user's must not get
/// these bits in its place. A part whose name is no longer this file is not placed.
fn land_copy(
    part: &Path,
    ours: &File,
    dest: &Path,
    mode: u32,
    replace: bool,
) -> Result<(), TransferError> {
    ours.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))
        .map_err(|e| TransferError::Local(format!("{}: {e}", dest.display())))?;
    if !names(part, ours) {
        return Err(TransferError::Local(format!(
            "{} was changed while the file was written; nothing was copied",
            part.display()
        )));
    }
    place_copy(part, dest, replace)
}

/// Puts the whole file `part` at `dest`. With `replace`, over a regular file there, and only
/// one: what is there is looked at again now, as a link or a folder may have come since the
/// copy was asked for. Without, only where nothing is.
fn place_copy(part: &Path, dest: &Path, replace: bool) -> Result<(), TransferError> {
    let local = |e: std::io::Error| TransferError::Local(format!("{}: {e}", dest.display()));
    if !replace {
        // Into its place without replacing a file that came there meanwhile.
        return std::fs::hard_link(part, dest).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                TransferError::Local(format!("{} {COPY_EXISTS}", dest.display()))
            }
            _ => local(e),
        });
    }
    match std::fs::symlink_metadata(dest) {
        Ok(m) if !m.is_file() => {
            return Err(TransferError::Local(format!(
                "{} is not a regular file; it is not replaced",
                dest.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(local(e)),
    }
    std::fs::rename(part, dest).map_err(local)
}

/// Opens the stream `ticket` reserved and writes the `size` bytes it sends into `file` (`shown`
/// in errors), Cancel cutting it short; a failed receive aborts the stream within bounds.
/// Leaves cleaning up `file` to the caller, who alone knows what it is.
fn fetch_into(
    host: Host,
    ticket: &str,
    file: File,
    shown: &Path,
    size: u64,
    cancel: &Cancel,
    progress: &mut impl FnMut(Progress),
) -> Result<(), TransferError> {
    let (mut reader, mut writer, _watch, handle) = open_watched(host.socket, ticket, cancel)?;
    if let Err(e) = receive(&mut reader, file, shown, size, cancel, progress) {
        abort(writer, &handle);
        return Err(e);
    }
    writer
        .finish()
        .map_err(|e| TransferError::Remote(e.to_string()))
        .and_then(|()| wait(reader, cancel))
}

/// Shuts a stream's connection down once `cancel` is set, until dropped: a read blocked on a
/// host that stopped sending then returns.
struct CancelWatch {
    /// Dropped to stop the watch at once (its thread waits on the other end).
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CancelWatch {
    /// Starts watching; an error when no thread can watch, as Cancel could not stop a read.
    fn start(
        cancel: Cancel,
        socket: std::os::unix::net::UnixStream,
    ) -> std::io::Result<CancelWatch> {
        Self::start_with(cancel, socket, |watch| {
            std::thread::Builder::new()
                .name("transfer-cancel".into())
                .spawn(watch)
        })
    }

    fn start_with(
        cancel: Cancel,
        socket: std::os::unix::net::UnixStream,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<std::thread::JoinHandle<()>>,
    ) -> std::io::Result<CancelWatch> {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let thread = spawn(Box::new(move || {
            loop {
                if cancel.is_cancelled() {
                    let _ = socket.shutdown(std::net::Shutdown::Both);
                    return;
                }
                // Woken at once when the watch is dropped, not at its next look at `cancel`.
                match stopped.recv_timeout(std::time::Duration::from_millis(50)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
            }
        }))?;
        Ok(CancelWatch {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A new file beside `dest` that this download alone writes until whole: two downloads to
/// the same place never share one.
fn create_part(dest: &Path) -> Result<(PathBuf, File), TransferError> {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // A short name of its own: `dest`'s may already be as long as names go.
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let part = dest.with_file_name(format!(".thurm-{}-{n}.part", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&part)
        {
            Ok(file) => return Ok((part, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(TransferError::Local(format!("{}: {e}", part.display())));
            }
        }
    }
}

fn receive(
    reader: &mut StreamReader,
    mut file: File,
    part: &Path,
    size: u64,
    cancel: &Cancel,
    progress: &mut impl FnMut(Progress),
) -> Result<(), TransferError> {
    let local = |e: std::io::Error| TransferError::Local(format!("{}: {e}", part.display()));
    let mut buf = vec![0u8; MAX_DATA];
    let mut p = Progress {
        done: 0,
        total: size,
    };
    progress(p);
    loop {
        if cancel.is_cancelled() {
            return Err(TransferError::Cancelled);
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) if cancel.is_cancelled() => return Err(TransferError::Cancelled),
            Err(e) => return Err(remote_error(&e)),
        };
        p.done += n as u64;
        if p.done > size {
            return Err(TransferError::Remote(
                "the host sent more than the file's size".into(),
            ));
        }
        file.write_all(&buf[..n]).map_err(local)?;
        progress(p);
    }
    if p.done != size {
        if cancel.is_cancelled() {
            return Err(TransferError::Cancelled);
        }
        return Err(TransferError::Remote(format!(
            "{} of {size} bytes arrived",
            p.done
        )));
    }
    file.sync_all().map_err(local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    use thurm_proto::stream::StreamFrame;
    use thurm_proto::{
        CAP_ACTIVE_SIZE, CAP_STREAMS, Envelope, PROTOCOL_VERSION, Request, Response, ServerMessage,
        codec,
    };

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("thurm-transfer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A daemon's end of a stream: answers the Hello and the StreamOpen, then the frames it
    /// gets until a `Close`, which it answers with `answer`.
    fn fake_daemon(answer: StreamClose) -> (UnixStream, std::thread::JoinHandle<Vec<StreamFrame>>) {
        fake_daemon_then(move |r, w| {
            let mut seen = Vec::new();
            while let Ok(Some(frame)) = thurm_proto::stream::read_frame(r) {
                let done = matches!(frame, StreamFrame::Close(_));
                seen.push(frame);
                if done {
                    thurm_proto::stream::write_frame(w, &StreamFrame::Close(answer)).unwrap();
                    break;
                }
            }
            seen
        })
    }

    /// A daemon's end that answers the Hello and the StreamOpen, then runs `then`.
    fn fake_daemon_then(
        then: impl FnOnce(&mut std::io::BufReader<UnixStream>, &mut UnixStream) -> Vec<StreamFrame>
        + Send
        + 'static,
    ) -> (UnixStream, std::thread::JoinHandle<Vec<StreamFrame>>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let daemon = std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(theirs.try_clone().unwrap());
            let mut w = theirs;
            for _ in 0..2 {
                let frame = codec::read_frame(&mut r).unwrap().unwrap();
                let env: Envelope = codec::decode(&frame).unwrap();
                let result = match env.request {
                    Request::Hello { .. } => Response::Hello {
                        version: PROTOCOL_VERSION,
                        daemon_pid: 1,
                        restored: false,
                        build: "fixture".into(),
                        capabilities: vec![
                            CAP_ACTIVE_SIZE.into(),
                            CAP_STREAMS.into(),
                            CAP_TRANSFER.into(),
                        ],
                    },
                    _ => Response::Ok,
                };
                let msg = ServerMessage::Response {
                    id: env.id,
                    result: Ok(result),
                };
                codec::write_message(&mut w, &msg).unwrap();
            }
            then(&mut r, &mut w)
        });
        (ours, daemon)
    }

    #[test]
    fn cancel_ends_a_download_the_host_stopped_sending() {
        // Some bytes, then nothing, the connection held open.
        let (socket, daemon) = fake_daemon_then(|r, w| {
            thurm_proto::stream::write_frame(w, &StreamFrame::Data(b"part".to_vec())).unwrap();
            let mut seen = Vec::new();
            while let Ok(Some(frame)) = thurm_proto::stream::read_frame(r) {
                seen.push(frame);
            }
            seen
        });
        let shutdown = socket.try_clone().unwrap();
        let (mut reader, _writer) = stream::open_on(socket, "ticket").unwrap();
        let dir = temp_dir("stalled");
        let (part, file) = create_part(&dir.join("big.bin")).unwrap();
        let cancel = Cancel::default();
        let watch = CancelWatch::start(cancel.clone(), shutdown).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let theirs = cancel.clone();
        let reading = std::thread::spawn(move || {
            let mut seen = Progress::default();
            let got = receive(&mut reader, file, &part, 1000, &theirs, &mut |p| seen = p);
            let _ = tx.send((got, seen));
        });
        std::thread::sleep(Duration::from_millis(150));
        cancel.cancel();
        let (got, seen) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the read stayed blocked after Cancel");
        assert!(matches!(got, Err(TransferError::Cancelled)), "{got:?}");
        assert_eq!(seen.done, 4);
        reading.join().unwrap();
        drop(watch);
        daemon.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nothing_at_the_path_is_told_from_other_failures() {
        let missing = ClientError::Daemon(format!("notes.md: {}", thurm_proto::DOWNLOAD_NOT_FOUND));
        assert!(matches!(
            TransferError::from(missing),
            TransferError::NotFound(_)
        ));
        let denied = ClientError::Daemon("cannot open notes.md: Permission denied".into());
        assert!(matches!(
            TransferError::from(denied),
            TransferError::Remote(_)
        ));
        assert!(matches!(
            TransferError::from(ClientError::Disconnected),
            TransferError::Remote(_)
        ));
    }

    #[test]
    fn cancelling_while_the_batch_waits_aborts_its_stream() {
        let (socket, daemon) =
            fake_daemon(StreamClose::Internal("the upload was cancelled".into()));
        let handle = socket.try_clone().unwrap();
        let (reader, mut writer) = stream::open_on(socket, "ticket").unwrap();
        writer.write_all(b"all of it").unwrap();
        writer.finish().unwrap();
        let cancel = Cancel::default();
        let later = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            later.cancel();
        });
        let got = wait_cancellable(reader, writer, handle, &cancel);
        assert!(matches!(got, Err(TransferError::Cancelled)), "{got:?}");
        assert_eq!(
            daemon.join().unwrap(),
            [
                StreamFrame::Data(b"all of it".to_vec()),
                StreamFrame::End,
                StreamFrame::Close(StreamClose::Aborted)
            ]
        );
    }

    #[test]
    fn a_copy_sends_the_bits_of_the_file_it_opened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("copy-mode");
        let path = dir.join("secret.txt");
        std::fs::write(&path, b"private").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let upload = Upload::Path(path.clone());
        let planned = plan(&upload).unwrap();
        let (mut source, mode) = open_planned_with_mode(&planned).unwrap();
        // Another file takes the name once this one is open: the bytes and the bits sent are
        // still the first one's.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"public!").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(mode, Some(0o600));
        let mut sent = Vec::new();
        source.reader.read_to_end(&mut sent).unwrap();
        assert_eq!(sent, b"private");
        // Bytes have no bits of their own.
        let data = Upload::Data {
            name: "a".into(),
            data: vec![1],
        };
        let (_, mode) = open_planned_with_mode(&plan(&data).unwrap()).unwrap();
        assert_eq!(mode, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_in_a_parts_place_gets_neither_the_copys_bits_nor_removed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("copy-swap");
        let dest = dir.join("dest.txt");
        let private = dir.join("private.key");
        std::fs::write(&private, b"secret").unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // The part's name leads to a private file of the user's by the time the copy is whole.
        let (part, mut file) = create_part(&dest).unwrap();
        file.write_all(b"new").unwrap();
        std::fs::remove_file(&part).unwrap();
        std::os::unix::fs::symlink(&private, &part).unwrap();
        let e = land_copy(&part, &file, &dest, 0o644, true).unwrap_err();
        assert!(e.to_string().contains("was changed while"), "{e}");
        assert_eq!(
            mode(&private),
            0o600,
            "the link's target got the copy's bits"
        );
        assert_eq!(std::fs::read(&private).unwrap(), b"secret");
        assert!(!dest.exists());
        // Not this copy's to remove.
        assert!(!names(&part, &file));
        std::fs::remove_file(&part).unwrap();

        // Another file under the part's name is no more this copy's than a link.
        let (part, file) = create_part(&dest).unwrap();
        std::fs::remove_file(&part).unwrap();
        std::fs::write(&part, b"theirs").unwrap();
        assert!(land_copy(&part, &file, &dest, 0o644, false).is_err());
        assert!(!dest.exists() && !names(&part, &file));
        std::fs::remove_file(&part).unwrap();

        // Left alone, the part lands with the bits asked for.
        let (part, mut file) = create_part(&dest).unwrap();
        file.write_all(b"new").unwrap();
        assert!(names(&part, &file));
        land_copy(&part, &file, &dest, 0o640, false).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert_eq!(mode(&dest), 0o640);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_copy_replaces_only_a_regular_file_that_is_there_when_it_lands() {
        let dir = temp_dir("copy-place");
        let part = dir.join(".part");
        let dest = dir.join("dest.txt");
        let target = dir.join("target.txt");
        std::fs::write(&target, b"kept").unwrap();
        let fresh = || std::fs::write(&part, b"new").unwrap();

        // A link that took the destination's place while the file came: kept, with its target.
        fresh();
        std::os::unix::fs::symlink(&target, &dest).unwrap();
        let e = place_copy(&part, &dest, true).unwrap_err();
        assert!(e.to_string().contains("not a regular file"), "{e}");
        assert!(std::fs::symlink_metadata(&dest).unwrap().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"kept");
        // A dangling link is a link all the same.
        std::fs::remove_file(&dest).unwrap();
        std::os::unix::fs::symlink(dir.join("nothing"), &dest).unwrap();
        assert!(place_copy(&part, &dest, true).is_err());
        assert!(!dir.join("nothing").exists());
        // So is a folder.
        std::fs::remove_file(&dest).unwrap();
        std::fs::create_dir(&dest).unwrap();
        assert!(place_copy(&part, &dest, true).is_err());
        std::fs::remove_dir(&dest).unwrap();

        // A regular file gives way, and nothing there is fine too.
        std::fs::write(&dest, b"old").unwrap();
        place_copy(&part, &dest, true).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        std::fs::remove_file(&dest).unwrap();
        fresh();
        place_copy(&part, &dest, true).unwrap();
        // Not to be replaced: a file that came meanwhile is kept.
        fresh();
        std::fs::write(&dest, b"theirs").unwrap();
        let e = place_copy(&part, &dest, false).unwrap_err();
        assert!(e.to_string().contains(COPY_EXISTS), "{e}");
        assert_eq!(std::fs::read(&dest).unwrap(), b"theirs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_taken_name_is_numbered_within_what_names_take() {
        assert_eq!(numbered("a.txt", 1), "a.txt");
        assert_eq!(numbered("a.txt", 2), "a 2.txt");
        assert_eq!(numbered("Makefile", 2), "Makefile 2");
        assert_eq!(numbered(".bashrc", 3), ".bashrc 3");
        assert_eq!(numbered("a.tar.gz", 2), "a.tar 2.gz");
        // A name as long as names go is cut to make room for its number.
        let long = "l".repeat(251) + ".txt";
        assert_eq!(numbered(&long, 1), long);
        let second = numbered(&long, 2);
        assert_eq!(second.len(), 255);
        assert!(second.ends_with(" 2.txt"), "{second}");
        // By whole characters.
        let wide = "é".repeat(125) + ".txt";
        let second = numbered(&wide, 2);
        assert!(
            second.len() <= 255 && second.ends_with(" 2.txt"),
            "{second}"
        );
        // So is an extension as long as names go, a character kept before it.
        let long_ext = "a.".to_owned() + &"e".repeat(253);
        assert_eq!(numbered(&long_ext, 1), long_ext);
        let second = numbered(&long_ext, 2);
        assert!(
            second.len() <= 255 && second.starts_with("a 2."),
            "{second}"
        );
    }

    #[test]
    fn no_thread_to_wait_for_the_outcome_aborts_the_batch() {
        // What reaches the daemon; the app does not stay for an answer.
        let (socket, daemon) = fake_daemon_then(|r, _| {
            let mut seen = Vec::new();
            while let Ok(Some(frame)) = thurm_proto::stream::read_frame(r) {
                seen.push(frame);
            }
            seen
        });
        let handle = socket.try_clone().unwrap();
        let (reader, mut writer) = stream::open_on(socket, "ticket").unwrap();
        writer.write_all(b"all of it").unwrap();
        writer.finish().unwrap();
        let got = wait_cancellable_with(reader, writer, handle, &Cancel::default(), |_| {
            Err(std::io::Error::other("no threads left"))
        });
        assert!(
            matches!(&got, Err(TransferError::Local(why)) if why.contains("no threads left")),
            "{got:?}"
        );
        assert_eq!(
            daemon.join().unwrap(),
            [
                StreamFrame::Data(b"all of it".to_vec()),
                StreamFrame::End,
                StreamFrame::Close(StreamClose::Aborted)
            ]
        );
    }

    #[test]
    fn a_cancel_too_late_reports_what_the_daemon_did() {
        // The daemon was typing the paths already: it closes Ok, and the upload went.
        let (socket, daemon) = fake_daemon(StreamClose::Ok);
        let handle = socket.try_clone().unwrap();
        let (reader, mut writer) = stream::open_on(socket, "ticket").unwrap();
        writer.finish().unwrap();
        let cancel = Cancel::default();
        cancel.cancel();
        assert!(wait_cancellable(reader, writer, handle, &cancel).is_ok());
        daemon.join().unwrap();
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let dir = temp_dir("fifo");
        let fifo = dir.join("pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let link = dir.join("link");
        std::os::unix::fs::symlink(&fifo, &link).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let results = [fifo, link].map(|p| plan(&Upload::Path(p)).err().map(|e| e.to_string()));
            let _ = tx.send(results);
        });
        let results = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("opening a FIFO blocked");
        for e in results {
            assert!(
                e.as_deref()
                    .is_some_and(|e| e.contains("not a regular file")),
                "{e:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn each_download_writes_a_part_of_its_own() {
        let dir = temp_dir("parts");
        let dest = dir.join("same.txt");
        let (a, mut fa) = create_part(&dest).unwrap();
        let (b, mut fb) = create_part(&dest).unwrap();
        assert_ne!(a, b);
        fa.write_all(b"first").unwrap();
        fb.write_all(b"second").unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"first");
        assert_eq!(std::fs::read(&b).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Descriptors this process has open.
    fn open_fds() -> usize {
        std::fs::read_dir("/dev/fd").map_or(0, |d| d.count())
    }

    #[test]
    fn a_large_batch_is_checked_without_holding_its_files_open() {
        let dir = temp_dir("many");
        let uploads: Vec<Upload> = (0..300)
            .map(|i| {
                let path = dir.join(format!("f{i}"));
                std::fs::write(&path, b"x").unwrap();
                Upload::Path(path)
            })
            .collect();
        let before = open_fds();
        let planned = uploads
            .iter()
            .map(plan)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        // Other tests open some meanwhile; a file each would be 300.
        assert!(open_fds() < before + 50, "{before} -> {}", open_fds());
        assert_eq!(planned.len(), 300);
        assert_eq!(planned.iter().map(|p| p.size).sum::<u64>(), 300);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_changed_after_it_was_checked_is_not_sent() {
        let dir = temp_dir("changed");
        let path = dir.join("a.txt");
        std::fs::write(&path, b"short").unwrap();
        let upload = Upload::Path(path.clone());
        let planned = plan(&upload).unwrap();
        std::fs::write(&path, b"longer now").unwrap();
        let e = open_planned(&planned).err().unwrap();
        assert!(e.to_string().contains("changed since it was chosen"), "{e}");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(open_planned(&planned).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_thread_to_watch_for_cancel_is_an_error() {
        let (a, _b) = UnixStream::pair().unwrap();
        let started = CancelWatch::start_with(Cancel::default(), a, |_| {
            Err(std::io::Error::other("no threads left"))
        });
        assert!(started.is_err());
    }

    #[test]
    fn cancel_ends_an_upload_the_host_stopped_taking() {
        // The host takes nothing after the handshake, the connection held open.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (socket, daemon) = fake_daemon_then(move |_, _| {
            let _ = held.recv();
            Vec::new()
        });
        let shutdown = socket.try_clone().unwrap();
        let (_reader, mut writer) = stream::open_on(socket, "ticket").unwrap();
        let cancel = Cancel::default();
        let watch = CancelWatch::start(cancel.clone(), shutdown).unwrap();
        let theirs = cancel.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let data = vec![1u8; 16 << 20];
            let mut source = Source {
                name: "big".into(),
                size: data.len() as u64,
                reader: Box::new(std::io::Cursor::new(data)),
            };
            let _ = tx.send(send(&mut source, &mut writer, &theirs, |_| {}));
        });
        std::thread::sleep(Duration::from_millis(150));
        cancel.cancel();
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the write stayed blocked after Cancel");
        assert!(matches!(got, Err(TransferError::Cancelled)), "{got:?}");
        drop(watch);
        let _ = release.send(());
        daemon.join().unwrap();
    }

    #[test]
    fn a_name_as_long_as_names_go_downloads() {
        let dir = temp_dir("long-name");
        let dest = dir.join(format!("{}.txt", "n".repeat(251)));
        std::fs::write(&dest, b"").unwrap();
        let (part, mut file) = create_part(&dest).unwrap();
        file.write_all(b"data").unwrap();
        std::fs::rename(&part, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"data");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancel_ends_an_end_the_host_stopped_taking() {
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (socket, daemon) = fake_daemon_then(move |_, _| {
            let _ = held.recv();
            Vec::new()
        });
        let shutdown = socket.try_clone().unwrap();
        let (_reader, mut writer) = stream::open_on(socket, "ticket").unwrap();
        // The host reads nothing: fill the connection until it takes no more.
        shutdown.set_nonblocking(true).unwrap();
        let junk = vec![0u8; 64 * 1024];
        while (&shutdown).write(&junk).is_ok() {}
        shutdown.set_nonblocking(false).unwrap();
        let cancel = Cancel::default();
        let watch = CancelWatch::start(cancel.clone(), shutdown).unwrap();
        let theirs = cancel.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut empty = Source {
                name: "empty".into(),
                size: 0,
                reader: Box::new(std::io::empty()),
            };
            let _ = tx.send(send_and_finish(
                &mut empty,
                &mut writer,
                watch,
                &theirs,
                |_| {},
            ));
        });
        std::thread::sleep(Duration::from_millis(150));
        cancel.cancel();
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writing the End stayed blocked after Cancel");
        assert!(matches!(got, Err(TransferError::Cancelled)), "{got:?}");
        let _ = release.send(());
        daemon.join().unwrap();
    }

    #[test]
    fn cancel_while_waiting_ends_even_if_the_host_stopped_reading() {
        // The host took everything up to the End, then reads and answers nothing.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (socket, daemon) = fake_daemon_then(move |r, _| {
            while let Ok(Some(frame)) = thurm_proto::stream::read_frame(r) {
                if frame == StreamFrame::End {
                    break;
                }
            }
            let _ = held.recv();
            Vec::new()
        });
        let handle = socket.try_clone().unwrap();
        let fill = socket.try_clone().unwrap();
        let (reader, mut writer) = stream::open_on(socket, "ticket").unwrap();
        writer.write_all(b"all of it").unwrap();
        writer.finish().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // Its side of the connection full: no abort frame can go out.
        fill.set_nonblocking(true).unwrap();
        let junk = vec![0u8; 64 * 1024];
        while (&fill).write(&junk).is_ok() {}
        fill.set_nonblocking(false).unwrap();
        let cancel = Cancel::default();
        let theirs = cancel.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(wait_cancellable(reader, writer, handle, &theirs));
        });
        std::thread::sleep(Duration::from_millis(100));
        cancel.cancel();
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("Cancel stayed stuck on the abort");
        assert!(matches!(got, Err(TransferError::Cancelled)), "{got:?}");
        let _ = release.send(());
        daemon.join().unwrap();
    }

    #[test]
    fn a_file_failing_to_read_ends_its_stream_even_if_the_host_stopped_reading() {
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (socket, daemon) = fake_daemon_then(move |_, _| {
            let _ = held.recv();
            Vec::new()
        });
        let shutdown = socket.try_clone().unwrap();
        let handle = socket.try_clone().unwrap();
        let fill = socket.try_clone().unwrap();
        let (_reader, writer) = stream::open_on(socket, "ticket").unwrap();
        fill.set_nonblocking(true).unwrap();
        let junk = vec![0u8; 64 * 1024];
        while (&fill).write(&junk).is_ok() {}
        fill.set_nonblocking(false).unwrap();
        let cancel = Cancel::default();
        let watch = CancelWatch::start(cancel.clone(), shutdown).unwrap();
        struct Unreadable;
        impl Read for Unreadable {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("the disk went away"))
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut source = Source {
                name: "gone.txt".into(),
                size: 10,
                reader: Box::new(Unreadable),
            };
            let got = send_one(&mut source, writer, watch, &handle, &cancel, |_| {});
            let _ = tx.send(got.err().map(|e| e.to_string()));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("aborting the stream stayed blocked");
        assert!(got.is_some_and(|e| e.contains("the disk went away")));
        let _ = release.send(());
        daemon.join().unwrap();
    }

    #[test]
    fn a_watch_stops_at_once() {
        let (a, _b) = UnixStream::pair().unwrap();
        let watch = CancelWatch::start(Cancel::default(), a).unwrap();
        // Let its thread settle into its wait.
        std::thread::sleep(Duration::from_millis(20));
        let started = std::time::Instant::now();
        drop(watch);
        assert!(
            started.elapsed() < Duration::from_millis(25),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn cancel_stops_a_stream_the_daemon_is_slow_to_open() {
        // The daemon end reads the handshake and answers nothing.
        let (ours, theirs) = UnixStream::pair().unwrap();
        let shutdown = ours.try_clone().unwrap();
        let handle = ours.try_clone().unwrap();
        let cancel = Cancel::default();
        let theirs_cancel = cancel.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let got = open_watched_on(ours, shutdown, handle, "ticket", &theirs_cancel);
            let _ = tx.send(got.err().map(|e| matches!(e, TransferError::Cancelled)));
        });
        std::thread::sleep(Duration::from_millis(150));
        cancel.cancel();
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Cancel did not stop the handshake");
        assert_eq!(got, Some(true));
        drop(theirs);
    }
}
