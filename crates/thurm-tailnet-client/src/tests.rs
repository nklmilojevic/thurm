use std::io::{Read, Write};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use super::*;

/// A runtime for the test's server, apart from the client's.
fn server_rt() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

/// A WebSocket server that echoes binary messages, upper-cased so the echo is told apart.
fn echo_server(rt: &Runtime) -> String {
    let ln = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = ln.local_addr().unwrap();
    rt.spawn(async move {
        while let Ok((tcp, _)) = ln.accept().await {
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Binary(b) = msg {
                        let up: Vec<u8> = b.to_ascii_uppercase();
                        if ws.send(Message::Binary(up.into())).await.is_err() {
                            break;
                        }
                    }
                }
            });
        }
    });
    format!("ws://{addr}/v1/connect")
}

/// A plain HTTP server answering every request with `response`.
fn http_server(rt: &Runtime, response: &'static str) -> String {
    let ln = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = ln.local_addr().unwrap();
    rt.spawn(async move {
        while let Ok((mut tcp, _)) = ln.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = tcp.read(&mut buf).await;
                let _ = tcp.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("ws://{addr}/v1/connect")
}

#[test]
fn relays_bytes_both_ways() {
    let server = server_rt();
    let url = echo_server(&server);
    let (mut stream, _session) = open(&url, CONNECT_TIMEOUT).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"hello").unwrap();
    let mut got = [0u8; 5];
    stream.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"HELLO");
}

#[test]
fn dropping_the_session_closes_the_stream() {
    let server = server_rt();
    let url = echo_server(&server);
    let (mut stream, session) = open(&url, CONNECT_TIMEOUT).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    drop(session);
    let mut buf = [0u8; 1];
    assert_eq!(stream.read(&mut buf).unwrap(), 0);
}

#[test]
fn a_refused_account_says_so() {
    let server = server_rt();
    let url = http_server(
        &server,
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 50\r\nConnection: close\r\n\r\n\
         forbidden: other@example.com is not the node owner",
    );
    match open(&url, CONNECT_TIMEOUT) {
        Err(ConnectError::Forbidden(body)) => {
            assert!(body.contains("other@example.com"), "{body}")
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[test]
fn a_daemon_that_is_down_says_so() {
    let server = server_rt();
    let url = http_server(
        &server,
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(matches!(
        open(&url, CONNECT_TIMEOUT),
        Err(ConnectError::DaemonUnavailable)
    ));
}

#[test]
fn a_silent_address_times_out_quickly() {
    // Accepts and never answers, like a tailnet device that is off behind a stale route.
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/v1/connect", ln.local_addr().unwrap());
    let started = Instant::now();
    let r = open(&url, Duration::from_millis(500));
    assert!(
        matches!(r, Err(ConnectError::Unreachable(_))),
        "{:?}",
        r.map(|_| ())
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    drop(ln);
}

#[test]
fn only_websocket_urls() {
    assert!(matches!(
        open("https://mac.tail1234.ts.net/v1/connect", CONNECT_TIMEOUT),
        Err(ConnectError::BadUrl(_))
    ));
}

#[test]
fn errors_tell_what_to_check() {
    let e = ConnectError::Unreachable("connection refused".into()).to_string();
    assert!(e.contains("Tailscale connected on this device"), "{e}");
}

/// A WebSocket server that upgrades and then neither reads nor writes, holding the connection.
fn silent_server(rt: &Runtime) -> String {
    let ln = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = ln.local_addr().unwrap();
    rt.spawn(async move {
        let mut held = Vec::new();
        while let Ok((tcp, _)) = ln.accept().await {
            held.push(tokio_tungstenite::accept_async(tcp).await.unwrap());
        }
    });
    format!("ws://{addr}/v1/connect")
}

const SHORT: Limits = Limits {
    connect: Duration::from_secs(1),
    ping_every: Duration::from_millis(200),
    silent: Duration::from_secs(1),
};

#[test]
fn a_daemon_that_never_says_hello_fails_within_the_limit() {
    let server = server_rt();
    let url = silent_server(&server);
    let started = Instant::now();
    // The silence check must not end it first: this is the Hello's deadline.
    let limits = Limits {
        silent: Duration::from_secs(30),
        ..SHORT
    };
    let r = connect_with(&url, "test", &[], limits, |_| {}, || {});
    assert!(
        matches!(r, Err(ConnectError::DaemonUnavailable)),
        "{:?}",
        r.map(|_| ())
    );
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn a_peer_that_stops_reading_ends_the_relay() {
    let server = server_rt();
    let url = silent_server(&server);
    let (stream, _session) = open_with(&url, SHORT).unwrap();
    let mut reader = stream.try_clone().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    // Write until the buffers fill and the relay's send stalls.
    let mut writer = stream;
    std::thread::spawn(move || {
        let chunk = vec![0u8; 64 * 1024];
        while writer.write_all(&chunk).is_ok() {}
    });
    let started = Instant::now();
    let mut buf = [0u8; 1];
    assert_eq!(
        reader.read(&mut buf).unwrap(),
        0,
        "the relay kept the stream open"
    );
    // The silence limit (1s) ends it, not whatever gives up on a stuck write later.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_send_that_stalls_late_gets_only_the_silence_left() {
    let server = server_rt();
    let url = silent_server(&server);
    let limits = Limits {
        silent: Duration::from_secs(2),
        ..SHORT
    };
    let started = Instant::now();
    let (stream, _session) = open_with(&url, limits).unwrap();
    let mut reader = stream.try_clone().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    // Quiet for most of the limit, then a send that stalls: it may not get a fresh limit.
    let mut writer = stream;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1600));
        let chunk = vec![0u8; 64 * 1024];
        while writer.write_all(&chunk).is_ok() {}
    });
    let mut buf = [0u8; 1];
    assert_eq!(
        reader.read(&mut buf).unwrap(),
        0,
        "the relay kept the stream open"
    );
    // Silent since the start, so it ends near 2s, not 1.6s plus another 2s.
    assert!(
        started.elapsed() < Duration::from_millis(2800),
        "{:?}",
        started.elapsed()
    );
}

/// One end of a socket pair whose other end is bridged to `addr` over TCP: what libtailscale's
/// `tailscale_dial` hands back.
fn dialed(addr: std::net::SocketAddr) -> OwnedFd {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let tcp = std::net::TcpStream::connect(addr).unwrap();
    let (mut t_rd, mut t_wr) = (theirs.try_clone().unwrap(), theirs);
    let (mut c_rd, mut c_wr) = (tcp.try_clone().unwrap(), tcp);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut t_rd, &mut c_wr);
        let _ = c_wr.shutdown(std::net::Shutdown::Write);
    });
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut c_rd, &mut t_wr);
        let _ = t_wr.shutdown(std::net::Shutdown::Both);
    });
    OwnedFd::from(ours)
}

/// A CA, and a server certificate it signed for `thurm.test`.
struct TestPki {
    ca: rustls::pki_types::CertificateDer<'static>,
    chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
}

fn test_pki() -> TestPki {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["thurm.test".to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &issuer)
        .unwrap();
    TestPki {
        ca: ca.der().clone(),
        chain: vec![leaf.der().clone()],
        key: rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
    }
}

/// A client config trusting only `ca`.
fn trusting(ca: &rustls::pki_types::CertificateDer<'static>) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.clone()).unwrap();
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

/// A TLS WebSocket server (certificate for `thurm.test`) that echoes binary messages,
/// upper-cased.
fn tls_echo_server(rt: &Runtime, pki: &TestPki) -> std::net::SocketAddr {
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(pki.chain.clone(), pki.key.clone_key())
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let ln = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = ln.local_addr().unwrap();
    rt.spawn(async move {
        while let Ok((tcp, _)) = ln.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(tls).await else {
                    return;
                };
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Binary(b) = msg {
                        let up: Vec<u8> = b.to_ascii_uppercase();
                        if ws.send(Message::Binary(up.into())).await.is_err() {
                            break;
                        }
                    }
                }
            });
        }
    });
    addr
}

#[test]
fn relays_over_a_dialed_socket_with_tls_for_the_url_host() {
    let server = server_rt();
    let pki = test_pki();
    let addr = tls_echo_server(&server, &pki);
    let (mut stream, _session) = open_fd_tls(
        dialed(addr),
        "wss://thurm.test/v1/connect",
        LIMITS,
        trusting(&pki.ca),
        false,
    )
    .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"over the tailnet").unwrap();
    let mut got = [0u8; 16];
    stream.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"OVER THE TAILNET");
}

#[test]
fn a_dialed_socket_to_another_host_is_refused() {
    let server = server_rt();
    let pki = test_pki();
    let addr = tls_echo_server(&server, &pki);
    // The certificate is for thurm.test: a socket dialed for another name must not pass.
    let r = open_fd_tls(
        dialed(addr),
        "wss://other.test/v1/connect",
        LIMITS,
        trusting(&pki.ca),
        false,
    );
    assert!(
        matches!(r, Err(ConnectError::Protocol(ref e)) if e.starts_with("TLS")),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn a_dialed_socket_with_an_untrusted_certificate_is_refused() {
    let server = server_rt();
    let pki = test_pki();
    let addr = tls_echo_server(&server, &pki);
    let stranger = test_pki();
    let r = open_fd_tls(
        dialed(addr),
        "wss://thurm.test/v1/connect",
        LIMITS,
        trusting(&stranger.ca),
        false,
    );
    assert!(
        matches!(r, Err(ConnectError::Protocol(ref e)) if e.starts_with("TLS")),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn a_dialed_socket_needs_wss() {
    let (ours, mut theirs) = UnixStream::pair().unwrap();
    theirs
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert!(matches!(
        open_fd(
            OwnedFd::from(ours),
            "ws://thurm.test/v1/connect",
            CONNECT_TIMEOUT
        ),
        Err(ConnectError::BadUrl(_))
    ));
    // Taken over and closed.
    let mut buf = [0u8; 1];
    assert_eq!(theirs.read(&mut buf).unwrap(), 0);
}

#[test]
fn a_tailnet_dialed_socket_may_be_plain_ws() {
    let server = server_rt();
    let url = echo_server(&server);
    let addr = url
        .trim_start_matches("ws://")
        .trim_end_matches("/v1/connect")
        .parse()
        .unwrap();
    let (mut stream, _session) = open_tailnet_fd(dialed(addr), &url, CONNECT_TIMEOUT).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"plain").unwrap();
    let mut got = [0u8; 5];
    stream.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"PLAIN");
}

#[test]
fn connect_tailnet_fd_takes_plain_ws_where_connect_fd_refuses_it() {
    let server = server_rt();
    let url = echo_server(&server);
    let addr = url
        .trim_start_matches("ws://")
        .trim_end_matches("/v1/connect")
        .parse()
        .unwrap();
    let refused = connect_fd(dialed(addr), &url, "test", &[], |_| {}, || {});
    assert!(matches!(refused, Err(ConnectError::BadUrl(_))));
    let r = connect_tailnet_fd(dialed(addr), &url, "test", &[], |_| {}, || {});
    assert!(
        !matches!(r, Err(ConnectError::BadUrl(_))),
        "{:?}",
        r.map(|_| ())
    );
}

#[test]
fn a_tailnet_dialed_socket_still_checks_tls_for_wss() {
    let server = server_rt();
    let pki = test_pki();
    let addr = tls_echo_server(&server, &pki);
    // open_tailnet_fd trusts public roots only: the test CA's certificate is refused.
    let r = open_tailnet_fd(dialed(addr), "wss://thurm.test/v1/connect", CONNECT_TIMEOUT);
    assert!(
        matches!(r, Err(ConnectError::Protocol(ref e)) if e.starts_with("TLS")),
        "{:?}",
        r.map(|_| ())
    );
    let (ours, _theirs) = UnixStream::pair().unwrap();
    assert!(matches!(
        open_tailnet_fd(OwnedFd::from(ours), "https://thurm.test/", CONNECT_TIMEOUT),
        Err(ConnectError::BadUrl(_))
    ));
}

/// The relay's decision at a time: the daemon last heard `ago` before, with `pinged` set.
fn decide(ago: Option<Duration>, pinged: Option<Duration>) -> Quiet {
    let now = tokio::time::Instant::now() + Duration::from_secs(100);
    let heard = match ago {
        Some(ago) => Heard::At(now - ago),
        None => Heard::Writing,
    };
    quiet(
        heard,
        pinged.map(|ago| now - ago),
        now,
        Duration::from_secs(15),
        Duration::from_secs(45),
    )
}

#[test]
fn a_daemon_that_talks_is_never_pinged() {
    assert_eq!(
        decide(Some(Duration::from_secs(1)), None),
        Quiet::Wait(Duration::from_secs(14))
    );
    assert_eq!(
        decide(Some(Duration::from_secs(14)), None),
        Quiet::Wait(Duration::from_secs(1))
    );
}

#[test]
fn a_quiet_daemon_is_pinged_once_per_silence() {
    assert_eq!(decide(Some(Duration::from_secs(15)), None), Quiet::Ping);
    assert_eq!(decide(Some(Duration::from_secs(40)), None), Quiet::Ping);
    // Pinged after it was last heard: wait for the answer, until the limit.
    assert_eq!(
        decide(Some(Duration::from_secs(16)), Some(Duration::from_secs(1))),
        Quiet::Wait(Duration::from_secs(29))
    );
    // A pong (or anything else) since the ping makes it a new silence.
    assert_eq!(
        decide(Some(Duration::from_secs(1)), Some(Duration::from_secs(2))),
        Quiet::Wait(Duration::from_secs(14))
    );
}

#[test]
fn a_daemon_silent_for_the_limit_is_gone() {
    assert_eq!(decide(Some(Duration::from_secs(45)), None), Quiet::Close);
    assert_eq!(
        decide(Some(Duration::from_secs(46)), Some(Duration::from_secs(20))),
        Quiet::Close
    );
}

#[test]
fn time_spent_writing_to_the_local_app_is_not_silence() {
    // However long the write has taken, the daemon is not quiet while it goes on.
    assert_eq!(decide(None, None), Quiet::Wait(Duration::from_secs(15)));
    let now = tokio::time::Instant::now();
    assert_eq!(
        silence_left(Heard::Writing, now, Duration::from_secs(45)),
        Duration::from_secs(45)
    );
}

#[test]
fn a_send_gets_only_the_silence_left() {
    let now = tokio::time::Instant::now();
    let heard = Heard::At(now - Duration::from_secs(10));
    assert_eq!(
        silence_left(heard, now, Duration::from_secs(45)),
        Duration::from_secs(35)
    );
    assert_eq!(
        silence_left(heard, now, Duration::from_secs(5)),
        Duration::ZERO
    );
}
