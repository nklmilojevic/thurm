//! An ssh client inside the app, for the iOS app, which cannot run the system `ssh`.
//!
//! One [`Session`] per host. Remote scripts run on exec channels ([`RemoteExec`], the same
//! probe and daemon start as the desktop's tunnels), and the daemon's socket is reached
//! through a `direct-streamlocal@openssh.com` channel relayed to one end of a
//! [`UnixStream::pair`], whose other end goes to [`thurm_client::Client::from_stream`].
//!
//! The only key is the device's ([`DeviceKey`], a P-256 key in the Secure Enclave): signing is
//! done by the app, never here. Host keys are pinned (SHA-256 fingerprints from pairing, or
//! trusted by the user on first sight for a host added by hand: [`SessionError`]).
//!
//! A host without Thurm gets interactive shells on session channels ([`Session::open_shell`]).

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult, Handle};
use russh::keys::ssh_key::public::{EcdsaPublicKey, KeyData};
use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use thurm_client::{Client, ClientError};
use thurm_proto::Event;
use thurm_remote::{HostInfo, RemoteExec, SshError};
use tokio::sync::mpsc;

/// The device's ssh key. The private half never leaves the app's keystore.
pub trait DeviceKey: Send + Sync + 'static {
    /// The public key as an uncompressed SEC1 point (65 bytes, CryptoKit's
    /// `x963Representation`).
    fn public_sec1(&self) -> Vec<u8>;
    /// ECDSA P-256 / SHA-256 signature of `data` as `r || s` (64 bytes, CryptoKit's
    /// `rawRepresentation`). May ask the user (Face ID).
    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, String>;
}

/// A paired host.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// The host keys from pairing, as `SHA256:<base64>` fingerprints (`ssh-keygen -lf`).
    pub host_keys: Vec<String>,
    /// The daemon's socket there; the host's default when unset.
    #[serde(default)]
    pub socket: Option<String>,
}

/// The device's public key in `authorized_keys` form, for pairing.
pub fn authorized_key(key: &dyn DeviceKey, comment: &str) -> Result<String, SshError> {
    let mut public = public_key(key)?;
    public.set_comment(comment);
    public
        .to_openssh()
        .map_err(|e| SshError::Attention(format!("device key: {e}")))
}

fn public_key(key: &dyn DeviceKey) -> Result<PublicKey, SshError> {
    let point = EcdsaPublicKey::from_sec1_bytes(&key.public_sec1())
        .map_err(|e| SshError::Attention(format!("device key: {e}")))?;
    Ok(PublicKey::new(KeyData::Ecdsa(point), ""))
}

struct Handler {
    pinned: Vec<String>,
    /// The fingerprint the host showed when it was not one of `pinned`.
    mismatch: Arc<std::sync::Mutex<Option<String>>>,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.mismatch.lock().unwrap() = Some("a host certificate".into());
            return Ok(false);
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let known = self.pinned.contains(&fingerprint);
        if !known {
            *self.mismatch.lock().unwrap() = Some(fingerprint);
        }
        Ok(known)
    }
}

/// Signs with the [`DeviceKey`] the way an ssh agent would: the request data followed by the
/// signature blob.
struct Signer(Arc<dyn DeviceKey>);

#[derive(Debug)]
struct SignError(String);

impl From<russh::SendError> for SignError {
    fn from(e: russh::SendError) -> Self {
        SignError(e.to_string())
    }
}

impl russh::Signer for Signer {
    type Error = SignError;

    fn auth_sign(
        &mut self,
        _key: &russh::keys::agent::AgentIdentity,
        _hash_alg: Option<HashAlg>,
        to_sign: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        let key = self.0.clone();
        async move {
            let raw = tokio::task::spawn_blocking(move || key.sign(&to_sign).map(|s| (to_sign, s)))
                .await
                .map_err(|e| SignError(e.to_string()))?;
            let (mut data, sig) = raw.map_err(SignError)?;
            append_signature(&mut data, &sig).map_err(SignError)?;
            Ok(data)
        }
    }
}

/// `string(signature)` where the signature is `string("ecdsa-sha2-nistp256")` followed by
/// `string(mpint r || mpint s)` (RFC 5656 §3.1.2).
fn append_signature(data: &mut Vec<u8>, raw: &[u8]) -> Result<(), String> {
    if raw.len() != 64 {
        return Err(format!(
            "the device key returned a {}-byte signature",
            raw.len()
        ));
    }
    let mut rs = Vec::with_capacity(74);
    put_mpint(&mut rs, &raw[..32]);
    put_mpint(&mut rs, &raw[32..]);
    let mut blob = Vec::with_capacity(rs.len() + 31);
    put_string(&mut blob, b"ecdsa-sha2-nistp256");
    put_string(&mut blob, &rs);
    put_string(data, &blob);
    Ok(())
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// A non-negative SSH mpint: no leading zero bytes, but one when the top bit is set.
fn put_mpint(out: &mut Vec<u8>, n: &[u8]) {
    let n = &n[n.iter().position(|&b| b != 0).unwrap_or(n.len())..];
    let pad = n.first().is_some_and(|&b| b & 0x80 != 0);
    out.extend_from_slice(&((n.len() + usize::from(pad)) as u32).to_be_bytes());
    if pad {
        out.push(0);
    }
    out.extend_from_slice(n);
}

/// A connected, authenticated ssh session. Blocking API; the session runs on its own runtime.
pub struct Session {
    rt: tokio::runtime::Runtime,
    handle: Handle<Handler>,
}

impl Session {
    /// Connecting and authenticating (a Face ID prompt included) each get `timeout`.
    pub fn connect(
        target: &Target,
        key: Arc<dyn DeviceKey>,
        timeout: Duration,
    ) -> Result<Session, SshError> {
        Session::connect_checked(target, key, timeout).map_err(SessionError::into_ssh)
    }

    /// [`Session::connect`], telling apart the failures a host added by hand needs the user for:
    /// a host key not yet trusted, one that changed, and the device key not authorized there.
    pub fn connect_checked(
        target: &Target,
        key: Arc<dyn DeviceKey>,
        timeout: Duration,
    ) -> Result<Session, SessionError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("thurm-ssh")
            .enable_all()
            .build()
            .map_err(|e| SshError::Attention(format!("ssh runtime: {e}")))?;
        let pinned_any = !target.host_keys.is_empty();
        let mismatch = Arc::new(std::sync::Mutex::new(None));
        let handler = Handler {
            pinned: target.host_keys.clone(),
            mismatch: mismatch.clone(),
        };
        let config = Arc::new(client::Config {
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 3,
            ..Default::default()
        });
        let public = public_key(key.as_ref())?;
        let addr = (target.host.clone(), target.port);
        let user = target.user.clone();
        let result = rt.block_on(async move {
            let connect = client::connect(config, addr, handler);
            let mut handle = match tokio::time::timeout(timeout, connect).await {
                Err(_) => {
                    return Err(SshError::Transient("ssh: connection timed out".into()).into());
                }
                Ok(Err(e)) => {
                    if let Some(seen) = mismatch.lock().unwrap().take() {
                        return Err(if pinned_any {
                            SessionError::HostKeyChanged(seen)
                        } else {
                            SessionError::UnknownHostKey(seen)
                        });
                    }
                    return Err(SshError::Transient(format!("ssh: {e}")).into());
                }
                Ok(Ok(h)) => h,
            };
            let mut signer = Signer(key);
            let auth = handle.authenticate_publickey_with(user, public, None, &mut signer);
            let auth = match tokio::time::timeout(timeout, auth).await {
                Err(_) => {
                    return Err(SshError::Transient(
                        "ssh: the host or this device's key did not finish signing in".into(),
                    )
                    .into());
                }
                Ok(r) => {
                    r.map_err(|SignError(e)| SshError::Attention(format!("ssh: signing: {e}")))?
                }
            };
            match auth {
                AuthResult::Success => Ok(handle),
                AuthResult::Failure { .. } => Err(SessionError::KeyRejected),
            }
        });
        match result {
            Ok(handle) => Ok(Session { rt, handle }),
            Err(e) => {
                // Dropping the runtime would wait for a signing call that never returns.
                rt.shutdown_background();
                Err(e)
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// Opens a channel to the Unix socket `path` on the host and relays it to one end of a
    /// socket pair, returned. The relay ends when either side closes.
    pub fn open_socket(&self, path: &str) -> Result<UnixStream, SshError> {
        let channel = self
            .rt
            .block_on(self.handle.channel_open_direct_streamlocal(path))
            .map_err(|e| {
                SshError::Attention(format!(
                    "the host refused to forward {path}: {e} (sshd needs \
                     AllowStreamLocalForwarding; Tailscale SSH needs 1.98 or later)"
                ))
            })?;
        let (ours, theirs) =
            UnixStream::pair().map_err(|e| SshError::Attention(format!("socket pair: {e}")))?;
        theirs
            .set_nonblocking(true)
            .map_err(|e| SshError::Attention(format!("socket pair: {e}")))?;
        let _guard = self.rt.enter();
        let mut theirs = tokio::net::UnixStream::from_std(theirs)
            .map_err(|e| SshError::Attention(format!("socket pair: {e}")))?;
        self.rt.spawn(async move {
            let mut channel = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut theirs, &mut channel).await;
        });
        Ok(ours)
    }

    /// Runs `line` as given (no wrapping) on an exec channel and returns its exit code and
    /// output, stdout and stderr together. For commands that must see the user's login shell,
    /// such as starting `mosh-server`.
    pub fn exec(&self, line: &str, timeout: Duration) -> Result<(Option<u32>, String), SshError> {
        let run = async {
            let mut ch = self.handle.channel_open_session().await?;
            ch.exec(true, line).await?;
            ch.eof().await?;
            let (mut out, mut code) = (Vec::new(), None);
            while let Some(msg) = ch.wait().await {
                match msg {
                    ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                        out.extend_from_slice(&data)
                    }
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    _ => {}
                }
            }
            Ok::<_, russh::Error>((code, out))
        };
        let (code, out) = self
            .rt
            .block_on(async { tokio::time::timeout(timeout, run).await })
            .map_err(|_| SshError::Transient("ssh: the remote command timed out".into()))?
            .map_err(|e| SshError::Transient(format!("ssh: {e}")))?;
        Ok((code, String::from_utf8_lossy(&out).into_owned()))
    }

    /// Starts the user's login shell on a pseudo-terminal of `size` (columns, rows), with
    /// `env` set when the host allows it (sshd's `AcceptEnv`). `on_event` gets its output, then
    /// one [`ShellEvent::Exit`], on the session's runtime.
    pub fn open_shell(
        &self,
        size: (u16, u16),
        term: &str,
        env: &[(String, String)],
        mut on_event: impl FnMut(ShellEvent) + Send + 'static,
    ) -> Result<Shell, SshError> {
        let open = async {
            let mut ch = self.handle.channel_open_session().await?;
            // Without a terminal, the shell would run but nothing full-screen would.
            ch.request_pty(true, term, size.0.into(), size.1.into(), 0, 0, &[])
                .await?;
            if !replied(&mut ch).await? {
                return Ok(Err(
                    "the host refused a terminal: a key line from Thurm's pairing (thurm-ios:…) \
                     has restrict, which blocks terminals; otherwise PermitTTY no or no-pty",
                ));
            }
            for (k, v) in env {
                ch.set_env(false, k.as_str(), v.as_str()).await?;
            }
            ch.request_shell(true).await?;
            if !replied(&mut ch).await? {
                return Ok(Err("the host refused to start a shell"));
            }
            Ok::<_, russh::Error>(Ok(ch))
        };
        let ch = self
            .rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(20), open).await })
            .map_err(|_| SshError::Transient("ssh: the shell did not start".into()))?
            .map_err(|e| SshError::Transient(format!("ssh: shell: {e}")))?
            .map_err(|refused| SshError::Attention(format!("ssh: {refused}")))?;
        let (mut read, write) = ch.split();
        let (tx, mut rx) = mpsc::unbounded_channel();
        self.rt.spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let sent = match cmd {
                    ShellCmd::Data(d) => write.data_bytes(d).await,
                    ShellCmd::Resize(c, r) => write.window_change(c.into(), r.into(), 0, 0).await,
                    ShellCmd::Close => {
                        let _ = write.eof().await;
                        write.close().await
                    }
                };
                if sent.is_err() {
                    return;
                }
            }
        });
        self.rt.spawn(async move {
            let mut code = None;
            while let Some(msg) = read.wait().await {
                match msg {
                    ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                        on_event(ShellEvent::Data(data.to_vec()))
                    }
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            on_event(ShellEvent::Exit(code));
        });
        Ok(Shell { tx })
    }

    pub fn disconnect(&self) {
        let _ = self
            .rt
            .block_on(self.handle.disconnect(Disconnect::ByApplication, "", "en"));
    }
}

impl RemoteExec for Session {
    fn run(
        &self,
        script: &str,
        args: &[&str],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<String, SshError> {
        let line = thurm_remote::ssh::remote_command(script, args).map_err(SshError::Attention)?;
        let run = async {
            let mut ch = self.handle.channel_open_session().await?;
            ch.exec(true, line).await?;
            if let Some(input) = input {
                ch.data(input).await?;
            }
            ch.eof().await?;
            let (mut out, mut err, mut code) = (Vec::new(), Vec::new(), None);
            while let Some(msg) = ch.wait().await {
                match msg {
                    ChannelMsg::Data { data } => out.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, ext: 1 } => err.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    _ => {}
                }
            }
            Ok::<_, russh::Error>((out, err, code))
        };
        let (out, err, code) = self
            .rt
            .block_on(async { tokio::time::timeout(timeout, run).await })
            .map_err(|_| SshError::Transient("ssh: the remote command timed out".into()))?
            .map_err(|e| SshError::Transient(format!("ssh: {e}")))?;
        match code {
            Some(0) => Ok(String::from_utf8_lossy(&out).into_owned()),
            Some(code) => Err(SshError::Remote {
                code: code as i32,
                stderr: String::from_utf8_lossy(&err).into_owned(),
            }),
            None => Err(SshError::Transient(
                "ssh: the remote command ended without an exit status".into(),
            )),
        }
    }
}

/// The host's answer to the last request sent on `ch` with `want_reply`.
async fn replied(ch: &mut russh::Channel<client::Msg>) -> Result<bool, russh::Error> {
    while let Some(msg) = ch.wait().await {
        match msg {
            ChannelMsg::Success => return Ok(true),
            ChannelMsg::Failure => return Ok(false),
            _ => {}
        }
    }
    Err(russh::Error::Disconnect)
}

/// The host did not accept the device's key, in the words the app shows.
const KEY_REJECTED: &str = "the host did not accept this device's key; add it from the Mac \
     (Thurm › Remotes… › Pair iPhone)";

/// Why [`Session::connect_checked`] failed.
#[derive(Debug)]
pub enum SessionError {
    Ssh(SshError),
    /// The host showed this key (`SHA256:` fingerprint) and none is trusted for it yet.
    UnknownHostKey(String),
    /// The host showed this key, not one of those trusted for it.
    HostKeyChanged(String),
    /// The host did not accept the device's key.
    KeyRejected,
}

impl SessionError {
    fn into_ssh(self) -> SshError {
        match self {
            SessionError::Ssh(e) => e,
            SessionError::UnknownHostKey(seen) | SessionError::HostKeyChanged(seen) => {
                SshError::Attention(format!(
                    "the host's key ({seen}) is not the one paired with this device; \
                     pair the host again if it was reinstalled"
                ))
            }
            SessionError::KeyRejected => SshError::Attention(KEY_REJECTED.into()),
        }
    }
}

impl From<SshError> for SessionError {
    fn from(e: SshError) -> Self {
        SessionError::Ssh(e)
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Ssh(e) => write!(f, "{e}"),
            SessionError::UnknownHostKey(k) => write!(f, "the host's key {k} is not trusted yet"),
            SessionError::HostKeyChanged(k) => {
                write!(
                    f,
                    "the host's key changed to {k}; it may have been reinstalled"
                )
            }
            SessionError::KeyRejected => f.write_str(
                "the host did not accept this device's key; add it to ~/.ssh/authorized_keys there",
            ),
        }
    }
}

impl std::error::Error for SessionError {}

/// What an interactive shell ([`Session::open_shell`]) does.
#[derive(Debug, PartialEq, Eq)]
pub enum ShellEvent {
    Data(Vec<u8>),
    /// The shell ended (with its exit status, when the host sent one), or its channel did.
    Exit(Option<u32>),
}

enum ShellCmd {
    Data(Vec<u8>),
    Resize(u16, u16),
    Close,
}

/// An interactive shell on its own channel. Dropping it leaves the shell running until the
/// session ends; [`Shell::close`] ends it.
pub struct Shell {
    tx: mpsc::UnboundedSender<ShellCmd>,
}

impl Shell {
    pub fn write(&self, data: &[u8]) {
        let _ = self.tx.send(ShellCmd::Data(data.to_vec()));
    }

    /// The terminal is now `cols` x `rows`.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(ShellCmd::Resize(cols, rows));
    }

    pub fn close(&self) {
        let _ = self.tx.send(ShellCmd::Close);
    }
}

/// Why connecting to a host's daemon failed.
#[derive(Debug)]
pub enum ConnectError {
    Ssh(SshError),
    /// The host does not accept the device's key yet: add it on the Mac, then try again.
    KeyRejected,
    /// No `thurm` on the host: install it from the Mac.
    NotInstalled,
    Daemon(ClientError),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Ssh(e) => write!(f, "{e}"),
            ConnectError::KeyRejected => f.write_str(KEY_REJECTED),
            ConnectError::NotInstalled => f.write_str(
                "Thurm's command-line tool is not installed on this host; on a Mac, use Thurm › \
                 Install Command-Line Tool, elsewhere `thurm remote add`",
            ),
            ConnectError::Daemon(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// A host's daemon, connected as a mobile client. Dropping it closes the connection.
pub struct Remote {
    pub client: Arc<Client>,
    pub host: HostInfo,
    pub session: Session,
}

/// Connects to `target`, starts its daemon when none runs, and says Hello as a mobile client.
/// Mirrors the desktop tunnel (`thurm_remote::tunnel`): probe, socket, start, forward.
pub fn connect(
    target: &Target,
    key: Arc<dyn DeviceKey>,
    client_name: &str,
    on_event: impl Fn(Event) + Send + 'static,
    on_disconnect: impl FnOnce() + Send + 'static,
) -> Result<Remote, ConnectError> {
    let session =
        Session::connect_checked(target, key, Duration::from_secs(20)).map_err(|e| match e {
            SessionError::KeyRejected => ConnectError::KeyRejected,
            e => ConnectError::Ssh(e.into_ssh()),
        })?;
    let host = thurm_remote::probe(&session).map_err(ConnectError::Ssh)?;
    let socket = match (&target.socket, &host.info) {
        (Some(s), _) => s.clone(),
        (None, Some(info)) => info.socket.clone(),
        (None, None) => return Err(ConnectError::NotInstalled),
    };
    thurm_remote::start_daemon(&session, &socket).map_err(ConnectError::Ssh)?;
    let stream = session.open_socket(&socket).map_err(ConnectError::Ssh)?;
    let client = Client::from_stream(
        stream,
        client_name,
        false,
        &[thurm_proto::CAP_MOBILE, thurm_proto::CAP_PANE_PAGER],
        on_event,
        on_disconnect,
    )
    .map_err(ConnectError::Daemon)?;
    Ok(Remote {
        client,
        host,
        session,
    })
}

#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(test)]
mod tests;
