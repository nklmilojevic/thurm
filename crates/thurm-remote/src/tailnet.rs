//! Reaching a `[[remote]]` over the tailnet (its `tailnet` setting) instead of ssh.
//!
//! The daemon's embedded node dials out for us: its `thurm-tailnet` helper serves a dial socket
//! ([`thurm_config::tailnet_dial_socket_path`]) where we ask for a connection to a peer, and
//! [`thurm_tailnet_client`] speaks the WebSocket over it. Without the node (off, or a helper too
//! old to dial), this machine's own Tailscale is tried, over the OS network.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thurm_config::TailnetTarget;
use thurm_tailnet_client::{ConnectError, Session};

pub use thurm_tailnet_client::CONNECT_TIMEOUT;

/// The dial socket's refusal codes (`tailnet/internal/dial`).
pub const CODE_NODE_OFF: &str = "node_off";
pub const CODE_NO_SUCH_PEER: &str = "no_such_peer";

/// How the connection reached the tailnet.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// Thurm's embedded node, through the dial socket.
    Node,
    /// This machine's own Tailscale.
    System,
}

impl Via {
    pub fn label(self) -> &'static str {
        match self {
            Via::Node => "via Thurm's Tailscale node",
            Via::System => "via this Mac's Tailscale",
        }
    }
}

#[derive(Debug)]
pub enum DialError {
    /// Nothing listens there: the daemon runs no helper, or one from before dialing out.
    NoSocket(String),
    /// The helper refused: `code` is one of the dial socket's.
    Refused {
        code: String,
        message: String,
    },
    Io(String),
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DialError::NoSocket(e) => write!(f, "Thurm's Tailscale node doesn't dial out ({e})"),
            DialError::Refused { message, .. } => write!(f, "{message}"),
            DialError::Io(e) => write!(f, "the dial socket: {e}"),
        }
    }
}

impl DialError {
    /// The node can't help: try this machine's own Tailscale.
    fn node_unavailable(&self) -> bool {
        match self {
            DialError::NoSocket(_) => true,
            DialError::Refused { code, .. } => code == CODE_NODE_OFF,
            DialError::Io(_) => false,
        }
    }
}

/// A peer, as the dial socket lists it.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub dns_name: String,
    #[serde(default)]
    pub addrs: Vec<String>,
    #[serde(default)]
    pub online: bool,
}

#[derive(Deserialize, Default)]
struct Reply {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    dns_name: String,
    #[serde(default)]
    peers: Vec<Peer>,
    #[serde(default)]
    error: String,
    #[serde(default)]
    code: String,
}

/// Sends `request` to the dial socket and reads its one-line answer. The stream is returned
/// as is: after an ok dial, it carries the peer's bytes.
fn ask(
    socket: &Path,
    request: &serde_json::Value,
    timeout: Duration,
) -> Result<(UnixStream, Reply), DialError> {
    let mut stream = UnixStream::connect(socket).map_err(|e| DialError::NoSocket(e.to_string()))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    let mut line = request.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| DialError::Io(e.to_string()))?;
    // One byte at a time: what follows the answer's newline belongs to the peer.
    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err(DialError::Io("closed without an answer".into())),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if answer.len() >= 64 * 1024 => {
                return Err(DialError::Io("answer too long".into()));
            }
            Ok(_) => answer.push(byte[0]),
            Err(e) => return Err(DialError::Io(e.to_string())),
        }
    }
    stream.set_read_timeout(None).ok();
    stream.set_write_timeout(None).ok();
    let reply: Reply = serde_json::from_slice(&answer)
        .map_err(|e| DialError::Io(format!("unreadable answer: {e}")))?;
    if !reply.ok {
        return Err(DialError::Refused {
            code: reply.code,
            message: reply.error,
        });
    }
    Ok((stream, reply))
}

/// Connects to `host:port` on the tailnet through the dial socket. Returns the connection
/// and the peer's MagicDNS name.
pub fn dial(
    socket: &Path,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<(UnixStream, String), DialError> {
    let (stream, reply) = ask(
        socket,
        &serde_json::json!({"host": host, "port": port}),
        timeout,
    )?;
    Ok((stream, reply.dns_name))
}

/// The tailnet's peers, as the embedded node sees them.
pub fn peers(socket: &Path, timeout: Duration) -> Result<Vec<Peer>, DialError> {
    ask(socket, &serde_json::json!({"peers": true}), timeout).map(|(_, r)| r.peers)
}

/// Why a remote could not be reached over the tailnet.
#[derive(Debug)]
pub enum OpenError {
    /// The address in the config is no good.
    Config(String),
    /// Neither the embedded node nor this machine's Tailscale can reach the tailnet.
    NoTailnet(String),
    /// The node has no such peer.
    NoSuchPeer(String),
    Connect(ConnectError),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Config(e) | OpenError::NoSuchPeer(e) => write!(f, "{e}"),
            OpenError::NoTailnet(e) => write!(
                f,
                "no way onto the tailnet: turn on Tailscale in Thurm (`thurm tailscale on`) or \
                 start the Tailscale app ({e})"
            ),
            OpenError::Connect(e) => write!(f, "{e}"),
        }
    }
}

/// An open connection to a remote daemon over the tailnet.
pub struct Opened {
    /// Our end of the relay: the daemon's byte stream.
    pub stream: UnixStream,
    /// Keeps the relay running.
    pub session: Session,
    pub via: Via,
    /// The WebSocket address it was opened at (a peer's name resolved).
    pub url: String,
}

/// Opens the daemon at `target` (a `[[remote]]`'s `tailnet`): through the embedded node's dial
/// socket when the node runs, else through this machine's own Tailscale (`wss://` only: with
/// that Tailscale down, a `ws://` address would go out unencrypted to whoever has it).
pub fn open(target: &TailnetTarget, socket: &Path, timeout: Duration) -> Result<Opened, OpenError> {
    let (host, port) = match target {
        TailnetTarget::Url { host, port, .. } => (host.as_str(), *port),
        TailnetTarget::Name(name) => (name.as_str(), 443),
    };
    match dial(socket, host, port, timeout) {
        Ok((stream, dns_name)) => {
            let url = match target {
                TailnetTarget::Url { url, .. } => url.clone(),
                TailnetTarget::Name(_) => {
                    format!("wss://{dns_name}{}", thurm_proto::tailnet::CONNECT_PATH)
                }
            };
            let (stream, session) =
                thurm_tailnet_client::open_tailnet_fd(OwnedFd::from(stream), &url, timeout)
                    .map_err(OpenError::Connect)?;
            Ok(Opened {
                stream,
                session,
                via: Via::Node,
                url,
            })
        }
        Err(DialError::Refused { code, message }) if code == CODE_NO_SUCH_PEER => {
            Err(OpenError::NoSuchPeer(message))
        }
        Err(e) if e.node_unavailable() => {
            let url = match target {
                TailnetTarget::Url { url, tls: true, .. } => url.clone(),
                TailnetTarget::Url { .. } => {
                    return Err(OpenError::NoTailnet(format!(
                        "{e}; a ws:// address goes through Thurm's node only"
                    )));
                }
                TailnetTarget::Name(name) => {
                    return Err(OpenError::NoTailnet(format!(
                        "{e}; through the Tailscale app, give {name}'s full wss:// address"
                    )));
                }
            };
            match thurm_tailnet_client::open(&url, timeout) {
                Ok((stream, session)) => Ok(Opened {
                    stream,
                    session,
                    via: Via::System,
                    url,
                }),
                Err(ConnectError::Unreachable(u)) => Err(OpenError::NoTailnet(format!(
                    "{e}; this Mac's Tailscale: {u}"
                ))),
                Err(c) => Err(OpenError::Connect(c)),
            }
        }
        Err(DialError::Refused { message, .. }) => {
            Err(OpenError::Connect(ConnectError::Unreachable(message)))
        }
        Err(e) => Err(OpenError::Connect(ConnectError::Unreachable(e.to_string()))),
    }
}

/// A remote daemon reached over the tailnet, as [`check`] found it.
#[derive(Serialize, Clone, Debug)]
pub struct Checked {
    /// Its WebSocket address, a peer's name resolved: what `tailnet` should say.
    pub url: String,
    pub via: Via,
    pub protocol: u32,
    pub build: String,
}

/// The host part of an ssh target (`me@devbox`, `ssh://me@devbox:2222`), as the peer's name to
/// try when no tailnet address is given. A `Host` alias stays as it is: it may name another
/// machine than its tailnet name.
pub fn peer_name(target: &str) -> &str {
    let t = target.trim();
    let t = t.strip_prefix("ssh://").unwrap_or(t);
    let t = t.rsplit_once('@').map_or(t, |(_, host)| host);
    let t = t.split('/').next().unwrap_or(t);
    match t.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
            host
        }
        _ => t,
    }
}

/// Reaches the daemon at `value` (a `tailnet` setting, or a peer's name) and says Hello, for
/// adding a host: resolves a name to its `wss://` address, and refuses what isn't a reachable
/// Thurm daemon with the reason.
pub fn check(value: &str, socket: &Path, timeout: Duration) -> Result<Checked, String> {
    let target = thurm_config::parse_remote_tailnet(value)?;
    let opened = open(&target, socket, timeout).map_err(|e| e.to_string())?;
    let Opened {
        stream,
        session,
        via,
        url,
    } = opened;
    let pinged = crate::tunnel::ping_stream(stream, timeout);
    drop(session);
    let (protocol, build) = match pinged {
        Ok(v) => v,
        Err(crate::tunnel::PingError::Protocol(v)) => (v, String::new()),
        Err(crate::tunnel::PingError::Unreachable(e)) => {
            return Err(format!("{url} answers, but not as a Thurm daemon: {e}"));
        }
    };
    Ok(Checked {
        url,
        via,
        protocol,
        build,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;

    fn read_line(stream: &UnixStream) -> std::io::Result<String> {
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        Ok(line)
    }

    /// A dial socket that answers each connection with `answer`, then echoes.
    fn fake_socket(answer: &'static str) -> (std::path::PathBuf, std::thread::JoinHandle<String>) {
        let dir = std::env::temp_dir().join(format!(
            "thurm-dial-{}-{}",
            std::process::id(),
            answer.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("d.sock");
        let ln = UnixListener::bind(&path).unwrap();
        let t = std::thread::spawn(move || {
            let (mut s, _) = ln.accept().unwrap();
            let request = read_line(&s).unwrap();
            s.write_all(answer.as_bytes()).unwrap();
            let mut buf = [0u8; 5];
            if s.read_exact(&mut buf).is_ok() {
                s.write_all(&buf).unwrap();
            }
            request
        });
        (path, t)
    }

    #[test]
    fn dial_returns_the_stream_after_the_answer() {
        let (path, t) = fake_socket(
            "{\"ok\":true,\"dns_name\":\"bastion.ts.net\",\"addr\":\"100.64.0.1:443\"}\n",
        );
        let (mut s, dns) = dial(&path, "bastion", 443, Duration::from_secs(5)).unwrap();
        assert_eq!(dns, "bastion.ts.net");
        s.write_all(b"hello").unwrap();
        let mut got = [0u8; 5];
        s.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
        let request: serde_json::Value = serde_json::from_str(&t.join().unwrap()).unwrap();
        assert_eq!(request, serde_json::json!({"host": "bastion", "port": 443}));
    }

    #[test]
    fn refusals_and_missing_sockets() {
        let (path, _t) =
            fake_socket("{\"error\":\"no peer named \\\"x\\\"\",\"code\":\"no_such_peer\"}\n");
        match dial(&path, "x", 443, Duration::from_secs(5)) {
            Err(DialError::Refused { code, message }) => {
                assert_eq!(code, CODE_NO_SUCH_PEER);
                assert!(message.contains("no peer"));
            }
            other => panic!("{:?}", other.map(|_| ())),
        }
        let gone = Path::new("/nonexistent/thurm/tailnet-dial.sock");
        let e = dial(gone, "x", 443, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(e, DialError::NoSocket(_)) && e.node_unavailable());
    }

    #[test]
    fn without_the_node_plain_and_name_addresses_are_refused() {
        let gone = Path::new("/nonexistent/thurm/tailnet-dial.sock");
        let ws = thurm_config::parse_remote_tailnet("ws://100.64.0.1:7297").unwrap();
        assert!(matches!(
            open(&ws, gone, Duration::from_secs(1)),
            Err(OpenError::NoTailnet(e)) if e.contains("ws://")
        ));
        let name = thurm_config::parse_remote_tailnet("bastion").unwrap();
        assert!(matches!(
            open(&name, gone, Duration::from_secs(1)),
            Err(OpenError::NoTailnet(e)) if e.contains("wss://")
        ));
    }

    #[test]
    fn a_node_that_is_off_is_not_an_error_of_its_own() {
        let (path, _t) = fake_socket(
            "{\"error\":\"the embedded Tailscale node is not running\",\"code\":\"node_off\"}\n",
        );
        let e = dial(&path, "x", 443, Duration::from_secs(5)).unwrap_err();
        assert!(e.node_unavailable());
    }

    #[test]
    fn peer_names_from_ssh_targets() {
        for (target, want) in [
            ("devbox", "devbox"),
            ("me@devbox", "devbox"),
            ("ssh://me@devbox:2222", "devbox"),
            ("devbox.tail1234.ts.net", "devbox.tail1234.ts.net"),
            (" me@bastion ", "bastion"),
        ] {
            assert_eq!(peer_name(target), want, "{target}");
        }
    }

    #[test]
    fn peers_listed() {
        let (path, _t) = fake_socket(
            "{\"ok\":true,\"peers\":[{\"name\":\"bastion\",\"dns_name\":\"bastion.ts.net\",\"addrs\":[\"100.64.0.1\"],\"online\":true}]}\n",
        );
        let p = peers(&path, Duration::from_secs(5)).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].dns_name, "bastion.ts.net");
        assert!(p[0].online);
    }
}
