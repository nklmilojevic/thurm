//! Daemon state: panes, clients, frame pump, monitor loop and request handling.

#[path = "active_size.rs"]
mod active_size;
mod agent_prompt;
mod focus;
mod layouts;
#[path = "terminal_attach.rs"]
mod terminal_attach;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::{Mutex, RwLock};

use thurm_config::{AgentDef, Config, EventHook, HookEvent, NoticeState, ProgramDef};
use thurm_proto::{
    AgentState, AgentStatus, CreatePane, Event, Layout, PROTOCOL_VERSION, PaneId, PaneInfo,
    PaneSize, ProgramRecord, Request, Response, ServerMessage, WaitCondition, WaitOutcome,
};
use thurm_term::{EngineConfig, MouseOutcome, TermEvent, Terminal};

use crate::agents::{self, AgentTracker, AiWant};
use crate::ai;
use crate::persist::{self, PaneSnapshot, SessionSnapshot, Store};
use crate::procinfo;
use crate::pty::{self, Pty};
use crate::shell;
use crate::upgrade;
use crate::user_exec;

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

/// Notifications program status reports may cause per pane within `PROGRAM_ALERT_WINDOW`.
const PROGRAM_ALERTS: usize = 3;
const PROGRAM_ALERT_WINDOW: Duration = Duration::from_secs(10);

/// How long before the daemon notices a SIGTERM a pane whose shell died still counts for the
/// session save. Quitting the app signals the daemon and the shells at once, and the reader
/// can see the shell go before the signal handler has run (and the main thread notices the
/// signal up to 100ms after that).
const TEARDOWN_GRACE: Duration = Duration::from_millis(500);

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
        notify: (String, String),
        sound: String,
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
    /// The iOS app ([`thurm_proto::CAP_MOBILE`]).
    pub mobile: AtomicBool,
    /// The push device the iOS app registered over this connection: while it is connected,
    /// that phone hears notices live instead of on its lock screen.
    pub push_device: Mutex<Option<String>>,
    /// The connection, to drop it from outside its reader (a revoked phone).
    pub socket: Mutex<Option<std::os::unix::net::UnixStream>>,
    /// Its panes follow the active client's size ([`thurm_proto::CAP_ACTIVE_SIZE`]).
    pub active_size: AtomicBool,
    /// `thurm events` ([`thurm_proto::CAP_STATE_EVENTS`]): gets every `PaneInfo`.
    pub state_events: AtomicBool,
    /// A UI that carries out the newer UI commands ([`thurm_proto::CAP_UI_LAYOUT`]).
    pub ui_layout: AtomicBool,
    /// Who connected: a local peer, or a tailnet caller the Tailscale helper admitted.
    pub principal: Mutex<Option<thurm_proto::Principal>>,
    pub name: Mutex<String>,
    /// Approximate bytes queued in `tx` (see [`message_weight`]); the writer subtracts what it
    /// wrote.
    pub queued: std::sync::atomic::AtomicUsize,
    /// Messages sent but not yet flushed to the socket; the writer subtracts them after a flush.
    pub unflushed: std::sync::atomic::AtomicUsize,
}

impl Client {
    pub fn send(&self, msg: ServerMessage) {
        self.queued
            .fetch_add(message_weight(&msg), Ordering::Relaxed);
        self.unflushed.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(msg).is_err() {
            self.queued.store(0, Ordering::Relaxed);
            self.unflushed.store(0, Ordering::Release);
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
    /// Per client, the size it shows the pane at (see `active_size`).
    pub wanted: HashMap<u64, active_size::Wanted>,
    /// The client whose size the pane has now.
    pub size_owner: Option<u64>,
    pub agent: AgentTracker,
    osc_cwd: Option<String>,
    command_started: Option<Instant>,
    command_line: Option<String>,
    /// The unterminated end of what was parsed (an escape sequence cut by a read, or part of a
    /// UTF-8 character): an in-place upgrade replays it into the new terminal ahead of the
    /// rest, which is still in the kernel.
    carry: Vec<u8>,
    /// The same for the stream forwarded to clients: an attaching client's copy gets it after
    /// the state, so the output that follows finishes the sequence there instead of showing
    /// its end as text.
    forwarded_carry: Vec<u8>,
    pending_input: Option<(Instant, Vec<u8>)>,
    shell_integration_seen: bool,
    /// Last git probe: when, for which directory, and whether one is in flight.
    git_probe: Option<(Instant, String)>,
    git_pending: bool,
    /// Progress the program reported with OSC 9;4 (program status progress shows otherwise).
    osc_progress: Option<thurm_proto::Progress>,
    /// When program status reports last caused notifications (see `PROGRAM_ALERTS`).
    program_alerts: Vec<Instant>,
    /// The foreground program's display name, for titles (`programs.names`).
    program_name: Option<String>,
    /// The title's command word, when it starts the foreground program (`vim` for Neovim).
    program_command: Option<String>,
    /// The last `names_program` answer: for which word and program.
    command_check: Option<((String, String), bool)>,
}

impl PaneState {
    /// The state an attaching client rebuilds the terminal from, ending inside the sequence
    /// the forwarded output is in the middle of.
    fn attach_state(&mut self) -> Vec<u8> {
        let mut state = self.term.serialize_state();
        state.extend_from_slice(&self.forwarded_carry);
        state
    }
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
    /// What forwarding `carry` gives clients (see `PaneState::forwarded_carry`).
    forwarded_carry: Vec<u8>,
}

pub struct Daemon {
    pub config: RwLock<Config>,
    engine: RwLock<EngineConfig>,
    /// System appearance reported by the GUI (dark until told otherwise).
    dark: AtomicBool,
    agent_defs: RwLock<Vec<AgentDef>>,
    program_defs: RwLock<Vec<ProgramDef>>,
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
    /// Agent notices to paired phones.
    push: crate::push::Pusher,
    /// Serving over Tailscale.
    pub tailnet: Arc<crate::tailnet::Tailnet>,
    /// Panes with an agent notice nobody has read yet (see `pane_read`).
    unread: Mutex<HashSet<PaneId>>,
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
    /// The panes UIs focused (`thurm focused`).
    focus: Mutex<focus::FocusLog>,
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
        persist::remove_old_scrollback(&state_dir);
        let store = config
            .session
            .persist
            .then(|| Store::new(state_dir.clone()));
        let git_channel = crossbeam_channel::bounded(256);
        let ai_channel = crossbeam_channel::bounded(64);
        let push = crate::push::Pusher::new(&state_dir, !config.push.relay.is_empty());
        let tailnet = crate::tailnet::Tailnet::new(socket.clone(), state_dir.join("tailscale"));
        Arc::new(Daemon {
            engine: RwLock::new(EngineConfig::from_config(&config, true)),
            dark: AtomicBool::new(true),
            agent_defs: RwLock::new(config.agent_defs()),
            program_defs: RwLock::new(config.program_defs()),
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
            push,
            tailnet,
            unread: Mutex::new(HashSet::new()),
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
            focus: Mutex::new(focus::FocusLog::default()),
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

    /// Sends `event` to every client (`ui_only`: the apps; the iOS app too, except for UI
    /// commands meant for a desktop window). A pane's state and notifications are sent with the
    /// pane locked, so they arrive in the order they happened. Pane lock, then client lock, never
    /// the other way round.
    fn broadcast(&self, event: Event, ui_only: bool) {
        let clients: Vec<_> = self.clients.lock().values().cloned().collect();
        // A phone with the app open hears it live; otherwise its lock screen does.
        let open = || -> Vec<String> {
            clients
                .iter()
                .filter(|c| c.mobile.load(Ordering::Relaxed))
                .filter_map(|c| c.push_device.lock().clone())
                .collect()
        };
        match &event {
            // Agent notices only, which carry their state's sound (`[notifications.<state>]`): a
            // program's own (OSC 9 / 99 / 777) and a finished command stay on the desktop.
            Event::Notify {
                pane,
                title,
                permission,
                sound: Some(_),
                ..
            } => {
                self.unread.lock().insert(*pane);
                self.push.notify(
                    crate::push::Notice {
                        pane: *pane,
                        title: title.clone(),
                        permission: *permission,
                    },
                    &open(),
                );
            }
            Event::PaneInfo(info) => self.push.agent_changed(
                info.id,
                crate::push::activity_state(info, crate::push::now()),
                &open(),
            ),
            // Phones with the app open take the pane's notices down themselves.
            Event::PaneRead { pane } => self.push.read(*pane, &open()),
            Event::PaneClosed { pane } => {
                // Its notices would open nothing.
                self.unread.lock().remove(pane);
                self.push.read(*pane, &open());
                self.push.agent_changed(*pane, None, &[]);
            }
            // A held pane outlives its process, but not its agent's notices (the apps take
            // theirs down on exit too).
            Event::PaneExited { pane, .. } => {
                self.unread.lock().remove(pane);
                self.push.read(*pane, &open());
            }
            _ => {}
        }
        let desktop_only = matches!(event, Event::Ui(_));
        let pane_info = matches!(event, Event::PaneInfo(_));
        for c in clients {
            let app =
                c.ui.load(Ordering::Relaxed) || (!desktop_only && c.mobile.load(Ordering::Relaxed));
            if !ui_only || app || (pane_info && c.state_events.load(Ordering::Relaxed)) {
                c.send(ServerMessage::Event(event.clone()));
            }
        }
    }

    /// The user read `pane` on some device (focused it, typed in it, answered its prompt): an
    /// agent notice still standing for it comes down everywhere, the desktops' and the phones'.
    /// Only the first read after a notice says so.
    fn pane_read(&self, pane: PaneId) {
        if self.unread.lock().remove(&pane) {
            self.broadcast(Event::PaneRead { pane }, true);
        }
    }

    fn has_ui_clients(&self) -> bool {
        self.clients
            .lock()
            .values()
            .any(|c| c.ui.load(Ordering::Relaxed))
    }

    /// A [`thurm_proto::CAP_UI_LAYOUT`] command, to the desktop UIs that carry it out. Refused
    /// when none does, or when an older one shows a pane it changes: that UI would leave the
    /// pane as it is.
    fn layout_ui(&self, cmd: thurm_proto::UiCommand) -> Result<Response, String> {
        let desktops: Vec<Arc<Client>> = self
            .clients
            .lock()
            .values()
            .filter(|c| c.ui.load(Ordering::Relaxed) && !c.mobile.load(Ordering::Relaxed))
            .cloned()
            .collect();
        let (capable, older): (Vec<_>, Vec<_>) = desktops
            .into_iter()
            .partition(|c| c.ui_layout.load(Ordering::Relaxed));
        let shown_by_older = cmd.layout_panes().into_iter().any(|p| {
            self.pane(p).is_some_and(|p| {
                let st = p.state.lock();
                older.iter().any(|c| st.subscribers.contains_key(&c.id))
            })
        });
        if capable.is_empty() || shown_by_older {
            return Err(thurm_proto::UI_TOO_OLD.into());
        }
        for c in capable {
            c.send(ServerMessage::Event(Event::Ui(cmd.clone())));
        }
        Ok(Response::Ok)
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
            mobile: AtomicBool::new(false),
            push_device: Mutex::new(None),
            socket: Mutex::new(None),
            active_size: AtomicBool::new(false),
            state_events: AtomicBool::new(false),
            ui_layout: AtomicBool::new(false),
            principal: Mutex::new(None),
            name: Mutex::new(String::new()),
            queued: std::sync::atomic::AtomicUsize::new(0),
            unflushed: std::sync::atomic::AtomicUsize::new(0),
        });
        self.clients.lock().insert(id, client.clone());
        *self.last_activity.lock() = Instant::now();
        (client, rx)
    }

    /// Waits up to `timeout` for every client's sent messages to reach its socket, so the reply
    /// to a shutdown request is not lost when the process exits right after.
    pub fn wait_clients_flushed(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let pending = self
                .clients
                .lock()
                .values()
                .any(|c| c.unflushed.load(Ordering::Acquire) > 0);
            if !pending {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn remove_client(&self, id: u64) {
        log::info!("client {id} gone");
        self.clients.lock().remove(&id);
        self.focus.lock().client_gone(id);
        for p in self.all_panes() {
            let mut st = p.state.lock();
            st.subscribers.remove(&id);
            self.release_terminal(&mut st, id);
            self.forget_size(&mut st, id);
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

    /// Create a pane. `restore` carries the id when restoring a session.
    pub fn create_pane(
        self: &Arc<Self>,
        req: CreatePane,
        restore: Option<PaneId>,
    ) -> anyhow::Result<PaneId> {
        let id = match restore {
            Some(id) => {
                self.next_pane.fetch_max(id + 1, Ordering::Relaxed);
                id
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
        if restored {
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
                forwarded_carry: Vec::new(),
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
                wanted: HashMap::new(),
                size_owner: None,
                agent: AgentTracker::default(),
                osc_cwd: p.osc_cwd,
                command_started: None,
                command_line: None,
                carry: p.carry,
                forwarded_carry: p.forwarded_carry,
                pending_input: None,
                shell_integration_seen: p.shell_integration_seen,
                git_probe: None,
                git_pending: false,
                osc_progress: None,
                program_alerts: Vec::new(),
                program_name: None,
                program_command: None,
                command_check: None,
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
                // Forwarded even without subscribers: one may attach inside a sequence.
                let data = st.term.advance_forward(slice);
                update_carry(&mut st.forwarded_carry, &data);
                if !st.subscribers.is_empty() {
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
        let state = need_state.then(|| st.attach_state());
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
        let state = st.attach_state();
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
        let state = st.attach_state();
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
                                sound: None,
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
                        if st.osc_progress.take().is_some() {
                            st.info.progress = None;
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
                        let started = st.command_started.take();
                        if let Some(start) = started {
                            let secs = cfg.command_finished_secs;
                            if cfg.enabled
                                && secs > 0
                                && start.elapsed() >= Duration::from_secs(secs)
                            {
                                // Only the command's name: its arguments can hold secrets.
                                let what = st
                                    .command_line
                                    .as_deref()
                                    .and_then(command_name)
                                    .unwrap_or("Command");
                                let status = match code {
                                    Some(0) | None => "finished".to_owned(),
                                    Some(c) => format!("failed (exit {c})"),
                                };
                                outgoing.push(Event::Notify {
                                    pane: pane.id,
                                    title: format!("{what} {status}"),
                                    body: format!("after {}s", start.elapsed().as_secs()),
                                    permission: None,
                                    sound: None,
                                });
                            }
                        }
                        let secs = started.map_or(0, |s| s.elapsed().as_secs());
                        self.run_hooks(
                            HookEvent::CommandFinished,
                            &st.info,
                            serde_json::json!({
                                "exit_code": code,
                                "duration_secs": secs,
                                "command": st.command_line,
                            }),
                            secs,
                        );
                        st.command_line = None;
                    }
                    TermEvent::Progress(p) => {
                        st.osc_progress = p;
                        st.info.progress =
                            p.or_else(|| agents::program::progress(&st.info.programs));
                        info_changed = true;
                    }
                    // Each report in turn: a request answered within the same read still
                    // notifies.
                    TermEvent::ProgramStatus(records) => {
                        outgoing.extend(self.sync_programs(pane.id, &mut st, records));
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
        let (hold, info) = {
            let mut st = pane.state.lock();
            st.info.alive = false;
            st.info.exit_code = code;
            if st.term.program_exited() {
                let records = st.term.program_status();
                let events = self.sync_programs(pane.id, &mut st, records);
                self.broadcast(Event::PaneInfo(st.info.clone()), false);
                for ev in events {
                    self.broadcast(ev, true);
                }
            }
            if st.hold {
                let msg = match code {
                    Some(c) => format!("\r\n\x1b[2m[process exited with code {c}]\x1b[0m"),
                    None => "\r\n\x1b[2m[process exited]\x1b[0m".to_owned(),
                };
                st.term.print(&msg);
            }
            (st.hold, st.info.clone())
        };
        if self.pane(pane.id).is_none() {
            // Closed explicitly; ClosePane already notified clients.
            return;
        }
        log::info!("pane {} exited ({code:?})", pane.id);
        self.run_hooks(
            HookEvent::PaneExited,
            &info,
            serde_json::json!({ "exit_code": code }),
            0,
        );
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
                    sound,
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
                        if !current {
                            return Vec::new();
                        }
                        let (title, body) = notify;
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
                            sound: Some(sound.clone()),
                        }]
                    });
                }
            }
        }
    }

    /// The pane's program status records are now `records` (OSC 7501, an exit, an
    /// acknowledgement): re-derive its progress and agent state from them. Returns the
    /// notifications to send; the caller sends the pane's info.
    fn sync_programs(
        &self,
        pane: PaneId,
        st: &mut PaneState,
        records: Vec<ProgramRecord>,
    ) -> Vec<Event> {
        let before = st.info.agent.clone();
        if !self.apply_programs(st, records) {
            return Vec::new();
        }
        let mut events =
            self.agent_notifications(pane, st, before.as_ref(), st.info.agent.as_ref());
        // A program can change its status as fast as it writes.
        let now = Instant::now();
        st.program_alerts
            .retain(|t| now.duration_since(*t) < PROGRAM_ALERT_WINDOW);
        events.retain(|ev| {
            if !matches!(ev, Event::Notify { .. }) {
                return true;
            }
            if st.program_alerts.len() >= PROGRAM_ALERTS {
                return false;
            }
            st.program_alerts.push(now);
            true
        });
        events
    }

    /// Like [`Daemon::sync_programs`], without notifications. Returns whether the agent state
    /// changed.
    fn apply_programs(&self, st: &mut PaneState, records: Vec<ProgramRecord>) -> bool {
        st.info.programs = records;
        st.info.progress = st
            .osc_progress
            .or_else(|| agents::program::progress(&st.info.programs));
        st.agent
            .set_programs(&st.info.programs, &self.agent_defs.read());
        let Some(new) = st.agent.refresh() else {
            return false;
        };
        st.info.agent = new;
        st.info.title = pane_title(st, st.info.foreground.as_ref());
        true
    }

    /// The user is back at the pane: what programs reported having finished is no longer news.
    fn acknowledge_programs(&self, pane: PaneId, st: &mut PaneState) {
        if st.term.acknowledge_program_status() {
            let records = st.term.program_status();
            let events = self.sync_programs(pane, st, records);
            self.broadcast(Event::PaneInfo(st.info.clone()), false);
            for ev in events {
                self.broadcast(ev, true);
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
            let (idle_after, interval, detect, ai_cfg, programs) = {
                let c = self.config.read();
                (
                    Duration::from_millis(c.agents.idle_after_ms),
                    Duration::from_secs(c.session.snapshot_interval_secs.max(1)),
                    c.agents.detect,
                    c.ai.clone(),
                    c.programs.clone(),
                )
            };
            let defs = self.agent_defs.read().clone();
            let program_defs = self.program_defs.read().clone();
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
                        let pane_st = &mut *st;
                        pane_st.agent.saw_title(pane_st.term.title());
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
                    let executable = fg.as_ref().map(procinfo::executable);
                    let program = fg
                        .as_ref()
                        .zip(executable.as_deref())
                        .filter(|_| programs.detect)
                        .and_then(|(p, exe)| thurm_config::identify(&program_defs, exe, &p.argv));
                    st.info.program = program.map(ProgramDef::to_match);
                    st.program_name = program.filter(|_| programs.names).map(|d| d.name.clone());
                    let word = st.term.title().map(command_word).unwrap_or("").to_owned();
                    st.program_command =
                        match (program.filter(|_| programs.names), &fg, &executable) {
                            // The command line the shell ran: its word started the program, an
                            // alias or a function included (`lg` for lazygit). Only a program the
                            // shell itself runs: lazygit's editor is not `lg`.
                            (Some(_), Some(p), Some(_))
                                if st
                                    .command_line
                                    .as_deref()
                                    .is_some_and(|c| starts_command(c, &word))
                                    && st
                                        .info
                                        .pid
                                        .is_some_and(|sh| procinfo::parent(p.pid) == Some(sh)) =>
                            {
                                Some(word)
                            }
                            (Some(def), Some(p), Some(exe)) => {
                                let key = (word.clone(), def.id.clone());
                                let named = match &st.command_check {
                                    Some((k, named)) if *k == key => *named,
                                    _ => {
                                        // The shell's PATH when it started, where it looked the word up.
                                        let path = st
                                            .info
                                            .pid
                                            .and_then(|pid| procinfo::env_var(pid, "PATH"))
                                            .or_else(|| std::env::var("PATH").ok());
                                        let named = names_program(
                                            &word,
                                            def,
                                            p,
                                            exe,
                                            &program_defs,
                                            path.as_deref(),
                                        );
                                        st.command_check = Some((key, named));
                                        named
                                    }
                                };
                                named.then_some(word)
                            }
                            _ => None,
                        };
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
                let _ = self.save_session();
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

    /// Writes the layout and each pane's cwd, command and agent. Scrollback stays in memory:
    /// it never reaches the disk, so a restart restores the panes without their history.
    pub fn save_session(&self) -> std::io::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let _saving = self.save_lock.lock();
        // Changes from here on (a hook during the save) make the session dirty again.
        let was_dirty = self.session_dirty.swap(false, Ordering::Relaxed);
        let mut panes = Vec::new();
        let teardown = self.teardown_exits();
        let mut saving = self.all_panes();
        for p in &teardown {
            if !saving.iter().any(|s| s.id == p.id) {
                saving.push(p.clone());
            }
        }
        saving.sort_by_key(|p| p.id);
        for pane in saving {
            let st = pane.state.lock();
            if !st.info.alive && !teardown.iter().any(|p| p.id == pane.id) {
                continue;
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
        store.save(&snap).inspect_err(|e| {
            log::warn!("failed to save session: {e}");
            if was_dirty {
                self.session_dirty.store(true, Ordering::Relaxed);
            }
        })
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
                agent_hook: st.agent.hook_handoff(),
                terminal_size_after_detach: st.terminal_attachment.map(|(_, size)| size.into()),
                programs: st.term.program_status_handoff(),
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
            tailnet: self.tailnet.handoff(),
        }
    }

    /// Starts serving over Tailscale as configured, taking over the previous image's helper
    /// (`adopt`: its pid and control socket) after an in-place upgrade.
    pub fn start_tailnet(self: &Arc<Self>, adopt: Option<(i32, RawFd)>) {
        let weak = Arc::downgrade(self);
        self.tailnet.on_change(move |status| {
            if let Some(d) = weak.upgrade() {
                d.broadcast(Event::TailscaleStatusChanged(status), false);
            }
        });
        if let Some((pid, fd)) = adopt {
            self.tailnet.adopt(pid, fd);
        }
        let cfg = self.config.read().tailscale.clone();
        self.tailnet.configure(&cfg);
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
            let mut forwarded_carry = Vec::new();
            if !pending.is_empty() {
                update_carry(&mut forwarded_carry, &term.advance_forward(&pending));
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
            term.restore_program_status(p.programs.clone());
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
                    forwarded_carry,
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
                        if let Some(hook) = p.agent_hook {
                            st.agent.restore_hook_handoff(hook);
                        }
                        if let Some(prompt) = p.agent_prompt {
                            st.agent.restore_pending_prompt(prompt);
                        }
                        st.info.agent = st.agent.state().cloned();
                        // Old news: notified by the image we replaced.
                        let records = st.term.program_status();
                        self.apply_programs(&mut st, records);
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
            match self.create_pane(req, Some(p.id)) {
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
                        // "The last one here" is only this pane's while no other pane ran
                        // the same agent in the same directory.
                        let shared = snap
                            .panes
                            .iter()
                            .filter(|o| o.agent == p.agent && o.cwd == p.cwd)
                            .count()
                            > 1;
                        // The exact session when hooks reported one, else "the last one here".
                        let resume =
                            def.and_then(|d| match (&d.resume_session, &p.agent_session) {
                                (Some(cmd), Some(id)) => {
                                    Some(cmd.iter().map(|a| a.replace("{session}", id)).collect())
                                }
                                _ if shared => {
                                    if d.resume.is_some()
                                        && let Some(pane) = self.pane(id)
                                    {
                                        pane.state.lock().term.print(&format!(
                                            "\r\n[{} not resumed: its session is unknown and \
                                             other panes ran it here]\r\n",
                                            d.name
                                        ));
                                    }
                                    None
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
        let shutting_down = matches!(req, Request::Shutdown { .. });
        let result = match req {
            // Answered once no post to the forgotten devices can still be on its way, which can
            // take the relay's timeout: off this client's request thread.
            req @ (Request::UnregisterPushDevice { .. } | Request::RevokePhone { .. }) => {
                let daemon = self.clone();
                let client = client.clone();
                std::thread::spawn(move || {
                    let result = daemon.dispatch(&client, req);
                    daemon.push.settle();
                    if id != 0 {
                        client.send(ServerMessage::Response { id, result });
                    }
                });
                return;
            }
            // The Tailscale helper can take a while (starting the node, signing out).
            req @ (Request::TailscaleEnable { .. }
            | Request::TailscaleDisable
            | Request::TailscaleLogin
            | Request::TailscaleLogout
            | Request::TailscaleRename { .. }) => {
                let daemon = self.clone();
                let client = client.clone();
                std::thread::spawn(move || {
                    let result = daemon.dispatch(&client, req);
                    if id != 0 {
                        client.send(ServerMessage::Response { id, result });
                    }
                });
                return;
            }
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
        let ok = result.is_ok();
        if id != 0 {
            client.send(ServerMessage::Response { id, result });
        }
        // Only once the reply is queued: the main loop exits soon after it sees the flag, and
        // waits only for replies already queued (see `wait_clients_flushed`).
        if shutting_down && ok {
            self.shutdown.store(true, Ordering::Release);
        }
    }

    /// Whether an agent going from `before` to `after` is news: it entered another state of the
    /// notification matrix, or showed a permission prompt it did not show before.
    fn is_agent_news(before: Option<&AgentState>, after: &AgentState) -> bool {
        let state = |a: &AgentState| NoticeState::of(a.status, a.permission.is_some());
        let new_prompt =
            after.permission.is_some() && after.permission != before.and_then(|b| b.permission);
        before.map(state) != Some(state(after)) || new_prompt
    }

    /// Notifications for an agent entering a state of the notification matrix
    /// (`[notifications.<state>]`): needs input, a permission prompt, a finished turn, a failure,
    /// or (if enabled) working or idle. With `ai.notifications` the model first says what the
    /// agent asks for or did (shown in the agent switcher too), and the notification waits for it.
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
        if !Self::is_agent_news(before, a) {
            return Vec::new();
        }
        let state = NoticeState::of(a.status, a.permission.is_some());
        let mut info = st.info.clone();
        info.agent = Some(a.clone());
        self.run_hooks(HookEvent::Agent(state), &info, serde_json::json!({}), 0);
        let notice = n.notice(state);
        if !n.enabled || !notice.notify {
            return Vec::new();
        }
        let done = matches!(state, NoticeState::Finished | NoticeState::Failed);
        let body = match state {
            NoticeState::Waiting | NoticeState::Blocked => a
                .message
                .clone()
                .unwrap_or_else(|| "is waiting for your input".into()),
            // A program says what it finished; a hooked turn says how long it took.
            NoticeState::Finished => match (&a.message, a.turn_ms) {
                (Some(m), _) if st.agent.program_reported() => m.clone(),
                (_, Some(ms)) => format!("finished after {}", agents::human_duration(ms)),
                (_, None) => "finished".into(),
            },
            NoticeState::Failed => a.message.clone().unwrap_or_else(|| "failed".into()),
            NoticeState::Working => "is working".into(),
            NoticeState::Idle => "is idle".into(),
        };
        let notify = (a.name.clone(), body);
        // A program that reports its status says itself what it wants or did.
        if ai_cfg.notifications()
            && (done || matches!(state, NoticeState::Waiting | NoticeState::Blocked))
            && !st.agent.program_reported()
            && !st.info.password_input
            && self.ai.unavailable().is_none()
        {
            let job = AiJob::Detail {
                pane,
                episode: st.agent.episode(),
                done,
                agent: a.name.clone(),
                hint: a.message.clone(),
                screen: agents::tail(&st.term.screen_text(), AI_SCREEN_LINES),
                notify: notify.clone(),
                sound: notice.sound.clone(),
            };
            if self.ai_tx.try_send(job).is_ok() {
                return Vec::new();
            }
        }
        let (title, body) = notify;
        vec![Event::Notify {
            pane,
            title,
            body,
            permission: a.permission.filter(|_| !done),
            sound: Some(notice.sound.clone()),
        }]
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
                capabilities,
            } => {
                if !thurm_proto::protocol_compatible(version) {
                    return Err(format!(
                        "protocol mismatch: daemon {PROTOCOL_VERSION}, client {version}"
                    ));
                }
                let has = |cap: &str| capabilities.iter().any(|c| c == cap);
                client.ui.store(ui, Ordering::Relaxed);
                // Capabilities belong to the client's build: a later Hello on the same connection
                // (the Mac app asks again to read the daemon's version, without listing them)
                // must not drop them.
                if has(thurm_proto::CAP_MOBILE) {
                    client.mobile.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_ACTIVE_SIZE) {
                    client.active_size.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_STATE_EVENTS) {
                    client.state_events.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_UI_LAYOUT) {
                    client.ui_layout.store(true, Ordering::Relaxed);
                }
                log::info!(
                    "client {} hello: {name} (protocol {version}{}{}{})",
                    client.id,
                    if has(thurm_proto::CAP_MOBILE) {
                        ", mobile"
                    } else {
                        ""
                    },
                    if has(thurm_proto::CAP_ACTIVE_SIZE) {
                        ", active size"
                    } else {
                        ""
                    },
                    if has(thurm_proto::CAP_STATE_EVENTS) {
                        ", state events"
                    } else {
                        ""
                    },
                );
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
                self.resize_for(client, pane, size)?;
                Ok(Response::Ok)
            }
            Request::Input { pane, data } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    self.used_by(client, st);
                    if !data.is_empty() {
                        st.agent.prompt.interrupted();
                    }
                    st.agent.user_answer(&data);
                    self.acknowledge_programs(pane, st);
                    if !data.is_empty() {
                        self.pane_read(pane);
                    }
                    p.write(data);
                })?;
                Ok(Response::Ok)
            }
            Request::Paste { pane, text } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    self.used_by(client, st);
                    if !text.is_empty() {
                        st.agent.prompt.interrupted();
                    }
                    st.agent.user_input();
                    self.acknowledge_programs(pane, st);
                    if !text.is_empty() {
                        self.pane_read(pane);
                    }
                    p.write(st.term.paste(&text));
                })?;
                Ok(Response::Ok)
            }
            Request::Key { pane, key } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    self.used_by(client, st);
                    let bytes = st.term.key(&key);
                    if !bytes.is_empty() {
                        st.agent.prompt.interrupted();
                        st.agent.user_answer(&bytes);
                        self.acknowledge_programs(pane, st);
                        self.pane_read(pane);
                        p.write(bytes);
                    }
                })?;
                Ok(Response::Ok)
            }
            Request::Mouse { pane, event } => {
                let copy_on_select = self.config.read().terminal.copy_on_select;
                let text = self.with_input_pane(client.id, pane, |p, st| {
                    self.used_by(client, st);
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
                    if focused {
                        self.used_by(client, st);
                    }
                    if let Some(b) = st.term.focus(focused) {
                        if !b.is_empty() {
                            st.agent.prompt.interrupted();
                        }
                        p.write(b);
                    }
                    self.focus
                        .lock()
                        .focus(client.id, pane, focused, Instant::now());
                    // Looking at a finished agent acknowledges it (Done → Idle).
                    if focused {
                        self.pane_read(pane);
                        st.agent.acknowledge();
                        self.acknowledge_programs(pane, st);
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
                    let state = st.attach_state();
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
                let result = self
                    .apply_layout(client.id, json, timeout_ms)
                    .unwrap_or_else(|error| thurm_proto::LayoutResult {
                        committed: false,
                        error: Some(error),
                    });
                Ok(Response::LayoutResult(result))
            }
            Request::RegisterPushDevice {
                device,
                relay,
                secret,
                host,
                name,
            } => {
                self.push.register(crate::push::Device {
                    device: device.clone(),
                    relay,
                    secret,
                    host,
                    name,
                })?;
                *client.push_device.lock() = Some(device);
                Ok(Response::Ok)
            }
            Request::UnregisterPushDevice { device } => {
                self.push.unregister(&device)?;
                Ok(Response::Ok)
            }
            Request::RevokePhone { name } => {
                let gone = self.push.revoke_named(&name)?;
                // Revoking its key leaves an ssh session already open, and which phone a
                // connection is (the key it came in with) is not known here: drop them all. The
                // others reconnect; the revoked one can no longer sign in.
                let clients: Vec<_> = self.clients.lock().values().cloned().collect();
                for c in clients.iter().filter(|c| c.mobile.load(Ordering::Relaxed)) {
                    log::info!("client {}: a phone was revoked; dropping it", c.id);
                    if let Some(s) = c.socket.lock().as_ref() {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                    }
                }
                log::info!("revoked {name}: {} push device(s) forgotten", gone.len());
                Ok(Response::Ok)
            }
            Request::FollowAgent {
                device,
                pane,
                token,
            } => {
                let state = self.with_pane(pane, |_, st| {
                    crate::push::activity_state(&st.info, crate::push::now())
                })?;
                log::info!("pane {pane}: a live activity follows it (device {device:.6}…)");
                self.push.follow(&device, pane, token, state)?;
                *client.push_device.lock() = Some(device);
                Ok(Response::Ok)
            }
            Request::Ping => Ok(Response::Ok),
            Request::TailscaleStatus => Ok(Response::TailscaleStatus(self.tailnet.status())),
            Request::TailscaleEnable {
                hostname,
                allow_tailnet_members,
                advertise_tags,
                auth_key,
            } => self
                .tailnet
                .enable(hostname, allow_tailnet_members, advertise_tags, auth_key)
                .map(Response::TailscaleStatus),
            Request::TailscaleDisable => self.tailnet.disable().map(Response::TailscaleStatus),
            Request::TailscaleLogin => self.tailnet.login().map(Response::TailscaleStatus),
            Request::TailscaleLogout => self.tailnet.logout().map(Response::TailscaleStatus),
            Request::TailscaleRename { hostname } => {
                self.tailnet.rename(hostname).map(Response::TailscaleStatus)
            }
            Request::Whoami => Ok(Response::Whoami(
                client.principal.lock().clone().unwrap_or_default(),
            )),
            Request::UnfollowAgent { device, pane } => {
                self.push.unfollow(&device, pane)?;
                Ok(Response::Ok)
            }
            Request::CommitLayout { request_id } => {
                self.commit_layout(client.id, request_id)?;
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
                if cmd.needs_ui_layout() {
                    return self.layout_ui(cmd);
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
            Request::FocusHistory => {
                let at = Instant::now();
                let history = self.focus.lock().history(|p| self.pane(p).is_some(), at);
                Ok(Response::FocusHistory(history))
            }
            Request::PaneModes { pane } => {
                use thurm_term::mode::TermMode;
                let m = self.with_pane(pane, |_, st| st.term.mode())?;
                Ok(Response::PaneModes(thurm_proto::PaneModes {
                    alt_screen: m.contains(TermMode::ALT_SCREEN),
                    mouse: m.intersects(TermMode::MOUSE_MODE),
                    bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
                    focus_reports: m.contains(TermMode::FOCUS_IN_OUT),
                }))
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
                // `handle` sets `shutdown` after queuing this reply.
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
                    st.agent.saw_title(st.term.title());
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
                        self.used_by(client, st);
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
                self.pane_read(pane);
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
            Request::RunCommand { name, pane } => self.run_command(&name, pane),
        }
    }

    /// Starts the `[[commands]]` entry `name` for `pane`. Its failure shows as a notification
    /// in that pane, since nothing else would tell the user who picked it.
    fn run_command(self: &Arc<Self>, name: &str, pane: Option<PaneId>) -> Result<Response, String> {
        let command = self
            .config
            .read()
            .command(name)
            .cloned()
            .ok_or_else(|| format!("no command named {name:?} in the config"))?;
        let info = pane
            .and_then(|id| self.pane(id))
            .map(|p| p.state.lock().info.clone());
        let daemon = Arc::downgrade(self);
        let title = command.name.clone();
        user_exec::start(
            user_exec::Launch {
                argv: &command.exec,
                cwd: info.as_ref().and_then(|i| i.cwd.as_deref()),
                env: self.user_exec_env(info.as_ref(), None),
                stdin: None,
            },
            move |error| {
                log::warn!("command {title:?} failed: {error}");
                if let (Some(daemon), Some(pane)) = (daemon.upgrade(), pane) {
                    daemon.broadcast(
                        Event::Notify {
                            pane,
                            title: format!("{title} failed"),
                            body: error,
                            permission: None,
                            sound: None,
                        },
                        true,
                    );
                }
            },
        )
        .map_err(|e| format!("cannot run {:?}: {e}", command.exec[0]))?;
        Ok(Response::Ok)
    }

    /// Runs the `[[on]]` hooks for `event` in the pane `info` describes, with `{"event", "pane",
    /// ...extra}` on their stdin. `secs` is how long a finished command ran, for `min_secs`
    /// (which other events ignore).
    fn run_hooks(&self, event: HookEvent, info: &PaneInfo, extra: serde_json::Value, secs: u64) {
        let hooks: Vec<EventHook> = self
            .config
            .read()
            .hooks(event)
            .filter(|h| event != HookEvent::CommandFinished || secs >= h.min_secs)
            .cloned()
            .collect();
        if hooks.is_empty() {
            return;
        }
        let mut payload = serde_json::json!({ "event": event.name(), "pane": info });
        if let (Some(p), serde_json::Value::Object(extra)) = (payload.as_object_mut(), extra) {
            p.extend(extra);
        }
        let mut stdin = payload.to_string().into_bytes();
        stdin.push(b'\n');
        let env = self.user_exec_env(Some(info), Some(event));
        let cwd = info.cwd.clone();
        // Callers hold the pane's lock: starting a login shell is not for under it.
        std::thread::spawn(move || {
            for hook in hooks {
                let label = format!("[[on]] {} {:?}", hook.event, hook.exec[0]);
                let started = user_exec::start(
                    user_exec::Launch {
                        argv: &hook.exec,
                        cwd: cwd.as_deref(),
                        env: env.clone(),
                        stdin: Some(stdin.clone()),
                    },
                    {
                        let label = label.clone();
                        move |error| log::warn!("{label} failed: {error}")
                    },
                );
                if let Err(e) = started {
                    log::warn!("{label} did not start: {e}");
                }
            }
        });
    }

    /// What a user command learns from its environment: the pane, how to reach this daemon,
    /// and for a hook its event.
    fn user_exec_env(
        &self,
        info: Option<&PaneInfo>,
        event: Option<HookEvent>,
    ) -> Vec<(String, String)> {
        let mut env = vec![(
            thurm_proto::ENV_SOCKET.to_owned(),
            self.socket.display().to_string(),
        )];
        if let Some(i) = info {
            env.push((thurm_proto::ENV_PANE_ID.to_owned(), i.id.to_string()));
        }
        if let Some(e) = event {
            env.push(("THURM_EVENT".to_owned(), e.name().to_owned()));
        }
        env
    }

    pub fn apply_config(&self, cfg: Config) {
        if cfg.ai != self.config.read().ai {
            // Turned off, or on again: ask the model afresh (it may have become available).
            self.ai.stop();
        }
        *self.engine.write() = EngineConfig::from_config(&cfg, self.dark.load(Ordering::Relaxed));
        *self.agent_defs.write() = cfg.agent_defs();
        *self.program_defs.write() = cfg.program_defs();
        self.push.set_enabled(!cfg.push.relay.is_empty());
        self.tailnet.configure(&cfg.tailscale);
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
                    WaitCondition::AgentStatus(want) => st.agent.state().is_some_and(|a| {
                        // Finished either way.
                        a.status == *want
                            || (*want == AgentStatus::Done && a.status == AgentStatus::Error)
                    }),
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
    let program = st
        .program_name
        .as_deref()
        .filter(|_| st.agent.state().is_none());
    if let Some(t) = st.term.title().filter(|t| !t.trim().is_empty()) {
        return match (program, fg) {
            (Some(name), Some(p)) => with_program_name(t, p, name, st.program_command.as_deref()),
            _ => t.to_owned(),
        };
    }
    if let Some(a) = st.agent.state() {
        return a.name.clone();
    }
    if let Some(p) = fg
        && !procinfo::is_shell(&p.name)
    {
        return program.unwrap_or(&p.name).to_owned();
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

/// The command a title starts with, as the shell shows what runs ("nvim" in "nvim ~/src").
fn command_word(title: &str) -> &str {
    let first = title.split_whitespace().next().unwrap_or("");
    first.rsplit('/').next().unwrap_or(first)
}

/// The command the shell's command line `line` runs, past variable assignments (`FOO=1 lg`).
fn command_name(line: &str) -> Option<&str> {
    line.split_whitespace()
        .find(|w| {
            !w.split_once('=')
                .is_some_and(|(k, _)| !k.is_empty() && !k.contains('/'))
        })
        .map(command_word)
}

/// Whether the shell's command line `line` runs `word` (a title's command).
fn starts_command(line: &str, word: &str) -> bool {
    !word.is_empty() && command_name(line) == Some(word)
}

/// Whether `word` (a title's command) starts the foreground program `def`, which runs as
/// `executable`: one of the process' own names, another of the program's commands, or a
/// command on `path` that resolves to the same executable (a `vim` linked to Neovim, or a
/// wrapper script beside the binary it runs).
fn names_program(
    word: &str,
    def: &ProgramDef,
    fg: &thurm_proto::ProcessInfo,
    executable: &str,
    programs: &[ProgramDef],
    path: Option<&str>,
) -> bool {
    if word.is_empty() {
        return false;
    }
    let own = [Some(fg.name.as_str()), fg.argv.first().map(String::as_str)];
    if own.into_iter().flatten().any(|n| command_word(n) == word) {
        return true;
    }
    if thurm_config::identify(programs, word, &[]).is_some_and(|d| d.id == def.id) {
        return true;
    }
    let executable = thurm_config::executable_name(executable);
    path.into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(word))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::canonicalize(p).ok())
        .is_some_and(|p| {
            p.file_name()
                .is_some_and(|n| thurm_config::executable_name(&n.to_string_lossy()) == executable)
        })
}

/// `title` with its leading command word replaced by the foreground program's display name,
/// when that word is the program (`command`, see `names_program`) or the process itself.
fn with_program_name(
    title: &str,
    fg: &thurm_proto::ProcessInfo,
    name: &str,
    command: Option<&str>,
) -> String {
    let (first, rest) = title.split_at(title.find(char::is_whitespace).unwrap_or(title.len()));
    let first_base = first.rsplit('/').next().unwrap_or(first);
    let is_process = [Some(fg.name.as_str()), fg.argv.first().map(String::as_str)]
        .into_iter()
        .flatten()
        .any(|p| p.rsplit('/').next() == Some(first_base));
    if (is_process || command == Some(first_base)) && !first_base.is_empty() {
        format!("{name}{rest}")
    } else {
        title.to_owned()
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

    fn agent(status: AgentStatus, permission: Option<u64>) -> AgentState {
        AgentState {
            name: "Claude Code".into(),
            kind: "claude".into(),
            status,
            session_id: None,
            message: None,
            turn_ms: None,
            turns: 0,
            hooked: false,
            topic: None,
            permission,
        }
    }

    #[test]
    fn a_prompt_going_away_while_still_waiting_is_news() {
        let blocked = agent(AgentStatus::NeedsInput, Some(7));
        let waiting = agent(AgentStatus::NeedsInput, None);
        assert!(Daemon::is_agent_news(Some(&blocked), &waiting));
        assert!(!Daemon::is_agent_news(Some(&waiting), &waiting));
        assert!(Daemon::is_agent_news(Some(&waiting), &blocked));
        // A different prompt while already blocked is news; the same one is not.
        assert!(Daemon::is_agent_news(
            Some(&blocked),
            &agent(AgentStatus::NeedsInput, Some(8))
        ));
        assert!(!Daemon::is_agent_news(Some(&blocked), &blocked));
        let working = agent(AgentStatus::Working, None);
        assert!(Daemon::is_agent_news(Some(&working), &waiting));
        assert!(!Daemon::is_agent_news(
            Some(&working),
            &agent(AgentStatus::Working, None)
        ));
    }

    #[test]
    fn program_name_replaces_the_command_word() {
        let fg = thurm_proto::ProcessInfo {
            pid: 1,
            name: "nvim".into(),
            argv: vec!["/opt/homebrew/bin/nvim".into(), "package.json".into()],
        };
        let t = |title| with_program_name(title, &fg, "Neovim", Some("vim"));
        assert_eq!(t("nvim ~/apps/replay-web"), "Neovim ~/apps/replay-web");
        assert_eq!(t("nvim"), "Neovim");
        assert_eq!(t("/opt/homebrew/bin/nvim x"), "Neovim x");
        // The program's own title is left alone.
        assert_eq!(
            t("package.json (~/apps) - NVIM"),
            "package.json (~/apps) - NVIM"
        );
        assert_eq!(t("nvimx y"), "nvimx y");
        // The command that started Neovim (see names_program).
        assert_eq!(t("vim -R Cargo.toml ~/src"), "Neovim -R Cargo.toml ~/src");
        // Other words stay, another program's included (a title Neovim set).
        assert_eq!(t("cd ~/src"), "cd ~/src");
        assert_eq!(t("git status"), "git status");
    }

    #[test]
    fn command_line_names_its_command() {
        assert!(starts_command("lg", "lg"));
        assert!(starts_command("lg --debug", "lg"));
        assert!(starts_command("FOO=1 BAR=x lg", "lg"));
        assert!(starts_command("~/bin/lg", "lg"));
        assert!(!starts_command("git status", "lg"));
        assert!(!starts_command("", ""));
        assert!(!starts_command("lg", ""));
        assert_eq!(
            command_name("TOKEN=s3cret curl -H 'Authorization: x' url"),
            Some("curl")
        );
        assert_eq!(command_name("FOO=1"), None);
    }

    #[test]
    fn title_commands_name_only_the_foreground_program() {
        let programs = thurm_config::builtin_programs();
        let neovim = programs.iter().find(|d| d.id == "neovim").unwrap();
        let fg = thurm_proto::ProcessInfo {
            pid: 1,
            name: "nvim".into(),
            argv: vec!["/nix/store/x-neovim-unwrapped/bin/nvim".into()],
        };
        // A PATH where vim is a link to a wrapper named nvim (as Nix installs Neovim).
        let dir = std::env::temp_dir().join(format!("thurm-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("nvim"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(dir.join("nvim"), dir.join("bin/vim")).unwrap();
        std::fs::write(dir.join("bin/vi"), "#!/bin/sh\n").unwrap();
        let path = dir.join("bin").display().to_string();
        let names = |word| names_program(word, neovim, &fg, "nvim", &programs, Some(&path));
        assert!(names("nvim"));
        assert!(names("vim"));
        // A vi of its own, another program, or a word no program has.
        assert!(!names("vi"));
        assert!(!names("git"));
        assert!(!names("cd"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn client_backlog_counts_bytes() {
        let (tx, _rx) = unbounded();
        let c = Client {
            id: 1,
            tx,
            ui: AtomicBool::new(false),
            mobile: AtomicBool::new(false),
            push_device: Mutex::new(None),
            socket: Mutex::new(None),
            active_size: AtomicBool::new(false),
            state_events: AtomicBool::new(false),
            ui_layout: AtomicBool::new(false),
            principal: Mutex::new(None),
            name: Mutex::new(String::new()),
            queued: std::sync::atomic::AtomicUsize::new(0),
            unflushed: std::sync::atomic::AtomicUsize::new(0),
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

    /// The main loop exits soon after it sees `shutdown`, waiting only for queued replies, so the
    /// flag must not go up before the reply to the shutdown request is queued.
    #[test]
    fn shutdown_reply_is_queued_before_the_flag() {
        let dir = std::env::temp_dir().join(format!("thurm-shutdown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let (client, rx) = daemon.add_client();
        let watcher = {
            let daemon = daemon.clone();
            let client = client.clone();
            std::thread::spawn(move || {
                while !daemon.shutdown.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                client.unflushed.load(Ordering::Acquire)
            })
        };
        daemon.handle(&client, 7, Request::Shutdown { kill_panes: false });
        assert!(
            watcher.join().unwrap() > 0,
            "shutdown was visible before its reply"
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerMessage::Response {
                id: 7,
                result: Ok(Response::Ok)
            })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An older version's plain-text scrollback goes at start-up even when sessions are not
    /// saved.
    #[test]
    fn old_scrollback_is_removed_without_persistence() {
        let dir = std::env::temp_dir().join(format!("thurm-old-scrollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = dir.join("state").join("scrollback");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("3.ansi"), b"export TOKEN=secret\r\n").unwrap();
        let mut config = Config::default();
        config.session.persist = false;
        let daemon = Daemon::new(
            config,
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(daemon.store.is_none());
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only agent notices go to phones: a program's own notification (OSC 9 / 99 / 777) and a
    /// finished command have no state sound and stay on the desktop.
    #[test]
    fn only_agent_notices_are_pushed() {
        use std::io::{BufRead, BufReader, Read, Write};
        let dir = std::env::temp_dir().join(format!("thurm-push-agent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tx, posts) = unbounded();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                let _ = reader.read_exact(&mut body);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = tx.send(String::from_utf8_lossy(&body).into_owned());
            }
        });
        let mut config = Config::default();
        config.push.relay = "https://push.example.net".into();
        let daemon = Daemon::new(
            config,
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        daemon
            .push
            .register(crate::push::Device {
                device: "d1".into(),
                relay,
                secret: "s".repeat(32),
                host: "studio".into(),
                name: "iPhone".into(),
            })
            .unwrap();
        let notify = |pane, sound: Option<&str>| Event::Notify {
            pane,
            title: "Build".into(),
            body: "done".into(),
            permission: None,
            sound: sound.map(str::to_owned),
        };
        daemon.broadcast(notify(1, None), true);
        daemon.broadcast(notify(2, Some("default")), true);
        let body: serde_json::Value =
            serde_json::from_str(&posts.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap();
        assert_eq!(body["pane"], 2, "{body}");
        assert!(
            posts.recv_timeout(Duration::from_millis(500)).is_err(),
            "a program notification was pushed"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
