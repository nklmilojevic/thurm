//! C ABI for the Swift front end. See `include/thurm.h` for the contract.

#![allow(non_camel_case_types, clippy::missing_safety_doc)]

#[cfg(feature = "mobile")]
mod mobile;
#[cfg(feature = "desktop")]
mod remote;
#[cfg(feature = "desktop")]
mod transfer;

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ::thurm_client::{Client, ClientError, ConnectOptions};
use parking_lot::{ArcMutexGuard, Mutex, RawMutex};
use thurm_proto::{
    Event, Frame, ImagePlacement, Key, KeyAction, KeyEvent, MouseButton, MouseEvent, MouseKind,
    NamedKey, PaneId, PaneSize, Request, Response,
};
use thurm_term::{ClientView, EngineConfig, MouseOutcome, TermEvent, Terminal};

pub type thurm_event_cb = Option<unsafe extern "C" fn(ctx: *mut c_void, json: *const c_char)>;
pub type thurm_frame_cb = Option<unsafe extern "C" fn(ctx: *mut c_void, pane: u64)>;
pub type thurm_ctx_drop_cb = Option<unsafe extern "C" fn(ctx: *mut c_void)>;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct thurm_cell {
    pub ch: u32,
    pub fg: u32,
    pub bg: u32,
    pub ul: u32,
    pub flags: u16,
    pub link: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct thurm_grid_info {
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_shape: u8,
    pub cursor_blinking: bool,
    pub cursor_wide: bool,
    pub display_offset: u32,
    pub history_size: u32,
    pub modes: u32,
    pub foreground: u32,
    pub background: u32,
    pub cursor_color: u32,
    pub cursor_text_color: u32,
    pub selection_fg: u32,
    pub selection_bg: u32,
    pub generation: u64,
    pub dirty_rows: u64,
    pub image_count: usize,
    pub link_count: usize,
    pub shift: i32,
    pub has_peek: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct thurm_image_placement {
    pub image: u32,
    pub placement: u32,
    pub row: i32,
    pub col: i32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub cols: u32,
    pub rows: u32,
    pub z: i32,
    pub dst_w: u32,
    pub dst_h: u32,
}

#[repr(C)]
pub struct thurm_key_event {
    pub kind: u32,
    pub code: u32,
    pub mods: u8,
    pub action: u8,
    pub text: *const c_char,
    pub shifted: u32,
    pub base_layout: u32,
}

#[repr(C)]
pub struct thurm_mouse_event {
    pub kind: u8,
    pub button: u8,
    pub mods: u8,
    pub clicks: u8,
    pub col: u16,
    pub row: u16,
    pub right_half: bool,
    pub x: u32,
    pub y: u32,
}

#[derive(Default)]
struct Grid {
    info: thurm_grid_info,
    cells: Vec<thurm_cell>,
    clusters: HashMap<(u16, u16), CString>,
    links: Vec<CString>,
    images: Vec<thurm_image_placement>,
    has_frame: bool,
    /// Generation at which a reader last locked and consumed `dirty_rows` / `shift`.
    last_consumed_gen: u64,
    /// History line above the viewport (smooth scrolling); empty when there is none.
    peek: Vec<thurm_cell>,
    peek_clusters: HashMap<u16, CString>,
}

/// Moves the rows of a `cols`×`rows` grid down by `shift` (up when negative), in place.
/// Exposed rows are cleared.
fn shift_rows(cells: &mut [thurm_cell], cols: usize, rows: usize, shift: i32) {
    let n = shift.unsigned_abs() as usize;
    if n >= rows {
        cells.fill(thurm_cell::default());
        return;
    }
    let keep = (rows - n) * cols;
    if shift > 0 {
        cells.copy_within(0..keep, n * cols);
        cells[..n * cols].fill(thurm_cell::default());
    } else {
        cells.copy_within(n * cols.., 0);
        cells[keep..].fill(thurm_cell::default());
    }
}

/// Moves a dirty-row mask by `shift` rows. Bit 63 stands for "row 63 or below".
fn shift_dirty(mask: u64, shift: i32, rows: usize) -> u64 {
    let mut out = 0u64;
    for bit in 0..64i64 {
        if mask & (1 << bit) == 0 {
            continue;
        }
        let last = if bit == 63 { rows as i64 - 1 } else { bit };
        for r in bit..=last.max(bit) {
            let to = r + shift as i64;
            if (0..rows as i64).contains(&to) {
                out |= 1 << to.min(63);
            }
        }
    }
    out
}

impl Grid {
    fn apply(&mut self, f: Frame) {
        let (cols, rows) = (f.cols as usize, f.rows as usize);
        let reset = f.full || self.info.cols != f.cols || self.info.rows != f.rows;
        if reset {
            self.cells = vec![thurm_cell::default(); cols * rows];
            self.clusters.clear();
            self.info.dirty_rows = u64::MAX;
            self.info.shift = 0;
        }
        self.info.cols = f.cols;
        self.info.rows = f.rows;
        if f.shift != 0 && !reset {
            // Row r now shows what row r - shift showed; exposed rows follow in `lines`.
            let s = f.shift as i64;
            shift_rows(&mut self.cells, cols, rows, f.shift);
            self.clusters = std::mem::take(&mut self.clusters)
                .into_iter()
                .filter_map(|((row, col), v)| {
                    let to = row as i64 + s;
                    (0..rows as i64)
                        .contains(&to)
                        .then_some(((to as u16, col), v))
                })
                .collect();
            self.info.dirty_rows = shift_dirty(self.info.dirty_rows, f.shift, rows);
            self.info.shift += f.shift;
        }
        let sent: HashSet<u16> = f.lines.iter().map(|l| l.row).collect();
        self.clusters.retain(|(row, _), _| !sent.contains(row));
        for line in f.lines {
            let r = line.row as usize;
            if r >= rows {
                continue;
            }
            let base = r * cols;
            for (c, cell) in line.cells.iter().enumerate().take(cols) {
                self.cells[base + c] = ffi_cell(cell);
            }
            for (col, text) in line.clusters {
                if let Ok(s) = CString::new(text) {
                    self.clusters.insert((line.row, col), s);
                }
            }
            self.info.dirty_rows |= 1u64 << r.min(63);
        }
        if let Some(peek) = f.peek {
            self.peek = peek.cells.iter().take(cols).map(ffi_cell).collect();
            self.peek_clusters = peek
                .clusters
                .into_iter()
                .filter_map(|(col, text)| Some((col, CString::new(text).ok()?)))
                .collect();
        }
        if !f.has_peek {
            self.peek.clear();
            self.peek_clusters.clear();
        }
        self.info.has_peek = !self.peek.is_empty();
        if !f.links.is_empty() {
            self.links = f
                .links
                .into_iter()
                .map(|l| CString::new(l).unwrap_or_default())
                .collect();
        }
        self.images.clear();
        self.images.extend(f.images.iter().map(placement));
        let i = &mut self.info;
        i.cursor_col = f.cursor.col;
        i.cursor_row = f.cursor.row;
        i.cursor_shape = f.cursor.shape as u8;
        i.cursor_blinking = f.cursor.blinking;
        i.cursor_wide = f.cursor.wide;
        i.display_offset = f.display_offset;
        i.history_size = f.history_size;
        i.modes = f.modes;
        i.foreground = f.colors.foreground;
        i.background = f.colors.background;
        i.cursor_color = f.colors.cursor;
        i.cursor_text_color = f.colors.cursor_text;
        i.selection_fg = f.colors.selection_fg;
        i.selection_bg = f.colors.selection_bg;
        i.generation += 1;
        i.image_count = self.images.len();
        i.link_count = self.links.len();
        self.has_frame = true;
    }
}

fn ffi_cell(cell: &thurm_proto::Cell) -> thurm_cell {
    thurm_cell {
        ch: cell.ch as u32,
        fg: cell.fg,
        bg: cell.bg,
        ul: cell.ul,
        flags: cell.flags,
        link: cell.link,
    }
}

fn placement(p: &ImagePlacement) -> thurm_image_placement {
    thurm_image_placement {
        image: p.image,
        placement: p.placement,
        row: p.row,
        col: p.col,
        x_offset: p.x_offset,
        y_offset: p.y_offset,
        src_x: p.src_x,
        src_y: p.src_y,
        src_w: p.src_w,
        src_h: p.src_h,
        cols: p.cols,
        rows: p.rows,
        z: p.z,
        dst_w: p.dst_w,
        dst_h: p.dst_h,
    }
}

struct ImageSlot {
    width: u32,
    height: u32,
    /// Shared with the terminal's copy of the image.
    rgba: Arc<Vec<u8>>,
    /// Unique per stored image, so a replaced image (same id) gets a new texture.
    serial: u64,
}

static IMAGE_SERIAL: AtomicU64 = AtomicU64::new(1);

type ImageKey = (PaneId, u32);

/// The app's own copy of a subscribed pane's terminal, fed the daemon's output stream (like
/// tty7: the daemon relays bytes, the app emulates and draws straight from its copy).
/// Viewport, selection and search are local; input it encodes goes to the daemon as bytes.
struct LocalPane {
    term: Terminal,
    view: ClientView,
}

impl LocalPane {
    /// Drops what the copy would do to the outside world (the daemon's copy does it) and frees
    /// images it no longer holds.
    fn drain(&mut self, pane: PaneId, images: &Mutex<HashMap<ImageKey, Arc<ImageSlot>>>) {
        for ev in self.term.drain_events() {
            if let TermEvent::ImageFreed(id) = ev {
                images.lock().remove(&(pane, id));
            }
        }
    }
}

struct Engine {
    cfg: thurm_config::Config,
    dark: bool,
    /// A theme shown instead of the configured one (the theme picker), not saved.
    preview: Option<thurm_config::Theme>,
}

impl Engine {
    fn engine_config(&self) -> EngineConfig {
        let mut ec = EngineConfig::from_config(&self.cfg, self.dark);
        if let Some(t) = &self.preview {
            ec.theme = t.clone();
        }
        ec
    }
}

#[derive(Default)]
struct Subscriptions {
    /// Refcount of active views currently showing each pane.
    active: HashMap<PaneId, usize>,
    /// Panes currently subscribed on the daemon (including in-flight before `Event::Attach`).
    subscribed: HashSet<PaneId>,
    /// Panes with `active == 0` kept subscribed & warm in LRU order (oldest at front).
    warm_order: VecDeque<PaneId>,
    /// Maximum number of parked warm panes (`0` = legacy immediate-unsubscribe behavior).
    warm_limit: usize,
    /// Number of cancelled in-flight `Subscribe`s per pane whose late `Event::Attach` must be
    /// discarded rather than resurrecting a `LocalPane`.
    discard_attach: HashMap<PaneId, usize>,
}

impl Subscriptions {
    fn remove_warm(&mut self, pane: PaneId) {
        self.warm_order.retain(|&p| p != pane);
    }

    fn is_warm(&self, pane: PaneId) -> bool {
        self.warm_order.contains(&pane)
    }

    fn remove_pane(&mut self, pane: PaneId) {
        self.active.remove(&pane);
        self.remove_warm(pane);
        self.subscribed.remove(&pane);
        self.discard_attach.remove(&pane);
    }

    fn clear(&mut self) {
        self.active.clear();
        self.warm_order.clear();
        self.subscribed.clear();
        self.discard_attach.clear();
    }

    fn evict_excess(&mut self, shared: &Shared, client: &Client) {
        while self.warm_order.len() > self.warm_limit {
            let Some(old) = self.warm_order.pop_front() else {
                break;
            };
            let was_subscribed = self.subscribed.remove(&old);
            let had_local = shared.locals.lock().remove(&old).is_some();
            if was_subscribed {
                if !had_local {
                    *self.discard_attach.entry(old).or_insert(0) += 1;
                }
                send_checked(client, Request::Unsubscribe { pane: old });
            }
        }
    }
}

struct Shared {
    grids: Mutex<HashMap<PaneId, Arc<Mutex<Grid>>>>,
    locked: Mutex<HashMap<PaneId, ArcMutexGuard<RawMutex, Grid>>>,
    images: Mutex<HashMap<(PaneId, u32), Arc<ImageSlot>>>,
    locked_images: Mutex<HashMap<ImageKey, Vec<Arc<ImageSlot>>>>,
    locals: Mutex<HashMap<PaneId, Arc<Mutex<LocalPane>>>>,
    subs: Mutex<Subscriptions>,
    engine: Mutex<Engine>,
    /// A wake-up is scheduled for panes inside a synchronized update (so its timeout shows).
    sync_timer: Mutex<HashSet<PaneId>>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            grids: Default::default(),
            locked: Default::default(),
            images: Default::default(),
            locked_images: Default::default(),
            locals: Default::default(),
            subs: Default::default(),
            engine: Mutex::new(Engine {
                cfg: thurm_config::Config::load().unwrap_or_default(),
                dark: true,
                preview: None,
            }),
            sync_timer: Default::default(),
        }
    }
}

impl Shared {
    fn local(&self, pane: PaneId) -> Option<Arc<Mutex<LocalPane>>> {
        self.locals.lock().get(&pane).cloned()
    }

    fn is_warm(&self, pane: PaneId) -> bool {
        self.subs.lock().is_warm(pane)
    }

    /// Active (non-warm) subscribed panes that currently have a local terminal copy.
    fn active_panes(&self) -> Vec<PaneId> {
        let subs = self.subs.lock();
        self.locals
            .lock()
            .keys()
            .copied()
            .filter(|p| !subs.is_warm(*p))
            .collect()
    }

    /// Show the panes in the light or dark variant of the theme, returning the active panes that
    /// need repainting when it changed.
    fn set_dark_panes(&self, dark: bool) -> Option<Vec<PaneId>> {
        let changed = std::mem::replace(&mut self.engine.lock().dark, dark) != dark;
        changed.then(|| self.reload_engine(false))
    }

    fn reload_engine(&self, reload_file: bool) -> Vec<PaneId> {
        let cfg = {
            let mut e = self.engine.lock();
            if reload_file && let Ok(c) = thurm_config::Config::load() {
                e.cfg = c;
                // Choosing a theme saves it and reloads: the preview has served its purpose.
                e.preview = None;
            }
            e.engine_config()
        };
        for local in self.locals.lock().values() {
            let mut l = local.lock();
            l.term.set_config(cfg.clone());
            l.view.invalidate();
        }
        self.active_panes()
    }
}

pub struct thurm_client {
    client: Arc<Client>,
    shared: Arc<Shared>,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: Arc<CallbackCtx>,
    /// The daemon's socket when the connection is a plain one: where its streams open.
    socket: Option<std::path::PathBuf>,
    /// What carries the connection when it is not a plain socket (the iOS app's ssh session);
    /// dropped after `client`.
    _transport: Option<Box<dyn std::any::Any + Send>>,
}

#[derive(Clone, Copy)]
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}
unsafe impl Sync for Ctx {}

/// Shared owner of the caller's opaque `ctx` pointer across the client handle, reader callbacks,
/// and background sync-flush tasks. Runs the optional `on_drop` hook once when the last reference
/// drops so callers never leak or prematurely free `ctx`. On a failed connection attempt,
/// [`CallbackCtx::detach`] waits for any in-flight callback to exit and clears the pointer so
/// lingering background threads can never invoke a callback with `ctx` after connect returns NULL.
struct CallbackCtx {
    raw: parking_lot::RwLock<Option<Ctx>>,
    on_drop: Mutex<thurm_ctx_drop_cb>,
}

impl CallbackCtx {
    fn new(raw: *mut c_void) -> Arc<Self> {
        Arc::new(Self {
            raw: parking_lot::RwLock::new(Some(Ctx(raw))),
            on_drop: Mutex::new(None),
        })
    }

    fn with_ptr<R>(&self, f: impl FnOnce(*mut c_void) -> R) -> Option<R> {
        let guard = self.raw.read();
        guard.map(|c| f(c.0))
    }

    fn detach(&self) {
        *self.raw.write() = None;
    }

    fn set_on_drop(&self, cb: thurm_ctx_drop_cb) {
        *self.on_drop.lock() = cb;
    }
}

impl Drop for CallbackCtx {
    fn drop(&mut self) {
        if let (Some(cb), Some(raw)) = (*self.on_drop.get_mut(), *self.raw.get_mut()) {
            unsafe { cb(raw.0) };
        }
    }
}

fn emit_json(cb: thurm_event_cb, ctx: &CallbackCtx, json: &str) {
    if let (Some(cb), Ok(s)) = (cb, CString::new(json)) {
        ctx.with_ptr(|ptr| unsafe { cb(ptr, s.as_ptr()) });
    }
}

unsafe fn opt_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(p) }.to_str().ok()
    }
}

fn into_c(s: String) -> *mut c_char {
    CString::new(s.replace('\0', ""))
        .unwrap_or_default()
        .into_raw()
}

fn error_json(e: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": e.to_string() }).to_string()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_connect(
    daemon_path: *const c_char,
    client_name: *const c_char,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: *mut c_void,
    err: *mut *mut c_char,
) -> *mut thurm_client {
    let socket = thurm_config::socket_path();
    unsafe {
        // This Mac's daemon only: what a remote host's plugins ask goes to its own users.
        connect_to(
            &[thurm_proto::CAP_EXT_UI, thurm_proto::CAP_EXT_FORWARDS],
            socket,
            daemon_path,
            client_name,
            on_event,
            on_frame,
            ctx,
            err,
        )
    }
}

/// Like `thurm_connect`, to the daemon on `socket` (a remote host's, through its tunnel).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_connect_socket(
    socket: *const c_char,
    daemon_path: *const c_char,
    client_name: *const c_char,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: *mut c_void,
    err: *mut *mut c_char,
) -> *mut thurm_client {
    let Some(socket) = (unsafe { opt_str(socket) }) else {
        if !err.is_null() {
            unsafe { *err = into_c("no socket".into()) };
        }
        return std::ptr::null_mut();
    };
    let socket = std::path::PathBuf::from(socket);
    unsafe {
        connect_to(
            remote_capabilities(&socket),
            socket,
            daemon_path,
            client_name,
            on_event,
            on_frame,
            ctx,
            err,
        )
    }
}

/// What a connection to `socket` says in `Hello` besides the build's capabilities. A tailnet
/// relay splices the bytes unchanged, so the daemon gets this Mac's capabilities as they are, and
/// it asks for deflated events there. An ssh tunnel compresses on its own (`Compression=yes`), so
/// it isn't asked twice. The desktop app hears a remote host's ports, to forward them.
fn remote_capabilities(socket: &Path) -> &'static [&'static str] {
    #[cfg(feature = "desktop")]
    {
        if remote::is_tailnet_socket(socket) {
            return &[thurm_proto::CAP_DEFLATE, thurm_proto::CAP_PORT_EVENTS];
        }
        &[thurm_proto::CAP_PORT_EVENTS]
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = socket;
        &[]
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn connect_to(
    capabilities: &[&str],
    socket: std::path::PathBuf,
    daemon_path: *const c_char,
    client_name: *const c_char,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: *mut c_void,
    err: *mut *mut c_char,
) -> *mut thurm_client {
    let daemon = unsafe { opt_str(daemon_path) }.map(Path::new);
    let name = unsafe { opt_str(client_name) }
        .unwrap_or("Thurm.app")
        .to_owned();
    let mut capabilities = capabilities.to_vec();
    capabilities.push(thurm_proto::CAP_AGENT_ACTIVITY);
    capabilities.push(thurm_proto::CAP_PENDING_PROMPT);
    let streams_on = socket.clone();
    let connected = new_connection(on_event, on_frame, ctx, |on_ev, on_disc| {
        Client::connect_with(
            ConnectOptions {
                socket,
                spawn_daemon: daemon,
                client_name: &name,
                ui: true,
            },
            &capabilities,
            on_ev,
            on_disc,
        )
        .map(|c| (c, None))
        .map_err(|e| e.to_string())
    });
    match connected {
        Ok(client) => {
            unsafe { (*client).socket = Some(streams_on) };
            client
        }
        Err(e) => {
            if !err.is_null() {
                unsafe { *err = into_c(e) };
            }
            std::ptr::null_mut()
        }
    }
}

type Transport = Option<Box<dyn std::any::Any + Send>>;

/// A `thurm_client` for the connection `connect` makes with the event and disconnect handlers
/// it is given.
fn new_connection(
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: *mut c_void,
    connect: impl FnOnce(
        Box<dyn Fn(Event) + Send>,
        Box<dyn FnOnce() + Send>,
    ) -> Result<(Arc<Client>, Transport), String>,
) -> Result<*mut thurm_client, String> {
    let shared = Arc::new(Shared::default());
    let ctx = CallbackCtx::new(ctx);
    let events_shared = shared.clone();
    let ev_ctx = ctx.clone();
    // A panic here would end the connection's reader thread (and with it the pane updates).
    let on_ev = move |ev: Event| {
        let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle_event(&events_shared, ev, on_event, on_frame, &ev_ctx)
        }));
        if handled.is_err() {
            log::error!("an event from the daemon panicked the app's handler");
        }
    };
    let disc_shared = shared.clone();
    let disc_ctx = ctx.clone();
    let on_disc = move || {
        disc_shared.subs.lock().clear();
        disc_shared.locals.lock().clear();
        emit_json(on_event, &disc_ctx, "\"Disconnected\"");
    };
    let (client, transport) = match connect(Box::new(on_ev), Box::new(on_disc)) {
        Ok(pair) => pair,
        Err(e) => {
            ctx.detach();
            return Err(e);
        }
    };
    Ok(Box::into_raw(Box::new(thurm_client {
        client,
        shared,
        on_event,
        on_frame,
        ctx,
        socket: None,
        _transport: transport,
    })))
}

/// Writes `data` to a new private file in the daemon's runtime directory (on the daemon's
/// machine) and returns its path, or NULL with `err` set. Free the result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_write_temp_file(
    client: *mut thurm_client,
    name: *const c_char,
    data: *const u8,
    len: usize,
    err: *mut *mut c_char,
) -> *mut c_char {
    let fail = |e: String| {
        if !err.is_null() {
            unsafe { *err = into_c(e) };
        }
        std::ptr::null_mut()
    };
    let Some(c) = (unsafe { client_ref(client) }) else {
        return fail("not connected".into());
    };
    if data.is_null() {
        return fail("no data".into());
    }
    let name = unsafe { opt_str(name) }.unwrap_or("paste.bin").to_owned();
    let data = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    match c.client.request(Request::WriteTempFile { name, data }) {
        Ok(Response::Text(path)) => into_c(path),
        Ok(other) => fail(format!("unexpected response {other:?}")),
        Err(e) => fail(e.to_string()),
    }
}

/// Whether the daemon listed `cap` (a `CAP_*` name) in its answer to the Hello.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_has_capability(
    client: *mut thurm_client,
    cap: *const c_char,
) -> bool {
    let (Some(c), Some(cap)) = (unsafe { client_ref(client) }, unsafe { opt_str(cap) }) else {
        return false;
    };
    c.client.has_capability(cap)
}

/// Answers the daemon's `ClipboardQuery` `id` with `len` bytes at `data`, or refuses it when
/// `data` is NULL. Does not wait for the daemon.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_clipboard_answer(
    client: *mut thurm_client,
    id: u64,
    data: *const u8,
    len: usize,
) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let data = (!data.is_null()).then(|| unsafe { std::slice::from_raw_parts(data, len) }.to_vec());
    // Up to 32 MiB over a tunnel that may be slow: written on a thread of its own, so the
    // caller (the app's main thread) does not wait for it.
    let client = c.client.clone();
    let sent = std::thread::Builder::new()
        .name("clipboard-answer".into())
        .spawn(move || {
            send_checked(&client, Request::ClipboardAnswer { id, data });
        });
    if let Err(e) = sent {
        log::warn!("cannot send a clipboard answer: {e}");
    }
}

/// Re-read this Mac's config into the connection's terminal copies (fonts aside, colors and
/// terminal behavior): remote daemons don't send ConfigReloaded when the Mac's config changes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_reload_engine(client: *mut thurm_client) {
    if let Some(c) = unsafe { client_ref(client) } {
        for pane in c.shared.reload_engine(true) {
            fire_frame(c.on_frame, &c.ctx, pane);
        }
    }
}

/// Show the connection's panes in the light or dark variant of this device's theme, without
/// telling the daemon: unlike a `SetAppearance` request, the host (and the other devices showing
/// its panes) keep their appearance. The iPhone follows its own.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_set_local_appearance(client: *mut thurm_client, dark: bool) {
    if let Some(c) = unsafe { client_ref(client) }
        && let Some(panes) = c.shared.set_dark_panes(dark)
    {
        for pane in panes {
            fire_frame(c.on_frame, &c.ctx, pane);
        }
    }
}

fn fire_frame(on_frame: thurm_frame_cb, ctx: &CallbackCtx, pane: PaneId) {
    if let Some(cb) = on_frame {
        ctx.with_ptr(|ptr| unsafe { cb(ptr, pane) });
    }
}

/// Inside a synchronized update the copy shows nothing new until it ends or times out; make
/// sure a frame is requested at the timeout even if no more output comes.
fn schedule_sync_flush(
    shared: &Arc<Shared>,
    pane: PaneId,
    deadline: Instant,
    on_frame: thurm_frame_cb,
    ctx: Arc<CallbackCtx>,
) {
    if !shared.sync_timer.lock().insert(pane) {
        return;
    }
    let owner = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("sync-flush".into())
        .spawn(move || {
            let shared = owner;
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            shared.sync_timer.lock().remove(&pane);
            let Some(local) = shared.local(pane) else {
                return;
            };
            let next = {
                let mut l = local.lock();
                match l.term.sync_deadline() {
                    Some(d) if d > Instant::now() => Some(d),
                    Some(_) => {
                        l.term.flush_sync();
                        l.drain(pane, &shared.images);
                        l.term.sync_deadline()
                    }
                    None => None,
                }
            };
            match next {
                Some(d) => schedule_sync_flush(&shared, pane, d, on_frame, ctx),
                None => {
                    if !shared.is_warm(pane) {
                        fire_frame(on_frame, &ctx, pane);
                    }
                }
            }
        });
    if spawned.is_err() {
        shared.sync_timer.lock().remove(&pane);
    }
}

fn handle_event(
    shared: &Arc<Shared>,
    ev: Event,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: &Arc<CallbackCtx>,
) {
    match ev {
        Event::Attach { pane, size, state } => {
            {
                let mut subs = shared.subs.lock();
                if let Some(n) = subs.discard_attach.get_mut(&pane) {
                    *n -= 1;
                    if *n == 0 {
                        subs.discard_attach.remove(&pane);
                    }
                    return;
                }
            }
            let mut term = Terminal::new(size, shared.engine.lock().engine_config());
            // The daemon has read (and replaced) the pane's own transmissions; any left in the
            // stream (an older or hostile remote daemon) must not read this Mac's files.
            term.set_reads_media(false);
            term.replay(&state);
            let _ = term.drain_events();
            let local = LocalPane {
                term,
                view: ClientView::new(),
            };
            let should_fire = {
                let mut subs = shared.subs.lock();
                if let Some(n) = subs.discard_attach.get_mut(&pane) {
                    *n -= 1;
                    if *n == 0 {
                        subs.discard_attach.remove(&pane);
                    }
                    return;
                }
                shared
                    .locals
                    .lock()
                    .insert(pane, Arc::new(Mutex::new(local)));
                !subs.is_warm(pane)
            };
            shared.images.lock().retain(|(p, _), _| *p != pane);
            if should_fire {
                fire_frame(on_frame, ctx, pane);
            }
        }
        Event::Output { pane, data } => {
            let Some(local) = shared.local(pane) else {
                return;
            };
            let sync = {
                let mut l = local.lock();
                l.term.advance(&data);
                l.drain(pane, &shared.images);
                l.term.sync_deadline()
            };
            match sync {
                Some(deadline) => {
                    schedule_sync_flush(shared, pane, deadline, on_frame, ctx.clone())
                }
                None => {
                    if !shared.is_warm(pane) {
                        fire_frame(on_frame, ctx, pane);
                    }
                }
            }
        }
        Event::Resized { pane, size } => {
            if let Some(local) = shared.local(pane) {
                let held = {
                    let mut l = local.lock();
                    l.term.resize(size);
                    l.term.sync_deadline()
                };
                // The resized grid waits for the program's redraw (see `Terminal::resize`).
                match held {
                    Some(deadline) => {
                        schedule_sync_flush(shared, pane, deadline, on_frame, ctx.clone())
                    }
                    None => {
                        if !shared.is_warm(pane) {
                            fire_frame(on_frame, ctx, pane);
                        }
                    }
                }
            }
        }
        Event::NotifyPrompt {
            pane,
            title,
            body,
            permission,
            sound,
            request,
        } => {
            let mut obj = serde_json::json!({
                "pane": pane,
                "title": title,
                "body": body,
                "permission": permission,
                "sound": sound,
            });
            if let Some(req) = request {
                obj["request"] = serde_json::Value::String(req);
            }
            let json = serde_json::json!({ "Notify": obj }).to_string();
            emit_json(on_event, ctx, &json);
        }
        other => {
            if let Event::PaneClosed { pane } = &other {
                shared.subs.lock().remove_pane(*pane);
                shared.grids.lock().remove(pane);
                shared.locals.lock().remove(pane);
                shared.images.lock().retain(|(p, _), _| p != pane);
            }
            let reloaded = matches!(other, Event::ConfigReloaded)
                .then(|| shared.reload_engine(true));
            match serde_json::to_string(&other) {
                Ok(json) => emit_json(on_event, ctx, &json),
                Err(e) => log::warn!("cannot encode event: {e}"),
            }
            if let Some(panes) = reloaded {
                for pane in panes {
                    fire_frame(on_frame, ctx, pane);
                }
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_disconnect(client: *mut thurm_client) {
    if !client.is_null() {
        drop(unsafe { Box::from_raw(client) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_client_set_ctx_drop(
    client: *mut thurm_client,
    on_drop: thurm_ctx_drop_cb,
) {
    if let Some(c) = unsafe { client_ref(client) } {
        c.ctx.set_on_drop(on_drop);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

unsafe fn client_ref<'a>(c: *mut thurm_client) -> Option<&'a thurm_client> {
    unsafe { c.as_ref() }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_request(
    client: *mut thurm_client,
    json: *const c_char,
) -> *mut c_char {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return into_c(error_json("null client"));
    };
    let Some(json) = (unsafe { opt_str(json) }) else {
        return into_c(error_json("invalid string"));
    };
    let req: Request = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(e) => return into_c(error_json(format!("bad request: {e}"))),
    };
    let req = match c.local_request(req) {
        Ok(resp) => return into_c(serde_json::to_string(&resp).unwrap_or_else(error_json)),
        Err(req) => req,
    };
    let out = match c.client.request(req) {
        Ok(resp) => serde_json::to_string(&resp).unwrap_or_else(error_json),
        Err(e) => error_json(e),
    };
    into_c(out)
}

/// Like [`thurm_request`], but gives up after `timeout_ms` (0: no limit) with an error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_request_timeout(
    client: *mut thurm_client,
    json: *const c_char,
    timeout_ms: u64,
) -> *mut c_char {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return into_c(error_json("null client"));
    };
    let req = match unsafe { parse_request(json) } {
        Ok(r) => r,
        Err(e) => return into_c(e),
    };
    let req = match c.local_request(req) {
        Ok(resp) => return into_c(serde_json::to_string(&resp).unwrap_or_else(error_json)),
        Err(req) => req,
    };
    into_c(daemon_request(&c.client, req, timeout_ms, direct_host(c)))
}

/// Callback of [`thurm_request_async`]: the response JSON (freed with `thurm_string_free` by
/// the callee), on an arbitrary thread.
pub type thurm_response_cb = Option<unsafe extern "C" fn(ctx: *mut c_void, json: *mut c_char)>;

/// Sends a request without waiting: `cb` gets the response (or an error after `timeout_ms`,
/// 0: no limit) on another thread. The connection is held until then, so the caller may
/// disconnect `client` meanwhile.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_request_async(
    client: *mut thurm_client,
    json: *const c_char,
    timeout_ms: u64,
    cb: thurm_response_cb,
    ctx: *mut c_void,
) {
    let reply = move |ctx: Ctx, out: String| {
        if let Some(cb) = cb {
            unsafe { cb(ctx.0, into_c(out)) };
        }
    };
    let ctx = Ctx(ctx);
    let Some(c) = (unsafe { client_ref(client) }) else {
        return reply(ctx, error_json("null client"));
    };
    let req = match unsafe { parse_request(json) } {
        Ok(r) => r,
        Err(e) => return reply(ctx, e),
    };
    let req = match c.local_request(req) {
        Ok(resp) => return reply(ctx, serde_json::to_string(&resp).unwrap_or_else(error_json)),
        Err(req) => req,
    };
    // The Arc keeps the connection alive for the request, whatever the caller does.
    let conn = c.client.clone();
    let direct = direct_host(c);
    let spawned = std::thread::Builder::new()
        .name("thurm-request".into())
        .spawn(move || reply(ctx, daemon_request(&conn, req, timeout_ms, direct)));
    if let Err(e) = spawned {
        reply(
            ctx,
            error_json(format!("cannot start a request thread: {e}")),
        );
    }
}

unsafe fn parse_request(json: *const c_char) -> Result<Request, String> {
    let json = unsafe { opt_str(json) }.ok_or_else(|| error_json("invalid string"))?;
    serde_json::from_str(json).map_err(|e| error_json(format!("bad request: {e}")))
}

/// Whether `c` is a host without Thurm (`thurm_direct_connect`), whose daemon is in the app.
#[cfg(feature = "mobile")]
fn direct_host(c: &thurm_client) -> bool {
    c._transport
        .as_ref()
        .is_some_and(|t| t.is::<thurm_direct::Direct>())
}

#[cfg(not(feature = "mobile"))]
fn direct_host(_: &thurm_client) -> bool {
    false
}

fn daemon_request(client: &Client, req: Request, timeout_ms: u64, direct: bool) -> String {
    let timeout = (timeout_ms > 0).then(|| std::time::Duration::from_millis(timeout_ms));
    match client.request_timeout(req, timeout) {
        Ok(resp) => serde_json::to_string(&resp).unwrap_or_else(error_json),
        Err(::thurm_client::ClientError::Timeout) if direct => {
            error_json("the host did not answer in time")
        }
        Err(e) => error_json(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_send(client: *mut thurm_client, json: *const c_char) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let Some(json) = (unsafe { opt_str(json) }) else {
        return;
    };
    match serde_json::from_str::<Request>(json) {
        Ok(req) => {
            if let Err(req) = c.local_request(req) {
                send_checked(&c.client, req);
            }
        }
        Err(e) => log::warn!("thurm_send: bad request: {e}"),
    }
}

impl thurm_client {
    /// Requests about what the app shows (viewport, selection, search) are answered by the
    /// pane's local copy; anything else comes back to go to the daemon.
    #[allow(clippy::result_large_err)]
    fn local_request(&self, req: Request) -> Result<Response, Request> {
        let pane = match &req {
            Request::Selection { pane, .. }
            | Request::CopySelection { pane }
            | Request::Search { pane, .. }
            | Request::Scroll { pane, .. } => *pane,
            Request::SetAppearance { dark } => {
                if let Some(panes) = self.shared.set_dark_panes(*dark) {
                    for pane in panes {
                        fire_frame(self.on_frame, &self.ctx, pane);
                    }
                }
                return Err(req);
            }
            _ => return Err(req),
        };
        let Some(local) = self.shared.local(pane) else {
            return Err(req);
        };
        let mut l = local.lock();
        let resp = match req {
            Request::Selection { op, .. } => {
                l.term.selection(op);
                Response::Ok
            }
            Request::CopySelection { .. } => Response::Text(l.term.selection_text()),
            Request::Search {
                query, direction, ..
            } => Response::Search {
                found: l.term.search(query.as_deref(), direction),
            },
            Request::Scroll { scroll, .. } => {
                l.term.scroll(scroll);
                Response::Ok
            }
            _ => unreachable!(),
        };
        drop(l);
        fire_frame(self.on_frame, &self.ctx, pane);
        Ok(resp)
    }

    /// Bytes the local copy encoded for the program (keys, mouse reports).
    fn input(&self, pane: PaneId, data: Vec<u8>) {
        if !data.is_empty() {
            send_checked(&self.client, Request::Input { pane, data });
        }
    }
}

/// Enqueues `req` onto `client`. If the bounded outgoing byte queue is full (peer/tunnel stalled),
/// forces a disconnect immediately rather than silently discarding accepted UI input or control
/// frames on an apparently live connection, allowing the UI to show offline state and resync.
fn send_checked(client: &Client, req: Request) -> bool {
    match client.send(req) {
        Ok(()) => true,
        Err(ClientError::QueueFull) => {
            log::warn!(
                "thurm-ffi: outgoing queue full; closing stalled connection to resynchronize"
            );
            client.force_close();
            false
        }
        Err(_) => false,
    }
}

fn send(client: *mut thurm_client, req: Request) {
    if let Some(c) = unsafe { client_ref(client) } {
        send_checked(&c.client, req);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_subscribe(client: *mut thurm_client, pane: u64) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let fire = {
        let mut subs = c.shared.subs.lock();
        if subs.subscribed.contains(&pane) {
            *subs.active.entry(pane).or_insert(0) += 1;
            subs.remove_warm(pane);
            // Warm hit or overlapping active view: stay subscribed without re-attaching.
            c.shared.locals.lock().contains_key(&pane)
        } else if send_checked(&c.client, Request::Subscribe { pane }) {
            *subs.active.entry(pane).or_insert(0) += 1;
            subs.remove_warm(pane);
            subs.subscribed.insert(pane);
            false
        } else {
            // Subscribe frame was not accepted (queue full -> disconnected, or already dead):
            // do not leave local bookkeeping claiming the pane is subscribed.
            false
        }
    };
    if fire {
        fire_frame(c.on_frame, &c.ctx, pane);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_unsubscribe(client: *mut thurm_client, pane: u64) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let mut subs = c.shared.subs.lock();
    if let Some(count) = subs.active.get_mut(&pane) {
        *count = count.saturating_sub(1);
        if *count > 0 {
            return;
        }
        subs.active.remove(&pane);
    }
    if subs.warm_limit > 0 && subs.subscribed.contains(&pane) {
        // Keep the local copy subscribed and warm in LRU order.
        subs.remove_warm(pane);
        subs.warm_order.push_back(pane);
        subs.evict_excess(&c.shared, &c.client);
    } else if subs.warm_limit == 0 {
        // Legacy behavior: drop copy immediately and unsubscribe on the daemon.
        let was_subscribed = subs.subscribed.remove(&pane);
        let had_local = c.shared.locals.lock().remove(&pane).is_some();
        if was_subscribed && !had_local {
            *subs.discard_attach.entry(pane).or_insert(0) += 1;
        }
        send_checked(&c.client, Request::Unsubscribe { pane });
    } else {
        c.shared.locals.lock().remove(&pane);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_set_warm_limit(client: *mut thurm_client, limit: u32) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let mut subs = c.shared.subs.lock();
    subs.warm_limit = limit as usize;
    subs.evict_excess(&c.shared, &c.client);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_evict_pane(client: *mut thurm_client, pane: u64) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let mut subs = c.shared.subs.lock();
    subs.active.remove(&pane);
    subs.remove_warm(pane);
    let was_subscribed = subs.subscribed.remove(&pane);
    let had_local = c.shared.locals.lock().remove(&pane).is_some();
    if was_subscribed {
        if !had_local {
            *subs.discard_attach.entry(pane).or_insert(0) += 1;
        }
        send_checked(&c.client, Request::Unsubscribe { pane });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_prewarm_pane(client: *mut thurm_client, pane: u64) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let mut subs = c.shared.subs.lock();
    if subs.warm_limit == 0 || subs.subscribed.contains(&pane) || subs.active.contains_key(&pane) {
        return;
    }
    if send_checked(&c.client, Request::Subscribe { pane }) {
        subs.subscribed.insert(pane);
        subs.warm_order.push_back(pane);
        subs.evict_excess(&c.shared, &c.client);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_input(
    client: *mut thurm_client,
    pane: u64,
    data: *const u8,
    len: usize,
) {
    if data.is_null() || len == 0 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    clear_local_selection(client, pane);
    send(client, Request::Input { pane, data });
}

fn clear_local_selection(client: *mut thurm_client, pane: PaneId) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let Some(local) = c.shared.local(pane) else {
        return;
    };
    if local.lock().term.clear_selection() {
        fire_frame(c.on_frame, &c.ctx, pane);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_paste(client: *mut thurm_client, pane: u64, text: *const c_char) {
    if let Some(t) = unsafe { opt_str(text) } {
        clear_local_selection(client, pane);
        send(
            client,
            Request::Paste {
                pane,
                text: t.to_owned(),
            },
        );
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_resize(
    client: *mut thurm_client,
    pane: u64,
    cols: u16,
    rows: u16,
    cell_width: u16,
    cell_height: u16,
) {
    send(
        client,
        Request::Resize {
            pane,
            size: PaneSize {
                cols,
                rows,
                cell_width,
                cell_height,
            },
        },
    );
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_focus(client: *mut thurm_client, pane: u64, focused: bool) {
    send(client, Request::Focus { pane, focused });
}

pub fn named_key(code: u32) -> Option<NamedKey> {
    use NamedKey::*;
    Some(match code {
        1 => Escape,
        2 => Enter,
        3 => Tab,
        4 => Backspace,
        5 => Insert,
        6 => Delete,
        7 => Left,
        8 => Right,
        9 => Up,
        10 => Down,
        11 => PageUp,
        12 => PageDown,
        13 => Home,
        14 => End,
        15 => CapsLock,
        16 => ScrollLock,
        17 => NumLock,
        18 => PrintScreen,
        19 => Pause,
        20 => Menu,
        21 => Kp0,
        22 => Kp1,
        23 => Kp2,
        24 => Kp3,
        25 => Kp4,
        26 => Kp5,
        27 => Kp6,
        28 => Kp7,
        29 => Kp8,
        30 => Kp9,
        31 => KpDecimal,
        32 => KpDivide,
        33 => KpMultiply,
        34 => KpSubtract,
        35 => KpAdd,
        36 => KpEnter,
        37 => KpEqual,
        38 => LeftShift,
        39 => LeftControl,
        40 => LeftAlt,
        41 => LeftSuper,
        42 => RightShift,
        43 => RightControl,
        44 => RightAlt,
        45 => RightSuper,
        46 => VolumeDown,
        47 => VolumeUp,
        48 => VolumeMute,
        100..=134 => F((code - 99) as u8),
        _ => return None,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_key(
    client: *mut thurm_client,
    pane: u64,
    ev: *const thurm_key_event,
) {
    let Some(ev) = (unsafe { ev.as_ref() }) else {
        return;
    };
    let key = if ev.kind == 1 {
        match named_key(ev.code) {
            Some(k) => Key::Named(k),
            None => return,
        }
    } else {
        match char::from_u32(ev.code) {
            Some(c) => Key::Char(c),
            None => return,
        }
    };
    let action = match ev.action {
        1 => KeyAction::Repeat,
        2 => KeyAction::Release,
        _ => KeyAction::Press,
    };
    let key = KeyEvent {
        key,
        mods: ev.mods,
        action,
        text: unsafe { opt_str(ev.text) }.unwrap_or("").to_owned(),
        shifted: (ev.shifted != 0)
            .then(|| char::from_u32(ev.shifted))
            .flatten(),
        base_layout: (ev.base_layout != 0)
            .then(|| char::from_u32(ev.base_layout))
            .flatten(),
    };
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    match c.shared.local(pane) {
        Some(local) => {
            let (bytes, cleared) = {
                let mut l = local.lock();
                let had = l.term.has_selection();
                let bytes = l.term.key(&key);
                (bytes, had && !l.term.has_selection())
            };
            if cleared {
                fire_frame(c.on_frame, &c.ctx, pane);
            }
            c.input(pane, bytes);
        }
        None => {
            send_checked(&c.client, Request::Key { pane, key });
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_mouse(
    client: *mut thurm_client,
    pane: u64,
    ev: *const thurm_mouse_event,
) {
    let Some(ev) = (unsafe { ev.as_ref() }) else {
        return;
    };
    let kind = match ev.kind {
        0 => MouseKind::Press,
        1 => MouseKind::Release,
        _ => MouseKind::Move,
    };
    let button = match ev.button {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        3 => MouseButton::Back,
        4 => MouseButton::Forward,
        _ => MouseButton::None,
    };
    let event = MouseEvent {
        kind,
        button,
        col: ev.col,
        row: ev.row,
        right_half: ev.right_half,
        mods: ev.mods,
        clicks: ev.clicks.max(1),
        x: ev.x,
        y: ev.y,
    };
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let Some(local) = c.shared.local(pane) else {
        send_checked(&c.client, Request::Mouse { pane, event });
        return;
    };
    let (text, bytes) = {
        let mut l = local.lock();
        let had = l.term.has_selection();
        let (outcome, bytes) = l.term.mouse(&event);
        let copy = outcome == MouseOutcome::SelectionDone
            && c.shared.engine.lock().cfg.terminal.copy_on_select;
        let cleared = had && !l.term.has_selection();
        if cleared || (outcome != MouseOutcome::None && outcome != MouseOutcome::Reported) {
            fire_frame(c.on_frame, &c.ctx, pane);
        }
        (copy.then(|| l.term.selection_text()), bytes)
    };
    c.input(pane, bytes);
    // The user's own selection (copy on select): no policy applies.
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        let json =
            serde_json::json!({"ClipboardStore": {"pane": pane, "text": text, "user": true}});
        emit_json(c.on_event, &c.ctx, &json.to_string());
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_wheel(
    client: *mut thurm_client,
    pane: u64,
    lines: i32,
    col: u16,
    row: u16,
    mods: u8,
) {
    if lines == 0 {
        return;
    }
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    match c.shared.local(pane) {
        Some(local) => {
            let bytes = local.lock().term.wheel(lines, col, row, mods);
            if bytes.is_empty() {
                // Scrolled the local viewport.
                fire_frame(c.on_frame, &c.ctx, pane);
            }
            c.input(pane, bytes);
        }
        None => {
            send_checked(
                &c.client,
                Request::Wheel {
                    pane,
                    lines,
                    col,
                    row,
                    mods,
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Grid access
// ---------------------------------------------------------------------------------------------

unsafe fn grid_lock_inner(
    client: *mut thurm_client,
    pane: u64,
    last_generation: Option<u64>,
    info: *mut thurm_grid_info,
    cells: *mut *const thurm_cell,
) -> bool {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return false;
    };
    let slot = c.shared.grids.lock().entry(pane).or_default().clone();
    let mut guard = slot.lock_arc();
    if let Some(local) = c.shared.local(pane) {
        let snap = {
            let mut l = local.lock();
            let l = &mut *l;
            l.term.snapshot(pane, &mut l.view)
        };
        if let Some(snap) = snap {
            let mut images = c.shared.images.lock();
            for img in snap.new_images {
                images.insert(
                    (pane, img.id),
                    Arc::new(ImageSlot {
                        width: img.width,
                        height: img.height,
                        rgba: Arc::clone(&img.rgba),
                        serial: IMAGE_SERIAL.fetch_add(1, Ordering::Relaxed),
                    }),
                );
            }
            drop(images);
            guard.apply(snap.frame);
        }
    }
    if !guard.has_frame {
        return false;
    }
    let mut out_info = guard.info;
    if let Some(last_gen) = last_generation {
        if last_gen == 0 {
            // A first-time reader has never drawn this grid, even if an incremental update's
            // dirty bits are still unconsumed from another reader.
            out_info.dirty_rows = u64::MAX;
            out_info.shift = 0;
        } else if out_info.generation != last_gen {
            // Preserve zero dirty_rows on a consecutive metadata/cursor-only update when no other
            // reader has already consumed that generation; force a full repaint only if the
            // reader skipped/rewound generations or another reader already consumed the mask.
            let jumped = out_info.generation != last_gen.saturating_add(1);
            let consumed_by_other = last_gen < guard.last_consumed_gen;
            if jumped || consumed_by_other {
                out_info.dirty_rows = u64::MAX;
                out_info.shift = 0;
            }
        }
    }
    if !info.is_null() {
        unsafe { *info = out_info };
    }
    if !cells.is_null() {
        unsafe { *cells = guard.cells.as_ptr() };
    }
    // The caller now has a consistent view; the next lock reports rows changed after this.
    guard.last_consumed_gen = guard.info.generation;
    guard.info.dirty_rows = 0;
    guard.info.shift = 0;
    // Double lock without unlock: dropping the previous guard releases it to avoid a deadlock.
    c.shared.locked.lock().insert(pane, guard);
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_lock(
    client: *mut thurm_client,
    pane: u64,
    info: *mut thurm_grid_info,
    cells: *mut *const thurm_cell,
) -> bool {
    unsafe { grid_lock_inner(client, pane, None, info, cells) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_lock_for(
    client: *mut thurm_client,
    pane: u64,
    last_generation: u64,
    info: *mut thurm_grid_info,
    cells: *mut *const thurm_cell,
) -> bool {
    unsafe { grid_lock_inner(client, pane, Some(last_generation), info, cells) }
}

unsafe fn locked_grid<'a>(client: *mut thurm_client, pane: u64) -> Option<&'a Grid> {
    let c = unsafe { client_ref(client) }?;
    let locked = c.shared.locked.lock();
    let slot = locked.get(&pane)?;
    // Safety: the mutex is held by the caller (between grid_lock and grid_unlock) and the
    // guard is kept alive in `locked` until unlock.
    Some(unsafe { &*ArcMutexGuard::mutex(slot).data_ptr() })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_cluster(
    client: *mut thurm_client,
    pane: u64,
    row: u16,
    col: u16,
) -> *const c_char {
    match unsafe { locked_grid(client, pane) }.and_then(|g| g.clusters.get(&(row, col))) {
        Some(s) => s.as_ptr(),
        None => std::ptr::null(),
    }
}

#[repr(C)]
pub struct thurm_cluster {
    pub row: u16,
    pub col: u16,
    pub text: *const c_char,
}

/// While locked: every grapheme cluster of the grid (one call instead of one per cell).
/// Copies up to `max` into `out` and returns the total count.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_clusters(
    client: *mut thurm_client,
    pane: u64,
    out: *mut thurm_cluster,
    max: usize,
) -> usize {
    let Some(g) = (unsafe { locked_grid(client, pane) }) else {
        return 0;
    };
    if !out.is_null() {
        for (i, ((row, col), text)) in g.clusters.iter().take(max).enumerate() {
            unsafe {
                *out.add(i) = thurm_cluster {
                    row: *row,
                    col: *col,
                    text: text.as_ptr(),
                }
            };
        }
    }
    g.clusters.len()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_peek(
    client: *mut thurm_client,
    pane: u64,
    cells: *mut *const thurm_cell,
) -> usize {
    let Some(g) = (unsafe { locked_grid(client, pane) }) else {
        return 0;
    };
    if !cells.is_null() {
        unsafe { *cells = g.peek.as_ptr() };
    }
    g.peek.len()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_peek_cluster(
    client: *mut thurm_client,
    pane: u64,
    col: u16,
) -> *const c_char {
    match unsafe { locked_grid(client, pane) }.and_then(|g| g.peek_clusters.get(&col)) {
        Some(s) => s.as_ptr(),
        None => std::ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_link(
    client: *mut thurm_client,
    pane: u64,
    index: u16,
) -> *const c_char {
    match unsafe { locked_grid(client, pane) }.and_then(|g| g.links.get(index as usize)) {
        Some(s) => s.as_ptr(),
        None => std::ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_images(
    client: *mut thurm_client,
    pane: u64,
    out: *mut thurm_image_placement,
    max: usize,
) -> usize {
    let Some(g) = (unsafe { locked_grid(client, pane) }) else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let n = g.images.len().min(max);
    unsafe { std::ptr::copy_nonoverlapping(g.images.as_ptr(), out, n) };
    n
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_unlock(client: *mut thurm_client, pane: u64) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    c.shared.locked.lock().remove(&pane);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_image_serial(
    client: *mut thurm_client,
    pane: u64,
    image: u32,
) -> u64 {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return 0;
    };
    c.shared
        .images
        .lock()
        .get(&(pane, image))
        .map_or(0, |slot| slot.serial)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_image_lock(
    client: *mut thurm_client,
    pane: u64,
    image: u32,
    width: *mut u32,
    height: *mut u32,
    rgba: *mut *const u8,
) -> bool {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return false;
    };
    let Some(slot) = c.shared.images.lock().get(&(pane, image)).cloned() else {
        return false;
    };
    unsafe {
        if !width.is_null() {
            *width = slot.width;
        }
        if !height.is_null() {
            *height = slot.height;
        }
        if !rgba.is_null() {
            *rgba = slot.rgba.as_ptr();
        }
    }
    c.shared
        .locked_images
        .lock()
        .entry((pane, image))
        .or_default()
        .push(slot);
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_image_unlock(client: *mut thurm_client, pane: u64, image: u32) {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return;
    };
    let mut locked = c.shared.locked_images.lock();
    if let Some(v) = locked.get_mut(&(pane, image)) {
        v.pop();
        if v.is_empty() {
            locked.remove(&(pane, image));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------------------------

/// Show every pane in theme `name` (a theme name or file) without saving it; `NULL` goes back
/// to the configured theme. Returns the theme as JSON (like `theme` in `thurm_config_json`),
/// or `NULL` for an unknown theme (nothing changes then). Free the result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_preview_theme(
    client: *mut thurm_client,
    name: *const c_char,
) -> *mut c_char {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return std::ptr::null_mut();
    };
    let theme = match unsafe { opt_str(name) } {
        Some(n) => match thurm_config::load_theme(n) {
            Ok(t) => Some(t),
            Err(_) => return std::ptr::null_mut(),
        },
        None => None,
    };
    let shown = {
        let mut e = c.shared.engine.lock();
        e.preview = theme;
        e.engine_config().theme
    };
    for pane in c.shared.reload_engine(false) {
        fire_frame(c.on_frame, &c.ctx, pane);
    }
    into_c(serde_json::to_string(&shown).unwrap_or_default())
}

/// Use theme `spec` (as `colors.theme`) in place of the config file's for the rest of this
/// process, when the file cannot record it; `NULL` goes back to the file's. Reload the config
/// and the engines afterwards. Returns `NULL`, or why `spec` is not a theme. Free the result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_set_session_theme(spec: *const c_char) -> *mut c_char {
    let Some(spec) = (unsafe { opt_str(spec) }) else {
        thurm_config::set_session_theme(None);
        return std::ptr::null_mut();
    };
    let spec = thurm_config::ThemeSpec::parse(spec);
    match spec.validate() {
        Ok(()) => {
            thurm_config::set_session_theme(Some(&spec));
            std::ptr::null_mut()
        }
        Err(e) => into_c(e),
    }
}

/// Theme `name` (a theme name or file) as JSON, like `theme` in `thurm_config_json`, or null
/// when there is no such theme: the theme picker draws its previews with it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_theme_json(name: *const c_char) -> *mut c_char {
    let Some(name) = (unsafe { opt_str(name) }) else {
        return std::ptr::null_mut();
    };
    match thurm_config::load_theme(name).map(|t| serde_json::to_string(&t)) {
        Ok(Ok(json)) => into_c(json),
        _ => std::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn thurm_config_json(dark: bool) -> *mut c_char {
    let _ = thurm_config::ensure_default_config();
    let json = match thurm_config::Config::load_with_warnings() {
        Ok((cfg, warnings)) if warnings.is_empty() => cfg.ui_json(dark),
        Ok((cfg, warnings)) => {
            let mut v: serde_json::Value =
                serde_json::from_str(&cfg.ui_json(dark)).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("warnings".into(), serde_json::json!(warnings));
            }
            v.to_string()
        }
        Err(e) => {
            let mut v: serde_json::Value =
                serde_json::from_str(&thurm_config::Config::default().ui_json(dark))
                    .unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("error".into(), serde_json::Value::String(e.to_string()));
            }
            v.to_string()
        }
    };
    into_c(json)
}

/// Stop the daemon on the default socket whatever version it is (SIGTERM; it saves the
/// session). Blocks until it has exited (at most ~10 s). Returns false if none was running or
/// it did not stop.
#[unsafe(no_mangle)]
pub extern "C" fn thurm_terminate_daemon() -> bool {
    ::thurm_client::terminate_daemon(&thurm_config::socket_path()).is_ok()
}

/// Replace the running daemon with `daemon_path` in place (its panes keep running).
/// Returns 0 when the daemon now runs this library's build, 1 when it is too old for in-place
/// upgrades (only `thurm_terminate_daemon` replaces it), 2 on failure. Blocks up to ~15 s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_upgrade_daemon(daemon_path: *const c_char) -> i32 {
    let Some(daemon) = (unsafe { opt_str(daemon_path) }) else {
        return 2;
    };
    match ::thurm_client::upgrade_daemon(&thurm_config::socket_path(), Path::new(daemon)) {
        Ok(_) => 0,
        Err(::thurm_client::UpgradeError::TooOld(_)) => 1,
        Err(e) => {
            log::warn!("daemon upgrade: {e}");
            2
        }
    }
}

/// This build's identifier (the daemon reports its own in the Hello response). Static; do not
/// free.
#[unsafe(no_mangle)]
pub extern "C" fn thurm_build_id() -> *const c_char {
    static BUILD: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();
    BUILD
        .get_or_init(|| std::ffi::CString::new(thurm_proto::BUILD).unwrap_or_default())
        .as_ptr()
}

/// The wire protocol version this library speaks (the app sends it in `Hello`).
#[unsafe(no_mangle)]
pub extern "C" fn thurm_protocol_version() -> u32 {
    thurm_proto::PROTOCOL_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn thurm_config_path() -> *mut c_char {
    into_c(thurm_config::config_path().display().to_string())
}

#[unsafe(no_mangle)]
pub extern "C" fn thurm_settings_json() -> *mut c_char {
    into_c(thurm_config::settings::view().to_string())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_settings_set(
    key: *const c_char,
    json: *const c_char,
) -> *mut c_char {
    let (Some(key), Some(json)) = (unsafe { opt_str(key) }, unsafe { opt_str(json) }) else {
        return into_c("no setting or value given".into());
    };
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(value) => value,
        Err(e) => return into_c(e.to_string()),
    };
    match thurm_config::settings::set(key, &value) {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => into_c(e),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn thurm_settings_overlay_path() -> *mut c_char {
    into_c(
        thurm_config::settings::open_overlay()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_events_reach_the_app_saying_whether_they_are_initial() {
        // The JSON the app decodes: the event's name, then its fields, `initial` among them.
        let json = |initial| {
            serde_json::to_value(Event::PortsChanged {
                pane: 3,
                ports: vec![thurm_proto::PortEntry {
                    port: 5173,
                    address: "127.0.0.1".into(),
                    pid: 9,
                    command: "vite".into(),
                }],
                complete: true,
                initial,
            })
            .unwrap()
        };
        assert_eq!(json(true)["PortsChanged"]["initial"], true);
        assert_eq!(json(false)["PortsChanged"]["initial"], false);
        assert_eq!(json(true)["PortsChanged"]["ports"][0]["port"], 5173);
    }
    use thurm_proto::{Cell, FrameRow};

    #[test]
    fn resize_flush_keeps_the_synchronized_update_timeout() {
        let shared = Arc::new(Shared::default());
        let size = PaneSize {
            cols: 40,
            rows: 5,
            cell_width: 10,
            cell_height: 20,
        };
        let mut term = Terminal::new(size, EngineConfig::default());
        term.resize(PaneSize { cols: 30, ..size });
        term.advance(b"\x1b[?2026h\x1b[H\x1b[2Jpartial");
        let deadline = term.sync_deadline().unwrap();
        let local = Arc::new(Mutex::new(LocalPane {
            term,
            view: ClientView::new(),
        }));
        shared.locals.lock().insert(1, local.clone());
        // No output follows: the timer must flush both the short resize hold and the
        // longer synchronized update before announcing a drawable frame.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        unsafe extern "C" fn frame(ctx: *mut c_void, _pane: u64) {
            let tx = unsafe { Box::from_raw(ctx.cast::<std::sync::mpsc::Sender<()>>()) };
            let _ = tx.send(());
        }
        let ctx = CallbackCtx::new(Box::into_raw(Box::new(tx)).cast());
        schedule_sync_flush(&shared, 1, deadline, Some(frame), ctx);
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let mut l = local.lock();
        assert!(
            !l.term.in_sync(),
            "callback fired before the second hold expired"
        );
        let l = &mut *l;
        assert_eq!(l.term.snapshot(1, &mut l.view).unwrap().frame.cols, 30);
    }

    #[test]
    fn async_requests_always_answer() {
        unsafe extern "C" fn cb(ctx: *mut c_void, json: *mut c_char) {
            let tx = unsafe { Box::from_raw(ctx as *mut std::sync::mpsc::Sender<String>) };
            let text = unsafe { CStr::from_ptr(json) }
                .to_string_lossy()
                .into_owned();
            unsafe { thurm_string_free(json) };
            let _ = tx.send(text);
        }
        for (client, json) in [
            (std::ptr::null_mut(), c"\"ListPanes\""),
            (std::ptr::null_mut(), c"not json"),
        ] {
            let (tx, rx) = std::sync::mpsc::channel::<String>();
            let ctx = Box::into_raw(Box::new(tx)) as *mut c_void;
            unsafe { thurm_request_async(client, json.as_ptr(), 100, Some(cb), ctx) };
            let text = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
            assert!(text.contains("error"), "{text}");
        }
    }

    #[test]
    fn local_appearance_changes_only_this_devices_engine() {
        let shared = Shared::default();
        assert!(shared.engine.lock().dark);
        assert!(shared.set_dark_panes(false).is_some());
        assert!(!shared.engine.lock().dark);
        assert!(shared.set_dark_panes(false).is_none(), "no change, no reload");
        // Without a connection there is nothing to change.
        unsafe { thurm_set_local_appearance(std::ptr::null_mut(), true) };
    }

    /// Through a connection to a daemon that records what it is sent: the local call turns
    /// the attached pane light and sends nothing; `SetAppearance` turns it dark and reaches
    /// the daemon.
    #[test]
    fn local_appearance_stays_off_the_daemon() {
        use std::io::BufReader;
        use std::os::unix::net::UnixListener;
        use thurm_proto::{Envelope, ServerMessage, codec};

        let dir = std::env::temp_dir().join(format!("thurm-ffi-look-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("s");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let received = Arc::new(Mutex::new(Vec::<Request>::new()));
        let log = received.clone();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut r) {
                let result = match &env.request {
                    Request::Hello { .. } => Response::Hello {
                        version: thurm_proto::PROTOCOL_VERSION,
                        daemon_pid: 1,
                        restored: false,
                        build: String::new(),
                        capabilities: thurm_proto::CAPABILITIES
                            .iter()
                            .map(|c| (*c).to_owned())
                            .collect(),
                    },
                    // The pane arrives before the answer, so it is attached when that returns.
                    Request::ListPanes => {
                        let attach = Event::Attach {
                            pane: 1,
                            size: PaneSize {
                                cols: 20,
                                rows: 5,
                                cell_width: 8,
                                cell_height: 16,
                            },
                            state: Vec::new(),
                        };
                        codec::write_message(&mut w, &ServerMessage::Event(attach)).unwrap();
                        Response::Ok
                    }
                    _ => Response::Ok,
                };
                log.lock().push(env.request);
                let resp = ServerMessage::Response {
                    id: env.id,
                    result: Ok(result),
                };
                codec::write_message(&mut w, &resp).unwrap();
            }
        });

        let socket = CString::new(sock.to_str().unwrap()).unwrap();
        let mut err = std::ptr::null_mut();
        let client = unsafe {
            thurm_connect_socket(
                socket.as_ptr(),
                std::ptr::null(),
                c"test".as_ptr(),
                None,
                None,
                std::ptr::null_mut(),
                &mut err,
            )
        };
        assert!(!client.is_null());
        // This device's theme, not the config of whoever runs the tests.
        let c = unsafe { client_ref(client) }.unwrap();
        c.shared.engine.lock().cfg =
            thurm_config::Config::parse("[colors]\ntheme = \"light:thurm-light,dark:thurm\"\n")
                .unwrap();
        let request = |json: &CStr| {
            let out = unsafe { thurm_request(client, json.as_ptr()) };
            let text = unsafe { CStr::from_ptr(out) }
                .to_string_lossy()
                .into_owned();
            unsafe { thurm_string_free(out) };
            text
        };
        assert_eq!(request(c"\"ListPanes\""), "\"Ok\"");
        let background = || {
            let mut info = thurm_grid_info::default();
            assert!(unsafe { thurm_grid_lock(client, 1, &mut info, std::ptr::null_mut()) });
            unsafe { thurm_grid_unlock(client, 1) };
            info.background
        };
        let light = thurm_config::builtin_theme("thurm-light")
            .unwrap()
            .background;
        let dark = thurm_config::builtin_theme("thurm").unwrap().background;
        assert_ne!(light, dark);
        assert_eq!(background(), dark);

        unsafe { thurm_set_local_appearance(client, false) };
        assert_eq!(background(), light);
        // A round trip: anything sent before it has reached the daemon.
        assert_eq!(request(c"\"ListPanes\""), "\"Ok\"");
        let seen = |r: &[Request]| r.iter().any(|r| matches!(r, Request::SetAppearance { .. }));
        assert!(!seen(&received.lock()), "{:?}", received.lock());

        assert_eq!(request(c"{\"SetAppearance\":{\"dark\":true}}"), "\"Ok\"");
        assert_eq!(background(), dark);
        assert!(
            received
                .lock()
                .iter()
                .any(|r| matches!(r, Request::SetAppearance { dark: true })),
            "{:?}",
            received.lock()
        );

        unsafe { thurm_disconnect(client) };
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn theme_json_has_the_colors() {
        let json = unsafe { thurm_theme_json(c"thurm-light".as_ptr()) };
        assert!(!json.is_null());
        let text = unsafe { CStr::from_ptr(json) }
            .to_string_lossy()
            .into_owned();
        unsafe { thurm_string_free(json) };
        let t: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(t["background"], 0xfafbfc);
        assert_eq!(t["palette"].as_array().unwrap().len(), 16);
        assert!(unsafe { thurm_theme_json(c"no such theme".as_ptr()) }.is_null());
        assert!(unsafe { thurm_theme_json(std::ptr::null()) }.is_null());
    }

    #[test]
    fn dirty_mask_shifts() {
        assert_eq!(shift_dirty(0b1011, 1, 10), 0b10110);
        assert_eq!(shift_dirty(0b1011, -1, 10), 0b101);
        // Bit 63 covers rows 63.., which stay folded into bit 63 or move below it.
        assert_eq!(
            shift_dirty(1 << 63, -2, 100),
            (1 << 61) | (1 << 62) | (1 << 63)
        );
        assert_eq!(shift_dirty(1 << 4, 10, 10), 0);
    }

    #[test]
    fn rows_shift_in_place() {
        let grid = |chars: &str| -> Vec<thurm_cell> {
            chars
                .chars()
                .map(|ch| thurm_cell {
                    ch: ch as u32,
                    ..Default::default()
                })
                .collect()
        };
        let text = |cells: &[thurm_cell]| -> String {
            cells
                .iter()
                .map(|c| char::from_u32(c.ch).filter(|&c| c != '\0').unwrap_or('.'))
                .collect()
        };
        // 2 columns, 4 rows.
        let mut cells = grid("aabbccdd");
        shift_rows(&mut cells, 2, 4, 1);
        assert_eq!(text(&cells), "..aabbcc");
        let mut cells = grid("aabbccdd");
        shift_rows(&mut cells, 2, 4, -3);
        assert_eq!(text(&cells), "dd......");
        let mut cells = grid("aabbccdd");
        shift_rows(&mut cells, 2, 4, 4);
        assert_eq!(text(&cells), "........");
    }

    #[test]
    fn grid_apply_shift_moves_cells_and_clusters() {
        let row = |r: u16, ch: char| FrameRow {
            row: r,
            cells: vec![
                Cell {
                    ch,
                    ..Default::default()
                };
                2
            ],
            clusters: vec![],
        };
        let mut g = Grid::default();
        let mut lines = vec![row(0, 'a'), row(1, 'b'), row(2, 'c')];
        lines[1].clusters = vec![(0, "e\u{301}".into())];
        g.apply(Frame {
            cols: 2,
            rows: 3,
            full: true,
            lines,
            ..Default::default()
        });
        g.info.dirty_rows = 0;
        g.apply(Frame {
            cols: 2,
            rows: 3,
            shift: 1,
            lines: vec![row(0, 'z')],
            ..Default::default()
        });
        let col0: String = (0..3)
            .map(|r| char::from_u32(g.cells[r * 2].ch).unwrap())
            .collect();
        assert_eq!(col0, "zab");
        assert!(g.clusters.contains_key(&(2, 0)) && !g.clusters.contains_key(&(1, 0)));
        assert_eq!(g.info.shift, 1);
        assert_eq!(g.info.dirty_rows, 0b001);
    }

    #[test]
    fn grid_apply_incremental() {
        let mut g = Grid::default();
        let mut f = Frame {
            pane: 1,
            cols: 3,
            rows: 2,
            full: true,
            ..Default::default()
        };
        f.lines = vec![
            FrameRow {
                row: 0,
                cells: vec![
                    Cell {
                        ch: 'a',
                        ..Default::default()
                    };
                    3
                ],
                clusters: vec![],
            },
            FrameRow {
                row: 1,
                cells: vec![
                    Cell {
                        ch: 'b',
                        ..Default::default()
                    };
                    3
                ],
                clusters: vec![(0, "e\u{301}".into())],
            },
        ];
        f.links = vec!["https://x".into()];
        g.apply(f);
        assert_eq!(g.cells[0].ch, 'a' as u32);
        assert_eq!(g.cells[3].ch, 'b' as u32);
        assert_eq!(g.info.dirty_rows, u64::MAX);
        assert_eq!(g.links.len(), 1);
        g.info.dirty_rows = 0;
        let f2 = Frame {
            pane: 1,
            cols: 3,
            rows: 2,
            lines: vec![FrameRow {
                row: 1,
                cells: vec![
                    Cell {
                        ch: 'c',
                        ..Default::default()
                    };
                    3
                ],
                clusters: vec![],
            }],
            ..Default::default()
        };
        g.apply(f2);
        assert_eq!(g.cells[0].ch, 'a' as u32);
        assert_eq!(g.cells[3].ch, 'c' as u32);
        assert_eq!(g.info.dirty_rows, 0b10);
        assert!(g.clusters.is_empty());
        assert_eq!(g.links.len(), 1, "empty links = unchanged");
        assert_eq!(g.info.generation, 2);
    }

    #[test]
    fn key_codes_match_header() {
        assert_eq!(named_key(1), Some(NamedKey::Escape));
        assert_eq!(named_key(48), Some(NamedKey::VolumeMute));
        assert_eq!(named_key(100), Some(NamedKey::F(1)));
        assert_eq!(named_key(134), Some(NamedKey::F(35)));
        assert_eq!(named_key(99), None);
        // The header's enum must agree with these numbers.
        let header = include_str!("../include/thurm.h");
        assert!(header.contains("THURM_KEY_F1 = 100"));
        assert!(header.contains("THURM_KEY_VOLUME_MUTE,\n    THURM_KEY_F1"));
    }

    #[test]
    fn request_json_shapes() {
        // Shapes documented in the header must parse.
        for s in [
            r#""ListPanes""#,
            r#"{"ClosePane":{"pane":3}}"#,
            r#"{"Scroll":{"pane":1,"scroll":"PageUp"}}"#,
            r#"{"Scroll":{"pane":1,"scroll":{"Lines":3}}}"#,
            r#"{"Selection":{"pane":1,"op":{"Start":{"col":0,"row":0,"right_half":false,"kind":"Simple"}}}}"#,
            r#"{"Search":{"pane":1,"query":"error","direction":"Backward"}}"#,
            r#"{"CreatePane":{"command":null,"cwd":"/tmp","env":[],"size":{"cols":80,"rows":24,"cell_width":8,"cell_height":16},"agent_preset":null,"inherit_cwd_from":null,"hold":false}}"#,
            r#"{"SetLayout":{"json":"{}"}}"#,
            r#"{"Hello":{"client":"Thurm.app","version":1,"ui":true}}"#,
            r#"{"ClipboardReply":{"pane":1,"text":"x"}}"#,
            r#"{"Shutdown":{"kill_panes":true}}"#,
            r#"{"SetAppearance":{"dark":false}}"#,
            r#"{"SetTheme":{"spec":"light:catppuccin-latte,dark:catppuccin-mocha"}}"#,
        ] {
            serde_json::from_str::<Request>(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        }
        let ev = serde_json::to_string(&Event::ConfigReloaded).unwrap();
        assert_eq!(ev, r#""ConfigReloaded""#);
        let ev = serde_json::to_string(&Event::ClipboardQuery {
            pane: 1,
            id: 7,
            mime: "text/plain".into(),
        })
        .unwrap();
        assert_eq!(
            ev, r#"{"ClipboardQuery":{"pane":1,"id":7,"mime":"text/plain"}}"#,
            "as the header documents it"
        );
        assert_eq!(
            serde_json::to_string(&Event::ClipboardWrite {
                pane: 1,
                id: 7,
                text: None
            })
            .unwrap(),
            r#"{"ClipboardWrite":{"pane":1,"id":7,"text":null}}"#
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    enum SubOp {
        Sub(PaneId),
        Unsub(PaneId),
    }

    struct MockCallbacks {
        frames: Mutex<Vec<PaneId>>,
        disconnected: std::sync::atomic::AtomicBool,
    }

    struct MockDaemon {
        client: *mut thurm_client,
        ops: Arc<Mutex<Vec<SubOp>>>,
        server_writer: Arc<Mutex<std::os::unix::net::UnixStream>>,
        callbacks: Arc<MockCallbacks>,
        ctx_ptr: *mut MockCallbacks,
    }

    impl MockDaemon {
        fn start(auto_attach: bool) -> Self {
            use std::io::BufReader;
            use std::os::unix::net::UnixStream;
            use thurm_proto::{Envelope, ServerMessage, codec};

            let (client_stream, server_stream) = UnixStream::pair().unwrap();
            let ops = Arc::new(Mutex::new(Vec::<SubOp>::new()));
            let log = ops.clone();
            let server_writer = Arc::new(Mutex::new(server_stream.try_clone().unwrap()));
            let resp_writer = server_writer.clone();

            std::thread::spawn(move || {
                let mut r = BufReader::new(server_stream);
                while let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut r) {
                    let mut w = resp_writer.lock();
                    let resp = match env.request {
                        Request::Hello { .. } => Some(Response::Hello {
                            version: thurm_proto::PROTOCOL_VERSION,
                            daemon_pid: 1,
                            restored: false,
                            build: String::new(),
                            capabilities: thurm_proto::CAPABILITIES
                                .iter()
                                .map(|c| (*c).to_owned())
                                .collect(),
                        }),
                        Request::Subscribe { pane } => {
                            log.lock().push(SubOp::Sub(pane));
                            if auto_attach {
                                let attach = Event::Attach {
                                    pane,
                                    size: PaneSize {
                                        cols: 40,
                                        rows: 10,
                                        cell_width: 8,
                                        cell_height: 16,
                                    },
                                    state: format!("pane-{pane}").into_bytes(),
                                };
                                let _ =
                                    codec::write_message(&mut *w, &ServerMessage::Event(attach));
                            }
                            None
                        }
                        Request::Unsubscribe { pane } => {
                            log.lock().push(SubOp::Unsub(pane));
                            None
                        }
                        Request::ListPanes => Some(Response::Panes(vec![])),
                        _ => (env.id != 0).then_some(Response::Ok),
                    };
                    if let Some(result) = resp {
                        let _ = codec::write_message(
                            &mut *w,
                            &ServerMessage::Response {
                                id: env.id,
                                result: Ok(result),
                            },
                        );
                    }
                }
            });

            let callbacks = Arc::new(MockCallbacks {
                frames: Mutex::new(Vec::new()),
                disconnected: std::sync::atomic::AtomicBool::new(false),
            });
            let ctx_ptr = Arc::into_raw(callbacks.clone()) as *mut MockCallbacks;
            unsafe extern "C" fn on_event(ctx: *mut c_void, json: *const c_char) {
                let cbs = unsafe { &*(ctx as *const MockCallbacks) };
                if let Ok(s) = unsafe { CStr::from_ptr(json) }.to_str()
                    && s == "\"Disconnected\""
                {
                    cbs.disconnected.store(true, Ordering::Release);
                }
            }
            unsafe extern "C" fn on_frame(ctx: *mut c_void, pane: u64) {
                let cbs = unsafe { &*(ctx as *const MockCallbacks) };
                cbs.frames.lock().push(pane);
            }
            let client = new_connection(
                Some(on_event),
                Some(on_frame),
                ctx_ptr.cast(),
                |on_ev, on_disc| {
                    Client::from_stream(client_stream, "test", true, &[], on_ev, on_disc)
                        .map(|c| (c, None))
                        .map_err(|e| e.to_string())
                },
            )
            .unwrap();

            Self {
                client,
                ops,
                server_writer,
                callbacks,
                ctx_ptr,
            }
        }

        /// Round-trips a synchronous request so all prior async `send()`s and server-side
        /// events written before it have been processed in FIFO order.
        fn sync(&self) {
            let out = unsafe { thurm_request(self.client, c"\"ListPanes\"".as_ptr()) };
            assert!(!out.is_null());
            unsafe { thurm_string_free(out) };
        }

        fn push(&self, ev: Event) {
            use thurm_proto::{ServerMessage, codec};
            let mut w = self.server_writer.lock();
            codec::write_message(&mut *w, &ServerMessage::Event(ev)).unwrap();
        }

        fn close_server(&self) {
            let _ = self.server_writer.lock().shutdown(std::net::Shutdown::Both);
        }

        fn wait_disconnected(&self) {
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            while Instant::now() < deadline {
                if self.callbacks.disconnected.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("timed out waiting for disconnect callback");
        }

        fn take_ops(&self) -> Vec<SubOp> {
            self.sync();
            std::mem::take(&mut *self.ops.lock())
        }

        fn take_frames(&self) -> Vec<PaneId> {
            std::mem::take(&mut *self.callbacks.frames.lock())
        }

        fn read_row0(&self, pane: PaneId) -> Option<(u16, u16, String)> {
            let mut info = thurm_grid_info::default();
            let mut cells: *const thurm_cell = std::ptr::null();
            if !unsafe { thurm_grid_lock(self.client, pane, &mut info, &mut cells) } {
                return None;
            }
            let row0: String = (0..info.cols as usize)
                .map(|c| {
                    let ch = unsafe { (*cells.add(c)).ch };
                    char::from_u32(ch).filter(|&c| c != '\0').unwrap_or(' ')
                })
                .collect::<String>()
                .trim_end()
                .to_owned();
            unsafe { thurm_grid_unlock(self.client, pane) };
            Some((info.cols, info.rows, row0))
        }
    }

    impl Drop for MockDaemon {
        fn drop(&mut self) {
            unsafe { thurm_disconnect(self.client) };
            self.wait_disconnected();
            unsafe { drop(Arc::from_raw(self.ctx_ptr)) };
        }
    }

    #[test]
    fn default_warm_limit_zero_preserves_legacy_unsubscribe() {
        let d = MockDaemon::start(true);
        unsafe { thurm_subscribe(d.client, 1) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(1)]);
        assert_eq!(d.take_frames(), vec![1]);
        assert!(
            unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(1)
                .is_some()
        );

        unsafe { thurm_unsubscribe(d.client, 1) };
        assert_eq!(d.take_ops(), vec![SubOp::Unsub(1)]);
        assert!(
            unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(1)
                .is_none()
        );

        // Re-subscribing sends Request::Subscribe again.
        unsafe { thurm_subscribe(d.client, 1) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(1)]);
        assert_eq!(d.take_frames(), vec![1]);
    }

    #[test]
    fn warm_hit_skips_subscribe_and_fires_frame_immediately() {
        let d = MockDaemon::start(true);
        unsafe { thurm_set_warm_limit(d.client, 4) };

        unsafe { thurm_subscribe(d.client, 7) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(7)]);
        assert_eq!(d.take_frames(), vec![7]);

        // Unsubscribing parks pane 7 warm without sending Request::Unsubscribe.
        unsafe { thurm_unsubscribe(d.client, 7) };
        assert_eq!(d.take_ops(), vec![]);
        assert!(
            unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(7)
                .is_some()
        );

        // Re-subscribing hits the warm cache: fires on_frame synchronously and sends nothing.
        unsafe { thurm_subscribe(d.client, 7) };
        assert_eq!(d.take_frames(), vec![7]);
        assert_eq!(d.take_ops(), vec![]);
        assert_eq!(d.read_row0(7), Some((40, 10, "pane-7".into())));
    }

    #[test]
    fn parked_warm_pane_applies_live_output_and_resize() {
        let d = MockDaemon::start(true);
        unsafe { thurm_set_warm_limit(d.client, 4) };

        unsafe { thurm_subscribe(d.client, 1) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(1)]);
        assert_eq!(d.take_frames(), vec![1]);

        // Park pane 1 warm.
        unsafe { thurm_unsubscribe(d.client, 1) };
        assert_eq!(d.take_ops(), vec![]);

        // Push resize and live output while parked; warm pane should update silently.
        d.push(Event::Resized {
            pane: 1,
            size: PaneSize {
                cols: 60,
                rows: 12,
                cell_width: 8,
                cell_height: 16,
            },
        });
        d.push(Event::Output {
            pane: 1,
            data: b"\r\x1b[2Klive-while-warm".to_vec(),
        });
        // Wait for any short resize hold timer to flush so snapshot reflects the resized grid.
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while Instant::now() < deadline {
            d.sync();
            let in_sync = unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(1)
                .unwrap()
                .lock()
                .term
                .in_sync();
            if !in_sync {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            d.take_frames().is_empty(),
            "parked warm pane should not spam on_frame"
        );

        // Re-subscribing immediately fires on_frame and shows the live-updated content & size.
        unsafe { thurm_subscribe(d.client, 1) };
        assert_eq!(d.take_frames(), vec![1]);
        assert_eq!(d.take_ops(), vec![]);
        assert_eq!(d.read_row0(1), Some((60, 12, "live-while-warm".into())));
    }

    #[test]
    fn warm_lru_evicts_oldest_and_limit_reduction_evicts_excess() {
        let d = MockDaemon::start(true);
        unsafe { thurm_set_warm_limit(d.client, 2) };

        for p in [1, 2] {
            unsafe { thurm_subscribe(d.client, p) };
            d.sync();
            unsafe { thurm_unsubscribe(d.client, p) };
        }
        assert_eq!(d.take_ops(), vec![SubOp::Sub(1), SubOp::Sub(2)]);

        // Touch pane 1 so pane 2 becomes the oldest warm entry.
        unsafe { thurm_subscribe(d.client, 1) };
        unsafe { thurm_unsubscribe(d.client, 1) };
        assert_eq!(d.take_ops(), vec![]);

        // Parking pane 3 exceeds limit 2 -> pane 2 (oldest) is evicted.
        unsafe { thurm_subscribe(d.client, 3) };
        d.sync();
        unsafe { thurm_unsubscribe(d.client, 3) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(3), SubOp::Unsub(2)]);
        let shared = &unsafe { client_ref(d.client) }.unwrap().shared;
        assert!(shared.local(1).is_some());
        assert!(shared.local(2).is_none());
        assert!(shared.local(3).is_some());

        // Lowering limit to 1 immediately evicts oldest remaining warm pane (1).
        unsafe { thurm_set_warm_limit(d.client, 1) };
        assert_eq!(d.take_ops(), vec![SubOp::Unsub(1)]);
        assert!(shared.local(1).is_none());
        assert!(shared.local(3).is_some());

        // Lowering limit to 0 evicts the last warm pane (3).
        unsafe { thurm_set_warm_limit(d.client, 0) };
        assert_eq!(d.take_ops(), vec![SubOp::Unsub(3)]);
        assert!(shared.local(3).is_none());
    }

    #[test]
    fn pane_closed_and_evict_pane_clean_up_immediately() {
        let d = MockDaemon::start(true);
        unsafe { thurm_set_warm_limit(d.client, 2) };

        for p in [1, 2] {
            unsafe { thurm_subscribe(d.client, p) };
            d.sync();
            unsafe { thurm_unsubscribe(d.client, p) };
        }
        let _ = d.take_ops();

        // PaneClosed for warm pane 1 frees its warm slot without sending Unsubscribe.
        d.push(Event::PaneClosed { pane: 1 });
        assert_eq!(d.take_ops(), vec![]);
        assert!(
            unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(1)
                .is_none()
        );

        // Parking pane 3 now fits in the freed slot without evicting pane 2.
        unsafe { thurm_subscribe(d.client, 3) };
        d.sync();
        unsafe { thurm_unsubscribe(d.client, 3) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(3)]);

        // thurm_evict_pane drops parked pane 2 immediately and sends Unsubscribe.
        unsafe { thurm_evict_pane(d.client, 2) };
        assert_eq!(d.take_ops(), vec![SubOp::Unsub(2)]);
        assert!(
            unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(2)
                .is_none()
        );

        // thurm_evict_pane on an active pane also drops it immediately and unsubscribes.
        unsafe { thurm_subscribe(d.client, 4) };
        d.sync();
        let _ = d.take_ops();
        unsafe { thurm_evict_pane(d.client, 4) };
        assert_eq!(d.take_ops(), vec![SubOp::Unsub(4)]);
        assert!(
            unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(4)
                .is_none()
        );
        // Calling evict again is a no-op (no duplicate Unsubscribe).
        unsafe { thurm_evict_pane(d.client, 4) };
        assert_eq!(d.take_ops(), vec![]);
    }

    #[test]
    fn prewarm_pane_subscribes_silently_and_respects_lru_limit() {
        let d = MockDaemon::start(true);
        // With warm_limit == 0, prewarm is a no-op.
        unsafe { thurm_prewarm_pane(d.client, 10) };
        assert_eq!(d.take_ops(), vec![]);

        unsafe { thurm_set_warm_limit(d.client, 2) };
        unsafe { thurm_prewarm_pane(d.client, 10) };
        unsafe { thurm_prewarm_pane(d.client, 11) };
        // Prewarming an already-warm pane is a no-op.
        unsafe { thurm_prewarm_pane(d.client, 10) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(10), SubOp::Sub(11)]);
        // Prewarmed Attach did not spuriously fire on_frame.
        assert!(d.take_frames().is_empty());

        // Prewarming a 3rd pane evicts the oldest warm pane (10).
        unsafe { thurm_prewarm_pane(d.client, 12) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(12), SubOp::Unsub(10)]);

        // Subscribing to prewarmed pane 11 is an instant warm hit with no Subscribe sent.
        unsafe { thurm_subscribe(d.client, 11) };
        assert_eq!(d.take_frames(), vec![11]);
        assert_eq!(d.take_ops(), vec![]);
        assert_eq!(d.read_row0(11), Some((40, 10, "pane-11".into())));

        // Prewarming an already-active pane is a no-op.
        unsafe { thurm_prewarm_pane(d.client, 11) };
        assert_eq!(d.take_ops(), vec![]);
    }

    #[test]
    fn subscribe_refcount_handles_overlapping_views() {
        let d = MockDaemon::start(true);
        // Works with warm_limit == 0 too: old view unsubscribing must not tear down new view.
        unsafe { thurm_subscribe(d.client, 5) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(5)]);
        assert_eq!(d.take_frames(), vec![5]);

        // Second view subscribes before first view unsubscribes.
        unsafe { thurm_subscribe(d.client, 5) };
        assert_eq!(d.take_frames(), vec![5]);
        assert_eq!(d.take_ops(), vec![]);

        // First view unsubscribes -> refcount 2 -> 1: stays subscribed & local stays alive.
        unsafe { thurm_unsubscribe(d.client, 5) };
        assert_eq!(d.take_ops(), vec![]);
        assert!(
            unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(5)
                .is_some()
        );

        // Second view unsubscribes -> refcount 1 -> 0: unsubscribes now (warm_limit == 0).
        unsafe { thurm_unsubscribe(d.client, 5) };
        assert_eq!(d.take_ops(), vec![SubOp::Unsub(5)]);
        assert!(
            unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(5)
                .is_none()
        );
    }

    #[test]
    fn late_attach_after_evict_does_not_leak_local_pane() {
        let d = MockDaemon::start(false);
        unsafe { thurm_subscribe(d.client, 9) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(9)]);

        // Evict before Event::Attach arrives.
        unsafe { thurm_evict_pane(d.client, 9) };
        assert_eq!(d.take_ops(), vec![SubOp::Unsub(9)]);

        // Late Event::Attach arrives from the cancelled Subscribe -> must be discarded.
        d.push(Event::Attach {
            pane: 9,
            size: PaneSize {
                cols: 40,
                rows: 10,
                cell_width: 8,
                cell_height: 16,
            },
            state: b"stale".to_vec(),
        });
        d.sync();
        assert!(
            unsafe { client_ref(d.client) }
                .unwrap()
                .shared
                .local(9)
                .is_none()
        );
        assert!(d.take_frames().is_empty());
    }

    #[test]
    fn disconnect_clears_warm_and_active_state() {
        let d = MockDaemon::start(true);
        unsafe { thurm_set_warm_limit(d.client, 4) };
        unsafe { thurm_subscribe(d.client, 1) };
        unsafe { thurm_subscribe(d.client, 2) };
        d.sync();
        unsafe { thurm_unsubscribe(d.client, 2) };
        assert_eq!(d.take_ops(), vec![SubOp::Sub(1), SubOp::Sub(2)]);

        d.close_server();
        d.wait_disconnected();

        let shared = &unsafe { client_ref(d.client) }.unwrap().shared;
        let subs = shared.subs.lock();
        assert!(subs.active.is_empty());
        assert!(subs.warm_order.is_empty());
        assert!(subs.subscribed.is_empty());
        drop(subs);
        assert!(shared.locals.lock().is_empty());
    }

    fn start_stalled_ffi_client() -> (
        *mut thurm_client,
        Arc<MockCallbacks>,
        *mut MockCallbacks,
        std::sync::mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        use std::os::unix::net::UnixStream;
        use thurm_proto::{Envelope, ServerMessage, codec};

        let (client_stream, mut peer) = UnixStream::pair().unwrap();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            codec::write_message(
                &mut peer,
                &ServerMessage::Response {
                    id: hello.id,
                    result: Ok(Response::Hello {
                        version: thurm_proto::PROTOCOL_VERSION,
                        daemon_pid: 1,
                        restored: false,
                        build: String::new(),
                        capabilities: thurm_proto::CAPABILITIES
                            .iter()
                            .map(|c| (*c).to_owned())
                            .collect(),
                    }),
                },
            )
            .unwrap();
            // Stop reading so outgoing writes stall and exhaust the client's byte budget.
            let _ = stop_rx.recv();
        });

        let callbacks = Arc::new(MockCallbacks {
            frames: Mutex::new(Vec::new()),
            disconnected: std::sync::atomic::AtomicBool::new(false),
        });
        let ctx_ptr = Arc::into_raw(callbacks.clone()) as *mut MockCallbacks;
        unsafe extern "C" fn on_event(ctx: *mut c_void, json: *const c_char) {
            let cbs = unsafe { &*(ctx as *const MockCallbacks) };
            if let Ok(s) = unsafe { CStr::from_ptr(json) }.to_str()
                && s == "\"Disconnected\""
            {
                cbs.disconnected.store(true, Ordering::Release);
            }
        }
        unsafe extern "C" fn on_frame(ctx: *mut c_void, pane: u64) {
            let cbs = unsafe { &*(ctx as *const MockCallbacks) };
            cbs.frames.lock().push(pane);
        }
        let client = new_connection(
            Some(on_event),
            Some(on_frame),
            ctx_ptr.cast(),
            |on_ev, on_disc| {
                Client::from_stream(client_stream, "test", true, &[], on_ev, on_disc)
                    .map(|c| (c, None))
                    .map_err(|e| e.to_string())
            },
        )
        .unwrap();
        (client, callbacks, ctx_ptr, stop_tx, server)
    }

    fn wait_ffi_disconnected(callbacks: &MockCallbacks) {
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while !callbacks.disconnected.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for Disconnected"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn stalled_peer_queue_full_on_input_disconnects_instead_of_silently_dropping() {
        let (client, callbacks, ctx_ptr, stop_tx, server) = start_stalled_ffi_client();
        let inner = unsafe { client_ref(client) }.unwrap();
        inner.client.set_max_queued_bytes(256 * 1024);

        // First input fills the Unix socket buffer and exceeds the configured byte budget for
        // subsequent writes; the next keystroke must trigger a disconnect/resync rather than
        // being silently dropped on an apparently live connection.
        let filler = vec![b'a'; 2 * 1024 * 1024];
        unsafe { thurm_input(client, 1, filler.as_ptr(), filler.len()) };
        assert!(inner.client.is_alive());

        let key = b"x";
        unsafe { thurm_input(client, 1, key.as_ptr(), key.len()) };
        assert!(!inner.client.is_alive());
        wait_ffi_disconnected(&callbacks);

        unsafe { thurm_disconnect(client) };
        drop(unsafe { Arc::from_raw(ctx_ptr) });
        let _ = stop_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn stalled_peer_queue_full_does_not_mark_subscribe_or_prewarm_active() {
        let (client, callbacks, ctx_ptr, stop_tx, server) = start_stalled_ffi_client();
        let inner = unsafe { client_ref(client) }.unwrap();
        inner.client.set_max_queued_bytes(256 * 1024);
        unsafe { thurm_set_warm_limit(client, 4) };

        let filler = vec![b'a'; 2 * 1024 * 1024];
        unsafe { thurm_input(client, 1, filler.as_ptr(), filler.len()) };
        assert!(inner.client.is_alive());

        // Subscribe fails with QueueFull: must not record pane 42 as subscribed/active, and must
        // close the stalled connection so the UI reconnects and resyncs.
        unsafe { thurm_subscribe(client, 42) };
        assert!(!inner.client.is_alive());
        {
            let subs = inner.shared.subs.lock();
            assert!(!subs.subscribed.contains(&42));
            assert!(!subs.active.contains_key(&42));
        }

        // Prewarm on the failed/disconnected client must likewise not record pane 43.
        unsafe { thurm_prewarm_pane(client, 43) };
        {
            let subs = inner.shared.subs.lock();
            assert!(!subs.subscribed.contains(&43));
            assert!(!subs.is_warm(43));
        }
        wait_ffi_disconnected(&callbacks);

        unsafe { thurm_disconnect(client) };
        drop(unsafe { Arc::from_raw(ctx_ptr) });
        let _ = stop_tx.send(());
        server.join().unwrap();
    }

    #[test]
    fn grid_apply_full_frame_resets_shift() {
        let mut g = Grid::default();
        let mk_row = |r: u16, ch: char| FrameRow {
            row: r,
            cells: vec![
                Cell {
                    ch,
                    ..Default::default()
                };
                2
            ],
            clusters: vec![],
        };
        g.apply(Frame {
            cols: 2,
            rows: 3,
            full: true,
            lines: vec![mk_row(0, 'a'), mk_row(1, 'b'), mk_row(2, 'c')],
            ..Default::default()
        });
        g.apply(Frame {
            cols: 2,
            rows: 3,
            shift: 2,
            lines: vec![mk_row(0, 'x')],
            ..Default::default()
        });
        assert_eq!(g.info.shift, 2);
        g.apply(Frame {
            cols: 2,
            rows: 3,
            full: true,
            shift: 5,
            lines: vec![mk_row(0, 'q'), mk_row(1, 'w'), mk_row(2, 'e')],
            ..Default::default()
        });
        assert_eq!(g.info.shift, 0, "full frame must reset shift to 0");
        assert_eq!(g.info.dirty_rows, u64::MAX);
    }

    #[test]
    fn grid_lock_for_multi_reader_preserves_dirty_rows() {
        let d = MockDaemon::start(true);
        unsafe { thurm_subscribe(d.client, 1) };
        d.sync();

        let lock_for = |last_gen: u64| -> thurm_grid_info {
            let mut info = thurm_grid_info::default();
            let mut cells: *const thurm_cell = std::ptr::null();
            assert!(unsafe { thurm_grid_lock_for(d.client, 1, last_gen, &mut info, &mut cells) });
            unsafe { thurm_grid_unlock(d.client, 1) };
            info
        };

        // Reader A locks first -> gen 1, all rows dirty.
        let a1 = lock_for(0);
        assert_eq!(a1.generation, 1);
        assert_eq!(a1.dirty_rows, u64::MAX);

        // Reader B locks first after Reader A consumed gen 1 -> must still see all rows dirty!
        let b1 = lock_for(0);
        assert_eq!(b1.generation, 1);
        assert_eq!(b1.dirty_rows, u64::MAX);
        assert_eq!(b1.shift, 0);

        // Incremental update arrives (row 1 changes).
        d.push(Event::Output {
            pane: 1,
            data: b"\r\nsecond-line".to_vec(),
        });
        d.sync();

        // Reader A locks with last_gen = 1 -> incremental dirty bits (not u64::MAX).
        let a2 = lock_for(a1.generation);
        assert_eq!(a2.generation, 2);
        assert_ne!(a2.dirty_rows, 0);
        assert_ne!(a2.dirty_rows, u64::MAX);

        // Reader B locks with last_gen = 1 AFTER Reader A already consumed gen 2's dirty bits.
        let b2 = lock_for(b1.generation);
        assert_eq!(b2.generation, 2);
        assert_eq!(b2.dirty_rows, u64::MAX);
        assert_eq!(b2.shift, 0);

        // Reader A locks again with no new frames -> generation unchanged, dirty_rows == 0.
        let a3 = lock_for(a2.generation);
        assert_eq!(a3.generation, 2);
        assert_eq!(a3.dirty_rows, 0);

        // Incremental update arrives (gen 3), leaving partial dirty bits unconsumed; a brand-new
        // first-time Reader C locking with last_gen = 0 MUST still get a full grid (u64::MAX).
        d.push(Event::Output {
            pane: 1,
            data: b"\r\nthird-line".to_vec(),
        });
        d.sync();
        let c1 = lock_for(0);
        assert_eq!(c1.generation, 3);
        assert_eq!(c1.dirty_rows, u64::MAX);
        assert_eq!(c1.shift, 0);

        // Metadata/cursor-only update (gen 4, no dirty rows): the first consecutive reader keeps
        // dirty_rows == 0 so it does not recopy the grid, while a second reader that missed or
        // had the mask consumed still gets a full grid.
        d.push(Event::Output {
            pane: 1,
            data: b"\x1b[?25l".to_vec(),
        });
        d.sync();
        let c2 = lock_for(c1.generation);
        assert_eq!(c2.generation, 4);
        assert_eq!(c2.dirty_rows, 0);
        assert_eq!(c2.shift, 0);
        assert_eq!(c2.cursor_shape, 4);

        let b3 = lock_for(b2.generation);
        assert_eq!(b3.generation, 4);
        assert_eq!(b3.dirty_rows, u64::MAX);
        assert_eq!(b3.shift, 0);

        // Second consecutive metadata-only update (gen 5): Reader B (at gen 4) gets dirty_rows == 0,
        // and Reader C (also at gen 4) locking AFTER Reader B consumed gen 5 gets u64::MAX.
        d.push(Event::Output {
            pane: 1,
            data: b"\x1b[?25h".to_vec(),
        });
        d.sync();
        let b4 = lock_for(b3.generation);
        assert_eq!(b4.generation, 5);
        assert_eq!(b4.dirty_rows, 0);
        assert_eq!(b4.shift, 0);
        assert_ne!(b4.cursor_shape, 4);

        let c3 = lock_for(c2.generation);
        assert_eq!(c3.generation, 5);
        assert_eq!(c3.dirty_rows, u64::MAX);
        assert_eq!(c3.shift, 0);
    }

    #[test]
    fn appearance_and_reload_fire_on_frame_for_active_panes() {
        let d = MockDaemon::start(true);
        unsafe { thurm_subscribe(d.client, 1) };
        d.sync();
        let _ = d.take_frames();

        unsafe { thurm_set_local_appearance(d.client, false) };
        assert_eq!(d.take_frames(), vec![1]);
        // Setting same appearance again is a no-op (no spurious frame).
        unsafe { thurm_set_local_appearance(d.client, false) };
        assert!(d.take_frames().is_empty());

        unsafe { thurm_reload_engine(d.client) };
        assert_eq!(d.take_frames(), vec![1]);
    }

    #[test]
    fn client_ctx_drop_runs_once_when_last_reference_drops() {
        use std::os::unix::net::UnixStream;
        use std::sync::atomic::AtomicUsize;
        use thurm_proto::{Envelope, ServerMessage, codec};

        let (client_stream, mut peer) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            codec::write_message(
                &mut peer,
                &ServerMessage::Response {
                    id: hello.id,
                    result: Ok(Response::Hello {
                        version: thurm_proto::PROTOCOL_VERSION,
                        daemon_pid: 1,
                        restored: false,
                        build: String::new(),
                        capabilities: thurm_proto::CAPABILITIES
                            .iter()
                            .map(|c| (*c).to_owned())
                            .collect(),
                    }),
                },
            )
            .unwrap();
            // Keep stream open until client disconnects.
            while codec::read_message::<_, Envelope>(&mut peer)
                .ok()
                .flatten()
                .is_some()
            {}
        });

        let drops = Arc::new(AtomicUsize::new(0));
        let raw = Arc::into_raw(drops.clone()) as *mut c_void;
        unsafe extern "C" fn on_drop(ctx: *mut c_void) {
            let counter = unsafe { Arc::from_raw(ctx as *const AtomicUsize) };
            counter.fetch_add(1, Ordering::SeqCst);
        }
        let client = new_connection(None, None, raw, |on_ev, on_disc| {
            Client::from_stream(client_stream, "test", true, &[], on_ev, on_disc)
                .map(|c| (c, None))
                .map_err(|e| e.to_string())
        })
        .unwrap();
        unsafe { thurm_client_set_ctx_drop(client, Some(on_drop)) };
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        unsafe { thurm_disconnect(client) };
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while drops.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "ctx_drop never ran");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        server.join().unwrap();
    }

    #[test]
    fn failed_connect_detaches_ctx_before_returning() {
        use std::sync::atomic::AtomicUsize;

        let calls = Arc::new(AtomicUsize::new(0));
        let raw = Arc::into_raw(calls.clone()) as *mut c_void;
        unsafe extern "C" fn on_ev(ctx: *mut c_void, _json: *const c_char) {
            let counter = unsafe { &*(ctx as *const AtomicUsize) };
            counter.fetch_add(1, Ordering::SeqCst);
        }

        let (tx, rx) = std::sync::mpsc::channel::<(Box<dyn Fn(Event) + Send>, Box<dyn FnOnce() + Send>)>();
        let res = new_connection(Some(on_ev), None, raw, |ev, disc| {
            tx.send((ev, disc)).unwrap();
            Err("hello failed".into())
        });
        assert!(res.is_err());

        // Caller can now treat `raw` as detached; even if a lingering reader thread fires
        // callbacks later, `on_ev` MUST NOT be invoked with `raw`.
        let (late_ev, late_disc) = rx.recv().unwrap();
        late_ev(Event::ConfigReloaded);
        late_disc();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        unsafe { drop(Arc::from_raw(raw as *const AtomicUsize)) };
    }

    #[test]
    fn notify_prompt_translates_to_notify_json_with_optional_request() {
        let shared = Arc::new(Shared::default());
        let events = Arc::new(Mutex::new(Vec::<String>::new()));
        let raw = Arc::into_raw(events.clone()) as *mut c_void;
        unsafe extern "C" fn on_ev(ctx: *mut c_void, json: *const c_char) {
            let log = unsafe { &*(ctx as *const Mutex<Vec<String>>) };
            let s = unsafe { CStr::from_ptr(json) }.to_string_lossy().into_owned();
            log.lock().push(s);
        }
        let ctx = CallbackCtx::new(raw);
        handle_event(
            &shared,
            Event::NotifyPrompt {
                pane: 4,
                title: "Claude Code".into(),
                body: "Run command".into(),
                permission: Some(9),
                sound: Some("Glass".into()),
                request: Some("Bash: git status".into()),
            },
            Some(on_ev),
            None,
            &ctx,
        );
        handle_event(
            &shared,
            Event::NotifyPrompt {
                pane: 5,
                title: "Claude Code".into(),
                body: "waiting".into(),
                permission: Some(10),
                sound: Some("Glass".into()),
                request: None,
            },
            Some(on_ev),
            None,
            &ctx,
        );
        let got = events.lock().clone();
        assert_eq!(got.len(), 2);
        let v0: serde_json::Value = serde_json::from_str(&got[0]).unwrap();
        assert_eq!(v0["Notify"]["pane"], 4);
        assert_eq!(v0["Notify"]["permission"], 9);
        assert_eq!(v0["Notify"]["request"], "Bash: git status");
        let v1: serde_json::Value = serde_json::from_str(&got[1]).unwrap();
        assert_eq!(v1["Notify"]["pane"], 5);
        assert!(v1["Notify"].get("request").is_none());
        drop(ctx);
        unsafe { drop(Arc::from_raw(raw as *const Mutex<Vec<String>>)) };
    }
}
