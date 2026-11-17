use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;

use thurm_client::ClientError;
use thurm_client::stream::{self, StreamReader, StreamWriter};
use thurm_proto::tailnet::write_ingress;

use super::*;

/// A TCP server on loopback that runs `serve` on its one connection; its port.
fn target(serve: impl FnOnce(TcpStream) + Send + 'static) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (conn, _) = listener.accept().unwrap();
        serve(conn);
    });
    (port, server)
}

/// Echoes until the caller finishes, then finishes too.
fn echo(mut conn: TcpStream) {
    let mut rd = conn.try_clone().unwrap();
    std::io::copy(&mut rd, &mut conn).unwrap();
    conn.shutdown(Shutdown::Write).unwrap();
}

fn open(env: &Env, c: &Client, port: u16) -> (StreamReader, StreamWriter) {
    let ticket = c.dial_begin(port).unwrap();
    stream::open(&env.socket, &ticket.ticket).unwrap()
}

fn refused(r: Result<(StreamReader, StreamWriter), ClientError>) -> String {
    match r {
        Err(ClientError::Daemon(e)) => e,
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("the stream opened"),
    }
}

#[test]
fn the_daemon_says_it_carries_streams() {
    let env = Env::new("streams-cap");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    assert!(c.has_capability(CAP_STREAMS));
}

#[test]
fn a_stream_echoes_and_closes_cleanly() {
    let env = Env::new("streams-echo");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let (port, server) = target(echo);
    let (mut r, mut w) = open(&env, &c, port);
    w.write_all(b"ping").unwrap();
    let mut got = [0u8; 4];
    r.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"ping");
    w.write_all(b" pong").unwrap();
    w.finish().unwrap();
    let mut rest = String::new();
    r.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, " pong");
    r.wait_closed().unwrap();
    server.join().unwrap();
    // The stream's connection was never a pane client, and the control one still works.
    assert!(matches!(
        c.request(Request::ListPanes),
        Ok(Response::Panes(_))
    ));
}

#[test]
fn the_caller_finishing_leaves_the_target_sending() {
    let env = Env::new("streams-half-caller");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let (port, server) = target(|mut conn| {
        let mut got = Vec::new();
        conn.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"question");
        conn.write_all(b"answer after your end").unwrap();
    });
    let (mut r, mut w) = open(&env, &c, port);
    w.write_all(b"question").unwrap();
    w.finish().unwrap();
    let mut got = String::new();
    r.read_to_string(&mut got).unwrap();
    assert_eq!(got, "answer after your end");
    r.wait_closed().unwrap();
    server.join().unwrap();
}

#[test]
fn the_target_finishing_leaves_the_caller_sending() {
    let env = Env::new("streams-half-target");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (port, server) = target(move |mut conn| {
        conn.write_all(b"banner").unwrap();
        conn.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        conn.read_to_end(&mut got).unwrap();
        tx.send(got).unwrap();
    });
    let (mut r, mut w) = open(&env, &c, port);
    let mut got = String::new();
    r.read_to_string(&mut got).unwrap();
    assert_eq!(got, "banner");
    w.write_all(b"still talking").unwrap();
    w.finish().unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        b"still talking"
    );
    r.wait_closed().unwrap();
    server.join().unwrap();
}

#[test]
fn a_stream_carries_64_mib_intact() {
    const SIZE: usize = 64 * 1024 * 1024;
    let env = Env::new("streams-64m");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let (port, server) = target(echo);
    let (mut r, mut w) = open(&env, &c, port);
    let byte = |i: usize| (i % 251) as u8;
    let sender = std::thread::spawn(move || {
        for start in (0..SIZE).step_by(1024 * 1024) {
            let chunk: Vec<u8> = (start..start + 1024 * 1024).map(byte).collect();
            w.write_all(&chunk).unwrap();
        }
        w.finish().unwrap();
    });
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = 0;
    loop {
        let n = r.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        for (k, b) in buf[..n].iter().enumerate() {
            assert_eq!(*b, byte(got + k), "byte {}", got + k);
        }
        got += n;
    }
    assert_eq!(got, SIZE);
    sender.join().unwrap();
    r.wait_closed().unwrap();
    server.join().unwrap();
}

#[test]
fn a_port_nobody_listens_on_is_refused() {
    let env = Env::new("streams-refused");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (mut r, _w) = open(&env, &c, port);
    let e = r.read(&mut [0u8; 16]).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused, "{e}");
}

#[test]
fn a_ticket_opens_one_stream() {
    let env = Env::new("streams-reuse");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let (port, server) = target(echo);
    let ticket = c.dial_begin(port).unwrap();
    assert_eq!(ticket.ticket.len(), 64);
    let (r, mut w) = stream::open(&env.socket, &ticket.ticket).unwrap();
    let e = refused(stream::open(&env.socket, &ticket.ticket));
    assert!(e.contains("unknown or used"), "{e}");
    w.finish().unwrap();
    r.wait_closed().unwrap();
    server.join().unwrap();
    let e = refused(stream::open(&env.socket, "00"));
    assert!(e.contains("unknown or used"), "{e}");
}

#[test]
fn a_ticket_expires() {
    let env = Env::new("streams-expiry");
    let _daemon = env.start_with(&[("THURM_STREAM_TICKET_MS", Path::new("200"))]);
    let (c, _rx) = env.connect();
    let ticket = c.dial_begin(1).unwrap();
    assert_eq!(ticket.expires_ms, 200);
    std::thread::sleep(Duration::from_millis(400));
    let e = refused(stream::open(&env.socket, &ticket.ticket));
    assert!(e.contains("expired"), "{e}");
}

#[test]
fn another_callers_ticket_is_refused_and_used_up() {
    let env = Env::new("streams-principal");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let (port, _server) = target(echo);
    let ticket = c.dial_begin(port).unwrap();
    let s = UnixStream::connect(&env.socket).unwrap();
    let caller = Principal {
        kind: PrincipalKind::Tailscale,
        display: "someone@example.com".into(),
        user_id: Some(42),
        login_name: Some("someone@example.com".into()),
        via: Some("node".into()),
        ..Default::default()
    };
    write_ingress(&mut &s, &caller).unwrap();
    let e = refused(stream::open_on(s, &ticket.ticket));
    assert!(e.contains("another caller"), "{e}");
    let e = refused(stream::open(&env.socket, &ticket.ticket));
    assert!(e.contains("unknown or used"), "{e}");
}

#[test]
fn stream_open_is_refused_anywhere_but_right_after_hello() {
    let env = Env::new("streams-late-open");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let ticket = c.dial_begin(1).unwrap();
    // Third request on its connection: an error, and the connection goes on as before.
    c.request(Request::ListPanes).unwrap();
    let e = c
        .request(Request::StreamOpen {
            ticket: ticket.ticket.clone(),
        })
        .unwrap_err();
    assert!(e.to_string().contains("first request after Hello"), "{e}");
    assert!(matches!(
        c.request(Request::ListPanes),
        Ok(Response::Panes(_))
    ));
}

#[test]
fn pending_tickets_are_capped_per_client() {
    let env = Env::new("streams-cap-pending");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    for _ in 0..64 {
        c.dial_begin(1).unwrap();
    }
    let Err(ClientError::Daemon(e)) = c.dial_begin(1) else {
        panic!("a 65th ticket was given");
    };
    assert!(e.contains("too many"), "{e}");
    let (other, _rx) = env.connect();
    other.dial_begin(1).unwrap();
}

#[test]
fn a_ticket_goes_with_the_connection_that_reserved_it() {
    let env = Env::new("streams-gone");
    let _daemon = env.start();
    let (c, rx) = env.connect();
    // Fresh tickets: one that won't open can only have gone with its connection.
    let tickets: Vec<_> = (0..64).map(|_| c.dial_begin(1).unwrap().ticket).collect();
    drop(c);
    drop(rx);
    // The daemon notices the connection closing on its own time; until then a ticket opens
    // (onto a port nobody listens on).
    for ticket in &tickets {
        match stream::open(&env.socket, ticket) {
            Ok(_) => std::thread::sleep(Duration::from_millis(50)),
            Err(ClientError::Daemon(e)) => {
                assert!(e.contains("unknown or used"), "{e}");
                return;
            }
            Err(e) => panic!("{e}"),
        }
    }
    panic!("the tickets outlived their connection");
}
