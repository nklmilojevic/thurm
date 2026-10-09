//! Safe wrapper around the `thurm_*` C ABI of `thurm-ffi`: one connection per daemon (this
//! machine's, and each connected `[[remote]]` host's through its tunnel). Mirrors
//! macos/Sources/Thurm/Core.swift.
//!
//! Callbacks arrive on a Rust background thread; they hop to the GTK main loop with
//! `glib::idle_add_once` and reach the app through `crate::app::with_app`.

use std::cell::{Cell as StdCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_void};
use std::fmt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;

use gtk::glib;
use parking_lot::Mutex;
use serde_json::Value;
use thurm_ffi::{
    thurm_cell, thurm_client, thurm_cluster, thurm_grid_info, thurm_image_placement,
    thurm_key_event, thurm_mouse_event,
};
use thurm_proto::PaneId;

pub use thurm_ffi::{
    thurm_cell as Cell, thurm_grid_info as GridInfo, thurm_image_placement as ImagePlacement,
};

/// A daemon the app talks to: this machine's (`LOCAL`) or a `[[remote]]` host's, by name.
pub type HostId = String;

pub const LOCAL: &str = "local";

/// A pane is its daemon plus that daemon's id: daemons number their panes independently.
#[derive(Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct PaneKey {
    pub host: HostId,
    pub id: PaneId,
}

impl PaneKey {
    pub fn new(host: &str, id: PaneId) -> PaneKey {
        PaneKey {
            host: host.to_string(),
            id,
        }
    }

    pub fn local(id: PaneId) -> PaneKey {
        PaneKey::new(LOCAL, id)
    }

    pub fn is_remote(&self) -> bool {
        self.host != LOCAL
    }
}

impl fmt::Display for PaneKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_remote() {
            write!(f, "{}:{}", self.host, self.id)
        } else {
            write!(f, "{}", self.id)
        }
    }
}

/// Which connection a callback belongs to. Leaked on purpose: the library may still call back
/// with it after a disconnect (the app ignores stale epochs).
struct Token {
    host: HostId,
    epoch: u64,
}

/// Panes with a frame not drawn yet: one main-loop hop per batch, like Core.swift's
/// `markDirty`.
fn dirty() -> &'static Mutex<HashSet<(HostId, u64, PaneId)>> {
    static DIRTY: OnceLock<Mutex<HashSet<(HostId, u64, PaneId)>>> = OnceLock::new();
    DIRTY.get_or_init(Default::default)
}

unsafe extern "C" fn on_event(ctx: *mut c_void, json: *const c_char) {
    if json.is_null() || ctx.is_null() {
        return;
    }
    let token = unsafe { &*(ctx as *const Token) };
    let text = unsafe { CStr::from_ptr(json) }
        .to_string_lossy()
        .into_owned();
    let value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            log::warn!(
                "unparseable event: {}",
                text.chars().take(200).collect::<String>()
            );
            return;
        }
    };
    let (host, epoch) = (token.host.clone(), token.epoch);
    glib::idle_add_once(move || {
        crate::app::with_app(|app| app.handle_event(&host, epoch, value));
    });
}

unsafe extern "C" fn on_frame(ctx: *mut c_void, pane: u64) {
    if ctx.is_null() {
        return;
    }
    let token = unsafe { &*(ctx as *const Token) };
    // With the connection's epoch: a frame from a replaced connection is dropped, not drawn
    // from the new one's pane of the same id.
    let key = (token.host.clone(), token.epoch, pane);
    if dirty().lock().insert(key.clone()) {
        glib::idle_add_once(move || {
            dirty().lock().remove(&key);
            crate::app::with_app(|app| app.frame_arrived(&PaneKey::new(&key.0, key.2), key.1));
        });
    }
}

unsafe extern "C" fn on_remote_status(_ctx: *mut c_void, json: *const c_char) {
    if json.is_null() {
        return;
    }
    let text = unsafe { CStr::from_ptr(json) }
        .to_string_lossy()
        .into_owned();
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        glib::idle_add_once(move || {
            crate::app::with_app(|app| app.remote_status(value));
        });
    }
}

/// `thurmd` next to this executable (an installed or `cargo build` tree), else `$THURM_DAEMON`,
/// else on `$PATH`.
pub fn daemon_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("THURM_DAEMON").map(PathBuf::from)
        && p.is_file()
    {
        return Some(p);
    }
    if let Some(dir) = helpers_dir() {
        return Some(dir.join("thurmd"));
    }
    thurm_config::which("thurmd")
}

/// The directory holding this build's `thurm` and `thurmd` (to copy onto a remote host).
pub fn helpers_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    dir.join("thurmd").is_file().then(|| dir.to_path_buf())
}

/// Takes ownership of a `char *` returned by the library.
unsafe fn take_string(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { thurm_ffi::thurm_string_free(p) };
    Some(s)
}

unsafe fn borrowed(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

pub fn build_id() -> String {
    unsafe { CStr::from_ptr(thurm_ffi::thurm_build_id()) }
        .to_string_lossy()
        .into_owned()
}

pub fn protocol_version() -> u32 {
    thurm_ffi::thurm_protocol_version()
}

pub fn terminate_daemon() -> bool {
    thurm_ffi::thurm_terminate_daemon()
}

/// Replaces the running daemon with `path` in place (0: done, 1: too old, 2: failed).
pub fn upgrade_daemon(path: &Path) -> i32 {
    let p = cstring(&path.to_string_lossy());
    unsafe { thurm_ffi::thurm_upgrade_daemon(p.as_ptr()) }
}

/// One remote operation (`thurm_remote_call`). Operations that talk to a host block on ssh.
pub fn remote_call(req: &Value) -> Value {
    let json = cstring(&req.to_string());
    let raw = unsafe { thurm_ffi::thurm_remote_call(json.as_ptr()) };
    let text = unsafe { take_string(raw) }.unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// `remote_call` on a worker thread; `done` runs on the main loop.
pub fn remote_call_async(req: Value, done: impl FnOnce(Value) + 'static) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(remote_call(&req));
    });
    let done = RefCell::new(Some(done));
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        match rx.try_recv() {
            Ok(v) => {
                if let Some(f) = done.borrow_mut().take() {
                    f(v);
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(_) => glib::ControlFlow::Break,
        }
    });
}

pub fn remotes_start() {
    unsafe { thurm_ffi::thurm_remotes_start(Some(on_remote_status), ptr::null_mut()) };
}

pub fn remotes_sync() {
    thurm_ffi::thurm_remotes_sync();
}

pub fn remotes_stop() {
    thurm_ffi::thurm_remotes_stop();
}

pub fn remote_kick(name: Option<&str>, restart: bool) {
    let n = name.map(cstring);
    unsafe {
        thurm_ffi::thurm_remote_kick(n.as_ref().map_or(ptr::null(), |n| n.as_ptr()), restart)
    };
}

type Pending = Box<dyn FnOnce(Value)>;

/// A history line: its cells and grapheme clusters by column.
pub type PeekLine = (Vec<thurm_cell>, Vec<(u16, String)>);

thread_local! {
    static PENDING: RefCell<HashMap<usize, Pending>> = Default::default();
    static NEXT_REQUEST: StdCell<usize> = const { StdCell::new(1) };
}

unsafe extern "C" fn on_response(ctx: *mut c_void, json: *mut c_char) {
    let text = unsafe { take_string(json) }.unwrap_or_default();
    let id = ctx as usize;
    glib::idle_add_once(move || {
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let Some(f) = PENDING.with(|p| p.borrow_mut().remove(&id)) {
            f(value);
        }
    });
}

fn token(host: &str, epoch: u64) -> *mut c_void {
    Box::into_raw(Box::new(Token {
        host: host.to_string(),
        epoch,
    })) as *mut c_void
}

/// One connection to a daemon.
pub struct Core {
    client: *mut thurm_client,
    pub host: HostId,
}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe { thurm_ffi::thurm_disconnect(self.client) };
    }
}

impl Core {
    /// Connects to this machine's daemon, spawning `daemon` when none runs.
    pub fn connect(daemon: Option<&Path>, epoch: u64) -> Result<Core, String> {
        let path = daemon.map(|p| cstring(&p.to_string_lossy()));
        let name = cstring("thurm-gtk");
        let mut err: *mut c_char = ptr::null_mut();
        let client = unsafe {
            thurm_ffi::thurm_connect(
                path.as_ref().map_or(ptr::null(), |p| p.as_ptr()),
                name.as_ptr(),
                Some(on_event),
                Some(on_frame),
                token(LOCAL, epoch),
                &mut err,
            )
        };
        let message = unsafe { take_string(err) };
        if client.is_null() {
            return Err(message.unwrap_or_else(|| "cannot connect to thurmd".into()));
        }
        Ok(Core {
            client,
            host: LOCAL.into(),
        })
    }

    /// Connects to `host`'s daemon through its tunnel's local socket.
    pub fn connect_remote(host: &str, socket: &str, epoch: u64) -> Result<Core, String> {
        let sock = cstring(socket);
        let name = cstring("thurm-gtk");
        let mut err: *mut c_char = ptr::null_mut();
        let client = unsafe {
            thurm_ffi::thurm_connect_socket(
                sock.as_ptr(),
                ptr::null(),
                name.as_ptr(),
                Some(on_event),
                Some(on_frame),
                token(host, epoch),
                &mut err,
            )
        };
        let message = unsafe { take_string(err) };
        if client.is_null() {
            return Err(message.unwrap_or_else(|| "cannot connect".into()));
        }
        Ok(Core {
            client,
            host: host.to_string(),
        })
    }

    /// Blocking request (at most `timeout_ms`, 0: no limit); `{"error": …}` on failure.
    pub fn request_timeout(&self, req: &Value, timeout_ms: u64) -> Value {
        let json = cstring(&req.to_string());
        let raw =
            unsafe { thurm_ffi::thurm_request_timeout(self.client, json.as_ptr(), timeout_ms) };
        let text = unsafe { take_string(raw) }.unwrap_or_default();
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let Some(e) = value.get("error").and_then(Value::as_str) {
            log::warn!(
                "request to {} failed: {e} ({})",
                self.host,
                req.to_string().chars().take(120).collect::<String>()
            );
        }
        value
    }

    /// Blocking request; never waits more than 10 s on the main thread.
    pub fn request(&self, req: &Value) -> Value {
        self.request_timeout(req, 10_000)
    }

    /// Sends a request without blocking; `done` runs on the main loop with the response.
    pub fn request_async(&self, req: &Value, timeout_ms: u64, done: impl FnOnce(Value) + 'static) {
        let id = NEXT_REQUEST.with(|n| {
            let v = n.get();
            n.set(v + 1);
            v
        });
        PENDING.with(|p| p.borrow_mut().insert(id, Box::new(done)));
        let json = cstring(&req.to_string());
        unsafe {
            thurm_ffi::thurm_request_async(
                self.client,
                json.as_ptr(),
                timeout_ms,
                Some(on_response),
                id as *mut c_void,
            )
        };
    }

    pub fn send(&self, req: &Value) {
        let json = cstring(&req.to_string());
        unsafe { thurm_ffi::thurm_send(self.client, json.as_ptr()) };
    }

    pub fn subscribe(&self, pane: PaneId) {
        unsafe { thurm_ffi::thurm_subscribe(self.client, pane) };
    }

    pub fn unsubscribe(&self, pane: PaneId) {
        unsafe { thurm_ffi::thurm_unsubscribe(self.client, pane) };
    }

    pub fn input(&self, pane: PaneId, text: &str) {
        if text.is_empty() {
            return;
        }
        unsafe { thurm_ffi::thurm_input(self.client, pane, text.as_ptr(), text.len()) };
    }

    pub fn paste(&self, pane: PaneId, text: &str) {
        let t = cstring(text);
        unsafe { thurm_ffi::thurm_paste(self.client, pane, t.as_ptr()) };
    }

    pub fn resize(&self, pane: PaneId, cols: u16, rows: u16, cell_w: u16, cell_h: u16) {
        unsafe { thurm_ffi::thurm_resize(self.client, pane, cols, rows, cell_w, cell_h) };
    }

    pub fn focus(&self, pane: PaneId, focused: bool) {
        unsafe { thurm_ffi::thurm_focus(self.client, pane, focused) };
    }

    /// Re-reads this machine's config into the connection's terminal copies (remote ones).
    pub fn reload_engine(&self) {
        unsafe { thurm_ffi::thurm_reload_engine(self.client) };
    }

    /// Shows every pane in theme `name` without saving it (`None`: the configured theme).
    pub fn preview_theme(&self, name: Option<&str>) -> Option<Value> {
        let n = name.map(cstring);
        let raw = unsafe {
            thurm_ffi::thurm_preview_theme(
                self.client,
                n.as_ref().map_or(ptr::null(), |n| n.as_ptr()),
            )
        };
        let text = unsafe { take_string(raw) }?;
        serde_json::from_str(&text).ok()
    }

    /// Writes `data` to a new private file on the daemon's machine; returns its path there.
    pub fn write_temp_file(&self, name: &str, data: &[u8]) -> Result<String, String> {
        let n = cstring(name);
        let mut err: *mut c_char = ptr::null_mut();
        let raw = unsafe {
            thurm_ffi::thurm_write_temp_file(
                self.client,
                n.as_ptr(),
                data.as_ptr(),
                data.len(),
                &mut err,
            )
        };
        let message = unsafe { take_string(err) };
        unsafe { take_string(raw) }
            .ok_or_else(|| message.unwrap_or_else(|| "cannot write the file".into()))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn key(
        &self,
        pane: PaneId,
        kind: u32,
        code: u32,
        mods: u8,
        action: u8,
        text: Option<&str>,
        shifted: u32,
        base_layout: u32,
    ) {
        let text = text.filter(|t| !t.is_empty()).map(cstring);
        let ev = thurm_key_event {
            kind,
            code,
            mods,
            action,
            text: text.as_ref().map_or(ptr::null(), |t| t.as_ptr()),
            shifted,
            base_layout,
        };
        unsafe { thurm_ffi::thurm_key(self.client, pane, &ev) };
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mouse(
        &self,
        pane: PaneId,
        kind: u8,
        button: u8,
        mods: u8,
        clicks: u8,
        col: u16,
        row: u16,
        right_half: bool,
        x: u32,
        y: u32,
    ) {
        let ev = thurm_mouse_event {
            kind,
            button,
            mods,
            clicks: clicks.clamp(1, 3),
            col,
            row,
            right_half,
            x,
            y,
        };
        unsafe { thurm_ffi::thurm_mouse(self.client, pane, &ev) };
    }

    pub fn wheel(&self, pane: PaneId, lines: i32, col: u16, row: u16, mods: u8) {
        if lines != 0 {
            unsafe { thurm_ffi::thurm_wheel(self.client, pane, lines, col, row, mods) };
        }
    }

    /// Reads the pane's screen under the library's lock; `None` before its first frame.
    pub fn with_grid<R>(&self, pane: PaneId, f: impl FnOnce(&Grid<'_>) -> R) -> Option<R> {
        let mut info = thurm_grid_info::default();
        let mut cells: *const thurm_cell = ptr::null();
        if !unsafe { thurm_ffi::thurm_grid_lock(self.client, pane, &mut info, &mut cells) } {
            return None;
        }
        let n = info.cols as usize * info.rows as usize;
        let slice = if cells.is_null() || n == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(cells, n) }
        };
        let grid = Grid {
            core: self,
            pane,
            info,
            cells: slice,
        };
        let r = f(&grid);
        unsafe { thurm_ffi::thurm_grid_unlock(self.client, pane) };
        Some(r)
    }

    /// Identifies the pixels stored for `image` (changes when it is replaced); 0 if unknown.
    pub fn image_serial(&self, pane: PaneId, image: u32) -> u64 {
        unsafe { thurm_ffi::thurm_image_serial(self.client, pane, image) }
    }

    /// The image as cairo wants it: premultiplied ARGB32 in native byte order, with its size.
    /// Images larger than this on a side are not drawn (cairo's limit is 32767).
    pub const MAX_IMAGE_SIDE: u32 = 16384;

    /// `None` for one larger than [`Self::MAX_IMAGE_SIDE`], before anything is copied.
    pub fn image_argb(&self, pane: PaneId, image: u32) -> Option<(i32, i32, Vec<u8>)> {
        let (mut w, mut h) = (0u32, 0u32);
        let mut rgba: *const u8 = ptr::null();
        if !unsafe {
            thurm_ffi::thurm_image_lock(self.client, pane, image, &mut w, &mut h, &mut rgba)
        } {
            return None;
        }
        let fits =
            (1..=Self::MAX_IMAGE_SIDE).contains(&w) && (1..=Self::MAX_IMAGE_SIDE).contains(&h);
        let n = w as usize * h as usize * 4;
        let out = (fits && !rgba.is_null()).then(|| {
            let src = unsafe { std::slice::from_raw_parts(rgba, n) };
            let mut out = vec![0u8; n];
            for (d, s) in out
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(src.as_chunks::<4>().0)
            {
                let a = s[3] as u32;
                let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
                // Little-endian ARGB32: B, G, R, A.
                d[0] = pm(s[2]);
                d[1] = pm(s[1]);
                d[2] = pm(s[0]);
                d[3] = s[3];
            }
            (w as i32, h as i32, out)
        });
        unsafe { thurm_ffi::thurm_image_unlock(self.client, pane, image) };
        out
    }
}

/// A locked pane screen (see `Core::with_grid`).
pub struct Grid<'a> {
    core: &'a Core,
    pane: PaneId,
    pub info: thurm_grid_info,
    pub cells: &'a [thurm_cell],
}

impl Grid<'_> {
    /// Every multi-codepoint grapheme cluster, by (row, col).
    pub fn clusters(&self) -> Vec<(u16, u16, String)> {
        let n = unsafe {
            thurm_ffi::thurm_grid_clusters(self.core.client, self.pane, ptr::null_mut(), 0)
        };
        if n == 0 {
            return Vec::new();
        }
        let mut buf: Vec<thurm_cluster> = (0..n)
            .map(|_| thurm_cluster {
                row: 0,
                col: 0,
                text: ptr::null(),
            })
            .collect();
        let got = unsafe {
            thurm_ffi::thurm_grid_clusters(self.core.client, self.pane, buf.as_mut_ptr(), n)
        };
        buf.truncate(got.min(n));
        buf.into_iter()
            .filter_map(|c| {
                let s = unsafe { borrowed(c.text) }?;
                Some((c.row, c.col, s))
            })
            .collect()
    }

    /// Hyperlink URI of a cell's `link` field (1-based; 0 is none).
    pub fn link(&self, link: u16) -> Option<String> {
        if link == 0 {
            return None;
        }
        unsafe {
            borrowed(thurm_ffi::thurm_grid_link(
                self.core.client,
                self.pane,
                link - 1,
            ))
        }
    }

    pub fn images(&self) -> Vec<thurm_image_placement> {
        let n = self.info.image_count;
        if n == 0 {
            return Vec::new();
        }
        let mut out = vec![thurm_image_placement::default(); n];
        let got = unsafe {
            thurm_ffi::thurm_grid_images(self.core.client, self.pane, out.as_mut_ptr(), n)
        };
        out.truncate(got.min(n));
        out
    }

    /// The history line just above the viewport (smooth scrolling), with its clusters.
    pub fn peek(&self) -> Option<PeekLine> {
        let mut cells: *const thurm_cell = ptr::null();
        let n = unsafe { thurm_ffi::thurm_grid_peek(self.core.client, self.pane, &mut cells) };
        if n == 0 || cells.is_null() {
            return None;
        }
        let line = unsafe { std::slice::from_raw_parts(cells, n) }.to_vec();
        let clusters = (0..n as u16)
            .filter_map(|c| {
                let s = unsafe {
                    borrowed(thurm_ffi::thurm_grid_peek_cluster(
                        self.core.client,
                        self.pane,
                        c,
                    ))
                }?;
                Some((c, s))
            })
            .collect();
        Some((line, clusters))
    }
}
