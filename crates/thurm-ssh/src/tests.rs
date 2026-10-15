//! Against an in-process russh server that runs exec requests with the local `sh` and forwards
//! streamlocal channels to local sockets.

use std::io::Write as _;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Mutex;

use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Session as ServerSession};
use russh::{Channel, ChannelId};
use thurm_proto::{Envelope, Request, Response, ServerMessage, codec};

use super::*;

struct TestKey(SigningKey);

impl TestKey {
    fn new(seed: u8) -> Arc<TestKey> {
        Arc::new(TestKey(SigningKey::from_slice(&[seed; 32]).unwrap()))
    }
}

impl DeviceKey for TestKey {
    fn public_sec1(&self) -> Vec<u8> {
        self.0
            .verifying_key()
            .to_sec1_point(false)
            .as_bytes()
            .to_vec()
    }

    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        let sig: Signature = self.0.sign(data);
        Ok(sig.to_bytes().to_vec())
    }
}

fn host_key() -> PrivateKey {
    PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[7; 32])),
        "test host",
    )
    .unwrap()
}

#[derive(Clone)]
struct Server {
    allowed: PublicKey,
}

impl server::Handler for Server {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if key.key_data() == self.allowed.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(String::from_utf8_lossy(data).into_owned())
            .output()
            .unwrap();
        session.data(channel, out.stdout)?;
        session.extended_data(channel, 1, out.stderr)?;
        session.exit_status_request(channel, out.status.code().unwrap_or(255) as u32)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn channel_open_direct_streamlocal(
        &mut self,
        channel: Channel<Msg>,
        socket_path: &str,
        reply: server::ChannelOpenHandle,
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        let Ok(mut local) = tokio::net::UnixStream::connect(socket_path).await else {
            reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };
        reply.accept().await;
        tokio::spawn(async move {
            let mut channel = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut local, &mut channel).await;
        });
        Ok(())
    }
}

/// An ssh server on a local port accepting `device`; its runtime lives as long as the value.
struct TestServer {
    port: u16,
    _rt: tokio::runtime::Runtime,
}

fn serve(device: &TestKey) -> TestServer {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let allowed = public_key(device).unwrap();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = Arc::new(server::Config {
        keys: vec![host_key()],
        auth_rejection_time: Duration::from_millis(10),
        ..Default::default()
    });
    rt.spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let handler = Server {
                allowed: allowed.clone(),
            };
            let config = config.clone();
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, tcp, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    TestServer { port, _rt: rt }
}

fn target(server: &TestServer) -> Target {
    Target {
        host: "127.0.0.1".into(),
        port: server.port,
        user: "me".into(),
        host_keys: vec![
            host_key()
                .public_key()
                .fingerprint(HashAlg::Sha256)
                .to_string(),
        ],
        socket: None,
    }
}

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
    assert!(start.elapsed() < Duration::from_secs(10), "{:?}", start.elapsed());
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
                        capabilities: vec![],
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
