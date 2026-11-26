//! Thin safe layer over libghostty-vt's C API: the parts of it Thurm uses.

use std::ffi::c_void;
use std::ptr;

use ghostty_vt_rs_sys as ffi;
// libghostty-vt has no kitty graphics where it can't read a clock (wasm32-freestanding): the
// calls go to stand-ins that store nothing.
#[cfg(not(target_arch = "wasm32"))]
use ghostty_vt_rs_sys as kitty_ffi;

pub use ffi::{ColorRgb, GridRef, Style, StyleColor};

/// Things libghostty-vt reports while it processes output (its "effects").
#[derive(Default)]
pub struct Effects {
    pub pty: Vec<Vec<u8>>,
    pub bell: bool,
    pub title_changed: bool,
    /// The working directory changed (OSC 7 / OSC 9;9 / OSC 1337 CurrentDir); see `Vt::pwd`.
    pub pwd_changed: bool,
    /// Text the program copied to the clipboard (OSC 52 / OSC 1337 Copy / OSC 5522).
    pub clipboard: Vec<String>,
    /// Whether programs may copy to the clipboard (Thurm's OSC 52 policy).
    pub copy_allowed: bool,
    /// Desktop notifications (OSC 9, OSC 777) as (title, body).
    pub notifications: Vec<(String, String)>,
    /// Progress reports (OSC 9;4): `None` clears.
    pub progress: Vec<Option<thurm_proto::Progress>>,
    /// Program status reports (OSC 7501), in stream order.
    pub programs: Vec<crate::program_status::Report>,
    /// Answer for color scheme queries (CSI ? 996 n): dark or light.
    pub dark: bool,
    /// Answer for XTWINOPS size queries.
    pub size: ffi::SizeReportSize,
}

/// A libghostty-vt terminal.
pub struct Vt {
    raw: ffi::Terminal,
    /// Heap-allocated so the userdata pointer libghostty-vt keeps stays valid when `Vt` moves.
    effects: Box<Effects>,
    /// Reused to list kitty graphics placements.
    placements: ffi::KittyGraphicsPlacementIterator,
    /// Mouse selection state (click count, anchor, dragging), and an event of each kind.
    gesture: ffi::SelectionGesture,
    press: ffi::SelectionGestureEvent,
    drag: ffi::SelectionGestureEvent,
    release: ffi::SelectionGestureEvent,
}

/// A selection: both ends as (column, row from the top of the scrollback), in the order the
/// user made them, and whether it is a rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sel {
    pub start: (u16, u32),
    pub end: (u16, u32),
    pub rectangle: bool,
}

pub use ffi::RenderStateDirty as RenderDirty;
pub use ffi::SelectionGestureBehavior as SelectBy;
pub use ffi::SelectionGestureGeometry as SelectGeometry;

/// Where the pointer is for a selection gesture: its cell (column, row from the top of the
/// scrollback) and pixel position in the grid.
#[derive(Clone, Copy, Debug)]
pub struct Pointer {
    pub cell: (u16, u32),
    pub px: (f64, f64),
}

/// A kitty graphics placement, and where it is drawn (`None` for virtual placements, shown
/// through Unicode placeholders, and placements off screen).
#[derive(Clone, Copy, Debug)]
pub struct KittyPlacement {
    pub image: u32,
    pub placement: u32,
    pub is_virtual: bool,
    pub x_offset: u32,
    pub y_offset: u32,
    /// Source rectangle as requested (zero width / height: the whole image).
    pub source: (u32, u32, u32, u32),
    /// Size in cells as requested (zero: from the image size).
    pub cols: u32,
    pub rows: u32,
    pub z: i32,
    pub render: Option<ffi::KittyGraphicsPlacementRenderInfo>,
}

// libghostty-vt terminals have no thread affinity; `Vt` is only ever used from one thread at a
// time (it is behind `&mut`).
unsafe impl Send for Vt {}

/// A position libghostty-vt keeps up to date as the screen scrolls, reflows or is pruned.
pub struct Tracked(ffi::TrackedGridRef);

unsafe impl Send for Tracked {}

impl Drop for Tracked {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_tracked_grid_ref_free(self.0) }
    }
}

impl Tracked {
    /// (column, row from the top of the scrollback), or `None` once the cell is gone.
    pub fn screen_point(&self) -> Option<(u16, u32)> {
        if !unsafe { ffi::ghostty_tracked_grid_ref_has_value(self.0) } {
            return None;
        }
        let mut out = ffi::PointCoordinate::default();
        let r =
            unsafe { ffi::ghostty_tracked_grid_ref_point(self.0, ffi::PointTag::SCREEN, &mut out) };
        (r == ffi::Result::SUCCESS).then_some((out.x, out.y))
    }
}

/// One cell's data, read through a grid reference.
#[derive(Clone, Copy, Default)]
pub struct RawCell {
    pub codepoint: u32,
    pub tag: ffi::CellContentTag::Type,
    pub wide: ffi::CellWide::Type,
    pub has_styling: bool,
    pub style_id: u16,
    pub has_hyperlink: bool,
    /// Background of a cell without text (erased with a colored background).
    pub bg_palette: u8,
    pub bg_rgb: ColorRgb,
}

pub const WIDE: ffi::CellWide::Type = ffi::CellWide::WIDE;
pub const SPACER_TAIL: ffi::CellWide::Type = ffi::CellWide::SPACER_TAIL;
pub const SPACER_HEAD: ffi::CellWide::Type = ffi::CellWide::SPACER_HEAD;
pub const TAG_GRAPHEME: ffi::CellContentTag::Type = ffi::CellContentTag::CODEPOINT_GRAPHEME;
pub const TAG_BG_PALETTE: ffi::CellContentTag::Type = ffi::CellContentTag::BG_COLOR_PALETTE;
pub const TAG_BG_RGB: ffi::CellContentTag::Type = ffi::CellContentTag::BG_COLOR_RGB;

/// `StyleColor::tag` values (the tag is a byte).
pub const COLOR_NONE: u8 = ffi::StyleColorTag::NONE as u8;
pub const COLOR_PALETTE: u8 = ffi::StyleColorTag::PALETTE as u8;
pub const COLOR_RGB: u8 = ffi::StyleColorTag::RGB as u8;

/// DEC private mode number (or ANSI with `ansi`), as libghostty-vt encodes it.
pub const fn mode(value: u16, ansi: bool) -> ffi::Mode {
    (value & 0x7fff) | ((ansi as u16) << 15)
}

pub fn default_style() -> Style {
    let mut s: Style = unsafe { std::mem::zeroed() };
    unsafe { ffi::ghostty_style_default(&mut s) };
    s
}

unsafe extern "C" fn on_write_pty(_: ffi::Terminal, ud: *mut c_void, data: *const u8, len: usize) {
    let fx = unsafe { &mut *(ud as *mut Effects) };
    if !data.is_null() && len > 0 {
        fx.pty
            .push(unsafe { std::slice::from_raw_parts(data, len) }.to_vec());
    }
}

unsafe extern "C" fn on_bell(_: ffi::Terminal, ud: *mut c_void) {
    unsafe { (*(ud as *mut Effects)).bell = true };
}

unsafe extern "C" fn on_title(_: ffi::Terminal, ud: *mut c_void) {
    unsafe { (*(ud as *mut Effects)).title_changed = true };
}

unsafe extern "C" fn on_color_scheme(
    _: ffi::Terminal,
    ud: *mut c_void,
    out: *mut ffi::ColorScheme::Type,
) -> bool {
    let dark = unsafe { (*(ud as *mut Effects)).dark };
    unsafe {
        *out = if dark {
            ffi::ColorScheme::DARK
        } else {
            ffi::ColorScheme::LIGHT
        }
    };
    true
}

unsafe extern "C" fn on_notification(
    _: ffi::Terminal,
    ud: *mut c_void,
    n: *const ffi::TerminalDesktopNotification,
) {
    let n = unsafe { &*n };
    let text = |s: &ffi::String| String::from_utf8_lossy(unsafe { ghostty_str(s) }).into_owned();
    let (title, body) = (text(&n.title), text(&n.body));
    unsafe { (*(ud as *mut Effects)).notifications.push((title, body)) };
}

unsafe extern "C" fn on_progress(
    _: ffi::Terminal,
    ud: *mut c_void,
    report: *const ffi::TerminalProgressReport,
) {
    use ffi::TerminalProgressState as P;
    use thurm_proto::{Progress, ProgressState as S};
    let r = unsafe { &*report };
    let percent = (r.progress >= 0).then(|| r.progress.min(100) as u8);
    let state = match r.state {
        P::REMOVE => None,
        P::ERROR => Some(S::Error),
        P::INDETERMINATE => Some(S::Indeterminate),
        P::PAUSE => Some(S::Paused),
        _ => Some(S::Normal),
    };
    let p = state.map(|state| Progress { state, percent });
    unsafe { (*(ud as *mut Effects)).progress.push(p) };
}

unsafe extern "C" fn on_program_status(
    _: ffi::Terminal,
    ud: *mut c_void,
    report: *const ffi::TerminalProgramStatus,
) {
    use crate::program_status::{Report, invisible_format};
    use ffi::{ProgramStatusKind as K, ProgramStatusState as St};
    use thurm_proto::{BlockedKind, ProgramRecord, ProgramState};
    let r = unsafe { &*report };
    let text = |s: &ffi::String| -> Option<String> {
        let s: String = String::from_utf8_lossy(unsafe { ghostty_str(s) })
            .chars()
            .filter(|c| !invisible_format(*c))
            .collect();
        (!s.is_empty()).then_some(s)
    };
    let state = match r.state {
        St::IDLE => ProgramState::Idle,
        St::WORKING => ProgramState::Working,
        St::DONE => ProgramState::Done,
        St::BLOCKED => ProgramState::Blocked,
        St::ERROR => ProgramState::Error,
        St::CLEAR => {
            let report = Report::Clear(text(&r.id));
            unsafe { (*(ud as *mut Effects)).programs.push(report) };
            return;
        }
        _ => return,
    };
    let kind = match r.kind {
        K::PERMISSION => Some(BlockedKind::Permission),
        K::QUESTION => Some(BlockedKind::Question),
        K::AUTH => Some(BlockedKind::Auth),
        _ => None,
    };
    let report = Report::Set(ProgramRecord {
        id: text(&r.id).unwrap_or_default(),
        state,
        kind,
        progress: u8::try_from(r.progress).ok().filter(|&p| p <= 100),
        app: text(&r.app),
        title: text(&r.title),
        message: text(&r.message),
        seen: false,
    });
    unsafe { (*(ud as *mut Effects)).programs.push(report) };
}

unsafe extern "C" fn on_pwd(_: ffi::Terminal, ud: *mut c_void) {
    unsafe { (*(ud as *mut Effects)).pwd_changed = true };
}

unsafe extern "C" fn on_clipboard(
    _: ffi::Terminal,
    ud: *mut c_void,
    write: *const ffi::ClipboardWrite,
) {
    let w = unsafe { &*write };
    let contents = if w.contents.is_null() {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(w.contents, w.contents_len) }
    };
    let text = contents.iter().find_map(|c| {
        let mime = unsafe { ghostty_str(&c.mime) };
        (mime == b"text/plain" || mime.starts_with(b"text/plain;"))
            .then(|| String::from_utf8_lossy(unsafe { ghostty_str(&c.data) }).into_owned())
    });
    let fx = unsafe { &mut *(ud as *mut Effects) };
    let result = match text {
        _ if !fx.copy_allowed || w.location != ffi::ClipboardLocation::STANDARD => {
            ffi::ClipboardWriteResult::DENIED
        }
        Some(text) => {
            fx.clipboard.push(text);
            ffi::ClipboardWriteResult::SUCCESS
        }
        None => ffi::ClipboardWriteResult::UNSUPPORTED,
    };
    let reply = ffi::ClipboardWriteReply {
        size: std::mem::size_of::<ffi::ClipboardWriteReply>(),
        result,
        remember: false,
    };
    if let Some(answer) = w.reply {
        unsafe { answer(write, &reply) };
    }
}

/// The bytes of a libghostty-vt string (borrowed for the call).
unsafe fn ghostty_str(s: &ffi::String) -> &[u8] {
    if s.ptr.is_null() || s.len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(s.ptr, s.len) }
    }
}

unsafe extern "C" fn on_size(
    _: ffi::Terminal,
    ud: *mut c_void,
    out: *mut ffi::SizeReportSize,
) -> bool {
    unsafe { *out = (*(ud as *mut Effects)).size };
    true
}

unsafe extern "C" fn on_device_attributes(
    _: ffi::Terminal,
    _: *mut c_void,
    out: *mut ffi::DeviceAttributes,
) -> bool {
    // A VT102 (`CSI ? 6 c`), and `CSI > 0 ; version ; 1 c`.
    let out = unsafe { &mut *out };
    out.primary.conformance_level = 6;
    out.primary.num_features = 0;
    out.secondary.device_type = 0;
    out.secondary.firmware_version = version_number(env!("CARGO_PKG_VERSION"));
    out.secondary.rom_cartridge = 1;
    out.tertiary.unit_id = 0;
    true
}

unsafe extern "C" fn on_xtversion(_: ffi::Terminal, _: *mut c_void) -> ffi::String {
    const VERSION: &str = concat!("Thurm ", env!("CARGO_PKG_VERSION"));
    ffi::String {
        ptr: VERSION.as_ptr(),
        len: VERSION.len(),
    }
}

fn version_number(v: &str) -> u16 {
    let mut it = v.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    let (a, b, c) = (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    );
    a.saturating_mul(10000)
        .saturating_add(b * 100)
        .saturating_add(c)
}

impl Vt {
    /// A terminal keeping at least `scrollback` lines of history.
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let mut raw: ffi::Terminal = ptr::null_mut();
        let r =
            unsafe { ffi::ghostty_terminal_new(ptr::null(), &mut raw, cols.max(1), rows.max(1)) };
        assert!(
            r == ffi::Result::SUCCESS && !raw.is_null(),
            "ghostty_terminal_new failed ({r})"
        );
        set_scrollback(raw, scrollback);
        unsafe {
            // No byte limit on top: the line limit alone decides. (NULL would restore the
            // default limit of 10,000 bytes.)
            let unlimited = usize::MAX;
            ffi::ghostty_terminal_set(
                raw,
                ffi::TerminalOption::SCROLLBACK_MAX_BYTES,
                (&unlimited as *const usize).cast(),
            );
        }
        install_png_decoder();
        let mut placements: ffi::KittyGraphicsPlacementIterator = ptr::null_mut();
        unsafe {
            kitty_ffi::ghostty_kitty_graphics_placement_iterator_new(ptr::null(), &mut placements)
        };
        let mut gesture: ffi::SelectionGesture = ptr::null_mut();
        let events = [
            ffi::SelectionGestureEventType::PRESS,
            ffi::SelectionGestureEventType::DRAG,
            ffi::SelectionGestureEventType::RELEASE,
        ]
        .map(|ty| {
            let mut ev: ffi::SelectionGestureEvent = ptr::null_mut();
            unsafe { ffi::ghostty_selection_gesture_event_new(ptr::null(), &mut ev, ty) };
            ev
        });
        unsafe { ffi::ghostty_selection_gesture_new(ptr::null(), &mut gesture) };
        let mut vt = Self {
            raw,
            effects: Box::default(),
            placements,
            gesture,
            press: events[0],
            drag: events[1],
            release: events[2],
        };
        let ud = &mut *vt.effects as *mut Effects as *const c_void;
        let write_pty: ffi::TerminalWritePtyFn = Some(on_write_pty);
        let bell: ffi::TerminalBellFn = Some(on_bell);
        let title: ffi::TerminalTitleChangedFn = Some(on_title);
        let size: ffi::TerminalSizeFn = Some(on_size);
        let da: ffi::TerminalDeviceAttributesFn = Some(on_device_attributes);
        let xtversion: ffi::TerminalXtversionFn = Some(on_xtversion);
        let pwd: ffi::TerminalPwdChangedFn = Some(on_pwd);
        let scheme: ffi::TerminalColorSchemeFn = Some(on_color_scheme);
        let notify: ffi::TerminalDesktopNotificationFn = Some(on_notification);
        let progress: ffi::TerminalProgressReportFn = Some(on_progress);
        let programs: ffi::TerminalProgramStatusFn = Some(on_program_status);
        let clipboard: ffi::TerminalClipboardWriteFn = Some(on_clipboard);
        unsafe {
            use ffi::TerminalOption as O;
            ffi::ghostty_terminal_set(raw, O::USERDATA, ud);
            ffi::ghostty_terminal_set(raw, O::WRITE_PTY, fn_ptr(write_pty));
            ffi::ghostty_terminal_set(raw, O::BELL, fn_ptr(bell));
            ffi::ghostty_terminal_set(raw, O::TITLE_CHANGED, fn_ptr(title));
            ffi::ghostty_terminal_set(raw, O::SIZE, fn_ptr(size));
            ffi::ghostty_terminal_set(raw, O::DEVICE_ATTRIBUTES, fn_ptr(da));
            ffi::ghostty_terminal_set(raw, O::XTVERSION, fn_ptr(xtversion));
            ffi::ghostty_terminal_set(raw, O::PWD_CHANGED, fn_ptr(pwd));
            ffi::ghostty_terminal_set(raw, O::COLOR_SCHEME, fn_ptr(scheme));
            ffi::ghostty_terminal_set(raw, O::DESKTOP_NOTIFICATION, fn_ptr(notify));
            ffi::ghostty_terminal_set(raw, O::PROGRESS_REPORT, fn_ptr(progress));
            ffi::ghostty_terminal_set(raw, O::PROGRAM_STATUS, fn_ptr(programs));
            ffi::ghostty_terminal_set(raw, O::CLIPBOARD_WRITE, fn_ptr(clipboard));
            // Kitty graphics start off (see `set_kitty_graphics`). Thurm reads files and shared
            // memory itself (see `kitty`), so libghostty-vt never gets to.
            let zero: u64 = 0;
            ffi::ghostty_terminal_set(
                raw,
                O::KITTY_IMAGE_STORAGE_LIMIT,
                (&zero as *const u64).cast(),
            );
            let off = false;
            for o in [O::KITTY_IMAGE_MEDIUM_FILE, O::KITTY_IMAGE_MEDIUM_SHARED_MEM] {
                ffi::ghostty_terminal_set(raw, o, (&off as *const bool).cast());
            }
            // Takes the allowed directory; none disables it.
            ffi::ghostty_terminal_set(raw, O::KITTY_IMAGE_MEDIUM_TEMP_FILE, ptr::null());
            ffi::ghostty_terminal_set(raw, O::GLYPH_PROTOCOL, (&off as *const bool).cast());
        }
        vt
    }

    pub(crate) fn raw(&self) -> ffi::Terminal {
        self.raw
    }

    pub fn effects(&mut self) -> &mut Effects {
        &mut self.effects
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if !bytes.is_empty() {
            unsafe { ffi::ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len()) }
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u32, cell_height: u32) {
        self.effects.size = ffi::SizeReportSize {
            rows,
            columns: cols,
            cell_width,
            cell_height,
        };
        unsafe {
            ffi::ghostty_terminal_resize(
                self.raw,
                cols.max(1),
                rows.max(1),
                cell_width,
                cell_height,
            );
        }
    }

    pub fn reset(&mut self) {
        unsafe { ffi::ghostty_terminal_reset(self.raw) }
    }

    fn get<T: Copy>(&self, data: ffi::TerminalData::Type) -> Option<T> {
        let mut out = std::mem::MaybeUninit::<T>::zeroed();
        let r = unsafe { ffi::ghostty_terminal_get(self.raw, data, out.as_mut_ptr().cast()) };
        (r == ffi::Result::SUCCESS).then(|| unsafe { out.assume_init() })
    }

    pub fn cols(&self) -> u16 {
        self.get(ffi::TerminalData::COLS).unwrap_or(1)
    }

    pub fn rows(&self) -> u16 {
        self.get(ffi::TerminalData::ROWS).unwrap_or(1)
    }

    /// Cursor (column, row) in the active area.
    pub fn cursor(&self) -> (u16, u16) {
        (
            self.get(ffi::TerminalData::CURSOR_X).unwrap_or(0),
            self.get(ffi::TerminalData::CURSOR_Y).unwrap_or(0),
        )
    }

    /// Where leaving the alternate screen with DECSET 1049 would put the primary cursor (its
    /// saved cursor, or the home position without one), as (column, row). Read on a snapshot
    /// copy: switching screens carries or restores cursors.
    pub fn saved_primary_cursor(&self) -> Option<(u16, u16)> {
        let (mut buf, mut len) = (ptr::null_mut::<u8>(), 0usize);
        let r = unsafe {
            ffi::ghostty_snapshot_encode_alloc(self.raw, ptr::null(), &mut buf, &mut len)
        };
        if r != ffi::Result::SUCCESS {
            return None;
        }
        let mut decoder: ffi::SnapshotDecoder = ptr::null_mut();
        let mut copy: ffi::Terminal = ptr::null_mut();
        let mut cursor = None;
        unsafe {
            if ffi::ghostty_snapshot_decoder_new_buf(ptr::null(), &mut decoder, buf, len)
                == ffi::Result::SUCCESS
            {
                // Only the active screens: the history is not needed.
                if ffi::ghostty_snapshot_decoder_ready(decoder, &mut copy) == ffi::Result::SUCCESS
                    && !copy.is_null()
                {
                    let leave = b"\x1b[?1049l";
                    ffi::ghostty_terminal_vt_write(copy, leave.as_ptr(), leave.len());
                    let get = |data| {
                        let mut v = 0u16;
                        (ffi::ghostty_terminal_get(copy, data, (&mut v as *mut u16).cast())
                            == ffi::Result::SUCCESS)
                            .then_some(v)
                    };
                    cursor = get(ffi::TerminalData::CURSOR_X).zip(get(ffi::TerminalData::CURSOR_Y));
                    ffi::ghostty_terminal_free(copy);
                }
                ffi::ghostty_snapshot_decoder_free(decoder);
            }
            ffi::ghostty_free(ptr::null(), buf, len);
        }
        cursor
    }

    pub fn cursor_pending_wrap(&self) -> bool {
        self.get(ffi::TerminalData::CURSOR_PENDING_WRAP)
            .unwrap_or(false)
    }

    /// The SGR style printed text gets.
    pub fn cursor_style(&self) -> Style {
        let mut s = default_style();
        let r = unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::TerminalData::CURSOR_SGR_STYLE,
                (&mut s as *mut Style).cast(),
            )
        };
        if r != ffi::Result::SUCCESS {
            return default_style();
        }
        s
    }

    pub fn is_alt_screen(&self) -> bool {
        self.get::<ffi::TerminalScreen::Type>(ffi::TerminalData::ACTIVE_SCREEN)
            == Some(ffi::TerminalScreen::ALTERNATE)
    }

    pub fn kitty_flags(&self) -> u8 {
        self.get(ffi::TerminalData::KITTY_KEYBOARD_FLAGS)
            .unwrap_or(0)
    }

    pub fn title(&self) -> String {
        self.get::<ffi::String>(ffi::TerminalData::TITLE)
            .filter(|s| !s.ptr.is_null() && s.len > 0)
            .map(|s| {
                String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(s.ptr, s.len) })
                    .into_owned()
            })
            .unwrap_or_default()
    }

    /// The working directory as the program last reported it (raw: a `file://` URL for OSC 7).
    pub fn pwd(&self) -> String {
        self.get::<ffi::String>(ffi::TerminalData::PWD)
            .map(|s| String::from_utf8_lossy(unsafe { ghostty_str(&s) }).into_owned())
            .unwrap_or_default()
    }

    pub fn scrollbar(&self) -> ffi::TerminalScrollbar {
        self.get(ffi::TerminalData::SCROLLBAR).unwrap_or_default()
    }

    /// Rows of scrollback above the active area.
    pub fn history(&self) -> usize {
        self.get(ffi::TerminalData::SCROLLBACK_ROWS).unwrap_or(0)
    }

    pub fn mode(&self, mode: ffi::Mode) -> bool {
        let mut m = ffi::TerminalModeConfig { mode, value: false };
        unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::TerminalData::MODE,
                (&mut m as *mut ffi::TerminalModeConfig).cast(),
            )
        };
        m.value
    }

    pub fn set_mode(&mut self, mode: ffi::Mode, value: bool) {
        let m = ffi::TerminalModeConfig { mode, value };
        unsafe {
            ffi::ghostty_terminal_set(
                self.raw,
                ffi::TerminalOption::MODE,
                (&m as *const ffi::TerminalModeConfig).cast(),
            )
        };
    }

    pub fn scroll_delta(&mut self, delta: isize) {
        self.scroll(
            ffi::TerminalScrollViewportTag::DELTA,
            ffi::TerminalScrollViewportValue { delta },
        );
    }

    pub fn scroll_top(&mut self) {
        self.scroll(
            ffi::TerminalScrollViewportTag::TOP,
            ffi::TerminalScrollViewportValue { row: 0 },
        );
    }

    pub fn scroll_bottom(&mut self) {
        self.scroll(
            ffi::TerminalScrollViewportTag::BOTTOM,
            ffi::TerminalScrollViewportValue { row: 0 },
        );
    }

    fn scroll(
        &mut self,
        tag: ffi::TerminalScrollViewportTag::Type,
        value: ffi::TerminalScrollViewportValue,
    ) {
        unsafe {
            ffi::ghostty_terminal_scroll_viewport(
                self.raw,
                ffi::TerminalScrollViewport { tag, value },
            );
        }
    }

    pub fn set_color(&mut self, which: ffi::TerminalOption::Type, rgb: u32) {
        let c = to_rgb(rgb);
        unsafe { ffi::ghostty_terminal_set(self.raw, which, (&c as *const ColorRgb).cast()) };
    }

    pub fn set_default_colors(&mut self, fg: u32, bg: u32, cursor: u32, palette: &[u32; 256]) {
        use ffi::TerminalOption as O;
        self.set_color(O::COLOR_FOREGROUND, fg);
        self.set_color(O::COLOR_BACKGROUND, bg);
        self.set_color(O::COLOR_CURSOR, cursor);
        let p: [ColorRgb; 256] = std::array::from_fn(|i| to_rgb(palette[i]));
        unsafe { ffi::ghostty_terminal_set(self.raw, O::COLOR_PALETTE, p.as_ptr().cast()) };
    }

    fn color(&self, data: ffi::TerminalData::Type) -> Option<u32> {
        self.get::<ColorRgb>(data).map(from_rgb)
    }

    /// Effective foreground, background and cursor colors (OSC overrides included), and the
    /// defaults they were set to.
    pub fn fg(&self) -> Option<u32> {
        self.color(ffi::TerminalData::COLOR_FOREGROUND)
    }
    pub fn bg(&self) -> Option<u32> {
        self.color(ffi::TerminalData::COLOR_BACKGROUND)
    }
    pub fn cursor_color(&self) -> Option<u32> {
        self.color(ffi::TerminalData::COLOR_CURSOR)
    }
    pub fn default_fg(&self) -> Option<u32> {
        self.color(ffi::TerminalData::COLOR_FOREGROUND_DEFAULT)
    }
    pub fn default_bg(&self) -> Option<u32> {
        self.color(ffi::TerminalData::COLOR_BACKGROUND_DEFAULT)
    }
    pub fn default_cursor_color(&self) -> Option<u32> {
        self.color(ffi::TerminalData::COLOR_CURSOR_DEFAULT)
    }

    fn palette_of(&self, data: ffi::TerminalData::Type) -> [u32; 256] {
        let mut p = [ColorRgb::default(); 256];
        unsafe { ffi::ghostty_terminal_get(self.raw, data, p.as_mut_ptr().cast()) };
        p.map(from_rgb)
    }

    pub fn palette(&self) -> [u32; 256] {
        self.palette_of(ffi::TerminalData::COLOR_PALETTE)
    }

    pub fn default_palette(&self) -> [u32; 256] {
        self.palette_of(ffi::TerminalData::COLOR_PALETTE_DEFAULT)
    }

    /// Grid reference for (column, row) with rows counted from the top of the scrollback.
    pub fn screen_ref(&self, x: u16, y: u32) -> Option<GridRef> {
        self.grid_ref(ffi::PointTag::SCREEN, x, y)
    }

    /// Grid reference for (column, row) in the active area.
    pub fn active_ref(&self, x: u16, y: u32) -> Option<GridRef> {
        self.grid_ref(ffi::PointTag::ACTIVE, x, y)
    }

    fn grid_ref(&self, tag: ffi::PointTag::Type, x: u16, y: u32) -> Option<GridRef> {
        let point = ffi::Point {
            tag,
            value: ffi::PointValue {
                coordinate: ffi::PointCoordinate { x, y },
            },
        };
        let mut out = GridRef {
            node: ptr::null_mut(),
            x: 0,
            y: 0,
        };
        let r = unsafe { ffi::ghostty_terminal_grid_ref(self.raw, point, &mut out) };
        (r == ffi::Result::SUCCESS && !out.node.is_null()).then_some(out)
    }

    /// Start tracking the cell at (column, row from the top of the scrollback).
    pub fn track(&self, x: u16, y: u32) -> Option<Tracked> {
        let point = ffi::Point {
            tag: ffi::PointTag::SCREEN,
            value: ffi::PointValue {
                coordinate: ffi::PointCoordinate { x, y },
            },
        };
        let mut out: ffi::TrackedGridRef = ptr::null_mut();
        let r = unsafe { ffi::ghostty_terminal_grid_ref_track(self.raw, point, &mut out) };
        (r == ffi::Result::SUCCESS && !out.is_null()).then(|| Tracked(out))
    }
}

impl Vt {
    /// Keep at least `lines` of scrollback (0: none).
    pub fn set_scrollback(&mut self, lines: usize) {
        set_scrollback(self.raw, lines);
    }

    /// The cursor a program gets by default, and back with DECSCUSR 0.
    pub fn set_default_cursor(&mut self, style: CursorStyle::Type, blink: bool) {
        unsafe {
            use ffi::TerminalOption as O;
            ffi::ghostty_terminal_set(
                self.raw,
                O::DEFAULT_CURSOR_STYLE,
                (&style as *const CursorStyle::Type).cast(),
            );
            ffi::ghostty_terminal_set(
                self.raw,
                O::DEFAULT_CURSOR_BLINK,
                (&blink as *const bool).cast(),
            );
        }
    }

    /// Kitty graphics with this much image memory; zero turns them off (and drops every image).
    pub fn set_kitty_graphics(&mut self, memory_limit: u64) {
        unsafe {
            ffi::ghostty_terminal_set(
                self.raw,
                ffi::TerminalOption::KITTY_IMAGE_STORAGE_LIMIT,
                (&memory_limit as *const u64).cast(),
            );
        }
    }

    fn kitty(&self) -> Option<ffi::KittyGraphics> {
        self.get::<ffi::KittyGraphics>(ffi::TerminalData::KITTY_GRAPHICS)
            .filter(|g| !g.is_null())
    }

    /// Changes whenever images or placements are added, replaced or deleted (not when they
    /// only move); 0 when nothing was ever stored.
    pub fn kitty_generation(&self) -> u64 {
        let Some(g) = self.kitty() else { return 0 };
        let mut out = 0u64;
        unsafe {
            kitty_ffi::ghostty_kitty_graphics_get(
                g,
                ffi::KittyGraphicsData::GENERATION,
                (&mut out as *mut u64).cast(),
            )
        };
        out
    }

    /// The active screen's placements.
    pub fn kitty_placements(&self) -> Vec<KittyPlacement> {
        let mut out = Vec::new();
        let Some(g) = self.kitty() else { return out };
        let mut it = self.placements;
        unsafe {
            if kitty_ffi::ghostty_kitty_graphics_get(
                g,
                ffi::KittyGraphicsData::PLACEMENT_ITERATOR,
                (&mut it as *mut ffi::KittyGraphicsPlacementIterator).cast(),
            ) != ffi::Result::SUCCESS
            {
                return out;
            }
            while kitty_ffi::ghostty_kitty_graphics_placement_next(it) {
                use ffi::KittyGraphicsPlacementData as D;
                let u32_of = |k| {
                    let mut v = 0u32;
                    kitty_ffi::ghostty_kitty_graphics_placement_get(
                        it,
                        k,
                        (&mut v as *mut u32).cast(),
                    );
                    v
                };
                let mut is_virtual = false;
                kitty_ffi::ghostty_kitty_graphics_placement_get(
                    it,
                    D::IS_VIRTUAL,
                    (&mut is_virtual as *mut bool).cast(),
                );
                let mut z = 0i32;
                kitty_ffi::ghostty_kitty_graphics_placement_get(
                    it,
                    D::Z,
                    (&mut z as *mut i32).cast(),
                );
                let image = u32_of(D::IMAGE_ID);
                let handle = kitty_ffi::ghostty_kitty_graphics_image(g, image);
                let render = (!is_virtual && !handle.is_null())
                    .then(|| {
                        let mut info = ffi::KittyGraphicsPlacementRenderInfo {
                            size: std::mem::size_of::<ffi::KittyGraphicsPlacementRenderInfo>(),
                            ..Default::default()
                        };
                        (kitty_ffi::ghostty_kitty_graphics_placement_render_info(
                            it, handle, self.raw, &mut info,
                        ) == ffi::Result::SUCCESS
                            && info.viewport_visible)
                            .then_some(info)
                    })
                    .flatten();
                out.push(KittyPlacement {
                    image,
                    placement: u32_of(D::PLACEMENT_ID),
                    is_virtual,
                    x_offset: u32_of(D::X_OFFSET),
                    y_offset: u32_of(D::Y_OFFSET),
                    source: (
                        u32_of(D::SOURCE_X),
                        u32_of(D::SOURCE_Y),
                        u32_of(D::SOURCE_WIDTH),
                        u32_of(D::SOURCE_HEIGHT),
                    ),
                    cols: u32_of(D::COLUMNS),
                    rows: u32_of(D::ROWS),
                    z,
                    render,
                });
            }
        }
        out
    }

    fn kitty_image_handle(&self, id: u32) -> Option<ffi::KittyGraphicsImage> {
        let g = self.kitty()?;
        let h = unsafe { kitty_ffi::ghostty_kitty_graphics_image(g, id) };
        (!h.is_null()).then_some(h)
    }

    /// Generation of the stored image `id` (changes when it is retransmitted), if stored.
    pub fn kitty_image_generation(&self, id: u32) -> Option<u64> {
        let h = self.kitty_image_handle(id)?;
        let mut v = 0u64;
        let r = unsafe {
            kitty_ffi::ghostty_kitty_graphics_image_get(
                h,
                ffi::KittyGraphicsImageData::GENERATION,
                (&mut v as *mut u64).cast(),
            )
        };
        (r == ffi::Result::SUCCESS).then_some(v)
    }

    /// Size of the stored image `id` in pixels.
    pub fn kitty_image_size(&self, id: u32) -> Option<(u32, u32)> {
        let h = self.kitty_image_handle(id)?;
        let (mut w, mut hgt) = (0u32, 0u32);
        unsafe {
            use ffi::KittyGraphicsImageData as D;
            kitty_ffi::ghostty_kitty_graphics_image_get(h, D::WIDTH, (&mut w as *mut u32).cast());
            kitty_ffi::ghostty_kitty_graphics_image_get(
                h,
                D::HEIGHT,
                (&mut hgt as *mut u32).cast(),
            );
        }
        Some((w, hgt))
    }

    /// Image `id` as (width, height, straight RGBA8).
    pub fn kitty_image(&self, id: u32) -> Option<(u32, u32, Vec<u8>)> {
        use ffi::KittyGraphicsImageData as D;
        let h = self.kitty_image_handle(id)?;
        let (mut w, mut hgt, mut format) = (0u32, 0u32, ffi::KittyImageFormat::RGBA);
        let mut pixels = ffi::String {
            ptr: ptr::null(),
            len: 0,
        };
        unsafe {
            kitty_ffi::ghostty_kitty_graphics_image_get(h, D::WIDTH, (&mut w as *mut u32).cast());
            kitty_ffi::ghostty_kitty_graphics_image_get(
                h,
                D::HEIGHT,
                (&mut hgt as *mut u32).cast(),
            );
            kitty_ffi::ghostty_kitty_graphics_image_get(
                h,
                D::FORMAT,
                (&mut format as *mut ffi::KittyImageFormat::Type).cast(),
            );
            kitty_ffi::ghostty_kitty_graphics_image_get(
                h,
                D::PIXELS,
                (&mut pixels as *mut ffi::String).cast(),
            );
        }
        if pixels.ptr.is_null() {
            return None;
        }
        let px = unsafe { std::slice::from_raw_parts(pixels.ptr, pixels.len) };
        let rgba = match format {
            ffi::KittyImageFormat::RGBA => px.to_vec(),
            ffi::KittyImageFormat::RGB => px
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|&[r, g, b]| [r, g, b, 255])
                .collect(),
            ffi::KittyImageFormat::GRAY_ALPHA => px
                .as_chunks::<2>()
                .0
                .iter()
                .flat_map(|&[g, a]| [g, g, g, a])
                .collect(),
            ffi::KittyImageFormat::GRAY => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            _ => return None,
        };
        Some((w, hgt, rgba))
    }
}

/// libghostty-vt decodes PNG images through the embedder (once per process).
fn install_png_decoder() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let f: ffi::SysDecodePngFn = Some(decode_png);
        unsafe { ffi::ghostty_sys_set(ffi::SysOption::GHOSTTY_SYS_OPT_DECODE_PNG, fn_ptr(f)) };
    });
}

unsafe extern "C" fn decode_png(
    _userdata: *mut c_void,
    allocator: *const ffi::Allocator,
    data: *const u8,
    data_len: usize,
    out: *mut ffi::SysImage,
) -> bool {
    let bytes = unsafe { std::slice::from_raw_parts(data, data_len) };
    let Some((width, height, rgba)) = crate::kitty::decode_png(bytes) else {
        return false;
    };
    let buf = unsafe { ffi::ghostty_alloc(allocator, rgba.len()) };
    if buf.is_null() {
        return false;
    }
    unsafe {
        ptr::copy_nonoverlapping(rgba.as_ptr(), buf, rgba.len());
        *out = ffi::SysImage {
            width,
            height,
            data: buf,
            data_len: rgba.len(),
        };
    }
    true
}

impl Vt {
    /// The active selection.
    pub fn selection(&self) -> Option<Sel> {
        let mut sel = ffi::Selection {
            start: empty_ref(),
            end: empty_ref(),
            rectangle: false,
        };
        let r = unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::TerminalData::SELECTION,
                (&mut sel as *mut ffi::Selection).cast(),
            )
        };
        if r != ffi::Result::SUCCESS {
            return None;
        }
        self.sel_from(&sel)
    }

    fn sel_from(&self, sel: &ffi::Selection) -> Option<Sel> {
        Some(Sel {
            start: self.screen_point_of(&sel.start)?,
            end: self.screen_point_of(&sel.end)?,
            rectangle: sel.rectangle,
        })
    }

    /// (column, row from the top of the scrollback) of a grid reference.
    pub fn screen_point_of(&self, r: &GridRef) -> Option<(u16, u32)> {
        let mut out = ffi::PointCoordinate { x: 0, y: 0 };
        let res = unsafe {
            ffi::ghostty_terminal_point_from_grid_ref(self.raw, r, ffi::PointTag::SCREEN, &mut out)
        };
        (res == ffi::Result::SUCCESS).then_some((out.x, out.y))
    }

    /// Install (or with `None`, clear) the active selection; libghostty-vt keeps it on the
    /// text as it scrolls and reflows.
    pub fn set_selection(&mut self, sel: Option<Sel>) {
        let raw = sel.and_then(|s| {
            Some(ffi::Selection {
                start: self.screen_ref(s.start.0, s.start.1)?,
                end: self.screen_ref(s.end.0, s.end.1)?,
                rectangle: s.rectangle,
            })
        });
        let value = raw
            .as_ref()
            .map_or(ptr::null(), |r| (r as *const ffi::Selection).cast());
        unsafe { ffi::ghostty_terminal_set(self.raw, ffi::TerminalOption::SELECTION, value) };
    }

    /// Select everything (scrollback and screen).
    pub fn select_all(&mut self) {
        let mut sel = ffi::Selection {
            start: empty_ref(),
            end: empty_ref(),
            rectangle: false,
        };
        if unsafe { ffi::ghostty_terminal_select_all(self.raw, &mut sel) } == ffi::Result::SUCCESS {
            let s = self.sel_from(&sel);
            self.set_selection(s);
        }
    }

    /// The selected text as it is copied: soft wraps joined, trailing blanks trimmed.
    pub fn selection_text(&self) -> String {
        let mut sel = ffi::Selection {
            start: empty_ref(),
            end: empty_ref(),
            rectangle: false,
        };
        let r = unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::TerminalData::SELECTION,
                (&mut sel as *mut ffi::Selection).cast(),
            )
        };
        if r != ffi::Result::SUCCESS {
            return String::new();
        }
        let format = |emit| {
            let bytes =
                self.try_format(emit, true, true, ffi::FORMATTER_EXTRAS_NONE, Some(&sel))?;
            // Valid UTF-8 (always, from the formatter) becomes the string without a copy.
            Some(
                String::from_utf8(bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
            )
        };
        // Concealed cells (SGR 8, e.g. a password echoed hidden) are not shown, so they are not
        // copied either. The plain formatter has no notion of them; the styled one marks them,
        // over exactly the cells the copy has, in one pass along the pages. Without it, nothing
        // says the plain text is safe to copy: nothing is copied. The styled text is read in the
        // formatter's buffer and freed before the plain one is made, so a large copy holds one
        // of the two at a time.
        let concealed = self.try_format_with(
            ffi::FormatterFormat::VT,
            true,
            true,
            ffi::FORMATTER_EXTRAS_NONE,
            Some(&sel),
            |b| conceals(&String::from_utf8_lossy(b)),
        );
        let Some(concealed) = concealed else {
            return String::new();
        };
        let Some(plain) = format(ffi::FormatterFormat::PLAIN) else {
            return String::new();
        };
        if !concealed {
            return plain;
        }
        // The plain text with each concealed character blanked as wide as its cells; should the
        // two not line up, the styled output's text, which blanks every concealed character
        // (as wide as the grid laid the concealed cells out, where it can tell).
        #[cfg(test)]
        let plain = if FALLBACK.with(|f| f.get()) {
            String::from("\u{1}")
        } else {
            plain
        };
        if let Some(out) = self.blank_concealed(&sel, &plain) {
            return out;
        }
        // The fallback (rare: the two did not line up) formats the styled text again, and
        // measures concealed text by the concealed cells.
        drop(plain);
        let Some(styled) = format(ffi::FormatterFormat::VT) else {
            return String::new();
        };
        let mut hidden: Vec<(Vec<char>, usize)> = Vec::new();
        self.for_each_selected_cell(&sel, |text, concealed, width| {
            if concealed {
                hidden.push((text.to_vec(), width));
            }
            true
        });
        styled_text(&styled, self.mode(mode(2027, false)), &hidden)
    }

    /// Grid references to the rows `sy..=ey` (column 0), in order. A lookup by row walks the
    /// pages from the top of the scrollback, so rather than one per row, each page's run of
    /// rows is found with a few (the rows of a page are consecutive) and stepped through.
    fn row_refs(&self, sy: u32, ey: u32) -> Option<Vec<GridRef>> {
        let mut out = Vec::with_capacity((ey - sy + 1) as usize);
        let mut y = sy;
        while y <= ey {
            let r = self.screen_ref(0, y)?;
            // Whether row `y + k` is on `r`'s page, `k` rows below it.
            let on_page = |k: u32| {
                u16::try_from(u32::from(r.y) + k).is_ok_and(|py| {
                    self.screen_ref(0, y + k)
                        .is_some_and(|p| p.node == r.node && p.y == py)
                })
            };
            // `last`: a row on the page; `past`: the first row known not to be (or past `ey`).
            let (mut last, mut step) = (0, 1);
            let mut past = loop {
                let k = last + step;
                if k > ey - y {
                    break ey - y + 1;
                }
                if !on_page(k) {
                    break k;
                }
                last = k;
                step *= 2;
            };
            while past - last > 1 {
                let mid = last + (past - last) / 2;
                if on_page(mid) {
                    last = mid;
                } else {
                    past = mid;
                }
            }
            out.extend((0..=last).map(|k| GridRef {
                y: r.y + k as u16,
                ..r
            }));
            y += last + 1;
        }
        Some(out)
    }

    /// Call `f` with each selected cell with text, in the order the formatter copies them:
    /// its characters, whether it is concealed, how many cells it takes; until `f` returns
    /// false. Nothing is kept per cell. `None` when the selection's cells cannot be found.
    fn for_each_selected_cell(
        &self,
        sel: &ffi::Selection,
        mut f: impl FnMut(&[char], bool, usize) -> bool,
    ) -> Option<()> {
        let mut ordered = *sel;
        let r = unsafe {
            ffi::ghostty_terminal_selection_ordered(
                self.raw,
                sel,
                ffi::SelectionOrder::FORWARD,
                &mut ordered,
            )
        };
        if r != ffi::Result::SUCCESS {
            return None;
        }
        let (sx, sy) = self.screen_point_of(&ordered.start)?;
        let (mut ex, mut ey) = self.screen_point_of(&ordered.end)?;
        let cols = self.cols();
        let rect = ordered.rectangle;
        // A linear selection ending on the spacer a wide character left at a row's end (it
        // moved to the next row) copies that character, as the formatter does.
        if !rect && cell(&ordered.end).wide == SPACER_HEAD {
            (ex, ey) = (0, ey + 1);
        }
        // The cell's characters (one buffer for them all).
        let mut text: Vec<char> = Vec::new();
        for (y, r0) in (sy..=ey).zip(self.row_refs(sy, ey)?) {
            let (from, to) = match (rect, y == sy, y == ey) {
                (true, ..) => (sx.min(ex), sx.max(ex)),
                (false, first, last) => {
                    (if first { sx } else { 0 }, if last { ex } else { cols - 1 })
                }
            };
            // A range starting on a wide character's second half copies the character.
            let from = if from > 0 && cell(&GridRef { x: from, ..r0 }).wide == SPACER_TAIL {
                from - 1
            } else {
                from
            };
            // Whether each style of the row seen so far conceals (style ids are per page).
            let mut styles: Vec<(u16, bool)> = Vec::new();
            for x in from..=to.min(cols - 1) {
                let r = GridRef { x, ..r0 };
                let c = cell(&r);
                if c.wide == SPACER_TAIL || c.wide == SPACER_HEAD {
                    continue;
                }
                text.clear();
                if c.tag == ffi::CellContentTag::CODEPOINT_GRAPHEME {
                    text.extend(graphemes(&r));
                } else {
                    text.extend(char::from_u32(c.codepoint));
                }
                // Blank cells: the formatter's text has ASCII blanks there (or nothing). A cell
                // whose character is a space is one, marks on it and all: the formatter blanks
                // it by its first character.
                if matches!(text.first(), None | Some('\0' | ' ')) {
                    continue;
                }
                let hidden = c.has_styling
                    && match styles.iter().find(|(id, _)| *id == c.style_id) {
                        Some(&(_, h)) => h,
                        None => {
                            let h = style(&r).flags & ffi::STYLE_FLAG_INVISIBLE != 0;
                            styles.push((c.style_id, h));
                            h
                        }
                    };
                if !f(&text, hidden, if c.wide == WIDE { 2 } else { 1 }) {
                    return Some(());
                }
            }
        }
        Some(())
    }

    /// `plain` (the plain formatter's copy of `sel`) with the characters of concealed cells
    /// turned into one blank per cell they take, trailing blanks of each line trimmed. `None`
    /// when the selected cells' characters are not the ones `plain` has. The two are walked
    /// together: what is held besides `plain` is the output.
    fn blank_concealed(&self, sel: &ffi::Selection, plain: &str) -> Option<String> {
        let mut out = String::with_capacity(plain.len());
        // The formatter's blanks and line breaks at the start of `rest` (other spaces are
        // cells' text) go to `out`; the rest of it is returned.
        fn breaks<'a>(rest: &'a str, out: &mut String) -> &'a str {
            let n = rest.len() - rest.trim_start_matches([' ', '\n', '\r']).len();
            for c in rest[..n].chars() {
                if c == '\n' {
                    let kept = out.trim_end_matches(' ').len();
                    out.truncate(kept);
                }
                out.push(c);
            }
            &rest[n..]
        }
        let mut rest = plain;
        let mut lined_up = true;
        self.for_each_selected_cell(sel, |text, hidden, width| {
            rest = breaks(rest, &mut out);
            let mut chars = rest.chars();
            if !text.iter().all(|&c| chars.next() == Some(c)) {
                lined_up = false;
                return false;
            }
            rest = chars.as_str();
            if hidden {
                out.extend(std::iter::repeat_n(' ', width));
            } else {
                out.extend(text);
            }
            true
        })?;
        if !lined_up || !breaks(rest, &mut out).is_empty() {
            return None;
        }
        let kept = out.trim_end_matches(' ').len();
        out.truncate(kept);
        Some(out)
    }

    /// The selected text as the plain formatter has it (what `selection_text` matches where
    /// nothing is concealed), and as `blank_concealed` makes it of that.
    #[cfg(test)]
    pub(crate) fn selection_copies(&self) -> (String, Option<String>) {
        let mut sel = ffi::Selection {
            start: empty_ref(),
            end: empty_ref(),
            rectangle: false,
        };
        let r = unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::TerminalData::SELECTION,
                (&mut sel as *mut ffi::Selection).cast(),
            )
        };
        if r != ffi::Result::SUCCESS {
            return (String::new(), Some(String::new()));
        }
        let bytes = self.format_raw(
            ffi::FormatterFormat::PLAIN,
            true,
            true,
            ffi::FORMATTER_EXTRAS_NONE,
            Some(&sel),
        );
        let plain = String::from_utf8_lossy(&bytes).into_owned();
        let blanked = self.blank_concealed(&sel, &plain);
        (plain, blanked)
    }

    /// A mouse press starting a selection (`by`: what a click selects), with word boundaries.
    /// Returns the selection it makes, if any (a single click selects nothing yet).
    pub fn gesture_press(&mut self, at: Pointer, by: SelectBy::Type, words: &[u32]) -> Option<Sel> {
        let ev = self.press;
        let behaviors = ffi::SelectionGestureBehaviors {
            single_click: by,
            double_click: by,
            triple_click: by,
        };
        unsafe {
            use ffi::SelectionGestureEventOption as O;
            // Untimed: the app counts clicks, and `by` says what this one selects.
            ffi::ghostty_selection_gesture_reset(self.gesture, self.raw);
            ffi::ghostty_selection_gesture_event_set(
                ev,
                O::BEHAVIORS,
                (&behaviors as *const ffi::SelectionGestureBehaviors).cast(),
            );
        }
        self.gesture_event(ev, at, None, false, words)
    }

    /// Dragging with the button held.
    pub fn gesture_drag(
        &mut self,
        at: Pointer,
        geometry: ffi::SelectionGestureGeometry,
        rectangle: bool,
        words: &[u32],
    ) -> Option<Sel> {
        let ev = self.drag;
        self.gesture_event(ev, at, Some(geometry), rectangle, words)
    }

    pub fn gesture_release(&mut self, at: Pointer) {
        let ev = self.release;
        if let Some(r) = self.screen_ref(at.cell.0, at.cell.1) {
            unsafe {
                ffi::ghostty_selection_gesture_event_set(
                    ev,
                    ffi::SelectionGestureEventOption::REF,
                    (&r as *const GridRef).cast(),
                );
                ffi::ghostty_selection_gesture_event(self.gesture, self.raw, ev, ptr::null_mut());
            }
        }
    }

    fn gesture_event(
        &mut self,
        ev: ffi::SelectionGestureEvent,
        at: Pointer,
        geometry: Option<ffi::SelectionGestureGeometry>,
        rectangle: bool,
        words: &[u32],
    ) -> Option<Sel> {
        let r = self.screen_ref(at.cell.0, at.cell.1)?;
        let pos = ffi::SurfacePosition {
            x: at.px.0,
            y: at.px.1,
        };
        let cps = ffi::Codepoints {
            ptr: words.as_ptr(),
            len: words.len(),
        };
        let mut sel = ffi::Selection {
            start: empty_ref(),
            end: empty_ref(),
            rectangle: false,
        };
        let res = unsafe {
            use ffi::SelectionGestureEventOption as O;
            let set = |o, v: *const c_void| {
                ffi::ghostty_selection_gesture_event_set(ev, o, v);
            };
            set(O::REF, (&r as *const GridRef).cast());
            set(O::POSITION, (&pos as *const ffi::SurfacePosition).cast());
            if !words.is_empty() {
                set(
                    O::WORD_BOUNDARY_CODEPOINTS,
                    (&cps as *const ffi::Codepoints).cast(),
                );
            }
            if let Some(g) = &geometry {
                set(
                    O::GEOMETRY,
                    (g as *const ffi::SelectionGestureGeometry).cast(),
                );
                set(O::RECTANGLE, (&rectangle as *const bool).cast());
            }
            ffi::ghostty_selection_gesture_event(self.gesture, self.raw, ev, &mut sel)
        };
        (res == ffi::Result::SUCCESS)
            .then(|| self.sel_from(&sel))
            .flatten()
    }
}

/// What the formatter includes besides the text.
#[derive(Clone, Copy, Debug, Default)]
pub struct FormatExtras {
    pub palette: bool,
    pub modes: bool,
    pub scrolling_region: bool,
    pub tabstops: bool,
    pub pwd: bool,
    pub keyboard: bool,
    pub cursor: bool,
    pub style: bool,
    pub hyperlink: bool,
    pub protection: bool,
    pub kitty_keyboard: bool,
    pub charsets: bool,
}

impl FormatExtras {
    fn bits(self) -> ffi::FormatterExtras {
        [
            (self.palette, ffi::FORMATTER_EXTRA_PALETTE),
            (self.modes, ffi::FORMATTER_EXTRA_MODES),
            (self.scrolling_region, ffi::FORMATTER_EXTRA_SCROLLING_REGION),
            (self.tabstops, ffi::FORMATTER_EXTRA_TABSTOPS),
            (self.pwd, ffi::FORMATTER_EXTRA_PWD),
            (self.keyboard, ffi::FORMATTER_EXTRA_KEYBOARD),
            (self.cursor, ffi::FORMATTER_EXTRA_CURSOR),
            (self.style, ffi::FORMATTER_EXTRA_STYLE),
            (self.hyperlink, ffi::FORMATTER_EXTRA_HYPERLINK),
            (self.protection, ffi::FORMATTER_EXTRA_PROTECTION),
            (self.kitty_keyboard, ffi::FORMATTER_EXTRA_KITTY_KEYBOARD),
            (self.charsets, ffi::FORMATTER_EXTRA_CHARSETS),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(0, |bits, (_, bit)| bits | bit)
    }
}

impl Vt {
    /// The active screen (or the rows `range` spans) as text: VT sequences (`vt`) or plain,
    /// soft wraps joined (`unwrap`), trailing blanks trimmed (`trim`).
    pub fn format(
        &self,
        vt: bool,
        unwrap: bool,
        trim: bool,
        extras: FormatExtras,
        range: Option<Sel>,
    ) -> Vec<u8> {
        let sel = range.and_then(|s| {
            Some(ffi::Selection {
                start: self.screen_ref(s.start.0, s.start.1)?,
                end: self.screen_ref(s.end.0, s.end.1)?,
                rectangle: s.rectangle,
            })
        });
        if range.is_some() && sel.is_none() {
            return Vec::new();
        }
        let emit = if vt {
            ffi::FormatterFormat::VT
        } else {
            ffi::FormatterFormat::PLAIN
        };
        self.format_raw(emit, unwrap, trim, extras.bits(), sel.as_ref())
    }

    /// Format the active screen (or `sel`) with a formatter set to these options; empty
    /// when formatting fails.
    fn format_raw(
        &self,
        emit: ffi::FormatterFormat::Type,
        unwrap: bool,
        trim: bool,
        extras: ffi::FormatterExtras,
        sel: Option<&ffi::Selection>,
    ) -> Vec<u8> {
        self.try_format(emit, unwrap, trim, extras, sel)
            .unwrap_or_default()
    }

    /// Format the active screen (or `sel`) with a formatter set to these options; `None` when
    /// formatting fails.
    fn try_format(
        &self,
        emit: ffi::FormatterFormat::Type,
        unwrap: bool,
        trim: bool,
        extras: ffi::FormatterExtras,
        sel: Option<&ffi::Selection>,
    ) -> Option<Vec<u8>> {
        self.try_format_with(emit, unwrap, trim, extras, sel, <[u8]>::to_vec)
    }

    /// `try_format`, handing `read` the formatter's own buffer rather than a copy of it.
    fn try_format_with<R>(
        &self,
        emit: ffi::FormatterFormat::Type,
        unwrap: bool,
        trim: bool,
        extras: ffi::FormatterExtras,
        sel: Option<&ffi::Selection>,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Option<R> {
        #[cfg(test)]
        if FAIL_FORMAT.with(|f| f.get()) == Some(emit) {
            return None;
        }
        let mut f: ffi::Formatter = ptr::null_mut();
        if unsafe { ffi::ghostty_formatter_terminal_new(ptr::null(), &mut f, self.raw) }
            != ffi::Result::SUCCESS
        {
            return None;
        }
        fn set<T>(f: ffi::Formatter, opt: ffi::FormatterOption::Type, value: &T) -> bool {
            let value = (value as *const T).cast();
            unsafe { ffi::ghostty_formatter_set(f, opt, value) == ffi::Result::SUCCESS }
        }
        use ffi::FormatterOption as O;
        let applied = set(f, O::EMIT, &emit)
            && set(f, O::UNWRAP, &unwrap)
            && set(f, O::TRIM, &trim)
            && set(f, O::EXTRAS, &extras)
            && sel.is_none_or(|s| set(f, O::SELECTION, s));
        if !applied {
            unsafe { ffi::ghostty_formatter_free(f) };
            return None;
        }
        let (mut out, mut len): (*mut u8, usize) = (ptr::null_mut(), 0);
        let r = unsafe { ffi::ghostty_formatter_format_alloc(f, ptr::null(), &mut out, &mut len) };
        let res = (r == ffi::Result::SUCCESS).then(|| {
            if out.is_null() {
                return read(&[]);
            }
            let res = read(unsafe { std::slice::from_raw_parts(out, len) });
            unsafe { ffi::ghostty_free(ptr::null(), out, len) };
            res
        });
        unsafe { ffi::ghostty_formatter_free(f) };
        res
    }
}

fn empty_ref() -> GridRef {
    GridRef {
        node: ptr::null_mut(),
        x: 0,
        y: 0,
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_selection_gesture_free(self.gesture, self.raw);
            for ev in [self.press, self.drag, self.release] {
                ffi::ghostty_selection_gesture_event_free(ev);
            }
            kitty_ffi::ghostty_kitty_graphics_placement_iterator_free(self.placements);
            ffi::ghostty_terminal_free(self.raw)
        }
    }
}

fn fn_ptr<F>(f: Option<F>) -> *const c_void {
    // Callback options take the function pointer itself as the value.
    match f {
        Some(f) => {
            let p: *const c_void = unsafe { std::mem::transmute_copy(&f) };
            p
        }
        None => ptr::null(),
    }
}

pub fn to_rgb(c: u32) -> ColorRgb {
    ColorRgb {
        r: (c >> 16) as u8,
        g: (c >> 8) as u8,
        b: c as u8,
    }
}

pub fn from_rgb(c: ColorRgb) -> u32 {
    (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32
}

/// Row-level data.
pub fn row_wrapped(r: &GridRef) -> bool {
    row_flag(r, ffi::RowData::WRAP)
}

/// Whether the row has prompt cells (OSC 133): `PROMPT` for a prompt's first line,
/// `PROMPT_CONTINUATION` for the others, `NONE`.
pub fn row_prompt(r: &GridRef) -> ffi::RowSemanticPrompt::Type {
    let mut row: ffi::Row = 0;
    if unsafe { ffi::ghostty_grid_ref_row(r, &mut row) } != ffi::Result::SUCCESS {
        return ffi::RowSemanticPrompt::NONE;
    }
    let mut v: ffi::RowSemanticPrompt::Type = ffi::RowSemanticPrompt::NONE;
    unsafe {
        ffi::ghostty_row_get(
            row,
            ffi::RowData::SEMANTIC_PROMPT,
            (&mut v as *mut ffi::RowSemanticPrompt::Type).cast(),
        )
    };
    v
}

pub use ffi::RowSemanticPrompt as RowPrompt;

/// What the cell is part of, as shell integration marked it (OSC 133): command output,
/// user input or a prompt.
pub fn cell_semantic(r: &GridRef) -> ffi::CellSemanticContent::Type {
    let mut raw: ffi::Cell = 0;
    if unsafe { ffi::ghostty_grid_ref_cell(r, &mut raw) } != ffi::Result::SUCCESS {
        return ffi::CellSemanticContent::OUTPUT;
    }
    let mut v: ffi::CellSemanticContent::Type = ffi::CellSemanticContent::OUTPUT;
    unsafe {
        ffi::ghostty_cell_get(
            raw,
            ffi::CellData::SEMANTIC_CONTENT,
            (&mut v as *mut ffi::CellSemanticContent::Type).cast(),
        )
    };
    v
}

pub use ffi::CellSemanticContent as Semantic;

pub fn row_has_hyperlink(r: &GridRef) -> bool {
    row_flag(r, ffi::RowData::HYPERLINK)
}

fn row_flag(r: &GridRef, data: ffi::RowData::Type) -> bool {
    let mut row: ffi::Row = 0;
    if unsafe { ffi::ghostty_grid_ref_row(r, &mut row) } != ffi::Result::SUCCESS {
        return false;
    }
    let mut v = false;
    unsafe { ffi::ghostty_row_get(row, data, (&mut v as *mut bool).cast()) };
    v
}

/// The cell `r` points at.
pub fn cell(r: &GridRef) -> RawCell {
    let mut raw: ffi::Cell = 0;
    if unsafe { ffi::ghostty_grid_ref_cell(r, &mut raw) } != ffi::Result::SUCCESS {
        return RawCell::default();
    }
    decode_cell(raw)
}

fn decode_cell(raw: ffi::Cell) -> RawCell {
    let mut c = RawCell::default();
    let keys = [
        ffi::CellData::CODEPOINT,
        ffi::CellData::CONTENT_TAG,
        ffi::CellData::WIDE,
        ffi::CellData::HAS_STYLING,
        ffi::CellData::STYLE_ID,
        ffi::CellData::HAS_HYPERLINK,
    ];
    let mut values: [*mut c_void; 6] = [
        (&mut c.codepoint as *mut u32).cast(),
        (&mut c.tag as *mut ffi::CellContentTag::Type).cast(),
        (&mut c.wide as *mut ffi::CellWide::Type).cast(),
        (&mut c.has_styling as *mut bool).cast(),
        (&mut c.style_id as *mut u16).cast(),
        (&mut c.has_hyperlink as *mut bool).cast(),
    ];
    unsafe {
        ffi::ghostty_cell_get_multi(
            raw,
            keys.len(),
            keys.as_ptr(),
            values.as_mut_ptr(),
            ptr::null_mut(),
        )
    };
    match c.tag {
        TAG_BG_PALETTE => unsafe {
            ffi::ghostty_cell_get(
                raw,
                ffi::CellData::COLOR_PALETTE,
                (&mut c.bg_palette as *mut u8).cast(),
            );
        },
        TAG_BG_RGB => unsafe {
            ffi::ghostty_cell_get(
                raw,
                ffi::CellData::COLOR_RGB,
                (&mut c.bg_rgb as *mut ColorRgb).cast(),
            );
        },
        _ => {}
    }
    c
}

/// libghostty-vt's render state of a terminal's viewport: rows it rebuilds only where they
/// changed, and marks dirty until [`Render::clean`].
pub struct Render {
    state: ffi::RenderState,
    rows: ffi::RenderStateRowIterator,
    cells: ffi::RenderStateRowCells,
}

unsafe impl Send for Render {}

/// A cell as the render state has it.
pub struct RenderCell {
    pub raw: RawCell,
    pub style: Option<Style>,
    /// Base character and combining characters of a grapheme cluster (empty otherwise).
    pub graphemes: Vec<char>,
}

impl Default for Render {
    fn default() -> Self {
        Self::new()
    }
}

impl Render {
    pub fn new() -> Self {
        let mut r = Self {
            state: ptr::null_mut(),
            rows: ptr::null_mut(),
            cells: ptr::null_mut(),
        };
        unsafe {
            ffi::ghostty_render_state_new(ptr::null(), &mut r.state);
            ffi::ghostty_render_state_row_iterator_new(ptr::null(), &mut r.rows);
            ffi::ghostty_render_state_row_cells_new(ptr::null(), &mut r.cells);
        }
        r
    }

    /// Bring it up to date with `vt`; whether anything changed since the last `clean`.
    pub fn update(&mut self, vt: &Vt) -> ffi::RenderStateDirty::Type {
        let mut dirty = ffi::RenderStateDirty::FULL;
        unsafe {
            ffi::ghostty_render_state_update(self.state, vt.raw());
            ffi::ghostty_render_state_get(
                self.state,
                ffi::RenderStateData::DIRTY,
                (&mut dirty as *mut ffi::RenderStateDirty::Type).cast(),
            );
        }
        dirty
    }

    /// For each viewport row (top to bottom), `f(row, dirty, wrapped, cells)`; the cells are
    /// read only when `want(row, dirty)` says so (`None` otherwise).
    pub fn rows(
        &mut self,
        mut want: impl FnMut(usize, bool) -> bool,
        mut f: impl FnMut(usize, bool, bool, Option<Vec<RenderCell>>),
    ) {
        use ffi::RenderStateRowCellsData as C;
        use ffi::RenderStateRowData as R;
        unsafe {
            let mut it = self.rows;
            if ffi::ghostty_render_state_get(
                self.state,
                ffi::RenderStateData::ROW_ITERATOR,
                (&mut it as *mut ffi::RenderStateRowIterator).cast(),
            ) != ffi::Result::SUCCESS
            {
                return;
            }
            let mut y = 0;
            while ffi::ghostty_render_state_row_iterator_next(it) {
                let mut dirty = true;
                ffi::ghostty_render_state_row_get(it, R::DIRTY, (&mut dirty as *mut bool).cast());
                let mut row: ffi::Row = 0;
                ffi::ghostty_render_state_row_get(it, R::RAW, (&mut row as *mut ffi::Row).cast());
                let mut wrapped = false;
                ffi::ghostty_row_get(row, ffi::RowData::WRAP, (&mut wrapped as *mut bool).cast());
                let cells = want(y, dirty).then(|| {
                    let mut cells = Vec::new();
                    let mut c = self.cells;
                    ffi::ghostty_render_state_row_get(
                        it,
                        R::CELLS,
                        (&mut c as *mut ffi::RenderStateRowCells).cast(),
                    );
                    while ffi::ghostty_render_state_row_cells_next(c) {
                        let mut raw: ffi::Cell = 0;
                        ffi::ghostty_render_state_row_cells_get(
                            c,
                            C::RAW,
                            (&mut raw as *mut ffi::Cell).cast(),
                        );
                        let cell = decode_cell(raw);
                        let style = cell.has_styling.then(|| {
                            let mut st = default_style();
                            ffi::ghostty_render_state_row_cells_get(
                                c,
                                C::STYLE,
                                (&mut st as *mut Style).cast(),
                            );
                            st
                        });
                        let graphemes = if cell.tag == TAG_GRAPHEME {
                            let mut len = 0u32;
                            ffi::ghostty_render_state_row_cells_get(
                                c,
                                C::GRAPHEMES_LEN,
                                (&mut len as *mut u32).cast(),
                            );
                            let mut buf = vec![0u32; len as usize];
                            let mut out = ffi::CodepointBuffer {
                                ptr: buf.as_mut_ptr(),
                                cap: buf.len(),
                                len: 0,
                            };
                            ffi::ghostty_render_state_row_cells_get(
                                c,
                                C::GRAPHEMES_BUF,
                                (&mut out as *mut ffi::CodepointBuffer).cast(),
                            );
                            buf.truncate(out.len.min(buf.len()));
                            buf.into_iter().filter_map(char::from_u32).collect()
                        } else {
                            Vec::new()
                        };
                        cells.push(RenderCell {
                            raw: cell,
                            style,
                            graphemes,
                        });
                    }
                    cells
                });
                f(y, dirty, wrapped, cells);
                let clean = false;
                ffi::ghostty_render_state_row_set(
                    it,
                    ffi::RenderStateRowOption::DIRTY,
                    (&clean as *const bool).cast(),
                );
                y += 1;
            }
            let clean = ffi::RenderStateDirty::FALSE;
            ffi::ghostty_render_state_set(
                self.state,
                ffi::RenderStateOption::DIRTY,
                (&clean as *const ffi::RenderStateDirty::Type).cast(),
            );
        }
    }
}

/// The cursor as the render state has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderCursor {
    pub style: ffi::CursorStyle::Type,
    pub visible: bool,
    pub blinking: bool,
}

pub use ffi::CursorStyle;
pub use ffi::CursorStyle as CursorVisual;

fn set_scrollback(raw: ffi::Terminal, lines: usize) {
    // libghostty-vt prunes whole pages (a few hundred rows), so the limit gets a page of
    // margin to keep at least `lines` lines.
    let max_lines = if lines == 0 { 0 } else { lines + 512 };
    unsafe {
        ffi::ghostty_terminal_set(
            raw,
            ffi::TerminalOption::SCROLLBACK_MAX_LINES,
            (&max_lines as *const usize).cast(),
        );
    }
}

impl Render {
    /// The cursor's style (DECSCUSR, or the default), visibility (DECTCEM) and blinking, as
    /// of the last `update`.
    pub fn cursor(&self) -> RenderCursor {
        let mut c = RenderCursor {
            style: CursorVisual::BLOCK,
            visible: true,
            blinking: false,
        };
        unsafe {
            use ffi::RenderStateData as D;
            ffi::ghostty_render_state_get(
                self.state,
                D::CURSOR_VISUAL_STYLE,
                (&mut c.style as *mut ffi::CursorStyle::Type).cast(),
            );
            ffi::ghostty_render_state_get(
                self.state,
                D::CURSOR_VISIBLE,
                (&mut c.visible as *mut bool).cast(),
            );
            ffi::ghostty_render_state_get(
                self.state,
                D::CURSOR_BLINKING,
                (&mut c.blinking as *mut bool).cast(),
            );
        }
        c
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_render_state_row_cells_free(self.cells);
            ffi::ghostty_render_state_row_iterator_free(self.rows);
            ffi::ghostty_render_state_free(self.state);
        }
    }
}

pub fn style(r: &GridRef) -> Style {
    let mut s = default_style();
    if unsafe { ffi::ghostty_grid_ref_style(r, &mut s) } != ffi::Result::SUCCESS {
        return default_style();
    }
    s
}

/// Cells the first grapheme cluster of `cps` takes, as the terminal lays it out, and how many
/// codepoints it is. `clustered`: grapheme clustering (mode 2027) is on; off, every codepoint
/// is its own cluster.
pub fn cluster_width(cps: &[u32], clustered: bool) -> (usize, usize) {
    if cps.is_empty() {
        return (0, 0);
    }
    if !clustered {
        return (1, unsafe { ffi::ghostty_unicode_codepoint_width(cps[0]) }
            as usize);
    }
    let mut width = 0u8;
    let n = unsafe { ffi::ghostty_unicode_grapheme_width(cps.as_ptr(), cps.len(), &mut width) };
    (n.max(1), width as usize)
}

/// The full grapheme cluster (base codepoint first) of the cell.
pub fn graphemes(r: &GridRef) -> Vec<char> {
    let mut buf = [0u32; 16];
    let mut len = 0usize;
    let mut res =
        unsafe { ffi::ghostty_grid_ref_graphemes(r, buf.as_mut_ptr(), buf.len(), &mut len) };
    let mut big = Vec::new();
    if res == ffi::Result::OUT_OF_SPACE {
        big = vec![0u32; len];
        res = unsafe { ffi::ghostty_grid_ref_graphemes(r, big.as_mut_ptr(), big.len(), &mut len) };
    }
    if res != ffi::Result::SUCCESS {
        return Vec::new();
    }
    let cps = if big.is_empty() {
        &buf[..len]
    } else {
        &big[..len]
    };
    cps.iter().filter_map(|&c| char::from_u32(c)).collect()
}

pub fn hyperlink(r: &GridRef) -> Option<String> {
    let mut buf = [0u8; 256];
    let mut len = 0usize;
    let mut res =
        unsafe { ffi::ghostty_grid_ref_hyperlink_uri(r, buf.as_mut_ptr(), buf.len(), &mut len) };
    let mut big = Vec::new();
    if res == ffi::Result::OUT_OF_SPACE {
        big = vec![0u8; len];
        res = unsafe {
            ffi::ghostty_grid_ref_hyperlink_uri(r, big.as_mut_ptr(), big.len(), &mut len)
        };
    }
    if res != ffi::Result::SUCCESS || len == 0 {
        return None;
    }
    let bytes = if big.is_empty() {
        &buf[..len]
    } else {
        &big[..len]
    };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(target_arch = "wasm32")]
#[allow(clippy::missing_safety_doc)]
mod kitty_ffi {
    use super::{c_void, ffi, ptr};

    pub unsafe fn ghostty_kitty_graphics_get(
        _: ffi::KittyGraphics,
        _: ffi::KittyGraphicsData::Type,
        _: *mut c_void,
    ) -> ffi::Result::Type {
        ffi::Result::NO_VALUE
    }

    pub unsafe fn ghostty_kitty_graphics_image(
        _: ffi::KittyGraphics,
        _: u32,
    ) -> ffi::KittyGraphicsImage {
        ptr::null_mut()
    }

    pub unsafe fn ghostty_kitty_graphics_image_get(
        _: ffi::KittyGraphicsImage,
        _: ffi::KittyGraphicsImageData::Type,
        _: *mut c_void,
    ) -> ffi::Result::Type {
        ffi::Result::NO_VALUE
    }

    pub unsafe fn ghostty_kitty_graphics_placement_iterator_new(
        _: *const ffi::Allocator,
        out: *mut ffi::KittyGraphicsPlacementIterator,
    ) -> ffi::Result::Type {
        unsafe { *out = ptr::null_mut() };
        ffi::Result::SUCCESS
    }

    pub unsafe fn ghostty_kitty_graphics_placement_iterator_free(
        _: ffi::KittyGraphicsPlacementIterator,
    ) {
    }

    pub unsafe fn ghostty_kitty_graphics_placement_next(
        _: ffi::KittyGraphicsPlacementIterator,
    ) -> bool {
        false
    }

    pub unsafe fn ghostty_kitty_graphics_placement_get(
        _: ffi::KittyGraphicsPlacementIterator,
        _: ffi::KittyGraphicsPlacementData::Type,
        _: *mut c_void,
    ) -> ffi::Result::Type {
        ffi::Result::NO_VALUE
    }

    pub unsafe fn ghostty_kitty_graphics_placement_render_info(
        _: ffi::KittyGraphicsPlacementIterator,
        _: ffi::KittyGraphicsImage,
        _: ffi::Terminal,
        _: *mut ffi::KittyGraphicsPlacementRenderInfo,
    ) -> ffi::Result::Type {
        ffi::Result::NO_VALUE
    }
}

#[cfg(test)]
thread_local! {
    /// Formatting with this output format fails (to test what a failure leaves).
    pub(crate) static FAIL_FORMAT: std::cell::Cell<Option<ffi::FormatterFormat::Type>> =
        const { std::cell::Cell::new(None) };
    /// A concealed copy takes the styled fallback, as when the plain text and the cells do
    /// not line up.
    pub(crate) static FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A piece of the formatter's styled (VT) output.
enum Piece<'a> {
    /// A character of the text, line breaks included.
    Text(char),
    /// SGR with these parameters.
    Sgr(&'a str),
    /// Cursor forward over this many cells (cells never written).
    Forward(usize),
}

/// The pieces of styled output that make its text, in order; other escape sequences (private
/// and intermediate CSI forms, OSC, DCS, SOS, PM, APC, nF and two-character escapes) are
/// skipped whole.
fn pieces(styled: &str, mut each: impl FnMut(Piece)) {
    let mut chars = styled.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '\x1b' {
            each(Piece::Text(c));
            continue;
        }
        match chars.next().map(|(_, c)| c) {
            // CSI: parameters, intermediates, final byte.
            Some('[') => {
                let start = chars.peek().map_or(styled.len(), |&(i, _)| i);
                let mut end = start;
                let mut intermediates = false;
                let mut fin = None;
                for (i, c) in chars.by_ref() {
                    match c {
                        '\x40'..='\x7e' => {
                            fin = Some(c);
                            break;
                        }
                        '\x20'..='\x2f' => intermediates = true,
                        _ => end = i + c.len_utf8(),
                    }
                }
                let params = &styled[start..end.max(start)];
                // Private (`<=>?`-led) and intermediate forms are something else.
                if intermediates || params.starts_with(['<', '=', '>', '?']) {
                    continue;
                }
                match fin {
                    Some('m') => each(Piece::Sgr(params)),
                    Some('C') => {
                        each(Piece::Forward(
                            params.parse::<u16>().unwrap_or(1).max(1) as usize
                        ));
                    }
                    _ => {}
                }
            }
            // OSC (hyperlinks): up to BEL or ST. DCS, SOS, PM, APC: up to ST.
            Some(k @ (']' | 'P' | 'X' | '^' | '_')) => {
                while let Some((_, c)) = chars.next() {
                    if (k == ']' && c == '\x07')
                        || (c == '\x1b' && chars.next_if(|&(_, c)| c == '\\').is_some())
                    {
                        break;
                    }
                }
            }
            // nF (`ESC ( B` and the like): intermediates, then the final byte.
            Some('\x20'..='\x2f') => {
                while chars
                    .next_if(|&(_, c)| ('\x20'..='\x2f').contains(&c))
                    .is_some()
                {}
                chars.next();
            }
            // Two-character escapes.
            _ => {}
        }
    }
}

/// Whether the styled output has a concealed character (SGR 8) other than a blank.
pub(crate) fn conceals(styled: &str) -> bool {
    let mut concealed = false;
    let mut any = false;
    pieces(styled, |p| match p {
        Piece::Sgr(params) => concealed = sgr_conceals(params, concealed),
        // Concealed blanks copy as blanks anyway; other spaces (U+3000) are text.
        Piece::Text(c) if concealed && !matches!(c, ' ' | '\n' | '\r') => any = true,
        _ => {}
    });
    any
}

/// The text of the formatter's styled (VT) output: escape sequences dropped, concealed
/// characters (SGR 8) turned into blanks, trailing blanks of each line trimmed and lines
/// ending in `\n`. The fallback for a concealed copy: the plain text, blanked by the cells, is
/// what the copy should be. `grid`: the concealed cells' characters and widths as laid out, in
/// order; concealed text is blanked as wide as those, and where they do not match it,
/// measured with grapheme clustering (mode 2027) as `clustered` says it is now.
pub(crate) fn styled_text(styled: &str, clustered: bool, grid: &[(Vec<char>, usize)]) -> String {
    let mut out = String::with_capacity(styled.len());
    let mut blanker = Blanker {
        grid,
        clustered,
        hidden: Vec::new(),
    };
    let mut concealed = false;
    // Line breaks and skipped cells after the last written cell (blanks too): kept only when
    // a written cell follows, so rows with none (erased to a background color, say) at the
    // end are not part of the text.
    let mut pending = String::new();
    // A carriage return waiting to see whether a line feed follows.
    let mut cr = false;
    pieces(styled, |p| {
        if cr && !matches!(p, Piece::Text('\n')) {
            pending.push('\r');
        }
        cr = false;
        match p {
            Piece::Text(c @ ('\r' | '\n')) => {
                blanker.flush(&mut out);
                let kept = pending.trim_end_matches(' ').len();
                pending.truncate(kept);
                if pending.is_empty() {
                    let kept = out.trim_end_matches(' ').len();
                    out.truncate(kept);
                }
                if c == '\r' {
                    cr = true;
                } else {
                    // CRLF (the VT formatter's line end) copies as LF.
                    pending.push('\n');
                }
            }
            Piece::Text(c) => {
                if concealed {
                    out.push_str(&pending);
                    pending.clear();
                    blanker.hidden.push(c as u32);
                } else {
                    blanker.flush(&mut out);
                    out.push_str(&pending);
                    pending.clear();
                    out.push(c);
                }
            }
            Piece::Sgr(params) => {
                let now = sgr_conceals(params, concealed);
                if concealed && !now {
                    blanker.flush(&mut out);
                }
                concealed = now;
            }
            Piece::Forward(n) => {
                blanker.flush(&mut out);
                pending.extend(std::iter::repeat_n(' ', n));
            }
        }
    });
    blanker.flush(&mut out);
    let kept = out.trim_end_matches(' ').len();
    out.truncate(kept);
    out
}

/// Concealed characters waiting to be blanked, and the grid's concealed cells to measure them
/// by.
struct Blanker<'a> {
    grid: &'a [(Vec<char>, usize)],
    clustered: bool,
    /// Measured together, so a cluster split by a color change still counts once.
    hidden: Vec<u32>,
}

impl Blanker<'_> {
    /// Append one blank per cell the waiting concealed characters cover, and clear them.
    fn flush(&mut self, out: &mut String) {
        let mut cells = 0;
        let mut rest = &self.hidden[..];
        while !rest.is_empty() {
            match self.grid.split_first() {
                // The grid's next concealed cell (none starts with a blank: a cell whose
                // character is a space is a blank one, marks and all).
                Some(((text, w), next))
                    if rest.len() >= text.len()
                        && text.iter().zip(rest).all(|(&c, &h)| c as u32 == h) =>
                {
                    cells += w;
                    rest = &rest[text.len()..];
                    self.grid = next;
                }
                // An ASCII blank (a written space, or the formatter's own for an empty cell)
                // is one cell and none of the grid's cells with text, with any zero-width
                // marks the terminal put on it (the VT formatter writes them along).
                _ if rest[0] == u32::from(' ') => {
                    let marks = rest[1..]
                        .iter()
                        .take_while(|&&c| unsafe { ffi::ghostty_unicode_codepoint_width(c) } == 0)
                        .count();
                    cells += 1;
                    rest = &rest[1 + marks..];
                }
                _ => {
                    // Off the grid's cells: measure the rest, and leave the grid be.
                    self.grid = &[];
                    let (n, w) = cluster_width(rest, self.clustered);
                    cells += w;
                    rest = &rest[n..];
                }
            }
        }
        out.extend(std::iter::repeat_n(' ', cells));
        self.hidden.clear();
    }
}

/// Whether text is concealed after the SGR sequence with these parameters.
fn sgr_conceals(params: &str, mut concealed: bool) -> bool {
    let mut ps = params.split(';');
    while let Some(p) = ps.next() {
        // Only the first `:` field names the attribute.
        match p.split(':').next().unwrap_or("") {
            "" | "0" | "28" => concealed = false,
            "8" => concealed = true,
            // Extended colors: skip their arguments (`38;5;8` is not SGR 8).
            "38" | "48" | "58" if !p.contains(':') => match ps.next() {
                Some("5") => {
                    ps.next();
                }
                Some("2") => {
                    ps.next();
                    ps.next();
                    ps.next();
                }
                _ => {}
            },
            _ => {}
        }
    }
    concealed
}
