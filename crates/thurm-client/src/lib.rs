//! Blocking client for `thurmd`.
//!
//! One reader thread demultiplexes responses (matched by request id) and events (delivered to
//! a callback). One writer thread drains outgoing frames in strict FIFO order so `send()` never
//! blocks the caller on socket I/O.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, Sender, bounded, unbounded};
use parking_lot::Mutex;

pub mod stream;

pub use thurm_proto as proto;
use thurm_proto::{Envelope, Event, PROTOCOL_VERSION, Request, Response, ServerMessage, codec};

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Response, String>>>>>;

const WRITE_QUEUED: u8 = 0;
const WRITE_STARTED: u8 = 1;
const WRITE_CANCELLED: u8 = 2;

/// Maximum total encoded frame bytes allowed in the outgoing writer queue (including the frame
/// currently being written) so a stalled peer or tunnel cannot grow client memory without bound.
pub const MAX_QUEUED_WRITE_BYTES: usize = 16 * 1024 * 1024;

/// Maximum time `Client::drop` waits for already accepted writes to drain on a live connection
/// before forcing socket shutdown so a stalled peer cannot hang teardown.
const DROP_FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

/// Releases a queued-byte reservation automatically when a `WriteJob` finishes, is cancelled,
/// or is dropped during teardown.
struct QueueGuard {
    counter: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct WriteJob {
    buf: Vec<u8>,
    deadline: Option<Instant>,
    /// Present for `request` / `request_timeout` so the caller gets the write result.
    /// `None` for fire-and-forget `send()`.
    done: Option<Sender<Result<(), ClientError>>>,
    /// Coordinates pre-write cancellation when a caller's deadline expires while the job is
    /// still waiting in the queue behind an earlier write.
    state: Option<Arc<AtomicU8>>,
    _guard: QueueGuard,
}

enum WriteCmd {
    Job(WriteJob),
    Flush(Sender<()>),
    FlushAndShutdown(Sender<()>),
    Shutdown,
}

pub struct Client {
    write_tx: Sender<WriteCmd>,
    queued_bytes: Arc<AtomicUsize>,
    max_queued_bytes: AtomicUsize,
    closer: UnixStream,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    forced_close: AtomicBool,
    pub hello: Mutex<Option<Response>>,
}

#[derive(Debug)]
pub enum ClientError {
    Connect(String),
    Disconnected,
    Timeout,
    QueueFull,
    Daemon(String),
    Io(std::io::Error),
}

/// The [`ClientError::Daemon`] of a request whose answer this build cannot decode (a daemon of
/// another build).
pub const UNREADABLE_ANSWER: &str = "the daemon's answer could not be read";

/// What a daemon without [`thurm_proto::CAP_TRANSFER`] is told.
pub const NO_TRANSFER: &str =
    "this Thurm daemon cannot take or send files yet; update Thurm on this host";

/// What a daemon without [`thurm_proto::CAP_COPY`] is told.
pub const NO_COPY: &str = "this Thurm daemon cannot copy files yet; update Thurm on this host";

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Connect(e) => write!(f, "cannot connect to thurmd: {e}"),
            ClientError::Disconnected => write!(f, "connection to thurmd lost"),
            ClientError::Timeout => write!(f, "thurmd did not answer in time"),
            ClientError::QueueFull => write!(f, "outgoing write queue to thurmd is full"),
            ClientError::Daemon(e) => write!(f, "{e}"),
            ClientError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e)
    }
}

pub struct ConnectOptions<'a> {
    pub socket: PathBuf,
    /// Spawn this daemon executable (with `--daemonize`) when nothing listens on the socket.
    pub spawn_daemon: Option<&'a Path>,
    pub client_name: &'a str,
    pub ui: bool,
}

impl Default for ConnectOptions<'_> {
    fn default() -> Self {
        Self {
            socket: thurm_config::socket_path(),
            spawn_daemon: None,
            client_name: "client",
            ui: false,
        }
    }
}

/// Pid of the process listening on `socket` (the daemon), from the peer credentials.
pub fn daemon_pid(socket: &Path) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let stream = connect_daemon(socket).ok()?;
    let fd = stream.as_raw_fd();
    #[cfg(target_vendor = "apple")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        (rc == 0 && pid > 0).then_some(pid as u32)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
    }
}

/// Pid of the daemon serving `socket`: its pid file (`<socket>.pid`), else a `thurmd` process of
/// ours whose command line names the socket (daemons from before pid files), else the socket's
/// peer pid (right for `--foreground` daemons; a daemonized one forked after binding).
fn find_daemon_pid(socket: &Path) -> Option<u32> {
    let alive = |pid: u32| unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    let mut pid_file = socket.as_os_str().to_owned();
    pid_file.push(".pid");
    if let Some(pid) = std::fs::read_to_string(PathBuf::from(pid_file))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&p| alive(p))
    {
        return Some(pid);
    }
    let uid = unsafe { libc::getuid() };
    let sock = socket.display().to_string();
    // Ask ps for the short executable name alongside args so spaces or hyphens in
    // argv[0] (e.g. `.../Thurm - Beta.app/.../Thurm Daemon.app/Contents/MacOS/thurmd`)
    // cannot be confused with options or shell wrappers. On Linux `comm` is the short
    // name; on macOS/BSD `comm` is the full path and `ucomm` is the short name.
    #[cfg(target_os = "linux")]
    let cols = "pid=,uid=,comm=,args=";
    #[cfg(not(target_os = "linux"))]
    let cols = "pid=,uid=,ucomm=,args=";
    let listed = std::process::Command::new("/bin/ps")
        .args(["-A", "-ww", "-o", cols])
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|line| parse_ps_daemon_pid(line, uid, &sock, std::process::id()))
        });
    listed.or_else(|| daemon_pid(socket).filter(|&p| alive(p)))
}

fn parse_ps_daemon_pid(line: &str, uid: u32, sock: &str, self_pid: u32) -> Option<u32> {
    let rest = line.trim_start();
    let (pid_str, rest) = rest.split_once(char::is_whitespace)?;
    let pid: u32 = pid_str.parse().ok()?;
    let rest = rest.trim_start();
    let (owner_str, rest) = rest.split_once(char::is_whitespace)?;
    let owner: u32 = owner_str.parse().ok()?;
    let rest = rest.trim_start();
    let (comm, rest) = rest.split_once(char::is_whitespace)?;
    if owner != uid || pid == self_pid || comm != "thurmd" {
        return None;
    }
    daemon_cmdline_has_socket(rest.trim_start(), sock).then_some(pid)
}

/// True when `cmdline` is a `thurmd` invocation whose options after the executable path
/// include `--socket <sock>`. Handles spaces and dash-leading words in the executable path
/// while rejecting non-daemon processes whose spaced `ucomm` merely started with `thurmd`.
fn daemon_cmdline_has_socket(cmdline: &str, sock: &str) -> bool {
    if sock.is_empty() {
        return false;
    }
    if let Some(rest) = cmdline.strip_prefix("thurmd")
        && rest.starts_with(char::is_whitespace)
        && opts_have_socket(rest.trim_start(), sock)
    {
        return true;
    }
    let mut search_from = 0;
    while let Some(rel) = cmdline[search_from..].find("/thurmd") {
        let end = search_from + rel + "/thurmd".len();
        search_from = end;
        let exe = &cmdline[..end];
        let rest = &cmdline[end..];
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        // A continuation token starting with `/`, `./`, or `../` indicates a second path
        // argument to another executable rather than a single spaced executable path.
        let single_path = exe
            .split_whitespace()
            .skip(1)
            .all(|t| !t.starts_with('/') && !t.starts_with("./") && !t.starts_with("../"));
        if single_path && opts_have_socket(rest.trim_start(), sock) {
            return true;
        }
    }
    false
}

fn opts_have_socket(opts: &str, sock: &str) -> bool {
    // `thurmd` only accepts options (starting with `-`, length > 1), never positional args.
    let first = opts.split_whitespace().next().unwrap_or("");
    if !first.starts_with('-') || first.len() <= 1 {
        return false;
    }
    let mut s = opts;
    while !s.is_empty() {
        s = s.trim_start();
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        let tok = &s[..end];
        s = &s[end..];
        if tok == "--socket" && s.starts_with(char::is_whitespace) {
            let after_ws = s.trim_start();
            if let Some(rest) = after_ws.strip_prefix(sock)
                && (rest.is_empty() || rest.starts_with(char::is_whitespace))
            {
                return true;
            }
        }
    }
    false
}

/// Stop the daemon on `socket` whatever protocol it speaks (e.g. one left over from an older
/// Thurm): SIGTERM saves the session and exits. Returns its pid once the socket is gone.
pub fn terminate_daemon(socket: &Path) -> Result<u32, ClientError> {
    let pid = find_daemon_pid(socket)
        .ok_or_else(|| ClientError::Connect("no daemon is running".into()))?;
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        return Err(ClientError::Io(std::io::Error::last_os_error()));
    }
    // Wait for that process to be gone (a new daemon may already own the socket).
    let deadline = Instant::now() + Duration::from_secs(10);
    while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        if Instant::now() > deadline {
            return Err(ClientError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(pid)
}

/// The protocol the daemon on `socket` speaks, and its build when it speaks ours.
pub fn probe_daemon(socket: &Path) -> Result<(u32, Option<String>), ClientError> {
    let opts = ConnectOptions {
        socket: socket.to_owned(),
        spawn_daemon: None,
        client_name: "thurm-probe",
        ui: false,
    };
    match Client::connect(opts, |_| {}, || {}) {
        Ok(c) => match c.hello.lock().clone() {
            Some(Response::Hello { version, build, .. }) => Ok((version, Some(build))),
            _ => Ok((PROTOCOL_VERSION, None)),
        },
        Err(ClientError::Daemon(e)) => mismatched_protocol(&e)
            .map(|v| (v, None))
            .ok_or(ClientError::Daemon(e)),
        Err(e) => Err(e),
    }
}

/// The daemon's version from a refused Hello ("protocol mismatch: daemon 12, client 13").
fn mismatched_protocol(error: &str) -> Option<u32> {
    let rest = error.split("protocol mismatch: daemon ").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[derive(Debug)]
pub enum UpgradeError {
    /// The daemon predates in-place upgrades: only a restart (`terminate_daemon`) replaces it.
    TooOld(u32),
    Failed(ClientError),
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpgradeError::TooOld(v) => write!(
                f,
                "the running thurmd (protocol {v}) can't be upgraded in place; restart it"
            ),
            UpgradeError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for UpgradeError {}

/// Replace the daemon on `socket` with `daemon` without stopping any pane: SIGUSR2 makes it
/// exec that binary, handing over its PTYs (see thurmd's `upgrade.rs`). Waits until the daemon
/// answers with our [`thurm_proto::BUILD`] (the caller ships `daemon`, so they match) and
/// returns its pid. Already that build: nothing to do.
pub fn upgrade_daemon(socket: &Path, daemon: &Path) -> Result<u32, UpgradeError> {
    let (version, build) = probe_daemon(socket).map_err(UpgradeError::Failed)?;
    if version < thurm_proto::HOT_UPGRADE_PROTOCOL {
        return Err(UpgradeError::TooOld(version));
    }
    let pid = find_daemon_pid(socket)
        .ok_or_else(|| UpgradeError::Failed(ClientError::Connect("no daemon is running".into())))?;
    if build.as_deref() == Some(thurm_proto::BUILD) {
        return Ok(pid);
    }
    let mut req = socket.as_os_str().to_owned();
    req.push(".upgrade");
    std::fs::write(PathBuf::from(req), daemon.as_os_str().as_encoded_bytes())
        .map_err(|e| UpgradeError::Failed(e.into()))?;
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGUSR2) } != 0 {
        return Err(UpgradeError::Failed(std::io::Error::last_os_error().into()));
    }
    // The daemon checks for the signal every 100 ms, then saves, execs and adopts its panes.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok((_, Some(b))) = probe_daemon(socket)
            && b == thurm_proto::BUILD
        {
            return Ok(pid);
        }
        if Instant::now() > deadline {
            return Err(UpgradeError::Failed(ClientError::Daemon(
                "thurmd did not upgrade (see thurmd.log)".into(),
            )));
        }
    }
}

/// Bundled macOS helper app path for `thurmd` relative to `Contents/Helpers`.
pub const BUNDLED_DAEMON: &str = "Thurm Daemon.app/Contents/MacOS/thurmd";

/// Locate `thurmd` inside `dir`: the macOS helper bundle first, then a flat `thurmd`.
pub fn find_daemon_in(dir: &Path) -> Option<PathBuf> {
    let bundled = dir.join(BUNDLED_DAEMON);
    if bundled.is_file() {
        return Some(bundled);
    }
    let flat = dir.join("thurmd");
    flat.is_file().then_some(flat)
}

/// Locate `thurmd`: next to the current executable (or its symlink target), then on PATH.
pub fn find_daemon() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent()
            && let Some(p) = find_daemon_in(dir)
        {
            return Some(p);
        }
        if let Ok(real) = std::fs::canonicalize(&exe)
            && real != exe
            && let Some(dir) = real.parent()
            && let Some(p) = find_daemon_in(dir)
        {
            return Some(p);
        }
    }
    thurm_config::which("thurmd")
}

/// Connects to the daemon socket at `path`, which must be served by this user: another user
/// who got to a shared directory first must not receive our keystrokes.
pub fn connect_daemon(path: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    if !thurm_config::peer_is_same_user(&stream) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is served by another user", path.display()),
        ));
    }
    Ok(stream)
}

fn connect_socket(opts: &ConnectOptions<'_>) -> Result<UnixStream, ClientError> {
    match connect_daemon(&opts.socket) {
        Ok(s) => return Ok(s),
        Err(e) if opts.spawn_daemon.is_none() => return Err(ClientError::Connect(e.to_string())),
        Err(_) => {}
    }
    let daemon = opts.spawn_daemon.expect("checked above");
    let mut cmd = std::process::Command::new(daemon);
    cmd.arg("--daemonize").arg("--socket").arg(&opts.socket);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| ClientError::Connect(format!("spawning {}: {e}", daemon.display())))?;
    // The launcher process exits right after forking the real daemon.
    let _ = child.wait();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match connect_daemon(&opts.socket) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() > deadline => return Err(ClientError::Connect(e.to_string())),
            Err(_) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

impl Client {
    /// Connect (spawning the daemon if allowed), say hello, and start the reader thread.
    /// `on_event` runs on the reader thread; `on_disconnect` runs once when the connection drops.
    pub fn connect(
        opts: ConnectOptions<'_>,
        on_event: impl Fn(Event) + Send + 'static,
        on_disconnect: impl FnOnce() + Send + 'static,
    ) -> Result<Arc<Client>, ClientError> {
        Self::connect_with(opts, &[], on_event, on_disconnect)
    }

    /// Like [`Client::connect`], with `extra_capabilities` in the Hello besides the build's.
    pub fn connect_with(
        opts: ConnectOptions<'_>,
        extra_capabilities: &[&str],
        on_event: impl Fn(Event) + Send + 'static,
        on_disconnect: impl FnOnce() + Send + 'static,
    ) -> Result<Arc<Client>, ClientError> {
        let stream = connect_socket(&opts)?;
        Self::from_stream(
            stream,
            opts.client_name,
            opts.ui,
            extra_capabilities,
            on_event,
            on_disconnect,
        )
    }

    /// Like [`Client::connect`], over an already connected stream: the daemon socket, or one end
    /// of a pair whose other end a transport (an in-process ssh channel) relays to the daemon.
    pub fn from_stream(
        stream: UnixStream,
        client_name: &str,
        ui: bool,
        extra_capabilities: &[&str],
        on_event: impl Fn(Event) + Send + 'static,
        on_disconnect: impl FnOnce() + Send + 'static,
    ) -> Result<Arc<Client>, ClientError> {
        Self::from_stream_timeout(
            stream,
            client_name,
            ui,
            extra_capabilities,
            None,
            on_event,
            on_disconnect,
        )
    }

    /// Like [`Client::from_stream`], giving the Hello at most `hello_timeout`. Whenever the
    /// Hello fails the stream is shut down, so the reader (and whatever relays the stream) ends.
    pub fn from_stream_timeout(
        stream: UnixStream,
        client_name: &str,
        ui: bool,
        extra_capabilities: &[&str],
        hello_timeout: Option<Duration>,
        on_event: impl Fn(Event) + Send + 'static,
        on_disconnect: impl FnOnce() + Send + 'static,
    ) -> Result<Arc<Client>, ClientError> {
        let read_half = stream.try_clone()?;
        let closer = stream.try_clone()?;
        let client_closer = stream.try_clone()?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let (write_tx, write_rx) = unbounded::<WriteCmd>();

        let writer_alive = alive.clone();
        std::thread::Builder::new()
            .name("thurm-client-writer".into())
            .spawn(move || {
                let mut stream = stream;
                while let Ok(cmd) = write_rx.recv() {
                    match cmd {
                        WriteCmd::Shutdown => break,
                        WriteCmd::Flush(done) => {
                            let _ = stream.flush();
                            let _ = done.send(());
                        }
                        WriteCmd::FlushAndShutdown(done) => {
                            let _ = stream.flush();
                            let _ = done.send(());
                            break;
                        }
                        WriteCmd::Job(job) => {
                            if let Some(state) = &job.state {
                                if job.deadline.is_some_and(|d| Instant::now() >= d) {
                                    let _ = state.compare_exchange(
                                        WRITE_QUEUED,
                                        WRITE_CANCELLED,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    );
                                    if let Some(done) = job.done {
                                        let _ = done.send(Err(ClientError::Timeout));
                                    }
                                    continue;
                                }
                                if state
                                    .compare_exchange(
                                        WRITE_QUEUED,
                                        WRITE_STARTED,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    )
                                    .is_err()
                                {
                                    // Cancelled by caller while waiting in the queue.
                                    continue;
                                }
                            } else if job.deadline.is_some_and(|d| Instant::now() >= d) {
                                if let Some(done) = job.done {
                                    let _ = done.send(Err(ClientError::Timeout));
                                }
                                continue;
                            }

                            let (res, broken) = write_frame(&mut stream, &job.buf, job.deadline);
                            if broken {
                                writer_alive.store(false, Ordering::Relaxed);
                                let _ = stream.shutdown(std::net::Shutdown::Both);
                            }
                            if let Some(done) = job.done {
                                let _ = done.send(res);
                            }
                            if broken {
                                break;
                            }
                        }
                    }
                }
            })?;

        let client = Arc::new(Client {
            write_tx: write_tx.clone(),
            queued_bytes,
            max_queued_bytes: AtomicUsize::new(MAX_QUEUED_WRITE_BYTES),
            closer: client_closer,
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            alive: alive.clone(),
            forced_close: AtomicBool::new(false),
            hello: Mutex::new(None),
        });

        std::thread::Builder::new()
            .name("thurm-client-reader".into())
            .spawn(move || {
                // However this thread ends (a panic in `on_event` too), requests still waiting
                // get an error instead of waiting forever, and the writer thread exits too.
                struct Finish(Arc<AtomicBool>, Pending, Sender<WriteCmd>);
                impl Drop for Finish {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Relaxed);
                        for (_, tx) in self.1.lock().drain() {
                            let _ = tx.send(Err("disconnected".into()));
                        }
                        let _ = self.2.send(WriteCmd::Shutdown);
                    }
                }
                let finish = Finish(alive.clone(), pending.clone(), write_tx);
                let mut reader = BufReader::with_capacity(256 * 1024, read_half);
                while let Ok(Some(frame)) = codec::read_frame(&mut reader) {
                    // A message of a newer daemon this client can't decode is skipped (the frames
                    // after it are intact); a response still ends its request, with an error.
                    let Ok(msg) = codec::decode_message(&frame) else {
                        if let Some(id) = codec::response_id(&frame)
                            && let Some(tx) = pending.lock().remove(&id)
                        {
                            let _ = tx.send(Err(UNREADABLE_ANSWER.into()));
                        }
                        continue;
                    };
                    match msg {
                        ServerMessage::Response { id, result } => {
                            if let Some(tx) = pending.lock().remove(&id) {
                                let _ = tx.send(result);
                            }
                        }
                        ServerMessage::Event(ev) => on_event(ev),
                    }
                }
                drop(finish);
                on_disconnect();
            })?;

        let hello = client
            .request_timeout(
                Request::Hello {
                    client: client_name.to_owned(),
                    version: PROTOCOL_VERSION,
                    ui,
                    capabilities: thurm_proto::CAPABILITIES
                        .iter()
                        .chain(extra_capabilities)
                        .map(|c| (*c).to_owned())
                        .collect(),
                },
                hello_timeout,
            )
            .inspect_err(|_| {
                client.force_close();
                let _ = closer.shutdown(std::net::Shutdown::Both);
            })?;
        if let Response::Hello {
            version,
            capabilities,
            ..
        } = &hello
            && !thurm_proto::hello_compatible(*version, capabilities)
        {
            client.force_close();
            let _ = closer.shutdown(std::net::Shutdown::Both);
            // Callers read the daemon's version from the "protocol mismatch: daemon N" form and
            // upgrade it in place, so an older build of this very version keeps it.
            let why = if thurm_proto::protocol_compatible(*version) {
                format!(
                    ": the daemon is an older build of protocol {version} (without {}), whose \
                     answers this client cannot read; upgrade it",
                    thurm_proto::CAP_ACTIVE_SIZE
                )
            } else {
                String::new()
            };
            return Err(ClientError::Daemon(format!(
                "protocol mismatch: daemon {version}, client {PROTOCOL_VERSION}{why}"
            )));
        }
        *client.hello.lock() = Some(hello);
        Ok(client)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Bytes currently queued or in-flight in the outgoing writer thread.
    pub fn queued_write_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Acquire)
    }

    /// Overrides the outgoing byte budget (test hook so stalled-peer backpressure tests do not
    /// need to encode tens of megabytes in unoptimized debug builds).
    #[doc(hidden)]
    pub fn set_max_queued_bytes(&self, bytes: usize) {
        self.max_queued_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Waits for every frame accepted before this call to finish writing to the socket.
    pub fn flush(&self, timeout: Option<Duration>) -> Result<(), ClientError> {
        if !self.is_alive() {
            return Err(ClientError::Disconnected);
        }
        let (done_tx, done_rx) = bounded(1);
        self.write_tx
            .send(WriteCmd::Flush(done_tx))
            .map_err(|_| ClientError::Disconnected)?;
        match timeout {
            None => done_rx.recv().map_err(|_| ClientError::Disconnected),
            Some(limit) => match done_rx.recv_timeout(limit) {
                Ok(()) => Ok(()),
                Err(RecvTimeoutError::Timeout) => Err(ClientError::Timeout),
                Err(RecvTimeoutError::Disconnected) => Err(ClientError::Disconnected),
            },
        }
    }

    /// Immediately shuts down the connection without waiting for queued writes to drain.
    /// Useful when tearing down a connection whose peer has stalled.
    pub fn force_close(&self) {
        self.forced_close.store(true, Ordering::Release);
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.write_tx.send(WriteCmd::Shutdown);
        let _ = self.closer.shutdown(std::net::Shutdown::Both);
    }

    fn reserve_write_bytes(&self, bytes: usize) -> Result<QueueGuard, ClientError> {
        // A single frame larger than `MAX_QUEUED_WRITE_BYTES` (e.g. a 32 MiB clipboard payload)
        // is allowed when the queue is completely idle, up to the protocol's max frame size;
        // once anything is in-flight or queued, total buffered bytes cannot exceed the cap.
        if bytes > codec::MAX_MESSAGE.saturating_add(4) {
            return Err(ClientError::QueueFull);
        }
        let cap = self.max_queued_bytes.load(Ordering::Relaxed);
        let mut cur = self.queued_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = cur.checked_add(bytes) else {
                return Err(ClientError::QueueFull);
            };
            if cur > 0 && next > cap {
                return Err(ClientError::QueueFull);
            }
            match self.queued_bytes.compare_exchange_weak(
                cur,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(QueueGuard {
                        counter: self.queued_bytes.clone(),
                        bytes,
                    });
                }
                Err(actual) => cur = actual,
            }
        }
    }

    fn encode_checked(&self, env: &Envelope) -> Result<Vec<u8>, ClientError> {
        if !self.is_alive() {
            return Err(ClientError::Disconnected);
        }
        if matches!(env.request, Request::SearchPaths { .. })
            && !matches!(&*self.hello.lock(), Some(Response::Hello { capabilities, .. })
                if capabilities.iter().any(|c| c == thurm_proto::CAP_PATH_SEARCH))
        {
            return Err(ClientError::Daemon(
                "Update Thurm on this host to use Insert Path.".into(),
            ));
        }
        if matches!(env.request, Request::Ext { .. })
            && !matches!(&*self.hello.lock(), Some(Response::Hello { capabilities, .. })
                if capabilities.iter().any(|c| c == thurm_proto::CAP_EXT))
        {
            return Err(ClientError::Daemon(
                "this Thurm daemon has no actions or plugins yet; update Thurm on this host".into(),
            ));
        }
        if env.request.is_clipboard_tool()
            && !matches!(&*self.hello.lock(), Some(Response::Hello { capabilities, .. })
                if capabilities.iter().any(|c| c == thurm_proto::CAP_CLIP_QUERY))
        {
            return Err(ClientError::Daemon(
                "this Thurm daemon has no clipboard tools yet; restart it with `thurm daemon upgrade`"
                    .into(),
            ));
        }
        Ok(codec::encode(env)?)
    }

    /// Writes `env` through the FIFO writer thread, giving up at `deadline` (waiting in the
    /// queue included). A write cut off mid-message would leave the stream out of step, so a
    /// timeout once writing began closes the connection; one before (queued behind an earlier
    /// frame, or an expired deadline) only fails this request.
    fn write_by(&self, env: &Envelope, deadline: Option<Instant>) -> Result<(), ClientError> {
        let buf = self.encode_checked(env)?;
        if let Some(d) = deadline
            && d.saturating_duration_since(Instant::now()).is_zero()
        {
            return Err(ClientError::Timeout);
        }
        let guard = self.reserve_write_bytes(buf.len())?;
        let (done_tx, done_rx) = bounded(1);
        let state = deadline.map(|_| Arc::new(AtomicU8::new(WRITE_QUEUED)));
        self.write_tx
            .send(WriteCmd::Job(WriteJob {
                buf,
                deadline,
                done: Some(done_tx),
                state: state.clone(),
                _guard: guard,
            }))
            .map_err(|_| ClientError::Disconnected)?;
        match deadline {
            None => done_rx.recv().map_err(|_| ClientError::Disconnected)?,
            Some(d) => match done_rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                Ok(res) => res,
                Err(RecvTimeoutError::Disconnected) => Err(ClientError::Disconnected),
                Err(RecvTimeoutError::Timeout) => {
                    let state = state.expect("set when deadline is Some");
                    if state
                        .compare_exchange(
                            WRITE_QUEUED,
                            WRITE_CANCELLED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        Err(ClientError::Timeout)
                    } else {
                        // The writer thread already started writing this frame with the same
                        // deadline; wait for it to finish (and update `alive` if it timed out
                        // mid-frame).
                        done_rx.recv().map_err(|_| ClientError::Disconnected)?
                    }
                }
            },
        }
    }

    /// Send and wait for the response (no timeout).
    pub fn request(&self, request: Request) -> Result<Response, ClientError> {
        self.request_timeout(request, None)
    }

    pub fn request_timeout(
        &self,
        request: Request,
        timeout: Option<Duration>,
    ) -> Result<Response, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = bounded(1);
        self.pending.lock().insert(id, tx);
        // One deadline for sending and for the answer.
        let deadline = timeout.map(|t| Instant::now() + t);
        if let Err(e) = self.write_by(&Envelope { id, request }, deadline) {
            self.pending.lock().remove(&id);
            return Err(e);
        }
        let result = match deadline {
            Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                Ok(r) => r,
                Err(_) => {
                    // Its late answer has nobody to go to.
                    self.pending.lock().remove(&id);
                    return Err(ClientError::Timeout);
                }
            },
            None => rx.recv().map_err(|_| ClientError::Disconnected)?,
        };
        if matches!(&result, Err(e) if e == "disconnected") {
            return Err(ClientError::Disconnected);
        }
        result.map_err(ClientError::Daemon)
    }

    /// Fire and forget (id 0: the daemon sends no response). Enqueues onto the FIFO writer
    /// thread without blocking the caller on socket I/O, rejecting with [`ClientError::QueueFull`]
    /// if the outgoing byte budget ([`MAX_QUEUED_WRITE_BYTES`]) is exhausted.
    pub fn send(&self, request: Request) -> Result<(), ClientError> {
        let buf = self.encode_checked(&Envelope { id: 0, request })?;
        let guard = self.reserve_write_bytes(buf.len())?;
        self.write_tx
            .send(WriteCmd::Job(WriteJob {
                buf,
                deadline: None,
                done: None,
                state: None,
                _guard: guard,
            }))
            .map_err(|_| ClientError::Disconnected)
    }

    /// The daemon's capabilities, from its answer to the Hello.
    pub fn capabilities(&self) -> Vec<String> {
        match &*self.hello.lock() {
            Some(Response::Hello { capabilities, .. }) => capabilities.clone(),
            _ => Vec::new(),
        }
    }

    /// Whether the daemon listed `cap` in its answer to the Hello.
    pub fn has_capability(&self, cap: &str) -> bool {
        matches!(&*self.hello.lock(), Some(Response::Hello { capabilities, .. })
            if capabilities.iter().any(|c| c == cap))
    }

    /// Reserves a stream to TCP `port` on the daemon's host (loopback); open it with
    /// [`stream::open`] before the ticket expires.
    pub fn dial_begin(&self, port: u16) -> Result<stream::Ticket, ClientError> {
        if !self.has_capability(thurm_proto::CAP_STREAMS) {
            return Err(ClientError::Daemon(stream::NO_STREAMS.into()));
        }
        match self.request(Request::DialBegin { port })? {
            Response::Ticket { ticket, expires_ms } => Ok(stream::Ticket { ticket, expires_ms }),
            _ => Err(ClientError::Daemon(UNREADABLE_ANSWER.into())),
        }
    }

    /// [`Client::dial_begin`] to the loopback address `at` only. A daemon without
    /// [`thurm_proto::CAP_DIAL_AT`] dials as for `Any` (`127.0.0.1`, then `[::1]`).
    pub fn dial_at(
        &self,
        port: u16,
        at: thurm_proto::stream::Loopback,
    ) -> Result<stream::Ticket, ClientError> {
        if at.is_any() || !self.has_capability(thurm_proto::CAP_DIAL_AT) {
            return self.dial_begin(port);
        }
        match self.request(Request::DialAt { port, at })? {
            Response::Ticket { ticket, expires_ms } => Ok(stream::Ticket { ticket, expires_ms }),
            _ => Err(ClientError::Daemon(UNREADABLE_ANSWER.into())),
        }
    }

    /// Reserves a stream that writes a file of `size` bytes for `pane` on the daemon's host,
    /// alone or as part of `batch` ([`Request::UploadBegin`]); open it with [`stream::open`]
    /// before the ticket expires.
    pub fn upload_begin(
        &self,
        pane: thurm_proto::PaneId,
        name: &str,
        size: u64,
        batch: Option<thurm_proto::UploadBatch>,
    ) -> Result<stream::Ticket, ClientError> {
        if !self.has_capability(thurm_proto::CAP_TRANSFER) {
            return Err(ClientError::Daemon(NO_TRANSFER.into()));
        }
        match self.request(Request::UploadBegin {
            pane,
            name: name.to_owned(),
            size,
            batch,
        })? {
            Response::Ticket { ticket, expires_ms } => Ok(stream::Ticket { ticket, expires_ms }),
            _ => Err(ClientError::Daemon(UNREADABLE_ANSWER.into())),
        }
    }

    /// Asks for the file at `path` as `pane` sees it ([`Request::DownloadBegin`]): its details,
    /// and a ticket for a stream sending it unless `if_version` is still its version.
    pub fn download_begin(
        &self,
        pane: thurm_proto::PaneId,
        path: &str,
        if_version: Option<&str>,
    ) -> Result<thurm_proto::DownloadInfo, ClientError> {
        if !self.has_capability(thurm_proto::CAP_TRANSFER) {
            return Err(ClientError::Daemon(NO_TRANSFER.into()));
        }
        match self.request(Request::DownloadBegin {
            pane,
            path: path.to_owned(),
            if_version: if_version.map(str::to_owned),
        })? {
            Response::Download(info) => Ok(info),
            _ => Err(ClientError::Daemon(UNREADABLE_ANSWER.into())),
        }
    }
}

impl Client {
    /// Reserves a stream that writes a file of `size` bytes at `path` on the daemon's host
    /// ([`Request::CopyTo`]); open it with [`stream::open`] before the ticket expires. Also
    /// where the file goes there.
    pub fn copy_to_begin(
        &self,
        path: &str,
        name: &str,
        size: u64,
        mode: u32,
        replace: bool,
    ) -> Result<(stream::Ticket, String), ClientError> {
        if !self.has_capability(thurm_proto::CAP_COPY) {
            return Err(ClientError::Daemon(NO_COPY.into()));
        }
        match self.request(Request::CopyTo {
            path: path.to_owned(),
            name: name.to_owned(),
            size,
            mode,
            replace,
        })? {
            Response::CopyTicket {
                ticket,
                expires_ms,
                path,
            } => Ok((stream::Ticket { ticket, expires_ms }, path)),
            _ => Err(ClientError::Daemon(UNREADABLE_ANSWER.into())),
        }
    }

    /// Asks for the file at `path` on the daemon's host ([`Request::CopyFrom`]): its details,
    /// and a ticket for a stream sending it.
    pub fn copy_from_begin(&self, path: &str) -> Result<thurm_proto::DownloadInfo, ClientError> {
        if !self.has_capability(thurm_proto::CAP_COPY) {
            return Err(ClientError::Daemon(NO_COPY.into()));
        }
        match self.request(Request::CopyFrom {
            path: path.to_owned(),
        })? {
            Response::Download(info) => Ok(info),
            _ => Err(ClientError::Daemon(UNREADABLE_ANSWER.into())),
        }
    }
}

/// Writes one encoded frame to `stream`, respecting `deadline`. Returns `(result, broken)`
/// where `broken` is true whenever the stream can no longer be used safely (partial write
/// timeout or socket error).
fn write_frame(
    stream: &mut UnixStream,
    buf: &[u8],
    deadline: Option<Instant>,
) -> (Result<(), ClientError>, bool) {
    let mut started = false;
    let written = match deadline {
        None => {
            started = true;
            stream.write_all(buf)
        }
        // Write by hand, giving each call only the time left: a peer that reads a little at a
        // time must not stretch the request past its deadline.
        Some(d) => (|| {
            let mut rest = buf;
            while !rest.is_empty() {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(std::io::ErrorKind::TimedOut.into());
                }
                stream.set_write_timeout(Some(left))?;
                // A write that times out may still have sent part of it (macOS reports EAGAIN
                // then): from here on the stream can be out of step.
                started = true;
                match stream.write(rest) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => rest = &rest[n..],
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })(),
    };
    if deadline.is_some() {
        let _ = stream.set_write_timeout(None);
    }
    match written {
        Ok(()) => (Ok(()), false),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            (Err(ClientError::Timeout), started)
        }
        Err(e) => (Err(e.into()), true),
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // On a normal disconnect of a live connection, let already accepted writes drain to the
        // socket before closing it (bounded by `DROP_FLUSH_TIMEOUT` so a stalled peer cannot hang
        // teardown). Forced or already-dead connections shut down immediately.
        if !self.forced_close.load(Ordering::Acquire) && self.alive.load(Ordering::Relaxed) {
            let (done_tx, done_rx) = bounded(1);
            if self
                .write_tx
                .send(WriteCmd::FlushAndShutdown(done_tx))
                .is_ok()
            {
                let _ = done_rx.recv_timeout(DROP_FLUSH_TIMEOUT);
            }
        } else {
            let _ = self.write_tx.send(WriteCmd::Shutdown);
        }
        let _ = self.closer.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn protocol_from_refused_hello() {
        assert_eq!(
            mismatched_protocol("protocol mismatch: daemon 12, client 13"),
            Some(12)
        );
        assert_eq!(mismatched_protocol("no such pane: 3"), None);
    }

    /// A daemon of this protocol from before `active-size`: refused with a message that says
    /// so, and still upgradable in place (`probe_daemon` reads its version, without a build).
    #[test]
    fn older_build_of_this_protocol_is_refused_and_upgradable() {
        let dir = std::env::temp_dir().join(format!("thurm-client-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s");
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = stream.unwrap();
                let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut s) else {
                    continue;
                };
                codec::write_message(
                    &mut s,
                    &ServerMessage::Response {
                        id: env.id,
                        result: Ok(Response::Hello {
                            version: PROTOCOL_VERSION,
                            daemon_pid: 1,
                            restored: false,
                            build: "older".into(),
                            capabilities: vec!["hello-capabilities".into()],
                        }),
                    },
                )
                .unwrap();
            }
        });
        let opts = || ConnectOptions {
            socket: sock.clone(),
            spawn_daemon: None,
            client_name: "test",
            ui: false,
        };
        let Err(ClientError::Daemon(e)) = Client::connect(opts(), |_| {}, || {}) else {
            panic!("an older build was accepted")
        };
        assert!(e.contains("older build"), "{e}");
        assert_eq!(mismatched_protocol(&e), Some(PROTOCOL_VERSION));
        assert_eq!(probe_daemon(&sock).unwrap(), (PROTOCOL_VERSION, None));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn path_search_requires_peer_support_and_keeps_old_connections_usable() {
        for supported in [false, true] {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            let server = std::thread::spawn(move || {
                let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
                let mut capabilities = vec![thurm_proto::CAP_ACTIVE_SIZE.to_owned()];
                if supported {
                    capabilities.push(thurm_proto::CAP_PATH_SEARCH.to_owned());
                }
                codec::write_message(
                    &mut peer,
                    &ServerMessage::Response {
                        id: hello.id,
                        result: Ok(Response::Hello {
                            version: PROTOCOL_VERSION,
                            daemon_pid: 1,
                            restored: false,
                            build: "fixture".into(),
                            capabilities,
                        }),
                    },
                )
                .unwrap();
                if supported {
                    let env: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
                    assert!(matches!(env.request, Request::SearchPaths { pane: 2, .. }));
                    codec::write_message(
                        &mut peer,
                        &ServerMessage::Response {
                            id: env.id,
                            result: Ok(Response::PathMatches(thurm_proto::PathMatches {
                                cwd: "/remote".into(),
                                ..Default::default()
                            })),
                        },
                    )
                    .unwrap();
                }
                // A peer without support must receive Complete next, never SearchPaths.
                let frame = codec::read_frame(&mut peer).unwrap().unwrap();
                let (id, tag, pane): (u64, u32, u64) = codec::decode(&frame).unwrap();
                assert_eq!((tag, pane), (35, 2));
                assert_eq!(frame, codec::encode(&(id, tag, pane)).unwrap()[4..]);
                codec::write_message(
                    &mut peer,
                    &(0u32, id, Ok::<_, String>((11u32, "", Vec::<u8>::new()))),
                )
                .unwrap();
            });
            let client = Client::from_stream(stream, "test", false, &[], |_| {}, || {}).unwrap();
            let result = client.request_timeout(
                Request::SearchPaths {
                    pane: 2,
                    query: "".into(),
                },
                Some(Duration::from_secs(2)),
            );
            if supported {
                assert!(
                    matches!(result, Ok(Response::PathMatches(paths)) if paths.cwd == "/remote")
                );
            } else {
                assert!(
                    matches!(result, Err(ClientError::Daemon(e)) if e.contains("Update Thurm"))
                );
                assert!(
                    client
                        .send(Request::SearchPaths {
                            pane: 2,
                            query: "".into()
                        })
                        .is_err()
                );
            }
            assert!(matches!(
                client.request(Request::Complete { pane: 2 }),
                Ok(Response::Completions(_))
            ));
            server.join().unwrap();
        }
    }

    #[test]
    fn connect_with_says_the_extra_capabilities() {
        let dir = std::env::temp_dir().join(format!("thurm-client-caps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = stream.unwrap();
                let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut s) else {
                    continue;
                };
                let Request::Hello { capabilities, .. } = env.request else {
                    continue;
                };
                seen_tx.send(capabilities).unwrap();
                codec::write_message(
                    &mut s,
                    &ServerMessage::Response {
                        id: env.id,
                        result: Ok(Response::Hello {
                            version: PROTOCOL_VERSION,
                            daemon_pid: 1,
                            restored: false,
                            build: "test".into(),
                            capabilities: thurm_proto::CAPABILITIES
                                .iter()
                                .map(|c| (*c).to_owned())
                                .collect(),
                        }),
                    },
                )
                .unwrap();
            }
        });
        let opts = || ConnectOptions {
            socket: sock.clone(),
            spawn_daemon: None,
            client_name: "test",
            ui: false,
        };
        let _plain = Client::connect(opts(), |_| {}, || {}).unwrap();
        let plain = seen_rx.recv().unwrap();
        assert!(!plain.iter().any(|c| c == thurm_proto::CAP_DEFLATE));
        let _deflate =
            Client::connect_with(opts(), &[thurm_proto::CAP_DEFLATE], |_| {}, || {}).unwrap();
        let asked = seen_rx.recv().unwrap();
        assert!(asked.iter().any(|c| c == thurm_proto::CAP_DEFLATE));
        assert!(
            thurm_proto::CAPABILITIES
                .iter()
                .all(|c| asked.iter().any(|a| a == c))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A daemon that answers Hello, never answers ListPanes, and hangs up on Capture.
    #[test]
    fn timeouts_and_hangups_end_waiting_requests() {
        let dir = std::env::temp_dir().join(format!("thurm-client-to-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut r) {
                match env.request {
                    Request::Hello { .. } => codec::write_message(
                        &mut w,
                        &ServerMessage::Response {
                            id: env.id,
                            result: Ok(Response::Hello {
                                version: PROTOCOL_VERSION,
                                daemon_pid: 1,
                                restored: false,
                                build: String::new(),
                                capabilities: thurm_proto::CAPABILITIES
                                    .iter()
                                    .map(|c| (*c).to_owned())
                                    .collect(),
                            }),
                        },
                    )
                    .unwrap(),
                    Request::Capture { .. } => return,
                    _ => {}
                }
            }
        });
        let client = Client::connect(
            ConnectOptions {
                socket: sock,
                spawn_daemon: None,
                client_name: "test",
                ui: false,
            },
            |_| {},
            || {},
        )
        .unwrap();
        let r = client.request_timeout(Request::ListPanes, Some(Duration::from_millis(100)));
        assert!(matches!(r, Err(ClientError::Timeout)), "{r:?}");
        // The timed-out request is not kept around.
        assert!(client.pending.lock().is_empty());
        // A request waiting without a limit ends when the daemon goes away.
        let waiting = {
            let client = client.clone();
            std::thread::spawn(move || client.request(Request::ListPanes))
        };
        std::thread::sleep(Duration::from_millis(50));
        let _ = client.send(Request::Capture {
            pane: 1,
            opts: thurm_proto::CaptureOpts::default(),
        });
        let r = waiting.join().unwrap();
        assert!(matches!(r, Err(ClientError::Disconnected)), "{r:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A daemon that answers Hello and then stops reading: a request that can't even be
    /// written still ends at its deadline, and the connection is closed.
    #[test]
    fn timeouts_cover_writing() {
        let dir = std::env::temp_dir().join(format!("thurm-client-wr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            if let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut r) {
                codec::write_message(
                    &mut w,
                    &ServerMessage::Response {
                        id: env.id,
                        result: Ok(Response::Hello {
                            version: PROTOCOL_VERSION,
                            daemon_pid: 1,
                            restored: false,
                            build: String::new(),
                            capabilities: thurm_proto::CAPABILITIES
                                .iter()
                                .map(|c| (*c).to_owned())
                                .collect(),
                        }),
                    },
                )
                .unwrap();
            }
            // Never read again (but keep the socket open).
            let _ = stop_rx.recv();
        });
        let client = Client::connect(
            ConnectOptions {
                socket: sock,
                spawn_daemon: None,
                client_name: "test",
                ui: false,
            },
            |_| {},
            || {},
        )
        .unwrap();
        let started = Instant::now();
        let r = client.request_timeout(
            Request::Input {
                pane: 1,
                // Far more than the socket holds, small enough to encode well within the
                // deadline (in debug builds too): the write starts, then stalls.
                data: vec![b'x'; 4 * 1024 * 1024],
            },
            Some(Duration::from_millis(500)),
        );
        assert!(matches!(r, Err(ClientError::Timeout)), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!client.is_alive());
        let _ = stop_tx.send(());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Minimal fake daemon: answers Hello and echoes ListPanes, pushes one event.
    #[test]
    fn request_response_and_events() {
        let dir = std::env::temp_dir().join(format!("thurm-client-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s");
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut r) {
                let resp = match env.request {
                    Request::Hello { .. } => Response::Hello {
                        version: PROTOCOL_VERSION,
                        daemon_pid: 1,
                        restored: false,
                        build: String::new(),
                        capabilities: thurm_proto::CAPABILITIES
                            .iter()
                            .map(|c| (*c).to_owned())
                            .collect(),
                    },
                    Request::ListPanes => {
                        codec::write_message(
                            &mut w,
                            &ServerMessage::Event(Event::Bell { pane: 9 }),
                        )
                        .unwrap();
                        Response::Panes(vec![])
                    }
                    _ => Response::Ok,
                };
                if env.id != 0 {
                    codec::write_message(
                        &mut w,
                        &ServerMessage::Response {
                            id: env.id,
                            result: Ok(resp),
                        },
                    )
                    .unwrap();
                }
            }
        });
        let (etx, erx) = crossbeam_channel::unbounded();
        let client = Client::connect(
            ConnectOptions {
                socket: sock,
                spawn_daemon: None,
                client_name: "test",
                ui: false,
            },
            move |ev| {
                let _ = etx.send(ev);
            },
            || {},
        )
        .unwrap();
        assert!(
            matches!(client.request(Request::ListPanes).unwrap(), Response::Panes(v) if v.is_empty())
        );
        assert!(matches!(
            erx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Event::Bell { pane: 9 }
        ));
        client
            .send(Request::Focus {
                pane: 1,
                focused: true,
            })
            .unwrap();
        // A deadline that passed before anything was written fails only that request.
        for _ in 0..100 {
            let r = client.request_timeout(Request::ListPanes, Some(Duration::ZERO));
            assert!(matches!(r, Err(ClientError::Timeout) | Ok(_)), "{r:?}");
        }
        assert!(client.is_alive());
        assert!(matches!(
            client.request(Request::ListPanes).unwrap(),
            Response::Panes(_)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn find_daemon_in_bundled_and_flat_layouts() {
        let dir = std::env::temp_dir().join(format!("thurm-find-daemon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(find_daemon_in(&dir), None);

        let flat = dir.join("thurmd");
        std::fs::write(&flat, b"").unwrap();
        assert_eq!(find_daemon_in(&dir), Some(flat));

        let bundled = dir.join(BUNDLED_DAEMON);
        std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        std::fs::write(&bundled, b"").unwrap();
        assert_eq!(find_daemon_in(&dir), Some(bundled));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ps_parser_handles_spaced_helper_bundle_path() {
        let sock = "/tmp/thurm-501/thurmd.sock";
        assert_eq!(
            parse_ps_daemon_pid(
                "42 501 thurmd           /Applications/Thurm.app/Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd --foreground --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            Some(42)
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "42 501 thurmd           /Applications/Thurm - Beta.app/Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            Some(42)
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "42 501 thurmd           /Applications/Thurm -Beta.app/Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd --daemonize --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            Some(42)
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "42 501 thurmd           /Applications/Thurm --beta 2.app/Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            Some(42)
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "42 501 thurmd           /Applications/Thurm -d 2.app/Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            Some(42)
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "42 501 thurmd           /Users/me/thurmd - test/Thurm Daemon.app/Contents/MacOS/thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            Some(42)
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "43 501 thurmd           /usr/local/bin/thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            Some(43)
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "43 501 thurmd           thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            Some(43)
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "43 501 thurmd           /Applications/Thurm - Beta.app/Contents/Helpers/Thurm Daemon.app/Contents/MacOS/thurmd --socket /tmp/my dir/thurmd.sock --foreground",
                501,
                "/tmp/my dir/thurmd.sock",
                99
            ),
            Some(43)
        );
        // Non-daemon wrappers and helpers must be rejected.
        assert_eq!(
            parse_ps_daemon_pid(
                "44 501 sh               /bin/sh -c /x/thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            None
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "44 501 sh               /bin/sh -c x/thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            None
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "45 501 thurm-tailnet    /Applications/Thurm.app/Contents/Helpers/thurm-tailnet --control-fd 3 --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            None
        );
        // A spaced ucomm whose first word is `thurmd` must not pass the cmdline check.
        assert_eq!(
            parse_ps_daemon_pid(
                "46 501 thurmd helper    /Applications/thurmd helper --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            None
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "47 501 thurmd           /usr/local/bin/thurmd --socket /tmp/thurm-501/thurmd.sock.other",
                501,
                sock,
                99
            ),
            None
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "42 502 thurmd           /usr/local/bin/thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            None
        );
        assert_eq!(
            parse_ps_daemon_pid(
                "99 501 thurmd           /usr/local/bin/thurmd --socket /tmp/thurm-501/thurmd.sock",
                501,
                sock,
                99
            ),
            None
        );
        // Cmdline check also rejects shell/wrapper invocations on its own.
        assert!(!daemon_cmdline_has_socket(
            "/bin/sh -c /x/thurmd --socket /tmp/thurm-501/thurmd.sock",
            sock
        ));
        assert!(!daemon_cmdline_has_socket(
            "/bin/sh -c ./thurmd --socket /tmp/thurm-501/thurmd.sock",
            sock
        ));
        assert!(!daemon_cmdline_has_socket(
            "sudo /usr/local/bin/thurmd --socket /tmp/thurm-501/thurmd.sock",
            sock
        ));
    }

    #[test]
    fn send_and_request_preserve_fifo_order() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let (seen_tx, seen_rx) = crossbeam_channel::unbounded();
        let server = std::thread::spawn(move || {
            let mut r = BufReader::new(peer.try_clone().unwrap());
            while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut r) {
                match env.request {
                    Request::Hello { .. } => {
                        codec::write_message(
                            &mut peer,
                            &ServerMessage::Response {
                                id: env.id,
                                result: Ok(Response::Hello {
                                    version: PROTOCOL_VERSION,
                                    daemon_pid: 1,
                                    restored: false,
                                    build: "test".into(),
                                    capabilities: thurm_proto::CAPABILITIES
                                        .iter()
                                        .map(|c| (*c).to_owned())
                                        .collect(),
                                }),
                            },
                        )
                        .unwrap();
                    }
                    Request::Focus { pane, focused } => {
                        seen_tx.send((0u8, pane, focused)).unwrap();
                    }
                    Request::ListPanes => {
                        seen_tx.send((1u8, env.id, false)).unwrap();
                        codec::write_message(
                            &mut peer,
                            &ServerMessage::Response {
                                id: env.id,
                                result: Ok(Response::Panes(vec![])),
                            },
                        )
                        .unwrap();
                    }
                    _ => {}
                }
            }
        });
        let client = Client::from_stream(stream, "test", false, &[], |_| {}, || {}).unwrap();
        let mut expected = Vec::new();
        for i in 1..=20u64 {
            let focused = i % 2 == 0;
            client.send(Request::Focus { pane: i, focused }).unwrap();
            expected.push((0u8, i, focused));
            if i % 4 == 0 {
                let resp = client.request(Request::ListPanes).unwrap();
                assert!(matches!(resp, Response::Panes(_)));
                expected.push((1u8, 0, false));
            }
        }
        // Final request synchronizes all preceding fire-and-forget sends.
        assert!(matches!(
            client.request(Request::ListPanes).unwrap(),
            Response::Panes(_)
        ));
        expected.push((1u8, 0, false));
        drop(client);
        server.join().unwrap();

        let got: Vec<(u8, u64, bool)> = seen_rx
            .try_iter()
            .map(|(kind, v, f)| {
                if kind == 1 {
                    (1, 0, false)
                } else {
                    (kind, v, f)
                }
            })
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn send_does_not_block_when_socket_stalls() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let server = std::thread::spawn(move || {
            let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            codec::write_message(
                &mut peer,
                &ServerMessage::Response {
                    id: hello.id,
                    result: Ok(Response::Hello {
                        version: PROTOCOL_VERSION,
                        daemon_pid: 1,
                        restored: false,
                        build: "test".into(),
                        capabilities: thurm_proto::CAPABILITIES
                            .iter()
                            .map(|c| (*c).to_owned())
                            .collect(),
                    }),
                },
            )
            .unwrap();
            // Stop reading so kernel socket buffers fill completely.
            let _ = stop_rx.recv();
        });
        let client = Client::from_stream(stream, "test", false, &[], |_| {}, || {}).unwrap();

        // Enqueue multiple multi-megabyte frames (far larger than the Unix socket buffer)
        // plus follow-up hot-path sends: none may block the calling thread.
        let started = Instant::now();
        for _ in 0..4 {
            client
                .send(Request::Input {
                    pane: 1,
                    data: vec![b'a'; 2 * 1024 * 1024],
                })
                .unwrap();
        }
        for i in 1..=20 {
            client
                .send(Request::Focus {
                    pane: i,
                    focused: true,
                })
                .unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "send() stalled on a full socket: {:?}",
            started.elapsed()
        );

        // A timed request queued behind the stalled send still returns Timeout on schedule.
        let req_started = Instant::now();
        let res = client.request_timeout(Request::ListPanes, Some(Duration::from_millis(150)));
        assert!(matches!(res, Err(ClientError::Timeout)), "{res:?}");
        assert!(
            req_started.elapsed() < Duration::from_secs(2),
            "request_timeout hung behind stalled send: {:?}",
            req_started.elapsed()
        );

        // Dropping the client shuts down the socket without blocking on the stalled writer.
        let drop_started = Instant::now();
        drop(client);
        assert!(
            drop_started.elapsed() < Duration::from_secs(1),
            "Drop blocked on stalled writer: {:?}",
            drop_started.elapsed()
        );
        let _ = stop_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn send_followed_by_immediate_drop_delivers_frames() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let (rx_tx, rx_rx) = crossbeam_channel::unbounded();
        let server = std::thread::spawn(move || {
            let mut r = BufReader::new(peer.try_clone().unwrap());
            while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut r) {
                match env.request {
                    Request::Hello { .. } => {
                        codec::write_message(
                            &mut peer,
                            &ServerMessage::Response {
                                id: env.id,
                                result: Ok(Response::Hello {
                                    version: PROTOCOL_VERSION,
                                    daemon_pid: 1,
                                    restored: false,
                                    build: "test".into(),
                                    capabilities: thurm_proto::CAPABILITIES
                                        .iter()
                                        .map(|c| (*c).to_owned())
                                        .collect(),
                                }),
                            },
                        )
                        .unwrap();
                    }
                    Request::Input { pane, data } => {
                        rx_tx.send((pane, data)).unwrap();
                    }
                    _ => {}
                }
            }
        });

        let client = Client::from_stream(stream, "test", false, &[], |_| {}, || {}).unwrap();
        for i in 1..=8u64 {
            client
                .send(Request::Input {
                    pane: i,
                    data: format!("payload-{i}").into_bytes(),
                })
                .unwrap();
        }
        // Immediately drop the last Client handle without waiting on a request/response:
        // graceful Drop drains all accepted frames before shutting down the socket.
        drop(client);
        server.join().unwrap();

        let received: Vec<(u64, Vec<u8>)> = rx_rx.try_iter().collect();
        let expected: Vec<(u64, Vec<u8>)> = (1..=8u64)
            .map(|i| (i, format!("payload-{i}").into_bytes()))
            .collect();
        assert_eq!(received, expected);
    }

    #[test]
    fn send_enforces_outgoing_byte_budget_on_stalled_peer() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let (resume_tx, resume_rx) = crossbeam_channel::bounded::<()>(1);
        let (count_tx, count_rx) = crossbeam_channel::bounded::<usize>(1);
        let server = std::thread::spawn(move || {
            let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            codec::write_message(
                &mut peer,
                &ServerMessage::Response {
                    id: hello.id,
                    result: Ok(Response::Hello {
                        version: PROTOCOL_VERSION,
                        daemon_pid: 1,
                        restored: false,
                        build: "test".into(),
                        capabilities: thurm_proto::CAPABILITIES
                            .iter()
                            .map(|c| (*c).to_owned())
                            .collect(),
                    }),
                },
            )
            .unwrap();
            // Stall reading until the client hits its outgoing byte budget.
            let _ = resume_rx.recv();
            let mut r = BufReader::new(peer);
            let mut received = 0usize;
            while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut r) {
                if matches!(env.request, Request::Input { .. }) {
                    received += 1;
                }
            }
            let _ = count_tx.send(received);
        });

        let client = Client::from_stream(stream, "test", false, &[], |_| {}, || {}).unwrap();
        let cap = 2 * 1024 * 1024;
        client.set_max_queued_bytes(cap);

        // First frame (2 MiB) fills the Unix socket buffer and blocks the writer thread while
        // saturating the 2 MiB queue budget; subsequent sends must immediately fail with QueueFull
        // without blocking or growing queued memory past the budget.
        let started = Instant::now();
        client
            .send(Request::Input {
                pane: 1,
                data: vec![b'x'; cap - 1024],
            })
            .unwrap();
        let mut accepted = 1usize;
        let mut rejected = 0usize;
        let chunk = vec![b'y'; 256 * 1024];
        for _ in 0..8 {
            match client.send(Request::Input {
                pane: 1,
                data: chunk.clone(),
            }) {
                Ok(()) => accepted += 1,
                Err(ClientError::QueueFull) => rejected += 1,
                Err(other) => panic!("unexpected error while queue full: {other:?}"),
            }
            assert!(
                client.queued_write_bytes() <= cap,
                "queued bytes {} exceeded cap {}",
                client.queued_write_bytes(),
                cap
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "bounded send() blocked: {:?}",
            started.elapsed()
        );
        assert!(
            rejected >= 1,
            "excess frames past byte budget must be rejected with QueueFull"
        );

        // Once the peer resumes reading, the queued frames drain, freeing the budget.
        let _ = resume_tx.send(());
        client.flush(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(client.queued_write_bytes(), 0);
        client
            .send(Request::Input {
                pane: 1,
                data: b"after-drain".to_vec(),
            })
            .unwrap();
        accepted += 1;
        drop(client);
        server.join().unwrap();
        assert_eq!(count_rx.recv().unwrap(), accepted);
    }

    #[test]
    fn force_close_unblocks_stalled_writer_immediately() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let server = std::thread::spawn(move || {
            let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            codec::write_message(
                &mut peer,
                &ServerMessage::Response {
                    id: hello.id,
                    result: Ok(Response::Hello {
                        version: PROTOCOL_VERSION,
                        daemon_pid: 1,
                        restored: false,
                        build: "test".into(),
                        capabilities: thurm_proto::CAPABILITIES
                            .iter()
                            .map(|c| (*c).to_owned())
                            .collect(),
                    }),
                },
            )
            .unwrap();
            let _ = stop_rx.recv();
        });

        let client = Client::from_stream(stream, "test", false, &[], |_| {}, || {}).unwrap();
        for _ in 0..4 {
            let _ = client.send(Request::Input {
                pane: 1,
                data: vec![b'z'; 2 * 1024 * 1024],
            });
        }
        let started = Instant::now();
        client.force_close();
        drop(client);
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "force_close() + drop waited unexpectedly: {:?}",
            started.elapsed()
        );
        let _ = stop_tx.send(());
        server.join().unwrap();
    }
}
