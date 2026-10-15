//! Connects to a daemon served over Tailscale (`thurm-tailnet`): a WebSocket at
//! `wss://<device>.<tailnet>.ts.net/v1/connect` (or `ws://` on a host's own tailnet address)
//! whose binary messages carry the daemon's byte stream unchanged.
//!
//! The device reaches the tailnet through its own Tailscale (the Tailscale app on iOS); this
//! only speaks WebSocket over TLS. The daemon admits the caller by its Tailscale identity, so
//! there is no key or password here. The relay ends in a Unix socket pair, so the connection is
//! an ordinary [`Client`] (the same trick as `thurm_ssh::Session::open_socket`).

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use thurm_client::{Client, ClientError};
use thurm_proto::Event;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::runtime::Runtime;
use tokio_tungstenite::tungstenite::{self, Message};

/// How long reaching the daemon may take: TCP, TLS and the upgrade. Short, so a device whose
/// Tailscale is off falls back to ssh quickly (a node that is off doesn't refuse; it's silent).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// A ping this often keeps the connection checked while nothing else is said.
const PING_EVERY: Duration = Duration::from_secs(15);
/// Nothing heard for this long (not even a pong): the connection is gone.
const SILENT_LIMIT: Duration = Duration::from_secs(45);

#[derive(Debug)]
pub enum ConnectError {
    /// Not a ws:// or wss:// URL.
    BadUrl(String),
    /// No connection: Tailscale is off on this device, or the daemon's device is.
    Unreachable(String),
    /// The daemon's device refused this device's Tailscale account (HTTP 403).
    Forbidden(String),
    /// The daemon's device answered, but its daemon doesn't (HTTP 503).
    DaemonUnavailable,
    /// Something answered that isn't a Thurm daemon, or the TLS handshake failed.
    Protocol(String),
    /// The daemon refused the Hello.
    Daemon(ClientError),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::BadUrl(e) => write!(f, "not a Thurm tailnet address: {e}"),
            ConnectError::Unreachable(e) => write!(
                f,
                "can't reach the Mac over Tailscale ({e}): is Tailscale connected on this device, \
                 and is Serve over Tailscale on on the Mac?"
            ),
            ConnectError::Forbidden(e) => write!(
                f,
                "the Mac doesn't let this device's Tailscale account in: {e}"
            ),
            ConnectError::DaemonUnavailable => {
                write!(
                    f,
                    "the Mac answered over Tailscale, but Thurm's daemon isn't running there"
                )
            }
            ConnectError::Protocol(e) => write!(f, "{e}"),
            ConnectError::Daemon(e) => write!(f, "the daemon refused the connection: {e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// The relay's runtime: dropping it ends the relay, and with it the connection.
pub struct Session {
    rt: Option<Runtime>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Not waiting: the relay tasks end as their sockets close.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

pub struct Remote {
    pub client: Arc<Client>,
    pub session: Session,
}

/// Connects to the daemon at `url` and says Hello (with `capabilities`, e.g. `CAP_MOBILE`).
pub fn connect(
    url: &str,
    client_name: &str,
    capabilities: &[&str],
    on_event: impl Fn(Event) + Send + 'static,
    on_disconnect: impl FnOnce() + Send + 'static,
) -> Result<Remote, ConnectError> {
    let (stream, session) = open(url, CONNECT_TIMEOUT)?;
    let client = Client::from_stream(
        stream,
        client_name,
        false,
        capabilities,
        on_event,
        on_disconnect,
    )
    .map_err(ConnectError::Daemon)?;
    Ok(Remote { client, session })
}

/// Opens the WebSocket at `url` and relays it to one end of a socket pair, returned with the
/// session that keeps the relay running.
pub fn open(url: &str, timeout: Duration) -> Result<(UnixStream, Session), ConnectError> {
    if !(url.starts_with("wss://") || url.starts_with("ws://")) {
        return Err(ConnectError::BadUrl(url.to_owned()));
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("thurm-tailnet")
        .enable_all()
        .build()
        .map_err(|e| ConnectError::Protocol(format!("starting the relay: {e}")))?;
    let session = Session { rt: Some(rt) };
    let rt = session.rt.as_ref().expect("just made");
    let ws = rt.block_on(async {
        let connector = tokio_tungstenite::Connector::Rustls(tls_config()?);
        let handshake =
            tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector));
        match tokio::time::timeout(timeout, handshake).await {
            Ok(Ok((ws, _))) => Ok(ws),
            Ok(Err(e)) => Err(classify(e)),
            Err(_) => Err(ConnectError::Unreachable(format!(
                "no answer within {} seconds",
                timeout.as_secs()
            ))),
        }
    })?;
    let (ours, theirs) =
        UnixStream::pair().map_err(|e| ConnectError::Protocol(format!("socket pair: {e}")))?;
    theirs
        .set_nonblocking(true)
        .map_err(|e| ConnectError::Protocol(format!("socket pair: {e}")))?;
    let theirs = {
        let _guard = rt.enter();
        tokio::net::UnixStream::from_std(theirs)
            .map_err(|e| ConnectError::Protocol(format!("socket pair: {e}")))?
    };
    rt.spawn(relay(ws, theirs));
    Ok((ours, session))
}

/// Publicly trusted roots: ts.net certificates are Let's Encrypt's.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, ConnectError> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| ConnectError::Protocol(format!("TLS: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

fn classify(e: tungstenite::Error) -> ConnectError {
    match e {
        tungstenite::Error::Http(resp) => {
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).trim().to_owned())
                .unwrap_or_default();
            match resp.status().as_u16() {
                403 => ConnectError::Forbidden(if body.is_empty() {
                    "forbidden".into()
                } else {
                    body
                }),
                503 => ConnectError::DaemonUnavailable,
                code => ConnectError::Protocol(format!(
                    "the address answered HTTP {code}, not a Thurm daemon"
                )),
            }
        }
        tungstenite::Error::Io(e) => ConnectError::Unreachable(e.to_string()),
        tungstenite::Error::Url(e) => ConnectError::BadUrl(e.to_string()),
        tungstenite::Error::Tls(e) => ConnectError::Protocol(format!("TLS: {e}")),
        e => ConnectError::Protocol(e.to_string()),
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Copies the daemon's bytes both ways until either side ends, pinging while it's quiet.
async fn relay(ws: Ws, local: tokio::net::UnixStream) {
    let (mut sink, mut stream) = ws.split();
    let (mut rd, mut wr) = local.into_split();
    let (heard_tx, mut heard_rx) = tokio::sync::watch::channel(());

    let down = async {
        while let Some(msg) = stream.next().await {
            let _ = heard_tx.send(());
            match msg {
                Ok(Message::Binary(b)) => {
                    if wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                // Pongs answer our pings; the library answers the daemon's pings itself.
                Ok(_) => {}
            }
        }
    };
    let up = async {
        let mut buf = vec![0u8; 64 * 1024];
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        let mut last_heard = tokio::time::Instant::now();
        loop {
            tokio::select! {
                n = rd.read(&mut buf) => match n {
                    Ok(0) | Err(_) => {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    }
                    Ok(n) => {
                        if sink.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                },
                _ = ping.tick() => {
                    if last_heard.elapsed() > SILENT_LIMIT {
                        log::info!("tailnet: no answer for {}s; closing", SILENT_LIMIT.as_secs());
                        break;
                    }
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
                changed = heard_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    last_heard = tokio::time::Instant::now();
                }
            }
        }
    };
    // Either side ending ends both: the socket pair closes and the client sees the disconnect.
    tokio::select! {
        _ = down => {}
        _ = up => {}
    }
}

#[cfg(test)]
mod tests;
