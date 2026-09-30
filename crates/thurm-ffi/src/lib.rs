//! C ABI for the Swift front end. See `include/thurm.h` for the contract.

#![allow(non_camel_case_types, clippy::missing_safety_doc)]

mod remote;

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use ::thurm_client::{Client, ConnectOptions};
use parking_lot::Mutex;
use thurm_proto::{
    Event, Frame, ImagePlacement, Key, KeyAction, KeyEvent, MouseButton, MouseEvent, MouseKind,
    NamedKey, PaneId, PaneSize, Request, Response,
};
use thurm_term::{ClientView, EngineConfig, MouseOutcome, TermEvent, Terminal};

pub type thurm_event_cb = Option<unsafe extern "C" fn(ctx: *mut c_void, json: *const c_char)>;
pub type thurm_frame_cb = Option<unsafe extern "C" fn(ctx: *mut c_void, pane: u64)>;

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
        if f.full || self.info.cols != f.cols || self.info.rows != f.rows {
            self.cells = vec![thurm_cell::default(); cols * rows];
            self.clusters.clear();
            self.info.dirty_rows = u64::MAX;
        }
        self.info.cols = f.cols;
        self.info.rows = f.rows;
        if f.shift != 0 && !f.full {
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

struct Shared {
    grids: Mutex<HashMap<PaneId, Arc<Mutex<Grid>>>>,
    locked: Mutex<HashMap<PaneId, Arc<Mutex<Grid>>>>,
    images: Mutex<HashMap<(PaneId, u32), Arc<ImageSlot>>>,
    locked_images: Mutex<HashMap<ImageKey, Vec<Arc<ImageSlot>>>>,
    locals: Mutex<HashMap<PaneId, Arc<Mutex<LocalPane>>>>,
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

    fn reload_engine(&self, reload_file: bool) {
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
    }
}

pub struct thurm_client {
    client: Arc<Client>,
    shared: Arc<Shared>,
    on_event: thurm_event_cb,
    on_frame: thurm_frame_cb,
    ctx: Ctx,
}

#[derive(Clone, Copy)]
struct Ctx(*mut c_void);
unsafe impl Send for Ctx {}
unsafe impl Sync for Ctx {}

fn emit_json(cb: thurm_event_cb, ctx: Ctx, json: &str) {
    if let (Some(cb), Ok(s)) = (cb, CString::new(json)) {
        unsafe { cb(ctx.0, s.as_ptr()) };
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
        connect_to(
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

unsafe fn connect_to(
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
    let shared = Arc::new(Shared::default());
    let ctx = Ctx(ctx);
    let conn_ctx = ctx;
    let events_shared = shared.clone();
    let on_ev = move |ev: Event| handle_event(&events_shared, ev, on_event, on_frame, ctx);
    let on_disc = move || emit_json(on_event, ctx, "\"Disconnected\"");
    match Client::connect(
        ConnectOptions {
            socket,
            spawn_daemon: daemon,
            client_name: &name,
            ui: true,
        },
        on_ev,
        on_disc,
    ) {
        Ok(client) => Box::into_raw(Box::new(thurm_client {
            client,
            shared,
            on_event,
            on_frame,
            ctx: conn_ctx,
        })),
        Err(e) => {
            if !err.is_null() {
                unsafe { *err = into_c(e.to_string()) };
            }
            std::ptr::null_mut()
        }
    }
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

/// Re-read this Mac's config into the connection's terminal copies (fonts aside, colors and
/// terminal behavior): remote daemons don't send ConfigReloaded when the Mac's config changes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_reload_engine(client: *mut thurm_client) {
    if let Some(c) = unsafe { client_ref(client) } {
        c.shared.reload_engine(true);
    }
}

fn fire_frame(on_frame: thurm_frame_cb, ctx: Ctx, pane: PaneId) {
    if let Some(cb) = on_frame {
        unsafe { cb(ctx.0, pane) };
    }
}

/// Inside a synchronized update the copy shows nothing new until it ends or times out; make
/// sure a frame is requested at the timeout even if no more output comes.
fn schedule_sync_flush(
    shared: &Arc<Shared>,
    pane: PaneId,
    deadline: Instant,
    on_frame: thurm_frame_cb,
    ctx: Ctx,
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
                        None
                    }
                    None => None,
                }
            };
            match next {
                Some(d) => schedule_sync_flush(&shared, pane, d, on_frame, ctx),
                None => fire_frame(on_frame, ctx, pane),
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
    ctx: Ctx,
) {
    match ev {
        Event::Attach { pane, size, state } => {
            let mut term = Terminal::new(size, shared.engine.lock().engine_config());
            // The daemon has read (and replaced) the pane's own transmissions; any left in the
            // stream (an older or hostile remote daemon) must not read this Mac's files.
            term.set_reads_media(false);
            term.replay(&state);
            let _ = term.drain_events();
            shared.images.lock().retain(|(p, _), _| *p != pane);
            let local = LocalPane {
                term,
                view: ClientView::new(),
            };
            shared
                .locals
                .lock()
                .insert(pane, Arc::new(Mutex::new(local)));
            fire_frame(on_frame, ctx, pane);
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
                Some(deadline) => schedule_sync_flush(shared, pane, deadline, on_frame, ctx),
                None => fire_frame(on_frame, ctx, pane),
            }
        }
        Event::Resized { pane, size } => {
            if let Some(local) = shared.local(pane) {
                let held = {
                    let mut l = local.lock();
                    l.term.resize(size);
                    l.term.sync_deadline()
                };
                // At a prompt the frame waits for the shell's redraw (see `Terminal::resize`).
                match held {
                    Some(deadline) => schedule_sync_flush(shared, pane, deadline, on_frame, ctx),
                    None => fire_frame(on_frame, ctx, pane),
                }
            }
        }
        other => {
            if let Event::PaneClosed { pane } = &other {
                shared.grids.lock().remove(pane);
                shared.locals.lock().remove(pane);
                shared.images.lock().retain(|(p, _), _| p != pane);
            }
            if matches!(other, Event::ConfigReloaded) {
                shared.reload_engine(true);
            }
            match serde_json::to_string(&other) {
                Ok(json) => emit_json(on_event, ctx, &json),
                Err(e) => log::warn!("cannot encode event: {e}"),
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
                let _ = c.client.send(req);
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
                let changed = {
                    let mut e = self.shared.engine.lock();
                    std::mem::replace(&mut e.dark, *dark) != *dark
                };
                if changed {
                    self.shared.reload_engine(false);
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
        fire_frame(self.on_frame, self.ctx, pane);
        Ok(resp)
    }

    /// Bytes the local copy encoded for the program (keys, mouse reports).
    fn input(&self, pane: PaneId, data: Vec<u8>) {
        if !data.is_empty() {
            let _ = self.client.send(Request::Input { pane, data });
        }
    }
}

fn send(client: *mut thurm_client, req: Request) {
    if let Some(c) = unsafe { client_ref(client) } {
        let _ = c.client.send(req);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_subscribe(client: *mut thurm_client, pane: u64) {
    send(client, Request::Subscribe { pane });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_unsubscribe(client: *mut thurm_client, pane: u64) {
    // Its copy stops getting output; input goes through the daemon until the next Attach.
    if let Some(c) = unsafe { client_ref(client) } {
        c.shared.locals.lock().remove(&pane);
    }
    send(client, Request::Unsubscribe { pane });
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
    send(client, Request::Input { pane, data });
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_paste(client: *mut thurm_client, pane: u64, text: *const c_char) {
    if let Some(t) = unsafe { opt_str(text) } {
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
            let bytes = local.lock().term.key(&key);
            c.input(pane, bytes);
        }
        None => {
            let _ = c.client.send(Request::Key { pane, key });
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
        let _ = c.client.send(Request::Mouse { pane, event });
        return;
    };
    let (text, bytes) = {
        let mut l = local.lock();
        let (outcome, bytes) = l.term.mouse(&event);
        let copy = outcome == MouseOutcome::SelectionDone
            && c.shared.engine.lock().cfg.terminal.copy_on_select;
        if outcome != MouseOutcome::None && outcome != MouseOutcome::Reported {
            fire_frame(c.on_frame, c.ctx, pane);
        }
        (copy.then(|| l.term.selection_text()), bytes)
    };
    c.input(pane, bytes);
    // The user's own selection (copy on select): no policy applies.
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        let json =
            serde_json::json!({"ClipboardStore": {"pane": pane, "text": text, "user": true}});
        emit_json(c.on_event, c.ctx, &json.to_string());
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
                fire_frame(c.on_frame, c.ctx, pane);
            }
            c.input(pane, bytes);
        }
        None => {
            let _ = c.client.send(Request::Wheel {
                pane,
                lines,
                col,
                row,
                mods,
            });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Grid access
// ---------------------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn thurm_grid_lock(
    client: *mut thurm_client,
    pane: u64,
    info: *mut thurm_grid_info,
    cells: *mut *const thurm_cell,
) -> bool {
    let Some(c) = (unsafe { client_ref(client) }) else {
        return false;
    };
    let slot = c.shared.grids.lock().entry(pane).or_default().clone();
    let mut guard = slot.lock();
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
    if !info.is_null() {
        unsafe { *info = guard.info };
    }
    if !cells.is_null() {
        unsafe { *cells = guard.cells.as_ptr() };
    }
    // The caller now has a consistent view; the next lock reports rows changed after this.
    guard.info.dirty_rows = 0;
    guard.info.shift = 0;
    std::mem::forget(guard);
    if let Some(prev) = c.shared.locked.lock().insert(pane, slot) {
        // Double lock without unlock: release the previous lock to avoid a deadlock.
        unsafe { prev.force_unlock() };
    }
    true
}

unsafe fn locked_grid<'a>(client: *mut thurm_client, pane: u64) -> Option<&'a Grid> {
    let c = unsafe { client_ref(client) }?;
    let locked = c.shared.locked.lock();
    let slot = locked.get(&pane)?;
    // Safety: the mutex is held by the caller (between grid_lock and grid_unlock) and the
    // Arc is kept alive in `locked` until unlock.
    Some(unsafe { &*slot.data_ptr() })
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
    if let Some(slot) = c.shared.locked.lock().remove(&pane) {
        unsafe { slot.force_unlock() };
    }
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
    c.shared.reload_engine(false);
    into_c(serde_json::to_string(&shown).unwrap_or_default())
}

#[unsafe(no_mangle)]
pub extern "C" fn thurm_config_json(dark: bool) -> *mut c_char {
    let _ = thurm_config::ensure_default_config();
    let json = match thurm_config::Config::load() {
        Ok(cfg) => cfg.ui_json(dark),
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

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_proto::{Cell, FrameRow};

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
    }
}
