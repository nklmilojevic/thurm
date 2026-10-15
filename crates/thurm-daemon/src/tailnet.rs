//! Serving over Tailscale (see `thurm_proto::tailnet`): runs the `thurm-tailnet` helper, tells it
//! what to serve, and keeps its status.
//!
//! The helper is a child process talking JSON lines over an inherited socket (fd 3). It never
//! outlives the daemon (it stops when that socket closes) and survives an in-place upgrade: the
//! socket and its pid ride along in the handoff.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crossbeam_channel::{Sender, bounded};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thurm_config::TailscaleConfig;
use thurm_proto::TailscaleStatus;
use thurm_proto::tailnet::{TailscaleState, valid_hostname, valid_tag};

/// How long a request to the helper may take (a sign-out reaches the coordination server).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(35);
/// The longest wait before restarting a helper that keeps exiting.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// What the helper should serve.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
struct NodeWant {
    enabled: bool,
    hostname: String,
    allow_tailnet_members: bool,
    advertise_tags: Vec<String>,
    auth_key: String,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
struct HostWant {
    listen: String,
    allow_identified: bool,
    serve: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Want {
    node: NodeWant,
    host: HostWant,
}

impl Want {
    fn any(&self) -> bool {
        self.node.enabled || !self.host.listen.is_empty() || self.host.serve
    }

    fn from_config(cfg: &TailscaleConfig) -> Want {
        let auth_key = if cfg.enabled && !cfg.auth_key_file.is_empty() {
            match std::fs::read_to_string(&cfg.auth_key_file) {
                Ok(k) => k.trim().to_owned(),
                Err(e) => {
                    log::warn!("tailscale: reading the auth key file: {e}");
                    String::new()
                }
            }
        } else {
            String::new()
        };
        Want {
            node: NodeWant {
                enabled: cfg.enabled,
                hostname: cfg.hostname.clone(),
                allow_tailnet_members: cfg.allow_tailnet_members,
                advertise_tags: cfg.advertise_tags.clone(),
                auth_key,
            },
            host: HostWant {
                listen: cfg.host_listen.clone(),
                allow_identified: cfg.host_allow_identified,
                serve: cfg.serve_socket,
            },
        }
    }
}

#[derive(Deserialize)]
struct Reply {
    id: Option<u64>,
    error: Option<String>,
    status: Option<TailscaleStatus>,
}

type Answer = Result<TailscaleStatus, String>;

struct Helper {
    /// Our end of the control socket.
    ctl: UnixStream,
    pid: i32,
    /// Tells this helper's reader apart from a later one's.
    generation: u64,
}

#[derive(Default)]
struct Inner {
    want: Want,
    /// The `[tailscale]` section last applied, so a reload that leaves it alone keeps what
    /// requests turned on or off since.
    config: Option<TailscaleConfig>,
    helper: Option<Helper>,
    pending: HashMap<u64, Sender<Answer>>,
    generation: u64,
    failures: u32,
}

pub struct Tailnet {
    /// The daemon's socket, which the helper connects tailnet callers to.
    socket: PathBuf,
    /// The embedded node's state.
    state_dir: PathBuf,
    inner: Mutex<Inner>,
    status: Mutex<TailscaleStatus>,
    on_change: OnceLock<Box<dyn Fn(TailscaleStatus) + Send + Sync>>,
    next_id: AtomicU64,
}

impl Tailnet {
    pub fn new(socket: PathBuf, state_dir: PathBuf) -> Arc<Tailnet> {
        Arc::new(Tailnet {
            socket,
            state_dir,
            inner: Mutex::new(Inner::default()),
            status: Mutex::new(TailscaleStatus::default()),
            on_change: OnceLock::new(),
            next_id: AtomicU64::new(1),
        })
    }

    /// Called with every status change.
    pub fn on_change(&self, f: impl Fn(TailscaleStatus) + Send + Sync + 'static) {
        let _ = self.on_change.set(Box::new(f));
    }

    pub fn status(&self) -> TailscaleStatus {
        self.status.lock().clone()
    }

    /// Applies the config's `[tailscale]` section when it changed since the last call.
    pub fn configure(self: &Arc<Self>, cfg: &TailscaleConfig) {
        {
            let mut inner = self.inner.lock();
            if inner.config.as_ref() == Some(cfg) {
                return;
            }
            inner.config = Some(cfg.clone());
            inner.want = Want::from_config(cfg);
        }
        let this = self.clone();
        // Off the caller's thread: starting the helper and the node takes a moment.
        std::thread::spawn(move || {
            if let Err(e) = this.apply() {
                log::warn!("tailscale: {e}");
            }
        });
    }

    /// Serve as the embedded node (`Request::TailscaleEnable`).
    pub fn enable(
        self: &Arc<Self>,
        hostname: Option<String>,
        allow_tailnet_members: bool,
        advertise_tags: Vec<String>,
        auth_key: Option<String>,
    ) -> Answer {
        if let Some(h) = &hostname
            && !valid_hostname(h)
        {
            return Err(format!(
                "invalid tailnet hostname {h:?}: one DNS label of 1 to 63 letters, digits or hyphens"
            ));
        }
        if let Some(t) = advertise_tags.iter().find(|t| !valid_tag(t)) {
            return Err(format!("invalid tag {t:?}: tags look like tag:name"));
        }
        if !advertise_tags.is_empty() && !allow_tailnet_members {
            return Err(
                "tailscale: advertising tags gives this node a machine identity with no \
                        owner, so owner-only access would admit nobody. Allow tailnet members \
                        or drop the tags."
                    .into(),
            );
        }
        {
            let mut inner = self.inner.lock();
            let node = &mut inner.want.node;
            node.enabled = true;
            if let Some(h) = hostname {
                node.hostname = h;
            }
            node.allow_tailnet_members = allow_tailnet_members;
            node.advertise_tags = advertise_tags;
            if let Some(k) = auth_key {
                node.auth_key = k;
            }
        }
        self.apply()
    }

    /// Stop serving as the embedded node; it stays signed in (`Request::TailscaleDisable`).
    pub fn disable(self: &Arc<Self>) -> Answer {
        self.inner.lock().want.node.enabled = false;
        self.apply()
    }

    /// Ask for a sign-in link (`Request::TailscaleLogin`).
    pub fn login(self: &Arc<Self>) -> Answer {
        if !self.inner.lock().want.node.enabled {
            return Err("enable Tailscale before requesting a sign-in link".into());
        }
        self.request(serde_json::json!({"cmd": "login"}))
    }

    /// Sign out and forget the node (`Request::TailscaleLogout`).
    pub fn logout(self: &Arc<Self>) -> Answer {
        if self.inner.lock().helper.is_none() {
            return Ok(self.status());
        }
        let status = self.request(serde_json::json!({"cmd": "logout"}))?;
        // Signed out: the node is off until enabled (and signed in) again.
        let mut inner = self.inner.lock();
        inner.want.node.enabled = false;
        inner.want.node.auth_key.clear();
        Ok(status)
    }

    /// Rename the node without signing in again (`Request::TailscaleRename`).
    pub fn rename(self: &Arc<Self>, hostname: String) -> Answer {
        if !valid_hostname(&hostname) {
            return Err(format!(
                "invalid tailnet hostname {hostname:?}: one DNS label of 1 to 63 letters, digits or hyphens"
            ));
        }
        self.inner.lock().want.node.hostname = hostname.clone();
        if self.inner.lock().helper.is_none() {
            let mut status = self.status.lock();
            status.hostname = hostname;
            return Ok(status.clone());
        }
        self.request(serde_json::json!({"cmd": "rename", "hostname": hostname}))
    }

    /// The helper's pid and control socket, to carry through an in-place upgrade (cleared of
    /// `FD_CLOEXEC` by the caller).
    pub fn handoff(&self) -> Option<(i32, RawFd)> {
        let inner = self.inner.lock();
        inner.helper.as_ref().map(|h| (h.pid, h.ctl.as_raw_fd()))
    }

    /// Takes over the helper of the image we replaced, then re-applies the config.
    pub fn adopt(self: &Arc<Self>, pid: i32, fd: RawFd) {
        let ctl = unsafe { UnixStream::from_raw_fd(fd) };
        let _ = crate::pty::set_cloexec(fd, true);
        log::info!("tailscale: adopted helper {pid}");
        self.attach(ctl, pid);
        let _ = self.request(serde_json::json!({"cmd": "status"}));
    }

    /// Starts the helper when something is to be served, and sends it what to serve.
    fn apply(self: &Arc<Self>) -> Answer {
        let want = {
            let inner = self.inner.lock();
            if inner.helper.is_none() && !inner.want.any() {
                let status = TailscaleStatus {
                    hostname: inner.want.node.hostname.clone(),
                    ..Default::default()
                };
                drop(inner);
                self.set_status(status.clone());
                return Ok(status);
            }
            inner.want.clone()
        };
        if self.inner.lock().helper.is_none() {
            self.spawn().inspect_err(|e| {
                self.set_status(TailscaleStatus {
                    state: TailscaleState::Error,
                    error: Some(e.clone()),
                    hostname: want.node.hostname.clone(),
                    ..Default::default()
                })
            })?;
        }
        self.request(serde_json::json!({
            "cmd": "apply",
            "node": want.node,
            "host": want.host,
        }))
    }

    fn helper_path() -> Result<PathBuf, String> {
        if let Some(p) = std::env::var_os("THURM_TAILNET") {
            return Ok(PathBuf::from(p));
        }
        let exe = std::env::current_exe().map_err(|e| format!("locating thurmd: {e}"))?;
        let path = exe.with_file_name("thurm-tailnet");
        if path.exists() {
            Ok(path)
        } else {
            Err(format!(
                "the Tailscale helper is not installed ({} is missing)",
                path.display()
            ))
        }
    }

    fn spawn(self: &Arc<Self>) -> Result<(), String> {
        let exe = Self::helper_path()?;
        thurm_config::ensure_private_dir(&self.state_dir)
            .map_err(|e| format!("creating {}: {e}", self.state_dir.display()))?;
        let serve_socket = self.socket.with_file_name("tailnet-serve.sock");
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("control socket: {e}"))?;
        let fd = theirs.as_raw_fd();
        let mut cmd = Command::new(&exe);
        cmd.arg("--control-fd")
            .arg("3")
            .arg("--socket")
            .arg(&self.socket)
            .arg("--state")
            .arg(&self.state_dir)
            .arg("--serve-socket")
            .arg(&serve_socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if fd == 3 {
                    let flags = libc::fcntl(3, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("starting {}: {e}", exe.display()))?;
        drop(theirs);
        let pid = child.id() as i32;
        // Dropping the handle neither kills nor waits: the reader reaps it once its socket
        // closes.
        drop(child);
        log::info!("tailscale: started helper {pid}");
        self.attach(ours, pid);
        Ok(())
    }

    /// Starts reading a helper's control socket.
    fn attach(self: &Arc<Self>, ctl: UnixStream, pid: i32) {
        let reader = match ctl.try_clone() {
            Ok(r) => r,
            Err(e) => {
                log::warn!("tailscale: control socket: {e}");
                return;
            }
        };
        let generation = {
            let mut inner = self.inner.lock();
            inner.generation += 1;
            let generation = inner.generation;
            inner.helper = Some(Helper {
                ctl,
                pid,
                generation,
            });
            generation
        };
        let this = self.clone();
        let _ = std::thread::Builder::new()
            .name("tailnet".into())
            .spawn(move || this.read(reader, pid, generation));
    }

    fn read(self: Arc<Self>, ctl: UnixStream, pid: i32, generation: u64) {
        for line in BufReader::new(ctl).lines() {
            let Ok(line) = line else { break };
            let reply: Reply = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    log::debug!("tailscale: unreadable helper message: {e}");
                    continue;
                }
            };
            if let Some(status) = &reply.status {
                self.set_status(status.clone());
            }
            if let Some(id) = reply.id {
                let waiter = self.inner.lock().pending.remove(&id);
                if let Some(tx) = waiter {
                    let answer = match reply.error {
                        Some(e) => Err(e),
                        None => Ok(reply.status.unwrap_or_else(|| self.status())),
                    };
                    let _ = tx.send(answer);
                }
            }
        }
        // The helper exited (or closed its socket): reap it, and start a new one if wanted.
        unsafe {
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
        let (restart, delay) = {
            let mut inner = self.inner.lock();
            if inner.helper.as_ref().map(|h| h.generation) != Some(generation) {
                return;
            }
            inner.helper = None;
            for (_, tx) in inner.pending.drain() {
                let _ = tx.send(Err("the Tailscale helper exited".into()));
            }
            inner.failures += 1;
            let delay = Duration::from_secs(1 << inner.failures.min(6)).min(MAX_BACKOFF);
            (inner.want.any(), delay)
        };
        log::warn!("tailscale: helper {pid} exited");
        if !restart {
            self.set_status(TailscaleStatus {
                hostname: self.inner.lock().want.node.hostname.clone(),
                ..Default::default()
            });
            return;
        }
        self.set_status(TailscaleStatus {
            state: TailscaleState::Stopped,
            error: Some("the Tailscale helper stopped; restarting it".into()),
            hostname: self.inner.lock().want.node.hostname.clone(),
            ..Default::default()
        });
        std::thread::sleep(delay);
        if let Err(e) = self.apply() {
            log::warn!("tailscale: restarting the helper: {e}");
        }
    }

    /// Sends a command and waits for its answer.
    fn request(&self, mut cmd: serde_json::Value) -> Answer {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        cmd["id"] = id.into();
        let (tx, rx) = bounded(1);
        {
            let mut inner = self.inner.lock();
            let Some(helper) = inner.helper.as_mut() else {
                return Err("the Tailscale helper is not running".into());
            };
            let mut line = cmd.to_string();
            line.push('\n');
            if let Err(e) = helper.ctl.write_all(line.as_bytes()) {
                return Err(format!("talking to the Tailscale helper: {e}"));
            }
            inner.pending.insert(id, tx);
        }
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(answer) => {
                if answer.is_ok() {
                    self.inner.lock().failures = 0;
                }
                answer
            }
            Err(_) => {
                self.inner.lock().pending.remove(&id);
                Err("the Tailscale helper did not answer in time".into())
            }
        }
    }

    fn set_status(&self, status: TailscaleStatus) {
        {
            let mut current = self.status.lock();
            if *current == status {
                return;
            }
            *current = status.clone();
        }
        // The link signs a device into the account: it goes to clients, not the log.
        if status.state == TailscaleState::NeedsLogin && status.auth_url.is_some() {
            log::info!("tailscale: waiting for sign-in (`thurm tailscale status` shows the link)");
        }
        if let Some(f) = self.on_change.get() {
            f(status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn want_from_config() {
        let cfg = TailscaleConfig {
            enabled: true,
            hostname: "mac".into(),
            host_listen: ":7297".into(),
            ..Default::default()
        };
        let w = Want::from_config(&cfg);
        assert!(w.any());
        assert_eq!(w.node.hostname, "mac");
        assert_eq!(w.host.listen, ":7297");
        assert!(!Want::from_config(&TailscaleConfig::default()).any());
    }

    #[test]
    fn enable_validates_before_starting_anything() {
        let t = Tailnet::new("/nonexistent/thurmd.sock".into(), "/nonexistent/ts".into());
        assert!(
            t.enable(Some("bad name".into()), false, vec![], None)
                .is_err()
        );
        assert!(t.enable(None, false, vec!["thurm".into()], None).is_err());
        let err = t
            .enable(None, false, vec!["tag:thurm".into()], None)
            .unwrap_err();
        assert!(err.contains("owner-only"), "{err}");
        assert!(t.inner.lock().helper.is_none());
    }

    #[test]
    fn login_needs_the_node_on() {
        let t = Tailnet::new("/nonexistent/thurmd.sock".into(), "/nonexistent/ts".into());
        assert!(t.login().is_err());
    }
}
