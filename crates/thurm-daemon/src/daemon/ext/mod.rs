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
use thurm_proto::{
    CreatePane, Event, PaneId, PaneInfo, PortEntry, Request, Response, ServerMessage,
};

use super::{Client, Daemon};
use crate::user_exec;
use registry::{Provider, Registry};

const PORTS_CHANGED: &str = "ports_changed";
const FORWARD_OPENED: &str = "forward_opened";
const FORWARD_CLOSED: &str = "forward_closed";

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
    /// Every app that forwards (by client), with the forwards it reported last
    /// ([`method::FORWARDS_REPORT`]). Its ids are that app's own: plugins see only the first
    /// app's (see [`Daemon::forwarding_app`]).
    forwards: Mutex<HashMap<u64, Vec<ext::ForwardInfo>>>,
    /// Held from a change of `forwards` until its `forward_opened` and `forward_closed` are
    /// sent (a report, an app gone): plugins hear the changes in the order they were made.
    /// Taken before any other of these locks.
    forward_events: Mutex<()>,
    /// The remote hosts' port lists each app relayed last ([`method::PORTS_REPORT`]), by
    /// client, host and pane: what a new subscriber is told first. Taken before `forwards`,
    /// whose first app is the one whose lists plugins hear.
    remote_ports: Mutex<HashMap<u64, RemoteLists>>,
    /// Subscribers to `ports_changed` waiting for the look that sends them their first lists:
    /// until it is done, they hear none of the watch's changes (a look under way when they
    /// subscribed may have a change they would take for a server just started). With each,
    /// whether that look left it out (it had unsubscribed meanwhile): back by the time the
    /// look is done, it is queued again.
    ports_joining: Mutex<HashMap<u64, bool>>,
}

/// A remote host's panes' lists: by host and pane, the ports and whether they are complete.
type RemoteLists = std::collections::BTreeMap<(String, u64), RemoteList>;

/// A remote pane's list as its app reported it last.
#[derive(Clone, PartialEq)]
struct RemoteList {
    ports: Vec<PortEntry>,
    complete: bool,
    /// Its host's daemon sent it as running already (`initial`), not as a change.
    initial: bool,
}

/// `ports_changed` as plugins hear it: of pane `id`, with `initial` when it was sent as running
/// already. A local pane adds `pane` (its info), a remote one `host`.
fn ports_changed(id: u64, ports: &[PortEntry], complete: bool, initial: bool) -> Value {
    let mut payload =
        json!({ "event": PORTS_CHANGED, "id": id, "ports": ports, "complete": complete });
    if initial {
        payload["initial"] = true.into();
    }
    payload
}

impl RemoteList {
    /// Listens on nothing, for sure (an empty, complete list).
    fn is_known_empty(&self) -> bool {
        self.ports.is_empty() && self.complete
    }
}

/// `ports_changed` of remote `host`'s `pane`; `initial` as given (what its daemon said, or that
/// a new subscriber hears it as running already).
fn remote_payload(host: &str, pane: u64, list: &RemoteList, initial: bool) -> Value {
    let mut payload = ports_changed(pane, &list.ports, list.complete, initial);
    payload["host"] = host.into();
    payload
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

/// `key` of `v` as an unsigned integer (an id): none when absent or null, refused when it is
/// anything else (`-1`, `"3"`), rather than taken for none.
fn opt_u64(v: &Value, key: &str) -> Result<Option<u64>, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => x
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("bad {key:?}: {x} is not an id")),
    }
}

/// `key` of `v` as a boolean: none when absent or null, refused when it is anything else.
fn opt_bool(v: &Value, key: &str) -> Result<Option<bool>, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => x
            .as_bool()
            .map(Some)
            .ok_or_else(|| format!("bad {key:?}: {x} is not true or false")),
    }
}

/// The remote lists `app` relayed, as `ports_changed` payloads of servers running already (for
/// a subscriber that just came: whatever they were, they run now).
fn remote_lists(all: &HashMap<u64, RemoteLists>, app: Option<u64>) -> Vec<Value> {
    app.and_then(|a| all.get(&a))
        .into_iter()
        .flatten()
        .map(|((host, pane), list)| remote_payload(host, *pane, list, true))
        .collect()
}

/// Refuses `method` unless `client` is the app, which opens forwards.
fn only_the_app(client: &Client, method: &str) -> Result<(), String> {
    if client.ext_forwards.load(Ordering::Relaxed) {
        Ok(())
    } else {
        Err(format!("{method} is only for the Thurm app"))
    }
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
            method::EVENTS_SUBSCRIBE => self.ext_subscribe(client, names(&p)),
            method::EVENTS_UNSUBSCRIBE => {
                // A client waiting for its first lists stays marked: it is still queued in the
                // watch, whose look clears the mark (`ext_ports_served`), and a subscription
                // again meanwhile is not queued twice.
                let names = names(&p);
                self.ext.registry.lock().unsubscribe(client.id, &names);
                // The last to hear of ports may have left: the watch sees whether to go on.
                if names.is_empty() || names.iter().any(|n| n == PORTS_CHANGED) {
                    self.ports.poke();
                }
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
            method::PORTS_LIST => {
                // No pane is every pane; a pane that is not one is refused, not taken for none.
                let pane = opt_u64(&p, "pane")?;
                let mut list = match self.answer(client, Request::Ports { pane })? {
                    Response::Ports(list) => list,
                    other => return Err(format!("unexpected answer: {other:?}")),
                };
                // A pane asked for whose processes were not looked at (it has none yet, or no
                // more) has a list too: empty, and complete only when nothing runs in it.
                if let Some(p) = pane
                    && !list.iter().any(|e| e.pane == p)
                {
                    let alive = self.pane(p).is_some_and(|x| x.state.lock().info.alive);
                    list.push(thurm_proto::PanePorts {
                        pane: p,
                        ports: Vec::new(),
                        complete: !alive,
                    });
                }
                Ok(json!(list))
            }
            method::PORTS_REPORT => {
                only_the_app(client, method)?;
                let host = str_param(&p, "host")?;
                // The app's connection to the host is gone: what it listed is not known now.
                // Plugins that heard its lists hear them end: nothing is known of them now.
                // The lists are kept and sent under their lock: what plugins hear of a pane is in
                // the order it was kept, whoever sends it (a report, a replay).
                if opt_bool(&p, "gone")? == Some(true) {
                    let mut all = self.ext.remote_ports.lock();
                    let mut ended = Vec::new();
                    if let Some(lists) = all.get_mut(&client.id) {
                        lists.retain(|(h, pane), list| {
                            // Plugins know an empty pane listens on nothing: no news.
                            if h == host && !list.is_known_empty() {
                                ended.push((h.clone(), *pane));
                            }
                            h != host
                        });
                    }
                    let heard = self.forwarding_app_id() == Some(client.id);
                    if heard {
                        self.remote_lists_ended(&ended);
                    }
                    return Ok(json!({ "heard": heard }));
                }
                let pane = opt_u64(&p, "pane")?.ok_or("missing \"pane\"")?;
                let ports: Vec<PortEntry> =
                    serde_json::from_value(p.get("ports").cloned().unwrap_or(json!([])))
                        .map_err(|e| format!("bad ports: {e}"))?;
                let complete = opt_bool(&p, "complete")?.unwrap_or(true);
                let initial = opt_bool(&p, "initial")? == Some(true);
                let mut all = self.ext.remote_ports.lock();
                // Not for an app that went: its lists went with it (`ext_client_gone`, which takes
                // this lock after the flag is set), and none are made for it again.
                if client.gone.load(Ordering::SeqCst) {
                    return Err("this client was disconnected".into());
                }
                let lists = all.entry(client.id).or_default();
                let at = (host.to_owned(), pane);
                // An empty list that could not read every process may have missed servers: kept,
                // so a later subscriber knows the pane is unsure, until a complete one or the
                // host going says otherwise.
                let list = RemoteList {
                    ports,
                    complete,
                    initial,
                };
                let holder = self.forwarding_app_id() == Some(client.id);
                // Kept even when empty for another app: one that takes over lists what it knows,
                // an empty pane too, rather than leave plugins guessing (a pane it never listed is
                // `gone`). The holder's empty pane is dropped once plugins heard it (a closed
                // pane's would pile up): what it said is all there is to say.
                // A pane plugins never heard listen on anything, still listening on nothing: no news
                // (as many come as the app connects to a host: a list for each of its panes).
                let unheard = !lists.contains_key(&at);
                if holder && list.is_known_empty() {
                    lists.remove(&at);
                    if unheard {
                        return Ok(json!({ "heard": true }));
                    }
                } else {
                    lists.insert(at, list.clone());
                }
                // Only the app that holds plugins' forwards is heard, as with its forwards. The
                // answer says whether plugins heard it: an app sends one they did not again as it
                // came.
                if !holder {
                    return Ok(json!({ "heard": false }));
                }
                // Tests: a report that takes a while, for what happens while it holds the lock.
                if let Some(delay) = crate::ports::test_delay("THURM_EXT_REPORT_DELAY_MS") {
                    std::thread::sleep(delay);
                }
                // `initial`: listening already when the app connected to the host, not news.
                self.emit(
                    PORTS_CHANGED,
                    None,
                    remote_payload(host, pane, &list, list.initial),
                );
                drop(all);
                Ok(json!({ "heard": true }))
            }
            method::FORWARDS_REPORT => {
                only_the_app(client, method)?;
                let report: ext::ForwardsReport = params(p)?;
                self.forwards_reported(client, report);
                Ok(Value::Null)
            }
            method::FORWARDS_LIST => {
                let app = self.forwarding_app()?;
                let forwards = self.ext.forwards.lock();
                let mut list: Vec<&ext::ForwardInfo> =
                    forwards.get(&app.id).into_iter().flatten().collect();
                list.sort_by_key(|f| f.id);
                Ok(json!(list))
            }
            method::FORWARDS_OPEN => {
                let open: ext::ForwardOpen = params(p)?;
                if open.host.is_empty() || open.port == 0 {
                    return Err("a host and a port to forward".into());
                }
                let app = self.forwarding_app()?;
                self.ask(&app, client, |id| ext::UiRequest::ForwardOpen { id, open })
            }
            method::FORWARDS_CLOSE => {
                let forward = opt_u64(&p, "id")?.ok_or("missing \"id\"")?;
                let app = self.forwarding_app()?;
                self.ask(&app, client, |id| ext::UiRequest::ForwardClose {
                    id,
                    forward,
                })
            }
            other => self.plugin_method(client, other, p),
        }
    }

    /// `events.subscribe`: `client` hears `names` from now on.
    ///
    /// Locks, always in this order: the remote lists' (`ports.report` takes it first too), the
    /// registry's, then `ports_joining`. For `ports_changed`, the remote lists' lock is held from
    /// before the registration until the replay is sent, so a report cannot reach a new
    /// subscriber between the two; other subscriptions don't wait on reports.
    fn ext_subscribe(
        self: &Arc<Self>,
        client: &Arc<Client>,
        names: Vec<String>,
    ) -> Result<Value, String> {
        let ports = names.iter().any(|n| n == PORTS_CHANGED);
        // Only one that joins (it does not hear `ports_changed` yet) takes the remote lists' lock;
        // a subscription that joins nothing does not wait on reports.
        let joining = ports && !self.ext.registry.lock().has(client.id, PORTS_CHANGED);
        let mut all = joining.then(|| self.ext.remote_ports.lock());
        // Only a new subscriber joins: one that has the lists keeps hearing changes.
        // `queue`: not waiting in the watch already (subscribed, out and in again before the look
        // that serves it): queued once, it gets its first lists once.
        let (joins, queue) = {
            let mut reg = self.ext_registry(client)?;
            let mut joins = ports && !reg.has(client.id, PORTS_CHANGED);
            // It stopped hearing them since the look above (another call of the same client):
            // the lock first, in its order, then the registry again.
            if joins && all.is_none() {
                drop(reg);
                all = Some(self.ext.remote_ports.lock());
                reg = self.ext_registry(client)?;
                joins = !reg.has(client.id, PORTS_CHANGED);
            }
            // Waiting for its first lists before it is a listener: no change of a look under way
            // reaches it first.
            let queue = joins
                && match self.ext.ports_joining.lock().entry(client.id) {
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(false);
                        true
                    }
                    std::collections::hash_map::Entry::Occupied(_) => false,
                };
            if let Err(e) = reg.subscribe(client.id, names) {
                if queue {
                    self.ext.ports_joining.lock().remove(&client.id);
                }
                return Err(e);
            }
            (joins, queue)
        };
        // Its panes' ports are watched only while someone hears of them. A new subscriber joins
        // the watch as a client does: the next look sends it alone every pane's list, empty ones
        // too (`ext_ports_to`), as running already. Of remote hosts, it gets the lists the app
        // relayed so far, the same way.
        if joins && let Some(all) = &all {
            if queue {
                self.ports.subscribed(self.clone(), client.id);
            } else {
                // Still queued from before: the watch may have gone to sleep meanwhile (nobody
                // listened); wake it to serve the queue.
                self.ports.poke();
            }
            for payload in remote_lists(all, self.forwarding_app_id()) {
                send_ext(client, notice::EVENT, &payload);
            }
        }
        drop(all);
        Ok(Value::Null)
    }

    /// The app that holds plugins' forwards, connected or leaving now.
    fn forwarding_app_id(&self) -> Option<u64> {
        self.ext.forwards.lock().keys().min().copied()
    }

    /// Remote panes whose lists plugins heard and nobody keeps up now (the app lost the host,
    /// or quit): each gets an empty list, not complete, as nothing is known of its ports, with
    /// `gone` (an incomplete list is otherwise a read that missed some processes).
    fn remote_lists_ended(&self, panes: &[(String, u64)]) {
        let payloads = panes.iter().map(|(host, pane)| {
            let mut payload = remote_payload(
                host,
                *pane,
                &RemoteList {
                    ports: Vec::new(),
                    complete: false,
                    initial: false,
                },
                false,
            );
            payload["gone"] = true.into();
            payload
        });
        self.deliver_ports(payloads.collect());
    }

    /// Sends `ports_changed` payloads to its subscribers, who are looked up once for all.
    fn deliver_ports(&self, payloads: Vec<Value>) {
        if payloads.is_empty() {
            return;
        }
        let listeners = self.ext.registry.lock().listeners(PORTS_CHANGED);
        for payload in &payloads {
            self.send_event(&listeners, payload);
        }
    }

    /// An app that forwards connected ([`thurm_proto::CAP_EXT_FORWARDS`]).
    pub(super) fn ext_forwarding_app_came(&self, client: u64) {
        self.ext.forwards.lock().entry(client).or_default();
    }

    /// The app that holds plugins' forwards: the one that connected first, for as long as it
    /// stays. Each app numbers its own forwards, so listing, opening, closing and the forward
    /// events all go by this one app's; another app's are heard once it is the first.
    fn forwarding_app(&self) -> Result<Arc<Client>, String> {
        let first = self.ext.forwards.lock().keys().min().copied();
        first
            .and_then(|id| self.clients.lock().get(&id).cloned())
            .filter(|c| !c.gone.load(Ordering::SeqCst))
            .ok_or_else(|| "no Thurm app that forwards ports is open (or it is too old)".into())
    }

    /// The forwards app `client` has now. Of the first app's, the ones that came are
    /// `forward_opened`, the ones that went `forward_closed`, with why when the report says;
    /// another app's are only kept.
    fn forwards_reported(&self, app: &Client, report: ext::ForwardsReport) {
        let client = app.id;
        let _ordered = self.ext.forward_events.lock();
        let changed = report.change.get("forward").and_then(|f| f.get("id"));
        let why = report
            .change
            .get("why")
            .and_then(Value::as_str)
            .unwrap_or("closed");
        let old = {
            let mut forwards = self.ext.forwards.lock();
            // Not for an app that went: its forwards went with it.
            if app.gone.load(Ordering::SeqCst) {
                return;
            }
            let old = forwards
                .insert(client, report.forwards.clone())
                .unwrap_or_default();
            if forwards.keys().min() != Some(&client) {
                return;
            }
            old
        };
        for f in old
            .iter()
            .filter(|f| !report.forwards.iter().any(|n| n.id == f.id))
        {
            let why = if changed.and_then(Value::as_u64) == Some(f.id) {
                why
            } else {
                "closed"
            };
            self.forward_event(FORWARD_CLOSED, f, Some(why));
        }
        for f in report
            .forwards
            .iter()
            .filter(|f| !old.iter().any(|o| o.id == f.id))
        {
            self.forward_event(FORWARD_OPENED, f, None);
        }
    }

    fn forward_event(&self, event: &str, f: &ext::ForwardInfo, why: Option<&str>) {
        let mut extra = json!(f);
        if let Some(why) = why {
            extra["why"] = why.into();
        }
        self.emit(event, None, extra);
    }

    /// Whether a subscriber wants its panes' ports watched: one that named `ports_changed`.
    pub(super) fn ext_watches_ports(&self) -> bool {
        self.ext.registry.lock().named(PORTS_CHANGED)
    }

    /// A change the watch found, for the extension API's subscribers but those in `except`:
    /// the ones that joined at this look, which get every list on their own
    /// ([`Daemon::ext_ports_to`]).
    pub(super) fn ext_ports_changed(&self, event: &Event, except: &[u64]) {
        // `ports_changed` has no `[[on]]` hooks: only the subscribers hear it. Left out: those still
        // waiting for their first lists (they get them on their own, and a change must not come
        // before them). With none, nothing is built.
        let listeners: Vec<u64> = self.ext.registry.lock().listeners(PORTS_CHANGED);
        let joining = self.ext.ports_joining.lock();
        let listeners: Vec<u64> = listeners
            .into_iter()
            .filter(|c| !except.contains(c) && !joining.contains_key(c))
            .collect();
        drop(joining);
        if listeners.is_empty() {
            return;
        }
        if let Some(payload) = self.ports_payload(event) {
            self.send_event(&listeners, &payload);
        }
    }

    /// A list of the watch for `client` alone, which joined it by subscribing: sent as running
    /// already, while it still hears `ports_changed`.
    pub(super) fn ext_ports_to(&self, client: &Client, event: &Event) {
        let reg = self.ext.registry.lock();
        if !reg.has(client.id, PORTS_CHANGED) {
            // Left out: queued again if it is back before the look is done. Under the
            // registry's lock, as a subscription marks it.
            if let Some(missed) = self.ext.ports_joining.lock().get_mut(&client.id) {
                *missed = true;
            }
            return;
        }
        drop(reg);
        if let Some(payload) = self.ports_payload(event) {
            send_ext(client, notice::EVENT, &payload);
        }
    }

    /// The look that sent `joined` their first lists is done: they hear changes now.
    pub(super) fn ext_ports_served(&self, joined: &[u64]) {
        // Tests: a look that takes a while to be done after it sent its lists.
        if !joined.is_empty()
            && let Some(delay) = crate::ports::test_delay("THURM_EXT_SERVED_DELAY_MS")
        {
            std::thread::sleep(delay);
        }
        let reg = self.ext.registry.lock();
        let mut joining = self.ext.ports_joining.lock();
        let mut again = Vec::new();
        for c in joined {
            // Left out by this look and subscribed again since: the next look serves it.
            if joining.remove(c) == Some(true) && reg.has(*c, PORTS_CHANGED) {
                joining.insert(*c, false);
                again.push(*c);
            }
        }
        drop(joining);
        drop(reg);
        for c in again {
            self.ports.rejoin(c);
        }
    }

    /// `ports_changed`'s payload for `event`, with `pane` while the pane is there, and
    /// `initial` when the watch sent it as a list of what was listening already as the
    /// subscriber joined.
    fn ports_payload(&self, event: &Event) -> Option<Value> {
        let Event::PortsChanged {
            pane,
            ports,
            complete,
            initial,
        } = event
        else {
            return None;
        };
        let mut payload = ports_changed(*pane, ports, *complete, *initial);
        if let Some(info) = self.pane(*pane).map(|p| p.state.lock().info.clone()) {
            payload["pane"] = serde_json::to_value(info).unwrap_or_default();
        }
        Some(payload)
    }

    /// Files sent to `pane` landed and their paths were typed there.
    pub(crate) fn files_uploaded(&self, pane: PaneId, paths: Vec<String>) {
        let info = self.pane(pane).map(|p| p.state.lock().info.clone());
        self.emit(
            "files_uploaded",
            info.as_ref(),
            json!({ "id": pane, "paths": paths }),
        );
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

    /// Forgets what `client` provided, heard and was asked.
    pub(super) fn ext_client_gone(&self, client: u64) {
        // Dropping the senders ends the waits on them.
        self.ext.pending.lock().retain(|_, p| p.to != client);
        // An app's forwards end with it; when it was the first, the next app's are plugins'
        // now. Their events go out before any report of the next app changes them.
        let ordered = self.ext.forward_events.lock();
        // The lists' lock is taken before the next app holds them, as a report takes it before
        // it asks who holds them: a report of the next app is heard as the holder's only after
        // the handover, which then does not tell it again.
        let mut all = self.ext.remote_ports.lock();
        let (first, gone, next, next_app) = {
            let mut forwards = self.ext.forwards.lock();
            let first = forwards.keys().min() == Some(&client);
            let gone = forwards.remove(&client).filter(|_| first);
            let next_app = forwards.keys().min().copied().filter(|_| first);
            let next = next_app.and_then(|id| forwards.get(&id).cloned());
            (first, gone, next, next_app)
        };
        // Tests: a handover that takes a while, for a report of the next app meanwhile.
        if next_app.is_some()
            && let Some(delay) = crate::ports::test_delay("THURM_EXT_HANDOVER_DELAY_MS")
        {
            std::thread::sleep(delay);
        }
        // The panes only the app that held plugins' lists knew of: no one tells of them now.
        // Under the lists' lock, as their reports: the next app's newer lists come after.
        {
            let had = all.remove(&client).unwrap_or_default();
            let next_has = next_app.and_then(|a| all.get(&a));
            let ended: Vec<(String, u64)> = had
                .iter()
                .filter(|(k, list)| {
                    first && !list.is_known_empty() && !next_has.is_some_and(|n| n.contains_key(*k))
                })
                .map(|(k, _)| k.clone())
                .collect();
            self.remote_lists_ended(&ended);
            // The remote lists heard from now on are that app's. Plugins heard the first app's:
            // a pane the next one lists the same is no news; any other list goes as that app
            // had it, a change or running already, as its host's daemon said.
            let mut news = Vec::new();
            for ((host, pane), list) in next_has.into_iter().flatten() {
                let heard = had.get(&(host.clone(), *pane));
                if heard.is_some_and(|h| h.ports == list.ports && h.complete == list.complete) {
                    continue;
                }
                // A pane plugins never heard of that listens on nothing: no news either.
                if heard.is_none() && list.is_known_empty() {
                    continue;
                }
                news.push(remote_payload(host, *pane, list, list.initial));
            }
            self.deliver_ports(news);
            // The next app holds plugins' lists now: its empty panes were said, or are no news.
            if let Some(lists) = next_app.and_then(|a| all.get_mut(&a)) {
                lists.retain(|_, list| !list.is_known_empty());
            }
        }
        drop(all);
        // Tests: an app gone takes a while to tell of the next app's forwards.
        if next.as_ref().is_some_and(|n| !n.is_empty())
            && let Some(delay) = crate::ports::test_delay("THURM_EXT_FORWARD_EVENT_DELAY_MS")
        {
            std::thread::sleep(delay);
        }
        for f in gone.iter().flatten() {
            self.forward_event(FORWARD_CLOSED, f, Some("app"));
        }
        for f in next.iter().flatten() {
            self.forward_event(FORWARD_OPENED, f, None);
        }
        drop(ordered);
        let (changed, watched) = {
            let mut registry = self.ext.registry.lock();
            let watched = registry.has(client, PORTS_CHANGED);
            (registry.client_gone(client), watched)
        };
        if changed {
            self.actions_changed();
        }
        // The last to hear of ports may have gone without unsubscribing: the watch sees whether
        // to go on, as on `events.unsubscribe`.
        if watched {
            self.ports.poke();
        }
        // After the registry's cleanup: a subscription under way holds the registry's lock while
        // it marks the client as joining, so it is done by now, and its mark goes too.
        self.ext.ports_joining.lock().remove(&client);
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
        self.ask(&app, caller, make)
    }

    /// Sends `make(id)` to `app` and waits for its answer.
    fn ask(
        &self,
        app: &Arc<Client>,
        caller: &Client,
        make: impl FnOnce(u64) -> ext::UiRequest,
    ) -> Result<Value, String> {
        // Nothing is asked for a caller that went: the app would act for no one.
        if caller.gone.load(Ordering::SeqCst) {
            return Err("the caller went away".into());
        }
        let (id, rx) = self.ext_question(app.id, 0);
        if app.gone.load(Ordering::SeqCst) {
            self.ext.pending.lock().remove(&id);
            return Err("the app went away".into());
        }
        send_ext(app, notice::UI_REQUEST, &json!(make(id)));
        let answer = self.ext_wait(id, &rx, "the app", caller);
        if answer.is_err() {
            send_ext(app, notice::UI_CANCEL, &json!({ "id": id }));
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
