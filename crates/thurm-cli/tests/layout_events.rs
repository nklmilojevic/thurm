use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use thurm_proto::{Envelope, Event, Request, Response, ServerMessage, codec};

struct TestSocket {
    dir: PathBuf,
    socket: PathBuf,
}

impl TestSocket {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("thle-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s");
        Self { dir, socket }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thurm"));
        c.env("THURM_SOCKET", &self.socket);
        c
    }

    fn listen(&self) -> UnixListener {
        UnixListener::bind(&self.socket).unwrap()
    }
}

impl Drop for TestSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn request(stream: &mut UnixStream) -> Envelope {
    codec::read_message(stream).unwrap().unwrap()
}

fn answer(stream: &mut UnixStream, id: u64, result: Result<Response, String>) {
    codec::write_message(stream, &ServerMessage::Response { id, result }).unwrap();
}

fn accept(listener: UnixListener) -> UnixStream {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let hello = request(&mut stream);
    assert!(matches!(hello.request, Request::Hello { .. }));
    answer(
        &mut stream,
        hello.id,
        Ok(Response::Hello {
            version: thurm_proto::PROTOCOL_VERSION,
            daemon_pid: std::process::id(),
            restored: false,
            build: thurm_proto::BUILD.into(),
            capabilities: Vec::new(),
        }),
    );
    stream
}

#[test]
fn apply_preserves_split_and_rolls_back_after_ui_failure() {
    let env = TestSocket::new("rollback");
    let file = env.dir.join("layout.json");
    std::fs::write(&file, r#"{"version":1,"title":"tests","root":{"type":"split","dir":"down","ratio":0.3,"first":{"type":"pane","command":["printf","%s","$(literal)"]},"second":{"type":"pane"}}}"#).unwrap();
    let listener = env.listen();
    let server = std::thread::spawn(move || {
        let mut s = accept(listener);
        let check = request(&mut s);
        assert!(matches!(check.request, Request::CheckUi));
        answer(&mut s, check.id, Ok(Response::Ok));
        for pane in [31, 32] {
            let req = request(&mut s);
            let Request::CreatePane(p) = req.request else {
                panic!("expected creation")
            };
            if pane == 31 {
                assert_eq!(p.command.unwrap(), ["printf", "%s", "$(literal)"]);
            }
            answer(&mut s, req.id, Ok(Response::PaneCreated { pane }));
        }
        let req = request(&mut s);
        let Request::ApplyLayout { json, .. } = req.request else {
            panic!("expected layout")
        };
        let tab: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(tab["root"]["ratio"], 0.3);
        assert_eq!(tab["root"]["second"]["id"], 32);
        answer(&mut s, req.id, Err("window closed".into()));
        for pane in [31, 32] {
            let req = request(&mut s);
            assert!(matches!(req.request, Request::ClosePane { pane: id } if id == pane));
            answer(&mut s, req.id, Ok(Response::Ok));
        }
    });
    let output = env
        .command()
        .args(["layout", "apply"])
        .arg(file)
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("window closed"));
}

#[test]
fn invalid_template_starts_no_process() {
    let env = TestSocket::new("invalid");
    let file = env.dir.join("layout.json");
    std::fs::write(
        &file,
        r#"{"version":1,"root":{"type":"pane","command":[]}}"#,
    )
    .unwrap();
    let listener = env.listen();
    let server = std::thread::spawn(move || {
        let mut s = accept(listener);
        assert!(
            codec::read_message::<_, Envelope>(&mut s)
                .unwrap()
                .is_none()
        );
    });
    let output = env
        .command()
        .args(["layout", "apply"])
        .arg(file)
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
}

#[test]
fn events_filter_metadata_and_fail_on_disconnect() {
    let env = TestSocket::new("events");
    let listener = env.listen();
    let server = std::thread::spawn(move || {
        let mut s = accept(listener);
        for event in [
            Event::PaneClosed { pane: 4 },
            Event::PaneClosed { pane: 5 },
            Event::Output {
                pane: 4,
                data: b"terminal text".to_vec(),
            },
        ] {
            codec::write_message(&mut s, &ServerMessage::Event(event)).unwrap();
        }
    });
    let output = env
        .command()
        .args(["events", "--json", "--pane", "4"])
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        r#"{"PaneClosed":{"pane":4}}"#
    );
}
