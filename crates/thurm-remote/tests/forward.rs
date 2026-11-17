//! Forwards carrying real connections, through a stand-in daemon whose streams echo (see
//! `support/tailnet_fakes.rs`).

#[path = "support/tailnet_fakes.rs"]
#[allow(dead_code)]
mod fakes;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use thurm_remote::forward::Forwards;

fn dir(name: &str) -> PathBuf {
    // Short: a Unix socket path has a length limit.
    let d = PathBuf::from(format!("/tmp/tfw-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Whether the peer ended `conn` within a few seconds.
fn ended(conn: &mut TcpStream) -> bool {
    conn.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 64];
    while Instant::now() < deadline {
        match conn.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return true,
        }
    }
    false
}

#[test]
fn closing_a_forward_ends_its_open_connections() {
    let d = dir("close");
    let socket = d.join("d.sock");
    fakes::daemon(&socket);
    let theirs = socket.clone();
    let f = Forwards::new(d.join("forwards.json"), move |_| theirs.clone());
    f.check("devbox").unwrap();
    let (fw, _) = f.open("devbox", 5173, Some(0)).unwrap();

    let mut conn = TcpStream::connect(("127.0.0.1", fw.local)).unwrap();
    conn.write_all(b"ping").unwrap();
    let mut got = [0u8; 4];
    conn.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"ping", "carried through the stream");

    // Closed while the connection is open and idle: it ends, it doesn't keep its access.
    f.close(fw.id).unwrap();
    assert!(ended(&mut conn), "the connection outlived its forward");

    // The same when the host goes away.
    let (fw, _) = f.open("devbox", 5174, Some(0)).unwrap();
    let mut conn = TcpStream::connect(("127.0.0.1", fw.local)).unwrap();
    conn.write_all(b"pong").unwrap();
    conn.read_exact(&mut got).unwrap();
    f.retain_hosts(&[]);
    assert!(ended(&mut conn), "the connection outlived its host");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_forward_carries_a_bounded_number_of_connections() {
    let d = dir("cap");
    // A daemon that never answers: each connection's worker waits on it.
    let socket = d.join("mute.sock");
    let _mute = UnixListener::bind(&socket).unwrap();
    let theirs = socket.clone();
    let f = Forwards::new(d.join("forwards.json"), move |_| theirs.clone()).with_max_connections(2);
    let (fw, _) = f.open("devbox", 6000, Some(0)).unwrap();

    let mut held: Vec<TcpStream> = (0..2)
        .map(|_| TcpStream::connect(("127.0.0.1", fw.local)).unwrap())
        .collect();
    // Let the accept loop take both before the third arrives.
    std::thread::sleep(Duration::from_millis(300));
    let mut extra = TcpStream::connect(("127.0.0.1", fw.local)).unwrap();
    assert!(
        ended(&mut extra),
        "a connection over the limit got a worker"
    );
    for c in &mut held {
        c.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut b = [0u8; 1];
        let r = c.read(&mut b);
        assert!(
            matches!(&r, Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
            "a connection within the limit was dropped: {r:?}"
        );
    }
    f.close(fw.id).unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_host_that_moves_drops_its_connection_and_forwards() {
    let d = dir("moved");
    let socket = d.join("d.sock");
    fakes::daemon(&socket);
    let theirs = socket.clone();
    let f = Forwards::new(d.join("forwards.json"), move |_| theirs.clone());
    f.check("devbox").unwrap();
    assert!(f.connected("devbox"));
    let (fw, _) = f.open("devbox", 5175, Some(0)).unwrap();
    let mut conn = TcpStream::connect(("127.0.0.1", fw.local)).unwrap();
    conn.write_all(b"ping").unwrap();
    let mut got = [0u8; 4];
    conn.read_exact(&mut got).unwrap();

    // Its [[remote]] now points elsewhere: nothing opened for the old destination stays.
    f.host_moved("devbox");
    assert!(!f.connected("devbox"));
    assert!(f.list().is_empty());
    assert!(
        ended(&mut conn),
        "a connection to the old destination outlived the move"
    );
    let _ = std::fs::remove_dir_all(&d);
}
