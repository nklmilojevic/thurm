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
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
}
