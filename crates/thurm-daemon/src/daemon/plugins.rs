//! The plugins the daemon runs. Each running plugin (and each script `thurm do` runs) has a
//! client of its own, through which it uses the extension API like any other client; the
//! notices that client gets go to the plugin.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;
use serde_json::{Value, json};
use thurm_proto::ext::{self, method, notice};
use thurm_proto::{Event, ServerMessage};

use super::ext::send_ext;
use super::{Client, Daemon};

/// The name of the plugin `init.lua` in the config directory is.
pub const INIT: &str = "init";

#[derive(Default)]
pub struct Plugins {
    lua: OnceLock<Option<thurm_lua::Runtime>>,
    instances: Mutex<HashMap<u64, Instance>>,
    next: AtomicU64,
    /// The running `init.lua`.
    init: Mutex<Option<u64>>,
    /// The last init.lua that started without an error: what a reload falls back on.
    init_good: Mutex<Option<String>>,
    /// Scripts running now.
    scripts: std::sync::atomic::AtomicUsize,
}

/// The most scripts that may run at once (each has a Lua state of its own).
const MAX_SCRIPTS: usize = 32;

struct Instance {
    client: Arc<Client>,
    name: String,
    /// For a script: the client that runs it, whose leaving stops it.
    owner: Option<u64>,
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

impl Daemon {
    /// Starts the Lua runtime and `init.lua`.
    pub fn start_plugins(self: &Arc<Self>) {
        let _ = self.lua();
        self.load_init();
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

    /// A new client for plugin `name`, and its instance id. What the client hears goes to the
    /// plugin.
    fn plugin_client(self: &Arc<Self>, name: &str, owner: Option<u64>) -> (u64, Arc<Client>) {
        let (client, rx) = self.add_client();
        client.ext.store(true, Ordering::Relaxed);
        *client.plugin.lock() = Some(name.to_owned());
        *client.name.lock() = format!("plugin:{name}");
        let instance = self.plugins.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.plugins.instances.lock().insert(
            instance,
            Instance {
                client: client.clone(),
                name: name.to_owned(),
                owner,
            },
        );
        let daemon = Arc::downgrade(self);
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
                    let ServerMessage::Event(Event::Ext { name, payload }) = msg else {
                        continue;
                    };
                    let Some(daemon) = daemon.upgrade() else {
                        return;
                    };
                    if let Some(Some(lua)) = daemon.plugins.lua.get() {
                        let payload = serde_json::from_str(&payload).unwrap_or(Value::Null);
                        lua.notice(instance, &name, payload);
                    }
                }
            });
        (instance, client)
    }

    /// Stops a plugin instance: its code, and its client with what it provided.
    fn stop_instance(self: &Arc<Self>, instance: u64) {
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
            self.stop_instance(id);
        }
    }

    /// Code of a plugin failed and nothing caught it.
    fn plugin_failed(&self, instance: u64, error: &str) {
        let name = self
            .plugins
            .instances
            .lock()
            .get(&instance)
            .map(|i| i.name.clone())
            .unwrap_or_default();
        log::warn!("plugin {name}: {error}");
        self.toast(format!("{name}: {error}"), None);
    }

    /// (Re)starts `init.lua` from the config directory, if there is one.
    pub(super) fn load_init(self: &Arc<Self>) {
        let file = thurm_config::config_dir().join("init.lua");
        // Read and checked before the running one stops: a file that can't be read or doesn't
        // compile leaves what works running, and says why. One that is gone stops it.
        let source = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(old) = self.plugins.init.lock().take() {
                    self.stop_instance(old);
                }
                return;
            }
            Err(e) => {
                log::warn!("{}: {e}", file.display());
                self.toast(format!("init.lua: {e}; the running one stays"), None);
                return;
            }
        };
        if let Err(e) = thurm_lua::check(&source, &file.display().to_string()) {
            log::warn!("init.lua: {e}");
            self.toast(format!("init.lua: {e}; the running one stays"), None);
            return;
        }
        if let Some(old) = self.plugins.init.lock().take() {
            self.stop_instance(old);
        }
        let fallback = self.plugins.init_good.lock().clone();
        self.start_init(source, file, fallback);
    }

    /// Runs `source` as init.lua. When its main chunk fails and `fallback` (the last one that
    /// worked) is given, that one runs again instead: an error in the new file costs nothing
    /// that worked.
    fn start_init(self: &Arc<Self>, source: String, file: PathBuf, fallback: Option<String>) {
        let Ok(lua) = self.lua() else { return };
        let (instance, _) = self.plugin_client(INIT, None);
        *self.plugins.init.lock() = Some(instance);
        let done = lua.load(thurm_lua::Spec {
            instance,
            name: INIT.into(),
            source: source.clone(),
            file: file.display().to_string(),
            dir: file.parent().map(PathBuf::from),
            modules: Default::default(),
            settings: Value::Null,
            args: Vec::new(),
            pane: None,
            log: None,
            script: false,
        });
        let daemon = Arc::downgrade(self);
        std::thread::spawn(move || {
            let Some(d) = daemon.upgrade() else { return };
            match done.recv() {
                Ok(Ok(_)) => *d.plugins.init_good.lock() = Some(source),
                Ok(Err(e)) => {
                    let current = *d.plugins.init.lock() == Some(instance);
                    match fallback.filter(|f| current && *f != source) {
                        Some(previous) => {
                            d.plugin_failed(instance, &format!("{e}; the previous init.lua runs"));
                            if let Some(i) = d.plugins.init.lock().take() {
                                d.stop_instance(i);
                            }
                            d.start_init(previous, file, None);
                        }
                        None => d.plugin_failed(instance, &e),
                    }
                }
                Err(_) => {}
            }
        });
    }

    /// The extension API's methods about plugins and scripts.
    pub(super) fn plugin_method(
        self: &Arc<Self>,
        client: &Arc<Client>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        match method {
            method::SCRIPT_RUN => {
                let run: ext::RunScript = if params.is_null() {
                    Default::default()
                } else {
                    serde_json::from_value(params).map_err(|e| format!("bad parameters: {e}"))?
                };
                self.run_script(client, run)
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
        // Each script has a Lua state of its own: only so many at once.
        if self.plugins.scripts.fetch_add(1, Ordering::SeqCst) >= MAX_SCRIPTS {
            self.plugins.scripts.fetch_sub(1, Ordering::SeqCst);
            return Err(format!("{MAX_SCRIPTS} scripts are running already"));
        }
        struct Running<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _running = Running(&self.plugins.scripts);
        let (instance, _) = self.plugin_client("script", Some(client.id));
        let to = client.clone();
        let log: thurm_lua::LogSink = Arc::new(move |level: &str, message: &str| {
            send_ext(
                &to,
                notice::SCRIPT_LOG,
                &json!({ "run": instance, "level": level, "message": message }),
            );
        });
        let name = if run.name.is_empty() {
            "script.lua".to_owned()
        } else {
            run.name
        };
        let done = lua.load(thurm_lua::Spec {
            instance,
            name: "script".into(),
            source: run.source,
            file: name,
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
            self.stop_instance(instance);
            return Err("the client running the script went away".into());
        }
        let result = done
            .recv()
            .unwrap_or_else(|_| Err("the Lua runtime stopped".into()));
        self.stop_instance(instance);
        result
    }
}
