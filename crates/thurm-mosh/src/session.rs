//! The UDP side of a mosh session, on a tokio runtime. The socket can be replaced at any time
//! (a new local port, the same key and sequence numbers): the server follows the newest address
//! it hears from, which is how a session survives changing networks and the app's suspension.

use std::io;
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::crypto::{Crypto, NO_TIMESTAMP, Packet, TO_CLIENT};
use crate::fragment::{self, Assembly};
use crate::ssp::{Apply, Engine};
use crate::wire::{self, HostEvent, UserEvent};

/// After this long without hearing from the server, move to a new socket.
const RESOCKET_AFTER: Duration = Duration::from_secs(6);
/// After this long, stop calling it reconnecting (it may come back; [`MoshSession::wake`]
/// tries again at once).
const STALLED_AFTER: Duration = Duration::from_secs(60);
/// How often a stalled session still tries a new socket.
const STALLED_RESOCKET: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Not heard from the server yet.
    Connecting,
    Connected,
    /// Not heard from the server for a while; trying new sockets.
    Reconnecting,
    /// Not heard from the server for a long while.
    Stalled,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MoshEvent {
    /// The host's screen moved from state `old_num` to `new_num`: apply `bytes` to the screen
    /// of `old_num`, the one shown or a snapshot (see [`Apply`]). Snapshots of states older than
    /// `oldest` are no longer needed.
    Screen {
        old_num: u64,
        new_num: u64,
        oldest: u64,
        bytes: Vec<u8>,
    },
    /// Keep a snapshot of the screen shown now, host state `num`: a later diff may start there.
    Snapshot {
        num: u64,
    },
    Status(Status),
}

enum Cmd {
    Event(UserEvent),
    Wake,
    Close,
}

/// A running session. Dropping it ends the session's task (the server lives on until its own
/// timeout, as mosh-server does when a client goes away).
pub struct MoshSession {
    tx: mpsc::UnboundedSender<Cmd>,
}

fn socket_for(addr: SocketAddr) -> io::Result<UdpSocket> {
    let local: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let s = StdUdpSocket::bind(local)?;
    s.connect(addr)?;
    s.set_nonblocking(true)?;
    UdpSocket::from_std(s)
}

impl MoshSession {
    /// Starts talking to `mosh-server` at `addr` with session `key`, telling it the terminal is
    /// `size` (columns, rows). `on_event` runs on the runtime.
    pub fn start(
        rt: &tokio::runtime::Handle,
        addr: SocketAddr,
        key: [u8; 16],
        size: (u16, u16),
        on_event: impl FnMut(MoshEvent) + Send + 'static,
    ) -> io::Result<MoshSession> {
        let socket = {
            let _guard = rt.enter();
            socket_for(addr)?
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let mut engine = Engine::new(Instant::now());
        engine.push(UserEvent::Resize {
            width: size.0,
            height: size.1,
        });
        let driver = Driver {
            addr,
            socket,
            crypto: Crypto::new(&key),
            engine,
            assembly: Assembly::default(),
            seq: 0,
            instruction_id: 0,
            started: Instant::now(),
            server_timestamp: None,
            last_resocket: Instant::now(),
            status: Status::Connecting,
            on_event: Box::new(on_event),
        };
        rt.spawn(driver.run(rx));
        Ok(MoshSession { tx })
    }

    pub fn send_keys(&self, keys: &[u8]) {
        let _ = self.tx.send(Cmd::Event(UserEvent::Keys(keys.to_vec())));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(Cmd::Event(UserEvent::Resize {
            width: cols,
            height: rows,
        }));
    }

    /// The network may have changed (or the app woke up): move to a new socket and say hello.
    pub fn wake(&self) {
        let _ = self.tx.send(Cmd::Wake);
    }

    pub fn close(&self) {
        let _ = self.tx.send(Cmd::Close);
    }
}

impl Drop for MoshSession {
    fn drop(&mut self) {
        self.close();
    }
}

struct Driver {
    addr: SocketAddr,
    socket: UdpSocket,
    crypto: Crypto,
    engine: Engine,
    assembly: Assembly,
    seq: u64,
    instruction_id: u64,
    started: Instant,
    /// The server's last timestamp and when it came, to send back (its round-trip estimate).
    server_timestamp: Option<(u16, Instant)>,
    last_resocket: Instant,
    status: Status,
    on_event: Box<dyn FnMut(MoshEvent) + Send>,
}

impl Driver {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        (self.on_event)(MoshEvent::Status(Status::Connecting));
        let mut buf = vec![0u8; 65536];
        loop {
            let deadline = self.engine.next_deadline(Instant::now());
            let check = Instant::now() + Duration::from_secs(1);
            tokio::select! {
                r = self.socket.recv(&mut buf) => match r {
                    Ok(n) => self.datagram(&buf[..n]),
                    // An ICMP error from an earlier send, or a socket the system took back
                    // while the app slept.
                    Err(e) => {
                        log::debug!("mosh: receive: {e}");
                        self.resocket();
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                cmd = rx.recv() => match cmd {
                    None | Some(Cmd::Close) => return,
                    Some(Cmd::Event(ev)) => self.engine.push(ev),
                    Some(Cmd::Wake) => {
                        self.resocket();
                        self.engine.force_send();
                    }
                },
                _ = tokio::time::sleep_until(deadline.min(check).into()) => {}
            }
            self.flush().await;
            self.watch();
        }
    }

    fn resocket(&mut self) {
        match socket_for(self.addr) {
            Ok(s) => {
                self.socket = s;
                self.assembly = Assembly::default();
            }
            Err(e) => log::debug!("mosh: new socket: {e}"),
        }
        self.last_resocket = Instant::now();
    }

    fn set_status(&mut self, status: Status) {
        if self.status != status {
            self.status = status;
            (self.on_event)(MoshEvent::Status(status));
        }
    }

    /// Moves to a new socket when the server has gone quiet.
    fn watch(&mut self) {
        let now = Instant::now();
        let silence = self.engine.silence(now);
        if silence < RESOCKET_AFTER {
            return;
        }
        if silence >= STALLED_AFTER {
            self.set_status(Status::Stalled);
        } else if self.engine.heard() {
            self.set_status(Status::Reconnecting);
        }
        let every = if silence >= STALLED_AFTER {
            STALLED_RESOCKET
        } else {
            RESOCKET_AFTER
        };
        if now.saturating_duration_since(self.last_resocket) >= every {
            self.resocket();
            self.engine.force_send();
        }
    }

    fn timestamp(&self, now: Instant) -> u16 {
        now.saturating_duration_since(self.started).as_millis() as u16
    }

    async fn flush(&mut self) {
        let now = Instant::now();
        while let Some(inst) = self.engine.poll(now) {
            if let Some(num) = self.engine.take_snapshot_due() {
                (self.on_event)(MoshEvent::Snapshot { num });
            }
            let compressed = wire::compress(&inst.encode());
            let id = self.instruction_id;
            self.instruction_id += 1;
            for frag in fragment::split(id, &compressed) {
                let timestamp_reply = match self.server_timestamp {
                    Some((ts, at))
                        if now.saturating_duration_since(at) < Duration::from_secs(1) =>
                    {
                        ts.wrapping_add(now.saturating_duration_since(at).as_millis() as u16)
                    }
                    _ => NO_TIMESTAMP,
                };
                let packet = Packet {
                    seq: self.seq,
                    timestamp: self.timestamp(now),
                    timestamp_reply,
                    payload: frag,
                };
                self.seq += 1;
                let datagram = self.crypto.seal(0, &packet);
                if let Err(e) = self.socket.send(&datagram).await {
                    log::debug!("mosh: send: {e}");
                    self.resocket();
                    return;
                }
            }
        }
    }

    fn datagram(&mut self, d: &[u8]) {
        let packet = match self.crypto.open(TO_CLIENT, d) {
            Ok(p) => p,
            Err(e) => {
                log::debug!("{e}");
                return;
            }
        };
        let now = Instant::now();
        if packet.timestamp != NO_TIMESTAMP {
            self.server_timestamp = Some((packet.timestamp, now));
        }
        let whole = match self.assembly.add(&packet.payload) {
            Ok(Some(w)) => w,
            Ok(None) => return,
            Err(e) => {
                log::debug!("{e}");
                return;
            }
        };
        let applied = wire::decompress(&whole)
            .and_then(|raw| wire::Instruction::decode(&raw))
            .and_then(|inst| self.engine.receive(now, inst));
        self.set_status(Status::Connected);
        match applied {
            Ok(Some(apply)) => self.screen(apply),
            Ok(None) => {}
            Err(e) => log::warn!("{e}"),
        }
    }

    fn screen(&mut self, apply: Apply) {
        let mut bytes = Vec::new();
        match wire::decode_host(&apply.diff) {
            Ok(events) => {
                for ev in events {
                    if let HostEvent::Bytes(b) = ev {
                        bytes.extend(b);
                    }
                }
            }
            Err(e) => log::warn!("{e}"),
        }
        (self.on_event)(MoshEvent::Screen {
            old_num: apply.old_num,
            new_num: apply.new_num,
            oldest: apply.oldest,
            bytes,
        });
    }
}
