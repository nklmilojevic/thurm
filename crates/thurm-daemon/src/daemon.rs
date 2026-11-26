//! Daemon state: panes, clients, frame pump, monitor loop and request handling.

#[path = "active_size.rs"]
mod active_size;
mod agent_prompt;
pub mod clip;
pub mod ext;
mod focus;
mod layouts;
mod plugins;
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
use crate::program_resume::{self, ProgramResumeState, ResumeExecSpec, TrustSubject, TrustVerdict};
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
/// The longest a handed-over resume ticket may still wait (see `program_resume::TICKET_TTL`).
const TICKET_TTL_MS: u64 = program_resume::TICKET_TTL.as_millis() as u64;
/// The most record bytes one [`Request::ResumeRecords`] answer holds: every pane's records
/// fit when a pane is named, a list of all panes past it is refused.
const RESUME_LIST_BYTES: usize = 8 * 1024 * 1024;

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
        push_kind: Option<crate::push::NoticeKind>,
    },
}

/// An event queued for broadcast, with optional push-relay metadata computed while the pane lock
/// was held (preserving pane-lock-before-client-lock order).
struct Outgoing {
    event: Event,
    push_kind: Option<crate::push::NoticeKind>,
}

impl From<Event> for Outgoing {
    fn from(event: Event) -> Self {
        Self {
            event,
            push_kind: None,
        }
    }
}

fn push_notice_kind(state: NoticeState) -> Option<crate::push::NoticeKind> {
    match state {
        NoticeState::Blocked => Some(crate::push::NoticeKind::Permission),
        NoticeState::Waiting => Some(crate::push::NoticeKind::NeedsInput),
        NoticeState::Finished => Some(crate::push::NoticeKind::Done),
        NoticeState::Failed => Some(crate::push::NoticeKind::Error),
        NoticeState::Working | NoticeState::Idle => None,
    }
}

/// Most of an unterminated escape sequence (or UTF-8 character) kept to replay after an
/// in-place upgrade (see `PaneState::carry`).
const MAX_CARRY: usize = 256 * 1024;

/// The most panes one `OwnPanes` or `DisownPanes` may name: a tab has a handful, and each one
/// costs a pane lock and a log line.
const MAX_CLAIMED_PANES: usize = 1024;

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
    /// The terminal's foreground process group when each part of `data` was read: from each
    /// offset on, until the next.
    groups: Vec<(usize, Option<u32>)>,
    eof: bool,
    /// The parser is working through a batch it took.
    parsing: bool,
}

/// `len` bytes read in parts, each with the foreground process group when it was read (from
/// its offset on), as `(start, end, group)`. Bytes before the first part have no group.
fn read_groups(len: usize, groups: &[(usize, Option<u32>)]) -> Vec<(usize, usize, Option<u32>)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut group = None;
    for &(at, g) in groups {
        let at = at.min(len);
        if at > start {
            out.push((start, at, group));
        }
        start = start.max(at);
        group = g;
    }
    if len > start || out.is_empty() {
        out.push((start, len, group));
    }
    out
}

/// Follows program reports (OSC 7501) through PTY reads, which split them anywhere.
#[derive(Default)]
struct ReportScan {
    /// The last read's last bytes: the start of a marker the next read may finish.
    tail: Vec<u8>,
    /// Inside a report: its end is still to come.
    open: bool,
}

impl ReportScan {
    const MARKER: &[u8] = b"]7501;";

    /// Whether this read has a part of a report from its marker on.
    fn read(&mut self, data: &[u8]) -> bool {
        let mut touched = self.open;
        let mut at = 0;
        let mut joined = std::mem::take(&mut self.tail);
        let kept = joined.len();
        joined.extend_from_slice(&data[..data.len().min(Self::MARKER.len() - 1)]);
        if let Some(i) = memchr::memmem::find(&joined, Self::MARKER) {
            // Too short to hold a marker on either side: it spans the two reads.
            touched = true;
            self.open = true;
            at = i + Self::MARKER.len() - kept;
        }
        loop {
            if self.open {
                // A report has neither ESC nor BEL: the first one ends it.
                let Some(i) = memchr::memchr2(0x1b, 0x07, &data[at..]) else {
                    break;
                };
                self.open = false;
                at += i + 1;
            }
            let Some(i) = memchr::memmem::find(&data[at..], Self::MARKER) else {
                break;
            };
            touched = true;
            self.open = true;
            at += i + Self::MARKER.len();
        }
        let keep = Self::MARKER.len() - 1;
        self.tail = if data.len() >= keep {
            data[data.len() - keep..].to_vec()
        } else {
            joined[joined.len().saturating_sub(keep)..].to_vec()
        };
        touched
    }
}

/// A shell without integration sends no prompt mark when a program it started ends (Ctrl+C),
/// so the terminal coming back to the shell (the pane's, or the one behind its PTY proxy) ends
/// what the program reported. Not for a program that is the pane's process itself, nor for
/// records the shell reported itself.
fn shell_took_the_terminal_back(st: &PaneState) -> bool {
    let is_shell = |pgrp: Option<u32>| {
        pgrp.and_then(procinfo::process_info)
            .is_some_and(|p| procinfo::is_shell(&p.name))
    };
    let shell = st.pty.foreground_pgrp();
    st.programs_pgrp.is_some()
        && st.programs_pgrp != st.info.pid
        && st.programs_pgrp != shell
        && st.pty.terminal_pgrp() == st.info.pid
        && is_shell(shell)
        && is_shell(st.info.pid)
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
    /// Dropped by `RevokePhone`: it may no longer register for push or follow an agent.
    pub revoked: AtomicBool,
    /// Its panes follow the active client's size ([`thurm_proto::CAP_ACTIVE_SIZE`]).
    pub active_size: AtomicBool,
    /// Shows one pane at a time and holds the size of the one it shows
    /// ([`thurm_proto::CAP_PANE_PAGER`]).
    pub pane_pager: AtomicBool,
    /// Claims panes with `OwnPanes` and hears whose size they have
    /// ([`thurm_proto::CAP_PANE_OWNER`]).
    pub pane_owner: AtomicBool,
    /// `thurm events` ([`thurm_proto::CAP_STATE_EVENTS`]): gets every `PaneInfo`.
    pub state_events: AtomicBool,
    /// Accepts pane snapshots with activity times.
    pub agent_activity: AtomicBool,
    /// Accepts `Event::NotifyPrompt` (carrying optional verbatim permission request) in place of
    /// `Event::Notify` ([`thurm_proto::CAP_PENDING_PROMPT`]).
    pub pending_prompt: AtomicBool,
    /// A UI that carries out the newer UI commands ([`thurm_proto::CAP_UI_LAYOUT`]).
    pub ui_layout: AtomicBool,
    /// A UI that gathers agents into a wall ([`thurm_proto::CAP_AGENT_WALL`]).
    pub agent_wall: AtomicBool,
    /// A UI that floats panes ([`thurm_proto::CAP_FLOATING`]).
    pub floating: AtomicBool,
    /// A UI that opens new floating panes ([`thurm_proto::CAP_FLOATING_NEW`]).
    pub floating_new: AtomicBool,
    /// A UI that places floating panes ([`thurm_proto::CAP_FLOATING_PLACE`]).
    pub floating_place: AtomicBool,
    /// A UI that minimizes floating panes ([`thurm_proto::CAP_FLOATING_MINIMIZE`]).
    pub floating_minimize: AtomicBool,
    /// Uses the extension API and hears its notices ([`thurm_proto::CAP_EXT`]).
    pub ext: AtomicBool,
    /// Shows what plugins ask the user ([`thurm_proto::CAP_EXT_UI`]).
    pub ext_ui: AtomicBool,
    /// Opens forwards for plugins and reports its own ([`thurm_proto::CAP_EXT_FORWARDS`]).
    pub ext_forwards: AtomicBool,
    /// Hears which ports panes listen on ([`thurm_proto::CAP_PORT_EVENTS`]).
    pub port_events: AtomicBool,
    /// The plugin this client is the daemon's connection for.
    pub plugin: Mutex<Option<String>>,
    /// Removed from the daemon: nothing it asks takes effect any more.
    pub gone: AtomicBool,
    /// Held while the client is taken out of the daemon, and while files are pasted for it
    /// (`paste_for`): a client that went away never has paths typed after it left. Its own, so
    /// one client's paste holds up no other's leaving; reentrant, as what a paste sets off may
    /// end a plugin's client on the same thread.
    pub exit: parking_lot::ReentrantMutex<()>,
    /// A remote client that takes deflated events ([`thurm_proto::CAP_DEFLATE`]). Shared with the
    /// client's writer thread, which encodes what it sends.
    pub deflate: Arc<AtomicBool>,
    /// Who connected: a local peer, or a tailnet caller the Tailscale helper admitted.
    pub principal: Mutex<Option<thurm_proto::Principal>>,
    pub name: Mutex<String>,
    /// Approximate bytes queued in `tx` (see [`message_weight`]); the writer subtracts what it
    /// wrote.
    pub queued: std::sync::atomic::AtomicUsize,
    /// Messages sent but not yet flushed to the socket; the writer subtracts them after a flush.
    pub unflushed: std::sync::atomic::AtomicUsize,
    /// When the socket last took bytes from the writer: a client it still takes output on is not
    /// gone, though it says nothing (see `mobile_silence`).
    pub last_written: Arc<Mutex<Instant>>,
    /// Answers clipboard tools' pastes ([`thurm_proto::CAP_CLIP_QUERY`]).
    pub clip_query: AtomicBool,
    /// That client is this daemon's own app (see `ext::peer_is_our_app`): asked before others.
    pub clip_verified: AtomicBool,
    /// Clipboard bytes queued in `tx` (see [`clip::clip_weight`]); the writer subtracts what it
    /// wrote. Counted against the clipboard data the daemon holds at once.
    pub clip_queued: std::sync::atomic::AtomicUsize,
}

impl Client {
    pub fn send(&self, msg: ServerMessage) {
        let msg = match msg {
            ServerMessage::Event(Event::NotifyPrompt {
                pane,
                title,
                body,
                permission,
                sound,
                ..
            }) if !self.pending_prompt.load(Ordering::Relaxed) => {
                ServerMessage::Event(Event::Notify {
                    pane,
                    title,
                    body,
                    permission,
                    sound,
                })
            }
            other => other,
        };
        let msg = if self.agent_activity.load(Ordering::Relaxed) {
            match msg {
                ServerMessage::Event(Event::PaneInfo(info)) => {
                    ServerMessage::Event(Event::PaneActivity(info.into()))
                }
                ServerMessage::Response {
                    id,
                    result: Ok(Response::PaneInfo(info)),
                } => ServerMessage::Response {
                    id,
                    result: Ok(Response::PaneActivity(info.into())),
                },
                ServerMessage::Response {
                    id,
                    result: Ok(Response::Panes(panes)),
                } => ServerMessage::Response {
                    id,
                    result: Ok(Response::PanesWithActivity(
                        panes.into_iter().map(Into::into).collect(),
                    )),
                },
                other => other,
            }
        } else {
            msg
        };
        self.queued
            .fetch_add(message_weight(&msg), Ordering::Relaxed);
        let clip = clip::clip_weight(&msg);
        self.clip_queued.fetch_add(clip, Ordering::SeqCst);
        self.unflushed.fetch_add(1, Ordering::AcqRel);
        if self.tx.send(msg).is_err() {
            self.queued.store(0, Ordering::Relaxed);
            self.clip_give_back(clip);
            self.unflushed.store(0, Ordering::Release);
        }
    }

    /// A process of this daemon's user on this machine: not a tailnet caller, nor another user.
    pub fn is_local_peer(&self) -> bool {
        self.principal.lock().as_ref().is_none_or(|p| {
            p.kind == thurm_proto::PrincipalKind::Peer && p.uid == Some(unsafe { libc::getuid() })
        })
    }

    /// `n` clipboard bytes left the queue. Never below zero: after [`Client::writer_ended`]
    /// set the count to zero, a send that counted its message and then failed gives it back
    /// to nothing.
    fn clip_give_back(&self, n: usize) {
        let _ = self
            .clip_queued
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_sub(n))
            });
    }

    /// The writer wrote `msg`.
    pub fn clip_written(&self, msg: &ServerMessage) {
        self.clip_give_back(clip::clip_weight(msg));
    }

    /// The writer stopped and its queue (`rx`, dropped before this) went with it: nothing is
    /// queued any more, whatever its last message, the one it failed to write, or one sent
    /// while it stopped.
    pub fn writer_ended(&self) {
        self.unflushed.store(0, Ordering::Release);
        self.clip_queued.store(0, Ordering::SeqCst);
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
        ServerMessage::Response {
            result: Ok(Response::ResumeRecords(list)),
            ..
        } => resume_records_weight(list),
        _ => 256,
    }
}

/// About what a [`Response::ResumeRecords`] answer takes in memory and on the wire.
fn resume_records_weight(list: &[thurm_proto::PaneResumeRecords]) -> usize {
    list.iter()
        .flat_map(|p| &p.records)
        .map(|r| {
            256 + r.process_name.len()
                + r.cwd.as_ref().map_or(0, String::len)
                + r.argv.iter().map(|a| a.len() + 8).sum::<usize>()
        })
        .sum::<usize>()
        .max(256)
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
    /// `close_pane` took the pane away and told clients so: its exit says nothing more. Set
    /// under this lock, where `pane_exited` decides, so no PaneInfo can follow PaneClosed.
    pub closed: bool,
    pub subscribers: HashMap<u64, Subscriber>,
    pub terminal_attachment: Option<(u64, PaneSize)>,
    /// Per client, the size it shows the pane at (see `active_size`).
    pub wanted: HashMap<u64, active_size::Wanted>,
    /// The client whose size the pane has now.
    pub size_owner: Option<u64>,
    /// The phone showing the pane: other clients' `Resize` waits until it lets go.
    pub size_held_by: Option<u64>,
    /// The holder claimed the pane with `OwnPanes`, rather than holding it as a phone.
    pub size_claimed_by: Option<u64>,
    /// Clients that asked for the pane while another held it (`OwnMode::Request`), in the order
    /// they asked: when the holder lets go, the first one still connected claims it.
    pub size_requested_by: Vec<u64>,
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
    /// A line to type into the shell once it is up (an agent or program resume in a shell
    /// without the integration hook): at the shell's first prompt, or when due if the shell
    /// is in the foreground and nobody `typed`.
    pending_input: Option<(Instant, Vec<u8>)>,
    /// How many times `pending_input` came due while the shell wasn't in the foreground yet
    /// (its startup files still running): it waits a second more each time, up to
    /// [`PENDING_INPUT_WAITS`].
    pending_waits: u32,
    /// A client typed or pasted into the pane.
    typed: bool,
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
    /// The process group that was in the foreground when the program records last changed:
    /// when the shell has the terminal back from it, the records are over (see `monitor`).
    programs_pgrp: Option<u32>,
    /// The foreground process group when the output being parsed was read (see `reader_loop`).
    output_pgrp: Option<u32>,
    /// Program Resume Protocol (OSC 7577) token, records, and replay tracking.
    resume: ProgramResumeState,
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

/// How [`Daemon::create_pane_with_resume`] starts a restored pane with an OSC 7577 command.
enum RestoreLaunch<'a> {
    Normal,
    /// Interactive shell (zsh, bash, fish) whose integration runs `--exec-resume` at its first
    /// prompt.
    Hook { exec: &'a Path, ticket: &'a str },
    /// Owning process was the PTY's first process: `exec thurmd --exec-resume <ticket>` via
    /// login shell.
    FirstProcess {
        exec: &'a Path,
        ticket: &'a str,
        title: String,
        /// Started instead when the resume fails, so the pane stays with the reason: the
        /// configured shell's program and arguments.
        shell: &'a [String],
    },
}

/// Whether the pane command `cmd` starts the same program with the same arguments as `program`
/// `args` (the configured shell): the same file, whether named by path or looked up on `PATH`,
/// by the same name (a shell reads its name: `sh` linked to Bash runs Bash as a POSIX shell).
fn same_invocation(cmd: &[String], program: &str, args: &[String]) -> bool {
    let name = |prog: &str| Path::new(prog).file_name().map(|n| n.to_owned());
    let resolve = |prog: &str| -> Option<std::path::PathBuf> {
        let found = if prog.contains('/') {
            std::path::PathBuf::from(prog)
        } else {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .filter(|d| d.is_absolute())
                .map(|d| d.join(prog))
                .find(|c| c.is_file())?
        };
        std::fs::canonicalize(found).ok()
    };
    match cmd.split_first() {
        Some((first, rest)) => {
            rest == args
                && name(first).is_some()
                && name(first) == name(program)
                && resolve(first).is_some_and(|a| resolve(program) == Some(a))
        }
        None => false,
    }
}

/// How many seconds a pending resume line waits, past its due time, for a slow shell to come
/// up before it is given up.
const PENDING_INPUT_WAITS: u32 = 30;

/// A restored pane's saved program, waiting for the pane's shell to redeem `ticket`
/// ([`Request::TakeResume`]).
pub(crate) struct ResumeTicket {
    pub ticket: String,
    pub pane: PaneId,
    pub spec: ResumeExecSpec,
    pub expires: Instant,
    /// [`resume_policy`] of the configuration that let the program resume: redeemed only while
    /// the configuration still says the same.
    pub policy: String,
}

/// A digest of what decides which programs resume (`resume_programs` and the effective
/// rules): a ticket issued under one policy is not redeemed under another.
pub(crate) fn resume_policy(cfg: &Config) -> String {
    use sha2::Digest;
    let json =
        serde_json::to_vec(&(cfg.session.resume_programs, cfg.resume_rules())).unwrap_or_default();
    sha2::Sha256::digest(&json)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
    /// The program records' group (see `PaneState::programs_pgrp`).
    programs_pgrp: Option<u32>,
    /// Program Resume Protocol (OSC 7577) state for this pane.
    resume: ProgramResumeState,
    /// A line still to be typed, its waits so far, and whether a client typed (see the
    /// `PaneState` fields): in place before the reader can see the shell's first prompt.
    pending_input: Option<(Instant, Vec<u8>)>,
    pending_waits: u32,
    typed: bool,
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
    /// Held while a push registration is admitted and while `RevokePhone` drops connections:
    /// a connection is either revoked before its registration (which it then refuses) or
    /// registers before the revocation (which then forgets it), never half of each.
    push_admission: Mutex<()>,
    /// Byte streams: tickets not yet used, and the count open.
    pub streams: crate::streams::Streams,
    /// Watches the ports panes listen on while a client wants to hear of them.
    pub ports: crate::ports::Watcher,
    /// Files coming in and going out on streams.
    pub transfers: crate::transfer::Transfers,
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
    /// Panes with a notification nobody has read yet (see `pane_read`).
    unread: Mutex<HashSet<PaneId>>,
    ai_rx: Receiver<AiJob>,
    store: Option<Store>,
    /// One session save at a time: the monitor's autosave, SIGTERM, upgrades and requests.
    save_lock: Mutex<()>,
    readers: ReaderGate,
    pub socket: PathBuf,
    integration_dir: Option<PathBuf>,
    session_dirty: AtomicBool,
    /// An OSC 7577 record was set or removed, or a pane closed: the monitor saves on its next
    /// tick, off the thread that parses output.
    resume_save_due: AtomicBool,
    /// An in-place upgrade is capturing the panes for the next image: input that would
    /// cancel a pending resume (`PaneState::typed`) is refused until the exec, or until the
    /// upgrade gives up (see [`Daemon::set_upgrading`]).
    upgrading: AtomicBool,
    /// Restored panes' saved programs waiting for their shell (see [`ResumeTicket`]).
    resume_tickets: Mutex<Vec<ResumeTicket>>,
    restored: AtomicBool,
    /// `restore_session` or `adopt` is still installing panes: a snapshot written now would
    /// lack the rest of them and the layout, so saves wait (the session stays dirty).
    loading: AtomicBool,
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
    /// Actions, key bindings and event subscriptions of the extension API.
    ext: ext::Ext,
    /// The plugins and scripts the daemon runs.
    plugins: plugins::Plugins,
    next_clip: AtomicU64,
    /// Clipboard tools' pastes waiting for an app's answer.
    pending_clips: Mutex<HashMap<u64, clip::PendingClip>>,
    /// The pastes on threads of their own, at most `clip::MAX_PENDING`.
    clip_slots: Arc<clip::Slots>,
    /// The clipboard bytes on their way, at most `clip::max_held()`.
    clip_bytes: Arc<clip::Slots>,
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
        let daemon = Arc::new(Daemon {
            engine: RwLock::new(EngineConfig::from_config(&config, true)),
            dark: AtomicBool::new(true),
            agent_defs: RwLock::new(config.agent_defs()),
            program_defs: RwLock::new(config.program_defs()),
            config: RwLock::new(config),
            panes: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            push_admission: Mutex::new(()),
            streams: Default::default(),
            ports: Default::default(),
            // On the state directory's disk: the socket's runtime directory is often a small
            // memory-backed one.
            transfers: crate::transfer::Transfers::new(state_dir.join("drops")),
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
            resume_save_due: AtomicBool::new(false),
            upgrading: AtomicBool::new(false),
            resume_tickets: Mutex::new(Vec::new()),
            restored: AtomicBool::new(false),
            loading: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            stopping,
            just_exited: Mutex::new(Vec::new()),
            stopped_at: std::sync::OnceLock::new(),
            last_activity: Mutex::new(Instant::now()),
            focus: Mutex::new(focus::FocusLog::default()),
            ext: ext::Ext::default(),
            plugins: plugins::Plugins::default(),
            next_clip: AtomicU64::new(1),
            pending_clips: Mutex::new(HashMap::new()),
            clip_slots: Arc::default(),
            clip_bytes: Arc::default(),
        });
        daemon.ext_config_applied();
        daemon
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
        self.broadcast_outgoing(Outgoing::from(event), ui_only);
    }

    fn broadcast_outgoing(&self, out: Outgoing, ui_only: bool) {
        let Outgoing { event, push_kind } = out;
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
            // Every notification is unread until its pane is read (`pane_read`). Agent notices
            // only, which carry their state's sound (`[notifications.<state>]`), go to phones:
            // a program's own (OSC 9 / 99 / 777) and a finished command stay on the desktop.
            Event::Notify {
                pane,
                title,
                permission,
                sound,
                ..
            }
            | Event::NotifyPrompt {
                pane,
                title,
                permission,
                sound,
                ..
            } => {
                self.unread.lock().insert(*pane);
                if sound.is_some() {
                    self.push.notify(
                        crate::push::Notice {
                            pane: *pane,
                            title: title.clone(),
                            permission: *permission,
                            kind: push_kind,
                        },
                        &open(),
                    );
                }
            }
            Event::PaneInfo(info) => {
                self.push
                    .agent_changed(info.id, crate::push::activity_state(info), &open())
            }
            // Phones with the app open take the pane's notices down themselves.
            Event::PaneRead { pane } => self.push.read(*pane, &open()),
            Event::PaneClosed { pane } => {
                self.ports.poke();
                self.focus.lock().pane_closed(*pane);
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
                self.ports.poke();
            }
            _ => {}
        }
        self.emit_for(&event);
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

    /// The user read `pane` on some device (focused it, typed in it, answered its prompt): a
    /// notification still standing for it comes down everywhere, the desktops' and the phones'.
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
        let wall = cmd.needs_agent_wall();
        let floating = cmd.needs_floating();
        let floating_new = cmd.needs_floating_new();
        let floating_place = cmd.needs_floating_place();
        let floating_minimize = cmd.needs_floating_minimize();
        let (capable, older): (Vec<_>, Vec<_>) = desktops.into_iter().partition(|c| {
            c.ui_layout.load(Ordering::Relaxed)
                && (!wall || c.agent_wall.load(Ordering::Relaxed))
                && (!floating || c.floating.load(Ordering::Relaxed))
                && (!floating_new || c.floating_new.load(Ordering::Relaxed))
                && (!floating_place || c.floating_place.load(Ordering::Relaxed))
                && (!floating_minimize || c.floating_minimize.load(Ordering::Relaxed))
        });
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
            revoked: AtomicBool::new(false),
            active_size: AtomicBool::new(false),
            pane_pager: AtomicBool::new(false),
            pane_owner: AtomicBool::new(false),
            state_events: AtomicBool::new(false),
            agent_activity: AtomicBool::new(false),
            pending_prompt: AtomicBool::new(false),
            ui_layout: AtomicBool::new(false),
            agent_wall: AtomicBool::new(false),
            floating: AtomicBool::new(false),
            floating_new: AtomicBool::new(false),
            floating_place: AtomicBool::new(false),
            floating_minimize: AtomicBool::new(false),
            ext: AtomicBool::new(false),
            ext_ui: AtomicBool::new(false),
            ext_forwards: AtomicBool::new(false),
            port_events: AtomicBool::new(false),
            plugin: Mutex::new(None),
            gone: AtomicBool::new(false),
            exit: parking_lot::ReentrantMutex::new(()),
            deflate: Arc::new(AtomicBool::new(false)),
            principal: Mutex::new(None),
            name: Mutex::new(String::new()),
            queued: std::sync::atomic::AtomicUsize::new(0),
            unflushed: std::sync::atomic::AtomicUsize::new(0),
            last_written: Arc::new(Mutex::new(Instant::now())),
            clip_query: AtomicBool::new(false),
            clip_verified: AtomicBool::new(false),
            clip_queued: std::sync::atomic::AtomicUsize::new(0),
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

    pub fn remove_client(self: &Arc<Self>, id: u64) {
        log::info!("client {id} gone");
        // Before its registrations go: one it is making now sees the flag (see `ext_registry`).
        let leaving = self.clients.lock().get(&id).cloned();
        if let Some(c) = leaving {
            let _exit = c.exit.lock();
            self.clients.lock().remove(&id);
            c.gone.store(true, Ordering::SeqCst);
        }
        self.focus.lock().client_gone(id);
        self.transfers.client_gone(id);
        self.ext_client_gone(id);
        self.plugins_owner_gone(id);
        self.streams.client_gone(id);
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
        self.create_pane_with_resume(req, restore, RestoreLaunch::Normal)
    }

    fn create_pane_with_resume(
        self: &Arc<Self>,
        req: CreatePane,
        restore: Option<PaneId>,
        resume_launch: RestoreLaunch<'_>,
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
        let resume_token = (!program_resume::is_root())
            .then(program_resume::generate_token)
            .transpose()?;
        let (spawn_command, resume_hook, title_override) = match resume_launch {
            RestoreLaunch::Normal => (command.clone(), None, None),
            RestoreLaunch::Hook { exec, ticket } => (None, Some((exec, ticket)), None),
            RestoreLaunch::FirstProcess {
                exec,
                ticket,
                title,
                shell,
            } => (
                Some(
                    vec![
                        exec.display().to_string(),
                        "--exec-resume".into(),
                        ticket.to_owned(),
                        "--or-shell".into(),
                    ]
                    .into_iter()
                    .chain(shell.iter().cloned())
                    .collect(),
                ),
                None,
                Some(title),
            ),
        };
        let opts = shell::spawn_options(shell::PaneLaunch {
            id,
            command: spawn_command,
            cwd: cwd.clone(),
            extra_env: req.env.clone(),
            size,
            config: &cfg,
            socket: &self.socket,
            integration_dir: self.integration_dir.as_deref(),
            token: &token,
            resume_token: resume_token.as_deref(),
            resume_hook,
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
            title: title_override
                .or_else(|| command.as_ref().and_then(|c| c.first().cloned()))
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
                programs_pgrp: None,
                resume: ProgramResumeState::with_token(resume_token),
                pending_input: None,
                pending_waits: 0,
                typed: false,
            },
            true,
        )?;
        log::info!("pane {id} created (pid {pid})");
        if restore.is_none()
            && let Some(p) = self.pane(id)
        {
            let info = p.state.lock().info.clone();
            self.emit("pane_created", Some(&info), serde_json::json!({}));
        }
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
                closed: false,
                subscribers: HashMap::new(),
                terminal_attachment: None,
                wanted: HashMap::new(),
                size_owner: None,
                size_held_by: None,
                size_claimed_by: None,
                size_requested_by: Vec::new(),
                agent: AgentTracker::with_titles(self.config.read().agents.session_titles),
                osc_cwd: p.osc_cwd,
                command_started: None,
                command_line: None,
                carry: p.carry,
                forwarded_carry: p.forwarded_carry,
                pending_input: p.pending_input,
                pending_waits: p.pending_waits,
                typed: p.typed,
                shell_integration_seen: p.shell_integration_seen,
                git_probe: None,
                git_pending: false,
                osc_progress: None,
                program_alerts: Vec::new(),
                program_name: None,
                program_command: None,
                command_check: None,
                programs_pgrp: p.programs_pgrp,
                output_pgrp: None,
                resume: p.resume,
            }),
        });
        self.panes.lock().insert(id, pane.clone());
        self.session_dirty.store(true, Ordering::Relaxed);
        self.ports.poke();

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
        let mut reports = ReportScan::default();
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
            // Who wrote it, as near the write as the daemon gets: a program that reports and
            // exits at once has usually not given the terminal back yet, while the parser may
            // only reach its bytes after the shell has.
            let mut group = pty::foreground_of(&reader);
            // Behind a PTY proxy, the reporting program is on the proxy's terminal. Looking
            // there takes a few more calls, so only for reads with a report in them.
            if reports.read(&buf[..n]) {
                group = group.map(procinfo::behind_proxy);
            }
            let quiet = last_read.elapsed() >= QUIET;
            last_read = Instant::now();
            let mut st = pipe.state.lock();
            if quiet && st.data.is_empty() && !st.parsing {
                // Output after a pause (an echo, a prompt): parse it right here rather than
                // waking the parser, which is idle and has nothing queued. Nothing else can be
                // appended meanwhile, so the order holds.
                drop(st);
                self.parse_guarded(&pane, &buf[..n], &[(0, group)]);
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
            if st.groups.last().is_none_or(|&(_, g)| g != group) {
                let at = st.data.len();
                st.groups.push((at, group));
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
        let mut groups = Vec::new();
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
                std::mem::swap(&mut st.groups, &mut groups);
                st.parsing = true;
                pipe.room.notify_one();
            }
            self.parse_guarded(&pane, &batch, &groups);
            batch.clear();
            groups.clear();
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
    fn parse_guarded(&self, pane: &Arc<Pane>, bytes: &[u8], groups: &[(usize, Option<u32>)]) {
        let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (start, end, group) in read_groups(bytes.len(), groups) {
                self.parse_output(pane, &bytes[start..end], group);
            }
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
    /// `group` is the foreground process group when `bytes` were read.
    fn parse_output(&self, pane: &Arc<Pane>, bytes: &[u8], group: Option<u32>) {
        for slice in bytes.chunks(PARSE_SLICE) {
            let events = {
                let mut st = pane.state.lock();
                st.output_pgrp = group;
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
        let (cfg, configured_shell) = {
            let c = self.config.read();
            let shell = events
                .iter()
                .any(|e| matches!(e, TermEvent::ProgramResume { .. }))
                .then(|| c.terminal.shell.as_ref().and_then(|s| s.first()).cloned())
                .flatten();
            (c.notifications.clone(), shell)
        };
        let mut info_changed = false;
        let mut resume_changed = false;
        let mut outgoing: Vec<Outgoing> = Vec::new();
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
                    TermEvent::Bell => outgoing.push(Outgoing::from(Event::Bell { pane: pane.id })),
                    TermEvent::ClipboardStore(text) => {
                        outgoing.push(Outgoing::from(Event::ClipboardStore {
                            pane: pane.id,
                            text,
                        }))
                    }
                    TermEvent::ClipboardLoad => {
                        outgoing.push(Outgoing::from(Event::ClipboardRequest { pane: pane.id }))
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
                            outgoing.push(Outgoing::from(Event::Notify {
                                pane: pane.id,
                                title,
                                body,
                                permission: None,
                                sound: None,
                            }));
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
                            // At its prompt the shell reads it, unless someone started typing
                            // a line it would join.
                            if st.typed {
                                self.resume_tickets.lock().retain(|t| t.pane != pane.id);
                                st.term.print(
                                    "\r\n[automatic resume skipped: the terminal was busy]\r\n",
                                );
                            } else {
                                pane.write(input);
                            }
                        }
                    }
                    TermEvent::CommandStart => {
                        self.ports.poke();
                        st.info.at_prompt = false;
                        st.command_started = Some(Instant::now());
                        info_changed = true;
                    }
                    TermEvent::CommandLine(c) => st.command_line = Some(c),
                    TermEvent::CommandFinished(code) => {
                        self.ports.poke();
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
                                outgoing.push(Outgoing::from(Event::Notify {
                                    pane: pane.id,
                                    title: format!("{what} {status}"),
                                    body: format!("after {}s", start.elapsed().as_secs()),
                                    permission: None,
                                    sound: None,
                                }));
                            }
                        }
                        let secs = started.map_or(0, |s| s.elapsed().as_secs());
                        // The command line goes to the user's own hooks only: it may hold a
                        // secret typed as an argument.
                        self.emit_with_private(
                            HookEvent::CommandFinished.name(),
                            Some(&st.info),
                            serde_json::json!({
                                "exit_code": code,
                                "duration_secs": secs,
                            }),
                            serde_json::json!({ "command": st.command_line }),
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
                        // Whose report: the group in the foreground when its bytes were read.
                        st.programs_pgrp = st.output_pgrp.or_else(|| st.pty.foreground_pgrp());
                        outgoing.extend(self.sync_programs(pane.id, &mut st, records));
                        info_changed = true;
                    }
                    TermEvent::ProgramResume { body, total_len } => {
                        let ctx = program_resume::PaneContext {
                            leader: st.pty.live_leader(),
                            tty_rdev: st.pty.slave_rdev(),
                            tty_session: st.pty.terminal_session(),
                            // Who is in front when the report's bytes were read.
                            foreground_pgrp: st.output_pgrp.or_else(|| st.pty.foreground_pgrp()),
                            has_explicit_command: st.command.is_some(),
                            trusted_shells: program_resume::PaneContext::resolve_trusted_shells(
                                configured_shell.as_deref(),
                            ),
                        };
                        if st.resume.handle_report(&body, total_len, &ctx) {
                            resume_changed = true;
                        }
                    }
                    // Subscribers run their own copy of the terminal and free images there.
                    TermEvent::ImageFreed(_) => {}
                }
            }
            if info_changed {
                outgoing.push(Outgoing::from(Event::PaneInfo(st.info.clone())));
            }
            for ev in outgoing {
                self.broadcast_outgoing(ev, true);
            }
        }
        if resume_changed
            && self.config.read().session.resume_programs != thurm_config::ResumeProgramsMode::Off
        {
            // Spec SHOULD: save each resume record and each removal when it happens. The
            // monitor does it within a tick, so this thread never waits on the disk.
            self.session_dirty.store(true, Ordering::Relaxed);
            self.resume_save_due.store(true, Ordering::Relaxed);
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
        let (hold, info, closed) = {
            let mut st = pane.state.lock();
            st.info.alive = false;
            st.info.exit_code = code;
            // Closed explicitly: a PaneInfo or a notification now would bring the pane back to
            // clients that already dropped it.
            if st.term.program_exited() && !st.closed {
                let records = st.term.program_status();
                let events = self.sync_programs(pane.id, &mut st, records);
                self.broadcast(Event::PaneInfo(st.info.clone()), false);
                for ev in events {
                    self.broadcast_outgoing(ev, true);
                }
            }
            if st.hold {
                let msg = match code {
                    Some(c) => format!("\r\n\x1b[2m[process exited with code {c}]\x1b[0m"),
                    None => "\r\n\x1b[2m[process exited]\x1b[0m".to_owned(),
                };
                st.term.print(&msg);
            }
            // The exit hook hears what exited, agent included.
            let info = st.info.clone();
            // A held pane outlives its process, not its agent: its state would hold the badge
            // and the agent lists with nothing left to answer. The title stays what it was at
            // the exit (the session's topic, say), not what the agent last set on the terminal.
            if st.hold && !st.closed && st.info.agent.is_some() {
                st.agent = AgentTracker::default();
                st.info.agent = None;
                self.broadcast(Event::PaneInfo(st.info.clone()), false);
            }
            (st.hold, info, st.closed)
        };
        // A close that took the pane out of the map may not have taken its lock yet.
        if closed || self.pane(pane.id).is_none() {
            // Closed explicitly; ClosePane already notified clients.
            return;
        }
        log::info!("pane {} exited ({code:?})", pane.id);
        self.emit(
            HookEvent::PaneExited.name(),
            Some(&info),
            serde_json::json!({ "exit_code": code }),
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
        self.resume_tickets.lock().retain(|t| t.pane != id);
        self.session_dirty.store(true, Ordering::Relaxed);
        // Under the pane's lock, as `pane_exited` broadcasts: its exit's PaneInfo comes before
        // PaneClosed or not at all.
        let mut st = pane.state.lock();
        st.closed = true;
        st.pty.hangup();
        drop(st);
        self.broadcast(Event::PaneClosed { pane: id }, false);
        // Spec SHOULD: discard a record when the user closes the terminal on purpose. One save
        // on the monitor's next tick covers every pane of a closed tab.
        self.resume_save_due.store(true, Ordering::Relaxed);
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
                    push_kind,
                } => {
                    let ask = if done {
                        ai::summary(&agent, &screen)
                    } else {
                        ai::attention(&agent, hint.as_deref(), &screen)
                    };
                    let detail = self.ai.ask(&ask).ok().and_then(|t| ai::clean_detail(&t));
                    self.update_agent(pane, |st| {
                        let current = st.agent.episode() == episode;
                        let permission = st.agent.permission_prompt().filter(|_| !done);
                        // Verbatim permission request comes strictly from the agent hook, never
                        // from the model's paraphrase (`detail`).
                        let request = permission.and_then(|_| st.agent.permission_request());
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
                        vec![Outgoing {
                            event: Event::NotifyPrompt {
                                pane,
                                title,
                                body,
                                permission,
                                sound: Some(sound.clone()),
                                request,
                            },
                            push_kind,
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
    ) -> Vec<Outgoing> {
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
            if !matches!(ev.event, Event::Notify { .. } | Event::NotifyPrompt { .. }) {
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
                self.broadcast_outgoing(ev, true);
            }
        }
    }

    /// Applies `f` to a pane's state, then re-derives its agent state and title and tells
    /// clients when they changed.
    fn update_agent(&self, pane: PaneId, f: impl FnOnce(&mut PaneState) -> Vec<Outgoing>) {
        let Some(p) = self.pane(pane) else { return };
        let mut st = p.state.lock();
        let events = f(&mut st);
        if let Some(new) = st.agent.refresh() {
            st.info.agent = new;
            st.info.title = pane_title(&st, st.info.foreground.as_ref());
            self.broadcast(Event::PaneInfo(st.info.clone()), false);
        }
        for ev in events {
            self.broadcast_outgoing(ev, true);
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
        let cwd = st.info.cwd.clone();
        for want in st.agent.ai_wants(
            cfg.titles(),
            cfg.status(),
            &tail,
            title.as_deref(),
            cwd.as_deref(),
        ) {
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
            let mut resume_expired = false;
            let resume_now_ms = program_resume::monotonic_ms();
            let daemon_stopping = self.stopping.load(Ordering::Relaxed);
            for pane in self.all_panes() {
                let mut events: Vec<Outgoing> = Vec::new();
                {
                    let mut st = pane.state.lock();
                    // Before skipping dead panes: a held pane's records expire too.
                    if !daemon_stopping && st.resume.poll_liveness(resume_now_ms) {
                        resume_expired = true;
                    }
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
                    let mut programs_changed = false;
                    if shell_took_the_terminal_back(&st) {
                        st.programs_pgrp = None;
                        if st.term.foreground_returned() {
                            let records = st.term.program_status();
                            events.extend(self.sync_programs(pane.id, &mut st, records));
                            programs_changed = true;
                        }
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
                    self.type_pending_input(&pane, &mut st);
                    // idle_ms changes every tick; only report it along with other changes, or
                    // every 2 s for clients that display it.
                    let mut cmp = st.info.clone();
                    cmp.idle_ms = before.idle_ms;
                    if cmp != before || programs_changed || tick.is_multiple_of(4) {
                        events.push(Outgoing::from(Event::PaneInfo(st.info.clone())));
                    }
                    // Under the pane's lock, as everywhere: a state taken before a hook's must
                    // not reach clients after it.
                    for ev in events {
                        self.broadcast_outgoing(ev, true);
                    }
                }
            }
            let resume_due = self.resume_save_due.swap(false, Ordering::Relaxed);
            {
                let now = Instant::now();
                self.resume_tickets.lock().retain(|t| t.expires > now);
            }
            if self.store.is_some() && (resume_expired || resume_due) {
                // A record came or went: saved now, and again a second later if that fails.
                let _ = self.save_session_now();
                last_save = Instant::now();
            } else if self.store.is_some()
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

    /// [`Daemon::save_session`] for a change that must reach the disk (a resume record that
    /// came or went): the session stays dirty when the write fails, so the next tick retries.
    fn save_session_now(&self) -> std::io::Result<()> {
        self.session_dirty.store(true, Ordering::Relaxed);
        self.save_session()
    }

    /// Writes the layout and each pane's cwd, command and agent. Scrollback stays in memory:
    /// it never reaches the disk, so a restart restores the panes without their history.
    pub fn save_session(&self) -> std::io::Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        if self.loading.load(Ordering::Relaxed) {
            self.session_dirty.store(true, Ordering::Relaxed);
            return Ok(());
        }
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
        let (resume_mode, resume_rules) = {
            let cfg = self.config.read();
            (cfg.session.resume_programs, cfg.resume_rules())
        };
        for pane in saving {
            let mut st = pane.state.lock();
            if !st.info.alive && !teardown.iter().any(|p| p.id == pane.id) {
                continue;
            }
            let foreground = st.pty.foreground_pgrp();
            let program_resume = st
                .resume
                .chosen_for_save(foreground, resume_mode, &resume_rules);
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
                program_resume,
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

    /// Types a restored pane's pending resume line once it is due (see `pending_input`). Not
    /// while an upgrade hands the panes over: the hand-off may already carry the line, for the
    /// next image to type; it stays for this monitor if the upgrade gives up. Read under the
    /// pane's lock, which `handoff` takes after setting the flag.
    fn type_pending_input(&self, pane: &Arc<Pane>, st: &mut PaneState) {
        if self.upgrading.load(Ordering::SeqCst) {
            return;
        }
        if let Some((due, _)) = &st.pending_input
            && Instant::now() >= *due
            && let Some((_, input)) = st.pending_input.take()
        {
            // Typed only into the pane's own shell, waiting in the foreground, and
            // never after someone typed: a startup job would read it as input, and
            // a half-typed line would run with it.
            let shell_waits = st.info.foreground.as_ref().is_some_and(|p| {
                procinfo::is_shell(&p.name)
                    && (st.info.pid == Some(p.pid) || procinfo::parent(p.pid) == st.info.pid)
            });
            if shell_waits && !st.typed {
                pane.write(input);
            } else if !st.typed && st.pending_waits < PENDING_INPUT_WAITS {
                // The shell isn't up yet (slow startup files run something in the
                // foreground): wait for it, or for its first prompt.
                st.pending_waits += 1;
                st.pending_input = Some((Instant::now() + Duration::from_secs(1), input));
            } else {
                self.resume_tickets.lock().retain(|t| t.pane != pane.id);
                st.term
                    .print("\r\n[automatic resume skipped: the terminal was busy]\r\n");
            }
        }
    }

    /// Lets the readers go on (the upgrade did not happen).
    pub fn resume_readers(&self) {
        self.readers.resume();
    }

    /// Set before [`Daemon::handoff`] captures the panes, and cleared if the upgrade doesn't
    /// happen: meanwhile the first input into a pane with a resume pending is refused, so the
    /// `typed` the next image gets is final (the bytes would reach the shell, but the image
    /// that would cancel the resume for them is not the one that types it).
    pub fn set_upgrading(&self, on: bool) {
        self.upgrading.store(on, Ordering::SeqCst);
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
                programs_pgrp: st.programs_pgrp,
                slave_rdev: st.pty.slave_rdev(),
                resume_token: st.resume.token().map(str::to_owned),
                resume_state: Some(st.resume.handoff()),
                pending_input: st.pending_input.as_ref().map(|(due, input)| {
                    (
                        due.saturating_duration_since(Instant::now()).as_millis() as u64,
                        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, input),
                    )
                }),
                pending_waits: st.pending_waits,
                typed: st.typed,
                osc_7577_discard: st.term.osc_7577_discard(),
            });
        }
        let now = Instant::now();
        let resume_tickets = self
            .resume_tickets
            .lock()
            .iter()
            .filter(|t| t.expires > now)
            .map(|t| upgrade::TicketHandoff {
                ticket: t.ticket.clone(),
                pane: t.pane,
                spec: t.spec.clone(),
                remaining_ms: (t.expires - now).as_millis() as u64,
                policy: t.policy.clone(),
            })
            .collect();
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
            resume_tickets,
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
        self.loading.store(true, Ordering::Relaxed);
        // Before the panes: a shell that redeems its ticket right away finds it.
        let now = Instant::now();
        *self.resume_tickets.lock() = h
            .resume_tickets
            .into_iter()
            .filter(|t| program_resume::is_ticket(&t.ticket))
            .map(|t| ResumeTicket {
                ticket: t.ticket,
                pane: t.pane,
                spec: t.spec,
                expires: now + Duration::from_millis(t.remaining_ms.min(TICKET_TTL_MS)),
                // Checked against this image's configuration when redeemed.
                policy: t.policy,
            })
            .collect();
        let mut adopted = 0;
        for p in h.panes {
            let mut size = sanitize_size(p.size.into());
            let pty = match Pty::adopt(p.fd, p.pid, p.exited, p.slave_rdev) {
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
            // Last, after the replay: the rest of an oversized OSC 7577 is still discarded.
            term.restore_osc_7577_discard(p.osc_7577_discard);
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
            let mut resume = ProgramResumeState::with_token(p.resume_token.clone());
            if let Some(rs) = p.resume_state {
                resume.restore_handoff(rs);
            }
            // Before the reader starts: the shell's first prompt may already wait in the PTY.
            let pending_input = p.pending_input.and_then(|(ms, b64)| {
                let input =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()?;
                Some((Instant::now() + Duration::from_millis(ms), input))
            });
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
                    programs_pgrp: p.programs_pgrp,
                    resume,
                    pending_input,
                    pending_waits: p.pending_waits,
                    typed: p.typed,
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
                            st.agent.titles = self.config.read().agents.session_titles;
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
        self.loading.store(false, Ordering::Relaxed);
        log::info!("adopted {adopted} panes from the previous daemon");
    }

    /// Recreate panes from the last snapshot (after a reboot or daemon restart).
    pub fn restore_session(self: &Arc<Self>) {
        let Some(store) = &self.store else { return };
        let Some(mut snap) = store.load() else { return };
        self.loading.store(true, Ordering::Relaxed);
        let cfg = self.config.read().clone();
        let defs = cfg.agent_defs();
        let resume_rules = cfg.resume_rules();
        let policy = resume_policy(&cfg);
        let my_uid = unsafe { libc::geteuid() };
        let thurmd_exe = std::env::current_exe().ok();
        let use_hook = shell::uses_hook_resume(&cfg, self.integration_dir.as_deref());
        let (shell_prog, shell_args) = shell::interactive_shell(&cfg);
        let shell_argv: Vec<String> = std::iter::once(shell_prog.clone())
            .chain(shell_args.iter().cloned())
            .collect();

        struct PlannedResume {
            spec: ResumeExecSpec,
            cwd: Option<String>,
            first_process: bool,
            program_label: String,
            rule: Option<String>,
        }

        // With OSC 7577 resume off, a saved record is as good as absent: the pane's command
        // and `resume_agents` apply as if it had never been reported.
        if program_resume::is_root()
            || cfg.session.resume_programs == thurm_config::ResumeProgramsMode::Off
        {
            for p in &mut snap.panes {
                p.program_resume = None;
            }
        }

        // First pass: validate every saved OSC 7577 record against current config, UID, canonical
        // paths, directory existence, and shell support; mark accepted ones `attempted = true` on
        // disk BEFORE spawning so a crash or failed command never retries.
        // Saved launch evidence is digested with this key: without it, rules fail closed.
        let evidence_key = snap
            .panes
            .iter()
            .any(|p| p.program_resume.is_some())
            .then(program_resume::evidence_key)
            .flatten();
        let mut plans: Vec<Option<PlannedResume>> = Vec::with_capacity(snap.panes.len());
        let mut marked_attempted = false;
        // Records an earlier start already tried: their pane gets a plain shell, since the
        // program (or its resume) may be what failed.
        let tried_before: Vec<bool> = snap
            .panes
            .iter()
            .map(|p| p.program_resume.as_ref().is_some_and(|r| r.attempted))
            .collect();
        for p in &mut snap.panes {
            let Some(rec) = p.program_resume.as_mut() else {
                plans.push(None);
                continue;
            };
            let prog = rec
                .argv
                .first()
                .map(|a| program_resume::escape_display(a))
                .unwrap_or_default();
            let skip = |reason: &str| {
                log::info!(
                    "pane {}: not resuming saved program {prog} (reason={reason})",
                    p.id
                );
            };
            if rec.attempted {
                skip("already_tried");
                plans.push(None);
                continue;
            }
            if rec.uid != my_uid {
                log::warn!(
                    "pane {}: discarding saved program {prog} (reason=uid_mismatch)",
                    p.id
                );
                p.program_resume = None;
                marked_attempted = true;
                plans.push(None);
                continue;
            }
            // The directory the command will actually start in: the report's `cwd` (which must
            // still exist), else the owner's directory if it still exists, else the pane's.
            let resolved_cwd = if rec.cwd_from_report {
                match &rec.cwd {
                    Some(dir) if Path::new(dir).is_dir() => Some(dir.clone()),
                    _ => {
                        skip("dir_gone");
                        plans.push(None);
                        continue;
                    }
                }
            } else {
                rec.cwd
                    .as_ref()
                    .filter(|d| Path::new(d).is_dir())
                    .cloned()
                    .or_else(|| p.cwd.clone())
            };
            let trust = program_resume::evaluate_trust(
                &TrustSubject {
                    argv: &rec.argv,
                    cwd: resolved_cwd.as_deref(),
                    process_exe: &rec.process_exe,
                    launch: program_resume::Launch::Saved {
                        evidence: rec.launch.as_ref(),
                        key: evidence_key.as_ref(),
                    },
                    wrapped: rec.wrapped,
                },
                cfg.session.resume_programs,
                &resume_rules,
                true,
            );
            let (rule, dir_restricted, pin_exe) = match trust {
                TrustVerdict::Trusted {
                    rule,
                    dirs_restricted,
                    pin_exe,
                } => (rule, dirs_restricted, pin_exe),
                TrustVerdict::Untrusted(reason) => {
                    skip(&format!("not_trusted trust_reason={reason}"));
                    plans.push(None);
                    continue;
                }
            };
            // The rule that trusted it limits the program to some directories: it may only
            // start in one that exists.
            if dir_restricted
                && !resolved_cwd
                    .as_deref()
                    .is_some_and(|d| Path::new(d).is_dir())
            {
                skip("dir_gone");
                plans.push(None);
                continue;
            }
            // A job of the shell resumes only in the default shell, the one the hook and the
            // typed line are made for: a pane made with another command (`thurm new -- bash`
            // under zsh, or another shell of the same name) is restored with that command, and
            // its program isn't resumed.
            if !rec.first_process
                && p.command
                    .as_ref()
                    .is_some_and(|c| !same_invocation(c, &shell_prog, &shell_args))
            {
                skip("own_command");
                plans.push(None);
                continue;
            }
            // A job of the shell runs from the shell's own first-prompt hook, or else is typed
            // into a POSIX shell once its startup files had time to run.
            if !rec.first_process
                && !use_hook
                && !program_resume::shell_supports_job_resume(&shell_prog)
            {
                skip("shell_unsupported");
                plans.push(None);
                continue;
            }
            if thurmd_exe.is_none() {
                log::warn!(
                    "pane {}: not resuming saved program {prog} (cannot locate thurmd binary)",
                    p.id
                );
                plans.push(None);
                continue;
            }
            let require_cwd = rec.cwd_from_report || dir_restricted;
            // A directory it must start in is pinned to the one checked here: its canonical
            // path, entered only while it is still the same directory.
            let (spec_cwd, cwd_id) = match resolved_cwd.as_deref() {
                Some(dir) if require_cwd => {
                    let canon = std::fs::canonicalize(dir)
                        .ok()
                        .map(|d| d.display().to_string());
                    let id = canon.as_deref().and_then(program_resume::dir_identity);
                    match (canon, id) {
                        (Some(canon), Some(id)) => (Some(canon), Some(id)),
                        _ => {
                            skip("dir_gone");
                            plans.push(None);
                            continue;
                        }
                    }
                }
                _ => (resolved_cwd.clone(), None),
            };
            // A rule that trusted the owner by its executable also decides which program runs:
            // only one found where that executable (or its script) is. Taken from the verdict,
            // not looked up by the rule's name: a rule need not have one.
            let exe_dirs = if pin_exe {
                let dirs = program_resume::saved_pin_dirs(&rec.process_exe, rec.launch.as_ref());
                if dirs.is_empty() {
                    skip("program_gone");
                    plans.push(None);
                    continue;
                }
                dirs
            } else {
                Vec::new()
            };
            rec.attempted = true;
            marked_attempted = true;
            let title = rec
                .argv
                .first()
                .map(|a| program_resume::escape_display(a.rsplit('/').next().unwrap_or(a)))
                .unwrap_or_else(|| "shell".into());
            plans.push(Some(PlannedResume {
                spec: ResumeExecSpec {
                    argv: rec.argv.clone(),
                    // Entered by the helper itself, after the shell's startup files (which may
                    // change directory) ran.
                    cwd: spec_cwd,
                    require_cwd,
                    cwd_id,
                    exe_dirs,
                },
                cwd: resolved_cwd,
                first_process: rec.first_process,
                program_label: title,
                rule,
            }));
        }
        // Nothing runs unless the snapshot says it was tried: a crash afterwards must not run it
        // again on the next start.
        if marked_attempted && let Err(e) = store.save(&snap) {
            log::warn!("not resuming saved programs: cannot record the attempt: {e}");
            for plan in &mut plans {
                *plan = None;
            }
        }

        let mut restored = 0;
        for ((p, plan), tried_before) in snap.panes.iter().zip(plans).zip(tried_before) {
            if let Some(plan) = plan {
                let exe = thurmd_exe.as_ref().expect("checked above");
                // The pane's shell redeems the ticket for the command; the command itself
                // stays in this process. In place before the shell starts.
                let ticket = match program_resume::generate_ticket() {
                    Ok(t) => t,
                    Err(e) => {
                        log::warn!("pane {}: cannot resume {}: {e}", p.id, plan.program_label);
                        String::new()
                    }
                };
                if !ticket.is_empty() {
                    self.resume_tickets.lock().push(ResumeTicket {
                        ticket: ticket.clone(),
                        pane: p.id,
                        spec: plan.spec.clone(),
                        expires: Instant::now() + program_resume::TICKET_TTL,
                        policy: policy.clone(),
                    });
                    let req = CreatePane {
                        // The pane's own command (the default shell, for a job of it).
                        command: p.command.clone(),
                        cwd: plan.cwd.clone(),
                        size: p.size.into(),
                        ..Default::default()
                    };
                    let launch = if plan.first_process {
                        RestoreLaunch::FirstProcess {
                            exec: exe,
                            ticket: &ticket,
                            title: plan.program_label.clone(),
                            shell: &shell_argv,
                        }
                    } else if use_hook {
                        RestoreLaunch::Hook {
                            exec: exe,
                            ticket: &ticket,
                        }
                    } else {
                        RestoreLaunch::Normal
                    };
                    match self.create_pane_with_resume(req, Some(p.id), launch) {
                        Ok(id) => {
                            restored += 1;
                            log::info!(
                                "pane {id}: resuming saved program {} (rule={})",
                                plan.program_label,
                                plan.rule.as_deref().unwrap_or("all")
                            );
                            if !plan.first_process
                                && !use_hook
                                && let Some(pane) = self.pane(id)
                            {
                                // No hook: typed like `resume_agents` does, once the shell
                                // holds the terminal and nobody typed (see `pending_input`). A
                                // leading space keeps it out of history where configured; only
                                // the helper's path and the ticket are typed, never `argv`.
                                let line = format!(
                                    " {} --exec-resume {}\r",
                                    shell::shell_quote(&exe.display().to_string()),
                                    ticket,
                                );
                                pane.state.lock().pending_input = Some((
                                    Instant::now() + Duration::from_millis(2500),
                                    line.into_bytes(),
                                ));
                            }
                            continue;
                        }
                        Err(e) => {
                            self.resume_tickets.lock().retain(|t| t.pane != p.id);
                            log::warn!(
                                "pane {}: cannot resume {}, starting a shell: {e}",
                                p.id,
                                plan.program_label
                            );
                        }
                    }
                }
            }

            // A record an earlier start already tried gets a plain shell in the pane's saved cwd,
            // not `p.command` or `agent_resume`: never run what may have failed twice. Any other
            // record not resumed here (untrusted, dir_gone, shell_unsupported, or a failed
            // launch) leaves the pane as if it had never been reported.
            if tried_before {
                let req = CreatePane {
                    command: None,
                    cwd: p.cwd.clone(),
                    size: p.size.into(),
                    ..Default::default()
                };
                match self.create_pane(req, Some(p.id)) {
                    Ok(_) => restored += 1,
                    Err(e) => log::warn!("failed to restore pane {}: {e}", p.id),
                }
                continue;
            }

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
        self.loading.store(false, Ordering::Relaxed);
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
            // The app may ask the user first. Admitted (a slot taken) before the thread.
            req if req.is_clipboard_tool() => {
                let Some((pane, ask)) = clip::Ask::of(req) else {
                    unreachable!("a clipboard request")
                };
                match self.clipboard_admit(client, pane, &ask) {
                    Err(e) => Err(e),
                    Ok((slot, bytes)) => {
                        let daemon = self.clone();
                        let client = client.clone();
                        let spawned =
                            std::thread::Builder::new()
                                .name("clipboard".into())
                                .spawn(move || {
                                    let asked =
                                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                                            || daemon.clipboard_ask(client.id, pane, ask, bytes),
                                        ));
                                    let (result, held) = asked.unwrap_or_else(|_| {
                                        (Err("the daemon hit an internal error".into()), None)
                                    });
                                    drop(slot);
                                    if id != 0 {
                                        client.send(ServerMessage::Response { id, result });
                                    }
                                    // Counted in the client's queue from here.
                                    drop(held);
                                });
                        // A thread refused: the slot went back with the closure.
                        if let Err(e) = spawned {
                            Err(format!("could not start a request thread: {e}"))
                        } else {
                            return;
                        }
                    }
                }
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
            Request::Ext { method, params } if ext::blocks(&method) => {
                let daemon = self.clone();
                let client = client.clone();
                let spawned = std::thread::Builder::new()
                    .name("ext".into())
                    .spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            daemon.ext_request(&client, &method, &params)
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
            // Slow queries and layout confirmation must not block later requests,
            // including the desktop client's reply. Responses carry their request ID.
            slow @ (Request::Complete { .. }
            | Request::SearchPaths { .. }
            | Request::Processes { .. }
            | Request::Ports { .. }
            | Request::ApplyLayout { .. }
            // An upload's admission may walk and prune the kept drops.
            | Request::UploadBegin { .. }
            | Request::DownloadBegin { .. }
            | Request::CopyTo { .. }
            | Request::CopyFrom { .. }) => {
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
    ) -> Vec<Outgoing> {
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
        self.emit(
            HookEvent::Agent(state).name(),
            Some(&info),
            serde_json::json!({}),
        );
        let notice = n.notice(state);
        if !n.enabled || !notice.notify {
            return Vec::new();
        }
        let done = matches!(state, NoticeState::Finished | NoticeState::Failed);
        let push_kind = push_notice_kind(state);
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
                push_kind,
            };
            if self.ai_tx.try_send(job).is_ok() {
                return Vec::new();
            }
        }
        let permission = a.permission.filter(|_| !done);
        let request = permission.and_then(|_| st.agent.permission_request());
        let (title, body) = notify;
        vec![Outgoing {
            event: Event::NotifyPrompt {
                pane,
                title,
                body,
                permission,
                sound: Some(notice.sound.clone()),
                request,
            },
            push_kind,
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
                if has(thurm_proto::CAP_PANE_PAGER) {
                    client.pane_pager.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_PANE_OWNER) {
                    client.pane_owner.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_STATE_EVENTS) {
                    client.state_events.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_AGENT_ACTIVITY) {
                    client.agent_activity.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_PENDING_PROMPT) {
                    client.pending_prompt.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_UI_LAYOUT) {
                    client.ui_layout.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_AGENT_WALL) {
                    client.agent_wall.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_FLOATING) {
                    client.floating.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_FLOATING_NEW) {
                    client.floating_new.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_FLOATING_PLACE) {
                    client.floating_place.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_FLOATING_MINIMIZE) {
                    client.floating_minimize.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_DEFLATE) {
                    client.deflate.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_EXT) {
                    client.ext.store(true, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_PORT_EVENTS) {
                    client.port_events.store(true, Ordering::Relaxed);
                    self.ports.subscribed(self.clone(), client.id);
                }
                if has(thurm_proto::CAP_CLIP_QUERY) {
                    client.clip_query.store(true, Ordering::Relaxed);
                    let verified = client
                        .socket
                        .lock()
                        .as_ref()
                        .is_some_and(ext::peer_is_our_app);
                    client.clip_verified.store(verified, Ordering::Relaxed);
                }
                // Plugins' questions go only to a desktop app of this user on this machine:
                // a tailnet caller or a phone saying so is not trusted with them.
                if has(thurm_proto::CAP_EXT_UI) {
                    let local = client.is_local_peer();
                    let desktop = ui && !client.mobile.load(Ordering::Relaxed);
                    let app = client
                        .socket
                        .lock()
                        .as_ref()
                        .is_none_or(ext::peer_is_our_app);
                    client
                        .ext_ui
                        .store(local && desktop && app, Ordering::Relaxed);
                }
                if has(thurm_proto::CAP_EXT_FORWARDS) {
                    let trusted = client.ext_ui.load(Ordering::Relaxed);
                    client.ext_forwards.store(trusted, Ordering::Relaxed);
                    if trusted {
                        self.ext_forwarding_app_came(client.id);
                    }
                }
                log::info!(
                    "client {} hello: {name} (protocol {version}{}{}{}{}{})",
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
                    if has(thurm_proto::CAP_PANE_PAGER) {
                        ", pane pager"
                    } else {
                        ""
                    },
                    if has(thurm_proto::CAP_PANE_OWNER) {
                        ", pane owner"
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
                        .copied()
                        .chain(std::iter::once(thurm_proto::CAP_PENDING_PROMPT))
                        .map(str::to_owned)
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
            Request::SearchPaths { pane, query } => {
                let (cwd, pid, foreground, alive) = self.with_pane(pane, |_, st| {
                    (
                        st.info.cwd.clone(),
                        st.info.pid,
                        st.info.foreground.clone(),
                        st.info.alive,
                    )
                })?;
                if !alive {
                    return Err("This terminal has closed.".into());
                }
                if foreground
                    .as_ref()
                    .is_some_and(|p| matches!(p.name.as_str(), "ssh" | "mosh" | "mosh-client"))
                {
                    return Err("Use a Thurm remote workspace to search this host.".into());
                }
                let cwd = cwd
                    .filter(|s| Path::new(s).is_absolute())
                    .ok_or("The terminal's working folder is not available.")?;
                let home = pid
                    .and_then(|pid| procinfo::env_var(pid, "HOME"))
                    .or_else(|| std::env::var("HOME").ok());
                crate::path_search::search(&query, Path::new(&cwd), home.as_deref())
                    .map(Response::PathMatches)
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
            Request::OwnPanes { panes, mode } => {
                self.own_panes(client, &claim_list(panes)?, mode);
                Ok(Response::Ok)
            }
            Request::DisownPanes { panes } => {
                self.disown_panes(client, &claim_list(panes)?);
                Ok(Response::Ok)
            }
            Request::Input { pane, data } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    if !data.is_empty() {
                        self.typing_frozen(pane, st)?;
                    }
                    self.used_by(client, st);
                    if !data.is_empty() {
                        st.typed = true;
                        st.agent.prompt.interrupted();
                    }
                    st.agent.user_answer(&data);
                    self.acknowledge_programs(pane, st);
                    if !data.is_empty() {
                        self.pane_read(pane);
                    }
                    p.write(data);
                    Ok::<_, String>(())
                })??;
                Ok(Response::Ok)
            }
            Request::Paste { pane, text } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    self.paste_text(client, pane, p, st, &text)
                })??;
                Ok(Response::Ok)
            }
            Request::Key { pane, key } => {
                self.with_input_pane(client.id, pane, |p, st| {
                    // Before encoding it (which scrolls and clears the selection): even a key
                    // that types nothing waits out the hand-off.
                    self.typing_frozen(pane, st)?;
                    let bytes = st.term.key(&key);
                    self.used_by(client, st);
                    if !bytes.is_empty() {
                        st.typed = true;
                        st.agent.prompt.interrupted();
                        st.agent.user_answer(&bytes);
                        self.acknowledge_programs(pane, st);
                        self.pane_read(pane);
                        p.write(bytes);
                    }
                    Ok::<_, String>(())
                })??;
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
                    let mut log = self.focus.lock();
                    log.focus(client.id, pane, focused, Instant::now());
                    if focused && clip::shares_clipboard(client) {
                        log.shown_by(client.id, pane);
                    }
                    drop(log);
                    // Looking at a finished agent acknowledges it (Done → Idle).
                    if focused {
                        self.emit("pane_focused", Some(&st.info), serde_json::json!({}));
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
                    if client.pane_owner.load(Ordering::Relaxed) {
                        client.send(ServerMessage::Event(Event::SizeOwner {
                            pane,
                            yours: st.size_owner == Some(client.id),
                        }));
                    }
                })?;
                Ok(Response::Ok)
            }
            Request::Unsubscribe { pane } => {
                self.with_pane(pane, |_, st| {
                    st.subscribers.remove(&client.id);
                    // Not shown here any more: its size goes to a client still showing it.
                    self.forget_size(st, client.id);
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
            // Spelt out, so a clipboard request added later is a compile error here.
            Request::ClipboardGet { .. }
            | Request::ClipboardSet { .. }
            | Request::ClipboardClear { .. } => unreachable!("handled in handle()"),
            Request::ClipboardAnswer { id, data } => self.clipboard_answer(client.id, id, data),
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
                let _admission = self.push_admission.lock();
                if client.revoked.load(Ordering::SeqCst) {
                    return Err("this connection was closed: a phone was revoked".into());
                }
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
                // Revoking its key (or replacing it) leaves an ssh session already open, and
                // which phone a connection is (the key it came in with) is not known here: drop
                // them all, tailnet ones included, whatever they claim to be. The others
                // reconnect and register again; the revoked one can no longer sign in. Dropped
                // first, so nothing the revoked phone sends re-registers it after the forgetting
                // below; `push_admission` orders this against registrations in flight.
                let _admission = self.push_admission.lock();
                let clients: Vec<_> = self.clients.lock().values().cloned().collect();
                for c in clients.iter().filter(|c| c.mobile.load(Ordering::Relaxed)) {
                    log::info!("client {}: a phone was revoked; dropping it", c.id);
                    c.revoked.store(true, Ordering::SeqCst);
                    if let Some(s) = c.socket.lock().as_ref() {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                    }
                }
                let gone = self.push.revoke_named(&name)?;
                log::info!("revoked {name}: {} push device(s) forgotten", gone.len());
                Ok(Response::Ok)
            }
            Request::FollowAgent {
                device,
                pane,
                token,
            } => {
                if client.revoked.load(Ordering::SeqCst) {
                    return Err("this connection was closed: a phone was revoked".into());
                }
                log::info!("pane {pane}: a live activity follows it (device {device:.6}…)");
                // Under the pane's lock, which its agent's transitions take too: the state
                // the follow starts from is the current one, and the next transition updates it.
                self.with_pane(pane, |_, st| {
                    self.push
                        .follow(&device, pane, token, crate::push::activity_state(&st.info))
                })??;
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
                self.sync_plugins(false);
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
                    // Late from an agent that has exited with its (held) pane.
                    if !st.info.alive {
                        return;
                    }
                    let before = st.info.agent.clone();
                    if st.agent.invalidate_dead_report_owner() {
                        st.info.agent = st.agent.state().cloned();
                    }
                    st.agent.saw_foreground(st.pty.foreground_pgrp());
                    if st.agent.public_report_active() && event != "session-title" {
                        return;
                    }
                    st.agent
                        .apply_hook(pgrp, &agent, &name, &event, session_id, message);
                    st.agent.titles = titles;
                    // Hooks without a path (Notification) keep following the current file.
                    // Followed even without titles: it is where interrupts show up.
                    if event != "session-title"
                        && (transcript_path.is_some() || event == "session-start" || agent == "agy")
                    {
                        st.agent.read_transcript(transcript_path.as_deref());
                    }
                    if let Some(new) = st.agent.refresh() {
                        let events =
                            self.agent_notifications(pane, st, before.as_ref(), new.as_ref());
                        st.info.agent = new;
                        st.info.title = pane_title(st, st.info.foreground.as_ref());
                        self.broadcast(Event::PaneInfo(st.info.clone()), false);
                        for ev in events {
                            self.broadcast_outgoing(ev, true);
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
                            self.broadcast_outgoing(ev, true);
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
            Request::Ext { method, params } => self.ext_request(client, &method, &params),
            Request::DialBegin { port } => {
                let principal = client.principal.lock().clone().unwrap_or_default();
                self.streams
                    .reserve(crate::streams::Kind::Tcp { port }, client.id, principal)
                    .map(|(ticket, expires_ms)| Response::Ticket { ticket, expires_ms })
            }
            // Taken by the connection's reader (`server::handle_client`) when it may be.
            Request::StreamOpen { .. } => {
                Err("StreamOpen is only the first request after Hello on a new connection".into())
            }
            Request::Ports { pane } => {
                self.ports.poke();
                let roots: Vec<(PaneId, u32)> = crate::ports::Host::pane_roots(&**self)
                    .into_iter()
                    .filter(|(id, _)| pane.is_none_or(|p| p == *id))
                    .collect();
                if let Some(p) = pane
                    && roots.is_empty()
                    && self.pane(p).is_none()
                {
                    return Err(format!("no such pane: {p}"));
                }
                crate::ports::look(&roots)
                    .map(|seen| Response::Ports(crate::ports::as_response(seen)))
                    .ok_or_else(|| "could not list the processes".to_owned())
            }
            Request::DialAt { port, at } => {
                let principal = client.principal.lock().clone().unwrap_or_default();
                self.streams
                    .reserve(
                        crate::streams::Kind::TcpAt { port, at },
                        client.id,
                        principal,
                    )
                    .map(|(ticket, expires_ms)| Response::Ticket { ticket, expires_ms })
            }
            Request::UploadBegin {
                pane,
                name,
                size,
                batch,
            } => {
                // Admitted while the client is still here, or not at all: its leaving waits for
                // this, then ends what was admitted. (The request runs off its connection, which
                // may have closed while it waited to.)
                let _exit = client.exit.lock();
                if client.gone.load(Ordering::SeqCst) {
                    return Err("the app sending the files went away".into());
                }
                // Its paths go to the program in the pane: a pane whose program ended (held
                // open) takes no files,
                // nor one whose input another client's attached terminal holds: refused now, not
                // when the files have come and their paths cannot be typed.
                if !self.with_input_pane(client.id, pane, |_, st| st.info.alive)? {
                    return Err("This terminal has closed.".into());
                }
                let principal = client.principal.lock().clone().unwrap_or_default();
                self.transfers
                    .upload_begin(self, client.id, principal, pane, &name, size, batch)
                    .map(|(ticket, expires_ms)| Response::Ticket { ticket, expires_ms })
            }
            Request::DownloadBegin {
                pane,
                path,
                if_version,
            } => {
                let (cwd, pid) =
                    self.with_pane(pane, |_, st| (st.info.cwd.clone(), st.info.pid))?;
                let home = pid
                    .and_then(|pid| procinfo::env_var(pid, "HOME"))
                    .or_else(|| std::env::var("HOME").ok());
                let principal = client.principal.lock().clone().unwrap_or_default();
                self.transfers
                    .download_begin(
                        self,
                        client.id,
                        principal,
                        pane,
                        &path,
                        cwd.as_deref(),
                        home.as_deref(),
                        if_version.as_deref(),
                        None,
                    )
                    .map(Response::Download)
            }
            Request::CopyTo {
                path,
                name,
                size,
                mode,
                replace,
            } => {
                let home = std::env::var("HOME").ok();
                let principal = client.principal.lock().clone().unwrap_or_default();
                self.transfers
                    .copy_to_begin(
                        self,
                        client.id,
                        principal,
                        &path,
                        &name,
                        size,
                        mode,
                        replace,
                        home.as_deref(),
                    )
                    .map(|(ticket, expires_ms, path)| Response::CopyTicket {
                        ticket,
                        expires_ms,
                        path,
                    })
            }
            Request::CopyFrom { path } => {
                let home = std::env::var("HOME").ok();
                let principal = client.principal.lock().clone().unwrap_or_default();
                self.transfers
                    .copy_from_begin(self, client.id, principal, &path, home.as_deref())
                    .map(Response::Download)
            }
            Request::DaemonId => thurm_config::daemon_id()
                .map(Response::Text)
                .map_err(|e| format!("the daemon's identifier: {e}")),
            Request::GetPendingPrompt { pane } => self.with_pane(pane, |_, st| {
                Response::PendingPrompt(st.agent.pending_prompt(pane))
            }),
            Request::ResumeRecords { pane } => {
                let (mode, rules) = {
                    let cfg = self.config.read();
                    (cfg.session.resume_programs, cfg.resume_rules())
                };
                let targets: Vec<Arc<Pane>> = match pane {
                    Some(id) => vec![self.pane(id).ok_or_else(|| format!("unknown pane {id}"))?],
                    None => self.all_panes(),
                };
                // A client still reading earlier answers gets no more (each can be large).
                if client.queued.load(Ordering::Relaxed) > OUTPUT_BACKLOG_BYTES {
                    return Err("busy: read the previous answers first".into());
                }
                let all = pane.is_none();
                let mut list = Vec::with_capacity(targets.len());
                let mut bytes = 0;
                for p in targets {
                    let records = {
                        let st = p.state.lock();
                        st.resume.inspect(st.pty.foreground_pgrp(), mode, &rules)
                    };
                    let entry = thurm_proto::PaneResumeRecords {
                        pane: p.id,
                        records,
                    };
                    bytes += resume_records_weight(std::slice::from_ref(&entry));
                    if all && bytes > RESUME_LIST_BYTES {
                        return Err(
                            "too many program resume records to list at once: ask for one pane \
                             (thurm resume --pane ID)"
                                .into(),
                        );
                    }
                    list.push(entry);
                }
                Ok(Response::ResumeRecords(list))
            }
            Request::TakeResume { ticket } => {
                // Only this user's own processes on this machine (the pane's shell).
                if !client.is_local_peer() {
                    return Err("not allowed".into());
                }
                let spec = {
                    let mut tickets = self.resume_tickets.lock();
                    let now = Instant::now();
                    let pos = tickets.iter().position(|t| {
                        t.expires > now && program_resume::ticket_eq(&t.ticket, &ticket)
                    });
                    pos.map(|i| tickets.swap_remove(i))
                };
                let ResumeTicket {
                    pane, spec, policy, ..
                } = spec.ok_or("unknown or used resume ticket")?;
                // Issued under other resume settings (reloaded, or another image's since an
                // upgrade): what let it resume may no longer.
                if policy != resume_policy(&self.config.read()) {
                    if let Some(pane) = self.pane(pane) {
                        pane.state
                            .lock()
                            .term
                            .print("\r\n[automatic resume skipped: resume settings changed]\r\n");
                    }
                    return Err(
                        "skipped: resume settings changed since the pane was restored".into(),
                    );
                }
                // Someone typed before the shell's first prompt: what they typed would go to
                // the resumed program (or join its line), so it isn't resumed. Every shell's
                // hook comes here; the typed line checks `typed` itself.
                if let Some(pane) = self.pane(pane) {
                    let mut st = pane.state.lock();
                    if st.typed {
                        st.term.print("\r\n[automatic resume skipped: the terminal was busy]\r\n");
                        return Err("skipped: input was typed before the shell was ready".into());
                    }
                }
                serde_json::to_string(&spec)
                    .map(Response::Text)
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// Pastes `text` into `pane` as [`Request::Paste`] does, for `client`, unless it has gone
    /// since it asked: files it uploaded, once they landed.
    pub(crate) fn paste_for(&self, client: u64, pane: PaneId, text: &str) -> Result<(), String> {
        self.paste_for_then(client, pane, text, || {})
    }

    /// [`Daemon::paste_for`], running `between` after the client is checked and before the
    /// write (tests).
    fn paste_for_then(
        &self,
        client: u64,
        pane: PaneId,
        text: &str,
        between: impl FnOnce(),
    ) -> Result<(), String> {
        let gone = || "the app sending the files went away".to_owned();
        let asker = self.clients.lock().get(&client).cloned().ok_or_else(gone)?;
        // The client stays from this check through the write: `remove_client` waits for it. A
        // client that went away asked for nothing more (it is marked before its uploads fail).
        let _exit = asker.exit.lock();
        if asker.gone.load(Ordering::SeqCst) {
            return Err(gone());
        }
        between();
        self.with_input_pane(client, pane, |p, st| {
            // A program that ended while its files came has nothing to type them to.
            if !st.info.alive {
                return Err("This terminal has closed.".to_owned());
            }
            self.paste_text(&asker, pane, p, st, text)
        })?
    }

    /// Types `text` into pane `pane` as a paste for `client`: what [`Request::Paste`] and the
    /// paste of uploaded files' paths both do.
    fn paste_text(
        &self,
        client: &Client,
        pane: PaneId,
        p: &Arc<Pane>,
        st: &mut PaneState,
        text: &str,
    ) -> Result<(), String> {
        if !text.is_empty() {
            self.typing_frozen(pane, st)?;
        }
        self.used_by(client, st);
        if !text.is_empty() {
            st.typed = true;
            st.agent.prompt.interrupted();
        }
        st.agent.user_input();
        self.acknowledge_programs(pane, st);
        if !text.is_empty() {
            self.pane_read(pane);
        }
        p.write(st.term.paste(text));
        Ok(())
    }

    /// [`Request::Ext`]: a method of the extension API, its parameters and result as JSON text.
    fn ext_request(
        self: &Arc<Self>,
        client: &Arc<Client>,
        method: &str,
        params: &str,
    ) -> Result<Response, String> {
        let params = if params.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(params).map_err(|e| format!("parameters: {e}"))?
        };
        let result = self.ext_call(client, method, params)?;
        Ok(Response::Text(result.to_string()))
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

    /// Runs the `[[on]]` hooks for `event` in the pane `info` describes, with `payload` (see
    /// `emit`) on their stdin. A finished command's `duration_secs` is checked against
    /// `min_secs`, which other events ignore.
    fn run_hooks(&self, event: HookEvent, info: &PaneInfo, payload: &serde_json::Value) {
        let secs = payload
            .get("duration_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
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
        let resume_changed = {
            let prev = self.config.read();
            if cfg.ai != prev.ai {
                // Turned off, or on again: ask the model afresh (it may have become available).
                self.ai.stop();
            }
            cfg.session.resume_programs != prev.session.resume_programs
                || cfg.session.resume_default_rules != prev.session.resume_default_rules
                || cfg.session.resume_rules != prev.session.resume_rules
        };
        *self.engine.write() = EngineConfig::from_config(&cfg, self.dark.load(Ordering::Relaxed));
        *self.agent_defs.write() = cfg.agent_defs();
        *self.program_defs.write() = cfg.program_defs();
        self.push.set_enabled(!cfg.push.relay.is_empty());
        self.tailnet.configure(&cfg.tailscale);
        let session_titles = cfg.agents.session_titles;
        *self.config.write() = cfg;
        self.ext_config_applied();
        let engine = self.engine.read().clone();
        for pane in self.all_panes() {
            let mut st = pane.state.lock();
            st.term.set_config(engine.clone());
            st.agent.titles = session_titles;
            drop(st);
        }
        if resume_changed {
            self.drop_stale_tickets();
            self.session_dirty.store(true, Ordering::Relaxed);
            let _ = self.save_session();
        }
        self.broadcast(Event::ConfigReloaded, false);
    }

    /// Forgets the resume tickets issued under other resume settings than the current ones,
    /// and the helper lines still to be typed for them (`TakeResume` refuses them anyway).
    fn drop_stale_tickets(&self) {
        let policy = resume_policy(&self.config.read());
        let mut stale = Vec::new();
        self.resume_tickets.lock().retain(|t| {
            let keep = t.policy == policy;
            if !keep {
                stale.push(t.pane);
            }
            keep
        });
        for id in stale {
            if let Some(pane) = self.pane(id) {
                let mut st = pane.state.lock();
                // A ticket's pane gets no other typed line: this one is the helper's.
                st.pending_input = None;
                st.term
                    .print("\r\n[automatic resume skipped: resume settings changed]\r\n");
            }
        }
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

impl crate::ports::Host for Daemon {
    fn wants_ports(&self) -> bool {
        // Not under the clients lock: the registry's is taken alone.
        self.ext_watches_ports()
            || self
                .clients
                .lock()
                .values()
                .any(|c| c.port_events.load(Ordering::Relaxed) && !c.gone.load(Ordering::Relaxed))
    }

    fn pane_roots(&self) -> Vec<(PaneId, u32)> {
        self.all_panes()
            .iter()
            .filter_map(|p| {
                let st = p.state.lock();
                if st.info.alive && !st.closed {
                    Some((p.id, st.info.pid?))
                } else {
                    None
                }
            })
            .collect()
    }

    fn send_ports(&self, event: Event, except: &[u64]) {
        self.ext_ports_changed(&event, except);
        let clients: Vec<_> = self.clients.lock().values().cloned().collect();
        for c in clients {
            if c.port_events.load(Ordering::Relaxed) && !except.contains(&c.id) {
                c.send(ServerMessage::Event(event.clone()));
            }
        }
    }

    fn send_ports_to(&self, client: u64, event: Event) {
        let Some(c) = self.clients.lock().get(&client).cloned() else {
            return;
        };
        // A plugin (or socket client) that joined by subscribing to `ports_changed`.
        self.ext_ports_to(&c, &event);
        if c.port_events.load(Ordering::Relaxed) {
            c.send(ServerMessage::Event(event));
        }
    }

    fn served(&self, joined: &[u64]) {
        self.ext_ports_served(joined);
    }

    fn watcher(&self) -> &crate::ports::Watcher {
        &self.ports
    }
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

/// The panes of an `OwnPanes` or `DisownPanes`, each once, in the order first named.
fn claim_list(panes: Vec<PaneId>) -> Result<Vec<PaneId>, String> {
    if panes.len() > MAX_CLAIMED_PANES {
        return Err(format!(
            "too many panes: {} (at most {MAX_CLAIMED_PANES})",
            panes.len()
        ));
    }
    let mut seen = HashSet::new();
    Ok(panes.into_iter().filter(|p| seen.insert(*p)).collect())
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
    let path = agent.settings_path();
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
    fn claim_lists_are_bounded_and_name_each_pane_once() {
        assert_eq!(claim_list(vec![3, 4, 3, 3, 5, 4]).unwrap(), [3, 4, 5]);
        assert_eq!(claim_list(vec![7; MAX_CLAIMED_PANES]).unwrap(), [7]);
        let err = claim_list(vec![7; MAX_CLAIMED_PANES + 1]).unwrap_err();
        assert!(err.contains("too many panes"), "{err}");
    }

    fn agent(status: AgentStatus, permission: Option<u64>) -> AgentState {
        AgentState {
            updated_at_ms: None,
            status_since_ms: None,
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
    fn output_keeps_the_group_it_was_read_under() {
        // A job's report, then the shell's prompt, read in one batch.
        assert_eq!(
            read_groups(10, &[(0, Some(7)), (6, Some(100))]),
            vec![(0, 6, Some(7)), (6, 10, Some(100))]
        );
        assert_eq!(read_groups(4, &[(0, Some(7))]), vec![(0, 4, Some(7))]);
        // Nothing recorded: no group, and the report falls back to the terminal's now.
        assert_eq!(read_groups(4, &[]), vec![(0, 4, None)]);
        assert_eq!(read_groups(0, &[]), vec![(0, 0, None)]);
    }

    /// A client leaving while files are pasted for it waits for the paste: nothing is typed for
    /// a client after it is gone.
    #[test]
    fn a_client_leaves_only_after_a_paste_for_it() {
        let dir = std::env::temp_dir().join(format!("thurm-paste-exit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let pane = daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["cat".into()]),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let (client, _rx) = daemon.add_client();
        let id = client.id;
        let (checked_tx, checked) = crossbeam_channel::bounded(1);
        let (release_tx, release) = crossbeam_channel::bounded::<()>(1);
        let pasting = {
            let daemon = daemon.clone();
            std::thread::spawn(move || {
                daemon.paste_for_then(id, pane, "a.txt ", || {
                    checked_tx.send(()).unwrap();
                    let _ = release.recv();
                })
            })
        };
        checked.recv().unwrap();
        let (left_tx, left) = crossbeam_channel::bounded(1);
        {
            let daemon = daemon.clone();
            std::thread::spawn(move || {
                daemon.remove_client(id);
                let _ = left_tx.send(());
            });
        }
        // The paste holds the client: its leaving waits.
        assert!(
            left.recv_timeout(Duration::from_millis(300)).is_err(),
            "the client left while a paste for it was being written"
        );
        release_tx.send(()).unwrap();
        assert!(pasting.join().unwrap().is_ok());
        left.recv_timeout(Duration::from_secs(5))
            .expect("the client never left");
        // Gone, it is pasted for no more.
        assert!(daemon.paste_for(id, pane, "b.txt ").is_err());
        daemon.close_pane(pane);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An upload asked for by a client that has gone by the time it is handled admits nothing.
    #[test]
    fn an_upload_handled_after_its_client_left_is_refused() {
        let dir = std::env::temp_dir().join(format!("thurm-upload-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let pane = daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["cat".into()]),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let (client, _rx) = daemon.add_client();
        daemon.remove_client(client.id);
        let got = daemon.dispatch(
            &client,
            Request::UploadBegin {
                pane,
                name: "a.txt".into(),
                size: 1,
                batch: None,
            },
        );
        assert!(matches!(&got, Err(e) if e.contains("went away")), "{got:?}");
        assert!(!dir.join("state/drops").join(pane.to_string()).exists());
        daemon.close_pane(pane);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A job that reports and exits at once can have given the terminal back by the time its
    /// bytes are parsed: the report is the group's that had it when they were read.
    #[test]
    fn a_report_belongs_to_the_group_it_was_read_under() {
        let dir = std::env::temp_dir().join(format!("thurm-report-group-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        // A pane that prints nothing, so its own parser doesn't run meanwhile.
        let id = daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["sleep".into(), "30".into()]),
                    size: PaneSize {
                        cols: 80,
                        rows: 24,
                        cell_width: 8,
                        cell_height: 16,
                    },
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let pane = daemon.pane(id).unwrap();
        let job = 1_000_000;
        assert_ne!(pane.state.lock().pty.foreground_pgrp(), Some(job));
        daemon.parse_output(&pane, b"\x1b]7501;state=working:app=x\x1b\\", Some(job));
        assert_eq!(pane.state.lock().programs_pgrp, Some(job));
        daemon.close_pane(id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Closing a pane hangs up its pty, and its reader then sees the program end. Clients have
    /// dropped the pane by then: a PaneInfo would bring it back to them, with its agent still
    /// counted on the Dock badge.
    #[test]
    fn a_closed_pane_is_not_heard_of_again() {
        let dir = std::env::temp_dir().join(format!("thurm-closed-quiet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let id = daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["sleep".into(), "30".into()]),
                    size: PaneSize {
                        cols: 80,
                        rows: 24,
                        cell_width: 8,
                        cell_height: 16,
                    },
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let pane = daemon.pane(id).unwrap();
        daemon.parse_output(&pane, b"\x1b]7501;state=working:app=x\x1b\\", None);
        let (client, rx) = daemon.add_client();
        client.ui.store(true, Ordering::Relaxed);
        daemon.close_pane(id);
        let mut closed = false;
        let mut after = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ServerMessage::Event(Event::PaneClosed { pane })) if pane == id => closed = true,
                Ok(ServerMessage::Event(ev)) if closed => after.push(ev),
                _ => {}
            }
        }
        assert!(closed);
        assert!(after.is_empty(), "heard after PaneClosed: {after:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exit can have looked at the pane before a close took it away: whether it speaks is
    /// decided under the pane's lock, where the close marks it, not by the pane map.
    #[test]
    fn an_exit_that_saw_the_pane_before_its_close_stays_quiet() {
        let dir = std::env::temp_dir().join(format!("thurm-closed-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let id = daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["sleep".into(), "30".into()]),
                    size: PaneSize {
                        cols: 80,
                        rows: 24,
                        cell_width: 8,
                        cell_height: 16,
                    },
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let pane = daemon.pane(id).unwrap();
        daemon.parse_output(&pane, b"\x1b]7501;state=working:app=x\x1b\\", None);
        let (client, rx) = daemon.add_client();
        client.ui.store(true, Ordering::Relaxed);
        daemon.close_pane(id);
        // The map as the exit saw it before the close.
        daemon.panes.lock().insert(id, pane.clone());
        daemon.pane_exited(&pane);
        daemon.panes.lock().remove(&id);
        let mut closed = false;
        let mut after = Vec::new();
        while let Ok(msg) = rx.recv_timeout(Duration::from_millis(200)) {
            match msg {
                ServerMessage::Event(Event::PaneClosed { pane }) if pane == id && !closed => {
                    closed = true
                }
                ServerMessage::Event(ev) if closed => after.push(ev),
                _ => {}
            }
        }
        assert!(closed);
        assert!(after.is_empty(), "heard after PaneClosed: {after:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The program in front of a pane whose shell runs behind a PTY proxy (`atuin pty-proxy`,
    /// here `script`) is the one on the proxy's terminal, not the proxy.
    #[test]
    fn the_foreground_is_seen_through_a_pty_proxy() {
        let dir = std::env::temp_dir().join(format!("thurm-pty-proxy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let foreground = |script: &str| {
            let id = daemon
                .create_pane(
                    CreatePane {
                        command: Some(vec!["sh".into(), "-c".into(), script.into()]),
                        size: PaneSize {
                            cols: 80,
                            rows: 24,
                            cell_width: 8,
                            cell_height: 16,
                        },
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            let pane = daemon.pane(id).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut name = None;
            while Instant::now() < deadline {
                name = pane
                    .state
                    .lock()
                    .pty
                    .foreground_pgrp()
                    .and_then(procinfo::process_info)
                    .map(|p| p.name);
                if name.as_deref() == Some("sleep") {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            daemon.close_pane(id);
            name
        };
        #[cfg(target_os = "macos")]
        let proxied = "script -q /dev/null /bin/sleep 30";
        #[cfg(not(target_os = "macos"))]
        let proxied = "script -qc 'exec /bin/sleep 30' /dev/null";
        assert_eq!(foreground(proxied).as_deref(), Some("sleep"));
        // Without job control the shell's job is in the shell's group: no proxy, the shell.
        let plain = foreground("/bin/sleep 30; :");
        assert!(
            plain.as_deref().is_some_and(procinfo::is_shell),
            "{plain:?}"
        );
        // A proxy in the shell's group but in the background, its input a pipe: the shell
        // still reads the pane, and is what is in front.
        #[cfg(target_os = "macos")]
        let background = "/bin/sleep 30 | script -q /dev/null /bin/sleep 30 & wait";
        #[cfg(not(target_os = "macos"))]
        let background = "/bin/sleep 30 | script -qc 'exec /bin/sleep 30' /dev/null & wait";
        let behind = foreground(background);
        assert!(
            behind.as_deref().is_some_and(procinfo::is_shell),
            "{behind:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// However the reads split a report, the read its end is in (where the terminal sees it)
    /// is looked at; output without reports never is.
    #[test]
    fn reports_are_found_across_reads() {
        let stream = b"ab\x1b]7501;state=working:app=x\x1b\\cd\x1b]7501;app=y\x07ef";
        // Where each report ends: the first ESC or BEL after its marker.
        let ends: Vec<usize> = memchr::memmem::find_iter(stream, ReportScan::MARKER)
            .map(|m| {
                m + stream[m..]
                    .iter()
                    .position(|&b| b == 0x1b || b == 0x07)
                    .unwrap()
            })
            .collect();
        assert_eq!(ends.len(), 2);
        let check = |cuts: &[usize]| {
            let mut scan = ReportScan::default();
            let mut start = 0;
            for &end in cuts.iter().chain([&stream.len()]) {
                let touched = scan.read(&stream[start..end]);
                for &e in &ends {
                    if (start..end).contains(&e) {
                        assert!(
                            touched,
                            "report end {e} missed in {start}..{end} ({cuts:?})"
                        );
                    }
                }
                start = end;
            }
        };
        for a in 0..=stream.len() {
            check(&[a]);
            for b in a..=stream.len() {
                check(&[a, b]);
            }
        }
        let mut scan = ReportScan::default();
        for chunk in b"plain ]750 output\x1b]0;title\x07 and 1;7501;".chunks(3) {
            assert!(!scan.read(chunk), "{chunk:?}");
        }
    }

    /// A program behind a PTY proxy reports, then ends without a prompt mark: its report is
    /// over once the shell behind the proxy has the terminal back.
    #[test]
    fn a_report_behind_a_pty_proxy_ends_with_its_program() {
        let dir = std::env::temp_dir().join(format!("thurm-proxy-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let report = dir.join("report.sh");
        std::fs::write(
            &report,
            "printf '\\033]7501;state=working:app=x\\033\\\\'\nsleep 1\n",
        )
        .unwrap();
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        // An interactive shell (job control) behind `script`.
        #[cfg(target_os = "macos")]
        let proxied = "script -q /dev/null /bin/sh -i; :";
        #[cfg(not(target_os = "macos"))]
        let proxied = "script -qc '/bin/sh -i' /dev/null; :";
        let id = daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["sh".into(), "-c".into(), proxied.into()]),
                    size: PaneSize {
                        cols: 80,
                        rows: 24,
                        cell_width: 8,
                        cell_height: 16,
                    },
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let pane = daemon.pane(id).unwrap();
        let wait_for = |what: &str, done: &dyn Fn(&PaneState) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done(&pane.state.lock()) {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let pane_pid = pane.state.lock().info.pid;
        let inner_shell = |st: &PaneState| {
            let fg = st.pty.foreground_pgrp();
            fg != pane_pid
                && fg
                    .and_then(procinfo::process_info)
                    .is_some_and(|p| procinfo::is_shell(&p.name))
        };
        wait_for("the shell behind the proxy", &inner_shell);
        pane.write(format!("sh {}\n", report.display()).into_bytes());
        wait_for("the report", &|st| {
            st.programs_pgrp.is_some() && st.programs_pgrp != pane_pid
        });
        assert!(!shell_took_the_terminal_back(&pane.state.lock()));
        wait_for("the shell to have the terminal back", &|st| {
            shell_took_the_terminal_back(st)
        });
        daemon.close_pane(id);
        let _ = std::fs::remove_dir_all(&dir);
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
            revoked: AtomicBool::new(false),
            active_size: AtomicBool::new(false),
            pane_pager: AtomicBool::new(false),
            pane_owner: AtomicBool::new(false),
            state_events: AtomicBool::new(false),
            agent_activity: AtomicBool::new(false),
            pending_prompt: AtomicBool::new(false),
            ui_layout: AtomicBool::new(false),
            agent_wall: AtomicBool::new(false),
            floating: AtomicBool::new(false),
            floating_new: AtomicBool::new(false),
            floating_place: AtomicBool::new(false),
            floating_minimize: AtomicBool::new(false),
            ext: AtomicBool::new(false),
            ext_ui: AtomicBool::new(false),
            ext_forwards: AtomicBool::new(false),
            port_events: AtomicBool::new(false),
            plugin: Mutex::new(None),
            gone: AtomicBool::new(false),
            exit: parking_lot::ReentrantMutex::new(()),
            deflate: Arc::new(AtomicBool::new(false)),
            principal: Mutex::new(None),
            name: Mutex::new(String::new()),
            queued: std::sync::atomic::AtomicUsize::new(0),
            unflushed: std::sync::atomic::AtomicUsize::new(0),
            last_written: Arc::new(Mutex::new(Instant::now())),
            clip_query: AtomicBool::new(false),
            clip_verified: AtomicBool::new(false),
            clip_queued: std::sync::atomic::AtomicUsize::new(0),
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

    /// A large copy queued for a client counts against the clipboard bytes held, not its output
    /// backlog: its panes do not fall back to re-sync because of it.
    #[test]
    fn a_large_copy_is_no_output_backlog() {
        let dir = std::env::temp_dir().join(format!("thurm-clip-backlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let (client, _rx) = daemon.add_client();
        client.send(ServerMessage::Event(Event::ClipboardWrite {
            pane: 1,
            id: 1,
            text: Some("x".repeat(thurm_proto::CLIP_MAX)),
        }));
        assert!(!client.backlogged());
        assert_eq!(
            client.clip_queued.load(Ordering::SeqCst),
            thurm_proto::CLIP_MAX
        );
        // An OSC 52 copy, sent to every app and phone, is not the tools' to count: a phone
        // that stops reading must not stop them.
        client.send(ServerMessage::Event(Event::ClipboardStore {
            pane: 1,
            text: "y".repeat(1000),
        }));
        assert_eq!(
            client.clip_queued.load(Ordering::SeqCst),
            thurm_proto::CLIP_MAX
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Clipboard bytes queued for a client are counted until written, and once its writer has
    /// stopped, none are: a send failing after that cannot take the count below zero.
    #[test]
    fn clipboard_bytes_queued_are_counted_once() {
        let dir = std::env::temp_dir().join(format!("thurm-clip-queued-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let (client, rx) = daemon.add_client();
        let data = |n| ServerMessage::Response {
            id: 1,
            result: Ok(Response::ClipboardData { data: vec![0; n] }),
        };
        client.send(data(100));
        client.send(ServerMessage::Event(Event::ClipboardWrite {
            pane: 1,
            id: 2,
            text: Some("x".repeat(30)),
        }));
        assert_eq!(client.clip_queued.load(Ordering::SeqCst), 130);
        let first = rx.recv().unwrap();
        client.clip_written(&first);
        assert_eq!(client.clip_queued.load(Ordering::SeqCst), 30);
        // The writer stops with a message still queued, and a send fails after it.
        drop(rx);
        client.writer_ended();
        client.send(data(50));
        assert_eq!(
            client.clip_queued.load(Ordering::SeqCst),
            0,
            "not wrapped, not 50"
        );
        client.clip_written(&data(10));
        assert_eq!(
            client.clip_queued.load(Ordering::SeqCst),
            0,
            "never below zero"
        );
        let _ = std::fs::remove_dir_all(dir);
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

    /// The last plugin to hear of ports unsubscribes: the watch stops at once, not at its next
    /// look (seconds or minutes away).
    #[test]
    fn the_port_watch_stops_when_its_last_subscriber_leaves() {
        let dir = std::env::temp_dir().join(format!("thurm-ports-leave-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let (client, _rx) = daemon.add_client();
        client.ext.store(true, Ordering::SeqCst);
        let ext = |method: &str| Request::Ext {
            method: method.into(),
            params: r#"{"names":["ports_changed"]}"#.into(),
        };
        let wait = |idle: bool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while daemon.ports.idle() != idle {
                assert!(Instant::now() < deadline, "the watch is not idle={idle}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        daemon.handle(&client, 1, ext(thurm_proto::ext::method::EVENTS_SUBSCRIBE));
        wait(false);
        // Its first look is done: the next is the shortest wait away.
        std::thread::sleep(Duration::from_millis(500));
        daemon.handle(
            &client,
            2,
            ext(thurm_proto::ext::method::EVENTS_UNSUBSCRIBE),
        );
        wait(true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The watch stops as soon as its last subscriber goes without unsubscribing, too.
    #[test]
    fn the_port_watch_stops_when_its_last_subscriber_goes() {
        let dir = std::env::temp_dir().join(format!("thurm-ports-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let (client, _rx) = daemon.add_client();
        client.ext.store(true, Ordering::SeqCst);
        let wait = |idle: bool| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while daemon.ports.idle() != idle {
                assert!(Instant::now() < deadline, "the watch is not idle={idle}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        daemon.handle(
            &client,
            1,
            Request::Ext {
                method: thurm_proto::ext::method::EVENTS_SUBSCRIBE.into(),
                params: r#"{"names":["ports_changed"]}"#.into(),
            },
        );
        wait(false);
        // Its first look is done: the next is the shortest wait away.
        std::thread::sleep(Duration::from_millis(500));
        daemon.remove_client(client.id);
        wait(true);
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
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
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
        // Both are unread all the same: reading the pane takes either down on the desktops.
        assert!(
            daemon.unread.lock().contains(&1),
            "a program notification is not unread"
        );
        assert!(daemon.unread.lock().contains(&2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A program's notification is unread until its pane is read, once.
    #[test]
    fn a_program_notification_is_unread_until_read() {
        let dir = std::env::temp_dir().join(format!("thurm-unread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        daemon.broadcast(
            Event::Notify {
                pane: 1,
                title: "make finished".into(),
                body: "after 20s".into(),
                permission: None,
                sound: None,
            },
            true,
        );
        assert!(daemon.unread.lock().contains(&1));
        daemon.pane_read(1);
        assert!(!daemon.unread.lock().contains(&1));
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

    #[test]
    fn get_pending_prompt_returns_active_prompt_and_none_after_answer() {
        let dir = std::env::temp_dir().join(format!("thurm-get-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let (client, _rx) = daemon.add_client();
        let pane = daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["cat".into()]),
                    ..Default::default()
                },
                None,
            )
            .unwrap();

        assert!(matches!(
            daemon
                .dispatch(&client, Request::GetPendingPrompt { pane })
                .unwrap(),
            Response::PendingPrompt(None)
        ));

        {
            let p = daemon.pane(pane).unwrap();
            let mut st = p.state.lock();
            st.agent.saw_foreground(Some(1));
            st.agent.apply_hook(
                Some(1),
                "claude",
                "Claude Code",
                "permission-prompt",
                Some("s1".into()),
                Some("Bash: cargo check".into()),
            );
            st.agent.refresh();
        }

        let resp = daemon
            .dispatch(&client, Request::GetPendingPrompt { pane })
            .unwrap();
        let Response::PendingPrompt(Some(prompt)) = resp else {
            panic!("expected PendingPrompt(Some), got {resp:?}");
        };
        assert_eq!(prompt.pane, pane);
        assert_eq!(prompt.title, "Claude Code");
        assert_eq!(prompt.body, "Bash: cargo check");
        assert_eq!(prompt.request.as_deref(), Some("Bash: cargo check"));

        // Verify Hello advertises CAP_PENDING_PROMPT and NotifyPrompt downgrades for legacy clients.
        let (legacy, legacy_rx) = daemon.add_client();
        legacy.ui.store(true, Ordering::Relaxed);
        let (modern, modern_rx) = daemon.add_client();
        modern.ui.store(true, Ordering::Relaxed);
        let hello = daemon
            .dispatch(
                &modern,
                Request::Hello {
                    client: "test".into(),
                    version: PROTOCOL_VERSION,
                    ui: true,
                    capabilities: vec![thurm_proto::CAP_PENDING_PROMPT.into()],
                },
            )
            .unwrap();
        let Response::Hello { capabilities, .. } = hello else {
            panic!("expected Hello");
        };
        assert!(capabilities.iter().any(|c| c == thurm_proto::CAP_PENDING_PROMPT));
        daemon.broadcast(
            Event::NotifyPrompt {
                pane,
                title: "Claude Code".into(),
                body: "Bash: cargo check".into(),
                permission: Some(prompt.prompt),
                sound: Some("default".into()),
                request: Some("Bash: cargo check".into()),
            },
            true,
        );
        assert!(matches!(
            legacy_rx.try_recv(),
            Ok(ServerMessage::Event(Event::Notify {
                pane: p,
                permission: Some(id),
                ..
            })) if p == pane && id == prompt.prompt
        ));
        assert!(matches!(
            modern_rx.try_recv(),
            Ok(ServerMessage::Event(Event::NotifyPrompt {
                pane: p,
                permission: Some(id),
                request: Some(ref r),
                ..
            })) if p == pane && id == prompt.prompt && r == "Bash: cargo check"
        ));

        assert!(matches!(
            daemon
                .dispatch(
                    &client,
                    Request::AnswerPermission {
                        pane,
                        prompt: prompt.prompt,
                        allow: true,
                    },
                )
                .unwrap(),
            Response::Ok
        ));
        assert!(matches!(
            daemon
                .dispatch(&client, Request::GetPendingPrompt { pane })
                .unwrap(),
            Response::PendingPrompt(None)
        ));

        daemon.close_pane(pane);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notify_prompt_downgrades_for_legacy_clients_and_get_pending_prompt_answers() {
        let dir = std::env::temp_dir().join(format!("thurm-pending-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let (legacy, legacy_rx) = daemon.add_client();
        legacy.ui.store(true, Ordering::Relaxed);
        let (modern, modern_rx) = daemon.add_client();
        modern.ui.store(true, Ordering::Relaxed);
        modern.pending_prompt.store(true, Ordering::Relaxed);

        // Hello advertises CAP_PENDING_PROMPT.
        let hello = daemon
            .dispatch(
                &modern,
                Request::Hello {
                    client: "test".into(),
                    version: PROTOCOL_VERSION,
                    ui: true,
                    capabilities: vec![thurm_proto::CAP_PENDING_PROMPT.into()],
                },
            )
            .unwrap();
        let Response::Hello { capabilities, .. } = hello else {
            panic!("expected Hello");
        };
        assert!(capabilities.iter().any(|c| c == thurm_proto::CAP_PENDING_PROMPT));

        // NotifyPrompt downgrades to Notify for clients without CAP_PENDING_PROMPT.
        daemon.broadcast(
            Event::NotifyPrompt {
                pane: 3,
                title: "Claude Code".into(),
                body: "Bash: git status".into(),
                permission: Some(7),
                sound: Some("default".into()),
                request: Some("Bash: git status".into()),
            },
            true,
        );
        assert!(matches!(
            legacy_rx.try_recv(),
            Ok(ServerMessage::Event(Event::Notify {
                pane: 3,
                permission: Some(7),
                ..
            }))
        ));
        assert!(matches!(
            modern_rx.try_recv(),
            Ok(ServerMessage::Event(Event::NotifyPrompt {
                pane: 3,
                permission: Some(7),
                request: Some(ref r),
                ..
            })) if r == "Bash: git status"
        ));

        // GetPendingPrompt returns active prompt for a real pane, then None once cleared.
        let pane = daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["cat".into()]),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        {
            let p = daemon.pane(pane).unwrap();
            let mut st = p.state.lock();
            st.agent.saw_foreground(Some(1));
            st.agent.apply_hook(
                Some(1),
                "claude",
                "Claude Code",
                "permission-prompt",
                Some("s1".into()),
                Some("Bash: cargo check".into()),
            );
            st.agent.refresh();
        }
        let resp = daemon
            .dispatch(&modern, Request::GetPendingPrompt { pane })
            .unwrap();
        let Response::PendingPrompt(Some(prompt)) = resp else {
            panic!("expected PendingPrompt(Some)");
        };
        assert_eq!(prompt.pane, pane);
        assert_eq!(prompt.request.as_deref(), Some("Bash: cargo check"));

        {
            let p = daemon.pane(pane).unwrap();
            let mut st = p.state.lock();
            st.agent.user_input();
            st.agent.refresh();
        }
        assert!(matches!(
            daemon
                .dispatch(&modern, Request::GetPendingPrompt { pane })
                .unwrap(),
            Response::PendingPrompt(None)
        ));
        daemon.close_pane(pane);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn push_relay_notice_omits_private_agent_topic() {
        use std::io::{BufRead, BufReader, Read, Write};
        let dir = std::env::temp_dir().join(format!("thurm-push-priv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) <= 2 {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = tx.send(String::from_utf8_lossy(&body).into_owned());
            }
        });

        let mut cfg = Config::default();
        cfg.push.relay = relay.clone();
        let daemon = Daemon::new(
            cfg,
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
                name: "iPhone-7f3a".into(),
            })
            .unwrap();

        let pane = daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["cat".into()]),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let events = {
            let p = daemon.pane(pane).unwrap();
            let mut st = p.state.lock();
            st.info.title = "previous-secret-shell-title".into();
            st.agent.saw_foreground(Some(1));
            st.agent.apply_hook(
                Some(1),
                "claude",
                "Claude Code",
                "session-title",
                Some("s1".into()),
                Some("Deploy API_KEY=supersecret".into()),
            );
            st.agent.apply_hook(
                Some(1),
                "claude",
                "Claude Code",
                "permission-prompt",
                Some("s1".into()),
                Some("Bash: curl -H 'Authorization: Bearer supersecret'".into()),
            );
            let new = st.agent.refresh().flatten();
            daemon.agent_notifications(pane, &st, None, new.as_ref())
        };
        assert!(!events.is_empty());
        for ev in events {
            daemon.broadcast_outgoing(ev, true);
        }

        let raw = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("push posted");
        let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(json.get("topic").is_none(), "relay payload leaked topic: {raw}");
        assert!(!raw.contains("supersecret"), "relay payload leaked secret: {raw}");
        assert!(!raw.contains("previous-secret-shell-title"), "relay payload leaked pane title: {raw}");
        assert_eq!(json["body"], "asks for permission");

        daemon.close_pane(pane);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("thurm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cat_pane(daemon: &Arc<Daemon>) -> PaneId {
        daemon
            .create_pane(
                CreatePane {
                    command: Some(vec!["cat".into()]),
                    ..Default::default()
                },
                None,
            )
            .unwrap()
    }

    fn ticket_for(pane: PaneId, ticket: &str, policy: String) -> ResumeTicket {
        ResumeTicket {
            ticket: ticket.to_owned(),
            pane,
            spec: ResumeExecSpec {
                argv: vec!["/bin/true".into()],
                cwd: None,
                require_cwd: false,
                cwd_id: None,
                exe_dirs: Vec::new(),
            },
            expires: Instant::now() + Duration::from_secs(60),
            policy,
        }
    }

    /// A pane command names the configured shell only when it is the same file with the same
    /// arguments: another shell of the same name is not interchangeable with it.
    #[test]
    fn a_shell_of_the_same_name_elsewhere_is_another_command() {
        let dir = test_dir("same-invocation");
        for d in ["a", "b", "c"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let a = dir.join("a/sh");
        let b = dir.join("b/sh");
        std::fs::write(&a, "#!/bin/sh\n").unwrap();
        std::fs::write(&b, "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink(&a, dir.join("c/sh")).unwrap();
        let s = |p: &Path| p.display().to_string();
        let a_s = s(&a);
        assert!(same_invocation(std::slice::from_ref(&a_s), &a_s, &[]));
        // The same file by another path.
        assert!(same_invocation(&[s(&dir.join("c/sh"))], &a_s, &[]));
        // The same file by another name (`sh` linked to `bash`): it may run differently.
        std::os::unix::fs::symlink(&a, dir.join("c/bash")).unwrap();
        assert!(!same_invocation(&[s(&dir.join("c/bash"))], &a_s, &[]));
        assert!(!same_invocation(
            std::slice::from_ref(&a_s),
            &s(&dir.join("c/bash")),
            &[]
        ));
        assert!(!same_invocation(&[s(&b)], &a_s, &[]));
        // The configured arguments must come along, and nothing else.
        assert!(!same_invocation(
            std::slice::from_ref(&a_s),
            &a_s,
            &["-l".into()]
        ));
        assert!(same_invocation(
            &[a_s.clone(), "-l".into()],
            &a_s,
            &["-l".into()]
        ));
        assert!(!same_invocation(&[a_s.clone(), "-x".into()], &a_s, &[]));
        assert!(!same_invocation(&[], &a_s, &[]));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A resume record's change that fails to save stays to be saved: the next tick retries
    /// instead of waiting for the snapshot interval.
    #[test]
    fn a_resume_save_that_fails_is_retried() {
        let dir = test_dir("resume-save-retry");
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(daemon.store.is_some());
        // Nothing can replace a non-empty directory where the snapshot goes.
        std::fs::create_dir_all(dir.join("state/session.json/x")).unwrap();
        daemon.session_dirty.store(false, Ordering::Relaxed);
        assert!(daemon.save_session_now().is_err());
        assert!(daemon.session_dirty.load(Ordering::Relaxed));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The shell's first prompt can already wait in the PTY when the next image adopts the
    /// pane: the line to type is in place before its reader sees it, and its waits so far
    /// come along.
    #[test]
    fn an_adopted_pane_types_its_line_at_a_prompt_already_waiting() {
        let dir = test_dir("adopt-pending");
        let old = Daemon::new(
            Config::default(),
            dir.join("a.sock"),
            dir.join("a"),
            Arc::new(AtomicBool::new(false)),
        );
        let id = cat_pane(&old);
        {
            let pane = old.pane(id).unwrap();
            let mut st = pane.state.lock();
            // Due long after the test: only the prompt can take it.
            st.pending_input = Some((
                Instant::now() + Duration::from_secs(600),
                b"RESUMED\n".to_vec(),
            ));
            st.pending_waits = 7;
        }
        assert!(old.quiesce_readers(Duration::from_secs(2), Duration::from_secs(5)));
        let mut h = old.handoff(-1, false);
        assert_eq!(h.panes[0].pending_waits, 7);
        // The new image's own copy of the PTY.
        let fd = unsafe { libc::dup(h.panes[0].fd) };
        assert!(fd >= 0);
        h.panes[0].fd = fd;
        // The prompt (`cat` prints it back, without the terminal's echo) waits unread.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(fd, &mut t), 0);
            t.c_lflag &= !libc::ECHO;
            assert_eq!(libc::tcsetattr(fd, libc::TCSANOW, &t), 0);
        }
        let prompt = b"\x1b]133;A\x07\n";
        assert_eq!(
            unsafe { libc::write(fd, prompt.as_ptr().cast(), prompt.len()) },
            prompt.len() as isize
        );
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut poll, 1, 5000) }, 1);

        let new = Daemon::new(
            Config::default(),
            dir.join("b.sock"),
            dir.join("b"),
            Arc::new(AtomicBool::new(false)),
        );
        new.adopt(h);
        let pane = new.pane(id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pane.state.lock().pending_input.is_some() {
            assert!(
                Instant::now() < deadline,
                "the prompt waiting in the PTY did not type the line"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(pane.state.lock().pending_waits, 7);
        new.close_pane(id);
        old.resume_readers();
        old.close_pane(id);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A ticket is redeemed only under the resume settings it was issued under: a reload that
    /// changes them forgets it (and its line to type), and one from another configuration (an
    /// upgrade's previous image) is refused.
    #[test]
    fn resume_tickets_follow_the_resume_settings() {
        let dir = test_dir("ticket-policy");
        let mut cfg = Config::default();
        cfg.session.resume_programs = thurm_config::ResumeProgramsMode::All;
        let daemon = Daemon::new(
            cfg.clone(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let id = cat_pane(&daemon);
        let pane = daemon.pane(id).unwrap();
        let t1 = "a1".repeat(16);
        daemon
            .resume_tickets
            .lock()
            .push(ticket_for(id, &t1, resume_policy(&cfg)));
        pane.state.lock().pending_input = Some((
            Instant::now() + Duration::from_secs(600),
            b" thurmd --exec-resume x\r".to_vec(),
        ));
        // A reload that leaves them alone keeps it.
        daemon.apply_config(cfg.clone());
        assert_eq!(daemon.resume_tickets.lock().len(), 1);
        assert!(pane.state.lock().pending_input.is_some());
        let mut off = cfg.clone();
        off.session.resume_programs = thurm_config::ResumeProgramsMode::Off;
        daemon.apply_config(off.clone());
        assert!(daemon.resume_tickets.lock().is_empty());
        assert!(pane.state.lock().pending_input.is_none());
        let (client, _rx) = daemon.add_client();
        let got = daemon.dispatch(&client, Request::TakeResume { ticket: t1 });
        assert!(got.is_err(), "{got:?}");

        // Issued under other settings than these: refused when redeemed.
        let t2 = "b2".repeat(16);
        daemon
            .resume_tickets
            .lock()
            .push(ticket_for(id, &t2, resume_policy(&cfg)));
        let got = daemon.dispatch(&client, Request::TakeResume { ticket: t2 });
        assert!(
            matches!(&got, Err(e) if e.contains("settings changed")),
            "{got:?}"
        );
        // Under these settings: redeemed.
        let t3 = "c3".repeat(16);
        daemon
            .resume_tickets
            .lock()
            .push(ticket_for(id, &t3, resume_policy(&off)));
        let got = daemon.dispatch(&client, Request::TakeResume { ticket: t3 });
        assert!(matches!(&got, Ok(Response::Text(_))), "{got:?}");
        daemon.close_pane(id);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// While an upgrade hands the panes over, a resume line that comes due stays pending (the
    /// hand-off may carry it for the next image to type); if the upgrade gives up, this
    /// image services it again.
    #[test]
    fn a_resume_line_due_during_an_upgrade_stays_pending() {
        let dir = test_dir("upgrade-pending");
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let id = cat_pane(&daemon);
        let pane = daemon.pane(id).unwrap();
        let due = Instant::now();
        pane.state.lock().pending_input = Some((due, b"x\r".to_vec()));
        daemon.set_upgrading(true);
        daemon.type_pending_input(&pane, &mut pane.state.lock());
        {
            let st = pane.state.lock();
            assert_eq!(st.pending_input, Some((due, b"x\r".to_vec())));
            assert_eq!(st.pending_waits, 0);
        }
        // Serviced again (`cat` is no shell: it waits for one).
        daemon.set_upgrading(false);
        daemon.type_pending_input(&pane, &mut pane.state.lock());
        {
            let st = pane.state.lock();
            assert_eq!(st.pending_waits, 1);
            assert!(st.pending_input.as_ref().is_some_and(|(d, _)| *d > due));
        }
        daemon.close_pane(id);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// While an upgrade hands the panes over, the first input into a pane waiting to resume
    /// is refused: the `typed` the hand-off carries stays true to what the shell got.
    #[test]
    fn input_that_would_cancel_a_resume_waits_out_an_upgrade() {
        let dir = test_dir("upgrade-input");
        let daemon = Daemon::new(
            Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let waiting = cat_pane(&daemon);
        let plain = cat_pane(&daemon);
        let ticketed = cat_pane(&daemon);
        daemon.pane(waiting).unwrap().state.lock().pending_input =
            Some((Instant::now() + Duration::from_secs(600), b"x\r".to_vec()));
        daemon
            .resume_tickets
            .lock()
            .push(ticket_for(ticketed, &"d4".repeat(16), String::new()));
        let (client, _rx) = daemon.add_client();
        let input = |pane| {
            daemon.dispatch(
                &client,
                Request::Input {
                    pane,
                    data: b"a".to_vec(),
                },
            )
        };
        daemon.set_upgrading(true);
        assert!(input(waiting).is_err());
        assert!(input(ticketed).is_err());
        let paste = daemon.dispatch(
            &client,
            Request::Paste {
                pane: waiting,
                text: "a".into(),
            },
        );
        assert!(paste.is_err());
        let key = thurm_proto::KeyEvent {
            key: thurm_proto::Key::Char('a'),
            mods: 0,
            action: thurm_proto::KeyAction::Press,
            text: "a".into(),
            shifted: None,
            base_layout: None,
        };
        let typed = daemon.dispatch(
            &client,
            Request::Key {
                pane: ticketed,
                key,
            },
        );
        assert!(typed.is_err());
        assert!(!daemon.pane(waiting).unwrap().state.lock().typed);
        assert!(!daemon.pane(ticketed).unwrap().state.lock().typed);
        // Nothing to cancel in the others.
        assert!(input(plain).is_ok());
        // A resize types nothing: it goes through.
        let size = PaneSize {
            cols: 91,
            rows: 27,
            ..PaneSize::default()
        };
        for pane in [waiting, ticketed] {
            let resized = daemon.dispatch(&client, Request::Resize { pane, size });
            assert!(resized.is_ok(), "{resized:?}");
            let p = daemon.pane(pane).unwrap();
            let st = p.state.lock();
            assert_eq!(st.info.size.cols, 91);
            assert!(!st.typed);
        }
        // The upgrade gave up: input goes through again, and cancels the resume.
        daemon.set_upgrading(false);
        assert!(input(waiting).is_ok());
        assert!(daemon.pane(waiting).unwrap().state.lock().typed);
        for id in [waiting, plain, ticketed] {
            daemon.close_pane(id);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
