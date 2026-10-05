//! Daemon state: panes, clients, frame pump, monitor loop and request handling.

#[path = "terminal_attach.rs"]
mod terminal_attach;
mod agent_prompt;
mod layouts;

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::{Mutex, RwLock};

use thurm_config::{AgentDef, Config};
use thurm_proto::{
    AgentState, AgentStatus, CreatePane, Event, Layout, PROTOCOL_VERSION, PaneId, PaneInfo,
    PaneSize, Request, Response, ServerMessage, WaitCondition, WaitOutcome,
};
use thurm_term::{EngineConfig, MouseOutcome, TermEvent, Terminal};

use crate::agents::{self, AgentTracker, AiWant};
use crate::ai;
use crate::persist::{self, PaneSnapshot, SessionSnapshot, Store};
use crate::procinfo;
use crate::pty::{self, Pty};
use crate::shell;
use crate::upgrade;

/// Messages queued for a client before its subscriptions switch to re-sync (it stops getting
/// output and gets a fresh `Attach` once it caught up).
const OUTPUT_BACKLOG: usize = 4096;
/// Bytes of output queued for a client before its subscriptions switch to re-sync (an
/// `Output` can be 256 KiB: counting messages alone let a stalled client pin a gigabyte).
const OUTPUT_BACKLOG_BYTES: usize = 16 * 1024 * 1024;

/// PTY output read ahead of the parser. A macOS PTY buffers only a few KiB, so a program that
/// writes faster than we parse would stall on every chunk; with the read-ahead it keeps
/// writing while the parser catches up, and still blocks once this much is pending.
const READ_AHEAD: usize = 8 * 1024 * 1024;
/// Read-ahead buffer capacity kept between bursts.
const KEEP_BUFFER: usize = 256 * 1024;
/// Most output parsed (and forwarded) under one pane lock.
const PARSE_SLICE: usize = 256 * 1024;
/// A read this long after the previous one is interactive output (an echo), which the reader
/// parses itself instead of handing it to the parser thread; bulk output reads back to back.
const QUIET: Duration = Duration::from_millis(2);

/// Screen lines the model reads for titles, requests and turn summaries.
const AI_SCREEN_LINES: usize = 60;

/// How long before the daemon notices a SIGTERM a pane whose shell died still counts for the
/// session save. Quitting the app signals the daemon and the shells at once, and the reader
/// can see the shell go before the signal handler has run (and the main thread notices the
/// signal up to 100ms after that).
const TEARDOWN_GRACE: Duration = Duration::from_millis(500);
/// Autosave serializes a busy pane's scrollback (under its lock) at most this often.
/// Explicit saves (quit, upgrade, `thurm save`) always do.
const HISTORY_AUTOSAVE_INTERVAL: Duration = Duration::from_secs(10);

/// Work for the on-device model (`[ai]`), done one at a time by `ai_worker`.
enum AiJob {
    Title {
        pane: PaneId,
        agent: String,
        prompt: Option<String>,
        screen: String,
    },
    Status {
        pane: PaneId,
        hash: u64,
        agent: String,
        screen: String,
    },
    /// What the agent asks for (or, `done`, what its turn did), for `episode` of its status;
    /// then the notification that waited for it (title, body without the detail).
    Detail {
        pane: PaneId,
        episode: u64,
        done: bool,
        agent: String,
        hint: Option<String>,
        screen: String,
        notify: Option<(String, String)>,
    },
}

/// Most of an unterminated escape sequence (or UTF-8 character) kept to replay after an
/// in-place upgrade (see `PaneState::carry`).
const MAX_CARRY: usize = 256 * 1024;

/// Stops every pane's reader between two reads, for an in-place upgrade: what the old image
/// read is then parsed (and serialized) completely, and the rest stays in the kernel for the
/// new image.
pub struct ReaderGate {
    paused: AtomicBool,
    /// Readable while paused, so readers blocked in `poll` wake up.
    wake_r: std::os::fd::OwnedFd,
    wake_w: std::os::fd::OwnedFd,
    state: Mutex<GateState>,
    changed: parking_lot::Condvar,
}

#[derive(Default)]
struct GateState {
    readers: usize,
    parked: usize,
}

impl ReaderGate {
    fn new() -> std::io::Result<ReaderGate> {
        use std::os::fd::FromRawFd;
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for fd in fds {
            pty::set_cloexec(fd, true)?;
            unsafe {
                libc::fcntl(
                    fd,
                    libc::F_SETFL,
                    libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
                )
            };
        }
        Ok(ReaderGate {
            paused: AtomicBool::new(false),
            wake_r: unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) },
            wake_w: unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) },
            state: Mutex::new(GateState::default()),
            changed: parking_lot::Condvar::new(),
        })
    }

    fn wake_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        self.wake_r.as_raw_fd()
    }

    /// A reader starts (`leave` when it ends).
    fn enter(&self) {
        self.state.lock().readers += 1;
    }

    fn leave(&self) {
        self.state.lock().readers -= 1;
        self.changed.notify_all();
    }

    /// Called by a reader before each read: waits while the gate is closed.
    fn park_if_paused(&self) {
        if !self.paused.load(Ordering::Acquire) {
            return;
        }
        let mut st = self.state.lock();
        st.parked += 1;
        self.changed.notify_all();
        while self.paused.load(Ordering::Acquire) {
            self.changed.wait(&mut st);
        }
        st.parked -= 1;
    }

    /// Closes the gate; true once every reader is parked, false after `timeout`.
    fn pause(&self, timeout: Duration) -> bool {
        use std::os::fd::AsRawFd;
        self.paused.store(true, Ordering::Release);
        unsafe { libc::write(self.wake_w.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        let deadline = Instant::now() + timeout;
        let mut st = self.state.lock();
        while st.parked < st.readers {
            if self.changed.wait_until(&mut st, deadline).timed_out() {
                return st.parked >= st.readers;
            }
        }
        true
    }

    fn resume(&self) {
        use std::os::fd::AsRawFd;
        let _st = self.state.lock();
        self.paused.store(false, Ordering::Release);
        let mut buf = [0u8; 64];
        while unsafe { libc::read(self.wake_r.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0
        {
        }
        self.changed.notify_all();
    }
}

/// Bytes read from a pane's PTY that its parser hasn't taken yet.
#[derive(Default)]
struct ReadAhead {
    state: Mutex<ReadAheadState>,
    /// The parser has data (or EOF) waiting.
    ready: parking_lot::Condvar,
    /// The reader has room again.
    room: parking_lot::Condvar,
}

#[derive(Default)]
struct ReadAheadState {
    data: Vec<u8>,
    eof: bool,
    /// The parser is working through a batch it took.
    parsing: bool,
}

/// A client subscribed to a pane's output.
#[derive(Default)]
pub struct Subscriber {
    /// Fell behind: waiting to be re-attached.
    resync: bool,
}

pub struct Client {
    pub id: u64,
    pub tx: Sender<ServerMessage>,
    pub ui: AtomicBool,
    pub name: Mutex<String>,
    /// Approximate bytes queued in `tx` (see [`message_weight`]); the writer subtracts what it
    /// wrote.
    pub queued: std::sync::atomic::AtomicUsize,
}

impl Client {
    pub fn send(&self, msg: ServerMessage) {
        self.queued
            .fetch_add(message_weight(&msg), Ordering::Relaxed);
        if self.tx.send(msg).is_err() {
            self.queued.store(0, Ordering::Relaxed);
        }
    }

    /// The client is far enough behind that it should get fresh state instead of output.
    fn backlogged(&self) -> bool {
        self.tx.len() > OUTPUT_BACKLOG || self.queued.load(Ordering::Relaxed) > OUTPUT_BACKLOG_BYTES
    }

    /// Caught up enough to be re-attached.
    fn caught_up(&self) -> bool {
        self.tx.len() < OUTPUT_BACKLOG / 2
            && self.queued.load(Ordering::Relaxed) < OUTPUT_BACKLOG_BYTES / 2
    }
}

/// What a queued message costs, for [`Client::queued`]: its output or state bytes, else a
/// little.
pub fn message_weight(msg: &ServerMessage) -> usize {
    match msg {
        ServerMessage::Event(Event::Output { data, .. }) => data.len(),
        ServerMessage::Event(Event::Attach { state, .. }) => state.len(),
        _ => 256,
    }
}

pub struct Pane {
    pub id: PaneId,
    pub state: Mutex<PaneState>,
    input: Sender<Vec<u8>>,
    /// Read from the PTY, not parsed yet.
    pipe: Arc<ReadAhead>,
}

impl Pane {
    pub fn write(&self, data: Vec<u8>) {
        if !data.is_empty() {
            let _ = self.input.send(data);
        }
    }
}

pub struct PaneState {
    pub term: Terminal,
    pub pty: Pty,
    pub info: PaneInfo,
    pub command: Option<Vec<String>>,
    pub hold: bool,
    pub subscribers: HashMap<u64, Subscriber>,
    pub terminal_attachment: Option<(u64, PaneSize)>,
    pub agent: AgentTracker,
    osc_cwd: Option<String>,
    command_started: Option<Instant>,
    command_line: Option<String>,
    last_history: Option<Arc<[u8]>>,
    /// When `last_history` was serialized.
    history_at: Option<Instant>,
    saved_generation: u64,
    /// The unterminated end of what was parsed (an escape sequence cut by a read, or part of a
    /// UTF-8 character): an in-place upgrade replays it into the new terminal ahead of the
    /// rest, which is still in the kernel.
    carry: Vec<u8>,
    pending_input: Option<(Instant, Vec<u8>)>,
    shell_integration_seen: bool,
    /// Last git probe: when, for which directory, and whether one is in flight.
    git_probe: Option<(Instant, String)>,
    git_pending: bool,
}

/// What [`Daemon::install_pane`] needs: a new pane, or one handed over by the previous daemon.
struct InstallPane {
    term: Terminal,
    pty: Pty,
    info: PaneInfo,
    command: Option<Vec<String>>,
    hold: bool,
    osc_cwd: Option<String>,
    shell_integration_seen: bool,
    /// Start of a sequence parsed by the image we replaced (see `PaneState::carry`).
    carry: Vec<u8>,
}

pub struct Daemon {
    pub config: RwLock<Config>,
    engine: RwLock<EngineConfig>,
    /// System appearance reported by the GUI (dark until told otherwise).
    dark: AtomicBool,
    agent_defs: RwLock<Vec<AgentDef>>,
    panes: Mutex<HashMap<PaneId, Arc<Pane>>>,
    clients: Mutex<HashMap<u64, Arc<Client>>>,
    layout: Mutex<Option<String>>,
    next_pane: AtomicU64,
    next_client: AtomicU64,
    next_layout: AtomicU64,
    pending_layouts: Mutex<HashMap<u64, layouts::PendingLayout>>,
    git_tx: crossbeam_channel::Sender<(PaneId, String)>,
    git_rx: crossbeam_channel::Receiver<(PaneId, String)>,
    ai: ai::Model,
    ai_tx: Sender<AiJob>,
    ai_rx: Receiver<AiJob>,
    store: Option<Store>,
    /// One session save at a time: the monitor's autosave, SIGTERM, upgrades and requests.
    save_lock: Mutex<()>,
    readers: ReaderGate,
    pub socket: PathBuf,
    integration_dir: Option<PathBuf>,
    session_dirty: AtomicBool,
    restored: AtomicBool,
    pub shutdown: AtomicBool,
    /// SIGTERM/SIGINT arrived. Shells dying from here on are part of the teardown (quitting
    /// the app that spawned us signals its whole tree at once) and stay in the saved session.
    pub stopping: Arc<AtomicBool>,
    /// Panes whose shell died less than `TEARDOWN_GRACE` ago, for a teardown save that
    /// lands just after (they close for the clients at once).
    just_exited: Mutex<Vec<(Arc<Pane>, Instant)>>,
    /// When a save first saw `stopping`: the end of the teardown window.
    stopped_at: std::sync::OnceLock<Instant>,
    last_activity: Mutex<Instant>,
}

impl Daemon {
    pub fn new(
        config: Config,
        socket: PathBuf,
        state_dir: PathBuf,
        stopping: Arc<AtomicBool>,
    ) -> Arc<Daemon> {
        let integration_dir = shell::install_integration(&state_dir)
            .map_err(|e| log::warn!("shell integration unavailable: {e}"))
            .ok();
        let store = config
            .session
            .persist
            .then(|| Store::new(state_dir.clone()));
        let git_channel = crossbeam_channel::bounded(256);
        let ai_channel = crossbeam_channel::bounded(64);
        Arc::new(Daemon {
            engine: RwLock::new(EngineConfig::from_config(&config, true)),
            dark: AtomicBool::new(true),
            agent_defs: RwLock::new(config.agent_defs()),
            config: RwLock::new(config),
            panes: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            layout: Mutex::new(None),
            next_pane: AtomicU64::new(1),
            next_client: AtomicU64::new(1),
            next_layout: AtomicU64::new(1),
            pending_layouts: Mutex::new(HashMap::new()),
            git_tx: git_channel.0,
            git_rx: git_channel.1,
            ai: ai::Model::new(),
            ai_tx: ai_channel.0,
            ai_rx: ai_channel.1,
            store,
            save_lock: Mutex::new(()),
            readers: ReaderGate::new().expect("reader gate pipe"),
            socket,
            integration_dir,
            session_dirty: AtomicBool::new(false),
            restored: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            stopping,
            just_exited: Mutex::new(Vec::new()),
            stopped_at: std::sync::OnceLock::new(),
            last_activity: Mutex::new(Instant::now()),
        })
    }

    pub fn pane(&self, id: PaneId) -> Option<Arc<Pane>> {
        self.panes.lock().get(&id).cloned()
    }

    fn all_panes(&self) -> Vec<Arc<Pane>> {
        let mut v: Vec<_> = self.panes.lock().values().cloned().collect();
        v.sort_by_key(|p| p.id);
        v
    }

    /// Sends `event` to every client (`ui_only`: the apps). A pane's state and notifications
    /// are sent with the pane locked, so they arrive in the order they happened. Pane lock,
    /// then client lock, never the other way round.
    fn broadcast(&self, event: Event, ui_only: bool) {
        let clients: Vec<_> = self.clients.lock().values().cloned().collect();
        for c in clients {
            if !ui_only || c.ui.load(Ordering::Relaxed) {
                c.send(ServerMessage::Event(event.clone()));
            }
        }
    }

    fn has_ui_clients(&self) -> bool {
        self.clients
            .lock()
            .values()
            .any(|c| c.ui.load(Ordering::Relaxed))
    }

    // -----------------------------------------------------------------------------------------
    // Clients
    // -----------------------------------------------------------------------------------------

    pub fn add_client(&self) -> (Arc<Client>, Receiver<ServerMessage>) {
        let (tx, rx) = unbounded();
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        let client = Arc::new(Client {
            id,
            tx,
            ui: AtomicBool::new(false),
            name: Mutex::new(String::new()),
            queued: std::sync::atomic::AtomicUsize::new(0),
        });
        self.clients.lock().insert(id, client.clone());
        *self.last_activity.lock() = Instant::now();
        (client, rx)
    }

    pub fn remove_client(&self, id: u64) {
        self.clients.lock().remove(&id);
        for p in self.all_panes() {
            let mut st = p.state.lock();
            st.subscribers.remove(&id);
            self.release_terminal(&mut st, id);
        }
        *self.last_activity.lock() = Instant::now();
    }

    // -----------------------------------------------------------------------------------------
    // Panes
    // -----------------------------------------------------------------------------------------

    /// The command forking the agent session in pane `src`, and that pane's directory.
    fn fork_command(&self, src: PaneId) -> anyhow::Result<(Vec<String>, Option<String>)> {
        let pane = self
            .pane(src)
            .ok_or_else(|| anyhow::anyhow!("no such pane: {src}"))?;
        let (agent, cwd) = {
            let st = pane.state.lock();
            (st.info.agent.clone(), st.info.cwd.clone())
        };
        let agent = agent.ok_or_else(|| anyhow::anyhow!("no agent is running in pane {src}"))?;
        let session = agent.session_id.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "{} hasn't reported its session id; install the hooks with `thurm hooks install`",
                agent.name
            )
        })?;
        let template = self
            .agent_defs
            .read()
            .iter()
            .find(|d| d.kind == agent.kind)
            .and_then(|d| d.fork_session.clone())
            .ok_or_else(|| anyhow::anyhow!("{} can't fork sessions", agent.name))?;
        let cmd = template
            .iter()
            .map(|a| a.replace("{session}", &session))
            .collect();
        Ok((cmd, cwd))
    }

    fn resolve_command(&self, req: &CreatePane) -> Option<Vec<String>> {
        if let Some(name) = &req.agent_preset {
            let cfg = self.config.read();
            if let Some(p) = cfg.agent_presets().into_iter().find(|p| &p.name == name) {
                return Some(p.command);
            }
            if let Some(d) = cfg
                .agent_defs()
                .into_iter()
                .find(|d| &d.kind == name || &d.name == name)
            {
                return d.launch;
            }
        }
        req.command.clone()
    }

    /// Create a pane. `restore` carries the id and saved scrollback when restoring a session.
    pub fn create_pane(
        self: &Arc<Self>,
        req: CreatePane,
        restore: Option<(PaneId, Option<Vec<u8>>)>,
    ) -> anyhow::Result<PaneId> {
        let id = match &restore {
            Some((id, _)) => {
                self.next_pane.fetch_max(id + 1, Ordering::Relaxed);
                *id
            }
            None => self.next_pane.fetch_add(1, Ordering::Relaxed),
        };
        let mut req = req;
        if let Some(src) = req.fork_from {
            let (cmd, cwd) = self.fork_command(src)?;
            req.command = Some(cmd);
            req.agent_preset = None;
            if req.cwd.is_none() {
                req.cwd = cwd;
            }
        }
        let command = self.resolve_command(&req);
        let cwd = req.cwd.clone().or_else(|| {
            let from = self.pane(req.inherit_cwd_from?)?;
            let st = from.state.lock();
            st.info.cwd.clone()
        });
        let size = sanitize_size(req.size);
        let cfg = self.config.read().clone();
        if restore.is_none() && cfg.agents.install_hooks {
            ensure_hooks(command.as_deref());
        }
        let token = shell_token()?;
        let opts = shell::spawn_options(shell::PaneLaunch {
            id,
            command: command.clone(),
            cwd: cwd.clone(),
            extra_env: req.env.clone(),
            size,
            config: &cfg,
            socket: &self.socket,
            integration_dir: self.integration_dir.as_deref(),
            token: &token,
        });
        let mut term = Terminal::new(size, self.engine.read().clone());
        term.set_shell_token(Some(token));
        let restored = restore.is_some();
        if let Some((_, Some(history))) = &restore {
            term.replay(history);
            term.print(&format!(
                "\r\n\x1b[2m── session restored · {} ──\x1b[0m\r\n",
                cwd.as_deref().unwrap_or("~")
            ));
        }
        let pty = Pty::spawn(opts)?;
        let pid = pty.pid();
        let info = PaneInfo {
            id,
            title: command
                .as_ref()
                .and_then(|c| c.first().cloned())
                .unwrap_or_else(|| "shell".into()),
            cwd: cwd.clone(),
            pid: Some(pid),
            alive: true,
            size,
            restored,
            ..Default::default()
        };
        self.install_pane(
            InstallPane {
                term,
                pty,
                info,
                command,
                hold: req.hold,
                osc_cwd: None,
                shell_integration_seen: false,
                carry: Vec::new(),
            },
            true,
        )?;
        log::info!("pane {id} created (pid {pid})");
        Ok(id)
    }

    /// Registers a pane around its terminal and PTY and starts its threads (the reader only
    /// when `read` is set: an exited pane has nothing left to read).
    fn install_pane(self: &Arc<Self>, p: InstallPane, read: bool) -> anyhow::Result<()> {
        let id = p.info.id;
        let reader = p.pty.reader()?;
        let writer = p.pty.writer()?;
        let (input_tx, input_rx) = unbounded::<Vec<u8>>();
        let pane = Arc::new(Pane {
            id,
            input: input_tx,
            pipe: Arc::new(ReadAhead::default()),
            state: Mutex::new(PaneState {
                term: p.term,
                pty: p.pty,
                info: p.info,
                command: p.command,
                hold: p.hold,
                subscribers: HashMap::new(),
                terminal_attachment: None,
                agent: AgentTracker::default(),
                osc_cwd: p.osc_cwd,
                command_started: None,
                command_line: None,
                last_history: None,
                history_at: None,
                saved_generation: 0,
                carry: p.carry,
                pending_input: None,
                shell_integration_seen: p.shell_integration_seen,
                git_probe: None,
                git_pending: false,
            }),
        });
        self.panes.lock().insert(id, pane.clone());
        self.session_dirty.store(true, Ordering::Relaxed);

        spawn_writer(id, writer, input_rx);
        if read {
            let daemon = self.clone();
            std::thread::Builder::new()
                .name(format!("pane-{id}-reader"))
                .spawn(move || daemon.reader_loop(pane, reader))?;
        }
        Ok(())
    }

    /// Reads the PTY as fast as it delivers, into the read-ahead; parsing happens on a
    /// second thread, so the program never waits for the parser.
    fn reader_loop(self: Arc<Self>, pane: Arc<Pane>, mut reader: File) {
        interactive_qos();
        self.readers.enter();
        struct Leave<'a>(&'a ReaderGate);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.0.leave();
            }
        }
        let _leave = Leave(&self.readers);
        let pipe = pane.pipe.clone();
        let parser = {
            let (daemon, pane, pipe) = (self.clone(), pane.clone(), pipe.clone());
            std::thread::Builder::new()
                .name(format!("pane-{}-parser", pane.id))
                .spawn(move || daemon.parser_loop(pane, pipe))
        };
        if let Err(e) = parser {
            log::warn!("pane {}: cannot start parser: {e}", pane.id);
            return self.pane_exited(&pane);
        }
        if let Err(e) = pty::set_nonblocking(&reader) {
            log::warn!("pane {}: PTY stays blocking: {e}", pane.id);
        }
        let mut buf = vec![0u8; 64 * 1024];
        let mut last_read = Instant::now();
        loop {
            self.readers.park_if_paused();
            let n = match pty::read_pty(&mut reader, &mut buf, self.readers.wake_fd()) {
                // Woken by the gate: park.
                Ok(None) => continue,
                Ok(Some(0)) => break,
                Ok(Some(n)) => n,
                Err(e) => {
                    log::warn!("pane {} read error: {e}", pane.id);
                    break;
                }
            };
            let quiet = last_read.elapsed() >= QUIET;
            last_read = Instant::now();
            let mut st = pipe.state.lock();
            if quiet && st.data.is_empty() && !st.parsing {
                // Output after a pause (an echo, a prompt): parse it right here rather than
                // waking the parser, which is idle and has nothing queued. Nothing else can be
                // appended meanwhile, so the order holds.
                drop(st);
                self.parse_guarded(&pane, &buf[..n]);
                continue;
            }
            while st.data.len() >= READ_AHEAD {
                pipe.room.wait(&mut st);
            }
            // A burst outgrew the small buffer: go straight to the full size in one allocation
            // (doubling would leave a trail of freed blocks the allocator keeps; one big block
            // is its own mapping, returned to the system when freed).
            if st.data.len() + n > st.data.capacity() && st.data.capacity() >= KEEP_BUFFER {
                let len = st.data.len();
                st.data.reserve_exact(READ_AHEAD + buf.len() - len);
            }
            st.data.extend_from_slice(&buf[..n]);
            pipe.ready.notify_one();
        }
        pipe.state.lock().eof = true;
        pipe.ready.notify_one();
    }

    /// Parses what the reader collected, a batch at a time.
    fn parser_loop(self: Arc<Self>, pane: Arc<Pane>, pipe: Arc<ReadAhead>) {
        interactive_qos();
        let mut batch = Vec::new();
        loop {
            {
                let mut st = pipe.state.lock();
                st.parsing = false;
                while st.data.is_empty() && !st.eof {
                    pipe.ready.wait(&mut st);
                }
                if st.data.is_empty() {
                    break;
                }
                // Swap buffers: the reader keeps appending into the (emptied) previous batch.
                std::mem::swap(&mut st.data, &mut batch);
                st.parsing = true;
                pipe.room.notify_one();
            }
            self.parse_guarded(&pane, &batch);
            batch.clear();
            // After a burst, give the memory back rather than keep megabytes per pane (the
            // reader's buffer is this one after the next swap).
            if batch.capacity() > KEEP_BUFFER {
                batch = Vec::new();
            }
        }
        self.pane_exited(&pane);
    }

    /// [`Daemon::parse_output`], surviving a panic in the terminal code: the pane goes on with
    /// the next output instead of freezing with a dead parser thread.
    fn parse_guarded(&self, pane: &Arc<Pane>, bytes: &[u8]) {
        let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.parse_output(pane, bytes)
        }));
        if parsed.is_err() {
            log::error!(
                "pane {}: the terminal panicked on {} bytes of output",
                pane.id,
                bytes.len()
            );
        }
    }

    /// Feeds PTY output to the pane's terminal and forwards it to subscribers, a slice per
    /// lock so that requests for the pane are not held up by a long batch.
    fn parse_output(&self, pane: &Arc<Pane>, bytes: &[u8]) {
        for slice in bytes.chunks(PARSE_SLICE) {
            let events = {
                let mut st = pane.state.lock();
                if st.subscribers.is_empty() {
                    st.term.advance(slice);
                } else {
                    let data = st.term.advance_forward(slice);
                    self.forward_output(pane.id, &mut st, data);
                }
                update_carry(&mut st.carry, slice);
                st.term.drain_events()
            };
            self.handle_term_events(pane, events);
        }
    }

    /// Sends PTY output to the pane's subscribers, in stream order (called with the pane
    /// locked). A client that falls behind is re-attached with fresh state once it caught up,
    /// rather than getting a stream with holes.
    fn forward_output(&self, pane: PaneId, st: &mut PaneState, data: Vec<u8>) {
        let clients = self.clients.lock().clone();
        let mut plan = Vec::new();
        let mut need_state = false;
        for (cid, sub) in st.subscribers.iter_mut() {
            let Some(c) = clients.get(cid) else { continue };
            if sub.resync {
                if c.caught_up() {
                    sub.resync = false;
                    need_state = true;
                    plan.push((c.clone(), true));
                }
            } else if c.backlogged() {
                log::warn!("client {cid} fell behind on pane {pane}; re-syncing");
                sub.resync = true;
            } else {
                plan.push((c.clone(), false));
            }
        }
        // The state already includes this chunk.
        let state = need_state.then(|| st.term.serialize_state());
        let size = st.term.size();
        // The last output event takes the buffer; only the others copy it.
        let last_output = plan.iter().rposition(|(_, attach)| !attach);
        let mut data = Some(data);
        for (i, (c, attach)) in plan.into_iter().enumerate() {
            let ev = if attach {
                Event::Attach {
                    pane,
                    size,
                    state: state.clone().unwrap_or_default(),
                }
            } else if data.as_ref().is_none_or(|d| d.is_empty()) {
                continue;
            } else {
                let data = if Some(i) == last_output {
                    data.take().unwrap_or_default()
                } else {
                    data.clone().unwrap_or_default()
                };
                Event::Output { pane, data }
            };
            c.send(ServerMessage::Event(ev));
        }
    }

    /// Re-attaches `pane`'s subscribers that were switched to re-sync and have caught up.
    fn recover_subscribers(&self, pane: PaneId, st: &mut PaneState) {
        if !st.subscribers.values().any(|s| s.resync) {
            return;
        }
        let clients = self.clients.lock().clone();
        let mut ready = Vec::new();
        for (cid, sub) in st.subscribers.iter_mut() {
            if let Some(c) = clients.get(cid)
                && sub.resync
                && c.caught_up()
            {
                sub.resync = false;
                ready.push(c.clone());
            }
        }
        if ready.is_empty() {
            return;
        }
        let state = st.term.serialize_state();
        let size = st.term.size();
        for c in ready {
            c.send(ServerMessage::Event(Event::Attach {
                pane,
                size,
                state: state.clone(),
            }));
        }
    }

    /// Re-sends the full state to a pane's subscribers after a change that isn't part of the
    /// output stream (clearing the scrollback, a reset).
    fn send_subscribers(&self, st: &PaneState, ev: Event) {
        let clients = self.clients.lock();
        for cid in st.subscribers.keys() {
            if let Some(c) = clients.get(cid) {
                c.send(ServerMessage::Event(ev.clone()));
            }
        }
    }

    fn reattach(&self, pane: PaneId) {
        let Some(p) = self.pane(pane) else { return };
        let mut st = p.state.lock();
        if st.subscribers.is_empty() {
            return;
        }
        let state = st.term.serialize_state();
        let size = st.term.size();
        let clients = self.clients.lock();
        for cid in st.subscribers.keys() {
            if let Some(c) = clients.get(cid) {
                c.send(ServerMessage::Event(Event::Attach {
                    pane,
                    size,
                    state: state.clone(),
                }));
            }
        }
    }

    fn handle_term_events(&self, pane: &Arc<Pane>, events: Vec<TermEvent>) {
        if events.is_empty() {
            return;
        }
        let cfg = self.config.read().notifications.clone();
        let mut info_changed = false;
        let mut outgoing = Vec::new();
        {
            let mut st = pane.state.lock();
            for ev in events {
                match ev {
                    TermEvent::PtyWrite(b) => pane.write(b),
                    TermEvent::Title(_) => {
                        // Through `pane_title`: an agent's session topic stays put while the
                        // agent repaints its own title ("◑ Fix login bug") with every frame.
                        let title = pane_title(&st, st.info.foreground.as_ref());
                        if title != st.info.title {
                            st.info.title = title;
                            info_changed = true;
                        }
                    }
                    TermEvent::Bell => outgoing.push(Event::Bell { pane: pane.id }),
                    TermEvent::ClipboardStore(text) => outgoing.push(Event::ClipboardStore {
                        pane: pane.id,
                        text,
                    }),
                    TermEvent::ClipboardLoad => {
                        outgoing.push(Event::ClipboardRequest { pane: pane.id })
                    }
                    TermEvent::Notify { title, body } => {
                        st.agent.flag_attention();
                        if cfg.enabled && cfg.program_notifications {
                            let title = if title.is_empty() {
                                st.agent
                                    .state()
                                    .map(|a| a.name.clone())
                                    .unwrap_or_else(|| st.info.title.clone())
                            } else {
                                title
                            };
                            outgoing.push(Event::Notify {
                                pane: pane.id,
                                title,
                                body,
                                permission: None,
                            });
                        }
                    }
                    TermEvent::Cwd(d) => {
                        st.shell_integration_seen = true;
                        if st.osc_cwd.as_deref() != Some(&d) {
                            st.osc_cwd = Some(d.clone());
                            st.info.cwd = Some(d);
                            info_changed = true;
                        }
                    }
                    TermEvent::PromptStart => {
                        st.shell_integration_seen = true;
                        if !st.info.at_prompt {
                            st.info.at_prompt = true;
                            info_changed = true;
                        }
                        // Whatever reported progress is done (or died without clearing it).
                        if st.info.progress.take().is_some() {
                            info_changed = true;
                        }
                        if let Some((_, input)) = st.pending_input.take() {
                            pane.write(input);
                        }
                    }
                    TermEvent::CommandStart => {
                        st.info.at_prompt = false;
                        st.command_started = Some(Instant::now());
                        info_changed = true;
                    }
                    TermEvent::CommandLine(c) => st.command_line = Some(c),
                    TermEvent::CommandFinished(code) => {
                        st.info.last_exit_status = code;
                        info_changed = true;
                        if let Some(start) = st.command_started.take() {
                            let secs = cfg.command_finished_secs;
                            if cfg.enabled
                                && secs > 0
                                && start.elapsed() >= Duration::from_secs(secs)
                            {
                                let what =
                                    st.command_line.take().unwrap_or_else(|| "Command".into());
                                let status = match code {
                                    Some(0) | None => "finished".to_owned(),
                                    Some(c) => format!("failed (exit {c})"),
                                };
                                outgoing.push(Event::Notify {
                                    pane: pane.id,
                                    title: format!("{what} {status}"),
                                    body: format!("after {}s", start.elapsed().as_secs()),
                                    permission: None,
                                });
                            }
                        }
                    }
                    TermEvent::Progress(p) => {
                        st.info.progress = p;
                        info_changed = true;
                    }
                    // Subscribers run their own copy of the terminal and free images there.
                    TermEvent::ImageFreed(_) => {}
                }
            }
            if info_changed {
                outgoing.push(Event::PaneInfo(st.info.clone()));
            }
            for ev in outgoing {
                self.broadcast(ev, true);
            }
        }
    }

    fn pane_exited(&self, pane: &Arc<Pane>) {
        // Give the kernel a moment to deliver the exit status, without holding the pane's
        // lock while waiting.
        let mut code = None;
        for _ in 0..50 {
            if let Some(c) = pane.state.lock().pty.try_wait() {
                code = c;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if self.stopping.load(Ordering::Relaxed) {
            log::info!("pane {} ended with the daemon", pane.id);
            return;
        }
        let hold = {
            let mut st = pane.state.lock();
            st.info.alive = false;
            st.info.exit_code = code;
            if st.hold {
                let msg = match code {
                    Some(c) => format!("\r\n\x1b[2m[process exited with code {c}]\x1b[0m"),
                    None => "\r\n\x1b[2m[process exited]\x1b[0m".to_owned(),
                };
                st.term.print(&msg);
            }
            st.hold
        };
        if self.pane(pane.id).is_none() {
            // Closed explicitly; ClosePane already notified clients.
            return;
        }
        log::info!("pane {} exited ({code:?})", pane.id);
        {
            let mut recent = self.just_exited.lock();
            recent.retain(|(_, at)| at.elapsed() < TEARDOWN_GRACE);
            recent.push((pane.clone(), Instant::now()));
        }
        self.broadcast(
            Event::PaneExited {
                pane: pane.id,
                code,
            },
            false,
        );
        if hold {
        } else {
            self.panes.lock().remove(&pane.id);
            self.session_dirty.store(true, Ordering::Relaxed);
            self.broadcast(Event::PaneClosed { pane: pane.id }, false);
        }
    }

    /// Panes whose shell died within `TEARDOWN_GRACE` of a SIGTERM/SIGINT: part of the
    /// teardown, so they stay in the saved session. Empty unless the daemon is stopping.
    fn teardown_exits(&self) -> Vec<Arc<Pane>> {
        if !self.stopping.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let stopped = *self.stopped_at.get_or_init(Instant::now);
        self.just_exited
            .lock()
            .iter()
            .filter(|(_, at)| stopped.saturating_duration_since(*at) < TEARDOWN_GRACE)
            .map(|(p, _)| p.clone())
            .collect()
    }

    pub fn close_pane(&self, id: PaneId) -> bool {
        let Some(pane) = self.panes.lock().remove(&id) else {
            return false;
        };
        self.just_exited.lock().retain(|(p, _)| p.id != id);
        pane.state.lock().pty.hangup();
        self.session_dirty.store(true, Ordering::Relaxed);
        self.broadcast(Event::PaneClosed { pane: id }, false);
        true
    }

    // -----------------------------------------------------------------------------------------
    // Frame pump
    // -----------------------------------------------------------------------------------------

    /// Runs the git probes requested by the monitor, one at a time.
    pub fn git_worker(self: Arc<Self>) {
        while let Ok((id, cwd)) = self.git_rx.recv() {
            let info = crate::git::probe(&cwd);
            let Some(pane) = self.pane(id) else { continue };
            let mut st = pane.state.lock();
            st.git_pending = false;
            if st.info.git != info {
                st.info.git = info;
                self.broadcast(Event::PaneInfo(st.info.clone()), false);
            }
        }
    }

    /// Asks the on-device model what the monitor queued, one question at a time.
    pub fn ai_worker(self: Arc<Self>) {
        while let Ok(job) = self.ai_rx.recv() {
            match job {
                AiJob::Title {
                    pane,
                    agent,
                    prompt,
                    screen,
                } => {
                    let topic = self
                        .ai
                        .ask(&ai::title(&agent, prompt.as_deref(), &screen))
                        .ok()
                        .and_then(|t| ai::clean_title(&t));
                    self.update_agent(pane, |st| {
                        st.agent.set_ai_topic(topic);
                        Vec::new()
                    });
                }
                AiJob::Status {
                    pane,
                    hash,
                    agent,
                    screen,
                } => {
                    let waiting = self
                        .ai
                        .ask(&ai::status(&agent, &screen))
                        .is_ok_and(|a| a.trim() == ai::WAITING);
                    // The next monitor tick uses the answer.
                    if let Some(p) = self.pane(pane) {
                        p.state.lock().agent.set_ai_waiting(hash, waiting);
                    }
                }
                AiJob::Detail {
                    pane,
                    episode,
                    done,
                    agent,
                    hint,
                    screen,
                    notify,
                } => {
                    let ask = if done {
                        ai::summary(&agent, &screen)
                    } else {
                        ai::attention(&agent, hint.as_deref(), &screen)
                    };
                    let detail = self.ai.ask(&ask).ok().and_then(|t| ai::clean_detail(&t));
                    self.update_agent(pane, |st| {
                        let current = st.agent.episode() == episode;
                        let permission = st.agent.permission_prompt();
                        if let Some(d) = &detail {
                            st.agent.set_ai_detail(episode, d.clone());
                        }
                        // The user already answered (or the agent moved on): old news.
                        let Some((title, body)) = notify.filter(|_| current) else {
                            return Vec::new();
                        };
                        let body = match (&detail, done) {
                            (Some(d), true) => format!("{d} ({body})"),
                            (Some(d), false) => d.clone(),
                            (None, _) => body,
                        };
                        vec![Event::Notify {
                            pane,
                            title,
                            body,
                            permission,
                        }]
                    });
                }
            }
        }
    }

    /// Applies `f` to a pane's state, then re-derives its agent state and title and tells
    /// clients when they changed.
    fn update_agent(&self, pane: PaneId, f: impl FnOnce(&mut PaneState) -> Vec<Event>) {
        let Some(p) = self.pane(pane) else { return };
        let mut st = p.state.lock();
        let events = f(&mut st);
        if let Some(new) = st.agent.refresh() {
            st.info.agent = new;
            st.info.title = pane_title(&st, st.info.foreground.as_ref());
            self.broadcast(Event::PaneInfo(st.info.clone()), false);
        }
        for ev in events {
            self.broadcast(ev, true);
        }
    }

    /// Queues what the model should look at for the pane's agent (see `AgentTracker::ai_wants`).
    fn queue_ai(
        &self,
        pane: PaneId,
        st: &mut PaneState,
        cfg: &thurm_config::AiConfig,
        screen: &str,
    ) {
        let Some(agent) = st.agent.state().map(|a| a.name.clone()) else {
            return;
        };
        if st.info.password_input || self.ai.unavailable().is_some() {
            return;
        }
        let tail = agents::tail(screen, 20);
        let title = st.term.title().map(str::to_owned);
        for want in st
            .agent
            .ai_wants(cfg.titles(), cfg.status(), &tail, title.as_deref())
        {
            let job = match want {
                AiWant::Title { prompt } => AiJob::Title {
                    pane,
                    agent: agent.clone(),
                    prompt,
                    screen: agents::tail(screen, AI_SCREEN_LINES),
                },
                AiWant::Status { hash } => AiJob::Status {
                    pane,
                    hash,
                    agent: agent.clone(),
                    screen: tail.clone(),
                },
            };
            if self.ai_tx.try_send(job).is_err() {
                log::debug!("pane {pane}: model queue full");
                // Asked again after the next turn.
                st.agent.set_ai_topic(None);
            }
        }
    }

    fn explain(&self, pane: PaneId) -> Result<String, String> {
        if !self.config.read().ai.explain() {
            return Err(
                "explaining needs the on-device model: set `enabled = true` under [ai] \
                 in the config (`thurm set ai.enabled true`)"
                    .into(),
            );
        }
        let (output, exit) = self.with_pane(pane, |_, st| {
            (st.term.last_command(150), st.info.last_exit_status)
        })?;
        let output = output.ok_or(
            "no finished command to explain (shell integration marks where commands start)",
        )?;
        let text = self.ai.ask(&ai::explain(&output, exit))?;
        Ok(text.trim().to_owned())
    }

    // -----------------------------------------------------------------------------------------
    // Monitor: foreground process, agents, password prompts, cwd, autosave
    // -----------------------------------------------------------------------------------------

    pub fn monitor(self: Arc<Self>) {
        let mut last_save = Instant::now();
        let mut tick: u64 = 0;
        while !self.shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            tick += 1;
            if !self.stopping.load(Ordering::Relaxed) {
                self.just_exited
                    .lock()
                    .retain(|(_, at)| at.elapsed() < TEARDOWN_GRACE);
            }
            let (idle_after, interval, detect, ai_cfg) = {
                let c = self.config.read();
                (
                    Duration::from_millis(c.agents.idle_after_ms),
                    Duration::from_secs(c.session.snapshot_interval_secs.max(1)),
                    c.agents.detect,
                    c.ai.clone(),
                )
            };
            let defs = self.agent_defs.read().clone();
            for pane in self.all_panes() {
                let mut events = Vec::new();
                {
                    let mut st = pane.state.lock();
                    if !st.info.alive {
                        continue;
                    }
                    // A shell that exited while a background job still holds the terminal is
                    // collected now (no zombie); its status is kept for when the pane ends.
                    let _ = st.pty.try_wait();
                    // Subscribers that fell behind get fresh state once they caught up, even if
                    // the pane has no new output to carry it.
                    self.recover_subscribers(pane.id, &mut st);
                    if st.term.sync_deadline().is_some_and(|d| d <= Instant::now()) {
                        st.term.flush_sync();
                    }
                    let before = st.info.clone();
                    if st.agent.invalidate_dead_report_owner() {
                        st.info.agent = st.agent.state().cloned();
                    }
                    let fg = st.pty.foreground_pgrp().and_then(procinfo::process_info);
                    st.info.password_input = st.pty.password_mode();
                    if (st.osc_cwd.is_none() || !st.shell_integration_seen)
                        && let Some(pid) = st.info.pid
                    {
                        // The shell's cwd (login(1) on macOS execs into the shell, same pid).
                        if let Some(c) = procinfo::cwd(pid) {
                            st.info.cwd = Some(c);
                        }
                    }
                    let idle = st.term.last_output.elapsed();
                    st.info.idle_ms = idle.as_millis().min(u64::MAX as u128) as u64;
                    if detect {
                        let is_agent = fg
                            .as_ref()
                            .is_some_and(|p| defs.iter().any(|d| d.matches(&p.name, &p.argv)));
                        let screen = if is_agent || st.agent.state().is_some() {
                            st.term.screen_text()
                        } else {
                            String::new()
                        };
                        let tail = agents::tail(&screen, 20);
                        if let Some(new) =
                            st.agent.update(&defs, fg.as_ref(), &tail, idle, idle_after)
                        {
                            events.extend(self.agent_notifications(
                                pane.id,
                                &st,
                                before.agent.as_ref(),
                                new.as_ref(),
                            ));
                            st.info.agent = new;
                        }
                        if ai_cfg.enabled {
                            self.queue_ai(pane.id, &mut st, &ai_cfg, &screen);
                        }
                    } else if let Some(new) = st.agent.update_report_foreground(fg.as_ref()) {
                        st.info.agent = new;
                    }
                    if let Some(p) = &fg {
                        if !procinfo::is_shell(&p.name) {
                            st.info.at_prompt = false;
                        } else if !st.shell_integration_seen {
                            st.info.at_prompt = true;
                        }
                    }
                    if let Some(cwd) = st.info.cwd.clone()
                        && !st.git_pending
                    {
                        // Re-probe on a directory change, else every 5 s in a repository
                        // (the diff moves while agents edit) and every 30 s elsewhere.
                        let every = Duration::from_secs(if st.info.git.is_some() { 5 } else { 30 });
                        let due = match &st.git_probe {
                            Some((at, dir)) => dir != &cwd || at.elapsed() >= every,
                            None => true,
                        };
                        if due && self.git_tx.try_send((pane.id, cwd.clone())).is_ok() {
                            st.git_pending = true;
                            st.git_probe = Some((Instant::now(), cwd));
                        }
                    }
                    st.info.title = pane_title(&st, fg.as_ref());
                    st.info.foreground = fg;
                    if let Some((due, _)) = &st.pending_input
                        && Instant::now() >= *due
                        && let Some((_, input)) = st.pending_input.take()
                    {
                        pane.write(input);
                    }
                    // idle_ms changes every tick; only report it along with other changes, or
                    // every 2 s for clients that display it.
                    let mut cmp = st.info.clone();
                    cmp.idle_ms = before.idle_ms;
                    if cmp != before || tick.is_multiple_of(4) {
                        events.push(Event::PaneInfo(st.info.clone()));
                    }
                    // Under the pane's lock, as everywhere: a state taken before a hook's must
                    // not reach clients after it.
                    for ev in events {
                        self.broadcast(ev, true);
                    }
                }
            }
            if self.store.is_some()
                && (self.session_dirty.load(Ordering::Relaxed)
                    && last_save.elapsed() > Duration::from_secs(1)
                    || last_save.elapsed() > interval)
            {
                let _ = self.save_session_with(false);
                last_save = Instant::now();
            }
            let idle_exit = self.config.read().session.daemon_idle_exit_secs;
            if idle_exit > 0
                && self.panes.lock().is_empty()
                && self.clients.lock().is_empty()
                && self.last_activity.lock().elapsed() > Duration::from_secs(idle_exit)
            {
                log::info!("idle, exiting");
                self.shutdown.store(true, Ordering::Relaxed);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Persistence
    // -----------------------------------------------------------------------------------------

    /// The layout without the panes that are gone, except those in `keep`.
    fn sanitized_layout(&self, keep: &[Arc<Pane>]) -> Option<String> {
        let raw = self.layout.lock().clone()?;
        let mut layout: Layout = serde_json::from_str(&raw).ok()?;
        let panes = self.panes.lock();
        layout.retain_panes(&|id| panes.contains_key(&id) || keep.iter().any(|p| p.id == id));
        serde_json::to_string(&layout).ok()
    }

    pub fn save_session(&self) -> std::io::Result<()> {
        self.save_session_with(true)
    }

    /// `full`: serialize every changed scrollback now. Autosave leaves panes whose history
    /// was serialized less than `HISTORY_AUTOSAVE_INTERVAL` ago for a later round.
    fn save_session_with(&self, full: bool) -> std::io::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let _saving = self.save_lock.lock();
        // Changes from here on (a hook during the save) make the session dirty again.
        let was_dirty = self.session_dirty.swap(false, Ordering::Relaxed);
        let lines = self.config.read().session.scrollback_lines;
        let mut panes = Vec::new();
        let mut scrollbacks = Vec::new();
        // Scrollbacks written by this save: marked saved only once it succeeded.
        let mut written = Vec::new();
        let teardown = self.teardown_exits();
        let mut saving = self.all_panes();
        for p in &teardown {
            if !saving.iter().any(|s| s.id == p.id) {
                saving.push(p.clone());
            }
        }
        saving.sort_by_key(|p| p.id);
        for pane in saving {
            let mut st = pane.state.lock();
            if !st.info.alive && !teardown.iter().any(|p| p.id == pane.id) {
                continue;
            }
            let generation = st.term.generation();
            let recent = st
                .history_at
                .is_some_and(|at| at.elapsed() < HISTORY_AUTOSAVE_INTERVAL);
            if (generation != st.saved_generation || st.last_history.is_none()) && (full || !recent)
            {
                if let Some(h) = st.term.serialize_history(lines) {
                    st.last_history = Some(Arc::from(h));
                    st.history_at = Some(Instant::now());
                }
                if let Some(h) = &st.last_history {
                    scrollbacks.push((pane.id, Arc::clone(h)));
                }
                written.push((Arc::clone(&pane), generation));
            }
            panes.push(PaneSnapshot {
                id: pane.id,
                cwd: st.info.cwd.clone(),
                title: st.info.title.clone(),
                command: st.command.clone(),
                agent: st.agent.state().map(|a| a.kind.clone()),
                agent_session: st
                    .agent
                    .state()
                    .and(st.agent.session_id())
                    .map(str::to_owned),
                agent_resume: st.agent.resume_argv().map(<[String]>::to_vec),
                size: st.info.size.into(),
            });
        }
        let snap = SessionSnapshot {
            version: persist::SNAPSHOT_VERSION,
            saved_at: persist::now_secs(),
            layout: self.sanitized_layout(&teardown),
            panes,
            next_pane_id: self.next_pane.load(Ordering::Relaxed),
        };
        match store.save(&snap, &scrollbacks) {
            Ok(()) => {
                for (pane, generation) in written {
                    pane.state.lock().saved_generation = generation;
                }
                Ok(())
            }
            Err(e) => {
                log::warn!("failed to save session: {e}");
                if was_dirty {
                    self.session_dirty.store(true, Ordering::Relaxed);
                }
                // Retry these scrollbacks on the next save, not after the autosave interval.
                for (pane, _) in written {
                    pane.state.lock().history_at = None;
                }
                Err(e)
            }
        }
    }

    /// Everything the next image needs to carry on (see `upgrade.rs`). The PTY fds stay owned
    /// by the panes; they only become the new image's through exec.
    /// Stops every pane's reader and waits until each pane parsed everything already read, so
    /// the terminals can be serialized with nothing lost: the rest of the output waits in the
    /// kernel for the next image. False when a reader didn't stop within `stall`, or a parser
    /// made no progress for `stall` (or everything took longer than `limit`).
    pub fn quiesce_readers(&self, stall: Duration, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        if !self.readers.pause(stall) {
            return false;
        }
        for pane in self.all_panes() {
            let mut generation = pane.state.lock().term.generation();
            let mut progress = Instant::now();
            loop {
                {
                    let st = pane.pipe.state.lock();
                    if st.data.is_empty() && !st.parsing {
                        break;
                    }
                }
                let now = Instant::now();
                let g = pane.state.lock().term.generation();
                if g != generation {
                    generation = g;
                    progress = now;
                }
                if now >= end || now - progress >= stall {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        true
    }

    /// Lets the readers go on (the upgrade did not happen).
    pub fn resume_readers(&self) {
        self.readers.resume();
    }

    pub fn handoff(&self, listener_fd: RawFd, log_to_file: bool) -> upgrade::Handoff {
        let mut panes = Vec::new();
        for pane in self.all_panes() {
            let mut st = pane.state.lock();
            let exited = st.pty.try_wait().or(st.pty.exit_status());
            let state = st.term.serialize_state();
            panes.push(upgrade::PaneHandoff {
                id: pane.id,
                fd: st.pty.master_fd(),
                pid: st.pty.pid(),
                size: st.info.size.into(),
                title: st.info.title.clone(),
                cwd: st.info.cwd.clone(),
                command: st.command.clone(),
                hold: st.hold,
                restored: st.info.restored,
                exited: exited.or((!st.info.alive).then_some(st.info.exit_code)),
                osc_cwd: st.osc_cwd.clone(),
                shell_integration_seen: st.shell_integration_seen,
                state: upgrade::PaneHandoff::encode_state(&state),
                pending: upgrade::PaneHandoff::encode_state(&st.carry),
                shell_token: st.term.shell_token().map(str::to_owned),
                shell_path: st.term.shell_path().map(str::to_owned),
                agent_report: st.agent.report_handoff(),
                agent_prompt: st.agent.pending_prompt_handoff(),
                terminal_size_after_detach: st.terminal_attachment.map(|(_, size)| size.into()),
            });
        }
        upgrade::Handoff {
            version: upgrade::HANDOFF_VERSION,
            listener_fd,
            lock_fd: None,
            log_to_file,
            layout: self.layout.lock().clone(),
            next_pane_id: self.next_pane.load(Ordering::Relaxed),
            restored: self.restored.load(Ordering::Relaxed),
            panes,
        }
    }

    /// Takes over the panes of the image we replaced.
    pub fn adopt(self: &Arc<Self>, h: upgrade::Handoff) {
        let mut adopted = 0;
        for p in h.panes {
            let mut size = sanitize_size(p.size.into());
            let pty = match Pty::adopt(p.fd, p.pid, p.exited) {
                Ok(pty) => pty,
                Err(e) => {
                    log::warn!("pane {}: cannot adopt fd {}: {e}", p.id, p.fd);
                    continue;
                }
            };
            let mut term = Terminal::new(size, self.engine.read().clone());
            term.replay(&p.state_bytes());
            // The start of a sequence the old image had parsed; the rest follows from the PTY.
            let pending = p.pending_bytes();
            if !pending.is_empty() {
                term.advance(&pending);
                let _ = term.drain_events();
            }
            if let Some(original) = p.terminal_size_after_detach {
                size = sanitize_size(original.into());
                if let Err(error) = pty.resize(size) {
                    log::warn!("pane {}: cannot restore size after upgrade: {error}", p.id);
                }
                term.resize(size);
            }
            term.set_shell_token(p.shell_token.clone());
            term.set_shell_path(p.shell_path.clone());
            let alive = p.exited.is_none();
            let info = PaneInfo {
                id: p.id,
                title: p.title,
                cwd: p.cwd,
                pid: Some(p.pid),
                alive,
                exit_code: p.exited.flatten(),
                size,
                restored: p.restored,
                ..Default::default()
            };
            self.next_pane.fetch_max(p.id + 1, Ordering::Relaxed);
            let installed = self.install_pane(
                InstallPane {
                    term,
                    pty,
                    info,
                    command: p.command,
                    hold: p.hold,
                    osc_cwd: p.osc_cwd,
                    shell_integration_seen: p.shell_integration_seen,
                    // Still the start of a sequence if the next upgrade comes before its end.
                    carry: pending,
                },
                alive,
            );
            match installed {
                Ok(()) => {
                    adopted += 1;
                    if let Some(pane) = self.pane(p.id) {
                        let mut st = pane.state.lock();
                        let fg = st.pty.foreground_pgrp();
                        st.agent.saw_foreground(fg);
                        if let Some(report) = p.agent_report {
                            st.agent.restore_report_handoff(report);
                        }
                        if let Some(prompt) = p.agent_prompt {
                            st.agent.restore_pending_prompt(prompt);
                        }
                        st.info.agent = st.agent.state().cloned();
                    }
                }
                Err(e) => log::warn!("pane {}: {e}", p.id),
            }
        }
        self.next_pane.fetch_max(h.next_pane_id, Ordering::Relaxed);
        *self.layout.lock() = h.layout;
        self.restored.store(h.restored, Ordering::Relaxed);
        log::info!("adopted {adopted} panes from the previous daemon");
    }

    /// Recreate panes from the last snapshot (after a reboot or daemon restart).
    pub fn restore_session(self: &Arc<Self>) {
        let Some(store) = &self.store else { return };
        let Some(snap) = store.load() else { return };
        let cfg = self.config.read().clone();
        let defs = cfg.agent_defs();
        let mut restored = 0;
        for p in &snap.panes {
            let history = store.load_scrollback(p.id);
            let reported_resume = p.agent_resume.as_ref().filter(|argv| {
                cfg.session.resume_agents
                    && p.agent_session.is_some()
                    && agents::reporting::validate_resume(argv).is_ok()
            });
            let req = CreatePane {
                // A reported resume runs inside a shell. Keep the pane if it fails or exits.
                command: if reported_resume.is_some() {
                    None
                } else {
                    p.command.clone()
                },
                cwd: p.cwd.clone(),
                size: p.size.into(),
                ..Default::default()
            };
            match self.create_pane(req, Some((p.id, history))) {
                Ok(id) => {
                    restored += 1;
                    if let Some(argv) = reported_resume {
                        if let Some(pane) = self.pane(id) {
                            let shell = cfg
                                .terminal
                                .shell
                                .as_ref()
                                .and_then(|s| s.first())
                                .cloned()
                                .unwrap_or_else(shell::user_shell);
                            let mut st = pane.state.lock();
                            match agents::reporting::resume_input(&shell, argv) {
                                Ok(input) => {
                                    st.pending_input =
                                        Some((Instant::now() + Duration::from_millis(2500), input))
                                }
                                Err(error) => {
                                    log::warn!("pane {id}: {error}");
                                    st.term
                                        .print(&format!("\r\n[agent resume skipped: {error}]\r\n"));
                                }
                            }
                        }
                    } else if cfg.session.resume_agents && p.command.is_none() {
                        let def = p
                            .agent
                            .as_ref()
                            .and_then(|k| defs.iter().find(|d| &d.kind == k));
                        // The exact session when hooks reported one, else "the last one here".
                        let resume =
                            def.and_then(|d| match (&d.resume_session, &p.agent_session) {
                                (Some(cmd), Some(id)) => {
                                    Some(cmd.iter().map(|a| a.replace("{session}", id)).collect())
                                }
                                _ => d.resume.clone(),
                            });
                        if let (Some(cmd), Some(pane)) = (resume, self.pane(id)) {
                            let line = cmd
                                .iter()
                                .map(|a| shell::shell_quote(a))
                                .collect::<Vec<_>>()
                                .join(" ");
                            pane.state.lock().pending_input = Some((
                                Instant::now() + Duration::from_millis(2500),
                                format!("{line}\r").into_bytes(),
                            ));
                        }
                    }
                }
                Err(e) => log::warn!("failed to restore pane {}: {e}", p.id),
            }
        }
        self.next_pane
            .fetch_max(snap.next_pane_id, Ordering::Relaxed);
        *self.layout.lock() = snap.layout;
        if restored > 0 {
            self.restored.store(true, Ordering::Relaxed);
            log::info!("restored {restored} panes from snapshot");
        }
    }

    // -----------------------------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------------------------

    pub fn handle(self: &Arc<Self>, client: &Arc<Client>, id: u64, req: Request) {
        *self.last_activity.lock() = Instant::now();
        let result = match req {
            Request::AgentPrompt {
                pane,
                text,
                wait,
                timeout_ms,
            } => match self.submit_agent_prompt(pane, &text, timeout_ms) {
                Err(error) => Err(error),
                Ok(ticket) if !wait => {
                    let _ = ticket;
                    Ok(Response::AgentPrompt(
                        thurm_proto::AgentPromptOutcome::Submitted,
                    ))
                }
                Ok(ticket) => {
                    let daemon = self.clone();
                    let client = client.clone();
                    std::thread::spawn(move || {
                        let result = daemon
                            .wait_agent_prompt(client.id, pane, ticket)
                            .map(Response::AgentPrompt);
                        if id != 0 {
                            client.send(ServerMessage::Response { id, result });
                        }
                    });
                    return;
                }
            },
            Request::Wait {
                pane,
                until,
                timeout_ms,
            } => {
                let daemon = self.clone();
                let client = client.clone();
                std::thread::spawn(move || {
                    let outcome = daemon.wait(client.id, pane, until, timeout_ms);
                    if id != 0 {
                        client.send(ServerMessage::Response {
                            id,
                            result: outcome.map(Response::Wait),
                        });
                    }
                });
                return;
            }
            Request::Explain { pane } => {
                let daemon = self.clone();
                let client = client.clone();
                std::thread::spawn(move || {
                    let result = daemon.explain(pane).map(Response::Text);
                    if id != 0 {
                        client.send(ServerMessage::Response { id, result });
                    }
                });
                return;
            }
            // Slow queries and layout confirmation must not block later requests,
            // including the desktop client's reply. Responses carry their request ID.
            slow @ (Request::Complete { .. }
            | Request::Processes { .. }
            | Request::ApplyLayout { .. }) => {
                let daemon = self.clone();
                let client = client.clone();
                let spawned = std::thread::Builder::new()
                    .name("request".into())
                    .spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            daemon.dispatch(&client, slow)
                        }))
                        .unwrap_or_else(|_| Err("the daemon hit an internal error".into()));
                        if id != 0 {
                            client.send(ServerMessage::Response { id, result });
                        }
                    });
                if let Err(e) = spawned {
                    Err(format!("could not start a request thread: {e}"))
                } else {
                    return;
                }
            }
            other => {
                // A panic answers the request with an error instead of leaving the client
                // waiting forever.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.dispatch(client, other)
                }))
                .unwrap_or_else(|_| Err("the daemon hit an internal error".into()))
            }
        };
        if id != 0 {
            client.send(ServerMessage::Response { id, result });
        }
    }

    /// Notifications for an agent status change: needs input, and (hooks) finished a turn.
    /// With `ai.notifications` the model first says what the agent asks for or did (shown in
    /// the agent switcher too), and the notification waits for it.
    fn agent_notifications(
        &self,
        pane: PaneId,
        st: &PaneState,
        before: Option<&AgentState>,
        after: Option<&AgentState>,
    ) -> Vec<Event> {
        let (n, ai_cfg) = {
            let c = self.config.read();
            (c.notifications.clone(), c.ai.clone())
        };
        let Some(a) = after else {
            return Vec::new();
        };
        let was = before.map(|b| b.status);
        // A permission prompt right after another request is news too.
        let new_prompt =
            a.permission.is_some() && a.permission != before.and_then(|b| b.permission);
        let (done, body) = match a.status {
            AgentStatus::NeedsInput if was != Some(AgentStatus::NeedsInput) || new_prompt => (
                false,
                (n.enabled && n.agent_needs_input).then(|| {
                    a.message
                        .clone()
                        .unwrap_or_else(|| "is waiting for your input".into())
                }),
            ),
            AgentStatus::Done if was != Some(AgentStatus::Done) => (
                true,
                (n.enabled && n.agent_done).then(|| match a.turn_ms {
                    Some(ms) => format!("finished after {}", agents::human_duration(ms)),
                    None => "finished".into(),
                }),
            ),
            _ => return Vec::new(),
        };
        let notify = body.map(|body| (a.name.clone(), body));
        if ai_cfg.notifications() && !st.info.password_input && self.ai.unavailable().is_none() {
            let job = AiJob::Detail {
                pane,
                episode: st.agent.episode(),
                done,
                agent: a.name.clone(),
                hint: a.message.clone(),
                screen: agents::tail(&st.term.screen_text(), AI_SCREEN_LINES),
                notify: notify.clone(),
            };
            if self.ai_tx.try_send(job).is_ok() {
                return Vec::new();
            }
        }
        let permission = a.permission.filter(|_| !done);
        notify
            .map(|(title, body)| Event::Notify {
                pane,
                title,
                body,
                permission,
            })
            .into_iter()
            .collect()
    }

    fn with_pane<T>(
        &self,
        id: PaneId,
        f: impl FnOnce(&Arc<Pane>, &mut PaneState) -> T,
    ) -> Result<T, String> {
        let pane = self.pane(id).ok_or_else(|| format!("no such pane: {id}"))?;
        let mut st = pane.state.lock();
        Ok(f(&pane, &mut st))
    }

    fn dispatch(self: &Arc<Self>, client: &Arc<Client>, req: Request) -> Result<Response, String> {
        match req {
            Request::AttachTerminal { pane, size } => self.attach_terminal(client, pane, size),
            Request::DetachTerminal { pane } => self.detach_terminal(client.id, pane),
            Request::Hello {
                client: name,
                version,
                ui,
                capabilities: _,
            } => {
                if version != PROTOCOL_VERSION {
                    return Err(format!(
                        "protocol mismatch: daemon {PROTOCOL_VERSION}, client {version}"
                    ));
                }
                client.ui.store(ui, Ordering::Relaxed);
                *client.name.lock() = name;
                Ok(Response::Hello {
                    version: PROTOCOL_VERSION,
                    daemon_pid: std::process::id(),
                    restored: self.restored.load(Ordering::Relaxed),
                    build: thurm_proto::BUILD.to_owned(),
                    capabilities: thurm_proto::CAPABILITIES
                        .iter()
                        .map(|c| (*c).to_owned())
                        .collect(),
                })
            }
            Request::CreatePane(req) => self
                .create_pane(req, None)
                .map(|pane| Response::PaneCreated { pane })
                .map_err(|e| e.to_string()),
            Request::ClosePane { pane } => {
                if self.close_pane(pane) {
                    Ok(Response::Ok)
                } else {
                    Err(format!("no such pane: {pane}"))
                }
            }
            Request::Complete { pane } => {
                let (line, cwd, at_prompt, path, pid) = self.with_pane(pane, |_, st| {
                    (
                        st.term.input_line(),
                        st.info.cwd.clone(),
                        st.info.at_prompt,
                        st.term.shell_path().map(str::to_owned),
                        st.info.pid,
                    )
                })?;
                // Until the shell reports its PATH (or for a shell handed over by a daemon
                // that gave it no token): the PATH it was started with.
                let path = path.or_else(|| procinfo::env_var(pid?, "PATH"));
                let Some(line) = line.filter(|_| at_prompt) else {
                    return Ok(Response::Completions(thurm_proto::Completions::default()));
                };
                let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
                Ok(Response::Completions(crate::complete::complete(
                    &line,
                    &cwd,
                    path.as_deref(),
                )))
            }
            Request::Processes { pane } => {
                let roots: Vec<(PaneId, u32)> = self
                    .all_panes()
                    .iter()
                    .filter(|p| pane.is_none_or(|id| id == p.id))
                    .filter_map(|p| Some((p.id, p.state.lock().info.pid?)))
                    .collect();
                let table = procinfo::process_table();
                let trees: Vec<(PaneId, Vec<procinfo::ProcRow>)> = roots
                    .iter()
                    .map(|&(id, pid)| (id, procinfo::descendants(&table, pid)))
                    .collect();
                let pids: Vec<u32> = trees
                    .iter()
                    .flat_map(|(_, t)| t.iter().map(|p| p.0))
                    .collect();
                let ports = procinfo::listening_ports(&pids);
                Ok(Response::Processes(
                    trees
                        .into_iter()
                        .map(|(id, tree)| thurm_proto::PaneProcesses {
                            pane: id,
                            processes: tree
                                .into_iter()
                                .map(|(pid, ppid, command)| thurm_proto::ProcEntry {
                                    pid,
                                    ppid,
                                    command,
                                    ports: ports.get(&pid).cloned().unwrap_or_default(),
                                })
                                .collect(),
                        })
                        .collect(),
                ))
            }
            Request::ListPanes => {
                let panes = self
                    .all_panes()
                    .iter()
                    .map(|p| p.state.lock().info.clone())
                    .collect();
                Ok(Response::Panes(panes))
            }
            Request::PaneInfo { pane } => {
                self.with_pane(pane, |_, st| Response::PaneInfo(st.info.clone()))
            }
            Request::Resize { pane, size } => {
                let size = sanitize_size(size);
                self.with_input_pane(client.id, pane, |_, st| {
                    if st.info.size != size {
                        if let Err(e) = st.pty.resize(size) {
                            log::warn!("pane {pane}: resize to {}x{}: {e}", size.cols, size.rows);
                        }
                        st.term.resize(size);
                        st.info.size = size;
                        self.send_subscribers(st, Event::Resized { pane, size });
                    }
                })?;
                Ok(Response::Ok)
            }
            Request::Input { pane, data } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    if !data.is_empty() {
                        st.agent.prompt.interrupted();
                    }
                    st.agent.user_answer(&data);
                    p.write(data);
                })?;
                Ok(Response::Ok)
            }
            Request::Paste { pane, text } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    if !text.is_empty() {
                        st.agent.prompt.interrupted();
                    }
                    st.agent.user_input();
                    p.write(st.term.paste(&text));
                })?;
                Ok(Response::Ok)
            }
            Request::Key { pane, key } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    let bytes = st.term.key(&key);
                    if !bytes.is_empty() {
                        st.agent.prompt.interrupted();
                        st.agent.user_answer(&bytes);
                        p.write(bytes);
                    }
                })?;
                Ok(Response::Ok)
            }
            Request::Mouse { pane, event } => {
                let copy_on_select = self.config.read().terminal.copy_on_select;
                let text = self.with_input_pane(client.id, pane, |p, st| {
                    let (outcome, bytes) = st.term.mouse(&event);
                    if !bytes.is_empty() {
                        st.agent.prompt.interrupted();
                    }
                    p.write(bytes);
                    (outcome == MouseOutcome::SelectionDone && copy_on_select)
                        .then(|| st.term.selection_text())
                })?;
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    client.send(ServerMessage::Event(Event::ClipboardStore { pane, text }));
                }
                Ok(Response::Ok)
            }
            Request::Wheel {
                pane,
                lines,
                col,
                row,
                mods,
            } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    let bytes = st.term.wheel(lines, col, row, mods);
                    if !bytes.is_empty() {
                        st.agent.prompt.interrupted();
                    }
                    p.write(bytes);
                })?;
                Ok(Response::Ok)
            }
            Request::Scroll { pane, scroll } => {
                // The viewport the user sees lives in the app.
                self.with_pane(pane, |_, st| st.term.scroll(scroll))?;
                self.broadcast(
                    Event::Ui(thurm_proto::UiCommand::Scroll { pane, scroll }),
                    false,
                );
                Ok(Response::Ok)
            }
            Request::Focus { pane, focused } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    if let Some(b) = st.term.focus(focused) {
                        if !b.is_empty() {
                            st.agent.prompt.interrupted();
                        }
                        p.write(b);
                    }
                    // Looking at a finished agent acknowledges it (Done → Idle).
                    if focused {
                        st.agent.acknowledge();
                        if let Some(new) = st.agent.refresh() {
                            st.info.agent = new;
                            self.broadcast(Event::PaneInfo(st.info.clone()), false);
                        }
                    }
                })?;
                Ok(Response::Ok)
            }
            Request::Subscribe { pane } => {
                // Under the pane lock, so no output slips in between the state and the stream.
                self.with_pane(pane, |_, st| {
                    let state = st.term.serialize_state();
                    let size = st.term.size();
                    client.send(ServerMessage::Event(Event::Attach { pane, size, state }));
                    st.subscribers.insert(client.id, Subscriber::default());
                })?;
                Ok(Response::Ok)
            }
            Request::Unsubscribe { pane } => {
                self.with_pane(pane, |_, st| {
                    st.subscribers.remove(&client.id);
                })?;
                Ok(Response::Ok)
            }
            Request::Selection { pane, op } => {
                self.with_pane(pane, |_, st| st.term.selection(op))?;
                Ok(Response::Ok)
            }
            Request::CopySelection { pane } => {
                self.with_pane(pane, |_, st| Response::Text(st.term.selection_text()))
            }
            Request::Search {
                pane,
                query,
                direction,
            } => {
                let found =
                    self.with_pane(pane, |_, st| st.term.search(query.as_deref(), direction))?;
                Ok(Response::Search { found })
            }
            Request::Capture { pane, opts } => {
                self.with_pane(pane, |_, st| Response::Text(st.term.capture(&opts)))
            }
            Request::Wait { .. } | Request::Explain { .. } | Request::AgentPrompt { .. } => {
                unreachable!("handled in handle()")
            }
            Request::SetLayout { json } => {
                serde_json::from_str::<Layout>(&json)
                    .map_err(|e| format!("invalid layout: {e}"))?;
                *self.layout.lock() = Some(json);
                self.broadcast(Event::LayoutChanged, false);
                self.session_dirty.store(true, Ordering::Relaxed);
                Ok(Response::Ok)
            }
            Request::GetLayout => Ok(Response::Layout(self.sanitized_layout(&[]))),
            Request::ApplyLayout { json, timeout_ms } => {
                self.apply_layout(client.id, json, timeout_ms)?;
                Ok(Response::Ok)
            }
            Request::LayoutApplied { request_id, error } => {
                self.layout_applied(client.id, request_id, error)?;
                Ok(Response::Ok)
            }
            Request::CheckUi => {
                if !self.has_ui_clients() {
                    return Err("no Thurm window is open".into());
                }
                Ok(Response::Ok)
            }
            Request::Ui(cmd) => {
                if !self.has_ui_clients() {
                    return Err("no Thurm window is open".into());
                }
                if matches!(cmd, thurm_proto::UiCommand::OpenLayout { .. }) {
                    return Err("use ApplyLayout to wait for desktop confirmation".into());
                }
                self.broadcast(Event::Ui(cmd), true);
                Ok(Response::Ok)
            }
            Request::ClipboardReply { pane, text } => {
                let events = self.with_input_pane(client.id, pane, |_, st| {
                    st.term.clipboard_reply(&text);
                    st.term.drain_events()
                })?;
                let pane = self.pane(pane).ok_or("pane vanished")?;
                self.handle_term_events(&pane, events);
                Ok(Response::Ok)
            }
            Request::ClearScrollback { pane } => {
                self.with_pane(pane, |_, st| st.term.clear_scrollback())?;
                self.reattach(pane);
                Ok(Response::Ok)
            }
            Request::ClearScreen { pane } => {
                self.with_pane(pane, |_, st| st.term.clear_screen())?;
                self.reattach(pane);
                Ok(Response::Ok)
            }
            Request::Reset { pane } => {
                self.with_pane(pane, |_, st| st.term.reset())?;
                self.reattach(pane);
                Ok(Response::Ok)
            }
            Request::ReloadConfig => {
                let (cfg, warnings) = Config::load_with_warnings().map_err(|e| e.to_string())?;
                for w in warnings {
                    log::warn!("config: {w}");
                }
                self.apply_config(cfg);
                Ok(Response::Ok)
            }
            Request::SaveSnapshot => self
                .save_session()
                .map(|()| Response::Ok)
                .map_err(|e| format!("could not save the session: {e}")),
            Request::Shutdown { kill_panes } => {
                log::info!("shutdown requested (kill panes: {kill_panes})");
                let _ = self.save_session();
                self.shutdown.store(true, Ordering::Relaxed);
                Ok(Response::Ok)
            }
            Request::ListAgentPresets => {
                Ok(Response::AgentPresets(self.config.read().agent_presets()))
            }
            Request::AgentHook {
                pane,
                agent,
                event,
                session_id,
                message,
                transcript_path,
                pgrp,
            } => {
                let titles = self.config.read().agents.session_titles;
                let name = self
                    .agent_defs
                    .read()
                    .iter()
                    .find(|d| d.kind == agent)
                    .map_or_else(|| agent.clone(), |d| d.name.clone());
                self.with_pane(pane, |_, st| {
                    let before = st.info.agent.clone();
                    if st.agent.invalidate_dead_report_owner() {
                        st.info.agent = st.agent.state().cloned();
                    }
                    st.agent.saw_foreground(st.pty.foreground_pgrp());
                    if st.agent.public_report_active() {
                        return;
                    }
                    st.agent
                        .apply_hook(pgrp, &agent, &name, &event, session_id, message);
                    st.agent.titles = titles;
                    // Hooks without a path (Notification) keep following the current file.
                    // Followed even without titles: it is where interrupts show up.
                    if transcript_path.is_some() || event == "session-start" {
                        st.agent.read_transcript(transcript_path.as_deref());
                    }
                    if let Some(new) = st.agent.refresh() {
                        let events =
                            self.agent_notifications(pane, st, before.as_ref(), new.as_ref());
                        st.info.agent = new;
                        st.info.title = pane_title(st, st.info.foreground.as_ref());
                        self.broadcast(Event::PaneInfo(st.info.clone()), false);
                        for ev in events {
                            self.broadcast(ev, true);
                        }
                    }
                })?;
                self.session_dirty.store(true, Ordering::Relaxed);
                Ok(Response::Ok)
            }
            Request::AgentExplain { pane } => {
                let defs = self.agent_defs.read().clone();
                let cfg = self.config.read();
                let idle_after = Duration::from_millis(cfg.agents.idle_after_ms);
                let detect = cfg.agents.detect;
                drop(cfg);
                let explanation = self.with_pane(pane, |_, st| {
                    if st.agent.invalidate_dead_report_owner() {
                        st.info.agent = st.agent.state().cloned();
                        self.broadcast(Event::PaneInfo(st.info.clone()), false);
                    }
                    let fg = st.pty.foreground_pgrp().and_then(procinfo::process_info);
                    let screen = agents::tail(&st.term.screen_text(), 20);
                    let idle = st.term.last_output.elapsed();
                    if detect
                        && let Some(new) =
                            st.agent
                                .update(&defs, fg.as_ref(), &screen, idle, idle_after)
                    {
                        st.info.agent = new;
                        st.info.title = pane_title(st, fg.as_ref());
                        self.broadcast(Event::PaneInfo(st.info.clone()), false);
                    }
                    if !detect && let Some(new) = st.agent.update_report_foreground(fg.as_ref()) {
                        st.info.agent = new;
                        st.info.title = pane_title(st, fg.as_ref());
                        self.broadcast(Event::PaneInfo(st.info.clone()), false);
                    }
                    let mut result = st.agent.explain(pane, &defs, fg, &screen, idle, idle_after);
                    if !detect {
                        result.rules.push("automatic detection is disabled".into());
                    }
                    result
                })?;
                Ok(Response::AgentExplanation(explanation))
            }
            Request::AgentReport { pane, report } => {
                let name = self
                    .agent_defs
                    .read()
                    .iter()
                    .find(|d| d.kind == report.agent)
                    .map_or_else(|| report.agent.clone(), |d| d.name.clone());
                self.with_pane(pane, |_, st| -> Result<(), String> {
                    if !st.info.alive {
                        return Err("pane process has exited".into());
                    }
                    if st.agent.invalidate_dead_report_owner() {
                        st.info.agent = st.agent.state().cloned();
                        self.broadcast(Event::PaneInfo(st.info.clone()), false);
                    }
                    let fg = st.pty.foreground_pgrp();
                    let (birth, pgrp) = procinfo::report_owner(report.owner_pid, fg)?;
                    let before = st.info.agent.clone();
                    st.agent.saw_foreground(fg);
                    st.agent.apply_report(&report, birth, pgrp, &name)?;
                    if let Some(new) = st.agent.refresh() {
                        let events =
                            self.agent_notifications(pane, st, before.as_ref(), new.as_ref());
                        st.info.agent = new;
                        st.info.title = pane_title(st, st.info.foreground.as_ref());
                        self.broadcast(Event::PaneInfo(st.info.clone()), false);
                        for ev in events {
                            self.broadcast(ev, true);
                        }
                    }
                    Ok(())
                })??;
                self.session_dirty.store(true, Ordering::Relaxed);
                Ok(Response::Ok)
            }
            Request::AgentRelease {
                pane,
                owner_pid,
                instance,
                sequence,
            } => {
                self.with_pane(pane, |_, st| -> Result<(), String> {
                    if st.agent.invalidate_dead_report_owner() {
                        st.info.agent = st.agent.state().cloned();
                        self.broadcast(Event::PaneInfo(st.info.clone()), false);
                    }
                    let fg = st.pty.foreground_pgrp();
                    let (birth, _) = procinfo::report_owner(owner_pid, fg)?;
                    st.agent.saw_foreground(fg);
                    st.agent
                        .release_report(owner_pid, birth, &instance, sequence)?;
                    st.info.agent = st.agent.state().cloned();
                    st.info.title = pane_title(st, st.info.foreground.as_ref());
                    self.broadcast(Event::PaneInfo(st.info.clone()), false);
                    Ok(())
                })??;
                self.session_dirty.store(true, Ordering::Relaxed);
                Ok(Response::Ok)
            }
            Request::SetAppearance { dark } => {
                let changed = self.dark.swap(dark, Ordering::Relaxed) != dark;
                let cfg = self.config.read().clone();
                if changed && cfg.theme_spec().follows_appearance() {
                    self.apply_config(cfg);
                }
                Ok(Response::Ok)
            }
            Request::SetSetting { key, value } => {
                thurm_config::write_setting(&key, &value)?;
                let cfg = Config::load().map_err(|e| e.to_string())?;
                self.apply_config(cfg);
                // Where it went: the daemon's config file, which a CLI with another
                // environment may not know about.
                Ok(Response::Text(
                    thurm_config::config_path().display().to_string(),
                ))
            }
            Request::AnswerPermission {
                pane,
                prompt,
                allow,
            } => {
                let mut answered = false;
                let mut attached = false;
                let p = self
                    .pane(pane)
                    .ok_or_else(|| format!("no such pane: {pane}"))?;
                self.update_agent(pane, |st| {
                    if st
                        .terminal_attachment
                        .is_some_and(|(owner, _)| owner != client.id)
                    {
                        attached = true;
                        return Vec::new();
                    }
                    if let Some(keys) = st.agent.answer_permission(prompt, allow) {
                        p.write(keys.to_vec());
                        answered = true;
                    }
                    Vec::new()
                });
                if attached {
                    return Err("pane input belongs to an attached terminal".into());
                }
                if !answered {
                    return Err("that permission prompt is no longer showing".into());
                }
                Ok(Response::Ok)
            }
            Request::SetTheme { spec } => {
                let spec = thurm_config::ThemeSpec::parse(&spec);
                spec.validate()?;
                thurm_config::write_theme(&spec)?;
                let cfg = Config::load().map_err(|e| e.to_string())?;
                self.apply_config(cfg);
                Ok(Response::Ok)
            }
            Request::WriteTempFile { name, data } => {
                let dir = self
                    .socket
                    .parent()
                    .unwrap_or(Path::new("/tmp"))
                    .join("paste");
                write_temp_file(&dir, &name, &data)
                    .map(|p| Response::Text(p.display().to_string()))
                    .map_err(|e| format!("cannot write {}: {e}", dir.display()))
            }
        }
    }

    pub fn apply_config(&self, cfg: Config) {
        if cfg.ai != self.config.read().ai {
            // Turned off, or on again: ask the model afresh (it may have become available).
            self.ai.stop();
        }
        *self.engine.write() = EngineConfig::from_config(&cfg, self.dark.load(Ordering::Relaxed));
        *self.agent_defs.write() = cfg.agent_defs();
        *self.config.write() = cfg;
        let engine = self.engine.read().clone();
        for pane in self.all_panes() {
            let mut st = pane.state.lock();
            st.term.set_config(engine.clone());
            drop(st);
        }
        self.broadcast(Event::ConfigReloaded, false);
    }

    fn wait(
        &self,
        client: u64,
        pane: PaneId,
        until: WaitCondition,
        timeout_ms: Option<u64>,
    ) -> Result<WaitOutcome, String> {
        let deadline = timeout_ms.map(|t| Instant::now() + Duration::from_millis(t));
        let regex = match &until {
            WaitCondition::Match { regex } => {
                Some(regex::Regex::new(regex).map_err(|e| e.to_string())?)
            }
            _ => None,
        };
        // Conditions about "becoming" free need the pane to have been busy first, otherwise
        // `wait --prompt` right after `send` would return before the command even started.
        let started = Instant::now();
        // The screen is only searched again after new output.
        let mut matched: Option<(u64, bool)> = None;
        loop {
            // Nobody is left to tell (the client disconnected).
            if !self.clients.lock().contains_key(&client) {
                return Ok(WaitOutcome::Timeout);
            }
            let Some(p) = self.pane(pane) else {
                return Ok(WaitOutcome::Exited { code: None });
            };
            {
                let st = p.state.lock();
                if !st.info.alive {
                    return Ok(WaitOutcome::Exited {
                        code: st.info.exit_code,
                    });
                }
                let settle = started.elapsed() > Duration::from_millis(300);
                let ok = match &until {
                    WaitCondition::Idle { quiet_ms } => {
                        st.term.last_output.elapsed() >= Duration::from_millis(*quiet_ms)
                    }
                    WaitCondition::Prompt => settle && st.info.at_prompt,
                    WaitCondition::Exit => false,
                    WaitCondition::Match { .. } => {
                        let generation = st.term.generation();
                        match matched {
                            Some((g, hit)) if g == generation => hit,
                            _ => {
                                let hit = regex
                                    .as_ref()
                                    .is_some_and(|r| r.is_match(&st.term.screen_text()));
                                matched = Some((generation, hit));
                                hit
                            }
                        }
                    }
                    WaitCondition::AgentStatus(want) => {
                        st.agent.state().is_some_and(|a| a.status == *want)
                    }
                    WaitCondition::AgentFree => {
                        settle
                            && match st.agent.state() {
                                Some(a) => a.status != AgentStatus::Working,
                                None => st.info.at_prompt,
                            }
                    }
                };
                if ok {
                    return Ok(WaitOutcome::Satisfied);
                }
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(WaitOutcome::Timeout);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Keeps the unterminated end of the output parsed so far in `carry`, given the next `slice`.
fn update_carry(carry: &mut Vec<u8>, slice: &[u8]) {
    if carry.is_empty() {
        let start = thurm_term::filter::unterminated_tail(slice);
        if slice.len() - start <= MAX_CARRY {
            carry.extend_from_slice(&slice[start..]);
        }
        return;
    }
    // The sequence may go on (or end) in this slice.
    carry.extend_from_slice(slice);
    let start = thurm_term::filter::unterminated_tail(carry);
    if carry.len() - start > MAX_CARRY {
        carry.clear();
    } else {
        carry.drain(..start);
    }
}

/// A random token for a pane's shell integration (see `Terminal::set_shell_token`). No pane
/// is created without one: a guessable token would let program output set the PATH again.
fn shell_token() -> anyhow::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| anyhow::anyhow!("no random source for the shell token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// PTY threads run at interactive priority: on macOS a daemon's threads otherwise may land on
/// efficiency cores and throttle output.
fn interactive_qos() {
    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

fn pane_title(st: &PaneState, fg: Option<&thurm_proto::ProcessInfo>) -> String {
    // What the agent session is about beats the agent's own generic title ("✳ Claude Code").
    if let Some(topic) = st.agent.state().and_then(|a| a.topic.as_ref()) {
        return topic.clone();
    }
    if let Some(t) = st.term.title().filter(|t| !t.trim().is_empty()) {
        return t.to_owned();
    }
    if let Some(a) = st.agent.state() {
        return a.name.clone();
    }
    if let Some(p) = fg
        && !procinfo::is_shell(&p.name)
    {
        return p.name.clone();
    }
    match &st.info.cwd {
        Some(c) => {
            let home = std::env::var("HOME").unwrap_or_default();
            if !home.is_empty() && c == &home {
                "~".into()
            } else {
                c.rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(c)
                    .to_owned()
            }
        }
        None => fg.map(|p| p.name.clone()).unwrap_or_else(|| "shell".into()),
    }
}

fn sanitize_size(mut s: PaneSize) -> PaneSize {
    s.cols = s.cols.clamp(2, 2000);
    s.rows = s.rows.clamp(1, 1000);
    if s.cell_width == 0 {
        s.cell_width = 8;
    }
    if s.cell_height == 0 {
        s.cell_height = 16;
    }
    s
}

fn spawn_writer(id: PaneId, mut writer: File, rx: Receiver<Vec<u8>>) {
    let _ = std::thread::Builder::new()
        .name(format!("pane-{id}-writer"))
        .spawn(move || {
            while let Ok(data) = rx.recv() {
                if let Err(e) = pty::write_all(&mut writer, &data) {
                    log::debug!("pane {id} write failed: {e}");
                    break;
                }
            }
        });
}

/// Writes `data` to a new file in `dir` (0700, created if needed) named after `name`'s
/// extension, readable by the owner only.
/// Deletes the files in `dir` older than `age` (pasted and dropped files are referred to by
/// path for a moment, then never again).
fn prune_old_files(dir: &Path, age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .ok()
            .filter(|m| m.is_file())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|elapsed| elapsed > age);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn write_temp_file(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    prune_old_files(dir, Duration::from_secs(24 * 60 * 60));
    let ext: String = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    for n in 0..100u32 {
        let path = dir.join(format!("paste-{stamp}-{n}.{ext}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(data)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free file name"))
}

/// Before an agent starts (`claude`, `codex`): its status hooks, so a host where the agent
/// was installed after Thurm reports Working / Needs input without `thurm hooks install`.
/// Only adds our entries (a backup of the file is kept); `agents.install_hooks = false`
/// turns it off.
fn ensure_hooks(command: Option<&[String]>) {
    let Some(agent) = command.and_then(thurm_config::hooks::agent_for_command) else {
        return;
    };
    let path = thurm_config::hooks::settings_path(agent);
    match thurm_config::hooks::ensure_at(agent, &path) {
        Ok(true) => log::info!("installed {} hooks in {}", agent.name, path.display()),
        Ok(false) => {}
        Err(e) => log::warn!("{} hooks: {e}", agent.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_backlog_counts_bytes() {
        let (tx, _rx) = unbounded();
        let c = Client {
            id: 1,
            tx,
            ui: AtomicBool::new(false),
            name: Mutex::new(String::new()),
            queued: std::sync::atomic::AtomicUsize::new(0),
        };
        // A few big outputs are a backlog, not only thousands of messages.
        for _ in 0..70 {
            c.send(ServerMessage::Event(Event::Output {
                pane: 1,
                data: vec![0; 256 * 1024],
            }));
        }
        assert!(c.backlogged() && !c.caught_up());
        c.queued.store(0, Ordering::Relaxed);
        assert!(!c.backlogged());
    }

    #[test]
    fn old_temporary_files_are_pruned() {
        let dir = std::env::temp_dir().join(format!("thurm-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.png");
        let new = dir.join("new.png");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&new, "x").unwrap();
        let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
        prune_old_files(&dir, Duration::from_secs(24 * 60 * 60));
        assert!(!old.exists() && new.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
