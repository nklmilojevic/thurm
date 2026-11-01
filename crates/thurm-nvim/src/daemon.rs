//! `thurm-nvim daemon BASE [nvim args]`: runs Neovim, keeps a pane per window, and draws
//! them. One thread owns all state; the others only read (Neovim, helpers, terminals) or run
//! blocking requests to Thurm, and post what happened as a [`Msg`].

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use rmpv::Value;
use thurm_client::{Client, ConnectOptions};
use thurm_proto::{CreatePane, PaneId, PaneSize, Request, Response, SplitDir, UiCommand};

use crate::cell::{FrameCell, Text};
use crate::grid::{Model, WinKind, style_from_get_hl};
use crate::input::{InputEvent, InputParser};
use crate::launch::{self, helper_socket, log_path, nvim_socket};
use crate::nvim::{self, Message, Nvim};
use crate::render::{PaneRenderer, enter_modes, leave_modes};
use crate::view::{PaneView, compose};
use crate::{fdpass, helper, tty};

const WINDOWS_LUA: &str = include_str!("windows.lua");

/// How long losing focus waits before Neovim hears of it: moving between our own panes is an
/// out then an in, which shouldn't fire FocusLost and FocusGained.
const FOCUS_OUT_DELAY: Duration = Duration::from_millis(80);

/// How long a focus change of ours keeps the panes' own focus reports from moving Neovim.
/// How long a lone ESC waits for the rest of a sequence split across reads before it counts as
/// the Escape key (with the kitty keyboard protocol, Escape comes as `CSI 27 u` anyway).
const ESCAPE_DELAY: Duration = Duration::from_millis(25);

/// How long Neovim may take to exit on its own before it is hung up on.
const NVIM_EXIT_GRACE: Duration = Duration::from_secs(2);

const OWN_FOCUS_GRACE: Duration = Duration::from_millis(600);

enum Msg {
    Nvim(Message),
    NvimGone,
    /// A helper connected and sent its terminal.
    Helper {
        id: Option<u32>,
        stream: UnixStream,
        tty: OwnedFd,
    },
    Resized(u32),
    HelperGone(u32),
    Input(u32, Vec<u8>),
    /// Thurm made the pane for our pane `id` (or couldn't).
    Created {
        id: u32,
        pane: Option<PaneId>,
    },
    FocusOutTimer(u64),
    /// A pane's read ended in a lone ESC, this many reads in: if no read came since, it was
    /// the Escape key.
    EscapeTimer(u32, u64),
    Signal,
}

/// Blocking requests to Thurm, run on their own thread.
enum ThurmOp {
    Split {
        id: u32,
        command: Vec<String>,
        from: Option<PaneId>,
        dir: SplitDir,
        size: (u16, u16),
    },
    Focus(PaneId),
}

/// One pane, showing one Neovim window.
struct Pane {
    /// Ours: 0 for the launcher's, then counting up. The helper says it in its hello.
    id: u32,
    /// Thurm's id for it, once known.
    thurm: Option<PaneId>,
    /// The Neovim window it shows; none while waiting for a reopened window.
    win: Option<i64>,
    /// Its window has had a grid: when the grid goes away, so does the window.
    seen_grid: bool,
    tty: Option<Tty>,
    helper: Option<UnixStream>,
    parser: InputParser,
    renderer: PaneRenderer,
    cols: u16,
    rows: u16,
    focused: bool,
    status: Option<Vec<FrameCell>>,
    /// The window size last asked of Neovim.
    asked_size: Option<(u16, u16)>,
    /// A bracketed paste is under way and Neovim has its first part (`nvim_paste` phase 1).
    pasting: bool,
    /// Reads from its terminal so far, to tell whether one came after a lone ESC.
    reads: u64,
}

struct Tty {
    file: File,
    saved: Option<libc::termios>,
}

impl Pane {
    fn new(id: u32, win: Option<i64>) -> Pane {
        Pane {
            id,
            thurm: None,
            win,
            seen_grid: false,
            tty: None,
            helper: None,
            parser: InputParser::new(),
            renderer: PaneRenderer::new(),
            cols: 80,
            rows: 24,
            focused: false,
            status: None,
            asked_size: None,
            pasting: false,
            reads: 0,
        }
    }

    fn view(&self, current: bool) -> PaneView<'_> {
        PaneView {
            cols: self.cols,
            rows: self.rows,
            grid: None,
            current,
            status: self.status.as_deref(),
        }
    }

    /// Gives the terminal back as it was, and hangs up the helper, which closes the pane.
    fn release(&mut self) {
        if let Some(mut t) = self.tty.take() {
            let mut out = Vec::new();
            leave_modes(&mut out);
            let _ = t.file.write_all(&out);
            if let Some(saved) = t.saved {
                tty::restore(t.file.as_raw_fd(), &saved);
            }
        }
        if let Some(h) = self.helper.take() {
            let _ = h.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// What a pending Neovim request was for.
enum Pending {
    UiAttach,
    ApiInfo,
    Setup,
    Statuslines(Vec<u32>),
    Ignore,
}

pub struct Daemon {
    tx: Sender<Msg>,
    nvim: Nvim,
    model: Model,
    panes: Vec<Pane>,
    next_id: u32,
    pending: HashMap<u32, Pending>,
    ready: bool,
    anchor: i64,
    /// Neovim's current window, as last reported.
    current: i64,
    /// Ordinary windows asked to become external.
    asked: HashSet<i64>,
    /// Windows closing to reopen (`:close` of the last one): their pane waits for the new one.
    reopening: HashSet<i64>,
    /// New windows and where Neovim put them: (side by side, before, split from).
    splits: HashMap<i64, (bool, bool, i64)>,
    statuslines_wanted: bool,
    statuslines_in_flight: bool,
    editor_size: (u16, u16),
    ui_focused: bool,
    focus_gen: u64,
    /// A Thurm focus change of ours, and when: until it lands, focus reports are stale.
    own_focus: Option<(PaneId, Instant)>,
    thurm_ops: Option<Sender<ThurmOp>>,
    /// Neovim closed our connection: it is exiting on its own.
    nvim_exiting: bool,
    exe: PathBuf,
    socket: PathBuf,
}

pub fn run(dir: &Path, nvim_args: &[String]) -> io::Result<()> {
    unsafe { libc::setsid() };
    let result = serve(dir, nvim_args);
    if let Err(e) = &result {
        log::error!("{e}");
    }
    let _ = std::fs::remove_file(helper_socket(dir));
    let _ = std::fs::remove_file(nvim_socket(dir));
    // The log stays when something went wrong, for the user to read.
    if result.is_ok() {
        let _ = std::fs::remove_file(log_path(dir));
        let _ = std::fs::remove_dir(dir);
    }
    result
}

/// Neovim, ended however the daemon ends.
///
/// Unless it is exiting on its own (`:qa`, which closes our connection while it still writes
/// files), it is hung up on, as when the terminal it runs in closes: it keeps the swap files of
/// changed buffers, for `:recover`, rather than lose the changes. One exiting on its own gets a
/// moment first: a hang-up then would keep swap files of a clean exit.
pub(crate) struct NvimProcess {
    pub(crate) child: Child,
    pub(crate) exiting: bool,
}

impl Drop for NvimProcess {
    fn drop(&mut self) {
        if self.exiting {
            let deadline = Instant::now() + NVIM_EXIT_GRACE;
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) };
        let _ = self.child.wait();
    }
}

fn serve(dir: &Path, nvim_args: &[String]) -> io::Result<()> {
    // `dir` is the launcher's fresh private directory; refuse anything else.
    if !thurm_config::private_dir_ok(dir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not private", dir.display()),
        ));
    }
    let socket = helper_socket(dir);
    let listener = UnixListener::bind(&socket)?;
    // Ready: the launcher becomes our first helper.
    io::stdout().write_all(b"r")?;
    io::stdout().flush()?;
    let devnull = File::open("/dev/null")?;
    unsafe { libc::dup2(devnull.as_raw_fd(), libc::STDOUT_FILENO) };

    let (tx, rx) = unbounded();
    {
        let tx = tx.clone();
        std::thread::spawn(move || accept_helpers(listener, tx));
    }
    {
        let (sig_r, sig_w) = UnixStream::pair()?;
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            signal_hook::low_level::pipe::register(s, sig_w.try_clone()?)?;
        }
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut b = [0u8; 8];
            if (&sig_r).read(&mut b).unwrap_or(0) > 0 {
                let _ = tx.send(Msg::Signal);
            }
        });
    }

    // The launcher's pane, before anything else: Neovim starts at its size.
    let first = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Msg::Helper {
            id: None,
            stream,
            tty,
        }) => (stream, tty),
        _ => return Err(io::Error::other("the launcher's pane never connected")),
    };

    let nvim_sock = nvim_socket(dir);
    let mut nvim_process = NvimProcess {
        child: spawn_nvim(&nvim_sock, nvim_args)?,
        exiting: false,
    };
    let (nvim, reader) = connect_retry(&nvim_sock, Duration::from_secs(10))?;
    {
        let tx = tx.clone();
        std::thread::spawn(move || read_nvim(reader, tx));
    }

    let mut d = Daemon {
        tx: tx.clone(),
        nvim,
        model: Model::default(),
        panes: Vec::new(),
        next_id: 1,
        pending: HashMap::new(),
        ready: false,
        anchor: 0,
        current: 0,
        asked: HashSet::new(),
        reopening: HashSet::new(),
        splits: HashMap::new(),
        statuslines_wanted: false,
        statuslines_in_flight: false,
        editor_size: (0, 0),
        ui_focused: true,
        focus_gen: 0,
        own_focus: None,
        thurm_ops: None,
        nvim_exiting: false,
        exe: std::env::current_exe()?,
        socket,
    };
    d.thurm_ops = start_thurm(tx.clone());
    let mut launcher = Pane::new(0, None);
    launcher.thurm = std::env::var(thurm_proto::ENV_PANE_ID)
        .ok()
        .and_then(|v| v.parse().ok());
    launcher.focused = true;
    d.panes.push(launcher);
    d.attach_tty(0, first.0, first.1);

    // However it ends, every terminal goes back as it was (Neovim goes with `nvim_process`).
    let result = d.start().and_then(|()| d.run_loop(&rx));
    for p in &mut d.panes {
        p.release();
    }
    nvim_process.exiting = d.nvim_exiting;
    result
}

fn spawn_nvim(socket: &Path, args: &[String]) -> io::Result<Child> {
    let _ = std::fs::remove_file(socket);
    let nvim = std::env::var_os("THURM_NVIM").unwrap_or_else(|| "nvim".into());
    Command::new(nvim)
        .arg("--headless")
        .arg("--listen")
        .arg(socket)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
}

fn connect_retry(socket: &Path, within: Duration) -> io::Result<(Nvim, BufReader<UnixStream>)> {
    let deadline = Instant::now() + within;
    loop {
        match Nvim::connect(socket) {
            Ok(c) => return Ok(c),
            Err(e) if Instant::now() > deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn read_nvim(mut reader: BufReader<UnixStream>, tx: Sender<Msg>) {
    loop {
        match nvim::read_message(&mut reader) {
            Ok(Some(m)) => {
                if tx.send(Msg::Nvim(m)).is_err() {
                    return;
                }
            }
            Ok(None) | Err(_) => {
                let _ = tx.send(Msg::NvimGone);
                return;
            }
        }
    }
}

fn accept_helpers(listener: UnixListener, tx: Sender<Msg>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // Only this user's helpers: the socket is in a private directory, but be sure.
        if !thurm_config::peer_is_same_user(&stream) {
            log::warn!("a helper of another user");
            continue;
        }
        let mut buf = [0u8; 32];
        let Ok((n, Some(fd))) = fdpass::recv(&stream, &mut buf) else {
            continue;
        };
        let Ok(id) = helper::parse_hello(&buf[..n]) else {
            log::warn!("a helper sent no hello");
            continue;
        };
        if tx
            .send(Msg::Helper {
                id,
                stream,
                tty: fd,
            })
            .is_err()
        {
            return;
        }
    }
}

/// Connects to Thurm and runs its requests on a thread of their own.
fn start_thurm(tx: Sender<Msg>) -> Option<Sender<ThurmOp>> {
    let client = match Client::connect(
        ConnectOptions {
            client_name: "thurm-nvim",
            ..Default::default()
        },
        |_| {},
        || {},
    ) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("no Thurm daemon: {e}");
            return None;
        }
    };
    let (ops_tx, ops_rx) = unbounded::<ThurmOp>();
    std::thread::spawn(move || {
        for op in ops_rx {
            match op {
                ThurmOp::Split {
                    id,
                    command,
                    from,
                    dir,
                    size,
                } => {
                    let pane = split(&client, command, from, dir, size);
                    let _ = tx.send(Msg::Created { id, pane });
                }
                ThurmOp::Focus(pane) => {
                    if let Err(e) = client.request(Request::Ui(UiCommand::Focus { pane })) {
                        log::warn!("focus {pane}: {e}");
                    }
                }
            }
        }
    });
    Some(ops_tx)
}

fn split(
    client: &Arc<Client>,
    command: Vec<String>,
    from: Option<PaneId>,
    dir: SplitDir,
    size: (u16, u16),
) -> Option<PaneId> {
    let created = client.request(Request::CreatePane(CreatePane {
        command: Some(command),
        size: PaneSize {
            cols: size.0,
            rows: size.1,
            ..PaneSize::default()
        },
        inherit_cwd_from: from,
        ..Default::default()
    }));
    let pane = match created {
        Ok(Response::PaneCreated { pane }) => pane,
        other => {
            log::warn!("creating a pane: {other:?}");
            return None;
        }
    };
    let open = UiCommand::Open {
        pane,
        split: Some(dir),
        target: from,
        new_window: false,
        percent: Some(50),
        background: true,
    };
    if client.request(Request::Ui(open)).is_err() {
        // An app from before `Open`: the plain split.
        if let Err(e) = client.request(Request::Ui(UiCommand::Split {
            target: from,
            pane,
            dir,
        })) {
            // Shown nowhere (no app window, say): it would only hold the window unseen. The
            // caller closes the window when it hears there is no pane.
            log::warn!("showing pane {pane}: {e}");
            let _ = client.request(Request::ClosePane { pane });
            return None;
        }
    }
    Some(pane)
}

impl Daemon {
    fn start(&mut self) -> io::Result<()> {
        let (cols, rows) = self
            .panes
            .first()
            .map(|p| (p.cols, p.rows))
            .unwrap_or((80, 24));
        self.editor_size = (cols, rows);
        let opts = Value::Map(
            ["ext_linegrid", "ext_multigrid", "rgb", "ext_termcolors"]
                .iter()
                .map(|k| (Value::from(*k), Value::from(true)))
                .collect(),
        );
        self.request(
            "nvim_ui_attach",
            vec![cols.into(), rows.into(), opts],
            Pending::UiAttach,
        )?;
        self.request("nvim_get_api_info", vec![], Pending::ApiInfo)?;
        self.nvim.flush()
    }

    fn request(&mut self, method: &str, params: Vec<Value>, what: Pending) -> io::Result<()> {
        let id = self.nvim.request(method, params)?;
        if !matches!(what, Pending::Ignore) {
            self.pending.insert(id, what);
        }
        Ok(())
    }

    fn lua(&mut self, code: &str, args: Vec<Value>, what: Pending) -> io::Result<()> {
        self.request("nvim_exec_lua", vec![code.into(), Value::Array(args)], what)
    }

    fn run_loop(&mut self, rx: &Receiver<Msg>) -> io::Result<()> {
        loop {
            let msg = rx
                .recv()
                .map_err(|_| io::Error::other("all senders gone"))?;
            match self.handle(msg) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(e) => return Err(e),
            }
            // Everything already waiting before drawing.
            while let Ok(msg) = rx.try_recv() {
                if !self.handle(msg)? {
                    return Ok(());
                }
            }
            self.nvim.flush()?;
            if self
                .panes
                .iter()
                .all(|p| p.win.is_none() && p.tty.is_none())
                && self.ready
            {
                return Ok(());
            }
        }
    }

    /// Handles one message; false to stop.
    fn handle(&mut self, msg: Msg) -> io::Result<bool> {
        match msg {
            Msg::Nvim(m) => self.on_nvim(m)?,
            Msg::NvimGone => {
                self.nvim_exiting = true;
                return Ok(false);
            }
            Msg::Signal => return Ok(false),
            Msg::Helper { id, stream, tty } => {
                let id = id.unwrap_or(0);
                if self.panes.iter().any(|p| p.id == id && p.tty.is_none()) {
                    self.attach_tty(id, stream, tty);
                    self.size_windows()?;
                    self.draw_all();
                } else {
                    log::warn!("a helper nobody asked for: {id}");
                }
            }
            Msg::Resized(id) => {
                if let Some(p) = self.pane_mut(id)
                    && let Some(t) = &p.tty
                    && let Some((c, r)) = tty::size(&t.file)
                {
                    p.cols = c;
                    p.rows = r;
                    p.renderer.invalidate();
                }
                self.size_windows()?;
                self.size_editor()?;
                self.draw_all();
            }
            Msg::HelperGone(id) => return self.pane_gone(id),
            Msg::Input(id, bytes) => self.on_input(id, &bytes)?,
            Msg::Created { id, pane } => match pane {
                Some(t) => {
                    let current = self.current;
                    let Some(p) = self.pane_mut(id) else {
                        return Ok(true);
                    };
                    p.thurm = Some(t);
                    // Its window became current before Thurm made the pane (`:vsplit`).
                    if p.win == Some(current)
                        && !p.focused
                        && let Some(ops) = &self.thurm_ops
                    {
                        self.own_focus = Some((t, Instant::now()));
                        let _ = ops.send(ThurmOp::Focus(t));
                    }
                }
                None => {
                    // No pane for it: close the window rather than leave it unseen.
                    if let Some(i) = self.panes.iter().position(|p| p.id == id) {
                        let p = self.panes.remove(i);
                        if let Some(w) = p.win {
                            self.lua("thurm_nvim.close(...)", vec![w.into()], Pending::Ignore)?;
                        }
                    }
                }
            },
            Msg::EscapeTimer(id, reads) => {
                let Some(p) = self.pane_mut(id) else {
                    return Ok(true);
                };
                if p.reads == reads {
                    let events = p.parser.flush_escape();
                    self.input_events(id, events)?;
                }
            }
            Msg::FocusOutTimer(generation) => {
                if generation == self.focus_gen
                    && !self.panes.iter().any(|p| p.focused)
                    && self.ui_focused
                {
                    self.ui_focused = false;
                    self.request("nvim_ui_set_focus", vec![false.into()], Pending::Ignore)?;
                }
            }
        }
        Ok(true)
    }

    fn pane_mut(&mut self, id: u32) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    fn attach_tty(&mut self, id: u32, stream: UnixStream, fd: OwnedFd) {
        let file = File::from(fd);
        let saved = tty::make_raw(file.as_raw_fd()).ok();
        let size = tty::size(&file);
        let reader = file.try_clone();
        let helper_reader = stream.try_clone();
        let Some(p) = self.pane_mut(id) else { return };
        if let Some((c, r)) = size {
            p.cols = c;
            p.rows = r;
        }
        let mut out = Vec::new();
        enter_modes(&mut out);
        let mut file = file;
        let _ = file.write_all(&out);
        p.renderer.invalidate();
        p.tty = Some(Tty { file, saved });
        p.helper = Some(stream);
        if let Ok(mut r) = reader {
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match r.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if tx.send(Msg::Input(id, buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
        if let Ok(mut h) = helper_reader {
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let mut b = [0u8; 16];
                loop {
                    match h.read(&mut b) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(Msg::HelperGone(id));
                            return;
                        }
                        Ok(n) => {
                            if b[..n].contains(&helper::RESIZED) {
                                let _ = tx.send(Msg::Resized(id));
                            }
                        }
                    }
                }
            });
        }
    }

    fn on_nvim(&mut self, m: Message) -> io::Result<()> {
        match m {
            Message::Notification { method, params } => match method.as_str() {
                "redraw" => {
                    if self.model.apply_redraw(&params) {
                        self.after_flush()?;
                    }
                }
                "thurm_split" => {
                    let win = params.first().and_then(nvim::handle).unwrap_or(0);
                    let side = params.get(1).and_then(Value::as_bool).unwrap_or(true);
                    let before = params.get(2).and_then(Value::as_bool).unwrap_or(false);
                    let from = params.get(3).and_then(nvim::handle).unwrap_or(0);
                    self.splits.insert(win, (side, before, from));
                }
                "thurm_win" => {
                    let win = params.first().and_then(nvim::handle).unwrap_or(0);
                    self.current_changed(win)?;
                }
                "thurm_reopen" => {
                    if let Some(w) = params.first().and_then(nvim::handle) {
                        self.reopening.insert(w);
                    }
                }
                _ => {}
            },
            Message::Response { id, result } => {
                let Some(what) = self.pending.remove(&id) else {
                    if let Err(e) = result {
                        log::warn!("Neovim: {}", nvim::error_text(&e));
                    }
                    return Ok(());
                };
                let result = match result {
                    Ok(v) => v,
                    // Without the UI or windows.lua there is nothing to show: give up, and the
                    // terminals go back as they were.
                    Err(e)
                        if matches!(
                            what,
                            Pending::UiAttach | Pending::ApiInfo | Pending::Setup
                        ) =>
                    {
                        return Err(io::Error::other(format!(
                            "setting up Neovim: {}",
                            nvim::error_text(&e)
                        )));
                    }
                    Err(e) => {
                        log::warn!("Neovim: {}", nvim::error_text(&e));
                        if matches!(what, Pending::Statuslines(_)) {
                            self.statuslines_in_flight = false;
                        }
                        return Ok(());
                    }
                };
                match what {
                    Pending::ApiInfo => {
                        let chan = result
                            .as_array()
                            .and_then(|a| a.first())
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        let (cols, rows) = self.editor_size;
                        let text_rows = rows.saturating_sub(1).max(1);
                        self.lua(
                            WINDOWS_LUA,
                            vec![chan.into(), cols.into(), text_rows.into()],
                            Pending::Setup,
                        )?;
                    }
                    Pending::Setup => {
                        let a = result.as_array().cloned().unwrap_or_default();
                        let first = a.first().and_then(nvim::handle).unwrap_or(0);
                        self.anchor = a.get(1).and_then(nvim::handle).unwrap_or(0);
                        self.current = first;
                        if let Some(p) = self.pane_mut(0) {
                            p.win = Some(first);
                        }
                        self.asked.insert(first);
                        self.ready = true;
                        self.reconcile()?;
                        self.draw_all();
                    }
                    Pending::Statuslines(ids) => {
                        self.statuslines_in_flight = false;
                        self.set_statuslines(&ids, &result)?;
                        if std::mem::take(&mut self.statuslines_wanted) {
                            self.ask_statuslines()?;
                        }
                    }
                    Pending::UiAttach | Pending::Ignore => {}
                }
            }
            Message::Request { id, method } => {
                self.nvim
                    .refuse(id, &format!("thurm-nvim serves no requests ({method})"))?;
            }
        }
        Ok(())
    }

    fn after_flush(&mut self) -> io::Result<()> {
        if self.ready {
            self.reconcile()?;
            if self.statuslines_in_flight {
                self.statuslines_wanted = true;
            } else {
                self.ask_statuslines()?;
            }
        }
        self.draw_all();
        Ok(())
    }

    /// Gives new windows panes, and lets panes of closed windows go.
    fn reconcile(&mut self) -> io::Result<()> {
        // Windows that went away.
        let mut gone = Vec::new();
        for p in &mut self.panes {
            let Some(w) = p.win else { continue };
            match self.model.grid_of_win(w) {
                Some(_) => p.seen_grid = true,
                None if p.seen_grid => gone.push(p.id),
                None => {}
            }
        }
        for id in gone {
            let Some(i) = self.panes.iter().position(|p| p.id == id) else {
                continue;
            };
            let w = self.panes[i].win;
            if let Some(w) = w
                && self.reopening.remove(&w)
            {
                self.panes[i].win = None;
                self.panes[i].seen_grid = false;
                continue;
            }
            let mut p = self.panes.remove(i);
            p.release();
        }
        let live: HashSet<i64> = self.model.wins.values().map(|w| w.win).collect();
        self.asked.retain(|w| live.contains(w));
        self.splits.retain(|w, _| live.contains(w));

        let wins: Vec<(i64, WinKind)> = self
            .model
            .wins
            .values()
            .map(|w| (w.win, w.kind.clone()))
            .collect();
        for (win, kind) in wins {
            match kind {
                WinKind::Ordinary if win != self.anchor && self.asked.insert(win) => {
                    let (c, r) = self.new_window_size(win);
                    self.lua(
                        "thurm_nvim.adopt(...)",
                        vec![win.into(), c.into(), r.into()],
                        Pending::Ignore,
                    )?;
                }
                WinKind::External if !self.panes.iter().any(|p| p.win == Some(win)) => {
                    if let Some(p) = self
                        .panes
                        .iter_mut()
                        .find(|p| p.win.is_none() && p.tty.is_some())
                    {
                        p.win = Some(win);
                        p.asked_size = None;
                    } else {
                        self.new_pane(win);
                    }
                }
                _ => {}
            }
        }
        self.size_windows()
    }

    /// A guess at a new window's size until its pane says: half the pane it splits.
    fn new_window_size(&self, win: i64) -> (u16, u16) {
        let (side, _, from) = self.splits.get(&win).copied().unwrap_or((true, false, 0));
        let p = self
            .panes
            .iter()
            .find(|p| p.win == Some(from))
            .or_else(|| self.current_pane())
            .or(self.panes.first());
        let (c, r) = p
            .map(|p| (p.cols, p.view(false).text_rows()))
            .unwrap_or((80, 22));
        if side {
            ((c / 2).max(1), r)
        } else {
            (c, (r / 2).max(1))
        }
    }

    fn new_pane(&mut self, win: i64) {
        let id = self.next_id;
        self.next_id += 1;
        let (side, before, from_win) = self.splits.get(&win).copied().unwrap_or((true, false, 0));
        let from = self
            .panes
            .iter()
            .find(|p| p.win == Some(from_win) && from_win != 0)
            .or_else(|| self.current_pane())
            .and_then(|p| p.thurm);
        let dir = match (side, before) {
            (true, false) => SplitDir::Right,
            (true, true) => SplitDir::Left,
            (false, false) => SplitDir::Down,
            (false, true) => SplitDir::Up,
        };
        let size = self.new_window_size(win);
        self.panes.push(Pane::new(id, Some(win)));
        let command = vec![
            self.exe.display().to_string(),
            launch::PANE.into(),
            self.socket.display().to_string(),
            id.to_string(),
        ];
        match &self.thurm_ops {
            Some(ops) => {
                let _ = ops.send(ThurmOp::Split {
                    id,
                    command,
                    from,
                    dir,
                    size: (size.0, size.1 + 2),
                });
            }
            None => {
                let _ = self.tx.send(Msg::Created { id, pane: None });
            }
        }
    }

    fn current_pane(&self) -> Option<&Pane> {
        self.panes.iter().find(|p| p.win == Some(self.current))
    }

    /// Asks Neovim to size each window to its pane.
    fn size_windows(&mut self) -> io::Result<()> {
        let mut asks = Vec::new();
        for p in &mut self.panes {
            let (Some(w), Some(_)) = (p.win, &p.tty) else {
                continue;
            };
            let size = (p.cols, p.view(false).text_rows());
            if p.asked_size != Some(size) && self.model.grid_of_win(w).is_some() {
                p.asked_size = Some(size);
                asks.push((w, size));
            }
        }
        for (w, (c, r)) in asks {
            self.lua(
                "thurm_nvim.size(...)",
                vec![w.into(), c.into(), r.into()],
                Pending::Ignore,
            )?;
        }
        Ok(())
    }

    /// The editor (and so the message grid and editor floats) takes the current pane's size.
    fn size_editor(&mut self) -> io::Result<()> {
        let Some(size) = self
            .current_pane()
            .filter(|p| p.tty.is_some())
            .map(|p| (p.cols, p.rows))
        else {
            return Ok(());
        };
        if size != self.editor_size {
            self.editor_size = size;
            self.request(
                "nvim_ui_try_resize",
                vec![size.0.into(), size.1.into()],
                Pending::Ignore,
            )?;
        }
        Ok(())
    }

    fn current_changed(&mut self, win: i64) -> io::Result<()> {
        self.current = win;
        self.size_editor()?;
        let target = self
            .current_pane()
            .and_then(|p| if p.focused { None } else { p.thurm });
        if let (Some(t), Some(ops)) = (target, &self.thurm_ops) {
            self.own_focus = Some((t, Instant::now()));
            let _ = ops.send(ThurmOp::Focus(t));
        }
        self.draw_all();
        Ok(())
    }

    fn ask_statuslines(&mut self) -> io::Result<()> {
        let shown: Vec<(u32, i64, u16)> = self
            .panes
            .iter()
            .filter_map(|p| Some((p.id, p.win?, p.cols)))
            .collect();
        if shown.is_empty() {
            return Ok(());
        }
        let wins = shown.iter().map(|(_, w, _)| Value::from(*w)).collect();
        let widths = shown.iter().map(|(_, _, c)| Value::from(*c)).collect();
        self.statuslines_in_flight = true;
        self.lua(
            "return thurm_nvim.statuslines(...)",
            vec![Value::Array(wins), Value::Array(widths)],
            Pending::Statuslines(shown.iter().map(|(id, _, _)| *id).collect()),
        )
    }

    fn set_statuslines(&mut self, ids: &[u32], result: &Value) -> io::Result<()> {
        let Some(all) = result.as_array() else {
            return Ok(());
        };
        let mut changed = false;
        for (id, v) in ids.iter().zip(all) {
            let cells = v.as_array().map(|cells| {
                let mut out = Vec::new();
                for c in cells {
                    let Some(parts) = c.as_array() else { continue };
                    let text = parts.first().and_then(Value::as_str).unwrap_or(" ");
                    let width = parts
                        .get(1)
                        .and_then(Value::as_u64)
                        .unwrap_or(1)
                        .clamp(1, 2) as u8;
                    let attrs = match parts.get(2) {
                        Some(Value::Map(m)) => style_from_get_hl(m),
                        _ => Default::default(),
                    };
                    let style = self.model.styles.resolve_style(attrs);
                    // Text from a file can hold control characters: shown as Neovim shows
                    // them, never sent to the terminal.
                    if let Some(shown) = crate::cell::visible_control(text) {
                        for ch in shown.chars() {
                            out.push(FrameCell {
                                text: Text::new(ch.encode_utf8(&mut [0; 4])),
                                width: 1,
                                style,
                            });
                        }
                        continue;
                    }
                    out.push(FrameCell {
                        text: Text::new(text),
                        width,
                        style,
                    });
                    if width == 2 {
                        out.push(FrameCell {
                            text: Text::EMPTY,
                            width: 0,
                            style,
                        });
                    }
                }
                out
            });
            if let Some(p) = self.pane_mut(*id)
                && p.status != cells
            {
                let had = p.status.is_some();
                changed |= had != cells.is_some();
                p.status = cells;
            }
        }
        if changed {
            self.size_windows()?;
        }
        self.draw_all();
        Ok(())
    }

    fn on_input(&mut self, id: u32, bytes: &[u8]) -> io::Result<()> {
        let tx = self.tx.clone();
        let Some(p) = self.pane_mut(id) else {
            return Ok(());
        };
        p.reads += 1;
        let events = p.parser.feed(bytes);
        if p.parser.pending_escape() {
            // Wait a moment for the rest of a sequence; nothing more and it was Escape.
            let reads = p.reads;
            std::thread::spawn(move || {
                std::thread::sleep(ESCAPE_DELAY);
                let _ = tx.send(Msg::EscapeTimer(id, reads));
            });
        }
        self.input_events(id, events)
    }

    fn input_events(&mut self, id: u32, events: Vec<InputEvent>) -> io::Result<()> {
        let Some(p) = self.pane_mut(id) else {
            return Ok(());
        };
        let (win, rows) = (p.win, p.view(false).text_rows());
        let grid = win.and_then(|w| self.model.grid_of_win(w));
        for ev in events {
            match ev {
                InputEvent::FocusIn => self.focus_report(id, true)?,
                InputEvent::FocusOut => self.focus_report(id, false)?,
                InputEvent::Keys(keys) => {
                    self.make_current(win)?;
                    self.request("nvim_input", vec![keys.into()], Pending::Ignore)?;
                }
                InputEvent::PasteStart => {
                    if let Some(p) = self.pane_mut(id) {
                        p.pasting = false;
                    }
                }
                // One paste, however many reads it took: phase 1, then 2s, then 3.
                InputEvent::Paste(text) => {
                    self.make_current(win)?;
                    let Some(p) = self.pane_mut(id) else { continue };
                    let phase = if std::mem::replace(&mut p.pasting, true) {
                        2
                    } else {
                        1
                    };
                    self.request(
                        "nvim_paste",
                        vec![text.into(), true.into(), phase.into()],
                        Pending::Ignore,
                    )?;
                }
                InputEvent::PasteEnd => {
                    let Some(p) = self.pane_mut(id) else { continue };
                    if std::mem::take(&mut p.pasting) {
                        self.request(
                            "nvim_paste",
                            vec!["".into(), true.into(), 3.into()],
                            Pending::Ignore,
                        )?;
                    }
                }
                InputEvent::Mouse(m) => {
                    let Some(g) = grid else { continue };
                    if m.row >= rows {
                        continue;
                    }
                    let (g, row, col) = self.hit(g, m.row as i32, m.col as i32);
                    self.request(
                        "nvim_input_mouse",
                        vec![
                            m.button.into(),
                            m.action.into(),
                            m.modifier.into(),
                            g.into(),
                            row.into(),
                            col.into(),
                        ],
                        Pending::Ignore,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// The grid under (`row`, `col`) of window grid `g`: the topmost float there, else `g`;
    /// and the cell in it.
    fn hit(&self, g: u64, row: i32, col: i32) -> (u64, i32, i32) {
        for (fg, at) in self.model.floats_on(g).into_iter().rev() {
            let Some(f) = self.model.grids.get(&fg) else {
                continue;
            };
            let (r, c) = (row - at.row, col - at.col);
            if r >= 0 && c >= 0 && r < f.rows as i32 && c < f.cols as i32 {
                return (fg, r, c);
            }
        }
        (g, row, col)
    }

    /// Typing into a pane whose window isn't current: it is now.
    fn make_current(&mut self, win: Option<i64>) -> io::Result<()> {
        if let Some(w) = win
            && w != self.current
        {
            self.current = w;
            self.request("nvim_set_current_win", vec![w.into()], Pending::Ignore)?;
        }
        Ok(())
    }

    fn focus_report(&mut self, id: u32, focused: bool) -> io::Result<()> {
        let Some(p) = self.pane_mut(id) else {
            return Ok(());
        };
        p.focused = focused;
        let (thurm, win) = (p.thurm, p.win);
        if focused {
            if !self.ui_focused {
                self.ui_focused = true;
                self.request("nvim_ui_set_focus", vec![true.into()], Pending::Ignore)?;
            }
            // Our own focus change still landing: reports from other panes are stale.
            let stale = matches!(self.own_focus, Some((t, at)) if Some(t) != thurm && at.elapsed() < OWN_FOCUS_GRACE);
            if Some(thurm).flatten().is_some()
                && self.own_focus.is_some_and(|(t, _)| Some(t) == thurm)
            {
                self.own_focus = None;
            }
            if !stale {
                self.make_current(win)?;
                self.size_editor()?;
                self.draw_all();
            }
        } else {
            self.focus_gen += 1;
            let generation = self.focus_gen;
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(FOCUS_OUT_DELAY);
                let _ = tx.send(Msg::FocusOutTimer(generation));
            });
        }
        Ok(())
    }

    /// A pane's helper hung up (the pane closed): its window closes. The last one ends the
    /// session (false), as closing the terminal Neovim runs in would: Neovim is hung up on and
    /// keeps the swap files of unsaved changes. Never `:qa!`, which would throw them away.
    fn pane_gone(&mut self, id: u32) -> io::Result<bool> {
        let Some(i) = self.panes.iter().position(|p| p.id == id) else {
            return Ok(true);
        };
        let mut p = self.panes.remove(i);
        p.release();
        if self
            .panes
            .iter()
            .all(|p| p.tty.is_none() && p.win.is_none())
        {
            return Ok(false);
        }
        if let Some(w) = p.win {
            self.lua("thurm_nvim.close(...)", vec![w.into()], Pending::Ignore)?;
        }
        Ok(true)
    }

    fn draw_all(&mut self) {
        let current = self.current;
        for i in 0..self.panes.len() {
            let p = &self.panes[i];
            if p.tty.is_none() {
                continue;
            }
            let mut view = p.view(p.win == Some(current));
            view.grid = p.win.and_then(|w| self.model.grid_of_win(w));
            let frame = compose(&self.model, &view);
            let p = &mut self.panes[i];
            let mut out = Vec::new();
            p.renderer.draw(&frame, &mut out);
            if let Some(t) = &mut p.tty
                && t.file.write_all(&out).is_err()
            {
                let _ = self.tx.send(Msg::HelperGone(p.id));
            }
        }
    }
}
