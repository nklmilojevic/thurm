//! Against the in-process server of [`crate::testing`].

use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Mutex;

use thurm_proto::{Envelope, Request, Response, ServerMessage, codec};

use super::*;
use crate::testing::{TestKey, serve, serve_with, target};

fn temp_dir(name: &str) -> PathBuf {
    // Short: socket paths are limited to 104 bytes.
    let dir = PathBuf::from(format!("/tmp/thurm-ssh-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn runs_remote_scripts_with_their_exit_codes() {
    let key = TestKey::new(1);
    let server = serve(&key);
    let s = Session::connect(&target(&server), key, Duration::from_secs(5)).unwrap();
    let out = s
        .run(
            "echo \"$1 $2\"",
            &["hello", "there"],
            None,
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(out, "hello there\n");
    let err = s
        .run("echo oops >&2; exit 3", &[], None, Duration::from_secs(5))
        .unwrap_err();
    assert_eq!(
        err,
        SshError::Remote {
            code: 3,
            stderr: "oops\n".into()
        }
    );
}

#[test]
fn refuses_a_host_key_other_than_the_paired_one() {
    let key = TestKey::new(1);
    let server = serve(&key);
    let mut t = target(&server);
    t.host_keys = vec!["SHA256:somethingElseEntirely".into()];
    let Err(SshError::Attention(e)) = Session::connect(&t, key, Duration::from_secs(5)) else {
        panic!("connected to an unpaired host key");
    };
    assert!(e.contains("is not the one paired"), "{e}");
}

#[test]
fn tells_an_untrusted_host_key_from_a_changed_one() {
    let key = TestKey::new(1);
    let server = serve(&key);
    let expected = target(&server).host_keys[0].clone();
    let mut t = target(&server);
    t.host_keys.clear();
    match Session::connect_checked(&t, key.clone(), Duration::from_secs(5)) {
        Err(SessionError::UnknownHostKey(seen)) => assert_eq!(seen, expected),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("connected without a trusted host key"),
    }
    t.host_keys = vec!["SHA256:somethingElseEntirely".into()];
    match Session::connect_checked(&t, key, Duration::from_secs(5)) {
        Err(SessionError::HostKeyChanged(seen)) => assert_eq!(seen, expected),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("connected to a changed host key"),
    }
    match Session::connect_checked(&target(&server), TestKey::new(2), Duration::from_secs(5)) {
        Err(SessionError::KeyRejected) => {}
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("an unknown key was accepted"),
    }
}

#[test]
fn runs_an_interactive_shell() {
    let key = TestKey::new(1);
    let server = serve(&key);
    let s = Session::connect(&target(&server), key, Duration::from_secs(5)).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let shell = s
        .open_shell((80, 24), "xterm-256color", &[], move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
    shell.write(b"echo hello from the shell\n");
    let mut out = Vec::new();
    while !String::from_utf8_lossy(&out).contains("hello from the shell") {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            ShellEvent::Data(d) => out.extend(d),
            ShellEvent::Exit(c) => panic!("the shell ended ({c:?})"),
        }
    }
    shell.resize(100, 30);
    shell.write(b"exit 4\n");
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            ShellEvent::Data(_) => {}
            ShellEvent::Exit(code) => {
                assert_eq!(code, Some(4));
                break;
            }
        }
    }
    let sizes = server.sizes.lock().unwrap().clone();
    assert_eq!(
        sizes,
        [("xterm-256color".into(), 80, 24), ("".into(), 100, 30)]
    );
}

#[test]
fn closing_a_shell_ends_it_and_keeps_the_session() {
    let key = TestKey::new(1);
    let server = serve(&key);
    let s = Session::connect(&target(&server), key, Duration::from_secs(5)).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let shell = s
        .open_shell((80, 24), "xterm-256color", &[], move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
    shell.write(b"echo ready\n");
    let mut out = Vec::new();
    while !String::from_utf8_lossy(&out).contains("ready") {
        if let ShellEvent::Data(d) = rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            out.extend(d);
        }
    }
    shell.close();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.exits.lock().unwrap().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the shell kept running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!s.is_closed());
    assert_eq!(
        s.exec("echo still", Duration::from_secs(5)).unwrap().1,
        "still\n"
    );
}

#[test]
fn a_refused_terminal_fails_the_shell() {
    let key = TestKey::new(1);
    let server = serve_with(&key, true);
    let s = Session::connect(&target(&server), key, Duration::from_secs(5)).unwrap();
    let Err(SshError::Attention(e)) = s.open_shell((80, 24), "xterm-256color", &[], |_| {}) else {
        panic!("a shell without a terminal");
    };
    assert!(e.contains("refused a terminal"), "{e}");
}

#[test]
fn execs_a_command_line_as_given() {
    let key = TestKey::new(1);
    let server = serve(&key);
    let s = Session::connect(&target(&server), key, Duration::from_secs(5)).unwrap();
    let (code, out) = s
        .exec("echo out; echo err >&2; exit 2", Duration::from_secs(5))
        .unwrap();
    assert_eq!(code, Some(2));
    assert!(out.contains("out") && out.contains("err"), "{out}");
}

/// A key whose signing never returns (a Face ID prompt nobody answers).
struct StuckKey(Arc<TestKey>);

impl DeviceKey for StuckKey {
    fn public_sec1(&self) -> Vec<u8> {
        self.0.public_sec1()
    }

    fn sign(&self, _: &[u8]) -> Result<Vec<u8>, String> {
        std::thread::sleep(Duration::from_secs(3600));
        Err("woke up".into())
    }
}

#[test]
fn a_sign_in_that_never_finishes_times_out() {
    let key = TestKey::new(1);
    let server = serve(&key);
    let start = std::time::Instant::now();
    let r = Session::connect(
        &target(&server),
        Arc::new(StuckKey(key)),
        Duration::from_secs(1),
    );
    assert!(matches!(r, Err(SshError::Transient(_))), "{:?}", r.err());
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn reports_a_device_key_the_host_does_not_know() {
    let server = serve(&TestKey::new(1));
    let Err(SshError::Attention(e)) =
        Session::connect(&target(&server), TestKey::new(2), Duration::from_secs(5))
    else {
        panic!("an unknown key was accepted");
    };
    assert!(e.contains("did not accept this device's key"), "{e}");
}

#[test]
fn authorized_key_line_is_openssh() {
    let line = authorized_key(TestKey::new(1).as_ref(), "thurm-ios:phone").unwrap();
    assert!(line.starts_with("ecdsa-sha2-nistp256 AAAA"), "{line}");
    assert!(line.ends_with(" thurm-ios:phone"), "{line}");
}

#[test]
fn mpints_are_minimal_and_positive() {
    let mut out = Vec::new();
    put_mpint(&mut out, &[0, 0, 0x12, 0x34]);
    assert_eq!(out, [0, 0, 0, 2, 0x12, 0x34]);
    out.clear();
    put_mpint(&mut out, &[0x80, 1]);
    assert_eq!(out, [0, 0, 0, 3, 0, 0x80, 1]);
    out.clear();
    put_mpint(&mut out, &[0, 0]);
    assert_eq!(out, [0, 0, 0, 0]);
}

/// A daemon that answers Hello and records the client's capabilities.
fn fake_daemon(socket: &std::path::Path) -> Arc<Mutex<Vec<String>>> {
    let listener = UnixListener::bind(socket).unwrap();
    let caps = Arc::new(Mutex::new(Vec::new()));
    let seen = caps.clone();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        while let Ok(Some(frame)) = codec::read_frame(&mut stream) {
            let env: Envelope = codec::decode(&frame).unwrap();
            let result = match env.request {
                Request::Hello { capabilities, .. } => {
                    *seen.lock().unwrap() = capabilities;
                    Ok(Response::Hello {
                        version: thurm_proto::PROTOCOL_VERSION,
                        daemon_pid: 1,
                        restored: false,
                        build: "test".into(),
                        capabilities: thurm_proto::CAPABILITIES
                            .iter()
                            .map(|c| (*c).to_owned())
                            .collect(),
                    })
                }
                _ => Ok(Response::Ok),
            };
            let msg = ServerMessage::Response { id: env.id, result };
            stream.write_all(&codec::encode(&msg).unwrap()).unwrap();
        }
    });
    caps
}

#[test]
fn reaches_the_daemon_socket_as_a_mobile_client() {
    let dir = temp_dir("socket");
    let socket = dir.join("d.sock");
    let caps = fake_daemon(&socket);
    let key = TestKey::new(1);
    let server = serve(&key);
    let s = Session::connect(&target(&server), key, Duration::from_secs(5)).unwrap();
    let stream = s.open_socket(socket.to_str().unwrap()).unwrap();
    let client = Client::from_stream(
        stream,
        "ios-test",
        false,
        &[thurm_proto::CAP_MOBILE],
        |_| {},
        || {},
    )
    .unwrap();
    assert!(matches!(
        client.request(Request::ListPanes),
        Ok(Response::Ok)
    ));
    assert!(
        caps.lock()
            .unwrap()
            .iter()
            .any(|c| c == thurm_proto::CAP_MOBILE)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn connect_reports_a_host_without_thurm() {
    let key = TestKey::new(1);
    let server = serve(&key);
    let mut t = target(&server);
    t.socket = None;
    let probe = Session::connect(&t, key.clone(), Duration::from_secs(5)).unwrap();
    let out = probe
        .run(
            "command -v thurm >/dev/null 2>&1 && echo found; true",
            &[],
            None,
            Duration::from_secs(5),
        )
        .unwrap();
    if out.contains("found") {
        // This machine has Thurm installed; the not-installed path can't be shown here.
        return;
    }
    match connect(&t, key, "ios-test", |_| {}, || {}) {
        Err(ConnectError::NotInstalled) => {}
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("connected without a daemon"),
    }
}
