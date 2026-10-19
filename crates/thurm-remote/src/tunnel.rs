//! One supervised tunnel per `[[remote]]` host: `ssh -N -L <local.sock>:<remote.sock>`, or for
//! a host with `tailnet` a relay from `<local.sock>` over the tailnet ([`crate::tailnet`]),
//! reconnected with exponential backoff (capped at two minutes, reset after a minute of
//! healthy connection) and on demand ([`Supervisor::kick`]: wake from sleep, network change,
//! the app's connection dropping).
//!
//! Every state change is reported to the app and written to `<name>.state` (JSON) next to the
//! local socket, where `thurm --remote` reads it. Background reconnects never install
//! anything: a host without Thurm or with another protocol only reports that.

use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use thurm_config::{RemoteConfig, TailnetTarget};
use thurm_proto::{Envelope, PROTOCOL_VERSION, Request, Response, ServerMessage, codec};

use crate::ssh::{Ssh, SshError};
use crate::tailnet::{self as tn, OpenError, Via};
use crate::{HostInfo, probe, start_daemon};
use thurm_tailnet_client::ConnectError;

pub const MAX_BACKOFF: Duration = Duration::from_secs(120);
/// A connection that lasted this long resets the backoff.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Connecting,
    Connected,
    /// Lost or not reached yet; retrying on its own.
    Reconnecting,
    /// ssh needs the user (host key, authentication, config); retried, but it won't pass
    /// by itself.
    NeedsAttention,
    /// Thurm is not installed on the host.
    NotInstalled,
    /// The host's daemon (or its `thurm`) speaks another protocol: install our build.
    UpgradeNeeded,
    /// `enabled = false`.
    Disabled,
    Stopped,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Connecting => "connecting",
            Phase::Connected => "connected",
            Phase::Reconnecting => "reconnecting",
            Phase::NeedsAttention => "needs attention",
            Phase::NotInstalled => "not installed",
            Phase::UpgradeNeeded => "upgrade needed",
            Phase::Disabled => "disabled",
            Phase::Stopped => "stopped",
        }
    }
}

/// How a host is reached.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Ssh,
    Tailnet,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub name: String,
    pub host: String,
    pub phase: Phase,
    /// What went wrong (ssh's stderr, the protocol mismatch...).
    pub message: Option<String>,
    /// Local end of the tunnel.
    pub socket: String,
    /// Unix seconds of the next retry, while waiting for one.
    pub retry_at: Option<u64>,
    /// Unix seconds this phase started.
    pub since: u64,
    /// The host's daemon build and protocol, once known.
    pub remote_build: Option<String>,
    pub remote_protocol: Option<u32>,
    /// Connected to another build of the same protocol: an upgrade is available.
    pub upgrade_available: bool,
    /// Platform, for the install offer.
    pub os: Option<String>,
    pub arch: Option<String>,
    /// logind removes the runtime directory (and the daemon's socket) at logout unless
    /// lingering is on.
    pub linger: Option<String>,
    /// pid of the app process that owns the tunnel (stale state files are ignored).
    pub owner_pid: u32,
    /// The `ssh` process of a connected tunnel.
    #[serde(default)]
    pub tunnel_pid: Option<u32>,
    #[serde(default)]
    pub transport: Transport,
    /// How a tailnet host's connection reached the tailnet, once connected.
    #[serde(default)]
    pub via: Option<Via>,
}

impl Status {
    fn new(remote: &RemoteConfig) -> Status {
        Status {
            name: remote.name.clone(),
            host: remote.host.clone(),
            phase: if remote.enabled {
                Phase::Connecting
            } else {
                Phase::Disabled
            },
            message: None,
            socket: thurm_config::remote_socket_path(&remote.name)
                .display()
                .to_string(),
            retry_at: None,
            since: now_secs(),
            remote_build: None,
            remote_protocol: None,
            upgrade_available: false,
            os: None,
            arch: None,
            linger: None,
            owner_pid: std::process::id(),
            tunnel_pid: None,
            transport: transport_of(remote),
            via: None,
        }
    }

    /// Reads `name`'s state file, when a live process owns it.
    pub fn read(name: &str) -> Option<Status> {
        let text = std::fs::read_to_string(thurm_config::remote_state_path(name)).ok()?;
        let s: Status = serde_json::from_str(&text).ok()?;
        let alive = unsafe { libc::kill(s.owner_pid as libc::pid_t, 0) } == 0;
        alive.then_some(s)
    }
}

fn transport_of(remote: &RemoteConfig) -> Transport {
    if remote.tailnet.is_some() {
        Transport::Tailnet
    } else {
        Transport::Ssh
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Delay before retry number `attempt` (0-based): 1, 2, 4 ... seconds, at most two minutes.
pub fn backoff(attempt: u32) -> Duration {
    let secs = 1u64.checked_shl(attempt.min(16)).unwrap_or(u64::MAX);
    Duration::from_secs(secs).min(MAX_BACKOFF)
}

/// Asks the daemon behind `socket` for its protocol and build, giving up after `timeout`
/// (a tunnel whose network is gone accepts the connection and then never answers).
pub fn ping(socket: &Path, timeout: Duration) -> Result<(u32, String), PingError> {
    let stream = UnixStream::connect(socket).map_err(|e| PingError::Unreachable(e.to_string()))?;
    ping_stream(stream, timeout)
}

/// Like [`ping`], over a connection already open to the daemon.
pub fn ping_stream(stream: UnixStream, timeout: Duration) -> Result<(u32, String), PingError> {
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    let mut w = stream
        .try_clone()
        .map_err(|e| PingError::Unreachable(e.to_string()))?;
    codec::write_message(
        &mut w,
        &Envelope {
            id: 1,
            request: Request::Hello {
                client: "thurm-remote-ping".into(),
                version: PROTOCOL_VERSION,
                ui: false,
                capabilities: Vec::new(),
            },
        },
    )
    .map_err(|e| PingError::Unreachable(e.to_string()))?;
    let mut r = BufReader::new(stream);
    loop {
        match codec::read_message::<_, ServerMessage>(&mut r) {
            Ok(Some(ServerMessage::Response { id: 1, result })) => {
                return match result {
                    Ok(Response::Hello {
                        version,
                        build,
                        capabilities,
                        ..
                    }) if thurm_proto::hello_compatible(version, &capabilities) => {
                        Ok((version, build))
                    }
                    Ok(Response::Hello { version, .. }) => Err(PingError::Protocol(version)),
                    Ok(other) => Err(PingError::Unreachable(format!("unexpected {other:?}"))),
                    Err(e) => match mismatched_protocol(&e) {
                        Some(v) => Err(PingError::Protocol(v)),
                        None => Err(PingError::Unreachable(e)),
                    },
                };
            }
            Ok(Some(_)) => continue,
            Ok(None) => return Err(PingError::Unreachable("connection closed".into())),
            Err(e) => return Err(PingError::Unreachable(e.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PingError {
    Unreachable(String),
    /// The daemon speaks this other protocol.
    Protocol(u32),
}

fn mismatched_protocol(error: &str) -> Option<u32> {
    let rest = error.split("protocol mismatch: daemon ").nth(1)?;
    rest.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

type Listener = Box<dyn Fn(&Status) + Send + Sync>;

struct Inner {
    remote: Mutex<RemoteConfig>,
    ssh: Mutex<Ssh>,
    status: Mutex<Status>,
    /// Retry now; with `restart`, also drop a connected tunnel first.
    wake: Mutex<Option<bool>>,
    /// The host or socket was edited: a connected tunnel goes to the old one, replace it.
    reconfigured: AtomicBool,
    cond: Condvar,
    stop: AtomicBool,
    listener: Listener,
    write_state: bool,
}

/// Keeps one host's tunnel up on a background thread.
pub struct Supervisor {
    inner: Arc<Inner>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Supervisor {
    /// Starts supervising `remote`. `listener` runs on the supervisor thread after every
    /// change. With `write_state`, the state file for `thurm --remote` is kept up to date.
    pub fn start(
        remote: RemoteConfig,
        ssh: Ssh,
        write_state: bool,
        listener: impl Fn(&Status) + Send + Sync + 'static,
    ) -> Supervisor {
        let inner = Arc::new(Inner {
            status: Mutex::new(Status::new(&remote)),
            remote: Mutex::new(remote),
            ssh: Mutex::new(ssh),
            wake: Mutex::new(None),
            reconfigured: AtomicBool::new(false),
            cond: Condvar::new(),
            stop: AtomicBool::new(false),
            listener: Box::new(listener),
            write_state,
        });
        let run = inner.clone();
        let name = run.remote.lock().name.clone();
        let thread = std::thread::Builder::new()
            .name(format!("tunnel-{name}"))
            .spawn(move || run.run())
            .expect("spawn tunnel thread");
        Supervisor {
            inner,
            thread: Some(thread),
        }
    }

    pub fn status(&self) -> Status {
        self.inner.status.lock().clone()
    }

    pub fn name(&self) -> String {
        self.inner.remote.lock().name.clone()
    }

    /// Retry now instead of waiting out the backoff. With `restart`, a connected tunnel is
    /// checked and replaced when it no longer answers (after sleep or a network change).
    pub fn kick(&self, restart: bool) {
        let mut w = self.inner.wake.lock();
        *w = Some(w.unwrap_or(false) || restart);
        self.inner.cond.notify_all();
    }

    /// Picks up an edited `[[remote]]` entry (the tunnel is replaced when the host or the
    /// socket changed).
    pub fn update(&self, remote: RemoteConfig) -> Result<(), String> {
        let ssh = Ssh::new(&remote.host)?;
        let (changed, moved) = {
            let mut cur = self.inner.remote.lock();
            let changed = *cur != remote;
            let moved = cur.host != remote.host
                || cur.socket != remote.socket
                || cur.tailnet != remote.tailnet;
            *cur = remote.clone();
            (changed, moved)
        };
        if moved {
            let extra = self.inner.ssh.lock().extra.clone();
            *self.inner.ssh.lock() = Ssh { extra, ..ssh };
            {
                let mut status = self.inner.status.lock();
                status.transport = transport_of(&remote);
                status.host = remote.host;
                status.via = None;
            }
            self.inner.reconfigured.store(true, Ordering::SeqCst);
        }
        if changed {
            self.kick(true);
        }
        Ok(())
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        self.inner.cond.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum Failure {
    Attention(String),
    Transient(String),
    NotInstalled(String),
    Upgrade(String),
}

impl From<SshError> for Failure {
    fn from(e: SshError) -> Self {
        match e {
            SshError::Attention(m) => Failure::Attention(m),
            SshError::Transient(m) => Failure::Transient(m),
            e @ SshError::Remote { .. } => Failure::Transient(e.to_string()),
        }
    }
}

struct Tunnel {
    child: Child,
    stderr: Arc<Mutex<String>>,
}

impl Tunnel {
    fn stderr(&self) -> String {
        self.stderr.lock().trim().to_owned()
    }

    /// `Some(exit code)` once ssh is gone.
    fn exited(&mut self) -> Option<Option<i32>> {
        match self.child.try_wait() {
            Ok(Some(s)) => Some(s.code()),
            Ok(None) => None,
            Err(_) => Some(None),
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A connected host's tunnel.
enum Link {
    Ssh(Tunnel),
    Tailnet(Relay),
}

/// A tailnet host's tunnel: each connection to the local socket gets its own connection over
/// the tailnet, as `ssh -L` gives each its own channel. Dropping it closes them all.
struct Relay {
    stop: Arc<AtomicBool>,
    /// Why the last connection couldn't be opened, until the supervisor looks.
    failure: Arc<Mutex<Option<String>>>,
    conns: Arc<Mutex<HashMap<u64, UnixStream>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
    fn start(
        listener: UnixListener,
        target: TailnetTarget,
        dial_socket: PathBuf,
        name: &str,
    ) -> std::io::Result<Relay> {
        // Polled, so stopping doesn't wait for a connection.
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let conns = Arc::new(Mutex::new(HashMap::new()));
        let (stopped, failed, open) = (stop.clone(), failure.clone(), conns.clone());
        let target = Arc::new(target);
        let dial_socket = Arc::new(dial_socket);
        let thread = std::thread::Builder::new()
            .name(format!("relay-{name}"))
            .spawn(move || {
                let next = AtomicU64::new(0);
                while !stopped.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            let id = next.fetch_add(1, Ordering::Relaxed);
                            let (target, dial_socket) = (target.clone(), dial_socket.clone());
                            let (failed, open) = (failed.clone(), open.clone());
                            std::thread::spawn(move || {
                                relay_one(conn, &target, &dial_socket, &failed, &open, id)
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(e) => {
                            *failed.lock() = Some(format!("the local socket: {e}"));
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                }
            })?;
        Ok(Relay {
            stop,
            failure,
            conns,
            thread: Some(thread),
        })
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        for (_, conn) in self.conns.lock().drain() {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Carries one local connection over its own tailnet connection until either side ends.
fn relay_one(
    conn: UnixStream,
    target: &TailnetTarget,
    dial_socket: &Path,
    failure: &Mutex<Option<String>>,
    conns: &Mutex<HashMap<u64, UnixStream>>,
    id: u64,
) {
    // An accepted socket inherits the listener's non-blocking mode on macOS.
    if conn.set_nonblocking(false).is_err() {
        return;
    }
    let opened = match tn::open(target, dial_socket, thurm_tailnet_client::CONNECT_TIMEOUT) {
        Ok(o) => o,
        Err(e) => {
            *failure.lock() = Some(e.to_string());
            return;
        }
    };
    if let Ok(c) = conn.try_clone() {
        conns.lock().insert(id, c);
    }
    let tn::Opened {
        stream, session, ..
    } = opened;
    splice(conn, stream);
    drop(session);
    conns.lock().remove(&id);
}

/// Copies both ways until either side ends, then closes both.
fn splice(a: UnixStream, b: UnixStream) {
    let (Ok(mut a_rd), Ok(mut b_rd)) = (a.try_clone(), b.try_clone()) else {
        return;
    };
    let (Ok(a2), Ok(b2)) = (a.try_clone(), b.try_clone()) else {
        return;
    };
    let close = move |x: &UnixStream, y: &UnixStream| {
        let _ = x.shutdown(std::net::Shutdown::Both);
        let _ = y.shutdown(std::net::Shutdown::Both);
    };
    let up = std::thread::spawn(move || {
        let mut b_wr = &b2;
        let _ = std::io::copy(&mut a_rd, &mut b_wr);
        close(&a2, &b2);
    });
    let mut a_wr = &a;
    let _ = std::io::copy(&mut b_rd, &mut a_wr);
    close(&a, &b);
    let _ = up.join();
}

impl Inner {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn set(&self, f: impl FnOnce(&mut Status)) {
        let snapshot = {
            let mut s = self.status.lock();
            let before = s.phase;
            f(&mut s);
            if s.phase != before {
                s.since = now_secs();
            }
            s.clone()
        };
        if self.write_state {
            write_state_file(&snapshot);
        }
        (self.listener)(&snapshot);
    }

    fn local_socket(&self) -> PathBuf {
        thurm_config::remote_socket_path(&self.remote.lock().name)
    }

    fn run(self: Arc<Self>) {
        let name = self.remote.lock().name.clone();
        if self.write_state {
            reap_stale_tunnel(&name);
        }
        let mut attempt = 0u32;
        loop {
            if self.stopped() {
                break;
            }
            if !self.remote.lock().enabled {
                self.set(|s| {
                    s.phase = Phase::Disabled;
                    s.message = None;
                    s.retry_at = None;
                });
                self.wait(None);
                continue;
            }
            self.set(|s| {
                if s.phase != Phase::NeedsAttention && s.phase != Phase::UpgradeNeeded {
                    s.phase = if attempt == 0 {
                        Phase::Connecting
                    } else {
                        Phase::Reconnecting
                    };
                }
                s.retry_at = None;
            });
            match self.connect() {
                Ok(link) => {
                    let connected = Instant::now();
                    let reason = match link {
                        Link::Ssh(tunnel) => self.hold(tunnel),
                        Link::Tailnet(relay) => self.hold_relay(relay),
                    };
                    if connected.elapsed() >= HEALTHY_AFTER {
                        attempt = 0;
                    }
                    if self.stopped() {
                        break;
                    }
                    self.set(|s| {
                        s.phase = Phase::Reconnecting;
                        s.message = Some(reason);
                        s.tunnel_pid = None;
                        s.via = None;
                    });
                    // A settings edit requests a new destination now. Do not apply the
                    // retry delay left over from failures at the previous destination.
                    if self.reconfigured.swap(false, Ordering::SeqCst) {
                        attempt = 0;
                        continue;
                    }
                }
                Err(f) => {
                    let (phase, message) = match f {
                        Failure::Attention(m) => (Phase::NeedsAttention, m),
                        Failure::Transient(m) => (Phase::Reconnecting, m),
                        Failure::NotInstalled(m) => (Phase::NotInstalled, m),
                        Failure::Upgrade(m) => (Phase::UpgradeNeeded, m),
                    };
                    self.set(|s| {
                        s.phase = phase;
                        s.message = Some(message);
                    });
                }
            }
            let delay = backoff(attempt);
            attempt = attempt.saturating_add(1);
            let retry_at = now_secs() + delay.as_secs();
            self.set(|s| s.retry_at = Some(retry_at));
            self.wait(Some(delay));
        }
        let _ = std::fs::remove_file(self.local_socket());
        self.set(|s| {
            s.phase = Phase::Stopped;
            s.retry_at = None;
        });
    }

    /// Sleeps until `timeout`, a kick or stop. Returns whether a restart was asked for.
    fn wait(&self, timeout: Option<Duration>) -> bool {
        let mut w = self.wake.lock();
        if w.is_none() && !self.stopped() {
            match timeout {
                Some(t) => {
                    self.cond.wait_for(&mut w, t);
                }
                None => self.cond.wait(&mut w),
            }
        }
        w.take().unwrap_or(false)
    }

    fn connect(&self) -> Result<Link, Failure> {
        // This attempt uses the current settings.
        self.reconfigured.store(false, Ordering::SeqCst);
        let tailnet = self.remote.lock().tailnet.clone();
        match tailnet {
            Some(t) => self.connect_tailnet(&t).map(Link::Tailnet),
            None => self.connect_ssh().map(Link::Ssh),
        }
    }

    /// Opens one connection over the tailnet to check the daemon there, then relays the local
    /// socket to it.
    fn connect_tailnet(&self, tailnet: &str) -> Result<Relay, Failure> {
        let name = self.remote.lock().name.clone();
        let target = thurm_config::parse_remote_tailnet(tailnet).map_err(Failure::Attention)?;
        crate::ssh::prepare_dir().map_err(|e| Failure::Attention(e.to_string()))?;
        let dial_socket = thurm_config::tailnet_dial_socket_path();
        let opened = match tn::open(&target, &dial_socket, thurm_tailnet_client::CONNECT_TIMEOUT) {
            Ok(o) => o,
            Err(OpenError::Connect(ConnectError::DaemonUnavailable)) => {
                // A host listener answers for a daemon that isn't running; ssh can start it.
                return Err(Failure::Transient(match self.start_daemon_over_ssh() {
                    Ok(()) => format!("the daemon on {name} wasn't running; started it over ssh"),
                    Err(e) => format!(
                        "{name} answers over the tailnet, but its daemon doesn't, and starting it \
                         over ssh failed: {e}"
                    ),
                }));
            }
            Err(e) => return Err(open_failure(&name, e)),
        };
        let tn::Opened {
            stream,
            session,
            via,
            ..
        } = opened;
        let pinged = ping_stream(stream, Duration::from_secs(10));
        drop(session);
        let (version, build) = match pinged {
            Ok(v) => v,
            Err(PingError::Protocol(v)) => (v, String::new()),
            Err(PingError::Unreachable(e)) => {
                return Err(Failure::Transient(format!(
                    "the daemon on {name} does not answer over the tailnet: {e}"
                )));
            }
        };
        if version != PROTOCOL_VERSION || build.is_empty() {
            return Err(Failure::Upgrade(format!(
                "the daemon on {name} speaks protocol {version}, this app {PROTOCOL_VERSION}; \
                 installing this build there needs ssh (`thurm remote install {name}`)"
            )));
        }
        let local = self.local_socket();
        let _ = std::fs::remove_file(&local);
        let listener = UnixListener::bind(&local)
            .map_err(|e| Failure::Attention(format!("{}: {e}", local.display())))?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| Failure::Attention(format!("{}: {e}", local.display())))?;
        }
        let relay = Relay::start(listener, target, dial_socket, &name)
            .map_err(|e| Failure::Attention(format!("starting the relay: {e}")))?;
        let upgrade_available = !crate::install::same_build(thurm_proto::BUILD, &build);
        self.set(|s| {
            s.tunnel_pid = None;
            s.phase = Phase::Connected;
            s.message = None;
            s.retry_at = None;
            s.remote_build = Some(build);
            s.remote_protocol = Some(version);
            s.upgrade_available = upgrade_available;
            s.via = Some(via);
        });
        Ok(relay)
    }

    /// Starts the host's daemon over ssh, for a tailnet host whose listener answers for it.
    fn start_daemon_over_ssh(&self) -> Result<(), String> {
        let ssh = self.ssh.lock().clone();
        let configured = self.remote.lock().socket.clone();
        let host = probe(&ssh).map_err(|e| e.to_string())?;
        let socket = configured
            .or(host.info.map(|i| i.socket))
            .ok_or("the thurm there is too old to say where its daemon listens")?;
        start_daemon(&ssh, &socket).map_err(|e| e.to_string())
    }

    fn connect_ssh(&self) -> Result<Tunnel, Failure> {
        let ssh = self.ssh.lock().clone();
        let configured_socket = self.remote.lock().socket.clone();
        crate::ssh::prepare_dir().map_err(|e| Failure::Attention(e.to_string()))?;
        let effective = ssh.effective_config()?;
        crate::ssh::check_forwards(&effective).map_err(Failure::Attention)?;
        let host: HostInfo = probe(&ssh)?;
        self.set(|s| {
            s.os = Some(host.os.clone());
            s.arch = Some(host.arch.clone());
            s.linger = host.linger.clone();
        });
        if host.thurm.is_none() {
            return Err(Failure::NotInstalled(format!(
                "Thurm is not installed on {} ({} {})",
                ssh.target, host.os, host.arch
            )));
        }
        let remote_socket = match (&configured_socket, &host.info) {
            (Some(s), _) => s.clone(),
            (None, Some(info)) => info.socket.clone(),
            (None, None) => {
                return Err(Failure::Upgrade(format!(
                    "the thurm on {} predates remote workspaces; install this build",
                    ssh.target
                )));
            }
        };
        if let Some(info) = &host.info {
            self.set(|s| {
                s.remote_build = Some(info.build.clone());
                s.remote_protocol = Some(info.protocol);
            });
        }
        start_daemon(&ssh, &remote_socket)?;

        let local = self.local_socket();
        let _ = std::fs::remove_file(&local);
        let mut cmd = ssh.tunnel_command(&local, &remote_socket);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| Failure::Attention(format!("cannot run ssh: {e}")))?;
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = stderr.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut s = sink.lock();
                    s.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if s.len() > 16 * 1024 {
                        let cut = s.len() - 8 * 1024;
                        let cut = (cut..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(0);
                        s.drain(..cut);
                    }
                }
            });
        }
        let mut tunnel = Tunnel { child, stderr };
        let (version, build) = self.await_tunnel(&mut tunnel, &local)?;
        if version != PROTOCOL_VERSION {
            return Err(Failure::Upgrade(format!(
                "the daemon on {} speaks protocol {version}, this app {PROTOCOL_VERSION}",
                ssh.target
            )));
        }
        if build.is_empty() {
            return Err(Failure::Upgrade(format!(
                "the daemon on {} is an older build this app cannot talk to",
                ssh.target
            )));
        }
        let upgrade_available = !crate::install::same_build(thurm_proto::BUILD, &build);
        let pid = tunnel.child.id();
        self.set(|s| {
            s.tunnel_pid = Some(pid);
            s.phase = Phase::Connected;
            s.message = None;
            s.retry_at = None;
            s.remote_build = Some(build);
            s.remote_protocol = Some(version);
            s.upgrade_available = upgrade_available;
        });
        Ok(tunnel)
    }

    /// Waits for the forward to answer. Returns the daemon's protocol and build.
    fn await_tunnel(&self, t: &mut Tunnel, local: &Path) -> Result<(u32, String), Failure> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if self.stopped() {
                return Err(Failure::Transient("stopped".into()));
            }
            if let Some(code) = t.exited() {
                // Let the reader thread collect ssh's last words.
                std::thread::sleep(Duration::from_millis(50));
                let err = t.stderr();
                return Err(match code {
                    Some(255) | None => crate::ssh::classify(&err).into(),
                    Some(code) => Failure::Transient(format!("ssh exited ({code}): {err}")),
                });
            }
            if local.exists() {
                match ping(local, Duration::from_secs(5)) {
                    Ok(v) => return Ok(v),
                    Err(PingError::Protocol(v)) => return Ok((v, String::new())),
                    Err(PingError::Unreachable(e)) if Instant::now() > deadline => {
                        return Err(Failure::Transient(format!(
                            "the tunnel is up but the daemon does not answer: {e} (an ssh server \
                             that cannot forward Unix sockets looks like this; `thurm remote \
                             doctor {}` checks)",
                            self.remote.lock().name
                        )));
                    }
                    Err(_) => {}
                }
            }
            if Instant::now() > deadline {
                return Err(Failure::Transient("timed out setting up the tunnel".into()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Watches a connected tailnet relay until a restart or a failed connection finds the
    /// host gone, or stop. Returns why it ended.
    fn hold_relay(&self, relay: Relay) -> String {
        let local = self.local_socket();
        loop {
            let restart = self.wait(Some(Duration::from_millis(500)));
            if self.stopped() {
                return "stopped".into();
            }
            if !self.remote.lock().enabled {
                return "disabled".into();
            }
            if self.reconfigured.load(Ordering::SeqCst) {
                return "the host's settings changed".into();
            }
            let failed = relay.failure.lock().take();
            if !restart && failed.is_none() {
                continue;
            }
            // One connection failing may be a blip: the host is gone once a fresh one fails.
            match ping(&local, Duration::from_secs(10)) {
                Ok((_, build)) => self.note_build(build),
                Err(PingError::Protocol(_)) => {
                    return "the daemon there now speaks another protocol".into();
                }
                Err(e) => {
                    return relay
                        .failure
                        .lock()
                        .take()
                        .or(failed)
                        .unwrap_or_else(|| format!("the connection stopped answering ({e:?})"));
                }
            }
        }
    }

    /// Still up, though the daemon may have been replaced in place (an upgrade): say so again
    /// with its build, so a client whose connection dropped with the old daemon connects again.
    fn note_build(&self, build: String) {
        let upgrade_available = !crate::install::same_build(thurm_proto::BUILD, &build);
        self.set(|s| {
            s.remote_build = Some(build);
            s.upgrade_available = upgrade_available;
        });
    }

    /// Watches a connected tunnel until ssh exits, a restart finds it dead, or stop.
    /// Returns why it ended.
    fn hold(&self, mut tunnel: Tunnel) -> String {
        let local = self.local_socket();
        loop {
            if let Ok(Some(status)) = tunnel.child.try_wait() {
                let err = tunnel.stderr();
                return if err.is_empty() {
                    format!("ssh exited ({status})")
                } else {
                    last_line(&err)
                };
            }
            let restart = self.wait(Some(Duration::from_millis(500)));
            if self.stopped() {
                return "stopped".into();
            }
            if !self.remote.lock().enabled {
                return "disabled".into();
            }
            if self.reconfigured.load(Ordering::SeqCst) {
                return "the host's settings changed".into();
            }
            if restart {
                match ping(&local, Duration::from_secs(4)) {
                    Ok((_, build)) => self.note_build(build),
                    Err(PingError::Protocol(_)) => {
                        return "the daemon there now speaks another protocol".into();
                    }
                    Err(_) => return "the connection stopped answering".into(),
                }
            }
        }
    }
}

/// What a failed tailnet connection means for the host's phase.
fn open_failure(name: &str, e: OpenError) -> Failure {
    match e {
        OpenError::Connect(ConnectError::Unreachable(m)) => {
            Failure::Transient(format!("{name} does not answer over the tailnet: {m}"))
        }
        OpenError::Connect(ConnectError::Forbidden(m)) => Failure::Attention(format!(
            "{name} does not let this Mac in over the tailnet: {m}"
        )),
        OpenError::Connect(ConnectError::DaemonUnavailable) => Failure::Transient(format!(
            "{name} answers over the tailnet, but its daemon doesn't"
        )),
        e => Failure::Attention(e.to_string()),
    }
}

fn last_line(s: &str) -> String {
    s.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(s)
        .trim()
        .to_owned()
}

/// The app that owned `name`'s tunnel went away without closing it (killed, crashed): its
/// `ssh -N` would keep a connection open forever. Stops it, when it is still that tunnel.
fn reap_stale_tunnel(name: &str) {
    let Ok(text) = std::fs::read_to_string(thurm_config::remote_state_path(name)) else {
        return;
    };
    let Ok(old) = serde_json::from_str::<Status>(&text) else {
        return;
    };
    let alive = |pid: u32| unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
    let Some(pid) = old.tunnel_pid else { return };
    if old.owner_pid == std::process::id() || alive(old.owner_pid) || !alive(pid) {
        return;
    }
    // Only an ssh forwarding this tunnel's socket (the pid may have been reused).
    let args = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if is_stale_tunnel(&args, &old.socket) {
        log::info!("stopping the tunnel ssh (pid {pid}) left by a previous Thurm");
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
}

fn is_stale_tunnel(args: &str, socket: &str) -> bool {
    let first = args.split_whitespace().next().unwrap_or("");
    (first == "ssh" || first.ends_with("/ssh"))
        && args.contains(" -N ")
        && args.contains(&format!("-L {socket}:"))
}

fn write_state_file(s: &Status) {
    let path = thurm_config::remote_state_path(&s.name);
    if let Ok(json) = serde_json::to_string(s) {
        let tmp = path.with_extension(format!("state.tmp{}", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_two_minutes() {
        let secs: Vec<u64> = (0..10).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 32, 64, 120, 120, 120]);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
    }

    #[test]
    fn stale_tunnels_recognized() {
        let args = "ssh -F /c -o BatchMode=yes -N -T -L /x/loop.sock:/r.sock devbox";
        assert!(is_stale_tunnel(args, "/x/loop.sock"));
        assert!(!is_stale_tunnel(args, "/x/other.sock"));
        assert!(!is_stale_tunnel("vim -N -L /x/loop.sock:", "/x/loop.sock"));
        assert!(!is_stale_tunnel("", "/x/loop.sock"));
    }

    #[test]
    fn status_json_shape() {
        let r = RemoteConfig {
            name: "devbox".into(),
            host: "devbox".into(),
            socket: None,
            enabled: true,
            clipboard_read: Default::default(),
            tailnet: None,
        };
        let s = Status::new(&r);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""phase":"connecting""#));
        assert_eq!(serde_json::from_str::<Status>(&json).unwrap(), s);
        assert_eq!(Phase::NeedsAttention.label(), "needs attention");
    }
}
