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
fn a_link_to_one_loopback_dials_that_one() {
    use thurm_proto::stream::Loopback;
    let d = dir("family");
    let socket = d.join("d.sock");
    fakes::daemon(&socket);
    let theirs = socket.clone();
    let f = Forwards::new(d.join("forwards.json"), move |_| theirs.clone());
    f.check("devbox").unwrap();
    let dials = |port| {
        fakes::DIALS
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| *p == port)
            .map(|(_, at)| *at)
            .collect::<Vec<_>>()
    };
    let roundtrip = |local: u16| {
        let mut conn = TcpStream::connect(("127.0.0.1", local)).unwrap();
        conn.write_all(b"ping").unwrap();
        let mut got = [0u8; 4];
        conn.read_exact(&mut got).unwrap();
    };
    // The host has one program on 127.0.0.1:4000 and another on [::1]:4000.
    let (v6, _) = f
        .open_for("devbox", 4000, Some(0), Some("http://[::1]:4000/"))
        .unwrap();
    let (any, new) = f
        .open_for("devbox", 4000, Some(0), Some("http://localhost:4000/"))
        .unwrap();
    assert!(new, "a localhost link reused the [::1] forward");
    let (v4, new) = f
        .open_for("devbox", 4000, Some(0), Some("http://127.0.0.1:4000/"))
        .unwrap();
    assert!(new, "a 127.0.0.1 link reused another forward");
    assert_eq!(
        (v6.at, any.at, v4.at),
        (Loopback::V6, Loopback::Any, Loopback::V4)
    );
    roundtrip(v6.local);
    assert_eq!(dials(4000), [Loopback::V6]);
    roundtrip(v4.local);
    roundtrip(any.local);
    assert_eq!(dials(4000), [Loopback::V6, Loopback::V4, Loopback::Any]);
    // Opened again for an [::1] link: the [::1] forward, no other.
    let (again, new) = f
        .open_for("devbox", 4000, Some(0), Some("http://[::1]:4000/x"))
        .unwrap();
    assert!(!new);
    assert_eq!(again.id, v6.id);
    for fw in [v6, any, v4] {
        f.close(fw.id).unwrap();
    }
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

/// A daemon that answers Hello and keeps each connection open until [`Downable::down`].
struct Downable {
    path: PathBuf,
    conns: std::sync::Arc<std::sync::Mutex<Vec<std::os::unix::net::UnixStream>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Downable {
    fn up(path: PathBuf) -> Downable {
        use thurm_proto::{Envelope, Request, Response, ServerMessage, codec};
        let ln = UnixListener::bind(&path).unwrap();
        let conns: std::sync::Arc<std::sync::Mutex<Vec<std::os::unix::net::UnixStream>>> =
            Default::default();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (kept, stopped) = (conns.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in ln.incoming() {
                if stopped.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let Ok(mut conn) = conn else { return };
                kept.lock().unwrap().push(conn.try_clone().unwrap());
                std::thread::spawn(move || {
                    while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut conn) {
                        let result = match env.request {
                            Request::Hello { .. } => Ok(Response::Hello {
                                version: thurm_proto::PROTOCOL_VERSION,
                                daemon_pid: std::process::id(),
                                restored: false,
                                build: thurm_proto::BUILD.into(),
                                capabilities: thurm_proto::CAPABILITIES
                                    .iter()
                                    .map(|c| (*c).to_owned())
                                    .collect(),
                            }),
                            _ => Ok(Response::Ok),
                        };
                        let msg = ServerMessage::Response { id: env.id, result };
                        if codec::write_message(&mut conn, &msg).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Downable { path, conns, stop }
    }

    /// The tunnel goes down: its connections end, and nothing answers on its socket.
    fn down(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = std::os::unix::net::UnixStream::connect(&self.path);
        for c in self.conns.lock().unwrap().drain(..) {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn a_dropped_connection_is_not_taken_for_a_live_host() {
    let d = dir("dropped");
    let socket = d.join("d.sock");
    let daemon = Downable::up(socket.clone());
    let theirs = socket.clone();
    let f = Forwards::new(d.join("forwards.json"), move |_| theirs.clone());
    f.check("devbox").unwrap();
    assert!(f.connected("devbox"));
    daemon.down();
    // The kept connection notices it is gone.
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.check("devbox").is_ok() {
        assert!(
            Instant::now() < deadline,
            "a dropped connection still passes the check"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let e = f.check("devbox").unwrap_err();
    assert!(e.contains("not connected"), "{e}");
    // Back up: the check makes a new connection.
    let _again = Downable::up(socket.clone());
    f.check("devbox").unwrap();
    let _ = std::fs::remove_dir_all(&d);
}

/// A daemon that answers Hello after `delay` and counts the dials it is asked for.
fn slow_daemon(path: PathBuf, delay: Duration) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    use thurm_proto::{Envelope, Request, Response, ServerMessage, codec};
    let dials = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = dials.clone();
    let ln = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for conn in ln.incoming() {
            let Ok(mut conn) = conn else { return };
            let counted = counted.clone();
            std::thread::spawn(move || {
                while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut conn) {
                    let result = match env.request {
                        Request::Hello { .. } => {
                            std::thread::sleep(delay);
                            Ok(Response::Hello {
                                version: thurm_proto::PROTOCOL_VERSION,
                                daemon_pid: std::process::id(),
                                restored: false,
                                build: thurm_proto::BUILD.into(),
                                capabilities: thurm_proto::CAPABILITIES
                                    .iter()
                                    .map(|c| (*c).to_owned())
                                    .collect(),
                            })
                        }
                        Request::DialBegin { port } => {
                            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            Ok(Response::Ticket {
                                ticket: format!("ticket-{port}"),
                                expires_ms: 60_000,
                            })
                        }
                        _ => Ok(Response::Ok),
                    };
                    let msg = ServerMessage::Response { id: env.id, result };
                    if codec::write_message(&mut conn, &msg).is_err() {
                        return;
                    }
                }
            });
        }
    });
    dials
}

#[test]
fn a_connection_to_a_closed_forward_asks_the_host_for_nothing() {
    let d = dir("no-dial");
    let socket = d.join("slow.sock");
    let dials = slow_daemon(socket.clone(), Duration::from_millis(400));
    let theirs = socket.clone();
    let f = Forwards::new(d.join("forwards.json"), move |_| theirs.clone());
    let (fw, _) = f.open("devbox", 5176, Some(0)).unwrap();
    // A connection comes in; while its worker still waits for the daemon, the forward closes.
    let _conn = TcpStream::connect(("127.0.0.1", fw.local)).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    f.close(fw.id).unwrap();
    // Well after the daemon answered: it was never asked to dial the port.
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(
        dials.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a closed forward dialled the host"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A daemon whose streams take the data and never answer or end (an idle service): it reports
/// when a stream's connection goes away.
fn idle_daemon(path: PathBuf) -> std::sync::mpsc::Receiver<()> {
    use thurm_proto::{Envelope, Request, Response, ServerMessage, codec};
    let (gone_tx, gone_rx) = std::sync::mpsc::channel();
    let ln = UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for conn in ln.incoming() {
            let Ok(mut conn) = conn else { return };
            let gone_tx = gone_tx.clone();
            std::thread::spawn(move || {
                while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut conn) {
                    let stream = matches!(env.request, Request::StreamOpen { .. });
                    let result = match env.request {
                        Request::Hello { .. } => Ok(Response::Hello {
                            version: thurm_proto::PROTOCOL_VERSION,
                            daemon_pid: std::process::id(),
                            restored: false,
                            build: thurm_proto::BUILD.into(),
                            capabilities: thurm_proto::CAPABILITIES
                                .iter()
                                .map(|c| (*c).to_owned())
                                .collect(),
                        }),
                        Request::DialBegin { port } => Ok(Response::Ticket {
                            ticket: format!("ticket-{port}"),
                            expires_ms: 60_000,
                        }),
                        _ => Ok(Response::Ok),
                    };
                    let msg = ServerMessage::Response { id: env.id, result };
                    if codec::write_message(&mut conn, &msg).is_err() {
                        return;
                    }
                    if stream {
                        // Take frames, End included, and never say anything back.
                        while let Ok(Some(_)) = thurm_proto::stream::read_frame(&mut conn) {}
                        let _ = gone_tx.send(());
                        return;
                    }
                }
            });
        }
    });
    gone_rx
}

#[test]
fn closing_ends_a_stream_whose_client_had_finished_sending() {
    let d = dir("half");
    let socket = d.join("idle.sock");
    let gone = idle_daemon(socket.clone());
    let theirs = socket.clone();
    let f = Forwards::new(d.join("forwards.json"), move |_| theirs.clone());
    let (fw, _) = f.open("devbox", 5177, Some(0)).unwrap();
    // A client sends its request and ends its side; the service on the host says nothing yet.
    let mut conn = TcpStream::connect(("127.0.0.1", fw.local)).unwrap();
    conn.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    conn.shutdown(std::net::Shutdown::Write).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    // Closing the forward ends the host's side too.
    f.close(fw.id).unwrap();
    assert!(
        gone.recv_timeout(Duration::from_secs(3)).is_ok(),
        "the stream to the host outlived its forward"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_connection_made_while_the_host_moved_is_not_kept() {
    let d = dir("moving-control");
    let socket = d.join("slow.sock");
    let _dials = slow_daemon(socket.clone(), Duration::from_millis(400));
    let theirs = socket.clone();
    let f = Forwards::new(d.join("forwards.json"), move |_| theirs.clone());
    // A check starts connecting to the host's daemon (slowly); the host is pointed elsewhere
    // meanwhile.
    let checking = {
        let f = f.clone();
        std::thread::spawn(move || f.check("devbox"))
    };
    std::thread::sleep(Duration::from_millis(100));
    f.host_moved("devbox");
    let r = checking.join().unwrap();
    assert!(r.is_err(), "a check across a move passed: {r:?}");
    assert!(
        !f.connected("devbox"),
        "a connection made for the old destination was kept"
    );
    let _ = std::fs::remove_dir_all(&d);
}
