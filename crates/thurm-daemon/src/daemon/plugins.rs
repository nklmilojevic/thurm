//! The plugins the daemon runs: `init.lua` beside the config file, the plugins in `plugins/`,
//! and the scripts `thurm do` runs.
//!
//! Each running plugin has a client of its own, through which it uses the extension API like
//! any other client; the notices that client gets go to the plugin. A Lua plugin runs in the
//! daemon ([`thurm_lua`]); a program plugin runs alongside it and speaks JSON-RPC 2.0, one
//! message per line, on its standard input and output: its requests are the API's methods,
//! the daemon's notices come as notifications, and an action to carry out comes as an
//! `action.invoke` request it answers.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ChildStdin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};
use thurm_config::plugins::{FoundPlugin, INIT, PluginKind};
use thurm_proto::ext::{self, method, notice};
use thurm_proto::{Event, ServerMessage};

use super::ext::send_ext;
use super::{Client, Daemon};
use crate::user_exec;

/// How many lines of its log the daemon keeps per plugin.
const LOG_LINES: usize = 500;

/// A program plugin that ran this long before it exited starts again at once.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// The longest a failing program plugin waits before it is started again.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long a stopped program plugin has before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Plugins {
    lua: OnceLock<Option<thurm_lua::Runtime>>,
    instances: Mutex<HashMap<u64, Instance>>,
    next: AtomicU64,
    records: Mutex<BTreeMap<String, Record>>,
}

/// A running plugin or script, and its client.
struct Instance {
    client: Arc<Client>,
    /// The plugin it runs, or `None` for a script.
    plugin: Option<String>,
    /// For a script: the client that runs it, whose leaving stops it.
    owner: Option<u64>,
}

/// A plugin the daemon knows of, running or not.
struct Record {
    found: FoundPlugin,
    /// `plugins.settings.<name>`, as it was when the plugin started.
    settings: Value,
    state: &'static str,
    error: Option<String>,
    instance: Option<u64>,
    /// The program of a program plugin, while it runs.
    process: Option<Arc<Process>>,
    log: VecDeque<String>,
    /// Bumped whenever the plugin is stopped or started: a supervisor of an older start knows
    /// that the exit it saw is not for it to act on.
    generation: u64,
    /// Program exits in a row, each soon after starting.
    failures: u32,
}

impl Record {
    fn log(&mut self, line: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }
}

/// How many messages may wait for a program plugin to read them.
const OUTBOX: usize = 1024;

struct Process {
    pgid: libc::pid_t,
    /// Messages for its standard input, which a thread of their own writes: a plugin that
    /// writes before it reads must not hold up the thread reading its output. `None` once its
    /// input is closed.
    outbox: Mutex<Option<crossbeam_channel::Sender<String>>>,
    life: Mutex<Life>,
}

#[derive(Default)]
struct Life {
    /// Its process was waited for: its pid, and so its group's, may name another one now.
    reaped: bool,
    /// When it was asked to stop.
    stopping: Option<Instant>,
}

impl Process {
    fn new(pgid: libc::pid_t, stdin: Option<ChildStdin>) -> Arc<Process> {
        let (tx, rx) = crossbeam_channel::bounded::<String>(OUTBOX);
        if let Some(mut w) = stdin {
            let _ = std::thread::Builder::new()
                .name("plugin-stdin".into())
                .spawn(move || {
                    // Ends, closing its input, when the outbox is closed.
                    for line in rx {
                        if w.write_all(line.as_bytes())
                            .and_then(|()| w.flush())
                            .is_err()
                        {
                            return;
                        }
                    }
                });
        }
        Arc::new(Process {
            pgid,
            outbox: Mutex::new(Some(tx)),
            life: Mutex::new(Life::default()),
        })
    }

    /// Queues one JSON-RPC message. A plugin that leaves a full outbox unread is not reading
    /// its input at all: it is closed, which tells the plugin to stop.
    fn send(&self, msg: &Value) {
        let mut outbox = self.outbox.lock();
        if let Some(tx) = outbox.as_ref() {
            let mut line = msg.to_string();
            line.push('\n');
            if let Err(crossbeam_channel::TrySendError::Full(_)) = tx.try_send(line) {
                log::warn!(
                    "plugin process {} does not read its input; closing it",
                    self.pgid
                );
                *outbox = None;
            }
        }
    }

    /// Closes its input and sends SIGTERM; the thread waiting for it sends SIGKILL after
    /// [`STOP_GRACE`].
    fn stop(&self) {
        *self.outbox.lock() = None;
        let mut life = self.life.lock();
        if !life.reaped && life.stopping.is_none() {
            life.stopping = Some(Instant::now());
            unsafe { libc::kill(-self.pgid, libc::SIGTERM) };
        }
    }

    /// Waits for its process group. Once the program exits (or was asked to stop), what is
    /// left of its group gets SIGTERM, and SIGKILL [`STOP_GRACE`] later: nothing it started
    /// outlives it. The program is reaped only after that, so its group id stays its own while
    /// it is signalled.
    fn wait(&self, child: &mut std::process::Child) -> std::io::Result<std::process::ExitStatus> {
        let pgid = self.pgid;
        let mut killed = false;
        let mut leader_exited: Option<Instant> = None;
        loop {
            {
                let mut life = self.life.lock();
                if leader_exited.is_none() && crate::user_exec::exited(pgid) {
                    leader_exited = Some(Instant::now());
                    if life.stopping.is_none() {
                        unsafe { libc::kill(-pgid, libc::SIGTERM) };
                    }
                }
                let since = match (life.stopping, leader_exited) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                if !killed && since.is_some_and(|at| at.elapsed() >= STOP_GRACE) {
                    unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    killed = true;
                }
                // Done when nothing but the exited program is left in its group.
                let group_alive = unsafe { libc::kill(-pgid, 0) } == 0;
                if leader_exited.is_some() && (killed || !group_alive) {
                    let status = child.wait()?;
                    life.reaped = true;
                    return Ok(status);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn reaped(&self) -> bool {
        self.life.lock().reaped
    }
}

/// The daemon as the Lua runtime sees it.
struct Host(Weak<Daemon>);

impl thurm_lua::Host for Host {
    fn call(&self, instance: u64, method: &str, params: Value) -> Result<Value, String> {
        let daemon = self.0.upgrade().ok_or("the daemon is stopping")?;
        let client = daemon
            .plugins
            .instances
            .lock()
            .get(&instance)
            .map(|i| i.client.clone())
            .ok_or("the plugin was stopped")?;
        daemon.ext_call(&client, method, params)
    }

    fn report(&self, instance: u64, error: &str) {
        if let Some(daemon) = self.0.upgrade() {
            daemon.plugin_failed(instance, error);
        }
    }
}

/// Where the notices of a plugin's client go.
type Sink = Box<dyn Fn(u64, &str, Value) + Send>;

fn init_plugin() -> Option<FoundPlugin> {
    let main = thurm_config::config_dir().join("init.lua");
    main.is_file().then(|| FoundPlugin {
        name: INIT.into(),
        version: String::new(),
        description: "init.lua beside config.toml".into(),
        path: main.clone(),
        dir: thurm_config::config_dir(),
        kind: PluginKind::Lua { main },
    })
}

impl Daemon {
    /// Finds the plugins and starts them.
    pub fn start_plugins(self: &Arc<Self>) {
        let _ = self.lua();
        self.sync_plugins(true);
    }

    /// Stops every plugin (the daemon is going).
    /// Waits for the program plugins to end (SIGKILL included), a little past [`STOP_GRACE`]:
    /// none may outlive the daemon.
    pub fn stop_plugins(self: &Arc<Self>) {
        let processes: Vec<Arc<Process>> = self
            .plugins
            .records
            .lock()
            .values()
            .filter_map(|r| r.process.clone())
            .collect();
        let names: Vec<String> = self.plugins.records.lock().keys().cloned().collect();
        for name in names {
            self.stop_plugin(&name, "stopped");
        }
        let deadline = Instant::now() + STOP_GRACE + Duration::from_millis(500);
        while Instant::now() < deadline && !processes.iter().all(|p| p.reaped()) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn lua(self: &Arc<Self>) -> Result<&thurm_lua::Runtime, String> {
        self.plugins
            .lua
            .get_or_init(|| {
                thurm_lua::Runtime::start(Arc::new(Host(Arc::downgrade(self))))
                    .map_err(|e| log::error!("cannot start the Lua runtime: {e}"))
                    .ok()
            })
            .as_ref()
            .ok_or_else(|| "the Lua runtime did not start".to_owned())
    }

    /// Looks for plugins again and brings the running ones in line with the config: new ones
    /// start, removed or disabled ones stop, and those whose settings changed start again, as
    /// do all Lua plugins (they are cheap to start, and pick up edits). With `restart_all`
    /// every plugin starts again.
    pub(super) fn sync_plugins(self: &Arc<Self>, restart_all: bool) {
        self.sync_plugins_in(restart_all, None);
    }

    /// [`Daemon::sync_plugins`] for plugin `only` alone when given: the others are left as
    /// they are.
    fn sync_plugins_in(self: &Arc<Self>, restart_all: bool, only: Option<&str>) {
        let cfg = self.config.read().plugins.clone();
        let (found, warnings) = thurm_config::plugins::discover(&thurm_config::plugins_dir());
        for w in &warnings {
            log::warn!("plugins: {w}");
        }
        let found: Vec<FoundPlugin> = init_plugin()
            .into_iter()
            .chain(found)
            .filter(|f| only.is_none_or(|n| f.name == n))
            .collect();
        let settings = |name: &str| {
            cfg.settings
                .get(name)
                .and_then(|v| serde_json::to_value(v).ok())
                .unwrap_or(Value::Null)
        };
        let gone: Vec<String> = {
            let records = self.plugins.records.lock();
            records
                .keys()
                .filter(|n| only.is_none_or(|o| o == n.as_str()))
                .filter(|n| !found.iter().any(|f| &f.name == *n))
                .cloned()
                .collect()
        };
        for name in gone {
            self.stop_plugin(&name, "stopped");
            self.plugins.records.lock().remove(&name);
        }
        for plugin in found {
            let name = plugin.name.clone();
            let settings = settings(&name);
            let disabled = cfg.is_disabled(&name);
            let restart = {
                let mut records = self.plugins.records.lock();
                let record = records.entry(name.clone()).or_insert_with(|| Record {
                    found: plugin.clone(),
                    settings: settings.clone(),
                    state: "stopped",
                    error: None,
                    instance: None,
                    process: None,
                    log: VecDeque::new(),
                    generation: 0,
                    failures: 0,
                });
                let running = record.state == "running" || record.state == "failed";
                let changed = record.found != plugin || record.settings != settings;
                let lua = matches!(plugin.kind, PluginKind::Lua { .. });
                record.found = plugin;
                record.settings = settings;
                if disabled {
                    None
                } else {
                    Some(!running || restart_all || changed || lua)
                }
            };
            match restart {
                None => self.stop_plugin(&name, "disabled"),
                Some(true) => {
                    self.stop_plugin(&name, "stopped");
                    self.start_plugin(&name);
                }
                Some(false) => {}
            }
        }
        self.plugins_changed();
    }

    /// Starts plugin `name` (stopped before).
    fn start_plugin(self: &Arc<Self>, name: &str) {
        self.start_plugin_if(name, None);
    }

    /// Starts plugin `name`; with `expected`, only if nothing stopped or started it since that
    /// generation and it is not disabled, checked as it starts.
    fn start_plugin_if(self: &Arc<Self>, name: &str, expected: Option<u64>) {
        let disabled = self.config.read().plugins.is_disabled(name);
        let (found, settings, generation) = {
            let mut records = self.plugins.records.lock();
            let Some(r) = records.get_mut(name) else {
                return;
            };
            if expected.is_some_and(|g| g != r.generation || disabled) {
                return;
            }
            // Unique across records: one made again for the same name never matches an older
            // start's.
            r.generation = self.plugins.next.fetch_add(1, Ordering::Relaxed) + 1;
            r.error = None;
            r.state = "running";
            (r.found.clone(), r.settings.clone(), r.generation)
        };
        let started = match &found.kind {
            PluginKind::Lua { main } => self.start_lua(&found, main, settings, generation),
            PluginKind::Process { exec } => self.start_process(&found, exec, settings, generation),
        };
        if let Err(e) = started {
            // Only while this start is still the plugin's: a stop or another start since
            // keeps the state it set.
            let current = match self.plugins.records.lock().get_mut(name) {
                Some(r) if r.generation == generation => {
                    r.state = "failed";
                    true
                }
                _ => false,
            };
            if current {
                self.plugin_error(name, &e);
            } else {
                log::warn!("plugin {name}: a start stopped since failed: {e}");
            }
        }
    }

    fn start_lua(
        self: &Arc<Self>,
        found: &FoundPlugin,
        main: &std::path::Path,
        settings: Value,
        generation: u64,
    ) -> Result<(), String> {
        let source = std::fs::read_to_string(main)
            .map_err(|e| format!("cannot read {}: {e}", main.display()))?;
        let lua = self.lua()?;
        let (instance, _) =
            self.plugin_client(&found.name, Some(found.name.clone()), None, self.lua_sink());
        let done = lua.load(thurm_lua::Spec {
            instance,
            name: found.name.clone(),
            source,
            file: main.display().to_string(),
            dir: Some(found.dir.clone()),
            modules: Default::default(),
            settings,
            args: Vec::new(),
            pane: None,
            log: None,
            script: false,
        });
        // Registered once the load is queued, if this start still holds: a stop after this
        // unloads it (the runtime takes the unload after the load), one before it is caught here.
        if !self.claim_start(&found.name, generation, instance, None) {
            self.end_instance(instance);
            return Ok(());
        }
        let daemon = Arc::downgrade(self);
        let name = found.name.clone();
        std::thread::spawn(move || {
            if let Ok(Err(e)) = done.recv()
                && let Some(d) = daemon.upgrade()
            {
                if let Some(r) = d.plugins.records.lock().get_mut(&name)
                    && r.instance == Some(instance)
                {
                    r.state = "failed";
                }
                d.plugin_failed(instance, &e);
            }
        });
        Ok(())
    }

    /// Notices for a Lua instance go to the runtime.
    fn lua_sink(self: &Arc<Self>) -> Sink {
        let daemon = Arc::downgrade(self);
        Box::new(move |instance, name, payload| {
            if let Some(d) = daemon.upgrade()
                && let Some(Some(lua)) = d.plugins.lua.get()
            {
                lua.notice(instance, name, payload);
            }
        })
    }

    fn start_process(
        self: &Arc<Self>,
        found: &FoundPlugin,
        exec: &[String],
        settings: Value,
        generation: u64,
    ) -> Result<(), String> {
        let name = found.name.clone();
        let mut env = self.user_exec_env(None, None);
        env.push(("THURM_PLUGIN".into(), name.clone()));
        env.push(("THURM_PLUGIN_DIR".into(), found.dir.display().to_string()));
        env.push(("THURM_PLUGIN_SETTINGS".into(), settings.to_string()));
        let dir = found.dir.display().to_string();
        let mut child = user_exec::spawn_piped(user_exec::Launch {
            argv: exec,
            cwd: Some(&dir),
            env,
            stdin: None,
        })
        .map_err(|e| format!("cannot start {:?}: {e}", exec[0]))?;
        let process = Process::new(child.id() as libc::pid_t, child.stdin.take());
        let to = process.clone();
        let (instance, client) = self.plugin_client(
            &name,
            Some(name.clone()),
            None,
            Box::new(move |_, notice_name, payload| {
                let msg = if notice_name == notice::ACTION_INVOKE {
                    json!({
                        "jsonrpc": "2.0",
                        "id": payload["id"],
                        "method": notice_name,
                        "params": {
                            "name": payload["name"],
                            "args": payload["args"],
                            "ctx": payload["ctx"],
                        },
                    })
                } else {
                    json!({ "jsonrpc": "2.0", "method": notice_name, "params": payload })
                };
                to.send(&msg);
            }),
        );
        // Stopped while it was starting: it goes at once. Nothing reads what it writes yet,
        // so it had no say; a stop from here on finds its process in the record.
        if !self.claim_start(&name, generation, instance, Some(process.clone())) {
            self.end_instance(instance);
            process.stop();
        }
        log::info!("plugin {name} started (pid {})", process.pgid);

        // Its stderr goes to its log while the record is still this start's; the exit is
        // logged after what it wrote last.
        let (stderr_done, stderr_ended) = crossbeam_channel::bounded::<()>(1);
        if let Some(stderr) = child.stderr.take() {
            let daemon = Arc::downgrade(self);
            let name = name.clone();
            let _ = std::thread::Builder::new()
                .name("plugin-stderr".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        let Some(d) = daemon.upgrade() else { return };
                        if let Some(r) = d.plugins.records.lock().get_mut(&name)
                            && r.generation == generation
                        {
                            r.log(line);
                        }
                    }
                    drop(stderr_done);
                });
        }
        if let Some(stdout) = child.stdout.take() {
            let daemon = Arc::downgrade(self);
            let process = process.clone();
            let _ = std::thread::Builder::new()
                .name("plugin-rpc".into())
                .spawn(move || {
                    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                        let Some(d) = daemon.upgrade() else { return };
                        d.plugin_message(instance, &client, &process, &line);
                    }
                });
        }
        let daemon = Arc::downgrade(self);
        let started = Instant::now();
        let _ = std::thread::Builder::new()
            .name("plugin-wait".into())
            .spawn(move || {
                let status = process.wait(&mut child);
                // A child of its own may keep stderr open: not for long.
                let _ = stderr_ended.recv_timeout(Duration::from_secs(1));
                if let Some(d) = daemon.upgrade() {
                    d.process_exited(&name, instance, generation, started, status);
                }
            });
        Ok(())
    }

    /// One line a program plugin wrote: a request (or notification) to answer, or its answer
    /// to an `action.invoke`.
    fn plugin_message(
        self: &Arc<Self>,
        instance: u64,
        client: &Arc<Client>,
        process: &Arc<Process>,
        line: &str,
    ) {
        // A plugin stopped has no say any more, though its process may still write.
        if line.trim().is_empty() || !self.instance_live(instance) {
            return;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                process.send(&json!({ "jsonrpc": "2.0", "id": null,
                    "error": { "code": -32700, "message": format!("not JSON: {e}") } }));
                return;
            }
        };
        let Some(method) = msg.get("method").and_then(Value::as_str).map(str::to_owned) else {
            // An answer to the daemon's question.
            if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                let error = msg.get("error").map(|e| {
                    e.get("message")
                        .and_then(Value::as_str)
                        .map_or_else(|| e.to_string(), str::to_owned)
                });
                let result = msg.get("result").cloned().unwrap_or(Value::Null);
                self.ext_reply(client.id, ext::Reply { id, result, error });
            }
            return;
        };
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let blocks = ext::method_blocks(&method);
        let answer = {
            let daemon = self.clone();
            let client = client.clone();
            let process = process.clone();
            move || {
                let result = if daemon.instance_live(instance) {
                    daemon.ext_call(&client, &method, params)
                } else {
                    Err("the plugin was stopped".into())
                };
                if let Some(id) = id {
                    let reply = match result {
                        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
                        Err(e) => json!({ "jsonrpc": "2.0", "id": id,
                            "error": { "code": -32000, "message": e } }),
                    };
                    process.send(&reply);
                }
            }
        };
        if blocks {
            std::thread::spawn(answer);
        } else {
            answer();
        }
    }

    /// A program plugin's process ended: unless it was stopped, it starts again, at once if
    /// it ran a while, else after a pause that doubles with each quick exit.
    fn process_exited(
        self: &Arc<Self>,
        name: &str,
        instance: u64,
        generation: u64,
        started: Instant,
        status: std::io::Result<std::process::ExitStatus>,
    ) {
        self.end_instance(instance);
        let how = match status {
            Ok(s) => match (s.code(), std::os::unix::process::ExitStatusExt::signal(&s)) {
                (Some(c), _) => format!("exited with status {c}"),
                (None, Some(sig)) => format!("killed by signal {sig}"),
                _ => "exited".into(),
            },
            Err(e) => e.to_string(),
        };
        let delay = {
            let mut records = self.plugins.records.lock();
            let Some(r) = records.get_mut(name) else {
                return;
            };
            if r.generation != generation {
                return;
            }
            r.process = None;
            r.instance = None;
            r.failures = if started.elapsed() >= HEALTHY_RUN {
                0
            } else {
                r.failures + 1
            };
            let delay = match r.failures {
                0 => Duration::ZERO,
                n => Duration::from_secs(1 << (n - 1).min(6)).min(MAX_BACKOFF),
            };
            let error = format!("{how}; starting again in {}s", delay.as_secs());
            r.log(format!("[thurm] {error}"));
            r.state = "failed";
            r.error = Some(error);
            delay
        };
        log::warn!("plugin {name} {how}");
        self.plugins_changed();
        let daemon = Arc::downgrade(self);
        let name = name.to_owned();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let Some(d) = daemon.upgrade() else { return };
            let current = d
                .plugins
                .records
                .lock()
                .get(&name)
                .is_some_and(|r| r.generation == generation);
            if current && !d.shutdown.load(Ordering::Relaxed) {
                d.start_plugin_if(&name, Some(generation));
                d.plugins_changed();
            }
        });
    }

    /// Stops plugin `name`, leaving it in `state` (`stopped`, `disabled`).
    fn stop_plugin(self: &Arc<Self>, name: &str, state: &'static str) {
        let (instance, process) = {
            let mut records = self.plugins.records.lock();
            let Some(r) = records.get_mut(name) else {
                return;
            };
            // Unique across records: one made again for the same name never matches an older
            // start's.
            r.generation = self.plugins.next.fetch_add(1, Ordering::Relaxed) + 1;
            r.state = state;
            r.error = None;
            r.failures = 0;
            (r.instance.take(), r.process.take())
        };
        if let Some(i) = instance {
            self.end_instance(i);
        }
        if let Some(p) = process {
            // Its standard input closes too: a plugin may take that as its cue.
            p.stop();
        }
    }

    /// Records `instance` (and `process`) as plugin `name`'s if the start `generation` still
    /// holds, under the records lock a stop takes: false when it was stopped meanwhile.
    fn claim_start(
        &self,
        name: &str,
        generation: u64,
        instance: u64,
        process: Option<Arc<Process>>,
    ) -> bool {
        let mut records = self.plugins.records.lock();
        match records.get_mut(name) {
            Some(r) if r.generation == generation => {
                r.instance = Some(instance);
                r.process = process;
                true
            }
            _ => false,
        }
    }

    /// The instance still runs: it was not stopped.
    fn instance_live(&self, instance: u64) -> bool {
        self.plugins.instances.lock().contains_key(&instance)
    }

    /// A new client for a plugin or script, and its instance id. What the client hears goes to
    /// `sink`.
    fn plugin_client(
        self: &Arc<Self>,
        name: &str,
        plugin: Option<String>,
        owner: Option<u64>,
        sink: Sink,
    ) -> (u64, Arc<Client>) {
        let (client, rx) = self.add_client();
        client.ext.store(true, Ordering::Relaxed);
        *client.plugin.lock() = Some(name.to_owned());
        *client.name.lock() = format!("plugin:{name}");
        let instance = self.plugins.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.plugins.instances.lock().insert(
            instance,
            Instance {
                client: client.clone(),
                plugin,
                owner,
            },
        );
        // Weak: the client holds the sender of `rx`, which must go with the client for the
        // loop to end.
        let weak = Arc::downgrade(&client);
        let _ = std::thread::Builder::new()
            .name("plugin-events".into())
            .spawn(move || {
                for msg in rx {
                    let Some(counted) = weak.upgrade() else {
                        return;
                    };
                    counted.queued.fetch_sub(
                        super::message_weight(&msg).min(counted.queued.load(Ordering::Relaxed)),
                        Ordering::Relaxed,
                    );
                    counted.unflushed.fetch_sub(1, Ordering::AcqRel);
                    if let ServerMessage::Event(Event::Ext { name, payload }) = msg {
                        let payload = serde_json::from_str(&payload).unwrap_or(Value::Null);
                        sink(instance, &name, payload);
                    }
                }
            });
        (instance, client)
    }

    /// Ends an instance: its Lua code, and its client with what it provided.
    fn end_instance(self: &Arc<Self>, instance: u64) {
        if let Some(Some(lua)) = self.plugins.lua.get() {
            lua.unload(instance);
        }
        let gone = self.plugins.instances.lock().remove(&instance);
        if let Some(i) = gone {
            self.remove_client(i.client.id);
        }
    }

    /// Stops the scripts client `owner` runs: it left.
    pub(super) fn plugins_owner_gone(self: &Arc<Self>, owner: u64) {
        let owned: Vec<u64> = self
            .plugins
            .instances
            .lock()
            .iter()
            .filter(|(_, i)| i.owner == Some(owner))
            .map(|(id, _)| *id)
            .collect();
        for id in owned {
            self.end_instance(id);
        }
    }

    /// A line for a plugin's log, from its client.
    pub(super) fn plugin_log(&self, client: &Client, level: &str, message: &str) {
        let Some(name) = client.plugin.lock().clone() else {
            return;
        };
        if let Some(r) = self.plugins.records.lock().get_mut(&name) {
            r.log(if level == "info" {
                message.to_owned()
            } else {
                format!("[{level}] {message}")
            });
        }
    }

    /// Code of a plugin failed and nothing caught it.
    fn plugin_failed(&self, instance: u64, error: &str) {
        let plugin = self
            .plugins
            .instances
            .lock()
            .get(&instance)
            .and_then(|i| i.plugin.clone());
        match plugin {
            Some(name) => self.plugin_error(&name, error),
            None => log::warn!("script: {error}"),
        }
    }

    /// Plugin `name` hit `error`: it goes in its log, and to the user.
    fn plugin_error(&self, name: &str, error: &str) {
        log::warn!("plugin {name}: {error}");
        if let Some(r) = self.plugins.records.lock().get_mut(name) {
            r.log(format!("[error] {error}"));
            r.error = Some(error.to_owned());
        }
        self.toast(format!("{name}: {error}"), None);
        self.plugins_changed();
    }

    fn plugins_changed(&self) {
        let clients: Vec<_> = self.clients.lock().values().cloned().collect();
        for c in clients {
            if c.ext.load(Ordering::Relaxed) && c.plugin.lock().is_none() {
                send_ext(&c, notice::PLUGINS_CHANGED, &json!({}));
            }
        }
    }

    fn plugin_list(&self) -> Vec<ext::PluginInfo> {
        let actions = self.ext_actions();
        self.plugins
            .records
            .lock()
            .values()
            .map(|r| {
                let source = format!("plugin:{}", r.found.name);
                ext::PluginInfo {
                    name: r.found.name.clone(),
                    version: r.found.version.clone(),
                    description: r.found.description.clone(),
                    kind: match r.found.kind {
                        PluginKind::Lua { .. } => "lua".into(),
                        PluginKind::Process { .. } => "process".into(),
                    },
                    path: r.found.path.display().to_string(),
                    state: r.state.into(),
                    error: r.error.clone(),
                    actions: actions
                        .iter()
                        .filter(|a| a.source == source && !a.hidden)
                        .map(|a| a.name.clone())
                        .collect(),
                }
            })
            .collect()
    }

    /// Writes `plugins.disabled` with `name` in it (`disable`) or not, and applies the config.
    fn set_disabled(self: &Arc<Self>, name: &str, disable: bool) -> Result<(), String> {
        if !ext::valid_plugin_name(name) {
            return Err(format!("{name:?} is not a plugin name"));
        }
        let mut disabled = self.config.read().plugins.disabled.clone();
        disabled.retain(|n| n != name);
        if disable {
            disabled.push(name.to_owned());
        }
        let literal = format!(
            "[{}]",
            disabled
                .iter()
                .map(|n| format!("{n:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        thurm_config::write_setting("plugins.disabled", &literal)?;
        let cfg = thurm_config::Config::load().map_err(|e| e.to_string())?;
        self.apply_config(cfg);
        self.sync_plugins_in(false, Some(name));
        Ok(())
    }

    /// The extension API's methods about plugins and scripts.
    pub(super) fn plugin_method(
        self: &Arc<Self>,
        client: &Arc<Client>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let name = || {
            params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "missing \"name\"".to_owned())
        };
        match method {
            method::SCRIPT_RUN => {
                let run: ext::RunScript = if params.is_null() {
                    Default::default()
                } else {
                    serde_json::from_value(params.clone())
                        .map_err(|e| format!("bad parameters: {e}"))?
                };
                self.run_script(client, run)
            }
            method::PLUGINS_LIST => {
                serde_json::to_value(self.plugin_list()).map_err(|e| e.to_string())
            }
            method::PLUGINS_RELOAD => {
                match params.get("name").and_then(Value::as_str) {
                    None => self.sync_plugins(true),
                    Some(name) => {
                        // Read again from its files (which may have changed, or gone) and
                        // started again; the other plugins are left alone.
                        self.sync_plugins_in(true, Some(name));
                        if !self.plugins.records.lock().contains_key(name) {
                            return Err(format!("no plugin named {name:?}"));
                        }
                        if self.config.read().plugins.is_disabled(name) {
                            return Err(format!("{name} is disabled"));
                        }
                    }
                }
                Ok(Value::Null)
            }
            method::PLUGINS_ENABLE => self.set_disabled(&name()?, false).map(|()| Value::Null),
            method::PLUGINS_DISABLE => self.set_disabled(&name()?, true).map(|()| Value::Null),
            method::PLUGINS_LOGS => {
                let name = name()?;
                let lines = params.get("lines").and_then(Value::as_u64).unwrap_or(100) as usize;
                let records = self.plugins.records.lock();
                let r = records
                    .get(&name)
                    .ok_or_else(|| format!("no plugin named {name:?}"))?;
                let skip = r.log.len().saturating_sub(lines);
                Ok(json!({ "lines": r.log.iter().skip(skip).collect::<Vec<_>>() }))
            }
            other => Err(format!("unknown method {other:?}")),
        }
    }

    /// Runs a script to its end; its log lines go to `client` as they come.
    fn run_script(
        self: &Arc<Self>,
        client: &Arc<Client>,
        run: ext::RunScript,
    ) -> Result<Value, String> {
        let lua = self.lua()?;
        let (instance, _) = self.plugin_client("script", None, Some(client.id), self.lua_sink());
        let to = client.clone();
        let log: thurm_lua::LogSink = Arc::new(move |level: &str, message: &str| {
            send_ext(
                &to,
                notice::SCRIPT_LOG,
                &json!({ "run": instance, "level": level, "message": message }),
            );
        });
        let file = if run.name.is_empty() {
            "script.lua".to_owned()
        } else {
            run.name
        };
        let done = lua.load(thurm_lua::Spec {
            instance,
            name: "script".into(),
            source: run.source,
            file,
            dir: run.dir.map(PathBuf::from),
            modules: run.modules,
            settings: Value::Null,
            args: run.args,
            pane: run.pane,
            log: Some(log),
            script: true,
        });
        // Its owner may have left before the instance was there to be stopped with it. Checked
        // once the load is queued: an owner leaving after this stops it with an unload, which
        // the runtime takes after the load.
        if !self.clients.lock().contains_key(&client.id) {
            self.end_instance(instance);
            return Err("the client running the script went away".into());
        }
        let result = done
            .recv()
            .unwrap_or_else(|_| Err("the Lua runtime stopped".into()));
        self.end_instance(instance);
        result
    }
}
