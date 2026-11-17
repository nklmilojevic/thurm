//! Files to and from a host's daemon ([`thurm_proto::CAP_TRANSFER`]), each on a byte stream of
//! its own: dropped and pasted files go up as one batch the daemon types the paths of once all
//! have landed; a file in a remote pane comes back to read here.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
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
            // Without blocking: a FIFO with no writer is refused below, not waited on.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
                .map_err(|e| TransferError::Local(format!("{shown}: {e}")))?;
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
    let (reader, writer) = last.expect("at least one file");
    wait_cancellable(reader, writer, cancel)
}

/// Waits for the batch's outcome on its last stream, everything sent. A cancel meanwhile
/// aborts the stream, which ends the batch on the daemon unless it is typing the paths
/// already; the daemon's answer then says which.
fn wait_cancellable(
    reader: StreamReader,
    writer: StreamWriter,
    cancel: &Cancel,
) -> Result<(), TransferError> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("upload-wait".into())
        .spawn(move || {
            let _ = tx.send(reader.wait_closed());
        })
        .map_err(|e| TransferError::Local(format!("cannot wait for the upload: {e}")))?;
    let mut writer = Some(writer);
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
                    let _ = w.close(StreamClose::Aborted);
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
    let (part, file) = create_part(dest)?;
    let received = receive(&mut reader, file, &part, info.size, cancel, &mut progress);
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

/// A new file beside `dest` that this download alone writes until whole: two downloads to
/// the same place never share one.
fn create_part(dest: &Path) -> Result<(PathBuf, File), TransferError> {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let part = dest.with_file_name(format!(".{name}.{}-{n}.part", std::process::id()));
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
            let mut seen = Vec::new();
            while let Ok(Some(frame)) = thurm_proto::stream::read_frame(&mut r) {
                let done = matches!(frame, StreamFrame::Close(_));
                seen.push(frame);
                if done {
                    thurm_proto::stream::write_frame(&mut w, &StreamFrame::Close(answer)).unwrap();
                    break;
                }
            }
            seen
        });
        (ours, daemon)
    }

    #[test]
    fn cancelling_while_the_batch_waits_aborts_its_stream() {
        let (socket, daemon) =
            fake_daemon(StreamClose::Internal("the upload was cancelled".into()));
        let (reader, mut writer) = stream::open_on(socket, "ticket").unwrap();
        writer.write_all(b"all of it").unwrap();
        writer.finish().unwrap();
        let cancel = Cancel::default();
        let later = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            later.cancel();
        });
        let got = wait_cancellable(reader, writer, &cancel);
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
    fn a_cancel_too_late_reports_what_the_daemon_did() {
        // The daemon was typing the paths already: it closes Ok, and the upload went.
        let (socket, daemon) = fake_daemon(StreamClose::Ok);
        let (reader, mut writer) = stream::open_on(socket, "ticket").unwrap();
        writer.finish().unwrap();
        let cancel = Cancel::default();
        cancel.cancel();
        assert!(wait_cancellable(reader, writer, &cancel).is_ok());
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
            let results = [fifo, link].map(|p| open_source(&Upload::Path(p)).err());
            let _ = tx.send(results.map(|e| e.map(|e| e.to_string())));
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
}
