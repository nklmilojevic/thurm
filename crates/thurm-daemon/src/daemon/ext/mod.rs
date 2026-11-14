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

/// The extension API's state in the daemon.
#[derive(Default)]
pub struct Ext {
    registry: Mutex<Registry>,
    /// Questions sent to clients (an action to carry out, a picker to show), by id: the asked
    /// client and where its answer goes.
    pending: Mutex<HashMap<u64, (u64, Sender<ext::Reply>)>>,
    next: AtomicU64,
}

/// Whether `method` can wait (on a program, another client, the user): such a request is not
/// answered on the connection's reader thread.
pub fn blocks(method: &str) -> bool {
    matches!(
        method,
        method::REQUEST
            | method::ACTIONS_RUN
            | method::UI_PICK
            | method::UI_INPUT
            | method::UI_CONFIRM
            | method::EXEC
            | method::SCRIPT_RUN
            | method::PLUGINS_RELOAD
            | method::PLUGINS_ENABLE
            | method::PLUGINS_DISABLE
    )
}

/// Methods that run programs or change what runs on this machine: host administration, for
/// clients on this machine (and plugins) only.
fn local_only(method: &str) -> bool {
    matches!(
        method,
        method::EXEC
            | method::SCRIPT_RUN
            | method::PLUGINS_RELOAD
            | method::PLUGINS_ENABLE
            | method::PLUGINS_DISABLE
    )
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
        if local_only(method) && client_is_tailnet(client) {
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
                self.run_action(&origin, run)
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
            method::UI_PICK => {
                let pick: ext::Pick = params(p)?;
                if pick.items.is_empty() {
                    return Ok(Value::Null);
                }
                self.ask_ui(|id| ext::UiRequest::Pick { id, pick })
            }
            method::UI_INPUT => {
                let input: ext::Input = params(p)?;
                self.ask_ui(|id| ext::UiRequest::Input { id, input })
            }
            method::UI_CONFIRM => {
                let confirm: ext::Confirm = params(p)?;
                let answer = self.ask_ui(|id| ext::UiRequest::Confirm { id, confirm })?;
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
                )
                .map_err(|e| format!("cannot run {:?}: {e}", exec.argv[0]))?;
                serde_json::to_value(result).map_err(|e| e.to_string())
            }
            method::LOG => {
                let message = str_param(&p, "message")?;
                let source = self.ext_source(client);
                match p.get("level").and_then(Value::as_str).unwrap_or("info") {
                    "error" => log::error!("{source}: {message}"),
                    "warn" => log::warn!("{source}: {message}"),
                    "debug" => log::debug!("{source}: {message}"),
                    _ => log::info!("{source}: {message}"),
                }
                Ok(Value::Null)
            }
            other => self.ext_call_more(client, other, p),
        }
    }

    /// Methods of later layers (plugins, scripts); unknown here.
    fn ext_call_more(
        self: &Arc<Self>,
        _client: &Arc<Client>,
        method: &str,
        _params: Value,
    ) -> Result<Value, String> {
        Err(format!("unknown method {method:?}"))
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

    /// Runs the action `run.name` and returns what it returned.
    pub fn run_action(
        self: &Arc<Self>,
        origin: &str,
        run: ext::RunAction,
    ) -> Result<Value, String> {
        if run.depth >= MAX_DEPTH {
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
        let pane = run.pane.or_else(|| self.focused_pane());
        match entry.provider {
            Provider::Config => self.run_command(&run.name, pane).map(|_| Value::Null),
            Provider::Client(id) => {
                let client = self
                    .clients
                    .lock()
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| format!("{} is gone", entry.info.source))?;
                let (qid, rx) = self.ext_question(id);
                let invoke = ext::Invoke {
                    id: qid,
                    name: run.name.clone(),
                    args: run.args,
                    ctx: ext::Context {
                        pane,
                        origin: origin.to_owned(),
                        depth: run.depth + 1,
                    },
                };
                send_ext(&client, notice::ACTION_INVOKE, &json!(invoke));
                self.ext_wait(qid, &rx, &entry.info.source)
            }
        }
    }

    /// A new question to client `to`: its id, and where the answer comes.
    fn ext_question(&self, to: u64) -> (u64, Receiver<ext::Reply>) {
        let id = self.ext.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = bounded(1);
        self.ext.pending.lock().insert(id, (to, tx));
        (id, rx)
    }

    fn ext_wait(&self, id: u64, rx: &Receiver<ext::Reply>, who: &str) -> Result<Value, String> {
        let reply = rx.recv_timeout(REPLY_TIMEOUT);
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

    /// An answer from client `from` to one of its questions; answers to someone else's are
    /// dropped.
    fn ext_reply(&self, from: u64, reply: ext::Reply) {
        let mut pending = self.ext.pending.lock();
        if pending.get(&reply.id).is_some_and(|(to, _)| *to == from)
            && let Some((_, tx)) = pending.remove(&reply.id)
        {
            let _ = tx.send(reply);
        }
    }

    /// Forgets what `client` provided, heard and was asked.
    pub(super) fn ext_client_gone(&self, client: u64) {
        // Dropping the senders ends the waits on them.
        self.ext.pending.lock().retain(|_, (to, _)| *to != client);
        if self.ext.registry.lock().client_gone(client) {
            self.actions_changed();
        }
    }

    /// The config's `[[commands]]` as actions; called whenever the config is applied.
    pub(super) fn ext_config_applied(&self) {
        let commands = self.config.read().commands.clone();
        self.ext.registry.lock().set_config(&commands);
        self.actions_changed();
    }

    /// Tells the clients that use the extension API to read the actions again.
    fn actions_changed(&self) {
        let clients: Vec<_> = self.clients.lock().values().cloned().collect();
        for c in clients {
            if c.ext.load(Ordering::Relaxed) {
                send_ext(&c, notice::ACTIONS_CHANGED, &json!({}));
            }
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
        let cmd = if open.float {
            UiCommand::OpenFloating {
                pane,
                target,
                background: open.background,
            }
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
    fn ask_ui(&self, make: impl FnOnce(u64) -> ext::UiRequest) -> Result<Value, String> {
        let app = self
            .ui_client()
            .ok_or("no Thurm window is open to ask in (or the app is too old)")?;
        let (id, rx) = self.ext_question(app.id);
        send_ext(&app, notice::UI_REQUEST, &json!(make(id)));
        let answer = self.ext_wait(id, &rx, "the app");
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
