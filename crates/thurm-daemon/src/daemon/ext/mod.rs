//! The extension API ([`thurm_proto::ext`]): actions, key bindings, events and the services
//! plugins use (panes, the UI, running programs). Every caller is a [`Client`]: an app or the
//! CLI on the socket, or a plugin the daemon runs, through a client of its own.

pub mod registry;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thurm_proto::ext::{self, method, notice};
use thurm_proto::{CreatePane, Event, PaneId, PaneInfo, Request, Response, ServerMessage};

use super::{Client, Daemon};
use crate::user_exec;
use registry::{Provider, Registry};

/// How deep actions may run one another.
const MAX_DEPTH: u32 = 16;

/// How long a provider has to carry out an action, and the user to answer a picker.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A question to a client: who was asked, how deep an action it is (0 for one to the user),
/// and where the answer goes.
struct Pending {
    to: u64,
    depth: u32,
    tx: Sender<ext::Reply>,
}

/// The extension API's state in the daemon.
#[derive(Default)]
pub struct Ext {
    registry: Mutex<Registry>,
    /// Questions sent to clients (an action to carry out, a picker to show), by id: the asked
    /// client and where its answer goes.
    pending: Mutex<HashMap<u64, Pending>>,
    next: AtomicU64,
    /// Views clients mounted in the window, in the order they were first mounted, with the
    /// client each belongs to.
    views: Mutex<Vec<ext::MountedView>>,
}

pub use thurm_proto::ext::method_blocks as blocks;

/// What a tailnet caller may ask of the extension API: only to read what there is. Running
/// actions and programs, providing actions, hearing events and managing plugins are for this
/// machine's own clients and its plugins.
fn over_tailnet(method: &str) -> bool {
    matches!(method, method::ACTIONS_LIST | method::PLUGINS_LIST)
}

/// A caller the Tailscale helper admitted, rather than a process of this machine.
fn client_is_tailnet(client: &Client) -> bool {
    client
        .principal
        .lock()
        .as_ref()
        .is_some_and(|p| p.kind == thurm_proto::PrincipalKind::Tailscale)
}

fn params<T: DeserializeOwned + Default>(v: Value) -> Result<T, String> {
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v).map_err(|e| format!("bad parameters: {e}"))
}

fn names(v: &Value) -> Vec<String> {
    v.get("names")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn str_param<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key:?}"))
}

impl Daemon {
    /// What `client` is called as the source of its actions and in logs: `plugin:<name>` for a
    /// plugin, else the name it gave (`app`, `cli`).
    pub(super) fn ext_source(&self, client: &Client) -> String {
        if let Some(p) = client.plugin.lock().as_ref() {
            return format!("plugin:{p}");
        }
        let name = client.name.lock().clone();
        if name.is_empty() {
            format!("client:{}", client.id)
        } else {
            name
        }
    }

    /// Answers one method of the extension API for `client`.
    pub fn ext_call(
        self: &Arc<Self>,
        client: &Arc<Client>,
        method: &str,
        params_json: Value,
    ) -> Result<Value, String> {
        let p = params_json;
        if client.gone.load(Ordering::SeqCst) {
            return Err("this client was disconnected".into());
        }
        if client_is_tailnet(client) && !over_tailnet(method) {
            return Err(format!(
                "{method} is for this machine's own clients, not over the tailnet"
            ));
        }
        match method {
            method::REQUEST => {
                let req: Request =
                    serde_json::from_value(p.get("request").cloned().ok_or("missing \"request\"")?)
                        .map_err(|e| format!("bad request: {e}"))?;
                if matches!(req, Request::Hello { .. } | Request::Ext { .. }) {
                    return Err("not through `request`".into());
                }
                let resp = self.answer(client, req)?;
                serde_json::to_value(resp).map_err(|e| e.to_string())
            }
            method::ACTIONS_LIST => {
                let reg = self.ext.registry.lock();
                Ok(json!(ext::Actions {
                    actions: reg.actions(),
                    bindings: reg.bindings(),
                }))
            }
            method::ACTIONS_REGISTER => {
                let actions: Vec<ext::ActionInfo> = serde_json::from_value(
                    p.get("actions").cloned().unwrap_or(Value::Array(vec![])),
                )
                .map_err(|e| format!("bad actions: {e}"))?;
                let source = self.ext_source(client);
                self.ext_registry(client)?
                    .register(client.id, &source, actions)?;
                self.actions_changed();
                Ok(Value::Null)
            }
            method::ACTIONS_UNREGISTER => {
                if self.ext.registry.lock().unregister(client.id, &names(&p)) {
                    self.actions_changed();
                }
                Ok(Value::Null)
            }
            method::ACTIONS_RUN => {
                let run: ext::RunAction = params(p)?;
                let origin = self.ext_source(client);
                self.run_action(client, &origin, run)
            }
            method::ACTIONS_RESULT | method::UI_REPLY => {
                let reply: ext::Reply = params(p)?;
                self.ext_reply(client.id, reply);
                Ok(Value::Null)
            }
            method::KEYS_BIND => {
                let b: ext::Binding = params(p)?;
                let source = self.ext_source(client);
                self.ext_registry(client)?.bind(client.id, &source, b)?;
                self.actions_changed();
                Ok(Value::Null)
            }
            method::KEYS_UNBIND => {
                let key = str_param(&p, "key")?;
                if self.ext.registry.lock().unbind(client.id, key) {
                    self.actions_changed();
                }
                Ok(Value::Null)
            }
            method::EVENTS_SUBSCRIBE => {
                self.ext_registry(client)?.subscribe(client.id, names(&p))?;
                Ok(Value::Null)
            }
            method::EVENTS_UNSUBSCRIBE => {
                self.ext.registry.lock().unsubscribe(client.id, &names(&p));
                Ok(Value::Null)
            }
            method::EVENTS_EMIT => {
                let name = str_param(&p, "name")?;
                if !name.contains('.') || !ext::valid_action_name(name) {
                    return Err(format!(
                        "{name:?}: a plugin's event names have a dot (`git.changed`)"
                    ));
                }
                let mut payload = match p.get("payload") {
                    Some(Value::Object(o)) => Value::Object(o.clone()),
                    None | Some(Value::Null) => json!({}),
                    Some(other) => json!({ "value": other }),
                };
                payload["event"] = name.into();
                payload["source"] = self.ext_source(client).into();
                self.deliver_event(name, &payload);
                Ok(Value::Null)
            }
            method::PANES_OPEN => {
                let open: ext::OpenPane = params(p)?;
                self.open_pane(open)
            }
            method::UI_TOAST => {
                let text = str_param(&p, "text")?.to_owned();
                let pane = p.get("pane").and_then(Value::as_u64);
                self.toast(text, pane);
                Ok(Value::Null)
            }
            method::UI_MOUNT => {
                let mount: ext::Mount = params(p)?;
                mount.check()?;
                self.mount_view(client, mount)?;
                Ok(json!({ "shown": self.ui_client().is_some() }))
            }
            method::UI_UNMOUNT => {
                let id = str_param(&p, "id")?;
                self.unmount_views(|v| v.client == client.id && v.id == id);
                Ok(Value::Null)
            }
            method::UI_PICK => {
                let pick: ext::Pick = params(p)?;
                if pick.items.is_empty() {
                    return Ok(Value::Null);
                }
                self.ask_ui(client, |id| ext::UiRequest::Pick { id, pick })
            }
            method::UI_INPUT => {
                let input: ext::Input = params(p)?;
                self.ask_ui(client, |id| ext::UiRequest::Input { id, input })
            }
            method::UI_CONFIRM => {
                let confirm: ext::Confirm = params(p)?;
                let answer = self.ask_ui(client, |id| ext::UiRequest::Confirm { id, confirm })?;
                Ok(Value::Bool(answer.as_bool().unwrap_or(false)))
            }
            method::EXEC => {
                let exec: ext::Exec = params(p)?;
                if exec.argv.is_empty() || exec.argv[0].is_empty() {
                    return Err("nothing to run".into());
                }
                let result = user_exec::run(
                    user_exec::Launch {
                        argv: &exec.argv,
                        cwd: exec.cwd.as_deref(),
                        env: self
                            .user_exec_env(None, None)
                            .into_iter()
                            .chain(exec.env)
                            .collect(),
                        stdin: exec.stdin.map(String::into_bytes),
                    },
                    exec.timeout_ms.map(Duration::from_millis),
                    // Stopped with its caller (a script that ended, a client that left).
                    &|| client.gone.load(Ordering::SeqCst),
                )
                .map_err(|e| format!("cannot run {:?}: {e}", exec.argv[0]))?;
                serde_json::to_value(result).map_err(|e| e.to_string())
            }
            method::LOG => {
                let message = str_param(&p, "message")?;
                let source = self.ext_source(client);
                let level = p.get("level").and_then(Value::as_str).unwrap_or("info");
                self.plugin_log(client, level, message);
                match level {
                    "error" => log::error!("{source}: {message}"),
                    "warn" => log::warn!("{source}: {message}"),
                    "debug" => log::debug!("{source}: {message}"),
                    _ => log::info!("{source}: {message}"),
                }
                Ok(Value::Null)
            }
            other => self.plugin_method(client, other, p),
        }
    }

    /// The registry, to add what `client` provides or hears: refused once the client is gone.
    /// Checked under the registry's lock, which its removal takes after setting `gone`, so a
    /// registration either sees the flag or is cleaned up with the rest.
    fn ext_registry(
        &self,
        client: &Client,
    ) -> Result<parking_lot::MutexGuard<'_, Registry>, String> {
        let reg = self.ext.registry.lock();
        if client.gone.load(Ordering::SeqCst) {
            return Err("this client was disconnected".into());
        }
        Ok(reg)
    }

    /// Every action, for the plugin list.
    pub(super) fn ext_actions(&self) -> Vec<ext::ActionInfo> {
        self.ext.registry.lock().actions()
    }

    /// A [`Request`] answered to its end, including those `handle` answers on a thread of
    /// their own.
    fn answer(self: &Arc<Self>, client: &Arc<Client>, req: Request) -> Result<Response, String> {
        match req {
            Request::Wait {
                pane,
                until,
                timeout_ms,
            } => self
                .wait(client.id, pane, until, timeout_ms)
                .map(Response::Wait),
            Request::Explain { pane } => self.explain(pane).map(Response::Text),
            Request::AgentPrompt {
                pane,
                text,
                wait,
                timeout_ms,
            } => {
                let ticket = self.submit_agent_prompt(pane, &text, timeout_ms)?;
                if !wait {
                    return Ok(Response::AgentPrompt(
                        thurm_proto::AgentPromptOutcome::Submitted,
                    ));
                }
                self.wait_agent_prompt(client.id, pane, ticket)
                    .map(Response::AgentPrompt)
            }
            other => self.dispatch(client, other),
        }
    }

    /// Runs the action `run.name` for `caller` and returns what it returned.
    pub fn run_action(
        self: &Arc<Self>,
        caller: &Client,
        origin: &str,
        run: ext::RunAction,
    ) -> Result<Value, String> {
        // How deep this runs: what the caller says, or more if the caller is itself carrying
        // out actions it was asked to (a provider need not pass its depth on for the limit to
        // hold).
        let depth = run.depth.max(self.ext_depth_of(caller, run.invocation));
        if depth >= MAX_DEPTH {
            return Err(format!(
                "actions ran one another {MAX_DEPTH} deep: stopped at {:?}",
                run.name
            ));
        }
        let entry = self
            .ext
            .registry
            .lock()
            .get(&run.name)
            .cloned()
            .ok_or_else(|| format!("no action named {:?}", run.name))?;
        let pane = if run.no_pane {
            None
        } else {
            run.pane.or_else(|| self.focused_pane())
        };
        match entry.provider {
            Provider::Config => self.run_command(&run.name, pane).map(|_| Value::Null),
            Provider::Client(id) => {
                let client = self
                    .clients
                    .lock()
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| format!("{} is gone", entry.info.source))?;
                let (qid, rx) = self.ext_question(id, depth + 1);
                // A provider that left before the question was there to be dropped with it.
                if client.gone.load(Ordering::SeqCst) {
                    self.ext.pending.lock().remove(&qid);
                    return Err(format!(
                        "{} went away before it answered",
                        entry.info.source
                    ));
                }
                let invoke = ext::Invoke {
                    id: qid,
                    name: run.name.clone(),
                    args: run.args,
                    ctx: ext::Context {
                        pane,
                        origin: origin.to_owned(),
                        depth: depth + 1,
                        // Another host's pane is the reason for none.
                        remote: run.remote.filter(|_| pane.is_none()),
                        selection: run.selection,
                        file: run.file,
                    },
                };
                send_ext(&client, notice::ACTION_INVOKE, &json!(invoke));
                self.ext_wait(qid, &rx, &entry.info.source, caller)
            }
        }
    }

    /// A new question to client `to`: its id, and where the answer comes.
    fn ext_question(&self, to: u64, depth: u32) -> (u64, Receiver<ext::Reply>) {
        let id = self.ext.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = bounded(1);
        self.ext
            .pending
            .lock()
            .insert(id, Pending { to, depth, tx });
        (id, rx)
    }

    /// Waits for the answer to question `id`; gives up when `caller`, who waits for it, is gone.
    fn ext_wait(
        &self,
        id: u64,
        rx: &Receiver<ext::Reply>,
        who: &str,
        caller: &Client,
    ) -> Result<Value, String> {
        let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
        let reply = loop {
            let step = deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(Duration::from_millis(200));
            match rx.recv_timeout(step) {
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
                    if std::time::Instant::now() < deadline =>
                {
                    if caller.gone.load(Ordering::SeqCst) {
                        self.ext.pending.lock().remove(&id);
                        return Err("the caller went away".into());
                    }
                }
                other => break other,
            }
        };
        self.ext.pending.lock().remove(&id);
        match reply {
            Ok(ext::Reply { error: Some(e), .. }) => Err(e),
            Ok(r) => Ok(r.result),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                Err(format!("{who} did not answer"))
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                Err(format!("{who} went away before it answered"))
            }
        }
    }

    /// How deep an action `client` is carrying out: invocation `from`'s, when it names one it
    /// was asked to carry out, else the deepest one; 0 when none.
    fn ext_depth_of(&self, client: &Client, from: Option<u64>) -> u32 {
        let pending = self.ext.pending.lock();
        if let Some(p) = from
            .and_then(|id| pending.get(&id))
            .filter(|p| p.to == client.id)
        {
            return p.depth;
        }
        pending
            .values()
            .filter(|p| p.to == client.id)
            .map(|p| p.depth)
            .max()
            .unwrap_or(0)
    }

    /// An answer from client `from` to one of its questions; answers to someone else's are
    /// dropped.
    pub(super) fn ext_reply(&self, from: u64, reply: ext::Reply) {
        let mut pending = self.ext.pending.lock();
        if pending.get(&reply.id).is_some_and(|p| p.to == from)
            && let Some(Pending { tx, .. }) = pending.remove(&reply.id)
        {
            let _ = tx.send(reply);
        }
    }

    /// Forgets what `client` provided, heard, was asked and showed.
    pub(super) fn ext_client_gone(&self, client: u64) {
        // Dropping the senders ends the waits on them.
        self.ext.pending.lock().retain(|_, p| p.to != client);
        if self.ext.registry.lock().client_gone(client) {
            self.actions_changed();
        }
        self.unmount_views(|v| v.client == client);
    }

    /// Shows `mount` for `client`, replacing its view of the same id. Refused once the client
    /// is gone (checked under the views' lock, which its removal takes after setting `gone`).
    fn mount_view(&self, client: &Client, mount: ext::Mount) -> Result<(), String> {
        self.change_views(|views| {
            if client.gone.load(Ordering::SeqCst) {
                return Err("this client was disconnected".into());
            }
            let view = ext::MountedView {
                client: client.id,
                id: mount.id,
                slot: mount.slot,
                view: mount.view,
            };
            let mine = views.iter().filter(|v| v.client == client.id).count();
            match views
                .iter_mut()
                .find(|v| v.client == client.id && v.id == view.id)
            {
                Some(old) => *old = view,
                None if mine >= ext::MAX_VIEWS => {
                    return Err(format!("at most {} views at a time", ext::MAX_VIEWS));
                }
                None => views.push(view),
            }
            Ok(true)
        })
    }

    /// Takes down the views `gone` picks.
    fn unmount_views(&self, gone: impl Fn(&ext::MountedView) -> bool) {
        let _ = self.change_views(|views| {
            let before = views.len();
            views.retain(|v| !gone(v));
            Ok(views.len() != before)
        });
    }

    /// Changes the views with `f` and, when it says they changed, sends them to the apps that
    /// draw them, under the same lock: notices go out in the order the changes were made.
    fn change_views(
        &self,
        f: impl FnOnce(&mut Vec<ext::MountedView>) -> Result<bool, String>,
    ) -> Result<(), String> {
        let mut views = self.ext.views.lock();
        if f(&mut views)? {
            let notice = json!({ "views": *views });
            self.notify_clients(
                |c| c.ext_ui.load(Ordering::Relaxed),
                notice::VIEWS_CHANGED,
                &notice,
            );
        }
        Ok(())
    }

    /// Sends `client` the views mounted now, as it starts drawing them.
    pub(super) fn send_views(&self, client: &Client) {
        let views = self.ext.views.lock();
        send_ext(client, notice::VIEWS_CHANGED, &json!({ "views": *views }));
    }

    /// The config's `[[commands]]` as actions; called whenever the config is applied.
    pub(super) fn ext_config_applied(&self) {
        let commands = self.config.read().commands.clone();
        self.ext.registry.lock().set_config(&commands);
        self.actions_changed();
    }

    /// Tells the clients that use the extension API to read the actions again.
    fn actions_changed(&self) {
        self.notify_clients(
            |c| c.ext.load(Ordering::Relaxed),
            notice::ACTIONS_CHANGED,
            &json!({}),
        );
    }

    fn notify_clients(&self, to: impl Fn(&Client) -> bool, name: &str, payload: &Value) {
        let clients: Vec<_> = self.clients.lock().values().cloned().collect();
        for c in clients.iter().filter(|c| to(c)) {
            send_ext(c, name, payload);
        }
    }

    /// An event of the daemon's own: the `[[on]]` hooks for it run, and the clients that
    /// subscribed hear it. `extra` is merged into the payload, beside `event` and `pane`.
    pub(super) fn emit(&self, event: &str, pane: Option<&PaneInfo>, extra: Value) {
        self.emit_with_private(event, pane, extra, Value::Null);
    }

    /// [`Daemon::emit`], with `private` added for the `[[on]]` hooks only: the user's own
    /// commands may see what other clients must not (a command line may hold a secret).
    pub(super) fn emit_with_private(
        &self,
        event: &str,
        pane: Option<&PaneInfo>,
        extra: Value,
        private: Value,
    ) {
        let hooks = thurm_config::HookEvent::parse(event).is_some_and(|e| {
            let secs = extra
                .get("duration_secs")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.config
                .read()
                .hooks(e)
                .any(|h| e != thurm_config::HookEvent::CommandFinished || secs >= h.min_secs)
        });
        let listeners = self.ext.registry.lock().listeners(event);
        if !hooks && listeners.is_empty() {
            return;
        }
        let mut payload = json!({ "event": event });
        if let Some(info) = pane {
            payload["pane"] = serde_json::to_value(info).unwrap_or_default();
        }
        if let (Some(p), Value::Object(extra)) = (payload.as_object_mut(), extra) {
            p.extend(extra);
        }
        if hooks && let (Some(e), Some(info)) = (thurm_config::HookEvent::parse(event), pane) {
            let mut hook_payload = payload.clone();
            if let (Some(p), Value::Object(private)) = (hook_payload.as_object_mut(), private) {
                p.extend(private);
            }
            self.run_hooks(e, info, &hook_payload);
        }
        self.send_event(&listeners, &payload);
    }

    /// Sends `payload` to the clients that subscribed to `event`.
    fn deliver_event(&self, event: &str, payload: &Value) {
        let listeners = self.ext.registry.lock().listeners(event);
        self.send_event(&listeners, payload);
    }

    fn send_event(&self, listeners: &[u64], payload: &Value) {
        if listeners.is_empty() {
            return;
        }
        // Not under the clients lock: sending can take a while.
        let targets: Vec<Arc<Client>> = {
            let clients = self.clients.lock();
            listeners
                .iter()
                .filter_map(|id| clients.get(id).cloned())
                .collect()
        };
        for c in targets {
            send_ext(&c, notice::EVENT, payload);
        }
    }

    /// Events that are a client message too, heard by the extension API's subscribers.
    pub(super) fn emit_for(&self, event: &Event) {
        match event {
            Event::PaneInfo(info) => self.emit("pane_changed", Some(info), json!({})),
            Event::PaneClosed { pane } => self.emit("pane_closed", None, json!({ "id": pane })),
            Event::Bell { pane } => self.emit("bell", None, json!({ "id": pane })),
            Event::Notify {
                pane, title, body, ..
            } => self.emit(
                "notification",
                None,
                json!({ "id": pane, "title": title, "body": body }),
            ),
            Event::ConfigReloaded => self.emit("config_reloaded", None, json!({})),
            _ => {}
        }
    }

    /// The pane focused in a UI now, else the one focused last.
    pub(super) fn focused_pane(&self) -> Option<PaneId> {
        let history = self
            .focus
            .lock()
            .history(|p| self.pane(p).is_some(), std::time::Instant::now());
        history
            .iter()
            .find(|r| r.focused)
            .or(history.first())
            .map(|r| r.pane)
    }

    /// `panes.open`: a new pane, shown as asked.
    fn open_pane(self: &Arc<Self>, open: ext::OpenPane) -> Result<Value, String> {
        use thurm_proto::{SplitDir, UiCommand};
        let split = match open.split.as_deref() {
            None => None,
            Some("right") => Some(SplitDir::Right),
            Some("down") => Some(SplitDir::Down),
            Some("left") => Some(SplitDir::Left),
            Some("up") => Some(SplitDir::Up),
            Some(other) => {
                return Err(format!("split {other:?}: use right, down, left or up"));
            }
        };
        let target = open.target.or_else(|| self.focused_pane());
        let place = float_place(&open, target, |p| self.pane(p).is_some())?;
        let pane = self
            .create_pane(
                CreatePane {
                    command: (!open.command.is_empty()).then_some(open.command),
                    cwd: open.cwd,
                    env: open.env.into_iter().collect(),
                    agent_preset: open.agent,
                    inherit_cwd_from: target,
                    hold: open.hold,
                    ..Default::default()
                },
                None,
            )
            .map_err(|e| e.to_string())?;
        if open.detached {
            return Ok(json!({ "pane": pane, "shown": false }));
        }
        let floating = UiCommand::OpenFloating {
            pane,
            target,
            background: open.background,
        };
        if let Some(place) = place {
            let at = UiCommand::OpenFloatingAt {
                pane,
                target,
                place,
                background: open.background,
            };
            // An app from before `OpenFloatingAt` floats it where new floating panes go.
            let shown = match self.layout_ui(at) {
                Err(e) if e == thurm_proto::UI_TOO_OLD => self.layout_ui(floating),
                other => other,
            }
            .is_ok();
            return Ok(json!({ "pane": pane, "shown": shown }));
        }
        let cmd = if open.float {
            floating
        } else {
            UiCommand::Open {
                pane,
                split,
                target,
                new_window: open.window,
                percent: open.percent,
                background: open.background,
            }
        };
        let shown = match self.layout_ui(cmd) {
            Ok(_) => true,
            // An app from before `Open` shows a split or a tab with the plain commands; a
            // floating pane it cannot show, and stays unshown rather than misplaced.
            Err(e) if e == thurm_proto::UI_TOO_OLD && !open.float && self.has_ui_clients() => {
                let plain = match split {
                    Some(dir) => UiCommand::Split { target, pane, dir },
                    None => UiCommand::NewTab {
                        pane,
                        new_window: open.window,
                    },
                };
                self.broadcast(Event::Ui(plain), true);
                true
            }
            Err(_) => false,
        };
        Ok(json!({ "pane": pane, "shown": shown }))
    }

    /// The desktop app that shows plugins' UI: the one with a focused pane, else any.
    fn ui_client(&self) -> Option<Arc<Client>> {
        let clients: Vec<Arc<Client>> = self
            .clients
            .lock()
            .values()
            .filter(|c| c.ext_ui.load(Ordering::Relaxed))
            .cloned()
            .collect();
        let focused = self.focus.lock().focused_clients();
        clients
            .iter()
            .find(|c| focused.contains(&c.id))
            .or(clients.first())
            .cloned()
    }

    /// A short message over `pane` (the focused one), or in the log without an app to show it.
    pub(super) fn toast(&self, text: String, pane: Option<PaneId>) {
        match self.ui_client() {
            Some(c) => {
                let id = self.ext.next.fetch_add(1, Ordering::Relaxed) + 1;
                let req = ext::UiRequest::Toast { id, text, pane };
                send_ext(&c, notice::UI_REQUEST, &json!(req));
            }
            None => log::info!("toast: {text}"),
        }
    }

    /// Asks the app to show `make(id)` and waits for the user's answer.
    fn ask_ui(
        &self,
        caller: &Client,
        make: impl FnOnce(u64) -> ext::UiRequest,
    ) -> Result<Value, String> {
        let app = self
            .ui_client()
            .ok_or("no Thurm window is open to ask in (or the app is too old)")?;
        let (id, rx) = self.ext_question(app.id, 0);
        if app.gone.load(Ordering::SeqCst) {
            self.ext.pending.lock().remove(&id);
            return Err("the app went away".into());
        }
        send_ext(&app, notice::UI_REQUEST, &json!(make(id)));
        let answer = self.ext_wait(id, &rx, "the app", caller);
        if answer.is_err() {
            send_ext(&app, notice::UI_CANCEL, &json!({ "id": id }));
        }
        answer
    }
}

/// Whether the peer on `stream` is the app this daemon belongs to. A daemon that runs from an
/// app bundle (`Thurm.app/Contents/Helpers/thurmd`) takes only that bundle's own executable,
/// by the peer's pid: another program of the same user cannot pose as the app to read what
/// plugins ask. A daemon outside a bundle (a development build, Linux) cannot tell the app
/// apart, and takes none (see [`unverified_ui_allowed`]).
pub fn peer_is_our_app(stream: &std::os::unix::net::UnixStream) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let Some(app) = std::env::current_exe()
            .ok()
            .and_then(|e| e.canonicalize().ok())
            .and_then(|exe| {
                let helpers = exe.parent()?;
                let contents = helpers.parent()?;
                let bundle = contents.parent()?;
                (helpers.file_name()? == "Helpers"
                    && contents.file_name()? == "Contents"
                    && bundle.extension()? == "app")
                    .then(|| contents.join("MacOS").join("Thurm"))
            })
        else {
            return unverified_ui_allowed();
        };
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let got = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        if got != 0 {
            return false;
        }
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n <= 0 {
            return false;
        }
        let peer = std::path::PathBuf::from(String::from_utf8_lossy(&buf[..n as usize]).as_ref());
        peer.canonicalize().ok() == app.canonicalize().ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = stream;
        unverified_ui_allowed()
    }
}

/// Whether a desktop UI whose app cannot be identified (a daemon outside an app bundle: a
/// development build, the tests) may still be asked what plugins ask: only when whoever
/// started the daemon said so, with `THURM_TRUST_UI_CLIENTS=1`. Otherwise no client is.
fn unverified_ui_allowed() -> bool {
    std::env::var_os("THURM_TRUST_UI_CLIENTS").is_some_and(|v| v == "1")
}

/// Sends a notice of the extension API to `client`.
pub(super) fn send_ext(client: &Client, name: &str, payload: &Value) {
    client.send(ServerMessage::Event(Event::Ext {
        name: name.to_owned(),
        payload: payload.to_string(),
    }));
}

/// Where `panes.open` floats its pane: at `frame`, or in `cells` counted from `target`. A place
/// over a `target` that is not a live pane is refused, before any pane is made.
fn float_place(
    open: &ext::OpenPane,
    target: Option<PaneId>,
    live: impl Fn(PaneId) -> bool,
) -> Result<Option<thurm_proto::FloatPlace>, String> {
    use thurm_proto::FloatPlace;
    if (open.frame.is_some() || open.cells.is_some())
        && let Some(t) = target.filter(|&t| !live(t))
    {
        return Err(format!("target {t}: no such pane"));
    }
    match (open.frame, open.cells) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err("give frame or cells, not both".into()),
        (Some([x, y, w, h]), None) => {
            if [x, y, w, h].iter().any(|f| !(0.0..=1.0).contains(f)) || w == 0.0 || h == 0.0 {
                return Err(
                    "frame: fractions go from 0 to 1, and the width and height can't be 0".into(),
                );
            }
            Ok(Some(FloatPlace::Fraction([x, y, w, h])))
        }
        (None, Some(c)) => {
            if c.cols < 2 || c.rows == 0 {
                return Err("cells: at least 2 columns by 1 row".into());
            }
            let anchor = target.ok_or("cells count from a pane: give target")?;
            Ok(Some(FloatPlace::Cells {
                anchor,
                col: c.col,
                row: c.row,
                cols: c.cols,
                rows: c.rows,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(frame: Option<[f64; 4]>, cells: Option<ext::FloatCells>) -> ext::OpenPane {
        ext::OpenPane {
            frame,
            cells,
            ..Default::default()
        }
    }

    fn float_place_in(
        open: &ext::OpenPane,
        target: Option<PaneId>,
    ) -> Result<Option<thurm_proto::FloatPlace>, String> {
        float_place(open, target, |_| true)
    }

    #[test]
    fn float_place_takes_a_frame_or_cells() {
        use thurm_proto::FloatPlace;
        assert_eq!(float_place_in(&open(None, None), Some(3)), Ok(None));
        assert_eq!(
            float_place_in(&open(Some([0.1, 0.2, 0.5, 0.4]), None), None),
            Ok(Some(FloatPlace::Fraction([0.1, 0.2, 0.5, 0.4])))
        );
        let cells = ext::FloatCells {
            col: -2,
            row: 1,
            cols: 40,
            rows: 10,
        };
        assert_eq!(
            float_place_in(&open(None, Some(cells)), Some(3)),
            Ok(Some(FloatPlace::Cells {
                anchor: 3,
                col: -2,
                row: 1,
                cols: 40,
                rows: 10
            }))
        );
    }

    #[test]
    fn float_place_refuses_what_the_app_cannot_place() {
        let cells = ext::FloatCells {
            cols: 40,
            rows: 10,
            ..Default::default()
        };
        assert!(float_place_in(&open(Some([0.0, 0.0, 1.0, 1.0]), Some(cells)), Some(1)).is_err());
        assert!(float_place_in(&open(Some([0.0, 0.0, 1.5, 1.0]), None), None).is_err());
        assert!(float_place_in(&open(Some([0.0, 0.0, 0.0, 1.0]), None), None).is_err());
        assert!(float_place_in(&open(Some([f64::NAN, 0.0, 0.5, 0.5]), None), None).is_err());
        assert!(float_place_in(&open(None, Some(cells)), None).is_err());
        let narrow = ext::FloatCells {
            cols: 1,
            rows: 10,
            ..Default::default()
        };
        assert!(float_place_in(&open(None, Some(narrow)), Some(1)).is_err());
        let gone = |p| p != 7;
        assert!(float_place(&open(None, Some(cells)), Some(7), gone).is_err());
        assert!(float_place(&open(Some([0.0, 0.0, 0.5, 0.5]), None), Some(7), gone).is_err());
        assert_eq!(float_place(&open(None, None), Some(7), gone), Ok(None));
    }
}
