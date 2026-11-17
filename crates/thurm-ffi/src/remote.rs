//! Remote workspaces for the app: the tunnel supervisors (one per enabled `[[remote]]`), the
//! install and handoff operations, and the policy for remote panes. See `thurm.h`.

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use thurm_config::Config;
use thurm_remote::doctor;
use thurm_remote::forward::{self, Forwards};
use thurm_remote::handoff::{self, Registry, SshHost};
use thurm_remote::install::{self, Method};
use thurm_remote::tunnel::{Supervisor, Transport};
use thurm_remote::{RemoteExec, Ssh, SshError, pairing, policy};

use crate::{Ctx, into_c, opt_str};

pub type thurm_remote_status_cb =
    Option<unsafe extern "C" fn(ctx: *mut c_void, json: *const c_char)>;

struct Manager {
    supervisors: HashMap<String, Supervisor>,
    cb: thurm_remote_status_cb,
    ctx: Ctx,
    /// Where each `[[remote]]` pointed at the last sync.
    destinations: HashMap<String, Destination>,
}

/// Where a `[[remote]]` reaches its daemon: its ssh target, socket and tailnet address. A host
/// whose destination changes under the same name is another machine to its forwards.
type Destination = (String, Option<String>, Option<String>);

fn destination(r: &thurm_config::RemoteConfig) -> Destination {
    (r.host.clone(), r.socket.clone(), r.tailnet.clone())
}

/// The hosts of `cfg` that point elsewhere than they did in `before`.
fn moved(before: &HashMap<String, Destination>, cfg: &Config) -> Vec<String> {
    cfg.remote
        .iter()
        .filter(|r| before.get(&r.name).is_some_and(|d| *d != destination(r)))
        .map(|r| r.name.clone())
        .collect()
}

static MANAGER: Mutex<Option<Manager>> = Mutex::new(None);

/// Where forward changes are reported: the status callback, kept apart from `MANAGER` because
/// closing forwards happens under its lock (`sync`).
static FORWARD_CB: Mutex<Option<(thurm_remote_status_cb, Ctx)>> = Mutex::new(None);

/// This app's forwards ([`forward`]).
fn forwards() -> &'static Forwards {
    static FORWARDS: std::sync::OnceLock<Forwards> = std::sync::OnceLock::new();
    FORWARDS.get_or_init(|| {
        let f = Forwards::default();
        report_changes(&f, |report| {
            if let Some((cb, ctx)) = *FORWARD_CB.lock() {
                emit(cb, ctx, report);
            }
        });
        f
    })
}

/// Reports each change of `f` to `out` as `{forwards, change}`: every open forward and what
/// changed, each forward as the app reads it ([`forward_json`]). One report at a time, each
/// built and sent together: one built earlier is never sent after one built later, so the last
/// list the app gets is the one in force.
fn report_changes(f: &Forwards, out: impl Fn(&str) + Send + Sync + 'static) {
    let me = f.clone();
    let in_order = Mutex::new(());
    f.on_change(move |change| {
        let _one_at_a_time = in_order.lock();
        let report = json!({ "forwards": forward_list(&me), "change": change_json(&change) });
        out(&report.to_string());
    });
}

/// `change` with its forward as [`forward_json`] gives it (`local_port`, `url`).
fn change_json(change: &forward::Change) -> Value {
    match change {
        forward::Change::Opened { forward } => {
            json!({ "change": "opened", "forward": forward_json(forward, None) })
        }
        forward::Change::Linked { forward } => {
            json!({ "change": "linked", "forward": forward_json(forward, None) })
        }
        forward::Change::Closed { forward, why } => {
            json!({ "change": "closed", "why": why, "forward": forward_json(forward, None) })
        }
    }
}

/// Every forward, with the URL it opens at.
fn forward_list(f: &Forwards) -> Vec<Value> {
    f.list().iter().map(|fw| forward_json(fw, None)).collect()
}

/// The forward's JSON for the app; `url` the link it opens at: `url` (the link it was just
/// opened for) or the one it was last opened for, rewritten to it; else its root over http.
fn forward_json(fw: &forward::Forward, url: Option<&str>) -> Value {
    let url = url
        .or(fw.link.as_deref())
        .and_then(|u| forward::rewrite(u, fw.local))
        .unwrap_or_else(|| format!("http://localhost:{}/", fw.local));
    json!({
        "id": fw.id,
        "host": fw.host,
        "remote_port": fw.remote_port,
        "local_port": fw.local,
        "url": url,
    })
}

/// Whether `socket` is the local end of a tailnet host's tunnel, a relay (see
/// `thurm_remote::tunnel`) rather than an ssh forward.
pub fn is_tailnet_socket(socket: &std::path::Path) -> bool {
    let guard = MANAGER.lock();
    guard.as_ref().is_some_and(|m| {
        m.supervisors.values().any(|s| {
            let status = s.status();
            status.transport == Transport::Tailnet && std::path::Path::new(&status.socket) == socket
        })
    })
}

fn emit(cb: thurm_remote_status_cb, ctx: Ctx, json: &str) {
    if let (Some(cb), Ok(s)) = (cb, std::ffi::CString::new(json)) {
        unsafe { cb(ctx.0, s.as_ptr()) };
    }
}

/// Brings `f` in line with the config: closes the forwards of a host pointed elsewhere (before
/// its tunnel moves) and of a host removed or turned off, and remembers where each host points.
/// A config that could not be read changes nothing: an empty list in its place would close
/// every forward, though no host went anywhere.
fn reconcile_forwards(
    f: &Forwards,
    destinations: &mut HashMap<String, Destination>,
    cfg: Result<&Config, String>,
) {
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            log::warn!("forwards kept as they are: the config could not be read: {e}");
            return;
        }
    };
    // Before the supervisors move their tunnels: a forward opened for the old destination must
    // not start reaching the new one.
    for name in moved(destinations, cfg) {
        f.host_moved(&name);
    }
    *destinations = cfg
        .remote
        .iter()
        .map(|r| (r.name.clone(), destination(r)))
        .collect();
    let enabled: Vec<&str> = cfg
        .remote
        .iter()
        .filter(|r| r.enabled)
        .map(|r| r.name.as_str())
        .collect();
    f.retain_hosts(&enabled);
}

/// Starts or stops supervisors so they match the config's enabled remotes; updates changed
/// ones.
fn sync(m: &mut Manager) {
    sync_with(m, Config::load().map_err(|e| e.to_string()));
}

/// [`sync`] with the config `loaded`. One that could not be read (being rewritten) changes
/// nothing: taken for no hosts, it would stop every tunnel, and every forward with it.
fn sync_with(m: &mut Manager, loaded: Result<Config, String>) {
    let cfg = match loaded {
        Ok(cfg) => cfg,
        Err(e) => {
            log::warn!("remotes kept as they are: the config could not be read: {e}");
            return;
        }
    };
    reconcile_forwards(forwards(), &mut m.destinations, Ok(&cfg));
    let wanted: HashMap<String, thurm_config::RemoteConfig> = cfg
        .remote
        .iter()
        .map(|r| (r.name.clone(), r.clone()))
        .collect();
    let gone: Vec<String> = m
        .supervisors
        .keys()
        .filter(|name| !wanted.contains_key(*name))
        .cloned()
        .collect();
    stop_later(
        gone.iter()
            .filter_map(|n| m.supervisors.remove(n))
            .collect(),
    );
    for (name, r) in wanted {
        match m.supervisors.get(&name) {
            Some(s) => match s.update(r) {
                // Forwards to a moved host open again once a connection with these settings
                // (the new destination's) is up.
                Ok(()) => forwards().await_settings(&name, s.settings()),
                // Not taken (an ssh target it can't use): the tunnel keeps where it goes, and
                // the host's panes are still there, so its forwards may open once it is
                // connected with the settings it has.
                Err(e) => {
                    log::warn!("remote {name}: {e}");
                    forwards().await_settings(&name, s.settings());
                }
            },
            None => {
                let ssh = match Ssh::new(&r.host) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("remote {name}: {e}");
                        continue;
                    }
                };
                let (cb, ctx) = (m.cb, m.ctx);
                let tunnel = forwards().host_started(&name);
                let sup = Supervisor::start(r, ssh, true, move |status| {
                    if status.phase == thurm_remote::tunnel::Phase::Connected {
                        forwards().host_connected(&status.name, tunnel, status.settings);
                    }
                    if let Ok(j) = serde_json::to_string(status) {
                        emit(cb, ctx, &j);
                    }
                });
                m.supervisors.insert(name, sup);
            }
        }
    }
}

/// Stops `supervisors` on another thread: stopping joins the tunnel thread, which must not
/// hold up the caller (the app's main thread, holding the manager's lock).
fn stop_later(supervisors: Vec<Supervisor>) {
    if supervisors.is_empty() {
        return;
    }
    // The closure owns them only through this slot: if the thread can't start, they are
    // left running (until the app exits) rather than stopped here.
    let slot = std::sync::Arc::new(Mutex::new(Some(supervisors)));
    let theirs = slot.clone();
    let spawned = std::thread::Builder::new()
        .name("remote-stop".into())
        .spawn(move || drop(theirs.lock().take()));
    if let Err(e) = spawned {
        log::warn!("cannot stop remote tunnels in the background: {e}");
        std::mem::forget(slot.lock().take());
    }
}

/// Starts supervising every enabled `[[remote]]`. `on_status` gets each host's status JSON
/// on every change (on a background thread). Calling it again replaces the callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_remotes_start(on_status: thurm_remote_status_cb, ctx: *mut c_void) {
    *FORWARD_CB.lock() = Some((on_status, Ctx(ctx)));
    let mut g = MANAGER.lock();
    if let Some(m) = g.as_mut() {
        m.cb = on_status;
        m.ctx = Ctx(ctx);
        // Supervisors keep their old callback; restart them with the new one.
        stop_later(m.supervisors.drain().map(|(_, s)| s).collect());
        sync(m);
        return;
    }
    let mut m = Manager {
        supervisors: HashMap::new(),
        cb: on_status,
        ctx: Ctx(ctx),
        destinations: HashMap::new(),
    };
    sync(&mut m);
    *g = Some(m);
}

/// Re-reads the `[[remote]]` entries (after a config change).
#[unsafe(no_mangle)]
pub extern "C" fn thurm_remotes_sync() {
    if let Some(m) = MANAGER.lock().as_mut() {
        sync(m);
    }
}

/// Stops every tunnel (app quitting). Remote daemons and their panes keep running.
#[unsafe(no_mangle)]
pub extern "C" fn thurm_remotes_stop() {
    let taken = MANAGER.lock().take();
    drop(taken);
    *FORWARD_CB.lock() = None;
}

/// Retry `name` (NULL: every host) now. With `restart`, check a connected tunnel and replace
/// it when it no longer answers (after wake from sleep or a network change).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_remote_kick(name: *const c_char, restart: bool) {
    let name = unsafe { opt_str(name) };
    if let Some(m) = MANAGER.lock().as_ref() {
        for (n, s) in &m.supervisors {
            if name.is_none_or(|x| x == n) {
                s.kick(restart);
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Call {
    Status,
    /// What installing on `name` would do.
    Plan {
        name: String,
        bins: Option<String>,
    },
    Install {
        name: String,
        method: Method,
        bins: Option<String>,
    },
    /// Replace `name`'s daemon with the installed build.
    UpgradeDaemon {
        name: String,
        allow_restart: bool,
    },
    /// Adds `[[remote]]` `name` after checking that ssh reaches `target` (Thurm › Remotes… ›
    /// Add Host…). Installing and the rest are `doctor` and `doctor_fix`.
    Add {
        name: String,
        target: String,
        /// Reach it over the tailnet instead (a peer's name or address; empty: the target), after
        /// checking the daemon answers there. ssh is not tried.
        #[serde(default)]
        tailnet: Option<String>,
    },
    /// Removes `[[remote]]` `name` (its daemon and panes keep running there).
    Remove {
        name: String,
    },
    /// Sets `key` of `[[remote]]` `name`: `host`, `enabled` or `clipboard_read`.
    Set {
        name: String,
        key: String,
        value: serde_json::Value,
    },
    /// What the host needs, with fixes (`thurm remote doctor`).
    Doctor {
        name: String,
        bins: Option<String>,
    },
    /// Runs the fix of check `id`.
    DoctorFix {
        name: String,
        id: String,
        bins: Option<String>,
        #[serde(default)]
        allow_restart: bool,
    },
    /// The ssh command line that runs `script` on `name` in a terminal (a pane of this Mac).
    TerminalArgv {
        name: String,
        script: String,
    },
    /// "Always for devbox": store `clipboard_read = "always"`.
    AllowClipboard {
        name: String,
    },
    ClipboardRead {
        host: Option<String>,
    },
    ClipboardWrite {
        host: Option<String>,
    },
    Link {
        host: Option<String>,
        url: String,
    },
    HandoffPrepare {
        host: String,
        path: String,
        branch: Option<String>,
    },
    HandoffSetPane {
        id: String,
        pane: Option<u64>,
    },
    HandoffFetch {
        id: String,
    },
    HandoffCheck {
        id: String,
    },
    HandoffCleanup {
        id: String,
        force: bool,
    },
    /// The tab closed while the host was offline: clean up once it is back.
    HandoffDefer {
        id: String,
    },
    HandoffList,
    /// Every enabled `[[remote]]` as the iOS pairing code describes it, or why it can't be in it.
    PairingHosts,
    /// Puts the iOS device key line `key` (`ecdsa-sha2-nistp256 … thurm-ios:<device>`) in
    /// `name`'s authorized_keys.
    AuthorizeDevice {
        name: String,
        key: String,
    },
    RevokeDevice {
        name: String,
        device: String,
    },
    /// The devices with a key on `name`.
    PairedDevices {
        name: String,
    },
    /// Whether this Mac's `thurm` is found, and speaks our protocol, from the environment sshd
    /// gives a login (Pair iPhone's check of the command-line tool).
    LocalProbe,
    /// Opens `host`'s `port` here (or the port of `url`, a link to its loopback), or returns
    /// the forward that has it: `{id, host, remote_port, local_port, url, new}`, `url` being
    /// `url` rewritten to the forward. Connects to the host's daemon first; call it off the
    /// main thread.
    ForwardOpen {
        host: String,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        url: Option<String>,
        /// The local port to listen on; else the remote port's number when free.
        #[serde(default)]
        local: Option<u16>,
    },
    ForwardClose {
        id: u64,
    },
    ForwardList,
    /// Forward `id` as it is now (as `forward_list` lists it), or an error once it has closed:
    /// asked before opening or copying a link the app has held on to.
    ForwardGet {
        id: u64,
    },
    /// A `PortsChanged` from `host`'s daemon.
    PortsChanged {
        host: String,
        pane: u64,
        ports: Vec<thurm_proto::PortEntry>,
        complete: bool,
    },
    /// The ports `host`'s panes listen on, as its last `PortsChanged` said.
    KnownPorts {
        host: String,
    },
    /// The connection to `host` dropped: its port lists are unknown until the daemon sends them
    /// again (every pane's, as the app joins the watch on connecting).
    HostDisconnected {
        host: String,
    },
    /// `host`'s panes on connecting (`ListPanes`): lists of others are dropped, and its
    /// forwards are looked at again, a host with no pane included.
    HostPanes {
        host: String,
        panes: Vec<u64>,
    },
}

/// The port `forward_open` forwards for `host`: `port`, or `url`'s. Only a configured host that
/// is turned on has forwards.
fn forward_port(
    cfg: &Config,
    host: &str,
    port: Option<u16>,
    url: Option<&str>,
) -> Result<u16, String> {
    let r = cfg
        .remote(host)
        .ok_or_else(|| format!("no remote named {host:?}"))?;
    forward::forwardable(r)?;
    let from_url = url
        .map(|u| {
            policy::LocalUrl::parse(u)
                .map(|l| l.port)
                .ok_or_else(|| format!("{u} is not a link to {host}'s localhost"))
        })
        .transpose()?;
    match (port, from_url) {
        // Both: they must agree, or the link would open at another service's forward.
        (Some(p), Some(u)) if p != u => Err(format!(
            "port {p} and the link's port {u} differ; give one of them"
        )),
        (Some(p), _) | (None, Some(p)) => Ok(p),
        (None, None) => Err("a port or a URL to forward".into()),
    }
}

/// Runs scripts with the local `/bin/sh` in the PATH sshd gives a login, so the probe sees what
/// a host's would.
struct LocalShell;

impl RemoteExec for LocalShell {
    fn run(
        &self,
        script: &str,
        args: &[&str],
        _input: Option<&[u8]>,
        _timeout: std::time::Duration,
    ) -> Result<String, SshError> {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .arg("thurm")
            .args(args)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .output()
            .map_err(|e| SshError::Attention(format!("sh: {e}")))?;
        match out.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
            code => Err(SshError::Remote {
                code: code.unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
        }
    }
}

fn remote(name: &str) -> Result<thurm_config::RemoteConfig, String> {
    Config::load()
        .map_err(|e| e.to_string())?
        .remote(name)
        .cloned()
        .ok_or_else(|| format!("no remote named {name:?}"))
}

fn ssh_for(name: &str) -> Result<(thurm_config::RemoteConfig, Ssh), String> {
    let r = remote(name)?;
    thurm_remote::ssh::prepare_dir().map_err(|e| e.to_string())?;
    let ssh = Ssh::new(&r.host)?;
    Ok((r, ssh))
}

fn host_for(name: &str) -> Result<SshHost, String> {
    let (r, ssh) = ssh_for(name)?;
    Ok(SshHost { name: r.name, ssh })
}

fn handoff(id: &str) -> Result<handoff::Handoff, String> {
    Registry::default()
        .get(id)
        .ok_or_else(|| format!("no handoff {id:?}"))
}

fn call(c: Call) -> Result<Value, String> {
    let reg = Registry::default();
    Ok(match c {
        Call::Status => {
            let g = MANAGER.lock();
            let list: Vec<_> = g
                .as_ref()
                .map(|m| m.supervisors.values().map(|s| s.status()).collect())
                .unwrap_or_default();
            json!(list)
        }
        Call::Plan { name, bins } => {
            let (r, ssh) = ssh_for(&name)?;
            let plan = install::plan(
                &ssh,
                r.socket.as_deref(),
                bins.as_deref().map(std::path::Path::new),
            )
            .map_err(|e| e.to_string())?;
            let mut v = serde_json::to_value(&plan).map_err(|e| e.to_string())?;
            v["labels"] = json!(plan.methods.iter().map(|m| m.label()).collect::<Vec<_>>());
            v
        }
        Call::Install { name, method, bins } => {
            let (_, ssh) = ssh_for(&name)?;
            let host = thurm_remote::probe(&ssh).map_err(|e| e.to_string())?;
            let bins = bins.map(PathBuf::from);
            let info = install::install(&ssh, &host, method, bins.as_deref())
                .map_err(|e| e.to_string())?;
            json!(info)
        }
        Call::UpgradeDaemon {
            name,
            allow_restart,
        } => {
            let (r, ssh) = ssh_for(&name)?;
            let plan = install::plan(&ssh, r.socket.as_deref(), None).map_err(|e| e.to_string())?;
            match install::upgrade_daemon(
                &ssh,
                r.socket.as_deref(),
                plan.daemon.as_ref(),
                allow_restart,
            ) {
                Ok(out) => json!({ "ok": true, "output": out }),
                Err(install::UpgradeError::WouldStopPanes { protocol }) => {
                    json!({ "ok": false, "would_stop_panes": true, "protocol": protocol })
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        Call::Add {
            name,
            target,
            tailnet,
        } => {
            thurm_config::validate_remote_name(&name)?;
            if Config::load()
                .map_err(|e| e.to_string())?
                .remote(&name)
                .is_some()
            {
                return Err(format!("a remote named {name:?} exists already"));
            }
            if let Some(tailnet) = tailnet {
                return add_tailnet(name, target, &tailnet);
            }
            thurm_remote::ssh::prepare_dir().map_err(|e| e.to_string())?;
            let ssh = Ssh::new(&target)?;
            let effective = ssh.effective_config().map_err(|e| e.to_string())?;
            thurm_remote::ssh::check_forwards(&effective)?;
            let host = thurm_remote::probe(&ssh).map_err(|e| e.to_string())?;
            let entry = thurm_config::RemoteConfig {
                name: name.clone(),
                host: target,
                socket: None,
                enabled: true,
                clipboard_read: thurm_config::ClipboardRead::Ask,
                tailnet: None,
                forward_links: true,
            };
            thurm_config::edit_remotes(|t| thurm_config::with_remote_added(t, &entry))?;
            if let Some(m) = MANAGER.lock().as_mut() {
                sync(m);
            }
            json!({ "name": name, "host": host })
        }
        Call::Remove { name } => {
            thurm_config::edit_remotes(|t| thurm_config::with_remote_removed(t, &name))?;
            if let Some(m) = MANAGER.lock().as_mut() {
                sync(m);
            }
            json!("Ok")
        }
        Call::Set { name, key, value } => {
            let literal = remote_setting_literal(&key, &value)?;
            thurm_config::edit_remotes(|t| {
                thurm_config::with_remote_setting(t, &name, &key, &literal)
            })?;
            if let Some(m) = MANAGER.lock().as_mut() {
                sync(m);
            }
            json!("Ok")
        }
        Call::Doctor { name, bins } => {
            let (r, ssh) = ssh_for(&name)?;
            let report = doctor::run(
                &ssh,
                &name,
                r.socket.as_deref(),
                bins.as_deref().map(std::path::Path::new),
            )
            .map_err(|e| e.to_string())?;
            json!(report)
        }
        Call::DoctorFix {
            name,
            id,
            bins,
            allow_restart,
        } => {
            let (r, ssh) = ssh_for(&name)?;
            let out = doctor::fix(
                &ssh,
                r.socket.as_deref(),
                bins.as_deref().map(std::path::Path::new),
                &id,
                allow_restart,
            )
            .map_err(|e| e.to_string())?;
            json!({ "output": out })
        }
        Call::TerminalArgv { name, script } => {
            let (_, ssh) = ssh_for(&name)?;
            json!(ssh.interactive_argv(&script)?)
        }
        Call::AllowClipboard { name } => {
            thurm_config::edit_remotes(|t| {
                thurm_config::with_remote_setting(t, &name, "clipboard_read", "\"always\"")
            })?;
            json!("Ok")
        }
        Call::ClipboardRead { host } => {
            let cfg = Config::load().unwrap_or_default();
            json!(policy::clipboard_read(&cfg, host.as_deref()))
        }
        Call::ClipboardWrite { host } => {
            let cfg = Config::load().unwrap_or_default();
            json!(policy::clipboard_write(&cfg, host.as_deref()))
        }
        Call::Link { host, url } => {
            static LINKS: policy::Links = policy::Links::new();
            json!(LINKS.link(Config::load().ok(), host.as_deref(), &url))
        }
        Call::HandoffPrepare { host, path, branch } => {
            let h = handoff::prepare_registered(
                &host_for(&host)?,
                std::path::Path::new(&path),
                branch.as_deref(),
                &reg,
            )?;
            json!(h)
        }
        Call::HandoffSetPane { id, pane } => {
            let h = reg
                .update(&id, |h| h.pane = pane)?
                .ok_or_else(|| format!("no handoff {id:?}"))?;
            json!(h)
        }
        Call::HandoffFetch { id } => {
            let mut h = handoff(&id)?;
            let res = handoff::fetch(&host_for(&h.host)?, &mut h);
            let h = reg.record_fetch(&h)?.unwrap_or(h);
            match res {
                Ok(_) => json!(h),
                Err(e) => return Err(e),
            }
        }
        Call::HandoffCheck { id } => {
            let h = handoff(&id)?;
            json!(handoff::check_cleanup(&host_for(&h.host)?, &h)?)
        }
        Call::HandoffCleanup { id, force } => {
            let mut h = handoff(&id)?;
            match handoff::cleanup(&host_for(&h.host)?, &mut h, force) {
                Ok(out) => {
                    reg.remove(&id)?;
                    json!(out)
                }
                Err(e) => {
                    reg.record_fetch(&h)?;
                    return Err(e);
                }
            }
        }
        Call::HandoffDefer { id } => {
            let h = reg
                .update(&id, |h| h.pending_cleanup = true)?
                .ok_or_else(|| format!("no handoff {id:?}"))?;
            json!(h)
        }
        Call::HandoffList => json!(reg.list()),
        Call::PairingHosts => {
            thurm_remote::ssh::prepare_dir().map_err(|e| e.to_string())?;
            let cfg = Config::load().map_err(|e| e.to_string())?;
            let entries: Vec<_> = cfg
                .remote
                .iter()
                .filter(|r| r.enabled)
                .map(|r| match Ssh::new(&r.host) {
                    Ok(ssh) => pairing::host_entry(r, &ssh),
                    Err(e) => pairing::HostEntry {
                        name: r.name.clone(),
                        error: Some(e),
                        ..Default::default()
                    },
                })
                .collect();
            json!(entries)
        }
        Call::AuthorizeDevice { name, key } => {
            let key = pairing::DeviceKey::parse(&key)?;
            let (_, ssh) = ssh_for(&name)?;
            match pairing::authorize(&ssh, &key) {
                Ok(()) => json!({ "device": key.device }),
                Err(e @ pairing::PairingError::NotWritable { nixos }) => json!({
                    "error": e.to_string(),
                    "not_writable": true,
                    "line": key.line(),
                    "nixos": nixos.then(|| pairing::nixos_snippet(&key)),
                }),
                Err(e) => return Err(e.to_string()),
            }
        }
        Call::RevokeDevice { name, device } => {
            let (_, ssh) = ssh_for(&name)?;
            pairing::revoke(&ssh, &device).map_err(|e| e.to_string())?;
            json!("Ok")
        }
        Call::PairedDevices { name } => {
            let (_, ssh) = ssh_for(&name)?;
            json!(pairing::devices(&ssh).map_err(|e| e.to_string())?)
        }
        Call::ForwardOpen {
            host,
            port,
            url,
            local,
        } => {
            let f = forwards();
            // Before reading where the host is: if it is pointed elsewhere from here on, this
            // open (checked against the old destination) is refused rather than attached to the
            // new one.
            let since = f.epoch(&host);
            let cfg = Config::load().map_err(|e| e.to_string())?;
            let port = forward_port(&cfg, &host, port, url.as_deref())?;
            f.check(&host)?;
            // With its link from the start: the change it reports lists the URL it opens at.
            let (fw, new) = f.open_since(&host, port, local, url.as_deref(), since)?;
            let mut v = forward_json(&fw, url.as_deref());
            v["new"] = json!(new);
            v
        }
        Call::ForwardClose { id } => json!(forward_json(&forwards().close(id)?, None)),
        Call::ForwardList => json!(forward_list(forwards())),
        Call::ForwardGet { id } => {
            forward_json(&forwards().get(id).ok_or("that forward has closed")?, None)
        }
        Call::PortsChanged {
            host,
            pane,
            ports,
            complete,
        } => {
            forwards().ports_changed(&host, pane, ports, complete);
            json!("Ok")
        }
        Call::KnownPorts { host } => json!(forwards().known(&host)),
        Call::HostDisconnected { host } => {
            forwards().host_disconnected(&host);
            json!("Ok")
        }
        Call::HostPanes { host, panes } => {
            forwards().host_panes(&host, &panes);
            json!("Ok")
        }
        Call::LocalProbe => {
            let host = thurm_remote::probe(&LocalShell).map_err(|e| e.to_string())?;
            json!({
                "found": host.thurm.is_some(),
                "protocol_ok": host.speaks_our_protocol(),
            })
        }
    })
}

/// `value` for `key` of a `[[remote]]` as a TOML literal, if the window may set that key.
fn remote_setting_literal(key: &str, value: &serde_json::Value) -> Result<String, String> {
    // A JSON string is also a TOML basic string.
    match (key, value) {
        ("host", serde_json::Value::String(s)) => {
            thurm_remote::ssh::validate_target(s.trim())?;
            Ok(json!(s.trim()).to_string())
        }
        ("enabled" | "forward_links", serde_json::Value::Bool(b)) => Ok(b.to_string()),
        ("tailnet", serde_json::Value::String(s)) => {
            thurm_config::parse_remote_tailnet(s)?;
            Ok(json!(s.trim()).to_string())
        }
        ("clipboard_read", serde_json::Value::String(s))
            if matches!(s.as_str(), "ask" | "always" | "never") =>
        {
            Ok(value.to_string())
        }
        _ => Err(format!("{key} can't be set to {value}")),
    }
}

/// Adds `name` reached over the tailnet at `tailnet` (empty: the host of the ssh `target`, as a
/// peer's name), once its daemon answers there.
fn add_tailnet(name: String, target: String, tailnet: &str) -> Result<Value, String> {
    let target = target.trim().to_owned();
    thurm_remote::ssh::validate_target(&target)?;
    let value = if tailnet.trim().is_empty() {
        thurm_remote::tailnet::peer_name(&target)
    } else {
        tailnet.trim()
    };
    let checked = thurm_remote::tailnet::check(
        value,
        &thurm_config::tailnet_dial_socket_path(),
        thurm_remote::tailnet::CONNECT_TIMEOUT,
    )?;
    let entry = thurm_config::RemoteConfig {
        name: name.clone(),
        host: target,
        socket: None,
        enabled: true,
        clipboard_read: thurm_config::ClipboardRead::Ask,
        tailnet: Some(checked.url.clone()),
        forward_links: true,
    };
    thurm_config::edit_remotes(|t| thurm_config::with_remote_added(t, &entry))?;
    if let Some(m) = MANAGER.lock().as_mut() {
        sync(m);
    }
    Ok(json!({ "name": name, "tailnet": checked }))
}

/// One remote operation: `json` is {"op": "...", ...} (see `Call`); answers the result as
/// JSON or {"error": "..."}. Operations that talk to a host block (ssh); call them off the
/// main thread. Free the result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_remote_call(json: *const c_char) -> *mut c_char {
    let Some(text) = (unsafe { opt_str(json) }) else {
        return into_c(crate::error_json("invalid string"));
    };
    let out = match serde_json::from_str::<Call>(text) {
        Ok(c) => match call(c) {
            Ok(v) => v.to_string(),
            Err(e) => crate::error_json(e),
        },
        Err(e) => crate::error_json(format!("bad call: {e}")),
    };
    into_c(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_parse() {
        for s in [
            r#"{"op":"status"}"#,
            r#"{"op":"plan","name":"devbox","bins":null}"#,
            r#"{"op":"install","name":"devbox","method":"download","bins":"/x"}"#,
            r#"{"op":"upgrade_daemon","name":"devbox","allow_restart":false}"#,
            r#"{"op":"clipboard_read","host":"devbox"}"#,
            r#"{"op":"link","host":null,"url":"https://x"}"#,
            r#"{"op":"handoff_prepare","host":"devbox","path":"/r","branch":null}"#,
            r#"{"op":"handoff_cleanup","id":"a/b/c","force":true}"#,
            r#"{"op":"handoff_list"}"#,
            r#"{"op":"add","name":"pi","target":"me@pi"}"#,
            r#"{"op":"doctor","name":"pi","bins":null}"#,
            r#"{"op":"doctor_fix","name":"pi","id":"hooks.claude","bins":null}"#,
            r#"{"op":"terminal_argv","name":"pi","script":"exec claude"}"#,
            r#"{"op":"remove","name":"pi"}"#,
            r#"{"op":"set","name":"pi","key":"enabled","value":false}"#,
            r#"{"op":"pairing_hosts"}"#,
            r#"{"op":"authorize_device","name":"pi","key":"ecdsa-sha2-nistp256 AAAA thurm-ios:iPhone"}"#,
            r#"{"op":"revoke_device","name":"pi","device":"iPhone"}"#,
            r#"{"op":"paired_devices","name":"pi"}"#,
            r#"{"op":"local_probe"}"#,
            r#"{"op":"forward_open","host":"pi","port":5173}"#,
            r#"{"op":"forward_open","host":"pi","url":"http://localhost:5173/","local":5174}"#,
            r#"{"op":"forward_close","id":3}"#,
            r#"{"op":"forward_list"}"#,
            r#"{"op":"forward_get","id":3}"#,
            r#"{"op":"ports_changed","host":"pi","pane":2,"ports":[{"port":5173,"address":"127.0.0.1","pid":9,"command":"vite"}],"complete":true}"#,
            r#"{"op":"known_ports","host":"pi"}"#,
            // As the app relays a PortsChanged: with `initial`, a list like any other here.
            r#"{"op":"ports_changed","host":"pi","pane":2,"ports":[],"complete":true,"initial":true}"#,
            r#"{"op":"host_disconnected","host":"pi"}"#,
            r#"{"op":"host_panes","host":"pi","panes":[]}"#,
            r#"{"op":"host_panes","host":"pi","panes":[2,3]}"#,
        ] {
            serde_json::from_str::<Call>(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        }
        let v = call(Call::Link {
            host: Some("devbox".into()),
            url: "file:///etc/hosts".into(),
        })
        .unwrap();
        assert_eq!(v, json!({"action": "copy_path", "path": "/etc/hosts"}));
        let v = call(Call::ClipboardRead { host: None }).unwrap();
        assert_eq!(v, json!("allow"));
        // A bad key is refused before looking for the host.
        let e = call(Call::AuthorizeDevice {
            name: "nowhere".into(),
            key: "ssh-ed25519 AAAA thurm-ios:iPhone".into(),
        })
        .unwrap_err();
        assert!(e.contains("not a Thurm device key"), "{e}");
    }

    #[test]
    fn an_unreadable_config_stops_no_tunnel() {
        let remote = |name: &str| thurm_config::RemoteConfig {
            name: name.into(),
            host: "me@nowhere".into(),
            socket: None,
            // Turned off: its supervisor makes no connection.
            enabled: false,
            clipboard_read: Default::default(),
            tailnet: None,
            forward_links: true,
        };
        let sup = Supervisor::start(
            remote("devbox"),
            Ssh::new("me@nowhere").unwrap(),
            false,
            |_| {},
        );
        let mut m = Manager {
            supervisors: HashMap::from([("devbox".to_owned(), sup)]),
            cb: None,
            ctx: Ctx(std::ptr::null_mut()),
            destinations: HashMap::new(),
        };
        // Being rewritten: nothing stops.
        sync_with(&mut m, Err("expected `=`".into()));
        assert!(m.supervisors.contains_key("devbox"));
        // Read, without the host: its tunnel stops.
        sync_with(&mut m, Ok(Config::parse("").unwrap()));
        assert!(m.supervisors.is_empty());
    }

    #[test]
    fn a_move_the_tunnel_cannot_take_does_not_shut_its_forwards_out_for_good() {
        // A name of its own: the app's forwards are shared by these tests.
        let name = "bad-move";
        let remote = |host: &str| thurm_config::RemoteConfig {
            name: name.into(),
            host: host.into(),
            socket: None,
            enabled: false,
            clipboard_read: Default::default(),
            tailnet: None,
            forward_links: true,
        };
        let cfg = |host: &str| {
            let mut c = Config::parse("").unwrap();
            c.remote = vec![remote(host)];
            c
        };
        // Its tunnel, connected with the settings it starts with.
        let tunnel = forwards().host_started(name);
        let sup = Supervisor::start(remote("me@one"), Ssh::new("me@one").unwrap(), false, |_| {});
        forwards().host_connected(name, tunnel, sup.settings());
        let mut m = Manager {
            supervisors: HashMap::from([(name.to_owned(), sup)]),
            cb: None,
            ctx: Ctx(std::ptr::null_mut()),
            destinations: HashMap::new(),
        };
        sync_with(&mut m, Ok(cfg("me@one")));
        assert_eq!(forwards().opens(name), Ok(()));
        // Pointed at an ssh target the tunnel refuses: it keeps going where it went.
        sync_with(&mut m, Ok(cfg("-oProxyCommand=x")));
        assert_eq!(forwards().opens(name), Ok(()), "forwards shut out for good");
    }

    #[test]
    fn reports_list_forwards_as_the_app_reads_them_and_in_order() {
        let dir = std::env::temp_dir().join(format!("thurm-ffi-reports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = Forwards::new(dir.join("forwards.json"), dir_socket);
        let reports = std::sync::Arc::new(Mutex::new(Vec::<Value>::new()));
        let theirs = reports.clone();
        report_changes(&f, move |r| {
            theirs.lock().push(serde_json::from_str(r).unwrap())
        });
        let (fw, _) = f.open("devbox", 7000, Some(0)).unwrap();
        f.close(fw.id).unwrap();
        {
            let r = reports.lock();
            assert_eq!(r[0]["change"]["change"], "opened");
            assert_eq!(r[0]["change"]["forward"]["local_port"], fw.local);
            assert_eq!(r[0]["forwards"][0]["local_port"], fw.local);
            assert_eq!(r[1]["change"]["why"], "closed");
            assert_eq!(r[1]["change"]["forward"]["local_port"], fw.local);
            assert_eq!(r[1]["forwards"], json!([]));
        }
        // Forwards opened and closed on many threads at once: the last report is what is open.
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let f = f.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        let (fw, _) = f.open(&format!("h{i}"), 8000 + j, Some(0)).unwrap();
                        if j % 2 == 0 {
                            f.close(fw.id).unwrap();
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let last = reports.lock().last().unwrap()["forwards"].clone();
        assert_eq!(last, json!(forward_list(&f)));
        for fw in f.list() {
            f.close(fw.id).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_config_closes_no_forward() {
        let dir = std::env::temp_dir().join(format!("thurm-ffi-reconcile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = Forwards::new(dir.join("forwards.json"), dir_socket);
        let (fw, _) = f.open("devbox", 7000, Some(0)).unwrap();
        let on = Config::parse("[[remote]]\nname = \"devbox\"\nhost = \"a\"\n").unwrap();
        let mut destinations = HashMap::new();
        reconcile_forwards(&f, &mut destinations, Ok(&on));
        assert_eq!(f.list(), vec![fw.clone()]);
        // The config can't be read for a moment (being rewritten): nothing changes.
        reconcile_forwards(&f, &mut destinations, Err("expected `=`".into()));
        assert_eq!(f.list(), vec![fw.clone()]);
        assert!(
            destinations.contains_key("devbox"),
            "where hosts point is kept"
        );
        // Read again, without the host: its forward closes.
        let none = Config::parse("").unwrap();
        reconcile_forwards(&f, &mut destinations, Ok(&none));
        assert!(f.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn dir_socket(host: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/nonexistent/{host}.sock"))
    }

    #[test]
    fn a_host_pointed_elsewhere_has_moved() {
        let cfg = |devbox: &str| {
            Config::parse(&format!(
                "[[remote]]\nname = \"devbox\"\n{devbox}\n[[remote]]\nname = \"pi\"\nhost = \"pi\"\n"
            ))
            .unwrap()
        };
        let before: HashMap<String, Destination> = cfg("host = \"a\"")
            .remote
            .iter()
            .map(|r| (r.name.clone(), destination(r)))
            .collect();
        // Settings that keep the destination: not moved.
        assert!(
            moved(
                &before,
                &cfg("host = \"a\"\nforward_links = false\nclipboard_read = \"never\"")
            )
            .is_empty()
        );
        assert!(moved(&before, &cfg("host = \"a\"\nenabled = false")).is_empty());
        // Another ssh target, socket or tailnet address under the same name: moved.
        assert_eq!(moved(&before, &cfg("host = \"b\"")), ["devbox"]);
        assert_eq!(
            moved(&before, &cfg("host = \"a\"\nsocket = \"/run/x.sock\"")),
            ["devbox"]
        );
        assert_eq!(
            moved(
                &before,
                &cfg("host = \"a\"\ntailnet = \"wss://devbox.tail1.ts.net/v1/connect\"")
            ),
            ["devbox"]
        );
        // A host added since: nothing to move.
        assert!(moved(&HashMap::new(), &cfg("host = \"b\"")).is_empty());
    }

    #[test]
    fn forwards_only_for_hosts_that_are_on() {
        let cfg = Config::parse(
            "[[remote]]\nname = \"on\"\nhost = \"a\"\n[[remote]]\nname = \"off\"\nhost = \"b\"\nenabled = false\n",
        )
        .unwrap();
        assert_eq!(forward_port(&cfg, "on", Some(5173), None), Ok(5173));
        assert_eq!(
            forward_port(&cfg, "on", None, Some("https://localhost:8443/x")),
            Ok(8443)
        );
        let e = forward_port(&cfg, "off", Some(5173), None).unwrap_err();
        assert!(e.contains("turned off"), "{e}");
        assert!(forward_port(&cfg, "gone", Some(5173), None).is_err());
        assert!(forward_port(&cfg, "on", None, Some("https://example.com")).is_err());
        assert!(forward_port(&cfg, "on", None, None).is_err());
        // A port and a link that agree; and that don't.
        assert_eq!(
            forward_port(&cfg, "on", Some(8443), Some("https://localhost:8443/x")),
            Ok(8443)
        );
        let e = forward_port(&cfg, "on", Some(5173), Some("http://localhost:3000/")).unwrap_err();
        assert!(e.contains("differ"), "{e}");
        assert!(forward_port(&cfg, "on", Some(5173), Some("https://example.com/")).is_err());
    }

    #[test]
    fn a_listed_forward_opens_at_the_link_it_was_opened_for() {
        let mut fw = forward::Forward {
            id: 1,
            host: "devbox".into(),
            remote_port: 8443,
            at: Default::default(),
            local: 18443,
            link: None,
        };
        assert_eq!(forward_json(&fw, None)["url"], "http://localhost:18443/");
        fw.link = Some("https://localhost:8443/app".into());
        assert_eq!(
            forward_json(&fw, None)["url"],
            "https://localhost:18443/app"
        );
        assert_eq!(
            forward_json(&fw, Some("http://127.0.0.1:8443/b"))["url"],
            "http://127.0.0.1:18443/b"
        );
    }

    #[test]
    fn remote_setting_literals() {
        assert_eq!(
            remote_setting_literal("enabled", &json!(false)).unwrap(),
            "false"
        );
        assert_eq!(
            remote_setting_literal("host", &json!(" me@pi ")).unwrap(),
            "\"me@pi\""
        );
        assert_eq!(
            remote_setting_literal("clipboard_read", &json!("never")).unwrap(),
            "\"never\""
        );
        assert!(remote_setting_literal("host", &json!("")).is_err());
        assert!(remote_setting_literal("host", &json!("a b")).is_err());
        assert!(remote_setting_literal("host", &json!("-oProxyCommand=evil")).is_err());
        assert!(remote_setting_literal("host", &json!(1)).is_err());
        assert!(remote_setting_literal("clipboard_read", &json!("maybe")).is_err());
        assert!(remote_setting_literal("enabled", &json!("yes")).is_err());
        assert_eq!(
            remote_setting_literal("forward_links", &json!(false)).unwrap(),
            "false"
        );
        assert!(remote_setting_literal("name", &json!("x")).is_err());
        assert_eq!(
            remote_setting_literal("tailnet", &json!("wss://bastion.tail1234.ts.net")).unwrap(),
            "\"wss://bastion.tail1234.ts.net\""
        );
        assert!(remote_setting_literal("tailnet", &json!("https://bastion")).is_err());
    }
}
