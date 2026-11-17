//! Forwards: a remote host's TCP port opened on this Mac's loopback. Each connection to the
//! local port becomes a byte stream through the host's daemon ([`thurm_proto::stream`]), which
//! dials the port on the host's own loopback. The Mac only ever dials out to the host; a forward
//! listens on `127.0.0.1` only.
//!
//! A forward is opened by the user (a `localhost` link, a picked port, `thurm remote forward`),
//! never because a port appeared. It closes when the user closes it, when its host is removed
//! or disabled, or when the host's complete port lists have lacked its port for
//! [`GONE_GRACE`] and no connection is open. A forward to a port no pane ever listed (a
//! container, a system service) closes only by hand.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thurm_client::{Client, ClientError, ConnectOptions};
use thurm_proto::{PaneId, PortEntry};

/// How long a forward outlives its port's disappearance from every complete list.
pub const GONE_GRACE: Duration = Duration::from_secs(30);

/// The most connections one forward carries at once; more are closed as they arrive.
pub const MAX_CONNECTIONS: usize = 64;

/// Whether `r` may have forwards: a host that is turned off has none.
pub fn forwardable(r: &thurm_config::RemoteConfig) -> Result<(), String> {
    if r.enabled {
        Ok(())
    } else {
        Err(format!(
            "{} is turned off (enabled = false in its [[remote]])",
            r.name
        ))
    }
}

/// An open forward.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Forward {
    pub id: u64,
    /// The `[[remote]]` name.
    pub host: String,
    pub remote_port: u16,
    /// The port on this Mac's `127.0.0.1`.
    pub local: u16,
    /// The link it was last opened for (`https://localhost:8443/app`), to open it again as such.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// What changed, for whoever shows forwards.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "change")]
pub enum Change {
    Opened {
        forward: Forward,
    },
    /// `why`: `closed` (by the user), `host` (removed or disabled), `gone` (the port went away).
    Closed {
        forward: Forward,
        why: String,
    },
}

/// A port some pane of a host listens on, as the host last said.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct KnownPort {
    pub pane: PaneId,
    #[serde(flatten)]
    pub entry: PortEntry,
}

type OnChange = Arc<dyn Fn(Change) + Send + Sync>;
type Conns = Arc<Mutex<HashMap<u64, TcpStream>>>;
type SocketFor = Arc<dyn Fn(&str) -> PathBuf + Send + Sync>;

struct Entry {
    forward: Forward,
    stop: Arc<AtomicBool>,
    /// Connections being carried now.
    active: Arc<AtomicUsize>,
    /// Those connections' local sockets, shut down when the forward closes.
    conns: Conns,
    /// The port was in one of the host's lists at some point: it may close on its own.
    listed: bool,
    /// Since when every complete list of the host has lacked the port.
    missing_since: Option<Instant>,
}

#[derive(Default)]
struct HostPorts {
    /// Each pane's latest list, and whether it was complete.
    panes: BTreeMap<PaneId, (Vec<PortEntry>, bool)>,
}

impl HostPorts {
    fn lists(&self, port: u16) -> bool {
        self.panes
            .values()
            .any(|(ports, _)| ports.iter().any(|p| p.port == port))
    }

    fn all_complete(&self) -> bool {
        self.panes.values().all(|(_, complete)| *complete)
    }
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    open: BTreeMap<u64, Entry>,
    hosts: HashMap<String, HostPorts>,
    controls: HashMap<String, Arc<Client>>,
    sweeping: bool,
}

/// Every forward of this process.
#[derive(Clone)]
pub struct Forwards {
    inner: Arc<Mutex<Inner>>,
    remembered: PathBuf,
    socket_for: SocketFor,
    on_change: Arc<Mutex<Option<OnChange>>>,
    grace: Duration,
    max_connections: usize,
}

impl Default for Forwards {
    /// Forwards to the hosts' tunnels, remembering local ports in the state directory.
    fn default() -> Self {
        Forwards::new(
            thurm_config::state_dir().join("forwards.json"),
            thurm_config::remote_socket_path,
        )
    }
}

impl Forwards {
    /// Forwards remembering local ports in `remembered`, reaching host `name`'s daemon at
    /// `socket_for(name)`.
    pub fn new(
        remembered: PathBuf,
        socket_for: impl Fn(&str) -> PathBuf + Send + Sync + 'static,
    ) -> Forwards {
        Forwards {
            inner: Default::default(),
            remembered,
            socket_for: Arc::new(socket_for),
            on_change: Default::default(),
            grace: GONE_GRACE,
            max_connections: MAX_CONNECTIONS,
        }
    }

    /// Like [`Forwards::new`], carrying at most `n` connections per forward (tests).
    pub fn with_max_connections(mut self, n: usize) -> Forwards {
        self.max_connections = n;
        self
    }

    /// Like [`Forwards::new`], closing forwards `grace` after their port went away (tests).
    pub fn with_grace(mut self, grace: Duration) -> Forwards {
        self.grace = grace;
        self
    }

    /// `f` hears every forward opened and closed (on the thread that did it).
    pub fn on_change(&self, f: impl Fn(Change) + Send + Sync + 'static) {
        *self.on_change.lock() = Some(Arc::new(f));
    }

    fn changed(&self, c: Change) {
        let f = self.on_change.lock().clone();
        if let Some(f) = f {
            f(c);
        }
    }

    pub fn list(&self) -> Vec<Forward> {
        self.inner
            .lock()
            .open
            .values()
            .map(|e| e.forward.clone())
            .collect()
    }

    /// The ports `host`'s panes listen on, as its last lists said, by port.
    pub fn known(&self, host: &str) -> Vec<KnownPort> {
        let g = self.inner.lock();
        let mut out: Vec<KnownPort> = g
            .hosts
            .get(host)
            .into_iter()
            .flat_map(|h| h.panes.iter())
            .flat_map(|(pane, (ports, _))| {
                ports.iter().map(|p| KnownPort {
                    pane: *pane,
                    entry: p.clone(),
                })
            })
            .collect();
        out.sort_by_key(|k| (k.entry.port, k.pane));
        out.dedup_by(|a, b| a.entry.port == b.entry.port);
        out
    }

    /// Opens `host`'s `remote_port` on this Mac, or returns the forward that has it already
    /// (`false`: not new). The local port is `preferred` when given (an error if it is taken),
    /// else the remote port's number, else the port remembered for it, else a free one; never
    /// below 1024 unless the remote port is.
    pub fn open(
        &self,
        host: &str,
        remote_port: u16,
        preferred: Option<u16>,
    ) -> Result<(Forward, bool), String> {
        if remote_port == 0 {
            return Err("port 0 is not a port to forward".into());
        }
        if let Some(f) = self.find(host, remote_port) {
            return existing(f, preferred);
        }
        let listener = match preferred {
            Some(p) => bind(p).map_err(|e| format!("cannot listen on 127.0.0.1:{p}: {e}"))?,
            None => {
                let remembered = self.remembered().get(&key(host, remote_port)).copied();
                [Some(remote_port), remembered]
                    .into_iter()
                    .flatten()
                    .filter(|&p| p >= 1024 || p == remote_port)
                    .find_map(|p| bind(p).ok())
                    .map_or_else(|| bind(0), Ok)
                    .map_err(|e| format!("cannot listen on 127.0.0.1: {e}"))?
            }
        };
        let local = listener.local_addr().map_err(|e| e.to_string())?.port();
        let mut g = self.inner.lock();
        // Another thread may have opened it meanwhile; theirs stands.
        if let Some(e) = g
            .open
            .values()
            .find(|e| e.forward.host == host && e.forward.remote_port == remote_port)
        {
            return existing(e.forward.clone(), preferred);
        }
        g.next_id += 1;
        let forward = Forward {
            id: g.next_id,
            host: host.to_owned(),
            remote_port,
            local,
            link: None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let conns: Conns = Default::default();
        let listed = g.hosts.get(host).is_some_and(|h| h.lists(remote_port));
        g.open.insert(
            forward.id,
            Entry {
                forward: forward.clone(),
                stop: stop.clone(),
                active: active.clone(),
                conns: conns.clone(),
                listed,
                missing_since: None,
            },
        );
        drop(g);
        let me = self.clone();
        let (h, accepting) = (host.to_owned(), forward.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("forward-{host}-{remote_port}"))
            .spawn(move || me.accept_loop(listener, h, accepting, stop, active, conns));
        if let Err(e) = spawned {
            self.inner.lock().open.remove(&forward.id);
            return Err(format!("cannot start the forward: {e}"));
        }
        self.remember(host, remote_port, local);
        self.changed(Change::Opened {
            forward: forward.clone(),
        });
        Ok((forward, true))
    }

    /// Whether `host`'s daemon answers and carries streams: what a forward needs, checked
    /// before opening one so the user hears why it can't work.
    pub fn check(&self, host: &str) -> Result<(), String> {
        let c = match self.control(host, false) {
            Ok(c) => c,
            Err(_) => self
                .control(host, true)
                .map_err(|e| format!("{host} is not connected: {e}"))?,
        };
        if c.has_capability(thurm_proto::CAP_STREAMS) {
            Ok(())
        } else {
            Err(format!(
                "Thurm on {host} is too old to forward ports; upgrade it (Thurm › Remotes…)"
            ))
        }
    }

    /// Forward `id` was opened for `link`: reopening it from a list opens that link.
    pub fn set_link(&self, id: u64, link: &str) {
        if let Some(e) = self.inner.lock().open.get_mut(&id) {
            e.forward.link = Some(link.to_owned());
        }
    }

    fn find(&self, host: &str, remote_port: u16) -> Option<Forward> {
        self.inner
            .lock()
            .open
            .values()
            .find(|e| e.forward.host == host && e.forward.remote_port == remote_port)
            .map(|e| e.forward.clone())
    }

    /// Closes forward `id` (by the user).
    pub fn close(&self, id: u64) -> Result<Forward, String> {
        self.close_why(id, "closed")
            .ok_or_else(|| format!("no forward {id}"))
    }

    /// Stops accepting and ends every connection the forward carries: closing it revokes the
    /// access of programs connected already.
    fn close_why(&self, id: u64, why: &str) -> Option<Forward> {
        let e = self.inner.lock().open.remove(&id)?;
        stop_listening(&e);
        for conn in e.conns.lock().values() {
            let _ = conn.shutdown(Shutdown::Both);
        }
        self.changed(Change::Closed {
            forward: e.forward.clone(),
            why: why.to_owned(),
        });
        Some(e.forward)
    }

    /// Closes the forwards of hosts not in `hosts` (removed or disabled), and forgets their
    /// ports and connections.
    pub fn retain_hosts(&self, hosts: &[&str]) {
        let ids: Vec<u64> = {
            let mut g = self.inner.lock();
            g.hosts.retain(|h, _| hosts.contains(&h.as_str()));
            g.controls.retain(|h, _| hosts.contains(&h.as_str()));
            g.open
                .values()
                .filter(|e| !hosts.contains(&e.forward.host.as_str()))
                .map(|e| e.forward.id)
                .collect()
        };
        for id in ids {
            self.close_why(id, "host");
        }
    }

    /// `host`'s pane `pane` listens on `ports` now (its whole list; `complete`: all of its
    /// process tree was read). Starts the clock on forwards whose port no list has any more.
    pub fn ports_changed(&self, host: &str, pane: PaneId, ports: Vec<PortEntry>, complete: bool) {
        self.ports_changed_at(Instant::now(), host, pane, ports, complete);
        self.ensure_sweeping();
    }

    /// [`Forwards::ports_changed`] at `now` (tests).
    pub fn ports_changed_at(
        &self,
        now: Instant,
        host: &str,
        pane: PaneId,
        ports: Vec<PortEntry>,
        complete: bool,
    ) {
        let mut g = self.inner.lock();
        let h = g.hosts.entry(host.to_owned()).or_default();
        if ports.is_empty() && complete {
            h.panes.remove(&pane);
        } else {
            h.panes.insert(pane, (ports, complete));
        }
        recompute(&mut g, host, now);
    }

    /// The connection to `host` dropped: what its panes listened on is unknown until the next
    /// [`Forwards::ports_snapshot`]. Its forwards stay, and none of them closes meanwhile.
    pub fn host_disconnected(&self, host: &str) {
        let mut g = self.inner.lock();
        g.hosts.remove(host);
        for e in g.open.values_mut().filter(|e| e.forward.host == host) {
            e.missing_since = None;
        }
    }

    /// Every pane's list of `host` at once ([`thurm_proto::Response::Ports`], asked for on
    /// connecting): replaces all that was known of the host, panes gone meanwhile included.
    pub fn ports_snapshot(&self, host: &str, panes: Vec<thurm_proto::PanePorts>) {
        self.ports_snapshot_at(Instant::now(), host, panes);
        self.ensure_sweeping();
    }

    /// [`Forwards::ports_snapshot`] at `now` (tests).
    pub fn ports_snapshot_at(&self, now: Instant, host: &str, panes: Vec<thurm_proto::PanePorts>) {
        let mut g = self.inner.lock();
        let h = g.hosts.entry(host.to_owned()).or_default();
        h.panes = panes
            .into_iter()
            .filter(|p| !(p.ports.is_empty() && p.complete))
            .map(|p| (p.pane, (p.ports, p.complete)))
            .collect();
        recompute(&mut g, host, now);
    }

    /// Closes the forwards whose port has been gone for the grace period with no connection
    /// open; the ones closed.
    pub fn sweep(&self, now: Instant) -> Vec<Forward> {
        let due: Vec<u64> = self
            .inner
            .lock()
            .open
            .values()
            .filter(|e| {
                e.missing_since.is_some_and(|t| now >= t + self.grace)
                    && e.active.load(Ordering::Acquire) == 0
            })
            .map(|e| e.forward.id)
            .collect();
        due.into_iter()
            .filter_map(|id| self.close_why(id, "gone"))
            .collect()
    }

    /// Runs [`Forwards::sweep`] every second while a forward waits to close.
    fn ensure_sweeping(&self) {
        let mut g = self.inner.lock();
        if g.sweeping || !g.open.values().any(|e| e.missing_since.is_some()) {
            return;
        }
        g.sweeping = true;
        drop(g);
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("forward-sweep".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(1).min(me.grace));
                    me.sweep(Instant::now());
                    let mut g = me.inner.lock();
                    if !g.open.values().any(|e| e.missing_since.is_some()) {
                        g.sweeping = false;
                        return;
                    }
                }
            });
        if spawned.is_err() {
            self.inner.lock().sweeping = false;
        }
    }

    fn accept_loop(
        &self,
        listener: TcpListener,
        host: String,
        forward: Forward,
        stop: Arc<AtomicBool>,
        active: Arc<AtomicUsize>,
        conns: Conns,
    ) {
        let mut next_conn = 0u64;
        for conn in listener.incoming() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let conn = match conn {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("forward {host}:{}: {e}", forward.remote_port);
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            // Any program on this machine can connect: a bounded number get a worker each.
            if active.load(Ordering::Acquire) >= self.max_connections {
                log::info!(
                    "forward {host}:{}: {} connections open, refusing another",
                    forward.remote_port,
                    self.max_connections
                );
                let _ = conn.shutdown(Shutdown::Both);
                continue;
            }
            let Ok(registered) = conn.try_clone() else {
                continue;
            };
            next_conn += 1;
            let conn_id = next_conn;
            conns.lock().insert(conn_id, registered);
            active.fetch_add(1, Ordering::AcqRel);
            // Closed between the accept and here: end it as `close_why` ended the others.
            if stop.load(Ordering::Acquire) {
                let _ = conn.shutdown(Shutdown::Both);
            }
            let me = self.clone();
            let (host, port) = (host.clone(), forward.remote_port);
            let (theirs_active, theirs_conns, theirs_stop) =
                (active.clone(), conns.clone(), stop.clone());
            let spawned = std::thread::Builder::new()
                .name(format!("forward-{host}-{port}-conn"))
                .spawn(move || {
                    if let Err(e) = me.carry(&host, port, conn, &theirs_stop) {
                        log::info!("forward {host}:{port}: {e}");
                    }
                    theirs_conns.lock().remove(&conn_id);
                    theirs_active.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                conns.lock().remove(&conn_id);
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// A control connection to `host`'s daemon, made once and kept.
    fn control(&self, host: &str, fresh: bool) -> Result<Arc<Client>, ClientError> {
        if !fresh && let Some(c) = self.inner.lock().controls.get(host) {
            return Ok(c.clone());
        }
        let c = Client::connect(
            ConnectOptions {
                socket: (self.socket_for)(host),
                spawn_daemon: None,
                client_name: "thurm-forward",
                ui: false,
            },
            |_| {},
            || {},
        )?;
        self.inner
            .lock()
            .controls
            .insert(host.to_owned(), c.clone());
        Ok(c)
    }

    /// One connection to the forward: a stream to `port` on `host`, spliced both ways.
    fn carry(
        &self,
        host: &str,
        port: u16,
        conn: TcpStream,
        closed: &AtomicBool,
    ) -> Result<(), String> {
        let ticket = match self.control(host, false).and_then(|c| c.dial_begin(port)) {
            Ok(t) => t,
            // The kept connection may be gone (the tunnel reconnected): once more, afresh.
            Err(_) => self
                .control(host, true)
                .and_then(|c| c.dial_begin(port))
                .map_err(|e| e.to_string())?,
        };
        let socket = (self.socket_for)(host);
        let (mut down, up) =
            thurm_client::stream::open(&socket, &ticket.ticket).map_err(|e| e.to_string())?;
        let _ = conn.set_nodelay(true);
        splice(conn, &mut down, up, closed)
    }

    fn remembered(&self) -> HashMap<String, u16> {
        std::fs::read(&self.remembered)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Keeps `local` as the port for (`host`, `remote_port`), so a URL stays the same between
    /// runs. Best effort.
    fn remember(&self, host: &str, remote_port: u16, local: u16) {
        let mut all = self.remembered();
        if all.insert(key(host, remote_port), local) == Some(local) {
            return;
        }
        if let Err(e) = write_json(&self.remembered, &all) {
            log::info!("cannot remember the forward's port: {e}");
        }
    }
}

/// The forward that has the port already, unless another local port was asked for (`0`: any).
fn existing(f: Forward, preferred: Option<u16>) -> Result<(Forward, bool), String> {
    match preferred {
        Some(p) if p != 0 && p != f.local => Err(format!(
            "{}:{} is forwarded to localhost:{} already; close that forward to use {p}",
            f.host, f.remote_port, f.local
        )),
        _ => Ok((f, false)),
    }
}

/// Whether each of `host`'s forwards is listed, and since when every complete list has lacked
/// it. Absent from a set of lists that is not complete, a port may still be there: no clock.
fn recompute(g: &mut Inner, host: &str, now: Instant) {
    let Inner { open, hosts, .. } = g;
    let empty = HostPorts::default();
    let h = hosts.get(host).unwrap_or(&empty);
    for e in open.values_mut().filter(|e| e.forward.host == host) {
        if h.lists(e.forward.remote_port) {
            e.listed = true;
            e.missing_since = None;
        } else if e.listed && h.all_complete() {
            e.missing_since.get_or_insert(now);
        } else {
            e.missing_since = None;
        }
    }
}

fn key(host: &str, port: u16) -> String {
    format!("{host}:{port}")
}

fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
}

/// Stops `e`'s accept loop: the flag, then a connection to wake its `accept`.
fn stop_listening(e: &Entry) {
    e.stop.store(true, Ordering::Release);
    let _ = TcpStream::connect_timeout(
        &([127, 0, 0, 1], e.forward.local).into(),
        Duration::from_secs(1),
    );
}

/// Carries `conn` both ways over a stream until both directions end. Once `closed` is set (the
/// forward closed, which shuts `conn` down), the stream is aborted rather than finished, so the
/// host's end goes too.
fn splice(
    conn: TcpStream,
    down: &mut thurm_client::stream::StreamReader,
    mut up: thurm_client::stream::StreamWriter,
    closed: &AtomicBool,
) -> Result<(), String> {
    let mut from_tcp = conn.try_clone().map_err(|e| e.to_string())?;
    let to_tcp = conn;
    std::thread::scope(|s| {
        let upward = s.spawn(move || -> std::io::Result<()> {
            let mut buf = vec![0u8; thurm_proto::stream::MAX_DATA];
            loop {
                // An error drops `up` unfinished, which aborts the stream.
                let n = from_tcp.read(&mut buf)?;
                if closed.load(Ordering::Acquire) {
                    return up.close(thurm_proto::stream::StreamClose::Aborted);
                }
                if n == 0 {
                    return up.finish();
                }
                up.write_all(&buf[..n])?;
            }
        });
        let mut to_tcp_w = &to_tcp;
        let downward = std::io::copy(down, &mut to_tcp_w);
        match &downward {
            Ok(_) => {
                let _ = to_tcp.shutdown(Shutdown::Write);
            }
            // Refused or failed on the host: the local side ends too.
            Err(_) => {
                let _ = to_tcp.shutdown(Shutdown::Both);
            }
        }
        let upward = upward
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("panicked")));
        downward.map_err(|e| e.to_string())?;
        upward.map_err(|e| e.to_string())
    })
}

/// `url` (an `http`/`https` link to this host's loopback) pointing at `local` on this Mac:
/// host and port swapped, user info, path, query and fragment kept. `localhost` stays
/// `localhost`; other loopback addresses become `127.0.0.1`, where forwards listen.
pub fn rewrite(url: &str, local: u16) -> Option<String> {
    let u = crate::policy::LocalUrl::parse(url)?;
    let host = if u.host.eq_ignore_ascii_case("localhost") {
        "localhost"
    } else {
        "127.0.0.1"
    };
    Some(format!(
        "{}://{}{host}:{local}{}",
        u.scheme, u.userinfo, u.rest
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(port: u16) -> PortEntry {
        PortEntry {
            port,
            address: "127.0.0.1".into(),
            pid: 42,
            command: "vite".into(),
        }
    }

    fn forwards(dir: &Path) -> Forwards {
        Forwards::new(dir.join("forwards.json"), |h| {
            PathBuf::from(format!("/nonexistent/{h}.sock"))
        })
    }

    /// Waits until nothing listens on `port` any more.
    fn wait_unbound(port: u16) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(("127.0.0.1", port)).is_ok() {
            assert!(Instant::now() < deadline, "{port} still listening");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("thurm-forward-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rewrites_keep_path_query_and_fragment() {
        assert_eq!(
            rewrite("http://localhost:5173/app/?q=1#top", 5174).as_deref(),
            Some("http://localhost:5174/app/?q=1#top")
        );
        assert_eq!(
            rewrite("https://127.0.0.1:8443", 9000).as_deref(),
            Some("https://127.0.0.1:9000")
        );
        assert_eq!(
            rewrite("http://[::1]:3000/x", 3000).as_deref(),
            Some("http://127.0.0.1:3000/x")
        );
        assert_eq!(
            rewrite("http://0.0.0.0:8000?a", 8001).as_deref(),
            Some("http://127.0.0.1:8001?a")
        );
        assert_eq!(
            rewrite("HTTP://LocalHost/", 18080).as_deref(),
            Some("http://localhost:18080/")
        );
        assert_eq!(
            rewrite("http://me:pw@localhost:4000/a", 4001).as_deref(),
            Some("http://me:pw@localhost:4001/a")
        );
        assert_eq!(rewrite("http://example.com:80/", 80), None);
    }

    #[test]
    fn a_forward_takes_the_remote_ports_number_then_remembers_its_own() {
        let dir = tempdir("numbers");
        let f = forwards(&dir);
        // A free port's number: the forward listens on it. Tests running alongside may take the
        // port between the probe and the forward; then another one.
        let (a, free) = (0..20)
            .find_map(|_| {
                let probe = TcpListener::bind("127.0.0.1:0").unwrap();
                let free = probe.local_addr().unwrap().port();
                drop(probe);
                let (a, new) = f.open("devbox", free, None).unwrap();
                assert!(new);
                if a.local == free {
                    return Some((a, free));
                }
                f.close(a.id).unwrap();
                None
            })
            .expect("a forward on its own port number");
        // The same host and port: the same forward.
        let (again, new) = f.open("devbox", free, None).unwrap();
        assert!(!new);
        assert_eq!(again, a);
        // Another host's same port can't have the number: a free one, remembered.
        let (b, _) = f.open("other", free, None).unwrap();
        assert_ne!(b.local, free);
        assert!(b.local >= 1024);
        f.close(b.id).unwrap();
        f.close(a.id).unwrap();
        wait_unbound(a.local);
        wait_unbound(b.local);
        // While `free` is taken by something else, `other` gets the port it had before.
        let squatter = TcpListener::bind(("127.0.0.1", free)).unwrap();
        let (b2, _) = f.open("other", free, None).unwrap();
        assert_eq!(b2.local, b.local);
        drop(squatter);
        // A port asked for that is taken is an error, not another port.
        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = held.local_addr().unwrap().port();
        assert!(f.open("devbox", 1, Some(taken)).is_err());
        f.close(b2.id).unwrap();
        assert!(f.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_low_port_only_for_a_low_remote_port() {
        let dir = tempdir("low");
        let f = forwards(&dir);
        // Remembered below 1024 for a high port (a hand-edited file): not used.
        write_json(
            &dir.join("forwards.json"),
            &HashMap::from([(key("devbox", 1), 80u16)]),
        )
        .unwrap();
        let hold = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = hold.local_addr().unwrap().port();
        write_json(
            &dir.join("forwards.json"),
            &HashMap::from([(key("devbox", busy), 81u16)]),
        )
        .unwrap();
        let (fw, _) = f.open("devbox", busy, None).unwrap();
        assert!(fw.local >= 1024, "{fw:?}");
        f.close(fw.id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closing_stops_listening() {
        let dir = tempdir("close");
        let f = forwards(&dir);
        let (fw, _) = f.open("devbox", 40123, Some(0)).unwrap();
        assert!(TcpStream::connect(("127.0.0.1", fw.local)).is_ok());
        f.close(fw.id).unwrap();
        // The accept loop returns and drops the listener.
        wait_unbound(fw.local);
        assert!(f.close(fw.id).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_forward_closes_after_its_port_is_gone_for_a_while() {
        let dir = tempdir("gone");
        let f = forwards(&dir);
        let changes = Arc::new(Mutex::new(Vec::new()));
        let seen = changes.clone();
        f.on_change(move |c| seen.lock().push(c));
        let t0 = Instant::now();
        f.ports_changed_at(t0, "devbox", 1, vec![entry(5173)], true);
        let (listed, _) = f.open("devbox", 5173, Some(0)).unwrap();
        let (never, _) = f.open("devbox", 6000, Some(0)).unwrap();
        assert_eq!(f.known("devbox").len(), 1);

        // Gone from an incomplete list: no clock yet.
        f.ports_changed_at(t0, "devbox", 1, vec![], false);
        assert!(f.sweep(t0 + GONE_GRACE * 2).is_empty());
        // Gone from a complete one: closes after the grace, not before.
        let t1 = t0 + Duration::from_secs(1);
        f.ports_changed_at(t1, "devbox", 1, vec![], true);
        assert!(
            f.sweep(t1 + GONE_GRACE - Duration::from_millis(1))
                .is_empty()
        );
        // Back in the meantime: the clock stops.
        f.ports_changed_at(
            t1 + Duration::from_secs(5),
            "devbox",
            2,
            vec![entry(5173)],
            true,
        );
        assert!(f.sweep(t1 + GONE_GRACE * 3).is_empty());
        let t2 = t1 + Duration::from_secs(10);
        f.ports_changed_at(t2, "devbox", 2, vec![], true);
        let closed = f.sweep(t2 + GONE_GRACE);
        assert_eq!(closed, vec![listed.clone()]);
        // A port no pane ever listed stays until closed by hand.
        assert_eq!(f.list(), vec![never.clone()]);
        let log = changes.lock().clone();
        assert!(log.contains(&Change::Closed {
            forward: listed,
            why: "gone".into()
        }));
        f.close(never.id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_open_connection_holds_a_gone_forward() {
        let dir = tempdir("held");
        let f = forwards(&dir);
        let t0 = Instant::now();
        f.ports_changed_at(t0, "devbox", 1, vec![entry(7000)], true);
        let (fw, _) = f.open("devbox", 7000, Some(0)).unwrap();
        f.ports_changed_at(t0, "devbox", 1, vec![], true);
        f.inner.lock().open[&fw.id]
            .active
            .fetch_add(1, Ordering::AcqRel);
        assert!(f.sweep(t0 + GONE_GRACE).is_empty());
        f.inner.lock().open[&fw.id]
            .active
            .fetch_sub(1, Ordering::AcqRel);
        assert_eq!(f.sweep(t0 + GONE_GRACE), vec![fw]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_explicit_local_port_is_not_swapped_for_the_existing_one() {
        let dir = tempdir("preferred");
        let f = forwards(&dir);
        let (fw, _) = f.open("devbox", 7200, Some(0)).unwrap();
        // The same ask, or any port: the forward there is.
        assert_eq!(f.open("devbox", 7200, None).unwrap(), (fw.clone(), false));
        assert_eq!(
            f.open("devbox", 7200, Some(0)).unwrap(),
            (fw.clone(), false)
        );
        assert_eq!(
            f.open("devbox", 7200, Some(fw.local)).unwrap(),
            (fw.clone(), false)
        );
        // Another port asked for: an error naming the one in use, not that one quietly.
        let other = if fw.local == 65535 {
            65534
        } else {
            fw.local + 1
        };
        let e = f.open("devbox", 7200, Some(other)).unwrap_err();
        assert!(e.contains(&format!("localhost:{}", fw.local)), "{e}");
        f.close(fw.id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_incomplete_list_stops_the_clock() {
        let dir = tempdir("incomplete");
        let f = forwards(&dir);
        let t0 = Instant::now();
        f.ports_changed_at(t0, "devbox", 1, vec![entry(5173)], true);
        let (fw, _) = f.open("devbox", 5173, Some(0)).unwrap();
        // Gone from a complete list: the clock starts.
        f.ports_changed_at(t0, "devbox", 1, vec![], true);
        // Then a list that could not be read whole: the port may still be there.
        f.ports_changed_at(t0 + Duration::from_secs(1), "devbox", 2, vec![], false);
        assert!(f.sweep(t0 + GONE_GRACE * 3).is_empty());
        // Complete again and still gone: the clock starts over from there.
        let t1 = t0 + GONE_GRACE * 3;
        f.ports_changed_at(t1, "devbox", 2, vec![], true);
        assert!(
            f.sweep(t1 + GONE_GRACE - Duration::from_millis(1))
                .is_empty()
        );
        assert_eq!(f.sweep(t1 + GONE_GRACE), vec![fw]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reconnected_host_starts_from_its_snapshot() {
        let dir = tempdir("snapshot");
        let f = forwards(&dir);
        let t0 = Instant::now();
        f.ports_changed_at(t0, "devbox", 1, vec![entry(5173)], true);
        f.ports_changed_at(t0, "devbox", 2, vec![entry(8080)], true);
        let (fw, _) = f.open("devbox", 5173, Some(0)).unwrap();
        // The connection drops: nothing is known of the host's ports, and nothing closes.
        f.host_disconnected("devbox");
        assert!(f.known("devbox").is_empty());
        assert!(f.sweep(t0 + GONE_GRACE * 3).is_empty());
        // Back: pane 1 ended while it was away (no event will ever say so); the snapshot does.
        let t1 = t0 + Duration::from_secs(60);
        f.ports_snapshot_at(
            t1,
            "devbox",
            vec![
                thurm_proto::PanePorts {
                    pane: 2,
                    ports: vec![entry(8080)],
                    complete: true,
                },
                thurm_proto::PanePorts {
                    pane: 3,
                    ports: vec![],
                    complete: true,
                },
            ],
        );
        assert_eq!(
            f.known("devbox")
                .iter()
                .map(|k| k.entry.port)
                .collect::<Vec<_>>(),
            [8080]
        );
        assert!(
            f.sweep(t1 + GONE_GRACE - Duration::from_millis(1))
                .is_empty()
        );
        assert_eq!(f.sweep(t1 + GONE_GRACE), vec![fw]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_forward_keeps_the_link_it_was_opened_for() {
        let dir = tempdir("link");
        let f = forwards(&dir);
        let (fw, _) = f.open("devbox", 8443, Some(0)).unwrap();
        assert_eq!(f.list()[0].link, None);
        f.set_link(fw.id, "https://localhost:8443/app?x=1");
        let listed = f.list().remove(0);
        assert_eq!(
            listed.link.as_deref(),
            Some("https://localhost:8443/app?x=1")
        );
        assert_eq!(
            rewrite(listed.link.as_deref().unwrap(), listed.local).unwrap(),
            format!("https://localhost:{}/app?x=1", listed.local)
        );
        f.close(fw.id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_turned_off_host_has_no_forwards() {
        let cfg = thurm_config::Config::parse(
            "[[remote]]\nname = \"on\"\nhost = \"a\"\n[[remote]]\nname = \"off\"\nhost = \"b\"\nenabled = false\n",
        )
        .unwrap();
        assert!(forwardable(cfg.remote("on").unwrap()).is_ok());
        let e = forwardable(cfg.remote("off").unwrap()).unwrap_err();
        assert!(e.contains("turned off"), "{e}");
    }

    #[test]
    fn removed_hosts_lose_their_forwards() {
        let dir = tempdir("hosts");
        let f = forwards(&dir);
        let (a, _) = f.open("devbox", 7100, Some(0)).unwrap();
        let (b, _) = f.open("pi", 7100, Some(0)).unwrap();
        f.retain_hosts(&["pi"]);
        assert_eq!(f.list(), vec![b.clone()]);
        assert!(f.close(a.id).is_err());
        f.close(b.id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
