//! Wire protocol between `thurmd` (the session daemon that owns every PTY) and its clients:
//! the macOS app (through `thurm-ffi`) and the `thurm` control CLI.
//!
//! Transport is a Unix domain socket carrying length-prefixed postcard messages
//! (see [`codec`]). Clients send [`Envelope`]s, the daemon answers with
//! [`ServerMessage::Response`] carrying the same id and pushes unsolicited
//! [`ServerMessage::Event`]s.

pub mod bytes;
pub mod codec;
pub mod layout;

use serde::{Deserialize, Serialize};

pub use layout::{Layout, LayoutNode, SplitDir, TabLayout, WindowLayout};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 14;

/// Daemons speaking this protocol or later replace themselves in place on SIGUSR2 (see
/// `thurm_client::upgrade_daemon`), keeping every pane's process running.
pub const HOT_UPGRADE_PROTOCOL: u32 = 13;

/// Identifies the exact build: release builds set `THURM_BUILD` (e.g. `0.2.0+412.3fa1c2e`), so a
/// daemon left running from an older build is told apart from the app's own.
pub const BUILD: &str = match option_env!("THURM_BUILD") {
    Some(b) => b,
    None => env!("CARGO_PKG_VERSION"),
};

/// What this build supports beyond [`PROTOCOL_VERSION`], exchanged in `Hello` so a later
/// version can accept older peers by capability instead of by exact version.
pub const CAPABILITIES: &[&str] = &["hello-capabilities", "temp-file", "remote-layout"];

/// Environment variable exported into every pane with the pane's id.
pub const ENV_PANE_ID: &str = "THURM_PANE_ID";
/// Environment variable exported into every pane with the daemon socket path.
pub const ENV_SOCKET: &str = "THURM_SOCKET";

pub type PaneId = u64;
pub type ImageId = u32;

/// Client → daemon request with a correlation id.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Envelope {
    pub id: u64,
    pub request: Request,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Request {
    /// Must be the first request on a connection.
    Hello {
        client: String,
        version: u32,
        ui: bool,
        /// The client's [`CAPABILITIES`]. Last field: daemons of protocol 13 decode the rest of
        /// the Hello and answer with their version.
        #[serde(default)]
        capabilities: Vec<String>,
    },
    CreatePane(CreatePane),
    /// Kill the pane's process group and forget the pane.
    ClosePane {
        pane: PaneId,
    },
    ListPanes,
    PaneInfo {
        pane: PaneId,
    },
    Resize {
        pane: PaneId,
        size: PaneSize,
    },
    /// Raw bytes written to the PTY.
    Input {
        pane: PaneId,
        data: Vec<u8>,
    },
    /// Text pasted by the user; wrapped in bracketed-paste markers when the app asked for it.
    Paste {
        pane: PaneId,
        text: String,
    },
    /// A key event; encoded by the daemon using the pane's current keyboard mode.
    Key {
        pane: PaneId,
        key: KeyEvent,
    },
    /// A mouse event; reported to the app or turned into a selection.
    Mouse {
        pane: PaneId,
        event: MouseEvent,
    },
    /// Mouse wheel / trackpad scrolling, in whole lines (positive = up).
    Wheel {
        pane: PaneId,
        lines: i32,
        col: u16,
        row: u16,
        mods: u8,
    },
    /// Scroll the viewport through the scrollback.
    Scroll {
        pane: PaneId,
        scroll: ScrollCmd,
    },
    Focus {
        pane: PaneId,
        focused: bool,
    },
    Subscribe {
        pane: PaneId,
    },
    Unsubscribe {
        pane: PaneId,
    },
    Selection {
        pane: PaneId,
        op: SelectionOp,
    },
    /// Returns [`Response::Text`] with the current selection (empty when none).
    CopySelection {
        pane: PaneId,
    },
    /// Search the scrollback. `None` clears the search.
    Search {
        pane: PaneId,
        query: Option<String>,
        direction: SearchDirection,
    },
    Capture {
        pane: PaneId,
        opts: CaptureOpts,
    },
    Wait {
        pane: PaneId,
        until: WaitCondition,
        timeout_ms: Option<u64>,
    },
    /// Store the GUI layout (JSON encoded [`Layout`]).
    SetLayout {
        json: String,
    },
    /// Returns [`Response::Layout`].
    GetLayout,
    /// Broadcast a UI command to every connected UI client.
    Ui(UiCommand),
    /// Answer to [`Event::ClipboardRequest`].
    ClipboardReply {
        pane: PaneId,
        text: String,
    },
    ClearScrollback {
        pane: PaneId,
    },
    /// Clear the screen and the scrollback, keeping the prompt (Cmd+K).
    ClearScreen {
        pane: PaneId,
    },
    Reset {
        pane: PaneId,
    },
    ReloadConfig,
    /// Persist a snapshot to disk now.
    SaveSnapshot,
    Shutdown {
        kill_panes: bool,
    },
    /// Returns the agent launch presets from the config.
    ListAgentPresets,
    /// An agent hook fired inside `pane` (`thurm agent-hook`). `event` is one of
    /// session-start, prompt-submit, notification, tool-complete, stop, session-end.
    AgentHook {
        pane: PaneId,
        agent: String,
        event: String,
        session_id: Option<String>,
        message: Option<String>,
        /// The agent's session transcript (Claude Code), read for the session's title.
        transcript_path: Option<String>,
    },
    /// What happened in the pane's last finished command, from the on-device model
    /// (`ai.explain`). Answers `Text`.
    Explain {
        pane: PaneId,
    },
    /// The GUI's system appearance; picks the theme when `colors.theme` follows it.
    SetAppearance {
        dark: bool,
    },
    /// Write `colors.theme` (a name or `light:NAME,dark:NAME`) to the config file and reload.
    SetTheme {
        spec: String,
    },
    /// Tab completion for the command line being typed at the pane's shell prompt (needs shell
    /// integration). Returns [`Response::Completions`].
    Complete {
        pane: PaneId,
    },
    /// Returns [`Response::Processes`]: every pane's process tree with listening ports, or
    /// only `pane`'s.
    Processes {
        pane: Option<PaneId>,
    },
    /// Write one setting (`key` = "section.name", `value` = TOML literal) and reload.
    SetSetting {
        key: String,
        value: String,
    },
    /// Write `data` to a new 0600 file named after `name` in the daemon's runtime directory and
    /// answer its path as [`Response::Text`] (an image pasted into a remote pane goes over as
    /// bytes, and the program gets a path it can read).
    WriteTempFile {
        name: String,
        #[serde(with = "bytes")]
        data: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CreatePane {
    /// Program and arguments; `None` launches the user's shell.
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub size: PaneSize,
    /// Start an agent preset by name instead of `command`.
    pub agent_preset: Option<String>,
    /// Inherit cwd from this pane when `cwd` is `None`.
    pub inherit_cwd_from: Option<PaneId>,
    /// Keep the pane open after the process exits.
    pub hold: bool,
    /// Fork the agent session running in this pane (its hooks reported the session id):
    /// the agent's `fork_session` command, in that pane's directory.
    #[serde(default)]
    pub fork_from: Option<PaneId>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Default for PaneSize {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ServerMessage {
    Response {
        id: u64,
        result: Result<Response, String>,
    },
    Event(Event),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
// Responses are short-lived and one at a time; boxing PaneInfo would buy nothing.
#[allow(clippy::large_enum_variant)]
pub enum Response {
    Ok,
    Hello {
        version: u32,
        daemon_pid: u32,
        restored: bool,
        /// The daemon's [`BUILD`].
        build: String,
        /// The daemon's [`CAPABILITIES`].
        #[serde(default)]
        capabilities: Vec<String>,
    },
    PaneCreated {
        pane: PaneId,
    },
    Panes(Vec<PaneInfo>),
    PaneInfo(PaneInfo),
    Text(String),
    Layout(Option<String>),
    Wait(WaitOutcome),
    Search {
        found: bool,
    },
    AgentPresets(Vec<AgentPreset>),
    Processes(Vec<PaneProcesses>),
    Completions(Completions),
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Event {
    /// Start of a subscription (or a re-sync): the pane's complete terminal state, to replay
    /// into a fresh local terminal of `size`. `Output` and `Resized` follow, in order.
    Attach {
        pane: PaneId,
        size: PaneSize,
        #[serde(with = "bytes")]
        state: Vec<u8>,
    },
    /// PTY output for a subscribed pane, to feed the client's local terminal.
    Output {
        pane: PaneId,
        #[serde(with = "bytes")]
        data: Vec<u8>,
    },
    /// The pane was resized at this point of the output stream.
    Resized {
        pane: PaneId,
        size: PaneSize,
    },
    /// A pane's metadata changed (title, cwd, foreground process, agent state...).
    PaneInfo(PaneInfo),
    PaneExited {
        pane: PaneId,
        code: Option<i32>,
    },
    PaneClosed {
        pane: PaneId,
    },
    Bell {
        pane: PaneId,
    },
    /// Desktop notification requested by the program (OSC 9 / 99 / 777) or by agent detection.
    Notify {
        pane: PaneId,
        title: String,
        body: String,
    },
    /// OSC 52 copy.
    ClipboardStore {
        pane: PaneId,
        text: String,
    },
    /// OSC 52 paste; the UI replies with [`Request::ClipboardReply`].
    ClipboardRequest {
        pane: PaneId,
    },
    Ui(UiCommand),
    ConfigReloaded,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PaneInfo {
    pub id: PaneId,
    pub title: String,
    pub cwd: Option<String>,
    pub pid: Option<u32>,
    pub foreground: Option<ProcessInfo>,
    pub agent: Option<AgentState>,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub size: PaneSize,
    /// Terminal has echo off with canonical mode on: somebody is typing a password.
    pub password_input: bool,
    /// Shell integration reports the shell sitting at a prompt.
    pub at_prompt: bool,
    pub last_exit_status: Option<i32>,
    /// Milliseconds since the pane last produced output.
    pub idle_ms: u64,
    pub restored: bool,
    /// ConEmu OSC 9;4 progress, while the program reports one.
    pub progress: Option<Progress>,
    /// Git status of the working directory, when it is inside a repository.
    #[serde(default)]
    pub git: Option<GitInfo>,
}

/// Progress reported by the running program (ConEmu OSC 9;4).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub state: ProgressState,
    /// 0-100, when the program gave one.
    pub percent: Option<u8>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    Normal,
    Error,
    /// Busy without a known percentage.
    Indeterminate,
    Paused,
}

/// Completion candidates for the word before the cursor.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Completions {
    /// The word being completed (as typed, before the cursor).
    pub word: String,
    pub items: Vec<CompletionItem>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    /// Replacement for `word`.
    pub text: String,
    pub kind: CompletionKind,
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Subcommand,
    Flag,
    Value,
    File,
    Directory,
}

/// Processes running in a pane (its shell and everything below it) and their listening ports.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneProcesses {
    pub pane: PaneId,
    pub processes: Vec<ProcEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    /// Command line.
    pub command: String,
    /// TCP ports it listens on.
    pub ports: Vec<u16>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct GitInfo {
    /// Work tree root.
    pub root: String,
    /// Branch, or the short commit when detached.
    pub branch: String,
    /// Lines added / removed versus HEAD (`git diff --numstat HEAD`).
    pub added: u32,
    pub removed: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub argv: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentState {
    /// Display name, e.g. "Claude Code".
    pub name: String,
    /// Stable identifier, e.g. "claude".
    pub kind: String,
    pub status: AgentStatus,
    /// The agent's own session id, from its hooks (exact resume on restore).
    #[serde(default)]
    pub session_id: Option<String>,
    /// What the agent asked for (hook notification message).
    #[serde(default)]
    pub message: Option<String>,
    /// Duration of the last finished turn, in milliseconds.
    #[serde(default)]
    pub turn_ms: Option<u64>,
    /// Turns finished in this session.
    #[serde(default)]
    pub turns: u32,
    /// Status comes from the agent's hooks rather than from reading the screen.
    #[serde(default)]
    pub hooked: bool,
    /// What the session is about: the title the agent gave it (Claude Code's generated or
    /// `/rename` title). Used as the pane's title.
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// Producing output.
    Working,
    /// Quiet, not asking for anything.
    Idle,
    /// Asked for the user's attention (permission prompt, notification).
    NeedsInput,
    /// Finished a turn the user hasn't looked at yet (hooks only); becomes Idle on input.
    Done,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AgentPreset {
    pub name: String,
    pub command: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------

/// Screen contents of a pane. The first frame after `Subscribe` (and after resizes) is `full`;
/// later frames only carry rows that changed since the previous frame sent to that client.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Frame {
    pub pane: PaneId,
    pub cols: u16,
    pub rows: u16,
    pub full: bool,
    /// Rows moved down by this many (negative: up) before `lines` apply, because the
    /// scrollback position changed: row `r` shows what row `r - shift` showed before.
    pub shift: i32,
    pub lines: Vec<FrameRow>,
    /// The history line just above the viewport, for smooth (sub-line) scrolling. Sent when it
    /// changed; `None` keeps the previous one.
    pub peek: Option<FrameRow>,
    /// Whether there is a history line above the viewport (`peek` is meaningful).
    pub has_peek: bool,
    pub cursor: CursorState,
    pub display_offset: u32,
    pub history_size: u32,
    /// `thurm_term::TermMode` bits.
    pub modes: u32,
    pub colors: FrameColors,
    /// Hyperlinks referenced by cells in `lines` (`Cell::link` is an index + 1).
    pub links: Vec<String>,
    /// Kitty graphics placements visible in the viewport (complete list every frame).
    pub images: Vec<ImagePlacement>,
    pub title: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct FrameRow {
    pub row: u16,
    pub cells: Vec<Cell>,
    /// Grapheme clusters that don't fit in a single `char`: (column, full cluster text).
    pub clusters: Vec<(u16, String)>,
}

/// A rendered cell. Colors are resolved `0x00RRGGBB`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Cell {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
    /// Underline color; `NO_COLOR` means "use fg".
    pub ul: u32,
    pub flags: u16,
    /// Index + 1 into `Frame::links`, 0 when the cell is not a hyperlink.
    pub link: u16,
}

pub const NO_COLOR: u32 = 0xFF00_0000;

pub mod cell_flags {
    pub const BOLD: u16 = 1 << 0;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const DOUBLE_UNDERLINE: u16 = 1 << 3;
    pub const UNDERCURL: u16 = 1 << 4;
    pub const DOTTED_UNDERLINE: u16 = 1 << 5;
    pub const DASHED_UNDERLINE: u16 = 1 << 6;
    pub const STRIKEOUT: u16 = 1 << 7;
    /// First half of a double-width character.
    pub const WIDE: u16 = 1 << 8;
    /// Second half of a double-width character; draw nothing.
    pub const WIDE_SPACER: u16 = 1 << 9;
    pub const HIDDEN: u16 = 1 << 10;
    pub const SELECTED: u16 = 1 << 11;
    pub const SEARCH_MATCH: u16 = 1 << 12;
    /// Background is the terminal default (the UI may draw it translucent).
    pub const DEFAULT_BG: u16 = 1 << 13;
    pub const DIM: u16 = 1 << 14;
    /// Focused search match.
    pub const SEARCH_FOCUS: u16 = 1 << 15;
    pub const ANY_UNDERLINE: u16 =
        UNDERLINE | DOUBLE_UNDERLINE | UNDERCURL | DOTTED_UNDERLINE | DASHED_UNDERLINE;
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorState {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub blinking: bool,
    /// The cursor sits on a wide character.
    pub wide: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
    Hidden,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameColors {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub cursor_text: u32,
    pub selection_fg: u32,
    pub selection_bg: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ImagePlacement {
    pub image: ImageId,
    pub placement: u32,
    /// Viewport row of the top-left cell (may be negative when partially scrolled off).
    pub row: i32,
    pub col: i32,
    /// Pixel offset inside the top-left cell.
    pub x_offset: u32,
    pub y_offset: u32,
    /// Source rectangle in image pixels.
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// Destination size in cells.
    pub cols: u32,
    pub rows: u32,
    pub z: i32,
    /// Destination size in pixels, overriding `cols`/`rows` when non-zero (slices of an image
    /// shown through Unicode placeholders).
    #[serde(default)]
    pub dst_w: u32,
    #[serde(default)]
    pub dst_h: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ImageData {
    pub pane: PaneId,
    pub image: ImageId,
    pub width: u32,
    pub height: u32,
    /// Straight (non premultiplied) RGBA8.
    pub rgba: Vec<u8>,
}

// ---------------------------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------------------------

/// Modifier bits, using the kitty keyboard protocol's encoding.
pub mod mods {
    pub const SHIFT: u8 = 1;
    pub const ALT: u8 = 2;
    pub const CTRL: u8 = 4;
    pub const SUPER: u8 = 8;
    pub const HYPER: u8 = 16;
    pub const META: u8 = 32;
    pub const CAPS_LOCK: u8 = 64;
    pub const NUM_LOCK: u8 = 128;
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    /// Bitset of [`mods`].
    pub mods: u8,
    pub action: KeyAction,
    /// Text the key produces with the current modifiers and layout ("" for none).
    pub text: String,
    /// The key's base character without modifiers, when it has one (kitty "alternate keys").
    pub shifted: Option<char>,
    /// Base-layout (US) key for non-Latin layouts.
    pub base_layout: Option<char>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyAction {
    #[default]
    Press,
    Repeat,
    Release,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A key producing a character. Holds the *unshifted* lowercase codepoint (e.g. 'a' for
    /// Shift+A, '1' for Shift+1).
    Char(char),
    Named(NamedKey),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Escape,
    Enter,
    Tab,
    Backspace,
    Insert,
    Delete,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    /// F1..=F35.
    F(u8),
    Kp0,
    Kp1,
    Kp2,
    Kp3,
    Kp4,
    Kp5,
    Kp6,
    Kp7,
    Kp8,
    Kp9,
    KpDecimal,
    KpDivide,
    KpMultiply,
    KpSubtract,
    KpAdd,
    KpEnter,
    KpEqual,
    KpSeparator,
    KpLeft,
    KpRight,
    KpUp,
    KpDown,
    KpPageUp,
    KpPageDown,
    KpHome,
    KpEnd,
    KpInsert,
    KpDelete,
    KpBegin,
    MediaPlay,
    MediaPause,
    MediaPlayPause,
    MediaStop,
    MediaNext,
    MediaPrev,
    VolumeDown,
    VolumeUp,
    VolumeMute,
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub button: MouseButton,
    /// Viewport cell.
    pub col: u16,
    pub row: u16,
    /// Pointer is on the right half of the cell (selection side).
    pub right_half: bool,
    pub mods: u8,
    /// 1 = single, 2 = double (word), 3 = triple (line) click.
    pub clicks: u8,
    /// Pixel position inside the pane (SGR pixel mouse mode).
    pub x: u32,
    pub y: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    Move,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
    None,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollCmd {
    Lines(i32),
    PageUp,
    PageDown,
    Top,
    Bottom,
    /// Jump to the previous/next shell prompt (OSC 133).
    PrevPrompt,
    NextPrompt,
    /// Scroll so the display offset (lines scrolled back) is exactly this, clamped.
    Offset(u32),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionOp {
    /// Start a selection at a viewport cell.
    Start {
        col: u16,
        row: u16,
        right_half: bool,
        kind: SelectionKind,
    },
    Update {
        col: u16,
        row: u16,
        right_half: bool,
    },
    Clear,
    SelectAll,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Block,
    Word,
    Line,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchDirection {
    Forward,
    Backward,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct CaptureOpts {
    /// Only the last N lines (screen + scrollback). `None` = visible screen only,
    /// unless `scrollback` is set.
    pub lines: Option<u32>,
    pub scrollback: bool,
    /// Keep SGR escape sequences.
    pub ansi: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum WaitCondition {
    /// No output for at least `quiet_ms`.
    Idle { quiet_ms: u64 },
    /// Shell integration reports a prompt (the running command finished).
    Prompt,
    /// Process exited.
    Exit,
    /// Regex matches somewhere in the visible screen.
    Match { regex: String },
    /// A detected agent is idle or waiting for input.
    AgentFree,
    /// The agent in the pane has this status (Done/NeedsInput need hooks to be reliable).
    AgentStatus(AgentStatus),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    Satisfied,
    Timeout,
    Exited { code: Option<i32> },
}

/// Commands the UI executes; sent by the CLI (through the daemon) or generated by the daemon.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    /// Show an existing pane in a new tab (of the frontmost window, or a new window).
    NewTab {
        pane: PaneId,
        new_window: bool,
    },
    /// Split `target` (or the focused pane) and show `pane` in the new half.
    Split {
        target: Option<PaneId>,
        pane: PaneId,
        dir: SplitDir,
    },
    Focus {
        pane: PaneId,
    },
    SetTabTitle {
        pane: PaneId,
        title: Option<String>,
    },
    /// Scroll the pane's viewport (`thurm scroll`); the viewport lives in the app.
    Scroll {
        pane: PaneId,
        scroll: ScrollCmd,
    },
}

impl Frame {
    pub fn empty(pane: PaneId) -> Self {
        Frame {
            pane,
            ..Default::default()
        }
    }
}

/// Encode a request envelope as JSON (used by the CLI's `--json` mode and in tests).
pub fn to_json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("serializable")
}
