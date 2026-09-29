//! Remote workspaces for the app: the tunnel supervisors (one per enabled `[[remote]]`), the
//! install and handoff operations, and the policy for remote panes. See `thurm.h`.

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use thurm_config::Config;
use thurm_remote::handoff::{self, Registry, SshHost};
use thurm_remote::install::{self, Method};
use thurm_remote::tunnel::Supervisor;
use thurm_remote::{Ssh, policy};

use crate::{Ctx, into_c, opt_str};

pub type thurm_remote_status_cb =
    Option<unsafe extern "C" fn(ctx: *mut c_void, json: *const c_char)>;

struct Manager {
    supervisors: HashMap<String, Supervisor>,
    cb: thurm_remote_status_cb,
    ctx: Ctx,
}

static MANAGER: Mutex<Option<Manager>> = Mutex::new(None);

fn emit(cb: thurm_remote_status_cb, ctx: Ctx, json: &str) {
    if let (Some(cb), Ok(s)) = (cb, std::ffi::CString::new(json)) {
        unsafe { cb(ctx.0, s.as_ptr()) };
    }
}

/// Starts or stops supervisors so they match the config's enabled remotes; updates changed
/// ones.
fn sync(m: &mut Manager) {
    let cfg = Config::load().unwrap_or_default();
    let wanted: HashMap<String, thurm_config::RemoteConfig> = cfg
        .remote
        .iter()
        .map(|r| (r.name.clone(), r.clone()))
        .collect();
    m.supervisors.retain(|name, _| wanted.contains_key(name));
    for (name, r) in wanted {
        match m.supervisors.get(&name) {
            Some(s) => {
                if let Err(e) = s.update(r) {
                    log::warn!("remote {name}: {e}");
                }
            }
            None => {
                let ssh = match Ssh::new(&r.host) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("remote {name}: {e}");
                        continue;
                    }
                };
                let (cb, ctx) = (m.cb, m.ctx);
                let sup = Supervisor::start(r, ssh, true, move |status| {
                    if let Ok(j) = serde_json::to_string(status) {
                        emit(cb, ctx, &j);
                    }
                });
                m.supervisors.insert(name, sup);
            }
        }
    }
}

/// Starts supervising every enabled `[[remote]]`. `on_status` gets each host's status JSON
/// on every change (on a background thread). Calling it again replaces the callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_remotes_start(on_status: thurm_remote_status_cb, ctx: *mut c_void) {
    let mut g = MANAGER.lock();
    if let Some(m) = g.as_mut() {
        m.cb = on_status;
        m.ctx = Ctx(ctx);
        // Supervisors keep their old callback; restart them with the new one.
        m.supervisors.clear();
        sync(m);
        return;
    }
    let mut m = Manager {
        supervisors: HashMap::new(),
        cb: on_status,
        ctx: Ctx(ctx),
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
        Call::AllowClipboard { name } => {
            thurm_config::edit_config(|t| {
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
        Call::Link { host, url } => json!(policy::link(host.as_deref(), &url)),
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
    })
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
    }
}
