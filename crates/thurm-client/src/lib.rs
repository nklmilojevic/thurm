//! Blocking client for `thurmd`.
//!
//! One reader thread demultiplexes responses (matched by request id) and events (delivered to
//! a callback). Writes are serialized through a mutex.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};
use parking_lot::Mutex;

pub use thurm_proto as proto;
use thurm_proto::{Envelope, Event, PROTOCOL_VERSION, Request, Response, ServerMessage, codec};

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Response, String>>>>>;

pub struct Client {
    writer: Mutex<BufWriter<UnixStream>>,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    pub hello: Mutex<Option<Response>>,
}

#[derive(Debug)]
pub enum ClientError {
    Connect(String),
    Disconnected,
    Timeout,
    Daemon(String),
    Io(std::io::Error),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Connect(e) => write!(f, "cannot connect to thurmd: {e}"),
            ClientError::Disconnected => write!(f, "connection to thurmd lost"),
            ClientError::Timeout => write!(f, "thurmd did not answer in time"),
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
    #[cfg(target_os = "macos")]
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
    #[cfg(not(target_os = "macos"))]
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
    let listed = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,uid=,args="])
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|line| {
                    let mut f = line.split_whitespace();
                    let pid: u32 = f.next()?.parse().ok()?;
                    let owner: u32 = f.next()?.parse().ok()?;
                    let args: Vec<&str> = f.collect();
                    let is_daemon = args
                        .first()
                        .is_some_and(|a| a.ends_with("/thurmd") || *a == "thurmd");
                    let same_socket = args.windows(2).any(|w| w[0] == "--socket" && w[1] == sock);
                    (owner == uid && is_daemon && same_socket && pid != std::process::id())
                        .then_some(pid)
                })
        });
    listed.or_else(|| daemon_pid(socket).filter(|&p| alive(p)))
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

/// Locate `thurmd`: next to the current executable, then on PATH.
pub fn find_daemon() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let p = dir.join("thurmd");
        if p.is_file() {
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
        let stream = connect_socket(&opts)?;
        let read_half = stream.try_clone()?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let client = Arc::new(Client {
            writer: Mutex::new(BufWriter::new(stream)),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            alive: alive.clone(),
            hello: Mutex::new(None),
        });

        std::thread::Builder::new()
            .name("thurm-client-reader".into())
            .spawn(move || {
                // However this thread ends (a panic in `on_event` too), requests still waiting
                // get an error instead of waiting forever.
                struct Finish(Arc<AtomicBool>, Pending);
                impl Drop for Finish {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Relaxed);
                        for (_, tx) in self.1.lock().drain() {
                            let _ = tx.send(Err("disconnected".into()));
                        }
                    }
                }
                let finish = Finish(alive.clone(), pending.clone());
                let mut reader = BufReader::with_capacity(256 * 1024, read_half);
                while let Ok(Some(frame)) = codec::read_frame(&mut reader) {
                    // A message of a newer daemon this client can't decode is skipped (the frames
                    // after it are intact); a response still ends its request, with an error.
                    let Ok(msg) = codec::decode::<ServerMessage>(&frame) else {
                        if let Some(id) = codec::response_id(&frame)
                            && let Some(tx) = pending.lock().remove(&id)
                        {
                            let _ = tx.send(Err("the daemon's answer could not be read".into()));
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

        let hello = client.request(Request::Hello {
            client: opts.client_name.to_owned(),
            version: PROTOCOL_VERSION,
            ui: opts.ui,
            capabilities: thurm_proto::CAPABILITIES
                .iter()
                .map(|c| (*c).to_owned())
                .collect(),
        })?;
        *client.hello.lock() = Some(hello);
        Ok(client)
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn write(&self, env: &Envelope) -> Result<(), ClientError> {
        self.write_by(env, None)
    }

    /// Writes `env`, giving up at `deadline` (waiting for the writer lock included). A write
    /// cut off mid-message would leave the stream out of step, so a timeout closes the
    /// connection.
    fn write_by(&self, env: &Envelope, deadline: Option<Instant>) -> Result<(), ClientError> {
        if !self.is_alive() {
            return Err(ClientError::Disconnected);
        }
        let buf = codec::encode(env)?;
        let mut w = match deadline {
            Some(d) => self.writer.try_lock_until(d).ok_or(ClientError::Timeout)?,
            None => self.writer.lock(),
        };
        let written = match deadline {
            None => w.write_all(&buf).and_then(|()| w.flush()),
            // Write by hand, giving each call only the time left: a peer that reads a little
            // at a time must not stretch the request past its deadline.
            Some(d) => (|| {
                // Every write flushes, so leftovers mean an earlier one failed partway: the
                // stream is out of step already.
                if !w.buffer().is_empty() {
                    return Err(std::io::ErrorKind::BrokenPipe.into());
                }
                let stream = w.get_mut();
                let mut rest = &buf[..];
                while !rest.is_empty() {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(std::io::ErrorKind::TimedOut.into());
                    }
                    stream.set_write_timeout(Some(left))?;
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
            let _ = w.get_ref().set_write_timeout(None);
        }
        match written {
            Ok(()) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                self.alive.store(false, Ordering::Relaxed);
                let _ = w.get_ref().shutdown(std::net::Shutdown::Both);
                Err(ClientError::Timeout)
            }
            // Out of step (see above): nothing more can be sent on it.
            Err(e) if deadline.is_some() && e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.alive.store(false, Ordering::Relaxed);
                let _ = w.get_ref().shutdown(std::net::Shutdown::Both);
                Err(ClientError::Disconnected)
            }
            Err(e) => Err(e.into()),
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

    /// Fire and forget (id 0: the daemon sends no response).
    pub fn send(&self, request: Request) -> Result<(), ClientError> {
        self.write(&Envelope { id: 0, request })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Closing both directions ends the reader thread and tells the daemon we're gone.
        let _ = self
            .writer
            .lock()
            .get_ref()
            .shutdown(std::net::Shutdown::Both);
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
                                version: 1,
                                daemon_pid: 1,
                                restored: false,
                                build: String::new(),
                                capabilities: Vec::new(),
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
                            version: 1,
                            daemon_pid: 1,
                            restored: false,
                            build: String::new(),
                            capabilities: Vec::new(),
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
                data: vec![b'x'; 32 * 1024 * 1024],
            },
            Some(Duration::from_millis(300)),
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
                        version: 1,
                        daemon_pid: 1,
                        restored: false,
                        build: String::new(),
                        capabilities: Vec::new(),
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
        let _ = std::fs::remove_dir_all(dir);
    }
}
