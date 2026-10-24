use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use thurm_proto::{Envelope, Request, Response, ServerMessage, codec};

struct TestSocket {
    dir: PathBuf,
    socket: PathBuf,
}

impl TestSocket {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("thsend-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s");
        Self { dir, socket }
    }
}

impl Drop for TestSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Runs `thurm send --pane 7 ARGS` against a fake daemon and returns the requests after the
/// Hello, each with when it arrived.
fn send(name: &str, args: &[&str]) -> Vec<(Request, Instant)> {
    let env = TestSocket::new(name);
    let listener = UnixListener::bind(&env.socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut requests = Vec::new();
        while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut stream) {
            let at = Instant::now();
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
                request => {
                    requests.push((request, at));
                    Ok(Response::Ok)
                }
            };
            codec::write_message(&mut stream, &ServerMessage::Response { id: env.id, result })
                .unwrap();
        }
        requests
    });
    let status = Command::new(env!("CARGO_BIN_EXE_thurm"))
        .env("THURM_SOCKET", &env.socket)
        .args(["send", "--pane", "7"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
    server.join().unwrap()
}

fn assert_enter_after_pause(requests: &[(Request, Instant)]) {
    let (enter, enter_at) = &requests[1];
    assert!(matches!(enter, Request::Input { pane: 7, data } if data == b"\r"));
    assert!(
        enter_at.duration_since(requests[0].1) >= Duration::from_millis(100),
        "Enter followed the text without a pause"
    );
}

#[test]
fn enter_follows_typed_text_on_its_own() {
    let requests = send("plain", &["hello", "world"]);
    assert_eq!(requests.len(), 2);
    assert!(matches!(&requests[0].0, Request::Input { pane: 7, data } if data == b"hello world"));
    assert_enter_after_pause(&requests);
}

#[test]
fn enter_follows_a_paste_on_its_own() {
    let requests = send("paste", &["--paste", "a\nb"]);
    assert_eq!(requests.len(), 2);
    assert!(matches!(&requests[0].0, Request::Paste { pane: 7, text } if text == "a\nb"));
    assert_enter_after_pause(&requests);
}

#[test]
fn no_enter_sends_only_the_text() {
    let requests = send("no-enter", &["--no-enter", "partial"]);
    assert_eq!(requests.len(), 1);
    assert!(matches!(&requests[0].0, Request::Input { pane: 7, data } if data == b"partial"));
}

#[test]
fn empty_text_is_just_an_enter() {
    let started = Instant::now();
    let requests = send("empty", &[]);
    assert_eq!(requests.len(), 1);
    assert!(matches!(&requests[0].0, Request::Input { pane: 7, data } if data == b"\r"));
    assert!(started.elapsed() < Duration::from_secs(2));
}
