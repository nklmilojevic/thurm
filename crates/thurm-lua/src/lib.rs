//! Lua plugins and scripts for `thurmd`.
//!
//! One thread runs every Lua state; each plugin (and each script) has a state of its own. Code
//! runs in coroutines: the main chunk, each action run, event handler and timer callback. A call
//! that can wait (running a program, an action, asking the user) is carried out on a worker
//! thread while its coroutine is suspended, so other plugins and handlers keep running.
//!
//! The daemon is a [`Host`]: plugins reach it only through the extension API
//! ([`thurm_proto::ext`]), as the client the host keeps for each plugin.

use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use mlua::{Function, HookTriggers, Lua, LuaSerdeExt, MultiValue, Table, Thread, VmState};
use serde_json::{Value, json};
use thurm_proto::ext::{method, notice};

const PRELUDE: &str = include_str!("prelude.lua");

/// Types of the `thurm` table for lua-language-server.
pub const TYPES: &str = include_str!("../types/thurm.lua");

/// How long code may run without waiting before it is stopped as a runaway.
const RUN_LIMIT: Duration = Duration::from_secs(5);

/// The most memory one Lua state may use.
const MEMORY_LIMIT: usize = 256 << 20;

/// What the runtime needs from the daemon.
pub trait Host: Send + Sync + 'static {
    /// Calls `method` of the extension API as the client of plugin `instance`. May block.
    fn call(&self, instance: u64, method: &str, params: Value) -> Result<Value, String>;
    /// An error in a plugin's code that nothing caught.
    fn report(&self, instance: u64, error: &str);
}

/// Where a script's `thurm.log` lines go: (level, message).
pub type LogSink = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// A plugin or script to run.
pub struct Spec {
    /// The host's id for it, used in every [`Host`] call.
    pub instance: u64,
    /// `thurm.plugin`, and the prefix of the hidden actions behind function key bindings.
    pub name: String,
    /// The code of its main chunk.
    pub source: String,
    /// The main chunk's file, for messages.
    pub file: String,
    /// The directory `require` looks in.
    pub dir: Option<PathBuf>,
    /// `thurm.settings`.
    pub settings: Value,
    /// `thurm.args`.
    pub args: Vec<String>,
    /// `thurm.pane`.
    pub pane: Option<u64>,
    /// Takes the plugin's log lines instead of the host's `log` method.
    pub log: Option<LogSink>,
    /// A script: it ends, and its state goes, when its main chunk returns.
    pub script: bool,
}

enum Job {
    Load(Box<Spec>, Sender<Result<Value, String>>),
    Unload(u64),
    Notice(u64, String, Value),
    Resume(u64, u64, Result<Value, String>),
    Stop,
}

/// Runs Lua plugins and scripts on a thread of its own.
pub struct Runtime {
    tx: Sender<Job>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Runtime {
    pub fn start(host: Arc<dyn Host>) -> std::io::Result<Runtime> {
        let (tx, rx) = unbounded();
        let engine_tx = tx.clone();
        let thread = std::thread::Builder::new()
            .name("lua".into())
            .spawn(move || Engine::new(host, engine_tx).run(rx))?;
        Ok(Runtime {
            tx,
            thread: Some(thread),
        })
    }

    /// Starts a plugin: its main chunk runs. The receiver gets the chunk's result once it
    /// returned (for a script, once the script ended), or why it failed.
    pub fn load(&self, spec: Spec) -> Receiver<Result<Value, String>> {
        let (done_tx, done_rx) = bounded(1);
        if let Err(e) = self.tx.send(Job::Load(Box::new(spec), done_tx))
            && let Job::Load(_, done) = e.into_inner()
        {
            let _ = done.send(Err("the Lua runtime stopped".into()));
        }
        done_rx
    }

    /// Stops a plugin: its state goes, with its timers and whatever it was waiting for.
    pub fn unload(&self, instance: u64) {
        let _ = self.tx.send(Job::Unload(instance));
    }

    /// Something the daemon sent the plugin's client ([`thurm_proto::ext::notice`]).
    pub fn notice(&self, instance: u64, name: &str, payload: Value) {
        let _ = self
            .tx
            .send(Job::Notice(instance, name.to_owned(), payload));
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.tx.send(Job::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What a coroutine is for.
#[derive(Clone, Copy)]
enum TaskKind {
    Main,
    /// An action run: its answer goes to the daemon's question `reply`.
    Action {
        reply: u64,
    },
    Handler,
    Timer,
}

struct Task {
    thread: Thread,
    kind: TaskKind,
}

/// What a timer does when it fires.
enum Fire {
    Call(Function),
    /// Resumes the coroutine waiting on this ticket (`thurm.sleep`).
    Wake(u64),
}

struct Timer {
    instance: u64,
    every: Option<Duration>,
    fire: Fire,
}

/// The part of a plugin its Lua functions change while it runs.
#[derive(Default)]
struct Shared {
    actions: HashMap<String, Function>,
    handlers: HashMap<String, Vec<Function>>,
    /// Coroutines the runtime runs: only these may wait.
    managed: HashSet<usize>,
    /// The action depth of each running action coroutine.
    depth: HashMap<usize, u32>,
    /// Tickets a coroutine of this plugin waits on.
    waiting: HashSet<u64>,
    new_timers: Vec<(u64, Duration, Option<Duration>, Fire)>,
    cancelled: HashSet<u64>,
}

struct Plugin {
    lua: Lua,
    name: String,
    script: bool,
    shared: Rc<RefCell<Shared>>,
    deadline: Rc<Cell<Instant>>,
    /// Suspended coroutines, by the ticket they wait on.
    tasks: HashMap<u64, Task>,
    main_done: Option<Sender<Result<Value, String>>>,
}

struct Engine {
    host: Arc<dyn Host>,
    tx: Sender<Job>,
    plugins: HashMap<u64, Plugin>,
    timers: BinaryHeap<Reverse<(Instant, u64)>>,
    timer_info: HashMap<u64, Timer>,
    ids: Arc<AtomicU64>,
}

fn ptr(t: &Thread) -> usize {
    t.to_pointer() as usize
}

impl Engine {
    fn new(host: Arc<dyn Host>, tx: Sender<Job>) -> Engine {
        Engine {
            host,
            tx,
            plugins: HashMap::new(),
            timers: BinaryHeap::new(),
            timer_info: HashMap::new(),
            ids: Arc::new(AtomicU64::new(1)),
        }
    }

    fn run(mut self, rx: Receiver<Job>) {
        loop {
            let job = match self.timers.peek() {
                Some(Reverse((at, _))) => {
                    match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(job) => Some(job),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match rx.recv() {
                    Ok(job) => Some(job),
                    Err(_) => return,
                },
            };
            match job {
                Some(Job::Stop) => return,
                Some(job) => self.handle(job),
                None => {}
            }
            self.fire_timers();
        }
    }

    fn handle(&mut self, job: Job) {
        match job {
            Job::Load(spec, done) => self.load(*spec, done),
            Job::Unload(instance) => self.unload(instance, "the plugin was stopped"),
            Job::Notice(instance, name, payload) => self.notice(instance, &name, payload),
            Job::Resume(instance, ticket, result) => self.wake(instance, ticket, result),
            Job::Stop => {}
        }
        self.collect_timers();
    }

    fn load(&mut self, spec: Spec, done: Sender<Result<Value, String>>) {
        let instance = spec.instance;
        let (lua, shared, deadline) = match self.state(&spec) {
            Ok(s) => s,
            Err(e) => {
                let _ = done.send(Err(describe(&e)));
                return;
            }
        };
        let main = match lua
            .load(&spec.source)
            .set_name(format!("@{}", short_path(&spec.file)))
            .into_function()
        {
            Ok(f) => f,
            Err(e) => {
                let _ = done.send(Err(describe(&e)));
                return;
            }
        };
        self.plugins.insert(
            instance,
            Plugin {
                lua,
                name: spec.name,
                script: spec.script,
                shared,
                deadline,
                tasks: HashMap::new(),
                main_done: Some(done),
            },
        );
        self.start(instance, main, MultiValue::new(), TaskKind::Main, None);
    }

    /// A fresh Lua state for `spec`, with `thurm` set up.
    #[allow(clippy::type_complexity)]
    fn state(&self, spec: &Spec) -> mlua::Result<(Lua, Rc<RefCell<Shared>>, Rc<Cell<Instant>>)> {
        let lua = Lua::new();
        lua.set_memory_limit(MEMORY_LIMIT)?;
        let deadline = Rc::new(Cell::new(Instant::now() + RUN_LIMIT));
        {
            let deadline = deadline.clone();
            lua.set_global_hook(
                HookTriggers::new().every_nth_instruction(10_000),
                move |_, _| {
                    if Instant::now() > deadline.get() {
                        Err(mlua::Error::runtime(format!(
                            "stopped after running {}s without waiting (an endless loop?)",
                            RUN_LIMIT.as_secs()
                        )))
                    } else {
                        Ok(VmState::Continue)
                    }
                },
            )?;
        }
        let shared = Rc::new(RefCell::new(Shared::default()));
        let p = self.primitives(&lua, spec, &shared)?;
        let thurm: Table = lua.load(PRELUDE).set_name("=[thurm]").call(p)?;
        lua.globals().set("thurm", thurm)?;
        Ok((lua, shared, deadline))
    }

    /// The runtime's functions the prelude builds `thurm` on.
    fn primitives(
        &self,
        lua: &Lua,
        spec: &Spec,
        shared: &Rc<RefCell<Shared>>,
    ) -> mlua::Result<Table> {
        let p = lua.create_table()?;
        let instance = spec.instance;
        p.set("plugin", spec.name.as_str())?;
        if let Some(dir) = &spec.dir {
            p.set("dir", dir.display().to_string())?;
        }
        p.set("settings", to_lua(lua, &spec.settings)?)?;
        p.set("args", spec.args.clone())?;
        p.set("pane", spec.pane)?;

        p.set(
            "blocks",
            lua.create_function(|_, m: String| Ok(thurm_proto::ext::method_blocks(&m)))?,
        )?;

        let host = self.host.clone();
        p.set(
            "call",
            lua.create_function(move |lua, (m, params): (String, mlua::Value)| {
                let params: Value = from_lua(lua, params)?;
                Ok(match host.call(instance, &m, params) {
                    Ok(v) => (true, to_lua(lua, &v)?),
                    Err(e) => (false, mlua::Value::String(lua.create_string(&e)?)),
                })
            })?,
        )?;

        let host = self.host.clone();
        let tx = self.tx.clone();
        let ids = self.ids.clone();
        let sh = shared.clone();
        p.set(
            "start",
            lua.create_function(move |lua, (m, params): (String, mlua::Value)| {
                let params: Value = from_lua(lua, params)?;
                let ticket = match waiting_ticket(lua, &sh, &ids) {
                    Ok(t) => t,
                    Err(e) => return Ok((None, Some(e))),
                };
                let host = host.clone();
                let tx = tx.clone();
                std::thread::Builder::new()
                    .name("lua-call".into())
                    .spawn(move || {
                        let result = host.call(instance, &m, params);
                        let _ = tx.send(Job::Resume(instance, ticket, result));
                    })
                    .map_err(mlua::Error::external)?;
                Ok((Some(ticket), None))
            })?,
        )?;

        let ids = self.ids.clone();
        let sh = shared.clone();
        p.set(
            "sleep",
            lua.create_function(move |lua, secs: f64| {
                let ticket = match waiting_ticket(lua, &sh, &ids) {
                    Ok(t) => t,
                    Err(e) => return Ok((None, Some(e))),
                };
                let id = ids.fetch_add(1, Ordering::Relaxed);
                sh.borrow_mut()
                    .new_timers
                    .push((id, seconds(secs), None, Fire::Wake(ticket)));
                Ok((Some(ticket), None))
            })?,
        )?;

        let ids = self.ids.clone();
        let sh = shared.clone();
        p.set(
            "timer",
            lua.create_function(move |_, (secs, f, repeat): (f64, Function, bool)| {
                let id = ids.fetch_add(1, Ordering::Relaxed);
                let after = seconds(secs);
                let every = repeat.then_some(after.max(Duration::from_millis(10)));
                sh.borrow_mut()
                    .new_timers
                    .push((id, after, every, Fire::Call(f)));
                Ok(id)
            })?,
        )?;

        let sh = shared.clone();
        p.set(
            "cancel",
            lua.create_function(move |_, id: u64| {
                sh.borrow_mut().cancelled.insert(id);
                Ok(())
            })?,
        )?;

        let sh = shared.clone();
        p.set(
            "set_action",
            lua.create_function(move |_, (name, f): (String, Function)| {
                sh.borrow_mut().actions.insert(name, f);
                Ok(())
            })?,
        )?;

        let sh = shared.clone();
        p.set(
            "on",
            lua.create_function(move |_, (event, f): (String, Function)| {
                let mut s = sh.borrow_mut();
                let list = s.handlers.entry(event).or_default();
                list.push(f);
                Ok(list.len() == 1)
            })?,
        )?;

        let sh = shared.clone();
        p.set(
            "depth",
            lua.create_function(move |lua, ()| {
                let me = ptr(&lua.current_thread());
                Ok(sh.borrow().depth.get(&me).copied().unwrap_or(0))
            })?,
        )?;

        let ids = self.ids.clone();
        p.set(
            "next_id",
            lua.create_function(move |_, ()| Ok(ids.fetch_add(1, Ordering::Relaxed)))?,
        )?;

        p.set(
            "json_encode",
            lua.create_function(|lua, v: mlua::Value| {
                let v: Value = from_lua(lua, v)?;
                Ok(v.to_string())
            })?,
        )?;
        p.set(
            "json_decode",
            lua.create_function(|lua, s: String| {
                let v: Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
                to_lua(lua, &v)
            })?,
        )?;

        let host = self.host.clone();
        let sink = spec.log.clone();
        p.set(
            "log",
            lua.create_function(move |_, (level, message): (String, String)| {
                match &sink {
                    Some(sink) => sink(&level, &message),
                    None => {
                        let _ = host.call(
                            instance,
                            method::LOG,
                            json!({ "level": level, "message": message }),
                        );
                    }
                }
                Ok(())
            })?,
        )?;
        Ok(p)
    }

    /// Runs `f` in a new coroutine of `instance`; `depth` for an action run.
    fn start(
        &mut self,
        instance: u64,
        f: Function,
        args: MultiValue,
        kind: TaskKind,
        depth: Option<u32>,
    ) {
        let Some(plugin) = self.plugins.get(&instance) else {
            return;
        };
        let thread = match plugin.lua.create_thread(f) {
            Ok(t) => t,
            Err(e) => {
                self.finish(instance, kind, Err(describe(&e)));
                return;
            }
        };
        {
            let mut s = plugin.shared.borrow_mut();
            s.managed.insert(ptr(&thread));
            if let Some(d) = depth {
                s.depth.insert(ptr(&thread), d);
            }
        }
        self.resume(instance, Task { thread, kind }, args);
    }

    /// The answer a coroutine waited on with `ticket` came: it carries on.
    fn wake(&mut self, instance: u64, ticket: u64, result: Result<Value, String>) {
        let Some(plugin) = self.plugins.get_mut(&instance) else {
            return;
        };
        plugin.shared.borrow_mut().waiting.remove(&ticket);
        let Some(task) = plugin.tasks.remove(&ticket) else {
            return;
        };
        let (ok, value) = match result {
            Ok(v) => (true, v),
            Err(e) => (false, Value::String(e)),
        };
        match to_lua(&plugin.lua, &value) {
            Ok(v) => {
                let args = MultiValue::from_vec(vec![mlua::Value::Boolean(ok), v]);
                self.resume(instance, task, args);
            }
            Err(e) => {
                self.end_task(instance, &task);
                self.finish(instance, task.kind, Err(describe(&e)));
            }
        }
    }

    fn resume(&mut self, instance: u64, task: Task, args: MultiValue) {
        let Some(plugin) = self.plugins.get_mut(&instance) else {
            return;
        };
        plugin.deadline.set(Instant::now() + RUN_LIMIT);
        let result = task.thread.resume::<MultiValue>(args);
        let suspended = task.thread.status() == mlua::thread::ThreadStatus::Resumable;
        let outcome = match result {
            Ok(values) if suspended => {
                let ticket = match values.front() {
                    Some(mlua::Value::Integer(t)) => Some(*t as u64),
                    _ => None,
                };
                match ticket.filter(|t| plugin.shared.borrow().waiting.contains(t)) {
                    Some(t) => {
                        plugin.tasks.insert(t, task);
                        return;
                    }
                    None => Err(
                        "coroutine.yield outside a coroutine of your own: Thurm runs \
                                 actions, handlers and timers in coroutines already"
                            .to_owned(),
                    ),
                }
            }
            Ok(values) => match values.into_iter().next() {
                Some(v) => from_lua::<Value>(&plugin.lua, v).map_err(|e| describe(&e)),
                None => Ok(Value::Null),
            },
            Err(e) => {
                let mut message = describe(&e);
                if let Some(at) = user_frame(&e.to_string())
                    && !message.contains(&at)
                {
                    message = format!("{at} {message}");
                }
                Err(message)
            }
        };
        self.end_task(instance, &task);
        self.finish(instance, task.kind, outcome);
    }

    fn end_task(&self, instance: u64, task: &Task) {
        if let Some(p) = self.plugins.get(&instance) {
            let me = ptr(&task.thread);
            let mut s = p.shared.borrow_mut();
            s.managed.remove(&me);
            s.depth.remove(&me);
        }
    }

    /// A coroutine ended with `result`.
    fn finish(&mut self, instance: u64, kind: TaskKind, result: Result<Value, String>) {
        match kind {
            TaskKind::Main => {
                let Some(plugin) = self.plugins.get_mut(&instance) else {
                    return;
                };
                if let Some(done) = plugin.main_done.take() {
                    let _ = done.send(result.clone());
                }
                if plugin.script {
                    self.unload(instance, "the script ended");
                } else if let Err(e) = &result {
                    log::warn!("plugin {} failed to start: {e}", plugin.name);
                }
            }
            TaskKind::Action { reply } => {
                let answer = match result {
                    Ok(v) => json!({ "id": reply, "result": v }),
                    Err(e) => json!({ "id": reply, "error": e }),
                };
                let _ = self.host.call(instance, method::ACTIONS_RESULT, answer);
            }
            TaskKind::Handler | TaskKind::Timer => {
                if let Err(e) = result {
                    self.host.report(instance, &e);
                }
            }
        }
    }

    fn unload(&mut self, instance: u64, why: &str) {
        let Some(mut plugin) = self.plugins.remove(&instance) else {
            return;
        };
        if let Some(done) = plugin.main_done.take() {
            let _ = done.send(Err(why.to_owned()));
        }
        self.timer_info.retain(|_, t| t.instance != instance);
    }

    fn notice(&mut self, instance: u64, name: &str, payload: Value) {
        let Some(plugin) = self.plugins.get(&instance) else {
            return;
        };
        match name {
            notice::ACTION_INVOKE => {
                let reply = payload["id"].as_u64().unwrap_or(0);
                let kind = TaskKind::Action { reply };
                let action = payload["name"].as_str().unwrap_or_default();
                let f = plugin.shared.borrow().actions.get(action).cloned();
                let Some(f) = f else {
                    let e = format!("{action:?} is not defined");
                    self.finish(instance, kind, Err(e));
                    return;
                };
                let depth = payload["ctx"]["depth"].as_u64().unwrap_or(1) as u32;
                let args = to_lua(&plugin.lua, &payload["ctx"]).and_then(|ctx| {
                    let args = to_lua(&plugin.lua, &payload["args"])?;
                    Ok(MultiValue::from_vec(vec![ctx, args]))
                });
                match args {
                    Ok(args) => self.start(instance, f, args, kind, Some(depth)),
                    Err(e) => self.finish(instance, kind, Err(describe(&e))),
                }
            }
            notice::EVENT => {
                let event = payload["event"].as_str().unwrap_or_default();
                let handlers: Vec<Function> = {
                    let s = plugin.shared.borrow();
                    s.handlers
                        .get(event)
                        .into_iter()
                        .chain(s.handlers.get("*"))
                        .flatten()
                        .cloned()
                        .collect()
                };
                for f in handlers {
                    let Some(plugin) = self.plugins.get(&instance) else {
                        return;
                    };
                    match to_lua(&plugin.lua, &payload) {
                        Ok(ev) => self.start(
                            instance,
                            f,
                            MultiValue::from_vec(vec![ev]),
                            TaskKind::Handler,
                            None,
                        ),
                        Err(e) => self.host.report(instance, &describe(&e)),
                    }
                }
            }
            _ => {}
        }
    }

    /// Takes the timers plugins set up or cancelled since the last look.
    fn collect_timers(&mut self) {
        let now = Instant::now();
        for (&instance, plugin) in &self.plugins {
            let mut s = plugin.shared.borrow_mut();
            for (id, after, every, fire) in s.new_timers.drain(..) {
                self.timers.push(Reverse((now + after, id)));
                self.timer_info.insert(
                    id,
                    Timer {
                        instance,
                        every,
                        fire,
                    },
                );
            }
            for id in s.cancelled.drain() {
                self.timer_info.remove(&id);
            }
        }
    }

    fn fire_timers(&mut self) {
        loop {
            self.collect_timers();
            let now = Instant::now();
            let Some(&Reverse((at, id))) = self.timers.peek() else {
                return;
            };
            if at > now {
                return;
            }
            self.timers.pop();
            let Some(timer) = self.timer_info.remove(&id) else {
                continue;
            };
            let instance = timer.instance;
            match timer.fire {
                Fire::Wake(ticket) => self.wake(instance, ticket, Ok(Value::Null)),
                Fire::Call(f) => {
                    if let Some(every) = timer.every {
                        self.timers.push(Reverse((now + every, id)));
                        self.timer_info.insert(
                            id,
                            Timer {
                                instance,
                                every: Some(every),
                                fire: Fire::Call(f.clone()),
                            },
                        );
                    }
                    self.start(instance, f, MultiValue::new(), TaskKind::Timer, None);
                }
            }
        }
    }
}

/// A ticket for the running coroutine to wait on; an error outside one the runtime runs.
fn waiting_ticket(lua: &Lua, shared: &Rc<RefCell<Shared>>, ids: &AtomicU64) -> Result<u64, String> {
    let me = ptr(&lua.current_thread());
    let mut s = shared.borrow_mut();
    if !s.managed.contains(&me) {
        return Err(
            "this call waits, which works only directly in an action, handler, timer \
                    or the main chunk (not in a coroutine of your own)"
                .into(),
        );
    }
    let ticket = ids.fetch_add(1, Ordering::Relaxed);
    s.waiting.insert(ticket);
    Ok(ticket)
}

fn seconds(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs.max(0.0)).unwrap_or(Duration::from_secs(365 * 86_400))
}

fn to_lua(lua: &Lua, v: &Value) -> mlua::Result<mlua::Value> {
    let options = mlua::serde::SerializeOptions::new()
        .serialize_none_to_null(false)
        .serialize_unit_to_null(false);
    lua.to_value_with(v, options)
}

fn from_lua<T: serde::de::DeserializeOwned>(lua: &Lua, v: mlua::Value) -> mlua::Result<T> {
    let options = mlua::serde::DeserializeOptions::new()
        .deny_unsupported_types(false)
        .deny_recursive_tables(false);
    lua.from_value_with(v, options)
}

/// An error as one line for the user: the message without Lua's traceback.
pub fn describe(e: &mlua::Error) -> String {
    let text = match e {
        mlua::Error::CallbackError { cause, .. } => return describe(cause),
        mlua::Error::WithContext { cause, .. } => return describe(cause),
        mlua::Error::RuntimeError(m) | mlua::Error::SyntaxError { message: m, .. } => m.clone(),
        other => other.to_string(),
    };
    let text = text
        .split("\nstack traceback:")
        .next()
        .unwrap_or_default()
        .trim();
    text.strip_prefix("runtime error: ")
        .unwrap_or(text)
        .to_owned()
}

/// The first frame of a traceback in the plugin's own files: `~/x/init.lua:12:`.
fn user_frame(traceback: &str) -> Option<String> {
    let (_, frames) = traceback.split_once("stack traceback:")?;
    frames
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("[C]") && !l.starts_with("[thurm]"))
        .find_map(|l| {
            let mut parts = l.splitn(3, ':');
            let file = parts.next()?;
            let line = parts.next()?;
            line.parse::<u32>().ok()?;
            Some(format!("{file}:{line}:"))
        })
}

/// `path` with the home directory as `~`.
fn short_path(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod tests;
