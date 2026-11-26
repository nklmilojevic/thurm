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

fn open_test(url: &str, timeout: Duration) -> Result<(UnixStream, Session), ConnectError> {
    open_with(
        url,
        Limits {
            connect: timeout,
            ..LIMITS
        },
    )
}

#[test]
fn relays_bytes_both_ways() {
    let server = server_rt();
    let url = echo_server(&server);
    let (mut stream, _session) = open_test(&url, CONNECT_TIMEOUT).unwrap();
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
    let (mut stream, session) = open_test(&url, CONNECT_TIMEOUT).unwrap();
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
    match open_test(&url, CONNECT_TIMEOUT) {
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
        open_test(&url, CONNECT_TIMEOUT),
        Err(ConnectError::DaemonUnavailable)
    ));
}

#[test]
fn a_silent_address_times_out_quickly() {
    // Accepts and never answers, like a tailnet device that is off behind a stale route.
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}/v1/connect", ln.local_addr().unwrap());
    let started = Instant::now();
    let r = open_test(&url, Duration::from_millis(500));
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
    // Cleartext ws:// is rejected by open() and connect(), even on a Tailscale IP.
    for ws in [
        "ws://127.0.0.1:7297/v1/connect",
        "ws://100.64.0.7:7297/v1/connect",
        "ws://host.tail1234.ts.net:7297/v1/connect",
    ] {
        assert!(
            matches!(open(ws, CONNECT_TIMEOUT), Err(ConnectError::BadUrl(_))),
            "{ws}"
        );
        assert!(
            matches!(
                connect(ws, "test", &[], |_| {}, || {}),
                Err(ConnectError::BadUrl(_))
            ),
            "{ws}"
        );
    }
}

#[test]
fn plain_tailnet_allows_only_ws_tailscale_ip_literals() {
    for ok in [
        "ws://100.64.0.1:7297/v1/connect",
        "ws://100.100.100.100:7297/v1/connect",
        "ws://100.127.255.254:7297/v1/connect",
        "ws://[fd7a:115c:a1e0::1]:7297/v1/connect",
        "ws://[fd7a:115c:a1e0:ab12::42]/v1/connect",
    ] {
        assert!(check_plain_tailnet_url(ok).is_ok(), "{ok}");
    }
    for bad in [
        "wss://100.64.0.1/v1/connect",
        "wss://host.tail1234.ts.net/v1/connect",
        "ws://100.63.255.255:7297/v1/connect",
        "ws://100.128.0.0:7297/v1/connect",
        "ws://127.0.0.1:7297/v1/connect",
        "ws://192.168.1.10:7297/v1/connect",
        "ws://[::1]:7297/v1/connect",
        "ws://[fe80::1]:7297/v1/connect",
        "ws://[fd7a:115c:a1e1::1]:7297/v1/connect",
        "ws://host.tail1234.ts.net:7297/v1/connect",
        "ws://100.64.0.1@evil.example:7297/v1/connect",
        "http://100.64.0.1:7297/v1/connect",
    ] {
        assert!(
            matches!(
                open_plain(bad, CONNECT_TIMEOUT),
                Err(ConnectError::BadUrl(_))
            ),
            "{bad}"
        );
        assert!(
            matches!(
                connect_plain(bad, "test", &[], |_| {}, || {}),
                Err(ConnectError::BadUrl(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn plain_tailnet_fails_closed_without_active_tailscale_tunnel() {
    use std::net::IpAddr;

    let target_v4: IpAddr = "100.64.0.7".parse().unwrap();
    let target_v6: IpAddr = "fd7a:115c:a1e0::7".parse().unwrap();

    // VPN off: physical Wi-Fi and cellular CGNAT (100.64/10 on pdp_ip0) MUST fail closed.
    let carrier_cgnat = [
        InterfaceAddr {
            name: "en0".into(),
            index: 1,
            addr: "192.168.1.5".parse().unwrap(),
            point_to_point: false,
        },
        InterfaceAddr {
            name: "pdp_ip0".into(),
            index: 2,
            addr: "100.64.5.10".parse().unwrap(),
            point_to_point: true,
        },
    ];
    assert!(matches!(
        select_tailnet_interface(target_v4, &carrier_cgnat),
        Err(ConnectError::Unreachable(_))
    ));
    assert!(matches!(
        open_plain_with("ws://100.64.0.7:7297/v1/connect", SHORT, Some(&carrier_cgnat)),
        Err(ConnectError::Unreachable(_))
    ));
    assert!(matches!(
        open_plain_with("ws://100.64.0.7:7297/v1/connect", SHORT, Some(&[])),
        Err(ConnectError::Unreachable(_))
    ));

    #[cfg(target_vendor = "apple")]
    let (tun_a, tun_b) = ("utun4", "utun5");
    #[cfg(target_os = "linux")]
    let (tun_a, tun_b) = ("tailscale0", "tailscale1");

    #[cfg(any(target_vendor = "apple", target_os = "linux"))]
    {
        // Two tunnels hold 100.64/10, but only tun_a also carries Tailscale's unique ULA -> pick tun_a.
        let disambiguated = [
            InterfaceAddr {
                name: tun_b.into(),
                index: 10,
                addr: "100.64.99.2".parse().unwrap(),
                point_to_point: true,
            },
            InterfaceAddr {
                name: tun_a.into(),
                index: 11,
                addr: "100.64.0.1".parse().unwrap(),
                point_to_point: true,
            },
            InterfaceAddr {
                name: tun_a.into(),
                index: 11,
                addr: "fd7a:115c:a1e0::1".parse().unwrap(),
                point_to_point: true,
            },
        ];
        let picked = select_tailnet_interface(target_v4, &disambiguated).unwrap();
        assert_eq!(picked.name, tun_a);
        assert_eq!(picked.index, 11);
        assert_eq!(picked.local_ip, "100.64.0.1".parse::<IpAddr>().unwrap());

        let picked_v6 = select_tailnet_interface(target_v6, &disambiguated).unwrap();
        assert_eq!(picked_v6.name, tun_a);
        assert_eq!(picked_v6.local_ip, "fd7a:115c:a1e0::1".parse::<IpAddr>().unwrap());

        // Ambiguous: two 100.64/10 tunnels without Tailscale ULA -> fail closed.
        let ambiguous = [
            InterfaceAddr {
                name: tun_a.into(),
                index: 11,
                addr: "100.64.0.1".parse().unwrap(),
                point_to_point: true,
            },
            InterfaceAddr {
                name: tun_b.into(),
                index: 12,
                addr: "100.64.99.2".parse().unwrap(),
                point_to_point: true,
            },
        ];
        assert!(matches!(
            select_tailnet_interface(target_v4, &ambiguous),
            Err(ConnectError::Unreachable(_))
        ));

        // On Apple, a lone utun* carrying only 100.64/10 (without Tailscale's unique ULA) could
        // be an unrelated corporate/CGNAT VPN, so it must fail closed; on Linux, `tailscale0`
        // identifies Tailscale by interface name.
        let lone_v4_only = [InterfaceAddr {
            name: tun_a.into(),
            index: 11,
            addr: "100.64.0.1".parse().unwrap(),
            point_to_point: true,
        }];
        #[cfg(target_vendor = "apple")]
        assert!(matches!(
            select_tailnet_interface(target_v4, &lone_v4_only),
            Err(ConnectError::Unreachable(_))
        ));
        #[cfg(target_os = "linux")]
        assert_eq!(
            select_tailnet_interface(target_v4, &lone_v4_only)
                .unwrap()
                .name,
            tun_a
        );
    }
}

#[test]
fn errors_tell_what_to_check() {
    let e = ConnectError::Unreachable("connection refused".into()).to_string();
    assert!(e.contains("Tailscale connected on this device"), "{e}");
    assert!(!e.contains("Mac"), "error should be host-neutral: {e}");
    let f = ConnectError::Forbidden("denied".into()).to_string();
    assert!(!f.contains("Mac"), "error should be host-neutral: {f}");
    let u = ConnectError::DaemonUnavailable.to_string();
    assert!(!u.contains("Mac"), "error should be host-neutral: {u}");
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

#[test]
fn a_local_write_is_stalled_once_it_takes_nothing_for_the_bound() {
    let now = tokio::time::Instant::now();
    let idle = Duration::from_secs(45);
    assert!(!stalled(now, now + Duration::from_secs(44), idle));
    assert!(stalled(now, now + idle, idle));
}

#[test]
fn a_local_app_that_takes_nothing_fails_the_write_at_the_bound() {
    let rt = server_rt();
    // Nothing reads the other end, so the write can't go on; the peer stays open.
    let (mut ours, _theirs) = tokio::io::duplex(8);
    let started = Instant::now();
    let wrote = rt.block_on(write_progressing(
        &mut ours,
        &[0u8; 64],
        Duration::from_millis(300),
    ));
    assert!(!wrote);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn a_slow_local_app_that_keeps_reading_is_not_stalled() {
    let rt = server_rt();
    let (mut ours, mut theirs) = tokio::io::duplex(8);
    // 25 reads of 8 bytes, 60 ms apart: longer than the bound, but never idle for it.
    rt.spawn(async move {
        let mut buf = [0u8; 8];
        for _ in 0..25 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            theirs.read_exact(&mut buf).await.unwrap();
        }
    });
    let wrote = rt.block_on(write_progressing(
        &mut ours,
        &[7u8; 200],
        Duration::from_millis(300),
    ));
    assert!(wrote);
}
