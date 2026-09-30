//! A single terminal: libghostty-vt's terminal plus everything Thurm layers on top of it.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use thurm_config::{Config, Osc52Mode, Theme};
use thurm_proto::{
    self as proto, CaptureOpts, CursorShape, CursorState, Frame, FrameColors, FrameRow,
    ImagePlacement, KeyEvent, MouseButton, MouseEvent, MouseKind, PaneSize, ScrollCmd,
    SearchDirection, SelectionKind, SelectionOp, cell_flags as cf, mods,
};

use crate::filter::{Chunk, StreamFilter};
use crate::keys;
use crate::kitty::{Image, Media, MediaReader};
pub use crate::mode::TermMode;
use crate::osc::{OscEvent, OscState};
use crate::vt::{self, GridRef, KittyPlacement, Vt};

// libghostty-vt mode identifiers.
const M_APP_CURSOR: u16 = vt::mode(1, false);
const M_ORIGIN: u16 = vt::mode(6, false);
const M_WRAP: u16 = vt::mode(7, false);
const M_X10_MOUSE: u16 = vt::mode(9, false);
const M_SHOW_CURSOR: u16 = vt::mode(25, false);
const M_APP_KEYPAD: u16 = vt::mode(66, false);
const M_MOUSE_CLICK: u16 = vt::mode(1000, false);
const M_MOUSE_DRAG: u16 = vt::mode(1002, false);
const M_MOUSE_MOTION: u16 = vt::mode(1003, false);
const M_FOCUS: u16 = vt::mode(1004, false);
const M_UTF8_MOUSE: u16 = vt::mode(1005, false);
const M_SGR_MOUSE: u16 = vt::mode(1006, false);
const M_ALT_SCROLL: u16 = vt::mode(1007, false);
const M_BRACKETED_PASTE: u16 = vt::mode(2004, false);
const M_SYNC: u16 = vt::mode(2026, false);
const M_INSERT: u16 = vt::mode(4, true);
const M_LINE_FEED: u16 = vt::mode(20, true);

/// After a resize erased the prompt: show the next frame once the shell's redraw has been quiet
/// this long...
const PROMPT_SETTLE: Duration = Duration::from_millis(10);
/// ...and at most this long after the resize (a shell that doesn't redraw).
const PROMPT_HOLD: Duration = Duration::from_millis(100);
/// Longest a synchronized update (DEC mode 2026) may hold frames back.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);

/// Engine settings derived from [`Config`].
#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub theme: Theme,
    pub bold_is_bright: bool,
    pub scrollback: usize,
    pub kitty_keyboard: bool,
    pub kitty_graphics: bool,
    pub osc52: Osc52Mode,
    pub image_memory: usize,
    pub word_separators: String,
    /// Cursor before a program sets one (DECSCUSR), and after it resets it.
    pub cursor_style: thurm_config::CursorStyle,
    pub cursor_blink: bool,
    /// Dark appearance (color scheme reports, CSI ? 996 n).
    pub dark: bool,
}

impl EngineConfig {
    /// `dark`: the system appearance, which picks the theme when it follows it.
    pub fn from_config(c: &Config, dark: bool) -> Self {
        Self {
            theme: c.theme_for(dark),
            bold_is_bright: c.colors.bold_is_bright,
            scrollback: c.terminal.scrollback,
            kitty_keyboard: c.terminal.kitty_keyboard,
            kitty_graphics: c.terminal.kitty_graphics,
            osc52: c.terminal.osc52,
            image_memory: c.terminal.image_memory_mib * 1024 * 1024,
            word_separators: c.terminal.word_separators.clone(),
            cursor_style: c.cursor.style,
            cursor_blink: c.cursor.blink,
            dark,
        }
    }

    /// The theme's 256-color palette (the xterm cube and gray ramp past the 16 it defines).
    fn palette(&self) -> [u32; 256] {
        let t = &self.theme;
        std::array::from_fn(|index| match index {
            0..=15 => t.palette[index],
            16..=231 => {
                let i = index - 16;
                let v = |x: usize| if x == 0 { 0 } else { 55 + 40 * x as u32 };
                (v(i / 36) << 16) | (v((i / 6) % 6) << 8) | v(i % 6)
            }
            _ => {
                let g = 8 + 10 * (index as u32 - 232);
                (g << 16) | (g << 8) | g
            }
        })
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self::from_config(&Config::default(), true)
    }
}

/// Things the embedding (the daemon) has to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    /// Bytes to write back to the PTY (terminal responses).
    PtyWrite(Vec<u8>),
    Title(Option<String>),
    Bell,
    ClipboardStore(String),
    /// The program wants the clipboard; answer with [`Terminal::clipboard_reply`].
    ClipboardLoad,
    Notify {
        title: String,
        body: String,
    },
    Cwd(String),
    PromptStart,
    CommandStart,
    CommandFinished(Option<i32>),
    CommandLine(String),
    Progress(Option<proto::Progress>),
    ImageFreed(u32),
}

/// A grid position: `line` 0 is the top of the active area, negative lines are scrollback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Point {
    line: i32,
    col: usize,
}

impl Point {
    fn new(line: i32, col: usize) -> Self {
        Self { line, col }
    }
}

/// A cell color as stored (before resolving against the palette).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Color {
    Default,
    Indexed(u8),
    Rgb(u32),
}

impl Color {
    fn from_style(c: &vt::StyleColor) -> Self {
        match c.tag {
            vt::COLOR_PALETTE => Color::Indexed(unsafe { c.value.palette }),
            vt::COLOR_RGB => Color::Rgb(vt::from_rgb(unsafe { c.value.rgb })),
            _ => Color::Default,
        }
    }
}

const A_BOLD: u16 = 1;
const A_ITALIC: u16 = 1 << 1;
const A_DIM: u16 = 1 << 2;
const A_INVERSE: u16 = 1 << 3;
const A_HIDDEN: u16 = 1 << 4;
const A_STRIKE: u16 = 1 << 5;

/// Underline styles (SGR 4:n).
const UL_NONE: u8 = 0;
const UL_SINGLE: u8 = 1;
const UL_DOUBLE: u8 = 2;
const UL_CURLY: u8 = 3;
const UL_DOTTED: u8 = 4;
const UL_DASHED: u8 = 5;

/// SGR state of a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Pen {
    fg: Color,
    bg: Color,
    ul_color: Option<Color>,
    attrs: u16,
    underline: u8,
}

impl Default for Pen {
    fn default() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            ul_color: None,
            attrs: 0,
            underline: UL_NONE,
        }
    }
}

impl Pen {
    fn from_style(s: &vt::Style) -> Self {
        let mut attrs = 0;
        for (on, a) in [
            (s.bold, A_BOLD),
            (s.italic, A_ITALIC),
            (s.faint, A_DIM),
            (s.inverse, A_INVERSE),
            (s.invisible, A_HIDDEN),
            (s.strikethrough, A_STRIKE),
        ] {
            if on {
                attrs |= a;
            }
        }
        let ul_color = match Color::from_style(&s.underline_color) {
            Color::Default => None,
            c => Some(c),
        };
        Self {
            fg: Color::from_style(&s.fg_color),
            bg: Color::from_style(&s.bg_color),
            ul_color,
            attrs,
            underline: s.underline.clamp(0, 5) as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Width {
    Narrow,
    Wide,
    /// Second half of a wide character.
    Spacer,
    /// End of a line whose wide character didn't fit and moved to the next line.
    LeadingSpacer,
}

#[derive(Clone, Debug)]
struct GCell {
    ch: char,
    /// Combining characters after `ch`.
    zw: Vec<char>,
    wide: Width,
    pen: Pen,
    link: Option<String>,
}

impl GCell {
    fn blank() -> Self {
        Self {
            ch: ' ',
            zw: Vec::new(),
            wide: Width::Narrow,
            pen: Pen::default(),
            link: None,
        }
    }

    fn is_spacer(&self) -> bool {
        matches!(self.wide, Width::Spacer | Width::LeadingSpacer)
    }
}

struct Row {
    cells: Vec<GCell>,
    /// Soft-wraps into the next row.
    wrapped: bool,
}

/// A viewport row from the render state, and its encoding (cells, clusters, hash) for the
/// decoration (selection, search matches, colors) of `Terminal::frame_deco`.
struct CachedRow {
    row: Row,
    encoded: Option<EncodedRow>,
}

/// A row as sent to clients: cells, grapheme clusters by column, and a hash of both.
type EncodedRow = (Vec<proto::Cell>, Vec<(u16, String)>, u64);

/// Rows read on demand, for walks over the grid.
struct Rows<'a> {
    t: &'a Terminal,
    cache: HashMap<i32, Row>,
}

impl<'a> Rows<'a> {
    fn new(t: &'a Terminal) -> Self {
        Self {
            t,
            cache: HashMap::new(),
        }
    }

    fn row(&mut self, line: i32) -> &Row {
        let t = self.t;
        self.cache.entry(line).or_insert_with(|| t.read_row(line))
    }

    fn cell(&mut self, p: Point) -> &GCell {
        let cols = self.t.cols;
        &self.row(p.line).cells[p.col.min(cols - 1)]
    }

    fn top(&self) -> i32 {
        -(self.t.vt.history() as i32)
    }

    fn bottom(&self) -> i32 {
        self.t.rows as i32 - 1
    }

    fn next(&self, p: Point) -> Option<Point> {
        if p.col + 1 < self.t.cols {
            Some(Point::new(p.line, p.col + 1))
        } else if p.line < self.bottom() {
            Some(Point::new(p.line + 1, 0))
        } else {
            None
        }
    }

    fn prev(&self, p: Point) -> Option<Point> {
        if p.col > 0 {
            Some(Point::new(p.line, p.col - 1))
        } else if p.line > self.top() {
            Some(Point::new(p.line - 1, self.t.cols - 1))
        } else {
            None
        }
    }
}

/// Per-client state for incremental frames.
#[derive(Default, Debug)]
pub struct ClientView {
    row_hashes: Vec<u64>,
    cols: u16,
    rows: u16,
    links_sent: usize,
    /// Images the client has, with the generation it has of each.
    images_sent: HashMap<u32, u64>,
    last: Option<FrameMeta>,
    /// Hash of the last `Frame::peek` sent (0 = none).
    peek_hash: u64,
}

#[derive(Debug, PartialEq, Clone)]
struct FrameMeta {
    cursor: CursorState,
    display_offset: u32,
    history_size: u32,
    modes: u32,
    colors: FrameColors,
    images: Vec<ImagePlacement>,
    title: String,
}

impl ClientView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything so the next snapshot is a full frame.
    pub fn invalidate(&mut self) {
        *self = Self {
            images_sent: std::mem::take(&mut self.images_sent),
            ..Self::default()
        };
    }
}

/// Result of [`Terminal::snapshot`].
pub struct Snapshot {
    pub frame: Frame,
    /// Images referenced by the frame that the client hasn't received yet.
    pub new_images: Vec<Image>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseOutcome {
    None,
    /// Bytes were produced for the PTY (returned separately).
    Reported,
    SelectionChanged,
    /// A selection finished (mouse released); carries whether it's non-empty.
    SelectionDone,
}

/// A selection being made with the mouse.
#[derive(Clone, Copy, Debug)]
enum Drag {
    /// libghostty-vt's selection gesture; `rect`: a rectangle.
    Gesture { rect: bool },
    /// Shift-click: moving the end of the existing selection.
    Extend,
}

/// A resolved selection: `start..=end`, or the rectangle they span.
#[derive(Clone, Copy, Debug)]
struct SelRange {
    start: Point,
    end: Point,
    block: bool,
}

impl SelRange {
    fn contains(&self, p: Point) -> bool {
        if self.block {
            p.line >= self.start.line
                && p.line <= self.end.line
                && p.col >= self.start.col
                && p.col <= self.end.col
        } else {
            p >= self.start && p <= self.end
        }
    }
}

/// A search match, `start..=end`.
type Match = (Point, Point);

fn match_contains(m: &Match, p: Point) -> bool {
    p >= m.0 && p <= m.1
}

struct SearchState {
    query: String,
    regex: regex::Regex,
    focused: Option<Match>,
}

/// Resolved colors for a frame.
struct Colors {
    palette: [u32; 256],
    fg: u32,
    bg: u32,
}

pub struct Terminal {
    vt: Vt,
    cols: usize,
    rows: usize,
    filter: StreamFilter,
    osc: OscState,
    /// libghostty-vt's key and mouse encoders.
    enc: keys::Encoders,
    /// libghostty-vt's render state of the viewport, and the rows read from it (with their
    /// encoding for clients, while the decoration they were encoded with holds).
    render: vt::Render,
    frame_rows: Vec<Option<CachedRow>>,
    frame_deco: u64,
    /// Bumped when the engine configuration changes (it affects how rows are encoded).
    cfg_gen: u64,
    /// File / shared memory kitty graphics transmissions, read by Thurm itself.
    media: MediaReader,
    /// Kitty images converted for clients, by id, with their libghostty-vt generation.
    images: HashMap<u32, (u64, Image)>,
    /// libghostty-vt's kitty graphics generation when `images` was last checked.
    kitty_generation: u64,
    /// While `advance_forward` runs: the stream to forward to clients' copies of this pane.
    forward: Option<Vec<u8>>,
    /// DECSTBM scroll region (1-based top, bottom) when not the whole screen, so attach
    /// snapshots can restore it.
    scroll_region: Option<(u16, u16)>,
    /// `$PATH` as the shell last reported it (shell integration).
    shell_path: Option<String>,
    /// Where the current prompt's input starts (OSC 133;B).
    input_start: Option<vt::Tracked>,
    /// Where the current prompt began (OSC 133;A), and whether the shell is still at it (no
    /// command started since, OSC 133;C).
    prompt_start: Option<vt::Tracked>,
    at_prompt: bool,
    /// Whether the shell redraws its prompt on resize. Default to true, as Ghostty does.
    /// An explicit OSC 133 redraw option stays in effect until the shell changes it.
    shell_redraws: bool,
    /// Frames held after a resize erased the prompt, until the shell has redrawn it:
    /// (latest end, current end). Otherwise one frame shows the prompt line empty.
    prompt_hold: Option<(Instant, Instant)>,
    /// The shell has sent the input-start mark after a resize.
    prompt_redraw_ready: bool,
    /// When the current synchronized update (DEC mode 2026) began.
    sync_since: Option<Instant>,
    alt_screen: bool,
    events: Vec<TermEvent>,
    /// Selection selector of a pending OSC 52 clipboard read.
    clipboard_load: Option<char>,
    size: PaneSize,
    cfg: EngineConfig,
    links: HashMap<String, u16>,
    link_list: Vec<String>,
    search: Option<SearchState>,
    title: Option<String>,
    focused: bool,
    /// Selection in progress with the left button held: a libghostty-vt gesture, or (Shift)
    /// moving the end of the existing selection; `rect`: a rectangle (Alt).
    selecting: Option<Drag>,
    /// Last time bytes were fed.
    pub last_output: Instant,
    /// Frame counter bumped whenever the screen may have changed.
    generation: u64,
}

impl Terminal {
    pub fn new(size: PaneSize, cfg: EngineConfig) -> Self {
        let cols = size.cols.max(2);
        let rows = size.rows.max(1);
        let mut vt = Vt::new(cols, rows, cfg.scrollback);
        vt.resize(cols, rows, size.cell_width as u32, size.cell_height as u32);
        let mut t = Self {
            vt,
            cols: cols as usize,
            rows: rows as usize,
            filter: StreamFilter::new(),
            osc: OscState::default(),
            enc: keys::Encoders::new(),
            render: vt::Render::new(),
            frame_rows: Vec::new(),
            frame_deco: 0,
            cfg_gen: 0,
            media: MediaReader::default(),
            images: HashMap::new(),
            kitty_generation: 0,
            forward: None,
            scroll_region: None,
            shell_path: None,
            input_start: None,
            prompt_start: None,
            at_prompt: false,
            shell_redraws: true,
            prompt_hold: None,
            prompt_redraw_ready: false,
            sync_since: None,
            alt_screen: false,
            events: Vec::new(),
            clipboard_load: None,
            size,
            links: HashMap::new(),
            link_list: Vec::new(),
            search: None,
            title: None,
            focused: true,
            selecting: None,
            last_output: Instant::now(),
            generation: 0,
            cfg,
        };
        t.apply_theme();
        t.apply_cursor();
        t.apply_graphics();
        t
    }

    /// Kitty graphics on (with the configured image memory) or off.
    fn apply_graphics(&mut self) {
        let on = self.cfg.kitty_graphics;
        self.vt
            .set_kitty_graphics(if on { self.cfg.image_memory as u64 } else { 0 });
        self.filter.all_apc = !on;
        self.media.set_max_bytes(self.cfg.image_memory);
    }

    /// For a copy of a pane's terminal fed a stream the daemon already filtered (the app's):
    /// file, temporary file and shared memory transmissions are dropped rather than read on
    /// this machine.
    pub fn set_reads_media(&mut self, reads: bool) {
        self.media.set_enabled(reads);
    }

    fn apply_cursor(&mut self) {
        let style = match self.cfg.cursor_style {
            thurm_config::CursorStyle::Block => vt::CursorStyle::BLOCK,
            thurm_config::CursorStyle::Beam => vt::CursorStyle::BAR,
            thurm_config::CursorStyle::Underline => vt::CursorStyle::UNDERLINE,
        };
        self.vt.set_default_cursor(style, self.cfg.cursor_blink);
    }

    fn apply_theme(&mut self) {
        self.vt.effects().dark = self.cfg.dark;
        self.vt.effects().copy_allowed =
            matches!(self.cfg.osc52, Osc52Mode::Copy | Osc52Mode::CopyPaste);
        let t = &self.cfg.theme;
        let palette = self.cfg.palette();
        self.vt
            .set_default_colors(t.foreground, t.background, t.cursor, &palette);
    }

    pub fn size(&self) -> PaneSize {
        self.size
    }

    pub fn mode(&self) -> TermMode {
        let v = &self.vt;
        let mut m = TermMode::empty();
        for (on, bit) in [
            (v.mode(M_SHOW_CURSOR), TermMode::SHOW_CURSOR),
            (v.mode(M_APP_CURSOR), TermMode::APP_CURSOR),
            (v.mode(M_APP_KEYPAD), TermMode::APP_KEYPAD),
            (
                v.mode(M_MOUSE_CLICK) || v.mode(M_X10_MOUSE),
                TermMode::MOUSE_REPORT_CLICK,
            ),
            (v.mode(M_BRACKETED_PASTE), TermMode::BRACKETED_PASTE),
            (v.mode(M_SGR_MOUSE), TermMode::SGR_MOUSE),
            (v.mode(M_MOUSE_MOTION), TermMode::MOUSE_MOTION),
            (v.mode(M_WRAP), TermMode::LINE_WRAP),
            (v.mode(M_LINE_FEED), TermMode::LINE_FEED_NEW_LINE),
            (v.mode(M_ORIGIN), TermMode::ORIGIN),
            (v.mode(M_INSERT), TermMode::INSERT),
            (v.mode(M_FOCUS), TermMode::FOCUS_IN_OUT),
            (v.is_alt_screen(), TermMode::ALT_SCREEN),
            (v.mode(M_MOUSE_DRAG), TermMode::MOUSE_DRAG),
            (v.mode(M_UTF8_MOUSE), TermMode::UTF8_MOUSE),
            (v.mode(M_ALT_SCROLL), TermMode::ALTERNATE_SCROLL),
        ] {
            m.set(bit, on);
        }
        if self.cfg.kitty_keyboard {
            let k = v.kitty_flags();
            for (flag, bit) in [
                (1, TermMode::DISAMBIGUATE_ESC_CODES),
                (2, TermMode::REPORT_EVENT_TYPES),
                (4, TermMode::REPORT_ALTERNATE_KEYS),
                (8, TermMode::REPORT_ALL_KEYS_AS_ESC),
                (16, TermMode::REPORT_ASSOCIATED_TEXT),
            ] {
                m.set(bit, k & flag != 0);
            }
        }
        m
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn set_config(&mut self, cfg: EngineConfig) {
        let graphics = (cfg.kitty_graphics, cfg.image_memory);
        let changed = graphics != (self.cfg.kitty_graphics, self.cfg.image_memory);
        let scheme_changed = cfg.dark != self.cfg.dark;
        self.cfg = cfg;
        // Programs that asked (mode 2031) hear about appearance changes.
        if scheme_changed && self.vt.mode(vt::mode(2031, false)) {
            let n = if self.cfg.dark { 1 } else { 2 };
            self.events
                .push(TermEvent::PtyWrite(format!("\x1b[?997;{n}n").into_bytes()));
        }
        self.cfg_gen += 1;
        self.apply_theme();
        self.apply_cursor();
        if changed {
            self.apply_graphics();
        }
        self.generation += 1;
    }

    /// Feed PTY output.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.last_output = Instant::now();
        self.generation += 1;
        let mut chunks = Vec::new();
        self.filter.feed(bytes, &mut chunks);
        if let Some(region) = self.filter.take_scroll_region() {
            self.scroll_region = region;
        }
        for chunk in chunks {
            match chunk {
                Chunk::Pass(b) => {
                    self.vt.write(&b);
                    self.collect_events();
                    if let Some(f) = &mut self.forward {
                        f.extend_from_slice(&b);
                    }
                }
                Chunk::Apc(body) => {
                    if let Some(direct) = self.handle_apc(&body) {
                        self.vt.write(&direct);
                        self.collect_events();
                        if let Some(f) = &mut self.forward {
                            f.extend_from_slice(&direct);
                        }
                    }
                }
                Chunk::Osc(body) => {
                    self.handle_osc(&body);
                    if let Some(f) = &mut self.forward {
                        f.extend_from_slice(b"\x1b]");
                        f.extend_from_slice(&body);
                        f.push(0x07);
                    }
                }
            }
        }
        self.after_write();
        self.check_images();
        // Settings and title updates can arrive before the prompt is drawn.
        // Start the short timer only after the shell marks the end of its prompt.
        if self.prompt_redraw_ready
            && !bytes.is_empty()
            && let Some((latest, end)) = &mut self.prompt_hold
        {
            *end = (self.last_output + PROMPT_SETTLE).min(*latest);
        }
    }

    /// Bookkeeping after the terminal processed output: synchronized updates and screen
    /// switches.
    fn after_write(&mut self) {
        if self.vt.mode(M_SYNC) {
            self.sync_since.get_or_insert_with(Instant::now);
        } else {
            self.sync_since = None;
        }
        let alt = self.vt.is_alt_screen();
        if alt != self.alt_screen {
            // Selections and matches belong to the screen they were made on.
            self.alt_screen = alt;
            self.vt.set_selection(None);
            if let Some(s) = &mut self.search {
                s.focused = None;
            }
        }
    }

    /// Like [`Terminal::advance`], and returns the bytes to forward to other copies of this
    /// terminal (the apps' local ones): the same stream, except that image transmissions this
    /// terminal consumed (files, shared memory) are rewritten as direct ones.
    pub fn advance_forward(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.forward = Some(Vec::with_capacity(bytes.len()));
        self.advance(bytes);
        self.forward.take().unwrap_or_default()
    }

    /// When frames held back end: a synchronized update (DEC mode 2026) timing out, or the
    /// prompt redraw after a resize settling. `None` when nothing is held.
    pub fn sync_deadline(&self) -> Option<Instant> {
        let sync = self.sync_since.map(|s| s + SYNC_TIMEOUT);
        let prompt = self.prompt_hold.map(|(_, end)| end);
        match (sync, prompt) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Finish whatever held frames and whose deadline expired.
    pub fn flush_sync(&mut self) {
        let now = Instant::now();
        if self.prompt_hold.is_some_and(|(_, end)| end <= now) {
            self.prompt_hold = None;
        }
        if self.sync_since.is_some_and(|s| s + SYNC_TIMEOUT <= now) {
            self.vt.set_mode(M_SYNC, false);
            self.sync_since = None;
        }
        self.generation += 1;
        self.collect_events();
    }

    /// Whether frames should be held back (a synchronized update, or a prompt redraw).
    pub fn in_sync(&self) -> bool {
        self.sync_since.is_some() || self.prompt_hold.is_some()
    }

    pub fn drain_events(&mut self) -> Vec<TermEvent> {
        std::mem::take(&mut self.events)
    }

    fn collect_events(&mut self) {
        let fx = self.vt.effects();
        let pty = std::mem::take(&mut fx.pty);
        let bell = std::mem::take(&mut fx.bell);
        let title_changed = std::mem::take(&mut fx.title_changed);
        let pwd_changed = std::mem::take(&mut fx.pwd_changed);
        let clipboard = std::mem::take(&mut fx.clipboard);
        let notifications = std::mem::take(&mut fx.notifications);
        let progress = std::mem::take(&mut fx.progress);
        for b in pty {
            self.events.push(TermEvent::PtyWrite(b));
        }
        if bell {
            self.events.push(TermEvent::Bell);
        }
        if pwd_changed {
            let pwd = self.vt.pwd();
            let dir = if pwd.starts_with("file:") {
                crate::osc::parse_file_url(&pwd)
            } else {
                (!pwd.is_empty()).then_some(pwd)
            };
            if let Some(dir) = dir {
                self.events.push(TermEvent::Cwd(dir));
            }
        }
        for (title, body) in notifications {
            self.events.push(TermEvent::Notify { title, body });
        }
        for p in progress {
            self.events.push(TermEvent::Progress(p));
        }
        for text in clipboard {
            self.events.push(TermEvent::ClipboardStore(text));
        }
        if title_changed {
            let t = self.vt.title();
            self.title = (!t.is_empty()).then_some(t);
            self.events.push(TermEvent::Title(self.title.clone()));
        }
    }

    pub fn clipboard_reply(&mut self, text: &str) {
        use base64::Engine;
        if let Some(sel) = self.clipboard_load.take() {
            let data = base64::engine::general_purpose::STANDARD.encode(text);
            self.events.push(TermEvent::PtyWrite(
                format!("\x1b]52;{sel};{data}\x07").into_bytes(),
            ));
        }
    }

    pub fn resize(&mut self, size: PaneSize) {
        if size == self.size {
            return;
        }
        self.size = size;
        let cols = size.cols.max(2);
        let rows = size.rows.max(1);
        // libghostty-vt clears a prompt the shell redraws (OSC 133;A;redraw=1); keep showing
        // the old frame until the shell has drawn the new one.
        if self.at_prompt && self.shell_redraws && !self.is_alt_screen() {
            self.clear_prompt_before_resize();
            let latest = Instant::now() + PROMPT_HOLD;
            self.prompt_hold = Some((latest, latest));
            self.prompt_redraw_ready = false;
        }
        self.vt
            .resize(cols, rows, size.cell_width as u32, size.cell_height as u32);
        self.cols = cols as usize;
        self.rows = rows as usize;
        // Resizing resets the scroll region (and ends a synchronized update).
        self.scroll_region = None;
        self.after_write();
        self.collect_events();
        self.generation += 1;
    }

    /// Erase the prompt (from its first row down) before the resize reflows it, the cursor
    /// staying put. libghostty-vt clears it too, but after reflowing, which leaves the rows a
    /// padded right prompt wrapped into. Terminal state decides, so every copy does the same.
    fn clear_prompt_before_resize(&mut self) {
        let history = self.vt.history();
        let cursor = history + self.vt.cursor().1 as usize;
        let Some(start) = Self::tracked_row(&self.prompt_start) else {
            return;
        };
        // On screen, above or at the cursor, and not implausibly tall.
        if start < history || start > cursor || cursor - start > 8 {
            return;
        }
        let origin = self.vt.mode(M_ORIGIN);
        let mut seq = String::from("\x1b7");
        if origin {
            seq.push_str("\x1b[?6l");
        }
        seq.push_str(&format!("\x1b[{};1H\x1b[J", start - history + 1));
        if origin {
            seq.push_str("\x1b[?6h");
        }
        seq.push_str("\x1b8");
        self.vt.write(seq.as_bytes());
    }

    // -----------------------------------------------------------------------------------------
    // Grid access
    // -----------------------------------------------------------------------------------------

    fn cursor_point(&self) -> Point {
        let (x, y) = self.vt.cursor();
        Point::new(y as i32, x as usize)
    }

    /// Reference to the first cell of `line`.
    fn row_ref(&self, line: i32) -> Option<GridRef> {
        if line >= 0 {
            self.vt.active_ref(0, line as u32)
        } else {
            let y = self.vt.history() as i32 + line;
            (y >= 0).then(|| self.vt.screen_ref(0, y as u32)).flatten()
        }
    }

    fn read_row(&self, line: i32) -> Row {
        match self.row_ref(line) {
            Some(r) => self.read_row_at(&r),
            None => Row {
                cells: vec![GCell::blank(); self.cols],
                wrapped: false,
            },
        }
    }

    fn read_row_at(&self, r0: &GridRef) -> Row {
        let mut styles: Vec<(u16, Pen)> = Vec::new();
        let mut cells = Vec::with_capacity(self.cols);
        for x in 0..self.cols {
            let r = GridRef { x: x as u16, ..*r0 };
            let c = vt::cell(&r);
            let pen = if c.has_styling {
                match styles.iter().find(|(id, _)| *id == c.style_id) {
                    Some((_, p)) => *p,
                    None => {
                        let p = Pen::from_style(&vt::style(&r));
                        styles.push((c.style_id, p));
                        p
                    }
                }
            } else {
                Pen::default()
            };
            let graphemes = if c.tag == vt::TAG_GRAPHEME {
                vt::graphemes(&r)
            } else {
                Vec::new()
            };
            let link = if c.has_hyperlink {
                vt::hyperlink(&r)
            } else {
                None
            };
            cells.push(gcell(&c, pen, &graphemes, link));
        }
        Row {
            cells,
            wrapped: vt::row_wrapped(r0),
        }
    }

    /// A viewport row from render-state cells (hyperlinks looked up on the grid).
    fn render_row(&self, vrow: usize, wrapped: bool, cells: Vec<vt::RenderCell>) -> Row {
        let line = vrow as i32 - self.display_offset() as i32;
        let r0 = cells
            .iter()
            .any(|c| c.raw.has_hyperlink)
            .then(|| self.row_ref(line))
            .flatten();
        let mut row: Vec<GCell> = cells
            .iter()
            .enumerate()
            .map(|(x, c)| {
                let pen = c.style.as_ref().map(Pen::from_style).unwrap_or_default();
                let link = r0
                    .filter(|_| c.raw.has_hyperlink)
                    .and_then(|r0| vt::hyperlink(&GridRef { x: x as u16, ..r0 }));
                gcell(&c.raw, pen, &c.graphemes, link)
            })
            .collect();
        row.resize(self.cols, GCell::blank());
        Row {
            cells: row,
            wrapped,
        }
    }

    fn display_offset(&self) -> usize {
        let sb = self.vt.scrollbar();
        sb.total.saturating_sub(sb.len).saturating_sub(sb.offset) as usize
    }

    fn set_display_offset(&mut self, offset: i64) {
        let current = self.display_offset() as i64;
        let target = offset.clamp(0, self.vt.history() as i64);
        if target != current {
            self.vt.scroll_delta((current - target) as isize);
        }
    }

    /// Scrolls the viewport by `lines` (positive: into the scrollback).
    fn scroll_lines(&mut self, lines: i64) {
        self.set_display_offset(self.display_offset() as i64 + lines);
    }

    // -----------------------------------------------------------------------------------------
    // OSC / APC
    // -----------------------------------------------------------------------------------------

    fn handle_osc(&mut self, body: &[u8]) {
        if let Some(rest) = body.strip_prefix(b"52;") {
            self.handle_osc52(rest);
            return;
        }
        let ev = self.osc.parse(body);
        if let Some(OscEvent::PromptStart { redraw: Some(r) }) = ev {
            self.shell_redraws = r;
        }
        // Prompt marks are libghostty-vt's too (prompt and input cells, clearing the prompt on
        // resize); it gets them first, as a prompt start moves to a fresh line.
        if let Some(mark) = self.semantic_mark(body) {
            self.vt.write(&mark);
            self.collect_events();
        }
        let Some(ev) = ev else {
            return;
        };
        match ev {
            OscEvent::Cwd(d) => self.events.push(TermEvent::Cwd(d)),
            OscEvent::Notify { title, body } => self.events.push(TermEvent::Notify { title, body }),
            OscEvent::PromptStart { .. } => {
                self.prompt_start = self.track(Point::new(self.cursor_point().line, 0));
                self.at_prompt = !self.is_alt_screen();
                self.events.push(TermEvent::PromptStart);
            }
            OscEvent::InputStart => {
                self.mark_prompt();
                self.prompt_redraw_ready = true;
            }
            OscEvent::ShellPath(p) => self.shell_path = Some(p),
            OscEvent::CommandStart => {
                self.at_prompt = false;
                self.events.push(TermEvent::CommandStart);
            }
            OscEvent::CommandFinished(code) => self.events.push(TermEvent::CommandFinished(code)),
            OscEvent::CommandLine(c) => self.events.push(TermEvent::CommandLine(c)),
        }
    }

    /// `OSC 52 ; selection ; ?`: paste, as the config allows.
    fn handle_osc52(&mut self, rest: &[u8]) {
        let Some(split) = rest.iter().position(|&b| b == b';') else {
            return;
        };
        let (sel, data) = (&rest[..split], &rest[split + 1..]);
        let sel = sel.first().copied().unwrap_or(b'c');
        if !matches!(sel, b'c' | b'p' | b's') {
            return;
        }
        // Writes are libghostty-vt's (see `collect_events`).
        if data == b"?" && matches!(self.cfg.osc52, Osc52Mode::Paste | Osc52Mode::CopyPaste) {
            self.clipboard_load = Some(sel as char);
            self.events.push(TermEvent::ClipboardLoad);
        }
    }

    /// The shell's `$PATH`, when its integration reported it.
    pub fn shell_path(&self) -> Option<&str> {
        self.shell_path.as_deref()
    }

    /// The command line being typed: text from the prompt's input start (OSC 133;B) to the
    /// cursor, across wrapped lines. None when not at a prompt with shell integration.
    pub fn input_line(&self) -> Option<String> {
        let cursor = self.cursor_point();
        let (x, y) = self.input_start.as_ref()?.screen_point()?;
        let start = Point::new(
            y as i32 - self.vt.history() as i32,
            (x as usize).min(self.cols - 1),
        );
        if start > cursor || cursor.line - start.line > 8 {
            return None;
        }
        Some(text_between(&mut Rows::new(self), start, cursor, true))
    }

    /// Remember where the prompt's input starts (it follows the text as it scrolls and
    /// reflows).
    fn mark_prompt(&mut self) {
        self.input_start = self.track(self.cursor_point());
    }

    /// OSC 133 / 633 prompt marks as libghostty-vt gets them: VS Code's as their OSC 133
    /// equivalents, with the current redraw setting on prompt-start marks.
    fn semantic_mark(&self, body: &[u8]) -> Option<Vec<u8>> {
        let (num, rest) = body.split_at(memchr::memchr(b';', body)?);
        let rest = &rest[1..];
        let kind = *rest.first()?;
        let mut mark = match (num, kind) {
            (b"133", _) => rest.to_vec(),
            (b"633", b'A' | b'B' | b'C' | b'D') => rest.to_vec(),
            _ => return None,
        };
        if kind == b'A' && !rest.windows(7).any(|w| w == b"redraw=") {
            mark.extend_from_slice(if self.shell_redraws {
                b";redraw=1"
            } else {
                b";redraw=0"
            });
        }
        let mut out = b"\x1b]133;".to_vec();
        out.append(&mut mark);
        out.push(0x07);
        Some(out)
    }

    /// A kitty graphics transmission taken out of the stream (from a file or shared memory,
    /// or any APC with graphics off): what to feed instead.
    fn handle_apc(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        if !self.cfg.kitty_graphics {
            return None;
        }
        match self.media.handle(body) {
            Media::Direct(bytes) => Some(bytes),
            Media::Reply(reply) => {
                self.events.push(TermEvent::PtyWrite(reply));
                None
            }
            Media::Wait | Media::Drop => None,
        }
    }

    /// After kitty graphics changed: forget converted images that were replaced, and report
    /// the ones that are gone.
    fn check_images(&mut self) {
        let generation = self.vt.kitty_generation();
        if generation == self.kitty_generation {
            return;
        }
        self.kitty_generation = generation;
        self.generation += 1;
        let vt = &self.vt;
        let mut freed = Vec::new();
        self.images
            .retain(|&id, (g, _)| match vt.kitty_image_generation(id) {
                Some(now) => now == *g,
                None => {
                    freed.push(id);
                    false
                }
            });
        for id in freed {
            self.events.push(TermEvent::ImageFreed(id));
        }
    }

    /// Kitty image `id` for clients, with its generation.
    fn client_image(&mut self, id: u32) -> Option<(u64, Image)> {
        let generation = self.vt.kitty_image_generation(id)?;
        if let Some((g, img)) = self.images.get(&id)
            && *g == generation
        {
            return Some((*g, img.clone()));
        }
        let (width, height, rgba) = self.vt.kitty_image(id)?;
        let img = Image {
            id,
            width,
            height,
            rgba: std::sync::Arc::new(rgba),
        };
        self.images.insert(id, (generation, img.clone()));
        Some((generation, img))
    }

    pub fn image(&mut self, id: u32) -> Option<Image> {
        self.client_image(id).map(|(_, img)| img)
    }

    // -----------------------------------------------------------------------------------------
    // Colors
    // -----------------------------------------------------------------------------------------

    fn colors(&self) -> Colors {
        let t = &self.cfg.theme;
        Colors {
            palette: self.vt.palette(),
            fg: self.vt.fg().unwrap_or(t.foreground),
            bg: self.vt.bg().unwrap_or(t.background),
        }
    }

    fn color(c: Color, default: u32, colors: &Colors) -> u32 {
        match c {
            Color::Default => default,
            Color::Indexed(i) => colors.palette[i as usize],
            Color::Rgb(v) => v,
        }
    }

    fn fg_color(&self, c: Color, attrs: u16, colors: &Colors) -> u32 {
        let bold = attrs & A_BOLD != 0;
        let v = match c {
            Color::Indexed(i) if bold && self.cfg.bold_is_bright && i < 8 => {
                colors.palette[i as usize + 8]
            }
            other => Self::color(other, colors.fg, colors),
        };
        if attrs & A_DIM != 0 { dim(v) } else { v }
    }

    fn frame_colors(&self, colors: &Colors) -> FrameColors {
        let t = &self.cfg.theme;
        FrameColors {
            foreground: colors.fg,
            background: colors.bg,
            cursor: self.vt.cursor_color().unwrap_or(t.cursor),
            cursor_text: t.cursor_text,
            selection_fg: t.selection_foreground,
            selection_bg: t.selection_background,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Frames
    // -----------------------------------------------------------------------------------------

    fn link_id(&mut self, uri: &str) -> u16 {
        if let Some(&id) = self.links.get(uri) {
            return id;
        }
        if self.link_list.len() >= u16::MAX as usize - 1 {
            return 0;
        }
        self.link_list.push(uri.to_owned());
        let id = self.link_list.len() as u16;
        self.links.insert(uri.to_owned(), id);
        id
    }

    /// Build a frame for a client, containing only what changed since the last frame that
    /// client received. Returns `None` when nothing changed.
    pub fn snapshot(&mut self, pane: proto::PaneId, view: &mut ClientView) -> Option<Snapshot> {
        if self.prompt_hold.is_some() {
            return None;
        }
        let cols = self.cols;
        let rows = self.rows;
        let full = view.cols as usize != cols || view.rows as usize != rows || view.last.is_none();
        if full {
            view.row_hashes = vec![0; rows];
            view.cols = cols as u16;
            view.rows = rows as u16;
        }

        let display_offset = self.display_offset();
        let history = self.vt.history();
        let selection = self.selection_range();
        let matches = self.visible_matches();
        let focused_match = self.search.as_ref().and_then(|s| s.focused);
        let colors = self.colors();

        // Scrollback moved: rows the client already has move with it, so only newly exposed
        // rows are sent (instead of the whole screen for every line scrolled).
        let mut shift = 0i32;
        if !full && let Some(last) = &view.last {
            let d = display_offset as i64 - last.display_offset as i64;
            if d != 0 && d.unsigned_abs() < rows as u64 {
                let old = std::mem::take(&mut view.row_hashes);
                view.row_hashes = (0..rows as i64)
                    .map(|r| {
                        usize::try_from(r - d)
                            .ok()
                            .and_then(|s| old.get(s).copied())
                            .unwrap_or(0)
                    })
                    .collect();
                shift = d as i32;
            }
        }

        // Rows from libghostty-vt's render state: only the ones it rebuilt are read again, and
        // only those (or all, when the decoration changed) are encoded again.
        let deco = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (
                selection.map(|s| (s.start, s.end, s.block)),
                &matches,
                focused_match,
            )
                .hash(&mut h);
            (colors.palette, colors.fg, colors.bg).hash(&mut h);
            (self.cfg_gen, display_offset, cols, rows).hash(&mut h);
            h.finish()
        };
        if self.frame_deco != deco {
            self.frame_deco = deco;
            for r in self.frame_rows.iter_mut().flatten() {
                r.encoded = None;
            }
        }
        let render_full = self.render.update(&self.vt) == vt::RenderDirty::FULL;
        if render_full || self.frame_rows.len() != rows {
            self.frame_rows = (0..rows).map(|_| None).collect();
        }
        let mut fresh = Vec::new();
        let cached = &self.frame_rows;
        self.render.rows(
            |y, dirty| dirty || cached.get(y).is_none_or(Option::is_none),
            |y, _, wrapped, cells| {
                if let Some(cells) = cells {
                    fresh.push((y, wrapped, cells));
                }
            },
        );
        for (y, wrapped, cells) in fresh {
            if y < rows {
                let row = self.render_row(y, wrapped, cells);
                self.frame_rows[y] = Some(CachedRow { row, encoded: None });
            }
        }
        let mut lines = Vec::new();
        for vrow in 0..rows {
            let line = vrow as i32 - display_offset as i32;
            let mut cached = self.frame_rows[vrow].take().unwrap_or_else(|| CachedRow {
                row: self.read_row(line),
                encoded: None,
            });
            if cached.encoded.is_none() {
                cached.encoded = Some(self.frame_row(
                    &cached.row,
                    line,
                    &colors,
                    selection.as_ref(),
                    &matches,
                    focused_match.as_ref(),
                ));
            }
            // Only rows the client doesn't have yet are cloned out of the cache.
            if let Some((cells, clusters, hash)) = &cached.encoded
                && (full || view.row_hashes[vrow] != *hash)
            {
                view.row_hashes[vrow] = *hash;
                lines.push(FrameRow {
                    row: vrow as u16,
                    cells: cells.clone(),
                    clusters: clusters.clone(),
                });
            }
            self.frame_rows[vrow] = Some(cached);
        }

        // Borrowed from the cache for the cursor and images below (put back before returning).
        let cache = std::mem::take(&mut self.frame_rows);
        let screen: Vec<&Row> = cache
            .iter()
            .map(|c| &c.as_ref().expect("filled above").row)
            .collect();

        let has_peek = display_offset < history;
        let mut peek = None;
        if has_peek {
            let line = -(display_offset as i32) - 1;
            let row = self.read_row(line);
            let (cells, clusters, hash) = self.frame_row(
                &row,
                line,
                &colors,
                selection.as_ref(),
                &matches,
                focused_match.as_ref(),
            );
            if full || view.peek_hash != hash {
                view.peek_hash = hash;
                peek = Some(FrameRow {
                    row: u16::MAX,
                    cells,
                    clusters,
                });
            }
        } else {
            view.peek_hash = 0;
        }

        // Cursor.
        let mode = self.mode();
        let mut cpoint = self.cursor_point();
        let crow = cpoint.line + display_offset as i32;
        let cursor_row = (0..rows as i32)
            .contains(&crow)
            .then(|| &screen[crow as usize]);
        if cursor_row.is_some_and(|r| r.cells[cpoint.col].wide == Width::Spacer) && cpoint.col > 0 {
            cpoint.col -= 1;
        }
        let rc = self.render.cursor();
        let blinking = rc.blinking;
        let mut shape = if rc.visible {
            match rc.style {
                vt::CursorVisual::BAR => CursorShape::Beam,
                vt::CursorVisual::UNDERLINE => CursorShape::Underline,
                vt::CursorVisual::BLOCK_HOLLOW => CursorShape::HollowBlock,
                _ => CursorShape::Block,
            }
        } else {
            CursorShape::Hidden
        };
        if crow < 0 || crow >= rows as i32 {
            shape = CursorShape::Hidden;
        }
        let cursor = CursorState {
            col: cpoint.col as u16,
            row: crow.clamp(0, rows as i32 - 1) as u16,
            shape,
            blinking,
            wide: cursor_row.is_some_and(|r| r.cells[cpoint.col].wide == Width::Wide),
        };

        // Images.
        let mut images = Vec::new();
        let mut new_images = Vec::new();
        if self.kitty_generation != 0 {
            let placements = self.vt.kitty_placements();
            images = self.placeholder_images(&screen, &placements);
            for p in &placements {
                let Some(r) = p.render else { continue };
                images.push(ImagePlacement {
                    image: p.image,
                    placement: p.placement,
                    row: r.viewport_row,
                    col: r.viewport_col,
                    x_offset: p.x_offset,
                    y_offset: p.y_offset,
                    src_x: r.source_x,
                    src_y: r.source_y,
                    src_w: r.source_width,
                    src_h: r.source_height,
                    cols: r.grid_cols,
                    rows: r.grid_rows,
                    z: p.z,
                    dst_w: r.pixel_width,
                    dst_h: r.pixel_height,
                });
            }
            // A stable order, so unchanged frames compare equal.
            images.sort_by_key(|p| (p.z, p.image, p.placement, p.row, p.col));
            let mut ids: Vec<u32> = images.iter().map(|p| p.image).collect();
            ids.dedup();
            for id in ids {
                if let Some((g, img)) = self.client_image(id)
                    && view.images_sent.insert(id, g) != Some(g)
                {
                    new_images.push(img);
                }
            }
        }

        drop(screen);
        self.frame_rows = cache;

        let meta = FrameMeta {
            cursor,
            display_offset: display_offset as u32,
            history_size: history as u32,
            modes: mode.bits(),
            colors: self.frame_colors(&colors),
            images,
            title: self.title.clone().unwrap_or_default(),
        };
        let links = if self.link_list.len() > view.links_sent || full {
            view.links_sent = self.link_list.len();
            self.link_list.clone()
        } else {
            Vec::new()
        };
        if !full
            && lines.is_empty()
            && peek.is_none()
            && links.is_empty()
            && view.last.as_ref() == Some(&meta)
        {
            return None;
        }
        view.last = Some(meta.clone());
        Some(Snapshot {
            frame: Frame {
                pane,
                cols: cols as u16,
                rows: rows as u16,
                full,
                shift,
                lines,
                peek,
                has_peek,
                cursor: meta.cursor,
                display_offset: meta.display_offset,
                history_size: meta.history_size,
                modes: meta.modes,
                colors: meta.colors,
                links,
                images: meta.images,
                title: meta.title,
            },
            new_images,
        })
    }

    /// Image slices for the Unicode placeholders on screen (libghostty-vt stores virtual
    /// placements; drawing them is up to the renderer).
    fn placeholder_images(
        &self,
        screen: &[&Row],
        placements: &[KittyPlacement],
    ) -> Vec<ImagePlacement> {
        use crate::placeholder::{PLACEHOLDER, decode_row};
        let mut slices = Vec::new();
        if !placements.iter().any(|p| p.is_virtual) {
            return slices;
        }
        let cols = self.cols;
        let cw = self.size.cell_width.max(1) as f64;
        let ch = self.size.cell_height.max(1) as f64;
        let color_id = |c: &Color| -> u32 {
            match c {
                Color::Rgb(v) => *v,
                Color::Indexed(i) => *i as u32,
                Color::Default => 0,
            }
        };
        for (vrow, row) in screen.iter().enumerate() {
            let raw: Vec<Option<(u32, u32, Vec<char>)>> = row
                .cells
                .iter()
                .map(|cell| {
                    (cell.ch == PLACEHOLDER).then(|| {
                        let ul = cell.pen.ul_color.map(|u| color_id(&u)).unwrap_or(0);
                        (color_id(&cell.pen.fg), ul, cell.zw.clone())
                    })
                })
                .collect();
            if raw.iter().all(Option::is_none) {
                continue;
            }
            let decoded = decode_row(&raw);
            // Runs of consecutive columns of the same placement row.
            let mut c = 0;
            while c < cols {
                let Some(first) = decoded[c] else {
                    c += 1;
                    continue;
                };
                let mut end = c + 1;
                while end < cols
                    && decoded[end].is_some_and(|d| {
                        d.image == first.image
                            && d.placement == first.placement
                            && d.row == first.row
                            && d.col == first.col + (end - c) as u32
                    })
                {
                    end += 1;
                }
                let found = placements.iter().find(|p| {
                    p.is_virtual
                        && p.image == first.image
                        && (first.placement == 0 || p.placement == first.placement)
                });
                if let Some(p) = found
                    && let Some((iw, ih)) = self.vt.kitty_image_size(p.image)
                {
                    let (src_x, src_y, src_w, src_h) = p.source;
                    let src_w = if src_w == 0 {
                        iw.saturating_sub(src_x)
                    } else {
                        src_w
                    };
                    let src_h = if src_h == 0 {
                        ih.saturating_sub(src_y)
                    } else {
                        src_h
                    };
                    let (sw, sh) = (src_w.max(1) as f64, src_h.max(1) as f64);
                    let bcols = if p.cols > 0 {
                        p.cols as f64
                    } else {
                        (sw / cw).ceil()
                    };
                    let brows = if p.rows > 0 {
                        p.rows as f64
                    } else {
                        (sh / ch).ceil()
                    };
                    // Fit into the placement's box keeping the aspect ratio, centered.
                    let (bw, bh) = (bcols * cw, brows * ch);
                    let scale = (bw / sw).min(bh / sh);
                    let (fw, fh) = (sw * scale, sh * scale);
                    let (ox, oy) = ((bw - fw) / 2.0, (bh - fh) / 2.0);
                    // This run's cells in box pixels, clipped to the fitted image.
                    let x0 = (first.col as f64 * cw).max(ox);
                    let x1 = ((first.col as f64 + (end - c) as f64) * cw).min(ox + fw);
                    let y0 = (first.row as f64 * ch).max(oy);
                    let y1 = ((first.row as f64 + 1.0) * ch).min(oy + fh);
                    if x1 > x0 && y1 > y0 {
                        slices.push(ImagePlacement {
                            image: p.image,
                            placement: p.placement,
                            row: vrow as i32,
                            col: c as i32,
                            x_offset: (x0 - first.col as f64 * cw).round() as u32,
                            y_offset: (y0 - first.row as f64 * ch).round() as u32,
                            src_x: src_x + ((x0 - ox) / scale) as u32,
                            src_y: src_y + ((y0 - oy) / scale) as u32,
                            src_w: (((x1 - x0) / scale).round() as u32).max(1),
                            src_h: (((y1 - y0) / scale).round() as u32).max(1),
                            cols: 0,
                            rows: 0,
                            z: p.z,
                            dst_w: (x1 - x0).round() as u32,
                            dst_h: (y1 - y0).round() as u32,
                        });
                    }
                }
                c = end;
            }
        }
        slices
    }

    /// Cells, clusters and change hash of one grid line as sent to clients.
    fn frame_row(
        &mut self,
        row: &Row,
        line: i32,
        colors: &Colors,
        selection: Option<&SelRange>,
        matches: &[Match],
        focused_match: Option<&Match>,
    ) -> EncodedRow {
        let mut cells = Vec::with_capacity(row.cells.len());
        let mut clusters = Vec::new();
        for (col, cell) in row.cells.iter().enumerate() {
            let point = Point::new(line, col);
            let mut out = self.convert_cell(cell, colors);
            if cell.ch == crate::placeholder::PLACEHOLDER {
                // An image slice is drawn here instead (see `placeholder_images`).
                out.ch = ' ';
                cells.push(out);
                continue;
            }
            if selection.is_some_and(|s| s.contains(point)) {
                out.flags |= cf::SELECTED;
            }
            if matches.iter().any(|m| match_contains(m, point)) {
                out.flags |= cf::SEARCH_MATCH;
                if focused_match.is_some_and(|m| match_contains(m, point)) {
                    out.flags |= cf::SEARCH_FOCUS;
                }
            }
            if let Some(link) = &cell.link {
                out.link = self.link_id(link);
            }
            if !cell.zw.is_empty() {
                let mut s = String::new();
                s.push(cell.ch);
                s.extend(cell.zw.iter());
                clusters.push((col as u16, s));
            }
            cells.push(out);
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        cells.hash(&mut h);
        clusters.hash(&mut h);
        (cells, clusters, h.finish() | 1)
    }

    fn convert_cell(&self, cell: &GCell, colors: &Colors) -> proto::Cell {
        let pen = &cell.pen;
        let mut fg = self.fg_color(pen.fg, pen.attrs, colors);
        let mut bg = Self::color(pen.bg, colors.bg, colors);
        let mut flags = 0u16;
        if pen.attrs & A_INVERSE != 0 {
            std::mem::swap(&mut fg, &mut bg);
        } else if pen.bg == Color::Default {
            flags |= cf::DEFAULT_BG;
        }
        for (a, b) in [
            (A_BOLD, cf::BOLD),
            (A_ITALIC, cf::ITALIC),
            (A_STRIKE, cf::STRIKEOUT),
            (A_HIDDEN, cf::HIDDEN),
            (A_DIM, cf::DIM),
        ] {
            if pen.attrs & a != 0 {
                flags |= b;
            }
        }
        flags |= match pen.underline {
            UL_SINGLE => cf::UNDERLINE,
            UL_DOUBLE => cf::DOUBLE_UNDERLINE,
            UL_CURLY => cf::UNDERCURL,
            UL_DOTTED => cf::DOTTED_UNDERLINE,
            UL_DASHED => cf::DASHED_UNDERLINE,
            _ => 0,
        };
        flags |= match cell.wide {
            Width::Wide => cf::WIDE,
            Width::Spacer | Width::LeadingSpacer => cf::WIDE_SPACER,
            Width::Narrow => 0,
        };
        let ul = pen
            .ul_color
            .map(|c| Self::color(c, colors.fg, colors))
            .unwrap_or(proto::NO_COLOR);
        proto::Cell {
            ch: cell.ch,
            fg,
            bg,
            ul,
            flags,
            link: 0,
        }
    }

    // -----------------------------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------------------------

    /// Encode a key for the PTY. Scrolls back to the bottom when bytes are produced.
    pub fn key(&mut self, ev: &KeyEvent) -> Vec<u8> {
        let bytes = self.enc.key(&self.vt, ev);
        if !bytes.is_empty() {
            self.scroll_to_bottom();
        }
        bytes
    }

    pub fn paste(&mut self, text: &str) -> Vec<u8> {
        self.scroll_to_bottom();
        keys::paste(text, self.mode().contains(TermMode::BRACKETED_PASTE))
    }

    pub fn focus(&mut self, focused: bool) -> Option<Vec<u8>> {
        self.focused = focused;
        self.generation += 1;
        self.mode()
            .contains(TermMode::FOCUS_IN_OUT)
            .then(|| keys::focus(focused))
    }

    /// Grid geometry for mouse positions.
    fn grid(&self) -> keys::Grid {
        keys::Grid {
            cols: self.cols as u16,
            rows: self.rows as u16,
            cell_width: self.size.cell_width as u32,
            cell_height: self.size.cell_height as u32,
        }
    }

    fn scroll_to_bottom(&mut self) {
        if self.display_offset() != 0 {
            self.vt.scroll_bottom();
            self.generation += 1;
        }
    }

    fn viewport_point(&self, col: u16, row: u16) -> Point {
        let offset = self.display_offset() as i32;
        let line = (row as i32).min(self.rows as i32 - 1) - offset;
        let col = (col as usize).min(self.cols - 1);
        Point::new(line, col)
    }

    /// Mouse press/release/motion: reported to the application when it grabbed the mouse
    /// (unless Shift is held), otherwise used for selection.
    pub fn mouse(&mut self, ev: &MouseEvent) -> (MouseOutcome, Vec<u8>) {
        let mode = self.mode();
        if mode.intersects(TermMode::MOUSE_MODE) && ev.mods & mods::SHIFT == 0 {
            return match self.enc.mouse(&self.vt, ev, self.grid()) {
                Some(b) => (MouseOutcome::Reported, b),
                None => (MouseOutcome::None, Vec::new()),
            };
        }
        let Some(at) = self.pointer(ev.col, ev.row, ev.right_half, Some((ev.x, ev.y))) else {
            return (MouseOutcome::None, Vec::new());
        };
        match (ev.kind, ev.button) {
            (MouseKind::Press, MouseButton::Left) => {
                if ev.mods & mods::SHIFT != 0
                    && let Some(sel) = self.vt.selection()
                {
                    self.vt.set_selection(Some(vt::Sel {
                        end: at.cell,
                        ..sel
                    }));
                    self.selecting = Some(Drag::Extend);
                } else {
                    let by = match ev.clicks {
                        2 => vt::SelectBy::WORD,
                        3.. => vt::SelectBy::LINE,
                        _ => vt::SelectBy::CELL,
                    };
                    let rect = ev.clicks < 2 && ev.mods & mods::ALT != 0;
                    self.press_selection(at, by);
                    self.selecting = Some(Drag::Gesture { rect });
                }
                self.generation += 1;
                (MouseOutcome::SelectionChanged, Vec::new())
            }
            (MouseKind::Move, _) if self.selecting.is_some() => {
                self.drag_selection(at);
                self.generation += 1;
                (MouseOutcome::SelectionChanged, Vec::new())
            }
            (MouseKind::Release, MouseButton::Left) if self.selecting.is_some() => {
                self.selecting = None;
                self.vt.gesture_release(at);
                if self.vt.selection().is_none() {
                    self.generation += 1;
                    return (MouseOutcome::SelectionChanged, Vec::new());
                }
                (MouseOutcome::SelectionDone, Vec::new())
            }
            _ => (MouseOutcome::None, Vec::new()),
        }
    }

    /// Wheel scrolling. Returns bytes for the PTY when the app consumes the wheel.
    pub fn wheel(&mut self, lines: i32, col: u16, row: u16, m: u8) -> Vec<u8> {
        let grabbed = self.mode().intersects(TermMode::MOUSE_MODE) && m & mods::SHIFT == 0;
        if (grabbed || self.is_alt_screen())
            && let Some(b) = self.enc.wheel(&self.vt, lines, col, row, m, self.grid())
        {
            return b;
        }
        self.scroll_lines(lines as i64);
        self.generation += 1;
        Vec::new()
    }

    pub fn scroll(&mut self, cmd: ScrollCmd) {
        let page = self.rows as i64;
        match cmd {
            ScrollCmd::Lines(n) => self.scroll_lines(n as i64),
            ScrollCmd::PageUp => self.scroll_lines(page),
            ScrollCmd::PageDown => self.scroll_lines(-page),
            ScrollCmd::Top => self.vt.scroll_top(),
            ScrollCmd::Bottom => self.vt.scroll_bottom(),
            ScrollCmd::PrevPrompt => self.jump_prompt(true),
            ScrollCmd::NextPrompt => self.jump_prompt(false),
            ScrollCmd::Offset(n) => self.set_display_offset(n as i64),
        }
        self.generation += 1;
    }

    /// Lines where prompts begin (libghostty-vt's OSC 133 prompt rows).
    fn prompt_lines(&self) -> Vec<i32> {
        let top = -(self.vt.history() as i32);
        (top..self.rows as i32)
            .filter(|&line| {
                self.row_ref(line)
                    .is_some_and(|r| vt::row_prompt(&r) == vt::RowPrompt::PROMPT)
            })
            .collect()
    }

    /// The last finished command (its prompt line and output, up to the current prompt) as
    /// plain text, keeping the last `max_lines` lines. Needs shell integration's prompt marks.
    pub fn last_command(&self, max_lines: usize) -> Option<String> {
        let prompts = self.prompt_lines();
        let [.., start, end] = prompts[..] else {
            return None;
        };
        let top = start.max(end - max_lines as i32);
        let lines: Vec<String> = self
            .export_lines(top, end - 1, false)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        let text = lines.join("\n");
        let text = text.trim_end();
        (!text.is_empty()).then(|| text.to_owned())
    }

    /// Row (from the top of the history) of a tracked position.
    fn tracked_row(t: &Option<vt::Tracked>) -> Option<usize> {
        t.as_ref()?.screen_point().map(|(_, y)| y as usize)
    }

    fn jump_prompt(&mut self, up: bool) {
        let offset = self.display_offset() as i32;
        let top_visible = -offset;
        let prompts = self.prompt_lines();
        let target = if up {
            prompts.into_iter().rev().find(|&l| l < top_visible)
        } else {
            prompts.into_iter().find(|&l| l > top_visible)
        };
        if let Some(line) = target {
            self.set_display_offset((-line).max(0) as i64);
        } else if !up {
            self.vt.scroll_bottom();
        }
    }

    // -----------------------------------------------------------------------------------------
    // Selection & search
    // -----------------------------------------------------------------------------------------

    fn track(&self, p: Point) -> Option<vt::Tracked> {
        let y = self.vt.history() as i32 + p.line;
        (y >= 0)
            .then(|| self.vt.track(p.col as u16, y as u32))
            .flatten()
    }

    /// The pointer for a selection gesture at a viewport cell: its pixel position is `px`
    /// when that is inside the cell, else a point on the half `right_half` says.
    fn pointer(
        &self,
        col: u16,
        row: u16,
        right_half: bool,
        px: Option<(u32, u32)>,
    ) -> Option<vt::Pointer> {
        let p = self.viewport_point(col, row);
        let y = self.vt.history() as i32 + p.line;
        if y < 0 {
            return None;
        }
        let (cw, ch) = (
            self.size.cell_width.max(1) as u32,
            self.size.cell_height.max(1) as u32,
        );
        let (x0, y0) = (
            p.col as u32 * cw,
            (row as u32).min(self.rows as u32 - 1) * ch,
        );
        let x = match px {
            Some((x, _)) if (x0..x0 + cw).contains(&x) => x,
            _ => x0 + if right_half { cw * 3 / 4 } else { cw / 4 },
        };
        let py = match px {
            Some((_, y)) if (y0..y0 + ch).contains(&y) => y,
            _ => y0 + ch / 2,
        };
        Some(vt::Pointer {
            cell: (p.col as u16, y as u32),
            px: (x as f64, py as f64),
        })
    }

    fn word_boundaries(&self) -> Vec<u32> {
        self.cfg.word_separators.chars().map(u32::from).collect()
    }

    /// Start a selection gesture (libghostty-vt's: a word or line right away, cells once
    /// dragged).
    fn press_selection(&mut self, at: vt::Pointer, by: vt::SelectBy::Type) {
        let words = self.word_boundaries();
        let sel = self.vt.gesture_press(at, by, &words);
        self.vt.set_selection(sel);
    }

    fn drag_selection(&mut self, at: vt::Pointer) {
        match self.selecting {
            Some(Drag::Extend) => {
                if let Some(sel) = self.vt.selection() {
                    self.vt.set_selection(Some(vt::Sel {
                        end: at.cell,
                        ..sel
                    }));
                }
            }
            Some(Drag::Gesture { rect }) => {
                let geometry = vt::SelectGeometry {
                    columns: self.cols as u32,
                    cell_width: self.size.cell_width.max(1) as u32,
                    padding_left: 0,
                    screen_height: self.rows as u32 * self.size.cell_height.max(1) as u32,
                };
                let words = self.word_boundaries();
                if let Some(sel) = self.vt.gesture_drag(at, geometry, rect, &words) {
                    self.vt.set_selection(Some(sel));
                }
            }
            None => {}
        }
    }

    /// The selection in grid points, ordered.
    fn selection_range(&self) -> Option<SelRange> {
        let sel = self.vt.selection()?;
        let history = self.vt.history() as i32;
        let point = |(x, y): (u16, u32)| Point::new(y as i32 - history, x as usize);
        let (a, b) = (point(sel.start), point(sel.end));
        Some(if sel.rectangle {
            SelRange {
                start: Point::new(a.line.min(b.line), a.col.min(b.col)),
                end: Point::new(a.line.max(b.line), a.col.max(b.col)),
                block: true,
            }
        } else {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            SelRange {
                start,
                end,
                block: false,
            }
        })
    }

    pub fn selection(&mut self, op: SelectionOp) {
        match op {
            SelectionOp::Start {
                col,
                row,
                right_half,
                kind,
            } => {
                let by = match kind {
                    SelectionKind::Simple | SelectionKind::Block => vt::SelectBy::CELL,
                    SelectionKind::Word => vt::SelectBy::WORD,
                    SelectionKind::Line => vt::SelectBy::LINE,
                };
                if let Some(at) = self.pointer(col, row, right_half, None) {
                    self.press_selection(at, by);
                    self.selecting = Some(Drag::Gesture {
                        rect: kind == SelectionKind::Block,
                    });
                }
            }
            SelectionOp::Update {
                col,
                row,
                right_half,
            } => {
                if let Some(at) = self.pointer(col, row, right_half, None) {
                    self.drag_selection(at);
                }
            }
            SelectionOp::Clear => {
                self.vt.set_selection(None);
                self.selecting = None;
            }
            SelectionOp::SelectAll => self.vt.select_all(),
        }
        self.generation += 1;
    }

    pub fn selection_text(&self) -> String {
        self.vt.selection_text()
    }

    /// Search towards older (`Backward`) or newer output; returns whether a match was found.
    pub fn search(&mut self, query: Option<&str>, dir: SearchDirection) -> bool {
        self.generation += 1;
        let Some(query) = query.filter(|q| !q.is_empty()) else {
            self.search = None;
            return false;
        };
        // Smart case: case-insensitive unless the query has uppercase letters.
        let has_upper = query.chars().any(char::is_uppercase);
        let Ok(regex) = regex::RegexBuilder::new(&regex_escape_if_invalid(query))
            .case_insensitive(!has_upper)
            .build()
        else {
            self.search = None;
            return false;
        };
        // Repeating the same query continues from the current match; a new query starts from
        // the newest output (Backward) or the oldest scrollback (Forward).
        let mut state = match self.search.take() {
            Some(mut s) if s.query == query => {
                s.regex = regex;
                s
            }
            _ => SearchState {
                query: query.to_owned(),
                regex,
                focused: None,
            },
        };
        let top = -(self.vt.history() as i32);
        let bottom = self.rows as i32 - 1;
        let found = {
            let mut rows = Rows::new(self);
            let all = find_matches(&mut rows, &state.regex, top, bottom, usize::MAX);
            match dir {
                SearchDirection::Backward => {
                    let origin = state
                        .focused
                        .and_then(|m| rows.prev(m.0))
                        .unwrap_or(Point::new(bottom, self.cols - 1));
                    all.iter()
                        .rev()
                        .find(|m| m.0 <= origin)
                        .or(all.last())
                        .copied()
                }
                SearchDirection::Forward => {
                    let origin = state
                        .focused
                        .and_then(|m| rows.next(m.1))
                        .unwrap_or(Point::new(top, 0));
                    all.iter().find(|m| m.0 >= origin).or(all.first()).copied()
                }
            }
        };
        if let Some(m) = &found {
            self.scroll_to_point(m.0);
        }
        state.focused = found;
        self.search = Some(state);
        found.is_some()
    }

    /// Scroll just enough to show `p`.
    fn scroll_to_point(&mut self, p: Point) {
        let offset = self.display_offset() as i32;
        let screen = self.rows as i32;
        if p.line < -offset {
            self.set_display_offset(-p.line as i64);
        } else if p.line >= screen - offset {
            self.set_display_offset((screen - p.line - 1).max(0) as i64);
        }
    }

    fn visible_matches(&self) -> Vec<Match> {
        let Some(state) = self.search.as_ref() else {
            return Vec::new();
        };
        let offset = self.display_offset() as i32;
        let top = -offset;
        let bottom = self.rows as i32 - 1 - offset;
        let mut rows = Rows::new(self);
        find_matches(&mut rows, &state.regex, top, bottom, 1000)
    }

    // -----------------------------------------------------------------------------------------
    // Text export
    // -----------------------------------------------------------------------------------------

    pub fn clear_scrollback(&mut self) {
        self.vt.write(b"\x1b[3J");
        self.collect_events();
        self.vt.set_selection(None);
        self.generation += 1;
    }

    /// Cmd+K: clear the screen and the scrollback, like Terminal.app and Ghostty. At a shell
    /// prompt the prompt (with what's typed so far) moves to the top; otherwise the cursor's
    /// line does. A full-screen program keeps its screen: only the scrollback goes.
    pub fn clear_screen(&mut self) {
        if !self.is_alt_screen() {
            let history = self.vt.history();
            let cursor = self.vt.cursor().1 as usize;
            let top = Self::tracked_row(&self.prompt_start)
                .filter(|_| self.at_prompt)
                .and_then(|s| s.checked_sub(history))
                .filter(|&row| row <= cursor)
                .unwrap_or(cursor);
            if top > 0 {
                // Scroll the rows above into history (cleared below), cursor along with them.
                let seq = format!("\x1b[{top}S\x1b[{top}A");
                self.vt.write(seq.as_bytes());
            }
        }
        self.clear_scrollback();
    }

    pub fn reset(&mut self) {
        self.vt.write(b"\x1bc");
        self.collect_events();
        self.filter.take_scroll_region();
        self.scroll_region = None;
        self.vt.set_selection(None);
        self.prompt_start = None;
        self.input_start = None;
        self.at_prompt = false;
        self.after_write();
        self.check_images();
        self.generation += 1;
    }

    pub fn is_alt_screen(&self) -> bool {
        self.vt.is_alt_screen()
    }

    /// Text of the screen / scrollback as requested by `opts`.
    pub fn capture(&self, opts: &CaptureOpts) -> String {
        let screen = self.rows as i32;
        let history = self.vt.history() as i32;
        let top = match (opts.lines, opts.scrollback) {
            (Some(n), _) => (screen - n as i32).max(-history),
            (None, true) => -history,
            (None, false) => 0,
        };
        // Plain text keeps rows as they are on screen; ANSI output rejoins soft wraps.
        let text = self.format_lines(top, screen - 1, opts.ansi, opts.ansi);
        let sep = if opts.ansi { "\r\n" } else { "\n" };
        let mut lines: Vec<&str> = text.split(sep).collect();
        while lines.last().is_some_and(|l| strip_sgr(l).trim().is_empty()) {
            lines.pop();
        }
        let mut out = String::new();
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        if opts.ansi {
            out.push_str("\x1b[0m");
        }
        out
    }

    /// ANSI-escaped copy of the last `max_lines` lines of the primary screen, for persistence.
    /// Returns `None` while the alternate screen is active.
    pub fn serialize_history(&self, max_lines: usize) -> Option<Vec<u8>> {
        if self.is_alt_screen() {
            return None;
        }
        let screen = self.rows as i32;
        let history = self.vt.history() as i32;
        // Up to the cursor's line, so we don't persist a screen full of blank lines.
        let cursor_line = self.cursor_point().line;
        let top = (cursor_line - max_lines as i32 + 1).max(-history);
        let bottom = cursor_line.min(screen - 1);
        let mut out = self.format_lines(top, bottom, true, true);
        out.push_str("\r\n\x1b[0m");
        Some(out.into_bytes())
    }

    /// Lines `top..=bottom` by libghostty-vt's formatter: VT sequences or plain text, soft
    /// wraps joined or not, trailing blanks trimmed.
    fn format_lines(&self, top: i32, bottom: i32, vt: bool, unwrap: bool) -> String {
        let history = self.vt.history() as i32;
        let (top, bottom) = (top + history, bottom + history);
        if top > bottom || top < 0 {
            return String::new();
        }
        let range = vt::Sel {
            start: (0, top as u32),
            end: (self.cols as u16 - 1, bottom as u32),
            rectangle: false,
        };
        let bytes = self
            .vt
            .format(vt, unwrap, true, vt::FormatExtras::default(), Some(range));
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Lines `top..=bottom` as (text, soft-wrapped) with optional SGR attributes.
    fn export_lines(&self, top: i32, bottom: i32, ansi: bool) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let mut sgr = SgrState::default();
        for line in top..=bottom {
            let row = self.read_row(line);
            let wrapped = row.wrapped;
            // Trim trailing blank default cells.
            let mut end = row.cells.len();
            if !wrapped {
                while end > 0 {
                    let c = &row.cells[end - 1];
                    let blank = c.ch == ' '
                        && c.pen.bg == Color::Default
                        && c.pen.attrs & A_INVERSE == 0
                        && c.pen.underline == UL_NONE;
                    if !blank {
                        break;
                    }
                    end -= 1;
                }
            }
            let mut text = String::new();
            for c in &row.cells[..end] {
                if c.is_spacer() {
                    continue;
                }
                if ansi {
                    sgr.transition(&c.pen, &mut text);
                }
                text.push(c.ch);
                text.extend(c.zw.iter());
            }
            if ansi && !wrapped && sgr.has_bg() {
                // Don't let a background color bleed into the next line on replay.
                text.push_str("\x1b[49m");
                if let Some(p) = &mut sgr.pen {
                    p.bg = Color::Default;
                }
            }
            out.push((text, wrapped));
        }
        out
    }

    /// Everything needed to rebuild this terminal elsewhere (an app attaching to the pane):
    /// scrollback and screen (both screens while a full-screen app runs), cursor, modes, kitty
    /// keyboard flags, scroll region, cursor style, palette overrides, title and prompt marks.
    /// Feed it to a fresh `Terminal` of the same size with [`Terminal::replay`].
    pub fn serialize_state(&mut self) -> Vec<u8> {
        let alt = self.is_alt_screen();
        // The alternate screen: every row positioned explicitly.
        let alt_screen = alt.then(|| self.export_screen_exact());
        if alt {
            // Look at the primary screen (mode 47 switches screens without clearing either).
            self.vt.write(b"\x1b[?47l");
        }
        let mut out = String::new();
        let history = self.vt.history() as i32;
        let screen = self.rows as i32;
        let lines = self.export_lines_marked(-history, screen - 1);
        let count = lines.len();
        for (i, (text, wrapped)) in lines.into_iter().enumerate() {
            out.push_str(&text);
            if !wrapped && i + 1 < count {
                out.push_str("\r\n");
            }
        }
        out.push_str("\x1b[0m");
        // The current prompt's marks (events in the copy too, unlike the replayed ones): where
        // it began, so the copy clears it on resize as this terminal does, and where its input
        // starts, for the copy's completion.
        let history = self.vt.history();
        let on_screen =
            |row: usize| (row >= history && row - history < screen as usize).then(|| row - history);
        if let Some(start) = Self::tracked_row(&self.prompt_start).and_then(on_screen) {
            let redraw = if self.shell_redraws {
                ";redraw=1"
            } else {
                ";redraw=0"
            };
            out.push_str(&format!("\x1b[{};1H\x1b]133;A{redraw}\x07", start + 1));
            if let Some((x, y)) = self.input_start.as_ref().and_then(|t| t.screen_point())
                && let Some(row) = on_screen(y as usize)
            {
                out.push_str(&format!("\x1b[{};{}H\x1b]133;B\x07", row + 1, x + 1));
            }
            if !self.at_prompt {
                out.push_str("\x1b]133;C\x07");
            }
        }
        let primary_cursor = self.cursor_point();
        if let Some(alt_screen) = &alt_screen {
            // Back to the alternate screen, and have the copy switch to it (after saving the
            // primary cursor) and draw it.
            out.push_str(&format!(
                "\x1b[{};{}H",
                primary_cursor.line + 1,
                primary_cursor.col + 1
            ));
            out.push_str("\x1b[?1049h");
            self.vt.write(b"\x1b[?47h");
            out.push_str(alt_screen);
        }
        self.collect_events();
        let mode = self.mode();
        let cursor = self.cursor_point();
        if let Some((top, bottom)) = self.scroll_region {
            out.push_str(&format!("\x1b[{top};{bottom}r"));
        }
        let set = |out: &mut String, on: bool, code: &str| {
            if on {
                out.push_str(&format!("\x1b[?{code}h"));
            }
        };
        set(&mut out, mode.contains(TermMode::ORIGIN), "6");
        let origin_top = if mode.contains(TermMode::ORIGIN) {
            self.scroll_region.map_or(1, |(t, _)| t as i32)
        } else {
            1
        };
        out.push_str(&format!(
            "\x1b[{};{}H",
            cursor.line + 2 - origin_top,
            cursor.col + 1
        ));
        // SGR for text printed next.
        let template = Pen::from_style(&self.vt.cursor_style());
        SgrState::default().transition(&template, &mut out);
        set(&mut out, mode.contains(TermMode::APP_CURSOR), "1");
        set(
            &mut out,
            mode.contains(TermMode::MOUSE_REPORT_CLICK),
            "1000",
        );
        set(&mut out, mode.contains(TermMode::MOUSE_DRAG), "1002");
        set(&mut out, mode.contains(TermMode::MOUSE_MOTION), "1003");
        set(&mut out, mode.contains(TermMode::FOCUS_IN_OUT), "1004");
        set(&mut out, mode.contains(TermMode::UTF8_MOUSE), "1005");
        set(&mut out, mode.contains(TermMode::SGR_MOUSE), "1006");
        set(&mut out, mode.contains(TermMode::BRACKETED_PASTE), "2004");
        if !mode.contains(TermMode::ALTERNATE_SCROLL) {
            out.push_str("\x1b[?1007l");
        }
        if !mode.contains(TermMode::LINE_WRAP) {
            out.push_str("\x1b[?7l");
        }
        if !mode.contains(TermMode::SHOW_CURSOR) {
            out.push_str("\x1b[?25l");
        }
        if mode.contains(TermMode::APP_KEYPAD) {
            out.push_str("\x1b=");
        }
        if mode.contains(TermMode::INSERT) {
            out.push_str("\x1b[4h");
        }
        if mode.contains(TermMode::LINE_FEED_NEW_LINE) {
            out.push_str("\x1b[20h");
        }
        let kitty = [
            (TermMode::DISAMBIGUATE_ESC_CODES, 1),
            (TermMode::REPORT_EVENT_TYPES, 2),
            (TermMode::REPORT_ALTERNATE_KEYS, 4),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
            (TermMode::REPORT_ASSOCIATED_TEXT, 16),
        ]
        .iter()
        .filter(|(m, _)| mode.contains(*m))
        .map(|(_, v)| v)
        .sum::<u32>();
        if kitty != 0 {
            out.push_str(&format!("\x1b[={kitty};1u"));
        }
        // DECSCUSR (the render state's update keeps its dirty rows for the next frame).
        self.render.update(&self.vt);
        let rc = self.render.cursor();
        let code = match rc.style {
            vt::CursorVisual::BAR => 6,
            vt::CursorVisual::UNDERLINE => 4,
            _ => 2,
        } - u8::from(rc.blinking);
        out.push_str(&format!("\x1b[{code} q"));
        let (palette, defaults) = (self.vt.palette(), self.vt.default_palette());
        for i in 0..256usize {
            if palette[i] != defaults[i] {
                let c = palette[i];
                out.push_str(&format!(
                    "\x1b]4;{i};rgb:{:02x}/{:02x}/{:02x}\x07",
                    (c >> 16) & 0xff,
                    (c >> 8) & 0xff,
                    c & 0xff
                ));
            }
        }
        for (current, default, osc) in [
            (self.vt.fg(), self.vt.default_fg(), 10),
            (self.vt.bg(), self.vt.default_bg(), 11),
            (self.vt.cursor_color(), self.vt.default_cursor_color(), 12),
        ] {
            if let Some(c) = current
                && current != default
            {
                out.push_str(&format!(
                    "\x1b]{osc};rgb:{:02x}/{:02x}/{:02x}\x07",
                    (c >> 16) & 0xff,
                    (c >> 8) & 0xff,
                    c & 0xff
                ));
            }
        }
        if let Some(title) = &self.title {
            out.push_str(&format!(
                "\x1b]2;{}\x07",
                title.replace(['\x07', '\x1b'], "")
            ));
        }
        out.into_bytes()
    }

    /// The active screen, each row placed with an explicit cursor position (no scrolling).
    fn export_screen_exact(&self) -> String {
        let rows = self.rows as i32;
        let mut out = String::new();
        for (r, (text, _)) in self.export_lines(0, rows - 1, true).into_iter().enumerate() {
            out.push_str(&format!("\x1b[{};1H", r + 1));
            out.push_str(&text);
            out.push_str("\x1b[0m");
        }
        out
    }

    /// Like `export_lines` with SGR, with the shell prompt marks (OSC 133) the cells carry, so
    /// prompt jumping works in the copy. They are replay marks: no events there.
    fn export_lines_marked(&self, top: i32, bottom: i32) -> Vec<(String, bool)> {
        use vt::Semantic;
        let mut out = self.export_lines(top, bottom, true);
        let mut current = Semantic::OUTPUT;
        for (i, line) in (top..=bottom).enumerate() {
            let Some(r0) = self.row_ref(line) else {
                continue;
            };
            let row_prompt = vt::row_prompt(&r0);
            if row_prompt == vt::RowPrompt::NONE && current == Semantic::OUTPUT {
                continue;
            }
            let mut marks = Vec::new();
            for x in 0..self.cols {
                let s = vt::cell_semantic(&GridRef { x: x as u16, ..r0 });
                let starts_prompt =
                    x == 0 && row_prompt == vt::RowPrompt::PROMPT && s == Semantic::PROMPT;
                if s != current || starts_prompt {
                    let kind = match s {
                        Semantic::PROMPT => 'P',
                        Semantic::INPUT => 'B',
                        _ => 'C',
                    };
                    marks.push((x, kind));
                    current = s;
                }
            }
            let (text, _) = &mut out[i];
            for (col, kind) in marks.into_iter().rev() {
                let at = byte_offset_of_column(text, col);
                text.insert_str(
                    at,
                    &format!("\x1b]133;{kind};{}\x07", crate::osc::REPLAY_MARK),
                );
            }
        }
        out
    }

    /// Replay previously serialized history (session restore).
    pub fn replay(&mut self, bytes: &[u8]) {
        // Through the stream filter, so prompt marks (OSC 133) and the scroll region apply.
        let idle_since = self.last_output;
        self.advance(bytes);
        self.last_output = idle_since;
        // Responses to queries embedded in old output must not reach the new shell.
        self.events.retain(|e| !matches!(e, TermEvent::PtyWrite(_)));
        self.generation += 1;
    }

    /// Screen text (visible lines), used for agent detection.
    pub fn screen_text(&self) -> String {
        self.export_lines(0, self.rows as i32 - 1, false)
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Write text directly into the terminal as if the program printed it.
    pub fn print(&mut self, text: &str) {
        self.vt.write(text.as_bytes());
        self.collect_events();
        self.after_write();
        self.generation += 1;
    }
}

/// Text of the cells from `start` up to (not including) `end`.
fn text_between(rows: &mut Rows, start: Point, end: Point, zw: bool) -> String {
    let mut text = String::new();
    let mut p = start;
    while p < end {
        let cell = rows.cell(p);
        if cell.wide != Width::Spacer {
            text.push(cell.ch);
            if zw {
                text.extend(cell.zw.iter());
            }
        }
        match rows.next(p) {
            Some(n) => p = n,
            None => break,
        }
    }
    text
}

/// Matches of `regex` in the lines overlapping `top..=bottom` (soft-wrapped lines are searched
/// as one), in order, at most `limit`.
fn find_matches(
    rows: &mut Rows,
    regex: &regex::Regex,
    top: i32,
    bottom: i32,
    limit: usize,
) -> Vec<Match> {
    let mut out = Vec::new();
    let top_limit = rows.top();
    let mut line = top.max(top_limit);
    while line > top_limit && rows.row(line - 1).wrapped {
        line -= 1;
    }
    let last_line = rows.bottom();
    while line <= bottom.min(last_line) && out.len() < limit {
        // One logical line: its text and where each character is.
        let mut text = String::new();
        let mut at: Vec<(usize, Point)> = Vec::new();
        loop {
            let row = rows.row(line);
            for (col, c) in row.cells.iter().enumerate() {
                if c.is_spacer() {
                    continue;
                }
                at.push((text.len(), Point::new(line, col)));
                text.push(c.ch);
                text.extend(c.zw.iter());
            }
            let wrapped = row.wrapped;
            line += 1;
            if !wrapped || line > last_line {
                break;
            }
        }
        let point_at = |byte: usize| -> Point {
            let i = at.partition_point(|(b, _)| *b <= byte).saturating_sub(1);
            at[i].1
        };
        for m in regex.find_iter(&text) {
            if m.start() == m.end() {
                continue;
            }
            let (s, e) = (point_at(m.start()), point_at(m.end() - 1));
            if e.line < top || s.line > bottom {
                continue;
            }
            out.push((s, e));
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// A grid cell from libghostty-vt's cell data, style and grapheme cluster.
fn gcell(c: &vt::RawCell, mut pen: Pen, graphemes: &[char], link: Option<String>) -> GCell {
    match c.tag {
        vt::TAG_BG_PALETTE => pen.bg = Color::Indexed(c.bg_palette),
        vt::TAG_BG_RGB => pen.bg = Color::Rgb(vt::from_rgb(c.bg_rgb)),
        _ => {}
    }
    let (ch, zw) = if c.tag == vt::TAG_GRAPHEME {
        let base = graphemes
            .first()
            .copied()
            .or_else(|| char::from_u32(c.codepoint))
            .unwrap_or(' ');
        (
            base,
            graphemes.get(1..).map(<[char]>::to_vec).unwrap_or_default(),
        )
    } else {
        (char::from_u32(c.codepoint).unwrap_or(' '), Vec::new())
    };
    GCell {
        ch: if c.codepoint == 0 { ' ' } else { ch },
        zw,
        wide: match c.wide {
            vt::WIDE => Width::Wide,
            vt::SPACER_TAIL => Width::Spacer,
            vt::SPACER_HEAD => Width::LeadingSpacer,
            _ => Width::Narrow,
        },
        pen,
        link,
    }
}

/// `text` without its CSI sequences.
fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for d in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Byte offset in an exported line (text with SGR escapes) where display column `col` starts.
fn byte_offset_of_column(text: &str, col: usize) -> usize {
    let mut column = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\x1b' {
            // Skip a CSI sequence.
            for (_, d) in chars.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if column >= col {
            return i;
        }
        if unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) > 0 {
            column += unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
        }
    }
    text.len()
}

#[derive(Default)]
struct SgrState {
    /// What the last emitted SGR set (`None`: nothing emitted yet).
    pen: Option<Pen>,
}

impl SgrState {
    fn has_bg(&self) -> bool {
        self.pen.is_some_and(|p| p.bg != Color::Default)
    }

    fn transition(&mut self, pen: &Pen, out: &mut String) {
        if self.pen.as_ref() == Some(pen) {
            return;
        }
        self.force(pen, out);
    }

    fn force(&mut self, pen: &Pen, out: &mut String) {
        let mut params: Vec<String> = vec!["0".into()];
        for (a, p) in [(A_BOLD, "1"), (A_DIM, "2"), (A_ITALIC, "3")] {
            if pen.attrs & a != 0 {
                params.push(p.into());
            }
        }
        match pen.underline {
            UL_SINGLE => params.push("4".into()),
            UL_DOUBLE => params.push("4:2".into()),
            UL_CURLY => params.push("4:3".into()),
            UL_DOTTED => params.push("4:4".into()),
            UL_DASHED => params.push("4:5".into()),
            _ => {}
        }
        for (a, p) in [(A_INVERSE, "7"), (A_HIDDEN, "8"), (A_STRIKE, "9")] {
            if pen.attrs & a != 0 {
                params.push(p.into());
            }
        }
        if let Some(p) = color_param(pen.fg, false) {
            params.push(p);
        }
        if let Some(p) = color_param(pen.bg, true) {
            params.push(p);
        }
        match pen.ul_color {
            Some(Color::Rgb(v)) => params.push(format!(
                "58:2::{}:{}:{}",
                (v >> 16) & 0xff,
                (v >> 8) & 0xff,
                v & 0xff
            )),
            Some(Color::Indexed(i)) => params.push(format!("58:5:{i}")),
            _ => {}
        }
        out.push_str("\x1b[");
        out.push_str(&params.join(";"));
        out.push('m');
        self.pen = Some(*pen);
    }
}

fn color_param(c: Color, bg: bool) -> Option<String> {
    let base = if bg { 40 } else { 30 };
    match c {
        Color::Default => None,
        Color::Indexed(n @ 0..=7) => Some(format!("{}", base + n as u32)),
        Color::Indexed(n @ 8..=15) => Some(format!("{}", base + 60 + n as u32 - 8)),
        Color::Indexed(i) => Some(format!("{};5;{i}", base + 8)),
        Color::Rgb(v) => Some(format!(
            "{};2;{};{};{}",
            base + 8,
            (v >> 16) & 0xff,
            (v >> 8) & 0xff,
            v & 0xff
        )),
    }
}

fn dim(c: u32) -> u32 {
    let f = |v: u32| (v as f32 * 0.66) as u32;
    f((c >> 16) & 0xff) << 16 | f((c >> 8) & 0xff) << 8 | f(c & 0xff)
}

/// Treat queries that aren't valid regexes as literal text.
fn regex_escape_if_invalid(q: &str) -> String {
    if regex::Regex::new(q).is_ok() {
        q.to_owned()
    } else {
        regex::escape(q)
    }
}

#[cfg(test)]
mod tests;
