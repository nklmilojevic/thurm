//! Remote workspaces (Remote.swift): `thurmd` on other machines through supervised ssh
//! tunnels. Connects when a tunnel is up, shows panes offline while it is not, applies the
//! clipboard policy, runs handoffs, and the Remotes window (install, upgrade, doctor, add).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use serde_json::{Value, json};
use thurm_proto::layout::TabLayout;
use thurm_proto::{AgentPreset, PaneInfo};

use crate::app::{self, App, leaves};
use crate::core::{self, Core, LOCAL, PaneKey};
use crate::dialogs::{self, Style, button};
use crate::model::SplitNode;
use crate::palette::Item;

#[derive(Default)]
pub struct Remotes {
    statuses: RefCell<HashMap<String, Value>>,
    presets: RefCell<HashMap<String, Vec<AgentPreset>>>,
    handoffs: RefCell<Vec<Value>>,
    handoff_errors: RefCell<HashMap<String, String>>,
    countdown: RefCell<Option<glib::SourceId>>,
    busy: RefCell<HashMap<String, String>>,
    reports: RefCell<HashMap<String, Value>>,
    fixing: RefCell<Option<(String, String)>>,
    window: RefCell<Option<Rc<RemotesWindow>>>,
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

impl Remotes {
    pub fn status(&self, host: &str) -> Option<Value> {
        self.statuses.borrow().get(host).cloned()
    }

    fn phase(&self, host: &str) -> Option<String> {
        self.status(host)
            .and_then(|s| s.get("phase").and_then(Value::as_str).map(str::to_string))
    }

    pub fn phase_label(&self, app: &App, host: &str) -> String {
        if host == LOCAL || app.is_connected(host) {
            return "connected".into();
        }
        match self.phase(host).as_deref() {
            Some("needs_attention") => "needs attention".into(),
            Some("not_installed") => "not installed".into(),
            Some("upgrade_needed") => "upgrade needed".into(),
            Some(p) => p.to_string(),
            None => "connecting".into(),
        }
    }

    /// The notice over `host`'s panes while it is not connected.
    pub fn offline_message(&self, app: &App, host: &str) -> Option<String> {
        if app.is_connected(host) {
            return None;
        }
        let status = self.status(host);
        let message = status
            .as_ref()
            .and_then(|s| s.get("message").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        Some(match status.as_ref().and_then(|s| s.get("phase").and_then(Value::as_str)) {
            None | Some("connecting") => format!("Connecting to {host}…"),
            Some("needs_attention") => {
                let mut m = format!("{host} needs attention");
                if !message.is_empty() {
                    m.push('\n');
                    m.push_str(&message);
                }
                m + "\nThurm › Remotes… to retry"
            }
            Some("not_installed") => {
                format!("Thurm is not installed on {host}.\nThurm › Remotes… to install it.")
            }
            Some("upgrade_needed") => format!(
                "{host} runs another version of Thurm\n{}\nThurm › Remotes… to upgrade it.",
                first_line(&message)
            ),
            Some("disabled") => format!("{host} is disabled (enabled = false in its [[remote]])."),
            Some(_) => {
                let retry = status
                    .as_ref()
                    .and_then(|s| s.get("retry_at").and_then(Value::as_f64))
                    .map(|t| (t - now()).ceil());
                let mut m = match retry {
                    Some(s) if s > 0.0 => format!("Disconnected — reconnecting in {s} s…"),
                    _ => "Disconnected — reconnecting…".into(),
                };
                if !message.is_empty() {
                    m.push('\n');
                    m.push_str(first_line(&message));
                }
                m
            }
        })
    }

    pub fn presets(&self, host: &str) -> Vec<AgentPreset> {
        self.presets.borrow().get(host).cloned().unwrap_or_default()
    }

    pub fn handoff(&self, id: &str) -> Option<Value> {
        self.handoffs
            .borrow()
            .iter()
            .find(|h| h.get("id").and_then(Value::as_str) == Some(id))
            .cloned()
    }

    pub fn handoff_error(&self, id: &str) -> Option<String> {
        self.handoff_errors.borrow().get(id).cloned()
    }

    pub fn handoff_for_pane(&self, key: &PaneKey) -> Option<String> {
        self.handoffs.borrow().iter().find_map(|h| {
            (h.get("host").and_then(Value::as_str) == Some(key.host.as_str())
                && h.get("pane").and_then(Value::as_u64) == Some(key.id))
                .then(|| h.get("id").and_then(Value::as_str).map(str::to_string))
                .flatten()
        })
    }

    fn reload_handoffs(&self) {
        let list = core::remote_call(&json!({"op": "handoff_list"}));
        *self.handoffs.borrow_mut() = list.as_array().cloned().unwrap_or_default();
    }

    /// A close for a pane of an offline host, applied when it is back (persisted).
    pub fn queue_close(&self, key: &PaneKey, pid: u32) {
        let mut all = pending_closes();
        all.entry(key.host.clone()).or_default().insert(key.id, pid);
        save_pending_closes(&all);
    }
}

fn pending_closes() -> HashMap<String, HashMap<u64, u32>> {
    let list = crate::integrations::load_state_list("pending_remote_closes");
    let mut out: HashMap<String, HashMap<u64, u32>> = HashMap::new();
    for entry in list {
        // "host:id:pid"
        let parts: Vec<&str> = entry.rsplitn(3, ':').collect();
        if let [pid, id, host] = parts[..]
            && let (Ok(id), Ok(pid)) = (id.parse(), pid.parse())
        {
            out.entry(host.to_string()).or_default().insert(id, pid);
        }
    }
    out
}

fn save_pending_closes(all: &HashMap<String, HashMap<u64, u32>>) {
    let list: Vec<String> = all
        .iter()
        .flat_map(|(h, m)| m.iter().map(move |(id, pid)| format!("{h}:{id}:{pid}")))
        .collect();
    crate::integrations::save_state_list("pending_remote_closes", &list);
}

pub fn start(app: &Rc<App>) {
    app.remotes.reload_handoffs();
    core::remotes_start();
    // Waking up or a new network: retry every tunnel now.
    let monitor = gtk::gio::NetworkMonitor::default();
    monitor.connect_network_changed(|_, _| core::remote_kick(None, true));
}

/// A tunnel's state changed.
pub fn status_changed(app: &Rc<App>, status: Value) {
    let Some(name) = status.get("name").and_then(Value::as_str).map(str::to_string) else {
        return;
    };
    let phase = status.get("phase").and_then(Value::as_str).unwrap_or("").to_string();
    let socket = status.get("socket").and_then(Value::as_str).unwrap_or("").to_string();
    let old_phase = app.remotes.phase(&name);
    app.remotes.statuses.borrow_mut().insert(name.clone(), status);
    // The Remotes window's checklist follows the host's state.
    let shown = app.remotes.window.borrow().as_ref().and_then(|w| w.selected.borrow().clone());
    if old_phase.as_deref() != Some(phase.as_str())
        && shown.as_deref() == Some(name.as_str())
        && app.remotes.reports.borrow().contains_key(&name)
    {
        doctor(app, &name);
    }
    if phase == "connected" {
        if !app.is_connected(&name) {
            let epoch = app.next_epoch(&name);
            match Core::connect_remote(&name, &socket, epoch) {
                Ok(core) => {
                    app.set_core(&name, Some(core));
                    // set_core bumps nothing on insert; keep the epoch we connected with.
                    remote_connected(app, &name);
                }
                Err(e) => {
                    log::warn!("connect to {name}: {e}");
                    core::remote_kick(Some(&name), true);
                }
            }
        }
    } else if app.is_connected(&name) {
        connection_lost(app, &name);
    }
    update_overlays(app, &name);
    sync_countdown(app);
    if let Some(w) = app.remotes.window.borrow().as_ref() {
        w.reload(app);
    }
    app.refresh_sidebar();
}

/// A remote connection closed: its panes show the offline notice until it is back.
pub fn connection_lost(app: &Rc<App>, host: &str) {
    app.set_core(host, None);
    update_overlays(app, host);
    sync_countdown(app);
    app.refresh_sidebar();
    core::remote_kick(Some(host), true);
}

fn update_overlays(app: &App, host: &str) {
    let message = app.remotes.offline_message(app, host);
    for v in app.views() {
        if v.key.host == host {
            v.set_offline(message.clone());
        }
    }
}

/// A 1 s timer refreshes the "reconnecting in N s" countdowns while one is pending.
fn sync_countdown(app: &App) {
    let needed = app.remotes.statuses.borrow().iter().any(|(h, s)| {
        !app.is_connected(h) && s.get("retry_at").and_then(Value::as_f64).is_some()
    });
    let running = app.remotes.countdown.borrow().is_some();
    if needed && !running {
        let id = glib::timeout_add_seconds_local(1, || {
            app::with_app(|a| {
                let hosts: Vec<String> = a.remotes.statuses.borrow().keys().cloned().collect();
                for h in hosts {
                    if !a.is_connected(&h) {
                        update_overlays(a, &h);
                    }
                }
            });
            glib::ControlFlow::Continue
        });
        *app.remotes.countdown.borrow_mut() = Some(id);
    } else if !needed && let Some(id) = app.remotes.countdown.borrow_mut().take() {
        id.remove();
    }
}

fn remote_connected(app: &Rc<App>, host: &str) {
    let Some(core) = app.core(host) else { return };
    core.send(&json!({"SetAppearance": {"dark": app::system_is_dark()}}));
    let resp = core.request(&json!("ListPanes"));
    let Some(list) = resp.get("Panes").and_then(Value::as_array) else {
        connection_lost(app, host);
        return;
    };
    let mut infos: Vec<PaneInfo> = list
        .iter()
        .filter_map(|p| serde_json::from_value(p.clone()).ok())
        .collect();
    // Closes queued while the host was away.
    let mut all = pending_closes();
    if let Some(pending) = all.get_mut(host) {
        pending.retain(|id, pid| {
            let Some(info) = infos.iter().find(|i| i.id == *id && i.alive) else {
                return false;
            };
            let same = info.restored || (*pid != 0 && info.pid == Some(*pid));
            if !same {
                return false;
            }
            core.request(&json!({"ClosePane": {"pane": id}})) != Value::String("Ok".into())
        });
        let still: HashSet<u64> = pending.keys().copied().collect();
        infos.retain(|i| !still.contains(&i.id));
    }
    save_pending_closes(&all);

    let alive: HashSet<u64> = infos.iter().filter(|i| i.alive).map(|i| i.id).collect();
    // Panes that ended while it was away.
    let known: Vec<PaneKey> = app
        .infos
        .borrow()
        .keys()
        .filter(|k| k.host == host)
        .cloned()
        .collect();
    let mut gone = 0;
    for k in known {
        if !alive.contains(&k.id) {
            app.infos.borrow_mut().remove(&k);
            if app.tab_of(&k).is_some() {
                gone += 1;
            }
            app.remove_pane_from_ui(&k);
        }
    }
    for info in infos.into_iter().filter(|i| i.alive) {
        app.pane_info_updated(PaneKey::new(host, info.id), info);
    }
    for v in app.views() {
        if v.key.host == host {
            v.set_offline(None);
            v.resubscribe();
        }
    }
    // Panes no tab or workspace holds are adopted into a workspace of that host.
    let mut unknown: Vec<u64> = alive
        .iter()
        .copied()
        .filter(|id| {
            let k = PaneKey::new(host, *id);
            app.tab_of(&k).is_none()
                && !app.workspaces.borrow().iter().any(|w| {
                    w.hidden_tabs.iter().any(|t| {
                        leaves(&t.root)
                            .iter()
                            .any(|(h, i)| *i == *id && h.unwrap_or(&w.host) == host)
                    })
                })
        })
        .collect();
    unknown.sort();
    if !unknown.is_empty() {
        let shown = app
            .current_workspace()
            .and_then(|id| app.workspace(id))
            .filter(|w| w.host == host)
            .map(|w| w.id);
        if shown.is_some() {
            for id in unknown {
                let k = PaneKey::new(host, id);
                let handoff = app.remotes.handoff_for_pane(&k);
                app.place_new_tab(k, handoff, false);
            }
        } else {
            let target = app
                .workspaces
                .borrow()
                .iter()
                .find(|w| w.host == host)
                .map(|w| w.id);
            let target = target.unwrap_or_else(|| {
                let ws = app.make_workspace(host);
                let id = ws.id;
                app.workspaces.borrow_mut().push(ws);
                id
            });
            let mut wss = app.workspaces.borrow_mut();
            if let Some(ws) = wss.iter_mut().find(|w| w.id == target) {
                for id in unknown {
                    let k = PaneKey::new(host, id);
                    ws.hidden_tabs.push(TabLayout {
                        title: None,
                        root: SplitNode::Leaf(k.clone()).to_layout(),
                        focused: id,
                        zoomed: None,
                        handoff: app.remotes.handoff_for_pane(&k),
                    });
                }
            }
        }
    }
    let presets: Vec<AgentPreset> = core
        .request(&json!("ListAgentPresets"))
        .get("AgentPresets")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    app.remotes.presets.borrow_mut().insert(host.to_string(), presets);
    if gone > 0 {
        app.toast(
            &format!("{gone} {} on {host} ended while it was away", if gone == 1 { "pane" } else { "panes" }),
            5.0,
        );
    }
    // Handoff cleanups deferred while it was offline.
    let pending: Vec<String> = app
        .remotes
        .handoffs
        .borrow()
        .iter()
        .filter(|h| {
            h.get("host").and_then(Value::as_str) == Some(host)
                && h.get("pending_cleanup").and_then(Value::as_bool) == Some(true)
        })
        .filter_map(|h| h.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    for id in pending {
        let r = core::remote_call(&json!({"op": "handoff_cleanup", "id": id, "force": false}));
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            app.toast(&format!("Handoff kept: {e}"), 6.0);
        }
    }
    app.remotes.reload_handoffs();
    app.focus_changed();
    app.schedule_save();
}

pub fn clipboard_write_allowed(host: &str) -> bool {
    core::remote_call(&json!({"op": "clipboard_write", "host": host})) == Value::String("allow".into())
}

/// OSC 52 read: local panes get the clipboard; remote ones per `clipboard_read`.
pub fn clipboard_request(app: &Rc<App>, key: &PaneKey) {
    let reply = {
        let key = key.clone();
        move |allow: bool| {
            app::with_app(|a| {
                let Some(core) = a.core(&key.host) else { return };
                if !allow {
                    core.send(&json!({"ClipboardReply": {"pane": key.id, "text": ""}}));
                    return;
                }
                let k = key.clone();
                a.display().clipboard().read_text_async(None::<&gtk::gio::Cancellable>, move |res| {
                    let text = res.ok().flatten().map(|t| t.to_string()).unwrap_or_default();
                    app::with_app(|a| {
                        if let Some(c) = a.core(&k.host) {
                            c.send(&json!({"ClipboardReply": {"pane": k.id, "text": text}}));
                        }
                    });
                });
            });
        }
    };
    if !key.is_remote() {
        reply(true);
        return;
    }
    let decision = core::remote_call(&json!({"op": "clipboard_read", "host": key.host}));
    match decision.as_str() {
        Some("allow") => reply(true),
        Some("ask") => {
            let Some(w) = app.win() else { return reply(false) };
            let host = key.host.clone();
            dialogs::ask(
                &w.window,
                &format!("{host} wants to read your clipboard"),
                &format!("A program in a pane on {host} asked for the contents of this computer's clipboard (OSC 52)."),
                &[
                    button("once", "Allow Once", Style::Suggested),
                    button("always", &format!("Always for {host}"), Style::Default),
                    button("cancel", "Deny", Style::Default),
                ],
                move |r| match r.as_str() {
                    "once" => reply(true),
                    "always" => {
                        core::remote_call(&json!({"op": "allow_clipboard", "name": host}));
                        reply(true);
                    }
                    _ => reply(false),
                },
            );
        }
        _ => reply(false),
    }
}

// MARK: handoffs

/// A remote agent finished or asks for something: bring its commits back.
pub fn agent_settled(app: &Rc<App>, key: &PaneKey) {
    if !key.is_remote() {
        return;
    }
    let id = app
        .tab_of(key)
        .and_then(|t| t.handoff.borrow().clone())
        .or_else(|| app.remotes.handoff_for_pane(key));
    if let Some(id) = id {
        fetch_handoff(app, &id, true);
    }
}

fn fetch_handoff(app: &Rc<App>, id: &str, quiet: bool) {
    let before = app.remotes.handoff(id).and_then(|h| h.get("fetched").cloned());
    let id = id.to_string();
    core::remote_call_async(json!({"op": "handoff_fetch", "id": id}), move |r| {
        app::with_app(|a| {
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                a.remotes.handoff_errors.borrow_mut().insert(id.clone(), e.to_string());
                a.toast(&format!("Fetching the handoff failed: {}", first_line(e)), 5.0);
            } else {
                a.remotes.handoff_errors.borrow_mut().remove(&id);
                if !quiet || r.get("fetched").cloned() != before {
                    let host = r.get("host").and_then(Value::as_str).unwrap_or("");
                    let branch = r.get("branch").and_then(Value::as_str).unwrap_or("");
                    a.toast(&format!("Fetched {branch} → thurm-{host}/{branch}"), 5.0);
                }
            }
            a.remotes.reload_handoffs();
            a.refresh_sidebar();
        });
    });
}

/// After the handoff tab closed: remove the worktree (or defer while offline).
pub fn cleanup_handoff(app: &Rc<App>, id: &str, remove: bool) {
    if !remove {
        return;
    }
    let Some(h) = app.remotes.handoff(id) else { return };
    let host = h.get("host").and_then(Value::as_str).unwrap_or("").to_string();
    if !app.is_connected(&host) {
        core::remote_call(&json!({"op": "handoff_defer", "id": id}));
        app.toast(&format!("{host} is offline: the worktree is removed once it is back"), 5.0);
        return;
    }
    run_cleanup(app, id.to_string(), false);
}

fn run_cleanup(app: &Rc<App>, id: String, force: bool) {
    let _ = app;
    core::remote_call_async(json!({"op": "handoff_cleanup", "id": id, "force": force}), move |r| {
        app::with_app(|a| {
            let Some(w) = a.win() else { return };
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                let id = id.clone();
                dialogs::ask(
                    &w.window,
                    "Remove it anyway?",
                    e,
                    &[button("remove", "Remove", Style::Destructive), button("cancel", "Keep", Style::Default)],
                    move |resp| {
                        if resp == "remove" {
                            app::with_app(|a| run_cleanup(a, id.clone(), true));
                        }
                    },
                );
            } else if let Some(m) = r.get("message").and_then(Value::as_str) {
                a.toast(m, 5.0);
            } else {
                dialogs::inform(&w.window, "Could not remove the handoff", &r.to_string());
            }
            a.remotes.reload_handoffs();
        });
    });
}

fn hand_off(app: &Rc<App>, host: &str, repo: &str) {
    let Some(win) = app.win() else { return };
    let mut items = Vec::new();
    let repo_name = repo.rsplit('/').next().unwrap_or(repo).to_string();
    let mut choices: Vec<(String, Option<String>, String)> = app
        .presets(host)
        .into_iter()
        .map(|p| (p.name.clone(), Some(p.name), p.command.join(" ")))
        .collect();
    choices.push(("Shell".into(), None, "type the command yourself".into()));
    for (title, preset, detail) in choices {
        let (h, r) = (host.to_string(), repo.to_string());
        items.push(Item::new(title, detail, move || {
            start_handoff(h.clone(), r.clone(), preset.clone());
        }));
    }
    crate::palette::show(
        &win,
        items,
        &format!("Hand off {repo_name} to {host} with…"),
        Some("↩ hand off"),
        0,
        None,
    );
}

fn start_handoff(host: String, repo: String, preset: Option<String>) {
    app::with_app(|a| a.toast(&format!("Handing off to {host}…"), 20.0));
    core::remote_call_async(
        json!({"op": "handoff_prepare", "host": host, "path": repo, "branch": null}),
        move |h| {
            app::with_app(|a| {
                if let Some(e) = h.get("error").and_then(Value::as_str) {
                    if let Some(w) = a.win() {
                        dialogs::inform(&w.window, &format!("Could not hand off to {host}"), e);
                    }
                    return;
                }
                a.remotes.reload_handoffs();
                let id = h.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let worktree = h.get("worktree").and_then(Value::as_str).map(str::to_string);
                let branch = h.get("branch").and_then(Value::as_str).unwrap_or("").to_string();
                let Some(core) = a.core(&host) else { return };
                let req = json!({"CreatePane": {
                    "command": null, "cwd": worktree, "env": [],
                    "size": {"cols": 120, "rows": 36, "cell_width": a.fonts().cell_w as u16, "cell_height": a.fonts().cell_h as u16},
                    "agent_preset": preset, "inherit_cwd_from": null, "hold": false, "fork_from": null,
                }});
                let Some(pane) = core.request(&req).pointer("/PaneCreated/pane").and_then(Value::as_u64) else {
                    return;
                };
                let key = PaneKey::new(&host, pane);
                a.place_new_tab(key, Some(id.clone()), true);
                core::remote_call_async(json!({"op": "handoff_set_pane", "id": id, "pane": pane}), |_| {
                    app::with_app(|a| a.remotes.reload_handoffs());
                });
                a.toast(&format!("{branch} on {host}: results come back as thurm-{host}/{branch}"), 5.0);
            });
        },
    );
}

pub fn palette_items(app: &Rc<App>) -> Vec<Item> {
    let mut items = Vec::new();
    let names = app.ui().remote_names();
    if names.is_empty() {
        return items;
    }
    if let Some(tab) = app.current_tab()
        && tab.host == LOCAL
    {
        let focused = tab.focused.borrow().clone();
        if let Some(git) = app.pane_info(&focused).and_then(|i| i.git) {
            let repo = git.root.clone();
            let name = repo.rsplit('/').next().unwrap_or(&repo).to_string();
            for host in app.connected_remotes() {
                let r = repo.clone();
                let h = host.clone();
                items.push(Item::new(format!("Hand Off to {host}…"), name.clone(), move || {
                    app::with_app(|a| hand_off(a, &h, &r));
                }));
            }
        }
    }
    if let Some(id) = app.current_tab().and_then(|t| t.handoff.borrow().clone()) {
        let branch = app
            .remotes
            .handoff(&id)
            .and_then(|h| h.get("branch").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        items.push(Item::new("Fetch Handoff Result", branch, move || {
            app::with_app(|a| fetch_handoff(a, &id, false));
        }));
    }
    for host in &names {
        if !app.is_connected(host) {
            let h = host.clone();
            items.push(Item::new(format!("Reconnect to {host}"), app.remotes.phase_label(app, host), move || {
                core::remote_kick(Some(&h), true);
            }));
        }
    }
    items.push(Item::new("Remotes…", "connection state, install, upgrade", || {
        app::with_app(show_window);
    }));
    items
}

// MARK: Remotes window

pub struct RemotesWindow {
    window: adw::Window,
    hosts: gtk::ListBox,
    status: gtk::Label,
    checks: gtk::ListBox,
    add: gtk::Button,
    check_again: gtk::Button,
    retry: gtk::Button,
    selected: RefCell<Option<String>>,
    reloading: Cell<bool>,
}

pub fn show_window(app: &Rc<App>) {
    if let Some(w) = app.remotes.window.borrow().as_ref() {
        w.window.present();
        w.reload(app);
        return;
    }
    let window = adw::Window::new();
    window.set_title(Some("Remotes"));
    window.set_default_size(760, 520);
    if let Some(w) = app.win() {
        window.set_transient_for(Some(&w.window));
    }
    let hosts = gtk::ListBox::new();
    hosts.add_css_class("boxed-list");
    let hosts_scroll = gtk::ScrolledWindow::new();
    hosts_scroll.set_min_content_height(150);
    hosts_scroll.set_child(Some(&hosts));
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_lines(4);
    status.add_css_class("dim-label");
    let checks = gtk::ListBox::new();
    checks.set_selection_mode(gtk::SelectionMode::None);
    checks.add_css_class("boxed-list");
    let checks_scroll = gtk::ScrolledWindow::new();
    checks_scroll.set_vexpand(true);
    checks_scroll.set_child(Some(&checks));
    let add = gtk::Button::with_label("Add Host…");
    let check_again = gtk::Button::with_label("Check Again");
    let retry = gtk::Button::with_label("Retry");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.append(&add);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    buttons.append(&spacer);
    buttons.append(&check_again);
    buttons.append(&retry);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.append(&hosts_scroll);
    body.append(&status);
    body.append(&checks_scroll);
    body.append(&buttons);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&body));
    window.set_content(Some(&toolbar));
    let rw = Rc::new(RemotesWindow {
        window: window.clone(),
        hosts,
        status,
        checks,
        add,
        check_again,
        retry,
        selected: RefCell::new(None),
        reloading: Cell::new(false),
    });
    *app.remotes.window.borrow_mut() = Some(rw.clone());
    rw.hosts.connect_row_selected(|_, row| {
        let Some(row) = row else { return };
        let name = row.widget_name().to_string();
        app::with_app(|a| {
            let w = a.remotes.window.borrow().clone();
            if let Some(w) = w {
                if w.reloading.get() {
                    return;
                }
                *w.selected.borrow_mut() = Some(name.clone());
                if !a.remotes.reports.borrow().contains_key(&name) {
                    doctor(a, &name);
                }
                w.reload(a);
            }
        });
    });
    rw.add.connect_clicked(|_| {
        app::with_app(|a| add_host(a, "", ""));
    });
    rw.check_again.connect_clicked(|_| {
        app::with_app(|a| {
            let sel = a.remotes.window.borrow().as_ref().and_then(|w| w.selected.borrow().clone());
            if let Some(h) = sel {
                doctor(a, &h);
            }
        });
    });
    rw.retry.connect_clicked(|_| {
        app::with_app(|a| {
            let sel = a.remotes.window.borrow().as_ref().and_then(|w| w.selected.borrow().clone());
            if let Some(h) = sel {
                core::remote_kick(Some(&h), true);
            }
        });
    });
    // Esc closes it, like the macOS panels.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let w = window.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape {
            w.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
    window.connect_close_request(|_| {
        app::with_app(|a| a.remotes.window.borrow_mut().take());
        glib::Propagation::Proceed
    });
    rw.reload(app);
    window.present();
}

impl RemotesWindow {
    fn reload(&self, app: &Rc<App>) {
        self.reloading.set(true);
        while let Some(r) = self.hosts.first_child() {
            self.hosts.remove(&r);
        }
        let remotes = app.ui().cfg.remote.clone();
        if self.selected.borrow().is_none() {
            *self.selected.borrow_mut() = remotes.first().map(|r| r.name.clone());
        }
        let selected = self.selected.borrow().clone();
        for r in &remotes {
            let status = app.remotes.status(&r.name);
            let busy = app.remotes.busy.borrow().get(&r.name).cloned();
            let state = busy.unwrap_or_else(|| app.remotes.phase_label(app, &r.name));
            let build = status
                .as_ref()
                .and_then(|s| s.get("remote_build").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            let row = adw::ActionRow::new();
            row.set_title(&r.name);
            row.set_subtitle(&glib::markup_escape_text(&format!("{}   {}", r.host, build)));
            let badge = gtk::Label::new(Some(&state));
            badge.add_css_class(match state.as_str() {
                "connected" => "success",
                "needs attention" | "upgrade needed" | "not installed" => "warning",
                _ => "dim-label",
            });
            row.add_suffix(&badge);
            row.set_widget_name(&r.name);
            self.hosts.append(&row);
            if selected.as_deref() == Some(r.name.as_str()) {
                self.hosts.select_row(Some(&row));
            }
        }
        let host = selected.clone().unwrap_or_default();
        let message = app
            .remotes
            .status(&host)
            .and_then(|s| s.get("message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        let busy = app.remotes.busy.borrow().get(&host).cloned().unwrap_or_default();
        self.status.set_text(if remotes.is_empty() {
            "No hosts yet: Add Host… connects to one over ssh."
        } else {
            ""
        });
        if !remotes.is_empty() {
            self.status.set_text(&[busy.as_str(), message.as_str()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join("\n"));
        }
        self.retry.set_sensitive(!host.is_empty() && !app.remotes.busy.borrow().contains_key(&host));
        self.check_again.set_sensitive(!host.is_empty() && app.remotes.fixing.borrow().is_none());
        while let Some(r) = self.checks.first_child() {
            self.checks.remove(&r);
        }
        if let Some(report) = app.remotes.reports.borrow().get(&host) {
            let checks: Vec<Value> = report
                .get("checks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| {
                    vec![json!({"id": "ssh", "title": "ssh", "state": "fail",
                        "detail": report.get("error").and_then(Value::as_str).unwrap_or("")})]
                });
            for c in checks {
                self.checks.append(&check_row(app, &host, &c));
            }
        }
        self.reloading.set(false);
    }
}

fn check_row(app: &Rc<App>, host: &str, c: &Value) -> adw::ActionRow {
    let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let state = s("state");
    let row = adw::ActionRow::new();
    row.set_title(&glib::markup_escape_text(&s("title")));
    row.set_subtitle(&glib::markup_escape_text(&s("detail")));
    row.set_subtitle_lines(4);
    let icon = gtk::Image::from_icon_name(match state.as_str() {
        "ok" => "object-select-symbolic",
        "warn" => "dialog-warning-symbolic",
        "fail" => "dialog-error-symbolic",
        _ => "list-remove-symbolic",
    });
    icon.add_css_class(match state.as_str() {
        "ok" => "success",
        "warn" => "warning",
        "fail" => "error",
        _ => "dim-label",
    });
    row.add_prefix(&icon);
    let id = s("id");
    let fixing = app.remotes.fixing.borrow().clone();
    if fixing.as_ref().is_some_and(|(h, i)| h == host && *i == id) {
        let b = gtk::Button::with_label("Fixing…");
        b.set_sensitive(false);
        b.set_valign(gtk::Align::Center);
        row.add_suffix(&b);
    } else if state == "fail" || state == "warn" {
        let fix = c.get("fix").filter(|f| !f.is_null());
        let terminal = c.get("terminal").and_then(Value::as_str).map(str::to_string);
        if let Some(f) = fix {
            let label = f.get("label").and_then(Value::as_str).unwrap_or("Fix").to_string();
            let b = gtk::Button::with_label(&label);
            b.set_valign(gtk::Align::Center);
            b.set_sensitive(fixing.is_none());
            let (h, i, title, confirm) = (
                host.to_string(),
                id.clone(),
                s("title"),
                f.get("confirm").and_then(Value::as_str).map(str::to_string),
            );
            b.connect_clicked(move |_| {
                app::with_app(|a| run_fix(a, &h, &i, &title, confirm.as_deref()));
            });
            row.add_suffix(&b);
        }
        if let Some(script) = terminal {
            let b = gtk::Button::with_label(if fix.is_some() { "In a Tab…" } else { "Run in a Tab…" });
            b.set_valign(gtk::Align::Center);
            let (h, title) = (host.to_string(), s("title"));
            b.connect_clicked(move |_| {
                app::with_app(|a| run_in_tab(a, &h, &script, &title));
            });
            row.add_suffix(&b);
        }
    }
    row
}

fn bins() -> Value {
    core::helpers_dir().map_or(Value::Null, |d| Value::String(d.display().to_string()))
}

fn set_busy(app: &Rc<App>, host: &str, text: Option<&str>) {
    match text {
        Some(t) => app.remotes.busy.borrow_mut().insert(host.to_string(), t.to_string()),
        None => app.remotes.busy.borrow_mut().remove(host),
    };
    if let Some(w) = app.remotes.window.borrow().clone() {
        w.reload(app);
    }
}

fn doctor(app: &Rc<App>, host: &str) {
    let first = !app.remotes.reports.borrow().contains_key(host);
    set_busy(app, host, Some(if first { "Checking…" } else { "Checking again…" }));
    let h = host.to_string();
    core::remote_call_async(json!({"op": "doctor", "name": host, "bins": bins()}), move |r| {
        app::with_app(|a| {
            a.remotes.reports.borrow_mut().insert(h.clone(), r);
            set_busy(a, &h, None);
        });
    });
}

fn run_fix(app: &Rc<App>, host: &str, id: &str, title: &str, confirm: Option<&str>) {
    if id == "thurm" || id == "daemon" {
        install(app, host);
        return;
    }
    let (h, i, t) = (host.to_string(), id.to_string(), title.to_string());
    let go = move || {
        app::with_app(|a| {
            *a.remotes.fixing.borrow_mut() = Some((h.clone(), i.clone()));
            if let Some(w) = a.remotes.window.borrow().clone() {
                w.reload(a);
            }
            let (h2, t2) = (h.clone(), t.clone());
            core::remote_call_async(
                json!({"op": "doctor_fix", "name": h, "id": i, "bins": bins()}),
                move |r| {
                    app::with_app(|a| {
                        a.remotes.fixing.borrow_mut().take();
                        if let Some(e) = r.get("error").and_then(Value::as_str)
                            && let Some(w) = a.remotes.window.borrow().as_ref()
                        {
                            dialogs::inform(&w.window, &format!("{t2} on {h2}"), e);
                        }
                        core::remote_kick(Some(&h2), true);
                        doctor(a, &h2);
                    });
                },
            );
        });
    };
    match (confirm, app.remotes.window.borrow().as_ref()) {
        (Some(c), Some(w)) => dialogs::confirm(&w.window, title, c, "Continue", false, go),
        _ => go(),
    }
}

/// Runs a doctor's script on the host in a new local tab (`ssh -t …`), kept open.
fn run_in_tab(app: &Rc<App>, host: &str, script: &str, title: &str) {
    let argv = core::remote_call(&json!({"op": "terminal_argv", "name": host, "script": script}));
    let Some(argv) = argv.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>()) else {
        return;
    };
    let Some(key) = app.create_pane(LOCAL, None, None, Some(argv), None, true) else { return };
    app.place_new_tab(key, None, true);
    app.toast(&format!("{title} on {host}: when it is done, Check Again in Thurm › Remotes…"), 6.0);
}

/// Plan, then install, then upgrade the remote daemon.
fn install(app: &Rc<App>, host: &str) {
    let h = host.to_string();
    set_busy(app, host, Some("Checking…"));
    core::remote_call_async(json!({"op": "plan", "name": host, "bins": bins()}), move |plan| {
        app::with_app(|a| {
            set_busy(a, &h, None);
            let Some(w) = a.remotes.window.borrow().clone() else { return };
            if let Some(e) = plan.get("error").and_then(Value::as_str) {
                dialogs::inform(&w.window, &format!("Could not reach {h}"), e);
                return;
            }
            if plan.get("installed").and_then(Value::as_bool) == Some(true) {
                upgrade_daemon(a, &h, &plan);
                return;
            }
            let methods: Vec<String> = plan
                .get("methods")
                .and_then(Value::as_array)
                .map(|m| m.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let labels: Vec<String> = plan
                .get("labels")
                .and_then(Value::as_array)
                .map(|m| m.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let os = plan.pointer("/host/os").and_then(Value::as_str).unwrap_or("");
            let arch = plan.pointer("/host/arch").and_then(Value::as_str).unwrap_or("");
            if methods.is_empty() {
                let problem = plan
                    .get("problem")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("No build fits {os} {arch}."));
                dialogs::inform(&w.window, &format!("Thurm cannot be installed on {h}"), &problem);
                return;
            }
            let mut buttons = Vec::new();
            for (m, l) in methods.iter().zip(labels.iter()).take(3) {
                buttons.push(button(m, l, Style::Default));
            }
            buttons.push(button("cancel", "Cancel", Style::Default));
            let h2 = h.clone();
            dialogs::ask(
                &w.window,
                &format!("Install Thurm {} on {h}?", core::build_id()),
                &format!("{os} {arch}. Thurm goes to ~/.local/share/thurm/bin there (and ~/.local/bin/thurm when that directory exists)."),
                &buttons,
                move |method| {
                    if method == "cancel" {
                        return;
                    }
                    app::with_app(|a| {
                        set_busy(a, &h2, Some("Installing…"));
                        let h3 = h2.clone();
                        core::remote_call_async(
                            json!({"op": "install", "name": h2, "method": method, "bins": bins()}),
                            move |r| {
                                app::with_app(|a| {
                                    set_busy(a, &h3, None);
                                    if let Some(e) = r.get("error").and_then(Value::as_str) {
                                        if let Some(w) = a.remotes.window.borrow().as_ref() {
                                            dialogs::inform(&w.window, &format!("Could not install Thurm on {h3}"), e);
                                        }
                                        return;
                                    }
                                    let plan = core::remote_call(&json!({"op": "plan", "name": h3, "bins": bins()}));
                                    upgrade_daemon(a, &h3, &plan);
                                });
                            },
                        );
                    });
                },
            );
        });
    });
}

fn upgrade_daemon(app: &Rc<App>, host: &str, plan: &Value) {
    let daemon = plan.get("daemon").filter(|d| !d.is_null());
    let running = daemon.and_then(|d| d.get("running")).and_then(Value::as_bool) == Some(true);
    let same = daemon.and_then(|d| d.get("build")).and_then(Value::as_str) == Some(core::build_id().as_str());
    if !running || same {
        core::remote_kick(Some(host), true);
        doctor(app, host);
        return;
    }
    let hot = daemon.and_then(|d| d.get("hot_upgrade")).and_then(Value::as_bool) == Some(true);
    let h = host.to_string();
    let go = move || {
        app::with_app(|a| {
            set_busy(a, &h, Some("Upgrading…"));
            let h2 = h.clone();
            core::remote_call_async(
                json!({"op": "upgrade_daemon", "name": h, "allow_restart": !hot}),
                move |r| {
                    app::with_app(|a| {
                        set_busy(a, &h2, None);
                        if let Some(e) = r.get("error").and_then(Value::as_str)
                            && let Some(w) = a.remotes.window.borrow().as_ref()
                        {
                            dialogs::inform(&w.window, &format!("Could not upgrade the daemon on {h2}"), e);
                        }
                        core::remote_kick(Some(&h2), true);
                        doctor(a, &h2);
                    });
                },
            );
        });
    };
    match (hot, app.remotes.window.borrow().as_ref()) {
        (false, Some(w)) => dialogs::confirm(
            &w.window,
            &format!("Restart the daemon on {host}?"),
            "It is too old to be replaced in place: restarting it stops the programs running in its panes (layout and scrollback come back).",
            "Restart",
            true,
            go,
        ),
        _ => go(),
    }
}

fn add_host(app: &Rc<App>, name: &str, target: &str) {
    let Some(parent) = app.remotes.window.borrow().as_ref().map(|w| w.window.clone()) else {
        return;
    };
    dialogs::prompt(
        &parent,
        "Add a Host",
        "Thurm connects with the system ssh: the host needs a key that works without a prompt (an agent is fine). Connect once with ssh in a terminal to accept its host key.",
        &[
            ("Name", "devbox", name),
            ("SSH target", "me@devbox, a Host alias, or ssh://me@devbox:2222", target),
        ],
        "Add",
        |values| {
            app::with_app(|a| {
                let (name, target) = (values[0].trim().to_string(), values[1].trim().to_string());
                if name.is_empty() || target.is_empty() {
                    add_host(a, &name, &target);
                    return;
                }
                if let Some(w) = a.remotes.window.borrow().as_ref() {
                    w.add.set_sensitive(false);
                    w.status.set_text(&format!("Connecting to {target}…"));
                }
                let (n, t) = (name.clone(), target.clone());
                core::remote_call_async(json!({"op": "add", "name": name, "target": target}), move |r| {
                    app::with_app(|a| {
                        let w = a.remotes.window.borrow().clone();
                        if let Some(w) = &w {
                            w.add.set_sensitive(true);
                        }
                        if let Some(e) = r.get("error").and_then(Value::as_str) {
                            if let Some(w) = w {
                                let (n2, t2) = (n.clone(), t.clone());
                                dialogs::ask(
                                    &w.window,
                                    &format!("Could not add {n}"),
                                    e,
                                    &[button("edit", "Edit", Style::Suggested), button("cancel", "Cancel", Style::Default)],
                                    move |r| {
                                        if r == "edit" {
                                            app::with_app(|a| add_host(a, &n2, &t2));
                                        }
                                    },
                                );
                            }
                            return;
                        }
                        a.reload_config(false);
                        if let Some(w) = &w {
                            *w.selected.borrow_mut() = Some(n.clone());
                        }
                        doctor(a, &n);
                    });
                });
            });
        },
    );
}

