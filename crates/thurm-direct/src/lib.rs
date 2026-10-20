//! A host without Thurm, for the iOS app: a small daemon inside the app that speaks the daemon
//! protocol over one end of a socket pair (the other goes to
//! `thurm_client::Client::from_stream`), so the app shows its shells like any host's panes.
//!
//! Each pane is a shell on the host: on an ssh channel ([`thurm_ssh::Shell`]), or in a mosh
//! session ([`thurm_mosh::MoshSession`]) started over ssh, which survives the phone changing
//! networks and the app being suspended. Like `thurmd`, it keeps every pane's terminal: that
//! answers the host's terminal queries (ssh), is what a subscribing app replays (`Attach`), and
//! gives the screens a mosh diff starts from.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use thurm_mosh::bootstrap::{self, BootstrapError};
use thurm_mosh::{MoshEvent, MoshSession, Status};
use thurm_proto::{
    CAP_ACTIVE_SIZE, CreatePane, Envelope, Event, LAYOUT_NO_WINDOW, PaneId, PaneInfo, PaneSize,
    ProgramMatch, Request, Response, ServerMessage, codec,
};
use thurm_ssh::{DeviceKey, Session, SessionError, Shell, ShellEvent, Target};
use thurm_term::{EngineConfig, TermEvent, Terminal};

/// A host as the app saved it.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct DirectTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Trusted host keys, as `SHA256:<base64>` fingerprints; empty until the user trusts one.
    #[serde(default)]
    pub host_keys: Vec<String>,
    #[serde(default)]
    pub mode: Mode,
    /// `mosh-server`'s path on the host, when the login shell's PATH misses it.
    #[serde(default)]
    pub mosh_server: Option<String>,
    /// The UDP port or range (`60000:61000`) for mosh, when a firewall needs it.
    #[serde(default)]
    pub mosh_ports: Option<String>,
}

/// How new shells reach the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// mosh, or ssh when mosh can't start there.
    #[default]
    Mosh,
    MoshOnly,
    Ssh,
}

/// Why connecting failed.
#[derive(Debug)]
pub enum DirectError {
    /// The host showed this key and none is trusted yet: ask the user.
    UnknownHostKey(String),
    /// The host showed this key, not a trusted one.
    HostKeyChanged(String),
    /// The host does not accept the device's key.
    KeyRejected,
    Failed(String),
}

impl std::fmt::Display for DirectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirectError::UnknownHostKey(k) => write!(f, "the host's key {k} is not trusted yet"),
            DirectError::HostKeyChanged(k) => {
                write!(
                    f,
                    "the host's key changed to {k}; it may have been reinstalled"
                )
            }
            DirectError::KeyRejected => f.write_str(
                "the host did not accept this device's key; add it to ~/.ssh/authorized_keys there",
            ),
            DirectError::Failed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DirectError {}

impl From<SessionError> for DirectError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::UnknownHostKey(k) => DirectError::UnknownHostKey(k),
            SessionError::HostKeyChanged(k) => DirectError::HostKeyChanged(k),
            SessionError::KeyRejected => DirectError::KeyRejected,
            SessionError::Ssh(e) => DirectError::Failed(e.to_string()),
        }
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const EXEC_TIMEOUT: Duration = Duration::from_secs(20);
/// How long ending the server waits for shells still starting (an ssh connection and a mosh
/// bootstrap, each with its own timeout).
const CREATING_TIMEOUT: Duration = Duration::from_secs(60);
/// Tests widen the moment between a shell starting and its pane having it (its first output
/// can come in it).
#[cfg(test)]
static SSH_START_DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// How long a new mosh session has to hear from its server over UDP (several startup probes)
/// before the pane gives up on mosh: a firewall dropping UDP looks like nothing at all.
const MOSH_ANSWER: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(10)
};

/// A served host. Dropping it ends nothing by itself: the server ends when the app closes its
/// end of the socket, and ends the host's shells with it.
pub struct Direct {
    inner: Arc<Inner>,
}

impl Direct {
    /// Every mosh session moves to a new socket at once (the network changed, or the app came
    /// back from the background).
    pub fn wake(&self) {
        for pane in self.inner.panes.lock().values() {
            if let Backend::Mosh { session, .. } = &pane.lock().backend {
                session.wake();
            }
        }
    }
}

/// Connects to `target` with the device key and serves it; returns the app's end of the socket.
pub fn serve(
    target: DirectTarget,
    key: Arc<dyn DeviceKey>,
) -> Result<(UnixStream, Direct), DirectError> {
    let (session, ip) = connect(&target, &key)?;
    let (ours, theirs) =
        UnixStream::pair().map_err(|e| DirectError::Failed(format!("socket pair: {e}")))?;
    let reader = theirs
        .try_clone()
        .map_err(|e| DirectError::Failed(format!("socket pair: {e}")))?;
    let mosh_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("thurm-mosh")
        .enable_all()
        .build()
        .map_err(|e| DirectError::Failed(format!("mosh runtime: {e}")))?;
    let inner = Arc::new(Inner {
        target,
        key,
        ip,
        session: Mutex::new(Some(Arc::new(session))),
        panes: Mutex::new(HashMap::new()),
        next_pane: AtomicU64::new(1),
        closed: AtomicBool::new(false),
        creating: Mutex::new(0),
        created: Condvar::new(),
        out: Mutex::new(theirs),
        mosh_rt: Some(mosh_rt),
    });
    let server = inner.clone();
    std::thread::Builder::new()
        .name("thurm-direct".into())
        .spawn(move || server.run(reader))
        .map_err(|e| DirectError::Failed(format!("server thread: {e}")))?;
    Ok((ours, Direct { inner }))
}

/// Connects over ssh to the first of the host's addresses that answers. The address is kept:
/// mosh-server listens on the address the ssh connection reached (`-s`), so mosh goes there.
fn connect(
    target: &DirectTarget,
    key: &Arc<dyn DeviceKey>,
) -> Result<(Session, IpAddr), DirectError> {
    let addrs: Vec<SocketAddr> = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .map_err(|e| DirectError::Failed(format!("{}: {e}", target.host)))?
        .collect();
    let mut last = DirectError::Failed(format!("{}: no address", target.host));
    for addr in addrs {
        let t = Target {
            host: addr.ip().to_string(),
            port: target.port,
            user: target.user.clone(),
            host_keys: target.host_keys.clone(),
            socket: None,
        };
        match Session::connect_checked(&t, key.clone(), CONNECT_TIMEOUT) {
            Ok(s) => return Ok((s, addr.ip())),
            Err(SessionError::Ssh(e)) => last = DirectError::Failed(e.to_string()),
            Err(e) => return Err(e.into()),
        }
    }
    Err(last)
}

enum Backend {
    /// Being set up: input waits for nothing (dropped).
    Starting,
    Ssh(Shell),
    Mosh {
        session: MoshSession,
        pid: Option<u32>,
        /// The host state shown.
        current: u64,
        /// Acknowledged host states the server may diff from, as terminal state.
        snapshots: BTreeMap<u64, Vec<u8>>,
        status: Status,
    },
    /// The shell ended or the connection went away.
    Ended,
}

impl Backend {
    fn write(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        match self {
            Backend::Ssh(shell) => shell.write(data),
            Backend::Mosh { session, .. } => session.send_keys(data),
            Backend::Starting | Backend::Ended => {}
        }
    }

    fn resize(&self, size: PaneSize) {
        match self {
            Backend::Ssh(shell) => shell.resize(size.cols, size.rows),
            Backend::Mosh { session, .. } => session.resize(size.cols, size.rows),
            Backend::Starting | Backend::Ended => {}
        }
    }

    fn program(&self) -> Option<ProgramMatch> {
        let (id, name, icon) = match self {
            Backend::Ssh(_) => ("ssh", "SSH".to_owned(), "terminal"),
            Backend::Mosh { status, .. } => (
                "mosh",
                match status {
                    Status::Connecting => "mosh (connecting)",
                    Status::Connected => "mosh",
                    Status::Reconnecting => "mosh (reconnecting)",
                    Status::Stalled => "mosh (no contact)",
                }
                .to_owned(),
                "antenna.radiowaves.left.and.right",
            ),
            Backend::Starting | Backend::Ended => return None,
        };
        Some(ProgramMatch {
            id: id.into(),
            name,
            icon: icon.into(),
            color: None,
            ink: None,
        })
    }
}

struct Pane {
    id: PaneId,
    term: Terminal,
    backend: Backend,
    subscribed: bool,
    alive: bool,
    exit_code: Option<i32>,
    /// Terminal replies to an ssh shell's first output, which can come before the shell is
    /// in `backend`: sent once it is.
    held_replies: Vec<u8>,
}

impl Pane {
    fn info(&self, fallback_title: &str) -> PaneInfo {
        PaneInfo {
            id: self.id,
            title: self
                .term
                .title()
                .filter(|t| !t.is_empty())
                .unwrap_or(fallback_title)
                .to_owned(),
            alive: self.alive,
            exit_code: self.exit_code,
            size: self.term.size(),
            program: self.backend.program(),
            ..Default::default()
        }
    }
}

/// Ends what a pane runs; returns its mosh-server's pid, to end on the host.
fn end_backend(backend: Backend) -> Option<u32> {
    match backend {
        Backend::Ssh(shell) => {
            shell.close();
            None
        }
        Backend::Mosh { session, pid, .. } => {
            session.close();
            pid
        }
        Backend::Starting | Backend::Ended => None,
    }
}

/// Ends mosh-servers on the host over `session`, when it is still connected.
fn kill(session: &Session, pids: &[u32]) {
    if pids.is_empty() || session.is_closed() {
        return;
    }
    let list: Vec<String> = pids.iter().map(u32::to_string).collect();
    let _ = session.exec(&format!("kill {}", list.join(" ")), EXEC_TIMEOUT);
}

fn new_terminal(size: PaneSize) -> Terminal {
    let mut term = Terminal::new(
        size,
        EngineConfig::from_config(&thurm_config::Config::default(), true),
    );
    // A host must not get the phone's files read into its terminal (kitty graphics by path).
    term.set_reads_media(false);
    term
}

struct Inner {
    target: DirectTarget,
    key: Arc<dyn DeviceKey>,
    ip: IpAddr,
    session: Mutex<Option<Arc<Session>>>,
    panes: Mutex<HashMap<PaneId, Arc<Mutex<Pane>>>>,
    next_pane: AtomicU64,
    /// The app closed its end: shells that finish starting now end at once.
    closed: AtomicBool,
    /// Shells being started (`CreatePane` workers), and their end, signalled.
    creating: Mutex<usize>,
    created: Condvar,
    out: Mutex<UnixStream>,
    /// Always set; taken on drop.
    mosh_rt: Option<tokio::runtime::Runtime>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last reference may be a mosh callback's, on the runtime itself: dropping it
        // there would block (and panic).
        if let Some(rt) = self.mosh_rt.take() {
            rt.shutdown_background();
        }
    }
}

impl Inner {
    fn title(&self) -> String {
        format!("{}@{}", self.target.user, self.target.host)
    }

    fn send(&self, msg: &ServerMessage) {
        match codec::encode(msg) {
            Ok(bytes) => {
                let _ = self.out.lock().write_all(&bytes);
            }
            Err(e) => log::warn!("direct: cannot encode: {e}"),
        }
    }

    fn event(&self, ev: Event) {
        self.send(&ServerMessage::Event(ev));
    }

    fn pane(&self, id: PaneId) -> Option<Arc<Mutex<Pane>>> {
        self.panes.lock().get(&id).cloned()
    }

    /// The ssh session, connecting again when it has gone away (the app was suspended).
    fn session(&self) -> Result<Arc<Session>, String> {
        let mut s = self.session.lock();
        if let Some(session) = s.as_ref().filter(|s| !s.is_closed()) {
            return Ok(session.clone());
        }
        let t = Target {
            host: self.ip.to_string(),
            port: self.target.port,
            user: self.target.user.clone(),
            host_keys: self.target.host_keys.clone(),
            socket: None,
        };
        let session = Arc::new(
            Session::connect_checked(&t, self.key.clone(), CONNECT_TIMEOUT)
                .map_err(|e| e.to_string())?,
        );
        *s = Some(session.clone());
        Ok(session)
    }

    fn run(self: Arc<Self>, mut reader: UnixStream) {
        while let Ok(Some(frame)) = codec::read_frame(&mut reader) {
            let env: Envelope = match codec::decode(&frame) {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("direct: bad request: {e}");
                    continue;
                }
            };
            if let Request::CreatePane(req) = env.request {
                // Starting mosh takes a few round trips: other requests go on meanwhile.
                *self.creating.lock() += 1;
                let server = self.clone();
                std::thread::spawn(move || {
                    let result = server.create(req);
                    server.send(&ServerMessage::Response { id: env.id, result });
                    *server.creating.lock() -= 1;
                    server.created.notify_all();
                });
                continue;
            }
            let result = self.handle(env.request);
            self.send(&ServerMessage::Response { id: env.id, result });
        }
        self.shutdown();
    }

    /// The app is gone, and with it the only holder of the mosh keys: end the host's shells,
    /// those still starting included (they end themselves once started, seeing `closed`).
    fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        {
            let deadline = Instant::now() + CREATING_TIMEOUT;
            let mut creating = self.creating.lock();
            while *creating > 0 {
                if self.created.wait_until(&mut creating, deadline).timed_out() {
                    break;
                }
            }
        }
        let panes: Vec<_> = self.panes.lock().drain().map(|(_, p)| p).collect();
        let pids: Vec<u32> = panes
            .iter()
            .filter_map(|p| end_backend(std::mem::replace(&mut p.lock().backend, Backend::Ended)))
            .collect();
        drop(panes);
        if let Some(session) = self.session.lock().take() {
            kill(&session, &pids);
            session.disconnect();
        }
    }

    fn handle(self: &Arc<Self>, req: Request) -> Result<Response, String> {
        let pane_of = |id: PaneId| self.pane(id).ok_or_else(|| format!("no pane {id}"));
        match req {
            Request::Hello { .. } => Ok(Response::Hello {
                version: thurm_proto::PROTOCOL_VERSION,
                daemon_pid: std::process::id(),
                restored: false,
                build: thurm_proto::BUILD.into(),
                capabilities: vec!["hello-capabilities".into(), CAP_ACTIVE_SIZE.into()],
            }),
            Request::Ping => Ok(Response::Ok),
            Request::ListPanes => {
                let title = self.title();
                let mut panes: Vec<PaneInfo> = self
                    .panes
                    .lock()
                    .values()
                    .map(|p| p.lock().info(&title))
                    .collect();
                panes.sort_by_key(|p| p.id);
                Ok(Response::Panes(panes))
            }
            Request::PaneInfo { pane } => Ok(Response::PaneInfo(
                pane_of(pane)?.lock().info(&self.title()),
            )),
            Request::ClosePane { pane } => {
                self.close(pane);
                Ok(Response::Ok)
            }
            Request::Subscribe { pane } => {
                let pane = pane_of(pane)?;
                let mut p = pane.lock();
                p.subscribed = true;
                let state = p.term.serialize_state();
                self.event(Event::Attach {
                    pane: p.id,
                    size: p.term.size(),
                    state,
                });
                Ok(Response::Ok)
            }
            Request::Unsubscribe { pane } => {
                if let Some(p) = self.pane(pane) {
                    p.lock().subscribed = false;
                }
                Ok(Response::Ok)
            }
            Request::Input { pane, data } => {
                pane_of(pane)?.lock().backend.write(&data);
                Ok(Response::Ok)
            }
            Request::Paste { pane, text } => {
                let pane = pane_of(pane)?;
                let mut p = pane.lock();
                let bytes = p.term.paste(&text);
                p.backend.write(&bytes);
                Ok(Response::Ok)
            }
            Request::Key { pane, key } => {
                let pane = pane_of(pane)?;
                let mut p = pane.lock();
                let bytes = p.term.key(&key);
                p.backend.write(&bytes);
                Ok(Response::Ok)
            }
            Request::Mouse { pane, event } => {
                let pane = pane_of(pane)?;
                let mut p = pane.lock();
                let (_, bytes) = p.term.mouse(&event);
                p.backend.write(&bytes);
                Ok(Response::Ok)
            }
            Request::Wheel {
                pane,
                lines,
                col,
                row,
                mods,
            } => {
                let pane = pane_of(pane)?;
                let mut p = pane.lock();
                let bytes = p.term.wheel(lines, col, row, mods);
                p.backend.write(&bytes);
                Ok(Response::Ok)
            }
            Request::Focus { pane, focused } => {
                let pane = pane_of(pane)?;
                let mut p = pane.lock();
                if let Some(bytes) = p.term.focus(focused) {
                    p.backend.write(&bytes);
                }
                Ok(Response::Ok)
            }
            Request::Resize { pane, size } => {
                let pane = pane_of(pane)?;
                let mut p = pane.lock();
                let now = p.term.size();
                if (now.cols, now.rows) != (size.cols, size.rows) {
                    p.term.resize(size);
                    p.backend.resize(size);
                    if p.subscribed {
                        self.event(Event::Resized { pane: p.id, size });
                    }
                }
                Ok(Response::Ok)
            }
            Request::ClipboardReply { pane, text } => {
                let pane = pane_of(pane)?;
                let mut p = pane.lock();
                p.term.clipboard_reply(&text);
                self.drain(&mut p);
                Ok(Response::Ok)
            }
            Request::GetLayout => Ok(Response::Layout(None)),
            // No window here: new panes stay loose panes.
            Request::CheckUi | Request::Ui(_) => Err(LAYOUT_NO_WINDOW.into()),
            // Notices come from a host's daemon; this host has none.
            Request::RegisterPushDevice { .. }
            | Request::UnregisterPushDevice { .. }
            | Request::FollowAgent { .. }
            | Request::UnfollowAgent { .. } => Ok(Response::Ok),
            _ => Err("not available on a host without Thurm".into()),
        }
    }

    fn close(&self, id: PaneId) {
        let Some(pane) = self.panes.lock().remove(&id) else {
            return;
        };
        // Still starting: its worker sees the pane gone and ends what it started.
        let backend = std::mem::replace(&mut pane.lock().backend, Backend::Ended);
        if let Some(pid) = end_backend(backend) {
            // Over the session there is now: reconnecting would hold up this request (the
            // server exits by itself after its timeout).
            if let Some(s) = self.session.lock().clone().filter(|s| !s.is_closed()) {
                std::thread::spawn(move || kill(&s, &[pid]));
            }
        }
        self.event(Event::PaneClosed { pane: id });
    }

    fn create(self: &Arc<Self>, req: CreatePane) -> Result<Response, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("the connection is closing".into());
        }
        let id = self.next_pane.fetch_add(1, Ordering::Relaxed);
        let size = req.size;
        let session = self.session()?;
        let pane = Arc::new(Mutex::new(Pane {
            id,
            term: new_terminal(size),
            backend: Backend::Starting,
            subscribed: false,
            alive: true,
            exit_code: None,
            held_replies: Vec::new(),
        }));
        self.panes.lock().insert(id, pane.clone());
        let started = match self.target.mode {
            Mode::Ssh => self.start_ssh(&session, &pane, size),
            Mode::MoshOnly => self
                .start_mosh(&session, &pane, size)
                .map_err(|e| e.to_string()),
            Mode::Mosh => match self.start_mosh(&session, &pane, size) {
                Ok(()) => Ok(()),
                Err(e) => {
                    let note = format!("\x1b[2m[mosh: {e}; using ssh]\x1b[0m\r\n");
                    pane.lock().term.advance(note.as_bytes());
                    self.start_ssh(&session, &pane, size)
                        .map_err(|s| format!("mosh: {e}; then ssh: {s}"))
                }
            },
        };
        if let Err(e) = started {
            self.panes.lock().remove(&id);
            return Err(e);
        }
        // Closed while it started (the pane, or the whole connection): end what it started.
        // A close from here on finds the backend in the pane and ends it itself.
        if self.closed.load(Ordering::SeqCst) || !self.panes.lock().contains_key(&id) {
            self.panes.lock().remove(&id);
            let backend = std::mem::replace(&mut pane.lock().backend, Backend::Ended);
            if let Some(pid) = end_backend(backend) {
                kill(&session, &[pid]);
            }
            return Err("the pane was closed while it started".into());
        }
        let info = pane.lock().info(&self.title());
        self.event(Event::PaneInfo(info));
        Ok(Response::PaneCreated { pane: id })
    }

    fn start_ssh(
        self: &Arc<Self>,
        session: &Session,
        pane: &Arc<Mutex<Pane>>,
        size: PaneSize,
    ) -> Result<(), String> {
        let id = pane.lock().id;
        let me = Arc::downgrade(self);
        let env = vec![
            ("LANG".to_owned(), "en_US.UTF-8".to_owned()),
            ("TERM_PROGRAM".to_owned(), "thurm".to_owned()),
        ];
        let shell = session
            .open_shell((size.cols, size.rows), "xterm-256color", &env, move |ev| {
                if let Some(me) = me.upgrade() {
                    me.on_shell(id, ev);
                }
            })
            .map_err(|e| e.to_string())?;
        #[cfg(test)]
        std::thread::sleep(Duration::from_millis(
            SSH_START_DELAY_MS.load(Ordering::Relaxed),
        ));
        let mut p = pane.lock();
        if matches!(p.backend, Backend::Ended) {
            // It ended already, and the pane says so (on_shell): nothing to install.
            shell.close();
            return Ok(());
        }
        let held = std::mem::take(&mut p.held_replies);
        if !held.is_empty() {
            shell.write(&held);
        }
        p.backend = Backend::Ssh(shell);
        Ok(())
    }

    fn start_mosh(
        self: &Arc<Self>,
        session: &Session,
        pane: &Arc<Mutex<Pane>>,
        size: PaneSize,
    ) -> Result<(), BootstrapError> {
        let opts = bootstrap::Options {
            server: self.target.mosh_server.clone(),
            ports: self.target.mosh_ports.clone(),
        };
        let exec = |line: &str| session.exec(line, EXEC_TIMEOUT).map_err(|e| e.to_string());
        let connect = bootstrap::start(&exec, &opts)?;
        let id = pane.lock().id;
        let me = Arc::downgrade(self);
        let (answered, answer) = std::sync::mpsc::sync_channel(1);
        let mut p = pane.lock();
        let initial = p.term.serialize_state();
        let started = MoshSession::start(
            self.mosh_rt.as_ref().expect("the mosh runtime").handle(),
            SocketAddr::new(self.ip, connect.port),
            connect.key,
            (size.cols, size.rows),
            move |ev| {
                if ev == MoshEvent::Status(Status::Connected) {
                    let _ = answered.try_send(());
                }
                if let Some(me) = me.upgrade() {
                    me.on_mosh(id, ev);
                }
            },
        );
        let mosh = match started {
            Ok(m) => m,
            Err(e) => {
                // The server started, detached, and would never hear from us.
                drop(p);
                kill(session, &connect.pid.into_iter().collect::<Vec<_>>());
                return Err(BootstrapError::Failed(format!("udp: {e}")));
            }
        };
        p.backend = Backend::Mosh {
            session: mosh,
            pid: connect.pid,
            current: 0,
            snapshots: BTreeMap::from([(0, initial)]),
            status: Status::Connecting,
        };
        // The session's events need the pane while this waits.
        drop(p);
        if answer.recv_timeout(MOSH_ANSWER).is_ok() {
            return Ok(());
        }
        let backend = std::mem::replace(&mut pane.lock().backend, Backend::Starting);
        if let Some(pid) = end_backend(backend) {
            kill(session, &[pid]);
        }
        Err(BootstrapError::Failed(format!(
            "no answer from mosh-server over UDP (port {}) within {} s; a firewall may block it",
            connect.port,
            MOSH_ANSWER.as_secs()
        )))
    }

    /// Takes care of what the pane's terminal wants done: its replies to the host (ssh only:
    /// mosh-server answers terminal queries itself), title, bell, clipboard and notifications.
    fn drain(&self, p: &mut Pane) {
        let mut info = false;
        for ev in p.term.drain_events() {
            match ev {
                TermEvent::PtyWrite(bytes) => match &p.backend {
                    Backend::Ssh(shell) => shell.write(&bytes),
                    Backend::Starting => p.held_replies.extend(bytes),
                    // mosh-server answers terminal queries itself.
                    Backend::Mosh { .. } | Backend::Ended => {}
                },
                TermEvent::Title(_) => info = true,
                TermEvent::Bell => self.event(Event::Bell { pane: p.id }),
                TermEvent::ClipboardStore(text) => {
                    self.event(Event::ClipboardStore { pane: p.id, text })
                }
                TermEvent::ClipboardLoad => self.event(Event::ClipboardRequest { pane: p.id }),
                TermEvent::Notify { title, body } => self.event(Event::Notify {
                    pane: p.id,
                    title,
                    body,
                    permission: None,
                    sound: None,
                }),
                _ => {}
            }
        }
        if info {
            self.event(Event::PaneInfo(p.info(&self.title())));
        }
    }

    fn on_shell(&self, id: PaneId, ev: ShellEvent) {
        let Some(pane) = self.pane(id) else {
            return;
        };
        match ev {
            ShellEvent::Data(data) => {
                let mut p = pane.lock();
                p.term.advance(&data);
                if p.subscribed {
                    self.event(Event::Output { pane: id, data });
                }
                self.drain(&mut p);
            }
            ShellEvent::Exit(Some(code)) => {
                {
                    let mut p = pane.lock();
                    p.alive = false;
                    p.exit_code = Some(code as i32);
                    p.backend = Backend::Ended;
                }
                self.event(Event::PaneExited {
                    pane: id,
                    code: Some(code as i32),
                });
                self.panes.lock().remove(&id);
                self.event(Event::PaneClosed { pane: id });
            }
            ShellEvent::Exit(None) => {
                // The connection went away (the app slept): keep the pane, say so.
                let mut p = pane.lock();
                if matches!(p.backend, Backend::Ended) {
                    return;
                }
                p.alive = false;
                p.backend = Backend::Ended;
                let note = b"\r\n\x1b[2m[connection lost]\x1b[0m\r\n".to_vec();
                p.term.advance(&note);
                if p.subscribed {
                    self.event(Event::Output {
                        pane: id,
                        data: note,
                    });
                }
                self.event(Event::PaneExited {
                    pane: id,
                    code: None,
                });
                self.event(Event::PaneInfo(p.info(&self.title())));
            }
        }
    }

    fn on_mosh(&self, id: PaneId, ev: MoshEvent) {
        let Some(pane) = self.pane(id) else {
            return;
        };
        let mut guard = pane.lock();
        let p = &mut *guard;
        let Backend::Mosh {
            current,
            snapshots,
            status,
            ..
        } = &mut p.backend
        else {
            return;
        };
        match ev {
            MoshEvent::Screen {
                old_num,
                new_num,
                oldest,
                bytes,
            } => {
                if old_num == *current {
                    p.term.advance(&bytes);
                    if p.subscribed {
                        self.event(Event::Output {
                            pane: id,
                            data: bytes,
                        });
                    }
                } else if let Some(snapshot) = snapshots.get(&old_num) {
                    // From an older screen than shown: rebuild, and send the app the result.
                    let mut term = new_terminal(p.term.size());
                    term.replay(snapshot);
                    term.advance(&bytes);
                    p.term = term;
                    if p.subscribed {
                        let state = p.term.serialize_state();
                        self.event(Event::Attach {
                            pane: id,
                            size: p.term.size(),
                            state,
                        });
                    }
                } else {
                    log::warn!("mosh: no screen {old_num} to apply a diff to");
                    return;
                }
                *current = new_num;
                snapshots.retain(|&n, _| n >= oldest);
                self.drain(p);
            }
            MoshEvent::Snapshot { num } => {
                if num == *current {
                    let state = p.term.serialize_state();
                    snapshots.insert(num, state);
                }
            }
            MoshEvent::Ended => {
                // The shell exited (mosh says no more): like an ssh shell's exit.
                p.alive = false;
                p.backend = Backend::Ended;
                drop(guard);
                self.event(Event::PaneExited {
                    pane: id,
                    code: None,
                });
                self.panes.lock().remove(&id);
                self.event(Event::PaneClosed { pane: id });
            }
            MoshEvent::Status(s) => {
                if *status != s {
                    *status = s;
                    self.event(Event::PaneInfo(p.info(&self.title())));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
