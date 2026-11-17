//! Byte streams through the daemon connection (see `thurm_proto::stream`): the tickets that
//! reserve them, and carrying an opened one.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use thurm_proto::Principal;
use thurm_proto::stream::{self, Loopback, MAX_DATA, StreamClose, StreamFrame};

/// Tickets one client may hold unused.
pub const MAX_PENDING_PER_CLIENT: usize = 64;

/// Tickets held unused, over all clients.
pub const MAX_PENDING: usize = 1024;

/// Streams open at once, over all clients.
pub const MAX_OPEN: usize = 256;

/// What a stream connects to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A TCP port on this host's loopback.
    Tcp { port: u16 },
    /// A TCP port on one of this host's loopback addresses.
    TcpAt { port: u16, at: Loopback },
    /// A file coming in: the `index`th of an upload batch (`crate::transfer`).
    Upload { batch: u64, index: u32 },
    /// A file going out (`crate::transfer`): `path`, of `size` bytes, whose metadata
    /// (`crate::transfer::stamp`) was `stamp` when reserved.
    Download {
        pane: u64,
        path: std::path::PathBuf,
        size: u64,
        stamp: u64,
    },
}

/// A stream reserved by a ticket.
#[derive(Debug)]
pub struct Reservation {
    pub kind: Kind,
    /// Who reserved it; only they may open it.
    pub principal: Principal,
    /// The connection that reserved it, for the per-client cap.
    pub client: u64,
    pub expires: Instant,
}

/// The daemon's streams: tickets not yet used, and how many are open.
pub struct Streams {
    pending: Mutex<HashMap<String, Reservation>>,
    open: Arc<AtomicUsize>,
    ttl: Duration,
}

/// Counts an open stream until dropped.
pub struct OpenStream {
    open: Arc<AtomicUsize>,
}

impl Drop for OpenStream {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Default for Streams {
    fn default() -> Self {
        Self::with_ttl(ticket_ttl())
    }
}

/// [`stream::TICKET_TTL_MS`], or `THURM_STREAM_TICKET_MS` (tests).
pub(crate) fn ticket_ttl() -> Duration {
    let ms = std::env::var("THURM_STREAM_TICKET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(stream::TICKET_TTL_MS);
    Duration::from_millis(ms.into())
}

/// Whether `a` and `b` are the same caller: the same local user, or the same tailnet user.
fn same_principal(a: &Principal, b: &Principal) -> bool {
    use thurm_proto::PrincipalKind;
    a.kind == b.kind
        && match a.kind {
            PrincipalKind::Peer => a.uid == b.uid,
            PrincipalKind::Tailscale => a.user_id.is_some() && a.user_id == b.user_id,
        }
}

fn new_ticket() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("no random source for the ticket: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl Streams {
    pub fn with_ttl(ttl: Duration) -> Streams {
        Streams {
            pending: Mutex::new(HashMap::new()),
            open: Arc::new(AtomicUsize::new(0)),
            ttl,
        }
    }

    /// Reserves a stream for `client`, connected as `principal`: its ticket and how long it is
    /// valid.
    pub fn reserve(
        &self,
        kind: Kind,
        client: u64,
        principal: Principal,
    ) -> Result<(String, u32), String> {
        if self.open.load(Ordering::Acquire) >= MAX_OPEN {
            return Err(format!("too many open streams (at most {MAX_OPEN})"));
        }
        let now = Instant::now();
        let mut pending = self.pending.lock();
        pending.retain(|_, r| r.expires > now);
        if pending.len() >= MAX_PENDING {
            return Err(format!(
                "too many streams waiting to open (at most {MAX_PENDING} in all)"
            ));
        }
        if pending.values().filter(|r| r.client == client).count() >= MAX_PENDING_PER_CLIENT {
            return Err(format!(
                "too many streams waiting to open (at most {MAX_PENDING_PER_CLIENT})"
            ));
        }
        let ticket = new_ticket()?;
        pending.insert(
            ticket.clone(),
            Reservation {
                kind,
                principal,
                client,
                expires: now + self.ttl,
            },
        );
        Ok((ticket, self.ttl.as_millis().min(u32::MAX as u128) as u32))
    }

    /// Forgets the tickets `client` reserved and did not use: a ticket lasts only while the
    /// connection that reserved it stays open.
    pub fn client_gone(&self, client: u64) {
        self.pending.lock().retain(|_, r| r.client != client);
    }

    /// Takes the reservation for `ticket`, presented by `principal`. Any attempt uses the ticket
    /// up: a wrong principal or a late one leaves nothing to try again.
    pub fn claim(
        &self,
        ticket: &str,
        principal: &Principal,
    ) -> Result<(Reservation, OpenStream), String> {
        let r = self
            .pending
            .lock()
            .remove(ticket)
            .ok_or("unknown or used stream ticket")?;
        if r.expires <= Instant::now() {
            return Err("the stream ticket expired".into());
        }
        if !same_principal(&r.principal, principal) {
            return Err("the stream ticket belongs to another caller".into());
        }
        let open = self.open.fetch_add(1, Ordering::AcqRel);
        let guard = OpenStream {
            open: self.open.clone(),
        };
        if open >= MAX_OPEN {
            return Err(format!("too many open streams (at most {MAX_OPEN})"));
        }
        Ok((r, guard))
    }

    #[cfg(test)]
    fn pending(&self) -> usize {
        self.pending.lock().len()
    }
}

/// How far each direction got, and whether the stream is over; behind the socket's writer.
struct Out {
    socket: UnixStream,
    /// This side sent `End`.
    sent_end: bool,
    /// The other side sent `End`.
    got_end: bool,
    /// A `Close` went out (or the socket failed): nothing more is sent.
    closed: bool,
}

impl Out {
    fn send(&mut self, frame: &StreamFrame) {
        if self.closed {
            return;
        }
        if stream::write_frame(&mut self.socket, frame).is_err() {
            self.closed = true;
        }
    }

    fn send_data(&mut self, data: &[u8]) -> bool {
        if self.closed {
            return false;
        }
        if stream::write_data(&mut self.socket, data).is_err() {
            self.closed = true;
        }
        !self.closed
    }

    /// Closes the stream with `why` (once).
    fn close(&mut self, why: StreamClose) {
        self.send(&StreamFrame::Close(why));
        self.closed = true;
        let _ = self.socket.shutdown(Shutdown::Both);
    }

    /// Closes the stream when both directions have ended.
    fn close_if_done(&mut self) {
        if self.sent_end && self.got_end && !self.closed {
            self.close(StreamClose::Ok);
        }
    }
}

/// Carries an opened stream: `reader` and `writer` are the connection's two halves (the reader
/// may hold bytes already read past the `StreamOpen`). Returns once the stream is over.
pub fn serve<R: Read>(kind: Kind, reader: R, writer: UnixStream, open: OpenStream) {
    serve_with(kind, reader, writer, open, |name, pump| {
        std::thread::Builder::new().name(name).spawn(pump)
    });
}

type Pump = Box<dyn FnOnce() + Send>;

/// [`serve`], starting the target-to-caller thread with `spawn`.
fn serve_with<R: Read>(
    kind: Kind,
    mut reader: R,
    writer: UnixStream,
    _open: OpenStream,
    spawn: impl FnOnce(String, Pump) -> std::io::Result<std::thread::JoinHandle<()>>,
) {
    let out = Arc::new(Mutex::new(Out {
        socket: writer,
        sent_end: false,
        got_end: false,
        closed: false,
    }));
    let (port, at) = match kind {
        Kind::Tcp { port } => (port, Loopback::Any),
        Kind::TcpAt { port, at } => (port, at),
        _ => {
            out.lock()
                .close(StreamClose::Internal("not a TCP stream".into()));
            return;
        }
    };
    let tcp = match dial_loopback(port, at) {
        Ok(tcp) => tcp,
        Err(e) => {
            out.lock().close(StreamClose::Refused(e));
            return;
        }
    };
    let Ok(tcp_rd) = tcp.try_clone() else {
        out.lock()
            .close(StreamClose::Internal("cloning the connection".into()));
        return;
    };
    // The target to the caller.
    let up = {
        let out = out.clone();
        spawn(
            format!("stream-{port}"),
            Box::new(move || pump_target(tcp_rd, &out)),
        )
    };
    // Without it nothing would ever end the caller's side: say so and stop.
    let up = match up {
        Ok(up) => up,
        Err(e) => {
            let _ = tcp.shutdown(Shutdown::Both);
            out.lock().close(StreamClose::Internal(format!(
                "starting the stream's thread: {e}"
            )));
            return;
        }
    };
    // The caller to the target.
    loop {
        match stream::read_frame(&mut reader) {
            Ok(Some(StreamFrame::Data(data))) => {
                if let Err(e) = (&tcp).write_all(&data) {
                    out.lock().close(StreamClose::Internal(e.to_string()));
                    break;
                }
            }
            Ok(Some(StreamFrame::End)) => {
                let _ = tcp.shutdown(Shutdown::Write);
                let mut o = out.lock();
                o.got_end = true;
                o.close_if_done();
            }
            Ok(Some(StreamFrame::Close(_))) => {
                out.lock().close(StreamClose::Aborted);
                break;
            }
            // Gone without a word, or out of step: nothing it sent can be trusted to be whole.
            Ok(None) | Err(_) => {
                let mut o = out.lock();
                o.closed = true;
                let _ = o.socket.shutdown(Shutdown::Both);
                break;
            }
        }
    }
    let _ = tcp.shutdown(Shutdown::Both);
    let _ = up.join();
}

/// Copies what the target sends into `Data` frames, then `End` (or a close on an error).
fn pump_target(mut tcp: TcpStream, out: &Mutex<Out>) {
    let mut buf = vec![0u8; MAX_DATA];
    loop {
        match tcp.read(&mut buf) {
            Ok(0) => {
                let mut o = out.lock();
                if !o.closed {
                    o.send(&StreamFrame::End);
                    o.sent_end = true;
                    o.close_if_done();
                }
                return;
            }
            Ok(n) => {
                if !out.lock().send_data(&buf[..n]) {
                    let _ = tcp.shutdown(Shutdown::Both);
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                out.lock().close(StreamClose::Internal(e.to_string()));
                return;
            }
        }
    }
}

/// Connects to `port` on the loopback address `at` (`Any`: IPv4, then IPv6). Never another
/// address.
fn dial_loopback(port: u16, at: Loopback) -> Result<TcpStream, String> {
    let v4: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, port).into();
    let v6: SocketAddr = (std::net::Ipv6Addr::LOCALHOST, port).into();
    let addrs = match at {
        Loopback::Any => vec![v4, v6],
        Loopback::V4 => vec![v4],
        Loopback::V6 => vec![v6],
    };
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(format!(
        "nothing accepts connections on port {port}: {}",
        last.map_or_else(String::new, |e| e.to_string())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_proto::PrincipalKind;

    fn peer(uid: u32) -> Principal {
        Principal {
            kind: PrincipalKind::Peer,
            uid: Some(uid),
            ..Default::default()
        }
    }

    fn tailnet(user: i64) -> Principal {
        Principal {
            kind: PrincipalKind::Tailscale,
            user_id: Some(user),
            ..Default::default()
        }
    }

    #[test]
    fn a_ticket_is_used_once() {
        let s = Streams::with_ttl(Duration::from_secs(60));
        let (t, ms) = s.reserve(Kind::Tcp { port: 1 }, 1, peer(501)).unwrap();
        assert_eq!(t.len(), 64);
        assert_eq!(ms, 60_000);
        let (r, _open) = s.claim(&t, &peer(501)).unwrap();
        assert_eq!(r.kind, Kind::Tcp { port: 1 });
        assert!(s.claim(&t, &peer(501)).is_err());
    }

    #[test]
    fn another_principal_uses_the_ticket_up() {
        let s = Streams::with_ttl(Duration::from_secs(60));
        let (t, _) = s.reserve(Kind::Tcp { port: 1 }, 1, tailnet(7)).unwrap();
        assert!(s.claim(&t, &tailnet(8)).is_err());
        assert!(s.claim(&t, &tailnet(7)).is_err());
        let (t, _) = s.reserve(Kind::Tcp { port: 1 }, 1, peer(501)).unwrap();
        assert!(s.claim(&t, &tailnet(7)).is_err());
        let (t, _) = s.reserve(Kind::Tcp { port: 1 }, 1, peer(501)).unwrap();
        assert!(s.claim(&t, &peer(502)).is_err());
    }

    #[test]
    fn a_tailnet_caller_without_a_user_matches_nobody() {
        let none = Principal {
            kind: PrincipalKind::Tailscale,
            ..Default::default()
        };
        assert!(!same_principal(&none, &none));
    }

    #[test]
    fn an_expired_ticket_is_refused_and_pruned() {
        let s = Streams::with_ttl(Duration::from_millis(1));
        let (t, _) = s.reserve(Kind::Tcp { port: 1 }, 1, peer(501)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(s.claim(&t, &peer(501)).is_err());
        s.reserve(Kind::Tcp { port: 1 }, 1, peer(501)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        s.reserve(Kind::Tcp { port: 1 }, 1, peer(501)).unwrap();
        assert_eq!(s.pending(), 1);
    }

    #[test]
    fn pending_tickets_are_capped_per_client() {
        let s = Streams::with_ttl(Duration::from_secs(60));
        for _ in 0..MAX_PENDING_PER_CLIENT {
            s.reserve(Kind::Tcp { port: 1 }, 1, peer(501)).unwrap();
        }
        assert!(s.reserve(Kind::Tcp { port: 1 }, 1, peer(501)).is_err());
        s.reserve(Kind::Tcp { port: 1 }, 2, peer(501)).unwrap();
    }

    #[test]
    fn pending_tickets_are_capped_in_all() {
        let s = Streams::with_ttl(Duration::from_secs(60));
        let clients = (MAX_PENDING / MAX_PENDING_PER_CLIENT) as u64;
        for client in 0..clients {
            for _ in 0..MAX_PENDING_PER_CLIENT {
                s.reserve(Kind::Tcp { port: 1 }, client, peer(501)).unwrap();
            }
        }
        let e = s
            .reserve(Kind::Tcp { port: 1 }, clients, peer(501))
            .unwrap_err();
        assert!(e.contains("in all"), "{e}");
        s.client_gone(0);
        s.reserve(Kind::Tcp { port: 1 }, clients, peer(501))
            .unwrap();
    }

    #[test]
    fn a_gone_client_leaves_no_tickets() {
        let s = Streams::with_ttl(Duration::from_secs(60));
        let (mine, _) = s.reserve(Kind::Tcp { port: 1 }, 1, peer(501)).unwrap();
        let (theirs, _) = s.reserve(Kind::Tcp { port: 1 }, 2, peer(501)).unwrap();
        s.client_gone(1);
        assert_eq!(s.pending(), 1);
        let Err(e) = s.claim(&mine, &peer(501)) else {
            panic!("a gone client's ticket opened");
        };
        assert!(e.contains("unknown or used"), "{e}");
        s.claim(&theirs, &peer(501)).unwrap();
    }

    #[test]
    fn a_dial_to_one_loopback_reaches_only_that_one() {
        // Two programs on the same port, one per loopback address.
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        let Ok(v6) = std::net::TcpListener::bind(("::1", port)) else {
            return; // No IPv6 here, or the port is taken there.
        };
        let peer_of = |at| dial_loopback(port, at).unwrap().peer_addr().unwrap();
        assert!(peer_of(Loopback::V6).is_ipv6());
        assert!(peer_of(Loopback::V4).is_ipv4());
        assert!(peer_of(Loopback::Any).is_ipv4());
        // Nothing on IPv6: a dial to it is refused, not sent to the IPv4 one.
        drop(v6);
        assert!(dial_loopback(port, Loopback::V6).is_err());
        assert!(
            dial_loopback(port, Loopback::Any)
                .unwrap()
                .peer_addr()
                .unwrap()
                .is_ipv4()
        );
    }

    #[test]
    fn a_stream_whose_thread_cannot_start_closes_with_the_reason() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let s = Streams::with_ttl(Duration::from_secs(60));
        let (t, _) = s.reserve(Kind::Tcp { port }, 1, peer(501)).unwrap();
        let (r, open) = s.claim(&t, &peer(501)).unwrap();
        let (mut caller, daemon) = UnixStream::pair().unwrap();
        let reader = daemon.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            serve_with(r.kind, reader, daemon, open, |_, _| {
                Err(std::io::Error::other("no threads left"))
            })
        });
        let Some(StreamFrame::Close(StreamClose::Internal(why))) =
            stream::read_frame(&mut caller).unwrap()
        else {
            panic!("no Internal close");
        };
        assert!(why.contains("no threads left"), "{why}");
        assert_eq!(stream::read_frame(&mut caller).unwrap(), None);
        server.join().unwrap();
        assert_eq!(s.open.load(Ordering::Acquire), 0);
    }

    #[test]
    fn open_streams_are_capped_and_counted_down() {
        let s = Streams::with_ttl(Duration::from_secs(60));
        let mut open = Vec::new();
        for i in 0..MAX_OPEN as u64 {
            let (t, _) = s.reserve(Kind::Tcp { port: 1 }, i, peer(501)).unwrap();
            open.push(s.claim(&t, &peer(501)).unwrap().1);
        }
        assert!(s.reserve(Kind::Tcp { port: 1 }, 0, peer(501)).is_err());
        open.pop();
        let (t, _) = s.reserve(Kind::Tcp { port: 1 }, 0, peer(501)).unwrap();
        s.claim(&t, &peer(501)).unwrap();
    }
}
