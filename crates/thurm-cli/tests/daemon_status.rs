use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use thurm_proto::{Envelope, Request, Response, ServerMessage, codec};

struct TestSocket {
    dir: PathBuf,
    socket: PathBuf,
}

impl TestSocket {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("thds-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s");
        Self { dir, socket }
    }

    fn status(&self) -> Option<i32> {
        Command::new(env!("CARGO_BIN_EXE_thurm"))
            .env("THURM_SOCKET", &self.socket)
            .args(["daemon", "status"])
            .output()
            .unwrap()
            .status
            .code()
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

/// Answers the Hello, then returns the id of the `ListPanes` that follows.
fn hello_then_list(listener: UnixListener) -> (UnixStream, u64) {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let hello = request(&mut stream);
    assert!(matches!(hello.request, Request::Hello { .. }));
    codec::write_message(
        &mut stream,
        &ServerMessage::Response {
            id: hello.id,
            result: Ok(Response::Hello {
                version: thurm_proto::PROTOCOL_VERSION,
                daemon_pid: std::process::id(),
                restored: false,
                build: thurm_proto::BUILD.into(),
                capabilities: thurm_proto::CAPABILITIES
                    .iter()
                    .map(|c| (*c).to_owned())
                    .collect(),
            }),
        },
    )
    .unwrap();
    let list = request(&mut stream);
    assert!(matches!(list.request, Request::ListPanes));
    (stream, list.id)
}

#[test]
fn unreadable_pane_list_is_incompatible() {
    let env = TestSocket::new("unreadable");
    let listener = UnixListener::bind(&env.socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut s, id) = hello_then_list(listener);
        // An empty pane list whose length then claims one pane: undecodable, as the pane list
        // of a daemon whose `PaneInfo` has other fields.
        let mut frame = codec::encode(&ServerMessage::Response {
            id,
            result: Ok(Response::Panes(Vec::new())),
        })
        .unwrap();
        *frame.last_mut().unwrap() = 1;
        s.write_all(&frame).unwrap();
        let _ = codec::read_message::<_, Envelope>(&mut s);
    });
    assert_eq!(env.status(), Some(4));
    server.join().unwrap();
}

#[test]
fn dropped_connection_is_an_error_not_incompatible() {
    let env = TestSocket::new("dropped");
    let listener = UnixListener::bind(&env.socket).unwrap();
    let server = std::thread::spawn(move || {
        let (s, _) = hello_then_list(listener);
        drop(s);
    });
    assert_eq!(env.status(), Some(1));
    server.join().unwrap();
}
