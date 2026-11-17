//! Files to and from a host's daemon ([`thurm_proto::CAP_TRANSFER`]), each on a byte stream of
//! its own: dropped and pasted files go up as one batch the daemon types the paths of once all
//! have landed; a file in a remote pane comes back to read here.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use thurm_client::stream::{self, StreamReader, StreamWriter};
use thurm_client::{Client, ClientError};
use thurm_proto::stream::{MAX_DATA, StreamClose};
use thurm_proto::{CAP_TRANSFER, DownloadInfo, PaneId, UploadBatch};

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
            TransferError::Local(e) | TransferError::Remote(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for TransferError {}

impl From<ClientError> for TransferError {
    fn from(e: ClientError) -> Self {
        TransferError::Remote(e.to_string())
    }
}

/// An [`Upload`] opened to read.
struct Source {
    name: String,
    size: u64,
    reader: Box<dyn Read + Send>,
}

fn open_source(upload: &Upload) -> Result<Source, TransferError> {
    match upload {
        Upload::Data { name, data } => Ok(Source {
            name: name.clone(),
            size: data.len() as u64,
            reader: Box::new(std::io::Cursor::new(data.clone())),
        }),
        Upload::Path(path) | Upload::Named { path, .. } => {
            let shown = path.display().to_string();
            let name = match upload {
                Upload::Named { name, .. } => name.clone(),
                _ => path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| shown.clone()),
            };
            let file =
                File::open(path).map_err(|e| TransferError::Local(format!("{shown}: {e}")))?;
            let meta = file
                .metadata()
                .map_err(|e| TransferError::Local(format!("{shown}: {e}")))?;
            if meta.is_dir() {
                return Err(TransferError::Folder(name));
            }
            if !meta.is_file() {
                return Err(TransferError::Local(format!(
                    "{shown} is not a regular file"
                )));
            }
            Ok(Source {
                name,
                size: meta.len(),
                reader: Box::new(file),
            })
        }
    }
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
    // Every file is checked before the first goes: a folder refuses the batch up front.
    let mut sources = files
        .iter()
        .map(open_source)
        .collect::<Result<Vec<_>, _>>()?;
    let count = sources.len() as u32;
    let id = batch_id();
    let mut p = Progress {
        done: 0,
        total: sources.iter().map(|s| s.size).sum(),
    };
    progress(p);
    let mut last = None;
    for (index, source) in sources.iter_mut().enumerate() {
        let ticket = host.client.upload_begin(
            pane,
            &source.name,
            source.size,
            Some(UploadBatch {
                id: id.clone(),
                index: index as u32,
                count,
            }),
        )?;
        let (reader, mut writer) = stream::open(host.socket, &ticket.ticket)?;
        // A cancel between files still has to reach the daemon: it ends the batch on this
        // stream, which is aborted unfinished.
        send(source, &mut writer, cancel, |n| {
            p.done += n;
            progress(p);
        })?;
        writer
            .finish()
            .map_err(|e| TransferError::Remote(e.to_string()))?;
        // Every stream of the batch closes with the same outcome: waiting on the last one is
        // enough, and the others need no connection meanwhile.
        last = Some((reader, writer));
    }
    let (reader, _writer) = last.expect("at least one file");
    wait(reader, cancel)
}

/// Copies `source` to `writer`, checking `cancel` between frames; drops (aborts) nothing
/// itself: on an error the caller's writer goes unfinished.
fn send(
    source: &mut Source,
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
        writer
            .write_all(&buf[..n])
            .map_err(|e| TransferError::Remote(e.to_string()))?;
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

/// Fetches the file at `path`, as `pane` sees it, into `dest` (written beside it first, then
/// moved into place). With `if_version` still the file's version, writes nothing. A file over
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
        return Err(TransferError::TooLarge {
            size: info.size,
            max,
        });
    }
    let ticket = info
        .ticket
        .as_deref()
        .ok_or_else(|| TransferError::Remote("the daemon sent no ticket".into()))?;
    let (mut reader, mut writer) = stream::open(host.socket, ticket)?;
    let part = part_path(dest);
    let received = receive(&mut reader, &part, info.size, cancel, &mut progress);
    if let Err(e) = received {
        let _ = std::fs::remove_file(&part);
        let _ = writer.close(StreamClose::Aborted);
        return Err(e);
    }
    let closed = writer
        .finish()
        .map_err(|e| TransferError::Remote(e.to_string()))
        .and_then(|()| wait(reader, cancel));
    if let Err(e) = closed {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dest).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        TransferError::Local(format!("{}: {e}", dest.display()))
    })?;
    Ok(Downloaded::Saved(info))
}

/// Where `dest` is written until whole.
fn part_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(".{name}.{}.part", std::process::id()))
}

fn receive(
    reader: &mut StreamReader,
    part: &Path,
    size: u64,
    cancel: &Cancel,
    progress: &mut impl FnMut(Progress),
) -> Result<(), TransferError> {
    let local = |e: std::io::Error| TransferError::Local(format!("{}: {e}", part.display()));
    let mut file = File::create(part).map_err(local)?;
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
        return Err(TransferError::Remote(format!(
            "{} of {size} bytes arrived",
            p.done
        )));
    }
    file.sync_all().map_err(local)
}
