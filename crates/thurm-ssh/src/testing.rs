//! An in-process ssh server for tests: it runs exec requests with the local `sh`, runs shells
//! as a local `sh` (without a terminal), and forwards streamlocal channels to local sockets.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::keys::{HashAlg, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Session as ServerSession};
use russh::{Channel, ChannelId};

use crate::{DeviceKey, Target, public_key};

/// A device key in memory.
pub struct TestKey(SigningKey);

impl TestKey {
    pub fn new(seed: u8) -> Arc<TestKey> {
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

/// The test server's host key.
pub fn host_key() -> PrivateKey {
    PrivateKey::new(
        KeypairData::Ed25519(Ed25519Keypair::from_seed(&[7; 32])),
        "test host",
    )
    .unwrap()
}

#[derive(Clone)]
struct Server {
    allowed: PublicKey,
    /// Each running shell's input.
    shells: Arc<Mutex<HashMap<ChannelId, std::process::ChildStdin>>>,
    /// What the client asked for: the terminal type and every size (pty, then window changes).
    sizes: Arc<Mutex<Vec<(String, u32, u32)>>>,
    /// Each ended shell's exit code.
    exits: Arc<Mutex<Vec<i32>>>,
    /// Refuse terminals (`PermitTTY no`).
    no_pty: bool,
    /// A script each shell runs first, before reading commands.
    shell_init: Option<String>,
}

/// How a [`serve_with`] server differs from [`serve`]'s.
#[derive(Clone, Default)]
pub struct Options {
    /// Refuse terminals (sshd's `PermitTTY no`).
    pub no_pty: bool,
    /// A script each shell runs first (what a login shell's profile would print), then it
    /// reads commands as usual.
    pub shell_init: Option<String>,
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

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        if self.no_pty {
            session.channel_failure(channel)?;
            return Ok(());
        }
        self.sizes.lock().unwrap().push((term.into(), cols, rows));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _: ChannelId,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.sizes.lock().unwrap().push(("".into(), cols, rows));
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        use std::io::Read as _;
        use std::process::{Command, Stdio};
        session.channel_success(channel)?;
        let mut sh = Command::new("/bin/sh");
        if let Some(init) = &self.shell_init {
            sh.args(["-c", &format!("{init}\nexec /bin/sh")]);
        }
        let mut child = sh
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.shells
            .lock()
            .unwrap()
            .insert(channel, child.stdin.take().unwrap());
        let handle = session.handle();
        let exits = self.exits.clone();
        let mut stdout = child.stdout.take().unwrap();
        tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = stdout.read(&mut buf) {
                let data = buf[..n].to_vec();
                let h = handle.clone();
                let _ = tokio::runtime::Handle::current()
                    .block_on(async move { h.data(channel, data).await });
            }
            let code = child.wait().map(|s| s.code().unwrap_or(255)).unwrap_or(255);
            exits.lock().unwrap().push(code);
            tokio::runtime::Handle::current().block_on(async move {
                let _ = handle.exit_status_request(channel, code as u32).await;
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            });
        });
        Ok(())
    }

    /// The client is done with the shell: its input ends, so it exits.
    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.shells.lock().unwrap().remove(&channel);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.shells.lock().unwrap().remove(&channel);
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        if let Some(stdin) = self.shells.lock().unwrap().get_mut(&channel) {
            let _ = stdin.write_all(data);
        }
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
pub struct TestServer {
    pub port: u16,
    /// The terminal type and every size clients asked for (pty, then window changes).
    pub sizes: Arc<Mutex<Vec<(String, u32, u32)>>>,
    /// Each ended shell's exit code.
    pub exits: Arc<Mutex<Vec<i32>>>,
    _rt: tokio::runtime::Runtime,
}

pub fn serve(device: &TestKey) -> TestServer {
    serve_with(device, Options::default())
}

/// Like [`serve`], as `opts` say.
pub fn serve_with(device: &TestKey, opts: Options) -> TestServer {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let allowed = public_key(device).unwrap();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let seen = sizes.clone();
    let exits = Arc::new(Mutex::new(Vec::new()));
    let ended = exits.clone();
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
                shells: Default::default(),
                sizes: seen.clone(),
                exits: ended.clone(),
                no_pty: opts.no_pty,
                shell_init: opts.shell_init.clone(),
            };
            let config = config.clone();
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, tcp, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    TestServer {
        port,
        sizes,
        exits,
        _rt: rt,
    }
}

/// A target for `server`, with its host key trusted.
pub fn target(server: &TestServer) -> Target {
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
