//! Thurm configuration.
//!
//! Loaded from `$XDG_CONFIG_HOME/thurm/config.toml` (falling back to `~/.config/thurm/config.toml`
//! on every platform, macOS included, like most terminals). Every field has a default, so an
//! empty or missing file is a valid configuration.

mod agents;
pub mod hooks;
mod themes;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use agents::{AgentDef, builtin_agents};
pub use themes::{Theme, builtin_theme, builtin_theme_names, is_own_theme};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Config {
    pub font: FontConfig,
    pub window: WindowConfig,
    pub cursor: CursorConfig,
    pub colors: ColorConfig,
    pub terminal: TerminalConfig,
    pub session: SessionConfig,
    pub security: SecurityConfig,
    pub notifications: NotificationConfig,
    pub agents: AgentsConfig,
    pub ai: AiConfig,
    pub updates: UpdatesConfig,
    pub quick_terminal: QuickTerminalConfig,
    /// Extra key bindings: `"cmd+shift+d" = "split_down"`.
    pub keybindings: std::collections::BTreeMap<String, String>,
    /// Other machines running `thurmd`, shown as remote workspaces (`[[remote]]`).
    pub remote: Vec<RemoteConfig>,
}

/// A host whose `thurmd` the app attaches to over the system `ssh`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RemoteConfig {
    /// Shown in the sidebar and used by `thurm --remote NAME`: letters, digits, `-`, `_`.
    pub name: String,
    /// ssh target: a `Host` alias, `user@host` or `ssh://user@host:port`.
    pub host: String,
    /// The remote daemon's socket; discovered with `thurm socket-path` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// OSC 52 clipboard reads by this host's programs: ask every time, always allow, or never.
    #[serde(default)]
    pub clipboard_read: ClipboardRead,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardRead {
    #[default]
    Ask,
    Always,
    Never,
}

/// The name the app gives this machine's own daemon; no `[[remote]]` may use it.
pub const LOCAL_HOST: &str = "local";

/// Checks a `[[remote]]` name (it names files and appears in commands).
pub fn validate_remote_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 32 {
        return Err(format!("remote name {name:?} must be 1-32 characters"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "remote name {name:?} may only contain letters, digits, '-' and '_'"
        ));
    }
    if name == LOCAL_HOST {
        return Err(format!("{LOCAL_HOST:?} is reserved for this machine"));
    }
    Ok(())
}

impl Config {
    pub fn remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remote.iter().find(|r| r.name == name)
    }

    fn validate(&self) -> Result<(), String> {
        // Numbers the app turns into sizes and pixel counts: a value out of these ranges would
        // be saved and then break (or crash) every launch.
        let f = &self.font;
        let w = &self.window;
        let q = &self.quick_terminal;
        let t = &self.terminal;
        for (key, value, lo, hi) in [
            ("font.size", f.size, 4.0, 200.0),
            ("font.line_height", f.line_height, 0.5, 4.0),
            ("font.letter_spacing", f.letter_spacing, -20.0, 100.0),
            ("window.padding_x", w.padding_x, 0.0, 200.0),
            ("window.padding_y", w.padding_y, 0.0, 200.0),
            ("window.opacity", w.opacity, 0.0, 1.0),
            (
                "window.unfocused_split_dim",
                w.unfocused_split_dim,
                0.0,
                1.0,
            ),
            ("window.sidebar_width", w.sidebar_width, 0.0, 2000.0),
            ("cursor.thickness", self.cursor.thickness, 0.0, 20.0),
            (
                "terminal.scroll_multiplier",
                t.scroll_multiplier,
                0.01,
                100.0,
            ),
            ("quick_terminal.size", q.size, 0.0, 1.0),
            (
                "quick_terminal.animation_duration",
                q.animation_duration,
                0.0,
                5.0,
            ),
            ("quick_terminal.opacity", q.opacity.unwrap_or(1.0), 0.0, 1.0),
        ] {
            check_range(key, value, lo, hi)?;
        }
        for (key, value, hi) in [
            ("window.blur", u64::from(w.blur), 100),
            ("window.columns", u64::from(w.columns), 1000),
            ("window.rows", u64::from(w.rows), 1000),
            ("window.sidebar_agent_rows", u64::from(w.sidebar_agent_rows), 100),
            ("terminal.scrollback", t.scrollback as u64, 10_000_000),
            (
                "terminal.image_memory_mib",
                t.image_memory_mib as u64,
                16_384,
            ),
            (
                "session.scrollback_lines",
                self.session.scrollback_lines as u64,
                1_000_000,
            ),
        ] {
            if value > hi {
                return Err(format!("{key} = {value} is out of range (at most {hi})"));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for r in &self.remote {
            validate_remote_name(&r.name)?;
            if r.host.trim().is_empty() {
                return Err(format!("remote {:?} has an empty host", r.name));
            }
            if r.host.starts_with('-') {
                return Err(format!("remote {:?}: host must not start with '-'", r.name));
            }
            if !seen.insert(r.name.as_str()) {
                return Err(format!("remote {:?} is defined twice", r.name));
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct FontConfig {
    pub family: String,
    pub size: f64,
    /// Render programming ligatures (`calt`/`liga`). Disabling also sets `-calt -liga`.
    pub ligatures: bool,
    /// Extra OpenType features, e.g. `["ss01", "zero", "-dlig"]`.
    pub features: Vec<String>,
    /// Line height multiplier.
    pub line_height: f64,
    /// Extra horizontal spacing in points.
    pub letter_spacing: f64,
    /// Faces for bold / italic / bold italic text (a family or a face name such as
    /// "TX-02 SemiBold ExtraCondensed"); unset = derived from `family`.
    pub family_bold: Option<String>,
    pub family_italic: Option<String>,
    pub family_bold_italic: Option<String>,
    /// Use CoreText font smoothing / "thicken" strokes on dark backgrounds.
    pub thicken: bool,
    /// How much thickening: 0 (none) to 255 (CoreText's full smoothing), like Ghostty.
    pub thicken_strength: u8,
    /// Fallback families tried before the system cascade list.
    pub fallback: Vec<String>,
    /// Fall back to the bundled "Symbols Nerd Font Mono" for icons (Nerd Font code points),
    /// scaled to the cell, so unpatched fonts show Nerd Font glyphs like in Ghostty.
    pub nerd_font_symbols: bool,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "JetBrains Mono".into(),
            size: 16.0,
            ligatures: true,
            features: Vec::new(),
            line_height: 1.0,
            letter_spacing: 0.0,
            family_bold: None,
            family_italic: None,
            family_bold_italic: None,
            thicken: true,
            thicken_strength: 255,
            fallback: vec!["SF Mono".into(), "Menlo".into()],
            nerd_font_symbols: true,
        }
    }
}

/// How a window shows its tabs.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TabStyle {
    /// Native macOS tabs in the titlebar.
    Native,
    /// A vertical sidebar (grouped by repository, with agent status and git info), like tty7.
    #[default]
    Sidebar,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OptionAsAlt {
    #[default]
    None,
    Left,
    Right,
    Both,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WindowConfig {
    pub padding_x: f64,
    pub padding_y: f64,
    /// Background opacity (0..1).
    pub opacity: f64,
    /// Background blur radius (requires opacity < 1).
    pub blur: u32,
    pub option_as_alt: OptionAsAlt,
    /// Initial size of new windows, in cells.
    pub columns: u16,
    pub rows: u16,
    /// Ask before closing a tab/window with a running process.
    pub confirm_close: bool,
    /// Quit the app when its last window closes (the daemon and the session keep running).
    pub quit_after_last_window: bool,
    /// Dim unfocused split panes (0 = off, 1 = fully).
    pub unfocused_split_dim: f64,
    /// Show the per-pane header strip with agent status in splits.
    pub pane_headers: bool,
    pub tab_style: TabStyle,
    /// Width of the tab sidebar in points.
    pub sidebar_width: f64,
    /// Rows the sidebar's agents panel shows before it scrolls (dragging its header sets it).
    pub sidebar_agent_rows: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            padding_x: 8.0,
            padding_y: 6.0,
            opacity: 1.0,
            blur: 0,
            option_as_alt: OptionAsAlt::None,
            columns: 110,
            rows: 32,
            confirm_close: true,
            quit_after_last_window: false,
            unfocused_split_dim: 0.25,
            pane_headers: false,
            tab_style: TabStyle::Sidebar,
            sidebar_width: 240.0,
            sidebar_agent_rows: 5,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    #[default]
    Block,
    Beam,
    Underline,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CursorConfig {
    pub style: CursorStyle,
    pub blink: bool,
    /// Beam / underline thickness in points.
    pub thickness: f64,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            style: CursorStyle::Block,
            blink: false,
            thickness: 2.0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct ColorConfig {
    /// Built-in theme name or path to a TOML theme file, or `light:NAME,dark:NAME` to follow
    /// the system appearance (see [`ThemeSpec`]).
    pub theme: Option<String>,
    pub foreground: Option<String>,
    pub background: Option<String>,
    pub cursor: Option<String>,
    pub cursor_text: Option<String>,
    pub selection_foreground: Option<String>,
    pub selection_background: Option<String>,
    /// Up to 16 ANSI palette overrides.
    pub palette: Vec<String>,
    /// Draw bold text with the bright palette variant.
    pub bold_is_bright: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Osc52Mode {
    Disabled,
    #[default]
    Copy,
    Paste,
    CopyPaste,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TerminalConfig {
    /// Scrollback lines kept per pane.
    pub scrollback: usize,
    /// Command to run instead of the login shell, e.g. `["/opt/homebrew/bin/fish", "-l"]`.
    pub shell: Option<Vec<String>>,
    pub env: std::collections::BTreeMap<String, String>,
    /// `TERM` value exported to programs.
    pub term: String,
    /// Inject shell integration (OSC 133 prompt marks, OSC 7 cwd) into zsh, bash and fish.
    pub shell_integration: bool,
    pub osc52: Osc52Mode,
    /// Enable the kitty keyboard protocol.
    pub kitty_keyboard: bool,
    /// Enable the kitty graphics protocol.
    pub kitty_graphics: bool,
    /// Max memory for kitty graphics per pane, in MiB.
    pub image_memory_mib: usize,
    /// Characters that end a word for double-click selection.
    pub word_separators: String,
    /// Copy to the clipboard as soon as a selection is made.
    pub copy_on_select: bool,
    /// Lines scrolled per line of wheel/trackpad movement (1 for mouse-reporting and
    /// alternate-screen apps).
    pub scroll_multiplier: f64,
    /// Trackpad scrolling moves the scrollback by fractions of a line.
    pub smooth_scroll: bool,
    /// Tab at a shell prompt shows Thurm's completion menu (commands, flags with descriptions,
    /// paths); Tab goes to the shell when there is nothing to offer.
    pub tab_completion: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback: 10_000,
            shell: None,
            env: Default::default(),
            term: "xterm-256color".into(),
            shell_integration: true,
            osc52: Osc52Mode::Copy,
            kitty_keyboard: true,
            kitty_graphics: true,
            image_memory_mib: 320,
            word_separators: ",│`|:\"' ()[]{}<>\t".into(),
            copy_on_select: false,
            scroll_multiplier: 3.0,
            smooth_scroll: true,
            tab_completion: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum QuitBehavior {
    /// Closing the app leaves shells running in the daemon; they reattach on next launch.
    #[default]
    Detach,
    /// Quitting the app terminates every shell (the layout is still restored next time).
    Terminate,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SessionConfig {
    /// Save layout, cwd and scrollback to disk and restore them after a reboot.
    pub persist: bool,
    /// Lines of scrollback per pane written to the snapshot.
    pub scrollback_lines: usize,
    pub snapshot_interval_secs: u64,
    pub quit: QuitBehavior,
    /// When a restored pane was running an agent with a `resume` command, run it again.
    pub resume_agents: bool,
    /// Exit the daemon this many seconds after the last pane closed (0 = never).
    pub daemon_idle_exit_secs: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            persist: true,
            scrollback_lines: 4000,
            snapshot_interval_secs: 15,
            quit: QuitBehavior::Detach,
            resume_agents: true,
            daemon_idle_exit_secs: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SecurityConfig {
    /// Turn on macOS Secure Keyboard Entry automatically while a password prompt is detected.
    pub auto_secure_input: bool,
    /// Show a lock badge in the pane while secure input is active.
    pub secure_input_indicator: bool,
    /// Ask before pasting text containing newlines into a shell outside bracketed paste.
    pub confirm_multiline_paste: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            auto_secure_input: true,
            secure_input_indicator: true,
            confirm_multiline_paste: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct NotificationConfig {
    pub enabled: bool,
    /// Notify when a detected agent needs input and the pane isn't focused.
    pub agent_needs_input: bool,
    /// Notify when an agent finishes a turn in an unfocused pane (needs `thurm hooks install`).
    pub agent_done: bool,
    /// Notify when a command running longer than this finishes in an unfocused pane (0 = off).
    pub command_finished_secs: u64,
    /// Honor OSC 9 / OSC 99 / OSC 777 notifications from programs.
    pub program_notifications: bool,
    pub bell_badge: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            agent_needs_input: true,
            agent_done: true,
            command_finished_secs: 15,
            program_notifications: true,
            bell_badge: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AgentsConfig {
    pub detect: bool,
    /// Milliseconds without output after which a working agent counts as idle.
    pub idle_after_ms: u64,
    /// Name tabs after what the agent session is about (Claude Code's session title; needs
    /// `thurm hooks install`).
    pub session_titles: bool,
    /// Additional / overriding agent definitions.
    pub define: Vec<AgentDef>,
    /// Launch presets shown in the command palette. Defaults to the installed built-ins.
    pub presets: Vec<thurm_proto::AgentPreset>,
    /// Add Thurm's status hooks to an agent's settings when it is launched without them
    /// (`thurm hooks install` does the same by hand).
    pub install_hooks: bool,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            detect: true,
            idle_after_ms: 1500,
            session_titles: true,
            define: Vec::new(),
            presets: Vec::new(),
            install_hooks: true,
        }
    }
}

/// Apple Intelligence's on-device model (macOS 26+, Apple silicon, Apple Intelligence on).
/// Nothing leaves the Mac, but it is opt-in: `enabled` turns the features below on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AiConfig {
    pub enabled: bool,
    /// Name agent tabs after the task when the agent gives the session no title.
    pub titles: bool,
    /// Ask the model whether a quiet agent waits for an answer when no `attention` pattern
    /// matches (agents without hooks).
    pub status: bool,
    /// Say what an agent asks for, and sum up a finished turn, in notifications and the
    /// agent switcher.
    pub notifications: bool,
    /// Explain the last command's output on request (`thurm explain`, Edit > Explain Last
    /// Command).
    pub explain: bool,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            titles: true,
            status: true,
            notifications: true,
            explain: true,
        }
    }
}

impl AiConfig {
    pub fn titles(&self) -> bool {
        self.enabled && self.titles
    }

    pub fn status(&self) -> bool {
        self.enabled && self.status
    }

    pub fn notifications(&self) -> bool {
        self.enabled && self.notifications
    }

    pub fn explain(&self) -> bool {
        self.enabled && self.explain
    }
}

/// Automatic updates (Sparkle), read by the app.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct UpdatesConfig {
    pub channel: UpdateChannel,
    /// Look for updates in the background (hourly).
    pub check_automatically: bool,
    /// Download updates in the background and install them when Thurm quits.
    pub download_automatically: bool,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self {
            channel: UpdateChannel::Auto,
            check_automatically: true,
            download_automatically: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    /// The channel of the installed build: a tip build keeps getting tip builds.
    #[default]
    Auto,
    /// Tagged releases.
    Release,
    /// A build of every commit to main (and every release).
    Tip,
}

/// The quick terminal: a window that slides in from a screen edge on a global hotkey, read by
/// the app.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct QuickTerminalConfig {
    /// Global shortcut that shows and hides it, like `"ctrl+grave"`; empty for none (Window >
    /// Quick Terminal still works).
    pub hotkey: String,
    pub position: QuickTerminalPosition,
    /// Fraction of the screen it takes (its height at the top or bottom, its width at the
    /// left or right, both in the center).
    pub size: f64,
    /// Hide it when another app or another Thurm window takes focus.
    pub autohide: bool,
    pub screen: QuickTerminalScreen,
    /// Slide animation length in seconds (0 = none).
    pub animation_duration: f64,
    /// Background opacity (0..1); unset: `window.opacity`.
    pub opacity: Option<f64>,
}

impl Default for QuickTerminalConfig {
    fn default() -> Self {
        Self {
            hotkey: String::new(),
            position: QuickTerminalPosition::Top,
            size: 0.4,
            autohide: true,
            screen: QuickTerminalScreen::Mouse,
            animation_duration: 0.2,
            opacity: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QuickTerminalPosition {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
    Center,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QuickTerminalScreen {
    /// The screen with the mouse pointer.
    #[default]
    Mouse,
    /// The screen with the menu bar.
    Main,
}

#[derive(Debug)]
pub struct ConfigError {
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for ConfigError {}

/// `~/.config/thurm` (honoring `XDG_CONFIG_HOME` and `THURM_CONFIG_DIR`).
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("THURM_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("thurm");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".config")
        .join("thurm")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Per-user state directory for snapshots
/// (`~/Library/Application Support/Thurm` on macOS, `~/.local/state/thurm` elsewhere).
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("THURM_STATE_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "macos")]
    {
        dirs::data_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Thurm")
    }
    #[cfg(not(target_os = "macos"))]
    {
        dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(std::env::temp_dir)
            .join("thurm")
    }
}

/// Unix socket of the daemon. Short enough for `sun_path` (104 bytes on macOS).
///
/// In `$XDG_RUNTIME_DIR` when set; else on macOS in the per-user temporary directory, which no
/// other user can create or enter, on Linux in `/run/user/<uid>` when it exists (sessions
/// without logind, such as Tailscale SSH's, do not set the variable but must find the same
/// daemon), and elsewhere in `/tmp/thurm-<uid>` (checked by [`private_dir_ok`] before use). A
/// daemon started by an earlier Thurm keeps answering on the old `/tmp` socket until it exits,
/// and is used there meanwhile.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("THURM_SOCKET") {
        return PathBuf::from(p);
    }
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let preferred = socket_dir_for(xdg.as_deref(), &run_user_dir()).join("thurmd.sock");
    match xdg {
        Some(_) => preferred,
        None => live_socket(preferred, legacy_socket_dir().join("thurmd.sock")),
    }
}

/// `preferred`, unless only `legacy` (in a private directory) has a daemon of ours answering.
fn live_socket(preferred: PathBuf, legacy: PathBuf) -> PathBuf {
    if legacy != preferred
        && !socket_answers(&preferred)
        && legacy.parent().is_some_and(private_dir_ok)
        && socket_answers(&legacy)
    {
        legacy
    } else {
        preferred
    }
}

/// The directory of the default daemon socket (see [`socket_path`]).
pub fn default_socket_dir() -> PathBuf {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    socket_dir_for(xdg.as_deref(), &run_user_dir())
}

/// [`default_socket_dir`] for a session's `$XDG_RUNTIME_DIR` and the user's `/run/user/<uid>`.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn socket_dir_for(xdg_runtime_dir: Option<&Path>, run_user: &Path) -> PathBuf {
    if let Some(dir) = xdg_runtime_dir {
        return dir.join(format!("thurm-{}", uid()));
    }
    #[cfg(target_os = "macos")]
    if let Some(tmp) = darwin_user_temp_dir() {
        return tmp.join("thurm");
    }
    #[cfg(target_os = "linux")]
    if private_dir_ok(run_user) {
        return run_user.join(format!("thurm-{}", uid()));
    }
    legacy_socket_dir()
}

/// Where systemd-logind keeps the user's runtime directory (`$XDG_RUNTIME_DIR` in its sessions).
fn run_user_dir() -> PathBuf {
    PathBuf::from(format!("/run/user/{}", uid()))
}

fn legacy_socket_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/thurm-{}", uid()))
}

/// `confstr(_CS_DARWIN_USER_TEMP_DIR)`: `/var/folders/…/T/`, private to the user.
#[cfg(target_os = "macos")]
fn darwin_user_temp_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; 1024];
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n == 0 || n > buf.len() {
        return None;
    }
    buf.truncate(n - 1);
    let dir = PathBuf::from(std::ffi::OsString::from_vec(buf));
    dir.is_absolute().then_some(dir)
}

/// Whether a daemon of this user answers on `socket`.
fn socket_answers(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok_and(|s| peer_is_same_user(&s))
}

/// Whether `dir` is a real directory (not a symlink) owned by this user that nobody else can
/// enter: where the daemon socket may live.
pub fn private_dir_ok(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::symlink_metadata(dir).is_ok_and(|m| {
        m.file_type().is_dir() && m.uid() == uid() && m.permissions().mode() & 0o077 == 0
    })
}

/// Creates `dir` (mode 0700) if missing, and checks it with [`private_dir_ok`]: a directory
/// another user created first (in `/tmp`) must not hold our socket.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    if private_dir_ok(dir) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a private directory owned by you (0700, not a symlink)",
                dir.display()
            ),
        ))
    }
}

/// Whether the process at the other end of `stream` runs as this user.
pub fn peer_is_same_user(stream: &std::os::unix::net::UnixStream) -> bool {
    peer_uid(stream).is_some_and(|u| u == uid())
}

/// Whether the process at the other end of `stream` may use this user's daemon: this user, or
/// root, which can act as this user anyway. Tailscale SSH dials a forwarded socket from
/// `tailscaled`, which runs as root.
pub fn peer_may_use_daemon(stream: &std::os::unix::net::UnixStream) -> bool {
    peer_uid(stream).is_some_and(|u| u == uid() || u == 0)
}

fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        (libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        ) == 0)
            .then_some(cred.uid)
    }
    #[cfg(not(target_os = "linux"))]
    unsafe {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        (libc::getpeereid(fd, &mut uid, &mut gid) == 0).then_some(uid)
    }
}

/// Where the app keeps its end of each remote tunnel (`<name>.sock`) and the tunnel's state
/// (`<name>.state`, read by `thurm --remote`): `~/Library/Caches/Thurm/remote` on macOS,
/// `$XDG_CACHE_HOME/thurm/remote` elsewhere. `THURM_REMOTE_DIR` overrides it.
pub fn remote_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("THURM_REMOTE_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "macos")]
    let base = dirs::cache_dir().map(|d| d.join("Thurm"));
    #[cfg(not(target_os = "macos"))]
    let base = dirs::cache_dir().map(|d| d.join("thurm"));
    base.unwrap_or_else(std::env::temp_dir).join("remote")
}

/// The local end of `name`'s tunnel.
pub fn remote_socket_path(name: &str) -> PathBuf {
    remote_dir().join(format!("{name}.sock"))
}

/// The tunnel state the app writes for `name` (JSON, see `thurm-remote`).
pub fn remote_state_path(name: &str) -> PathBuf {
    remote_dir().join(format!("{name}.state"))
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

impl Config {
    /// Load the config file; a missing file yields the defaults.
    pub fn load() -> Result<Config, ConfigError> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|message| ConfigError {
                path: path.to_owned(),
                message,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(ConfigError {
                path: path.to_owned(),
                message: e.to_string(),
            }),
        }
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        Self::parse_with_warnings(text).map(|(cfg, _)| cfg)
    }

    /// Like [`Config::parse`], plus the keys it didn't know (a typo, or a setting of a newer
    /// Thurm): they are ignored, with a warning, instead of failing the whole file.
    pub fn parse_with_warnings(text: &str) -> Result<(Config, Vec<String>), String> {
        let mut unknown = Vec::new();
        let de = toml::Deserializer::parse(text).map_err(|e| e.to_string())?;
        let cfg: Config = serde_ignored::deserialize(de, |path| {
            unknown.push(format!("unknown setting `{path}` ignored"));
        })
        .map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok((cfg, unknown))
    }

    /// Like [`Config::parse`], but an unknown key is an error: for text Thurm writes (`thurm
    /// set`, remotes), where a typo must not be saved.
    pub fn parse_strict(text: &str) -> Result<Config, String> {
        let (cfg, warnings) = Self::parse_with_warnings(text)?;
        match warnings.first() {
            Some(w) => Err(w.clone()),
            None => Ok(cfg),
        }
    }

    /// Like [`Config::load`], with the warnings of [`Config::parse_with_warnings`].
    pub fn load_with_warnings() -> Result<(Config, Vec<String>), ConfigError> {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse_with_warnings(&text).map_err(|message| ConfigError {
                path: path.clone(),
                message,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::default(), vec![])),
            Err(e) => Err(ConfigError {
                path,
                message: e.to_string(),
            }),
        }
    }

    pub fn theme_spec(&self) -> ThemeSpec {
        ThemeSpec::parse(self.colors.theme.as_deref().unwrap_or(DEFAULT_THEME))
    }

    /// The effective color theme for the given system appearance: the named theme with
    /// per-color overrides applied.
    pub fn theme_for(&self, dark: bool) -> Theme {
        let mut theme = load_theme(self.theme_spec().name(dark))
            .unwrap_or_else(|_| builtin_theme(DEFAULT_THEME).expect("default theme"));
        let c = &self.colors;
        let set = |slot: &mut u32, v: &Option<String>| {
            if let Some(rgb) = v.as_deref().and_then(parse_color) {
                *slot = rgb;
            }
        };
        set(&mut theme.foreground, &c.foreground);
        set(&mut theme.background, &c.background);
        set(&mut theme.cursor, &c.cursor);
        set(&mut theme.cursor_text, &c.cursor_text);
        set(&mut theme.selection_foreground, &c.selection_foreground);
        set(&mut theme.selection_background, &c.selection_background);
        for (i, v) in c.palette.iter().take(16).enumerate() {
            if let Some(rgb) = parse_color(v) {
                theme.palette[i] = rgb;
            }
        }
        theme
    }

    /// Built-in agent definitions merged with user definitions (same `kind` overrides).
    pub fn agent_defs(&self) -> Vec<AgentDef> {
        let mut defs = builtin_agents();
        for user in &self.agents.define {
            if let Some(existing) = defs.iter_mut().find(|d| d.kind == user.kind) {
                *existing = user.clone();
            } else {
                defs.push(user.clone());
            }
        }
        defs
    }

    /// Launch presets: explicit ones from the config, otherwise one per built-in agent
    /// whose executable is on `PATH`.
    pub fn agent_presets(&self) -> Vec<thurm_proto::AgentPreset> {
        if !self.agents.presets.is_empty() {
            return self.agents.presets.clone();
        }
        self.agent_defs()
            .into_iter()
            .filter_map(|d| {
                let launch = d.launch.clone()?;
                let exe = launch.first()?;
                which(exe)?;
                Some(thurm_proto::AgentPreset {
                    name: d.name,
                    command: launch,
                })
            })
            .collect()
    }

    /// OpenType feature list handed to CoreText.
    pub fn font_features(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.font.ligatures {
            out.push("-calt".to_owned());
            out.push("-liga".to_owned());
            out.push("-dlig".to_owned());
        }
        out.extend(self.font.features.iter().cloned());
        out
    }

    /// JSON consumed by the Swift UI (theme resolved for the given appearance included).
    pub fn ui_json(&self, dark: bool) -> String {
        #[derive(Serialize)]
        struct Ui<'a> {
            config: &'a Config,
            theme: Theme,
            theme_spec: ThemeSpec,
            themes: Vec<ThemeInfo>,
            font_features: Vec<String>,
            agent_presets: Vec<thurm_proto::AgentPreset>,
            config_path: String,
        }
        serde_json::to_string(&Ui {
            config: self,
            theme: self.theme_for(dark),
            theme_spec: self.theme_spec(),
            themes: theme_list(),
            font_features: self.font_features(),
            agent_presets: self.agent_presets(),
            config_path: config_path().display().to_string(),
        })
        .expect("serializable")
    }
}

pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Locate an executable on `PATH` (plus common macOS install locations that GUI apps miss).
pub fn which(exe: &str) -> Option<PathBuf> {
    if exe.contains('/') {
        let p = expand_home(exe);
        return p.is_file().then_some(p);
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for extra in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "~/.local/bin",
        "~/.npm-global/bin",
        "~/.bun/bin",
        "~/.cargo/bin",
    ] {
        dirs.push(expand_home(extra));
    }
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file())
}

/// Parse `#rrggbb`, `rrggbb`, `0xrrggbb` or `#rgb`.
pub fn parse_color(s: &str) -> Option<u32> {
    let s = s.trim();
    let hex = s
        .strip_prefix('#')
        .or_else(|| s.strip_prefix("0x"))
        .unwrap_or(s);
    match hex.len() {
        6 => u32::from_str_radix(hex, 16).ok(),
        3 => {
            let v = u32::from_str_radix(hex, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
            Some((r * 17) << 16 | (g * 17) << 8 | (b * 17))
        }
        _ => None,
    }
}

pub const DEFAULT_THEME: &str = "thurm";

/// The `colors.theme` value: one theme, or Ghostty-style `light:NAME,dark:NAME` to follow the
/// system appearance. Both halves are equal for a single theme.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ThemeSpec {
    pub light: String,
    pub dark: String,
}

impl ThemeSpec {
    pub fn single(name: &str) -> Self {
        Self {
            light: name.to_owned(),
            dark: name.to_owned(),
        }
    }

    pub fn parse(s: &str) -> Self {
        let (mut light, mut dark, mut plain) = (None, None, None);
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if let Some(v) = part.strip_prefix("light:") {
                light = Some(v.trim().to_owned());
            } else if let Some(v) = part.strip_prefix("dark:") {
                dark = Some(v.trim().to_owned());
            } else {
                plain = Some(part.to_owned());
            }
        }
        let fallback = plain
            .or_else(|| dark.clone())
            .or_else(|| light.clone())
            .unwrap_or_else(|| DEFAULT_THEME.to_owned());
        Self {
            light: light.unwrap_or_else(|| fallback.clone()),
            dark: dark.unwrap_or(fallback),
        }
    }

    /// Checks that both halves name a built-in theme or a readable theme file.
    pub fn validate(&self) -> Result<(), String> {
        load_theme(&self.light)?;
        load_theme(&self.dark)?;
        Ok(())
    }

    pub fn follows_appearance(&self) -> bool {
        self.light != self.dark
    }

    pub fn name(&self, dark: bool) -> &str {
        if dark { &self.dark } else { &self.light }
    }
}

impl std::fmt::Display for ThemeSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.follows_appearance() {
            write!(f, "light:{},dark:{}", self.light, self.dark)
        } else {
            f.write_str(&self.dark)
        }
    }
}

/// A built-in theme by name, or a theme file by path.
pub fn load_theme(name: &str) -> Result<Theme, String> {
    if let Some(t) = builtin_theme(name) {
        return Ok(t);
    }
    Theme::load_file(&expand_home(name)).map_err(|e| format!("unknown theme {name:?}: {e}"))
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    pub dark: bool,
    /// One of Thurm's own themes, rather than from the bundled collection.
    pub own: bool,
}

/// Built-in themes (Thurm's own, then the collection) with their light/dark classification.
pub fn theme_list() -> Vec<ThemeInfo> {
    builtin_theme_names()
        .into_iter()
        .filter_map(|n| builtin_theme(&n))
        .map(|t| ThemeInfo {
            dark: t.is_dark(),
            own: themes::is_own_theme(&t.name),
            name: t.name,
        })
        .collect()
}

/// Returns `text` (a config file) with `colors.theme` set to `spec`, keeping comments and
/// formatting. Fails if the result would not be a valid config.
pub fn with_theme(text: &str, spec: &ThemeSpec) -> Result<String, String> {
    with_setting(text, "colors.theme", &format!("{:?}", spec.to_string()))
}

/// Returns `text` (a config file) with `key` ("section.name") set to `value`, a TOML value
/// literal (`"sidebar"`, `240.0`, `true`), keeping comments and formatting. Fails if the
/// result is not a valid config.
pub fn with_setting(text: &str, key: &str, value: &str) -> Result<String, String> {
    let (section, name) = key
        .split_once('.')
        .filter(|(s, n)| !s.is_empty() && !n.is_empty() && !n.contains('.'))
        .ok_or_else(|| format!("expected section.key, got {key:?}"))?;
    let value: toml_edit::Value = value.parse().map_err(|e| format!("{value:?}: {e}"))?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let table = doc
        .entry(section)
        .or_insert_with(toml_edit::table)
        .as_table_like_mut()
        .ok_or_else(|| format!("[{section}] is not a table"))?;
    match table.get_mut(name).and_then(toml_edit::Item::as_value_mut) {
        // Replace just the value: the key keeps its leading comment, the value its trailing one.
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = value;
            *existing.decor_mut() = decor;
        }
        None => {
            table.insert(name, toml_edit::Item::Value(value));
        }
    }
    let out = doc.to_string();
    check_edit(text, &out)?;
    Ok(out)
}

/// A command-line value as a TOML literal: anything TOML parses as a value is used as is
/// (`true`, `240.0`, `"x"`), a bare word becomes a string.
pub fn value_literal(v: &str) -> String {
    if v.parse::<toml_edit::Value>().is_ok() {
        v.to_owned()
    } else {
        toml_edit::Value::from(v).to_string()
    }
}

/// Persist `spec` as the theme in the config file (created from the default if missing).
pub fn write_theme(spec: &ThemeSpec) -> Result<(), String> {
    write_setting("colors.theme", &value_literal(&spec.to_string()))
}

/// Persist one setting (see [`with_setting`]) in the config file.
pub fn write_setting(key: &str, value: &str) -> Result<(), String> {
    edit_config(|text| with_setting(text, key, value))
}

/// Returns `text` (a config file) with a `[[remote]]` entry for `remote` added, keeping
/// comments and formatting. Fails when the name is taken or the result is invalid.
pub fn with_remote_added(text: &str, remote: &RemoteConfig) -> Result<String, String> {
    validate_remote_name(&remote.name)?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let item = doc
        .entry("remote")
        .or_insert_with(|| toml_edit::Item::ArrayOfTables(Default::default()));
    let tables = item
        .as_array_of_tables_mut()
        .ok_or("`remote` is not an array of tables ([[remote]])")?;
    if tables
        .iter()
        .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(remote.name.as_str()))
    {
        return Err(format!("a remote named {:?} exists already", remote.name));
    }
    let mut t = toml_edit::Table::new();
    t["name"] = toml_edit::value(remote.name.as_str());
    t["host"] = toml_edit::value(remote.host.as_str());
    if let Some(sock) = &remote.socket {
        t["socket"] = toml_edit::value(sock.as_str());
    }
    if !remote.enabled {
        t["enabled"] = toml_edit::value(false);
    }
    if remote.clipboard_read != ClipboardRead::Ask {
        t["clipboard_read"] = toml_edit::value(match remote.clipboard_read {
            ClipboardRead::Always => "always",
            ClipboardRead::Never => "never",
            ClipboardRead::Ask => "ask",
        });
    }
    tables.push(t);
    let out = doc.to_string();
    check_edit(text, &out)?;
    Ok(out)
}

/// Returns `text` without the `[[remote]]` entry named `name`.
pub fn with_remote_removed(text: &str, name: &str) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let tables = doc
        .get_mut("remote")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .ok_or_else(|| format!("no remote named {name:?}"))?;
    let before = tables.len();
    tables.retain(|t| t.get("name").and_then(|v| v.as_str()) != Some(name));
    if tables.len() == before {
        return Err(format!("no remote named {name:?}"));
    }
    if tables.is_empty() {
        doc.remove("remote");
    }
    let out = doc.to_string();
    check_edit(text, &out)?;
    Ok(out)
}

/// Returns `text` with `key` of the `[[remote]]` named `name` set to `value` (a TOML literal).
pub fn with_remote_setting(
    text: &str,
    name: &str,
    key: &str,
    value: &str,
) -> Result<String, String> {
    let value: toml_edit::Value = value.parse().map_err(|e| format!("{value:?}: {e}"))?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let table = doc
        .get_mut("remote")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .and_then(|ts| {
            ts.iter_mut()
                .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
        })
        .ok_or_else(|| format!("no remote named {name:?}"))?;
    table[key] = toml_edit::Item::Value(value);
    let out = doc.to_string();
    check_edit(text, &out)?;
    Ok(out)
}

/// Rewrites the config file with `edit` applied to its text (created from the default if
/// missing), atomically.
pub fn edit_config(edit: impl FnOnce(&str) -> Result<String, String>) -> Result<(), String> {
    let path = ensure_default_config().map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let out = edit(&text)?;
    replace_file(&path, out.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

/// Replaces the contents of `path` atomically, keeping what the user set up around it: a
/// symlink (a Home Manager or dotfiles link) stays a link and its target gets the new
/// contents, and the file keeps its permissions (a new file is private).
pub fn replace_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let target = match std::fs::canonicalize(path) {
        Ok(t) => t,
        // A link to a file that doesn't exist yet: create that file, keep the link.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Follow the chain of links to the missing file at its end.
            let mut at = path.to_owned();
            for _ in 0..40 {
                match std::fs::symlink_metadata(&at) {
                    Ok(m) if m.file_type().is_symlink() => {
                        let link = std::fs::read_link(&at)?;
                        at = match at.parent() {
                            Some(dir) if link.is_relative() => dir.join(link),
                            _ => link,
                        };
                    }
                    _ => break,
                }
            }
            at
        }
        Err(e) => return Err(e),
    };
    let mode = std::fs::metadata(&target)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = target.with_file_name(format!(".{name}.tmp{}-{seq}", std::process::id()));
    // Only a file this call created is removed on failure.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| {
        f.write_all(contents)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Validates `after`, an edit of the config text `before`: it must parse, and add no unknown
/// key (a typo in `thurm set`). Unknown keys `before` already had (settings of a newer build)
/// stay.
fn check_edit(before: &str, after: &str) -> Result<(), String> {
    let (_, new) = Config::parse_with_warnings(after)?;
    let old = Config::parse_with_warnings(before)
        .map(|(_, w)| w)
        .unwrap_or_default();
    match new.into_iter().find(|w| !old.contains(w)) {
        Some(w) => Err(w),
        None => Ok(()),
    }
}

fn check_range(key: &str, value: f64, lo: f64, hi: f64) -> Result<(), String> {
    if value.is_finite() && (lo..=hi).contains(&value) {
        Ok(())
    } else {
        Err(format!("{key} = {value} is out of range ({lo} to {hi})"))
    }
}

/// Commented default config written on first launch.
pub const DEFAULT_CONFIG_TOML: &str = include_str!("default_config.toml");

/// Write the default config if none exists. Returns the path.
pub fn ensure_default_config() -> std::io::Result<PathBuf> {
    let path = config_path();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, DEFAULT_CONFIG_TOML)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_parse_and_validate() {
        let cfg = Config::parse(
            r#"
[[remote]]
name = "devbox"
host = "nkl@devbox"

[[remote]]
name = "mini"
host = "ssh://me@mini.local:2222"
socket = "/tmp/x.sock"
enabled = false
clipboard_read = "always"
"#,
        )
        .unwrap();
        assert_eq!(cfg.remote.len(), 2);
        let d = cfg.remote("devbox").unwrap();
        assert!(d.enabled && d.socket.is_none() && d.clipboard_read == ClipboardRead::Ask);
        let m = cfg.remote("mini").unwrap();
        assert!(!m.enabled && m.clipboard_read == ClipboardRead::Always);
        for bad in [
            "[[remote]]\nname = \"local\"\nhost = \"x\"",
            "[[remote]]\nname = \"a/b\"\nhost = \"x\"",
            "[[remote]]\nname = \"a\"\nhost = \"-oProxyCommand=x\"",
            "[[remote]]\nname = \"a\"\nhost = \"x\"\n[[remote]]\nname = \"a\"\nhost = \"y\"",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        // An unknown key is ignored when loading, refused when Thurm writes the file.
        let typo = "[[remote]]\nname = \"a\"\nhost = \"x\"\nport = 1";
        assert_eq!(Config::parse_with_warnings(typo).unwrap().1.len(), 1);
        assert!(Config::parse_strict(typo).is_err());
        // Edits keep unknown keys the file already had, and refuse new ones.
        let newer = "[window]\nfuture_setting = true\n";
        let out = with_setting(newer, "font.size", "15.0").unwrap();
        assert!(out.contains("future_setting"));
        assert!(with_remote_setting(typo, "a", "enabled", "false").is_ok());
        assert!(with_setting(newer, "window.nope", "1").is_err());
    }

    #[test]
    fn remotes_added_and_removed_keeping_comments() {
        let text = "# my config\n[font]\nsize = 14.0 # big\n";
        let r = RemoteConfig {
            name: "devbox".into(),
            host: "devbox".into(),
            socket: None,
            enabled: true,
            clipboard_read: ClipboardRead::Ask,
        };
        let added = with_remote_added(text, &r).unwrap();
        assert!(added.starts_with("# my config") && added.contains("# big"));
        assert_eq!(Config::parse(&added).unwrap().remote, vec![r.clone()]);
        assert!(with_remote_added(&added, &r).is_err(), "names are unique");
        let always = with_remote_setting(&added, "devbox", "clipboard_read", "\"always\"").unwrap();
        assert_eq!(
            Config::parse(&always).unwrap().remote[0].clipboard_read,
            ClipboardRead::Always
        );
        assert!(with_remote_setting(&added, "devbox", "clipboard_read", "\"maybe\"").is_err());
        let removed = with_remote_removed(&always, "devbox").unwrap();
        assert!(Config::parse(&removed).unwrap().remote.is_empty());
        assert!(!removed.contains("[[remote]]"));
        assert!(with_remote_removed(&removed, "devbox").is_err());
    }

    #[test]
    fn empty_is_default() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn update_settings() {
        assert_eq!(Config::default().updates.channel, UpdateChannel::Auto);
        let c =
            Config::parse("[updates]\nchannel = \"tip\"\ndownload_automatically = true").unwrap();
        assert_eq!(c.updates.channel, UpdateChannel::Tip);
        assert!(c.updates.check_automatically && c.updates.download_automatically);
        assert!(Config::parse("[updates]\nchannel = \"nightly\"").is_err());
        // The menu writes the channel into configs from before [updates] existed.
        let text = with_setting("[font]\nsize = 13\n", "updates.channel", "\"release\"").unwrap();
        assert_eq!(
            Config::parse(&text).unwrap().updates.channel,
            UpdateChannel::Release
        );
    }

    #[test]
    fn default_config_file_parses() {
        let c = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(c.font.size, Config::default().font.size);
    }

    #[test]
    fn unconfigured_font_is_bundled_jetbrains_mono_at_16pt() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.font.family, "JetBrains Mono");
        assert_eq!(c.font.size, 16.0);
        let bundled = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../macos/Resources/fonts/JetBrainsMono-Regular.ttf"
        );
        assert!(std::path::Path::new(bundled).exists(), "missing {bundled}");
    }

    #[test]
    fn overrides_and_features() {
        let c = Config::parse(
            r##"
            [font]
            family = "Fira Code"
            ligatures = false
            features = ["ss01"]
            [colors]
            theme = "tokyo-night"
            background = "#000000"
            palette = ["#111111"]
            [[agents.define]]
            kind = "claude"
            name = "Claude"
            processes = ["claude"]
            "##,
        )
        .unwrap();
        assert_eq!(c.font_features(), vec!["-calt", "-liga", "-dlig", "ss01"]);
        let t = c.theme_for(true);
        assert_eq!(t.background, 0);
        assert_eq!(t.palette[0], 0x111111);
        let defs = c.agent_defs();
        assert_eq!(defs.iter().filter(|d| d.kind == "claude").count(), 1);
        let claude = defs.iter().find(|d| d.kind == "claude").unwrap();
        assert_eq!(claude.name, "Claude");
        // Fields added later (`working_title`) default when a definition leaves them out.
        assert!(claude.working_title.is_empty());
        assert!(c.ui_json(true).contains("\"theme\""));
    }

    #[test]
    fn theme_spec_parsing() {
        let s = ThemeSpec::parse("light:catppuccin-latte, dark:catppuccin-mocha");
        assert!(s.follows_appearance());
        assert_eq!(s.name(false), "catppuccin-latte");
        assert_eq!(s.name(true), "catppuccin-mocha");
        assert_eq!(
            s.to_string(),
            "light:catppuccin-latte,dark:catppuccin-mocha"
        );
        let s = ThemeSpec::parse("nord");
        assert!(!s.follows_appearance());
        assert_eq!(s.to_string(), "nord");
        assert_eq!(ThemeSpec::parse("dark:nord").name(false), "nord");
        assert_eq!(ThemeSpec::parse("").name(true), DEFAULT_THEME);
        assert!(ThemeSpec::parse("light:nord,dark:thurm").validate().is_ok());
        assert!(ThemeSpec::parse("light:nord,dark:nope").validate().is_err());

        let c = Config::parse("[colors]\ntheme = \"light:thurm-light,dark:dracula\"").unwrap();
        assert_eq!(c.theme_for(false).name, "thurm-light");
        assert_eq!(c.theme_for(true).name, "dracula");
    }

    #[test]
    fn settings_written_and_validated() {
        let out = with_setting(DEFAULT_CONFIG_TOML, "window.tab_style", "\"native\"").unwrap();
        assert_eq!(
            Config::parse(&out).unwrap().window.tab_style,
            TabStyle::Native
        );
        assert!(out.contains("# Tabs:"), "comments kept");
        let out = with_setting(&out, "window.sidebar_width", "300.0").unwrap();
        assert_eq!(Config::parse(&out).unwrap().window.sidebar_width, 300.0);
        assert!(with_setting(&out, "window.tab_style", "\"diagonal\"").is_err());
        assert!(with_setting(&out, "window.nope", "1").is_err());
        assert!(with_setting(&out, "tab_style", "1").is_err());
        assert_eq!(value_literal("sidebar"), "\"sidebar\"");
        assert_eq!(value_literal("300.0"), "300.0");
        assert_eq!(value_literal("true"), "true");
    }

    #[test]
    fn themes_classified() {
        let list = theme_list();
        let dark = |n: &str| list.iter().find(|t| t.name == n).unwrap().dark;
        assert!(dark("thurm") && dark("catppuccin-mocha") && dark("solarized-dark"));
        assert!(!dark("thurm-light") && !dark("catppuccin-latte"));
    }

    #[test]
    fn theme_written_preserving_comments() {
        let spec = ThemeSpec::single("nord");
        let out = with_theme(DEFAULT_CONFIG_TOML, &spec).unwrap();
        assert!(out.contains("# Built-in: thurm"));
        assert_eq!(
            Config::parse(&out).unwrap().colors.theme.as_deref(),
            Some("nord")
        );

        let out = with_theme("[font]\nsize = 14.0\n", &ThemeSpec::parse("light:a,dark:b")).unwrap();
        assert!(out.contains("size = 14.0"));
        assert_eq!(
            Config::parse(&out).unwrap().colors.theme.as_deref(),
            Some("light:a,dark:b")
        );
        let out = with_theme(&out, &spec).unwrap();
        assert_eq!(out.matches("theme =").count(), 1);
    }

    #[test]
    fn ai_is_opt_in() {
        let c = Config::default();
        assert!(!c.ai.titles() && !c.ai.status() && !c.ai.notifications() && !c.ai.explain());
        let c = Config::parse("[ai]\nenabled = true\nstatus = false").unwrap();
        assert!(c.ai.titles() && !c.ai.status() && c.ai.notifications() && c.ai.explain());
        let c = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        assert!(!c.ai.enabled);
    }

    #[test]
    fn quick_terminal_settings() {
        let c = Config::default().quick_terminal;
        assert!(c.hotkey.is_empty() && c.autohide);
        assert_eq!(c.position, QuickTerminalPosition::Top);
        let c = Config::parse(
            "[quick_terminal]\nhotkey = \"ctrl+grave\"\nposition = \"bottom\"\nscreen = \"main\"",
        )
        .unwrap()
        .quick_terminal;
        assert_eq!(c.hotkey, "ctrl+grave");
        assert_eq!(c.position, QuickTerminalPosition::Bottom);
        assert_eq!(c.screen, QuickTerminalScreen::Main);
        assert_eq!(c.opacity, None);
        let c = Config::parse("[quick_terminal]\nopacity = 0.85").unwrap();
        assert_eq!(c.quick_terminal.opacity, Some(0.85));
        assert!(Config::parse("[quick_terminal]\nposition = \"diagonal\"").is_err());
        let c = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(c.quick_terminal, QuickTerminalConfig::default());
    }

    #[test]
    fn unknown_keys_are_ignored_with_a_warning() {
        let (c, warnings) = Config::parse_with_warnings(
            "[font]\nfamliy = \"x\"\nsize = 15.0\n[window]\nfuture_setting = true\n\
             [[remote]]\nname = \"box\"\nhost = \"box\"\ntypo = 1\n",
        )
        .unwrap();
        // The rest of the file still applies, remotes included.
        assert_eq!(c.font.size, 15.0);
        assert_eq!(c.remote.len(), 1);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(
            warnings.iter().any(|w| w.contains("font.famliy")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("window.future_setting")),
            "{warnings:?}"
        );
        assert!(Config::parse_with_warnings("").unwrap().1.is_empty());
    }

    #[test]
    fn numbers_out_of_range_are_rejected() {
        for bad in [
            "[font]\nletter_spacing = 1e30",
            "[font]\nsize = 5000.0",
            "[font]\nsize = nan",
            "[font]\nline_height = inf",
            "[window]\nopacity = 2.0",
            "[window]\npadding_x = -1.0",
            "[window]\ncolumns = 60000",
            "[cursor]\nthickness = 100.0",
            "[terminal]\nscroll_multiplier = 0.0",
            "[quick_terminal]\nsize = 3.0",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        // `thurm set` refuses them rather than saving a config that breaks every launch.
        assert!(with_setting("", "font.letter_spacing", "1e30").is_err());
        assert!(Config::parse("[font]\nsize = 14.0\nletter_spacing = -1.0").is_ok());
    }

    #[test]
    fn value_literals_are_valid_toml() {
        for v in [
            "plain",
            "tab\there",
            "esc\u{1b}[31m",
            "quote\"and\\slash",
            "snow ☃",
        ] {
            let lit = value_literal(v);
            let parsed: toml_edit::Value = lit.parse().unwrap_or_else(|e| panic!("{lit}: {e}"));
            assert_eq!(parsed.as_str(), Some(v), "{lit}");
        }
        assert_eq!(value_literal("true"), "true");
        assert_eq!(value_literal("240.0"), "240.0");
    }

    #[test]
    fn replacing_a_file_keeps_links_and_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("thurm-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        let real = dir.join("dotfiles/config.toml");
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        replace_file(&link, b"new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        // A link to a file not created yet stays a link.
        let dangling = dir.join("dangling.toml");
        std::os::unix::fs::symlink("dotfiles/later.toml", &dangling).unwrap();
        replace_file(&dangling, b"x").unwrap();
        assert!(
            std::fs::symlink_metadata(&dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("dotfiles/later.toml")).unwrap(),
            "x"
        );
        // And a chain of links ending at a missing file.
        std::os::unix::fs::symlink("hop.toml", dir.join("chain.toml")).unwrap();
        std::os::unix::fs::symlink("dotfiles/end.toml", dir.join("hop.toml")).unwrap();
        replace_file(&dir.join("chain.toml"), b"y").unwrap();
        for l in ["chain.toml", "hop.toml"] {
            assert!(
                std::fs::symlink_metadata(dir.join(l))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("dotfiles/end.toml")).unwrap(),
            "y"
        );
        // Someone else's leftover temporary file is not deleted.
        let leftover = dir.join(format!("dotfiles/.config.toml.tmp{}", std::process::id()));
        std::fs::write(&leftover, "keep").unwrap();
        replace_file(&link, b"newer").unwrap();
        assert_eq!(std::fs::read_to_string(&leftover).unwrap(), "keep");
        // A new file is private.
        let fresh = dir.join("fresh.json");
        replace_file(&fresh, b"{}").unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn example_config_matches_the_defaults() {
        let text = include_str!("../../../config.example.toml");
        let (c, warnings) = Config::parse_with_warnings(text).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(c, Config::default());
    }

    #[test]
    fn socket_directories_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("thurm-sockdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let fresh = base.join("fresh");
        ensure_private_dir(&fresh).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Someone else's (here: too open) directory is refused, not chmodded.
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_private_dir(&open).is_err());
        assert_eq!(
            std::fs::metadata(&open).unwrap().permissions().mode() & 0o777,
            0o777
        );
        // A symlink to a private directory is refused too.
        let link = base.join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        assert!(!private_dir_ok(&link));
        assert!(ensure_private_dir(&link).is_err());
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sessions_without_xdg_runtime_dir_find_the_same_socket() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("thurm-rundir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let run = base.join("run");
        ensure_private_dir(&run).unwrap();
        // A logind session sets XDG_RUNTIME_DIR to /run/user/<uid>; Tailscale SSH may not.
        assert_eq!(socket_dir_for(None, &run), socket_dir_for(Some(&run), &run));
        // No usable runtime directory: the private one in /tmp.
        assert_eq!(socket_dir_for(None, &base.join("missing")), legacy_socket_dir());
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(socket_dir_for(None, &run), legacy_socket_dir());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_daemon_on_the_legacy_socket_is_used_until_it_exits() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;
        let base = std::env::temp_dir().join(format!("thurm-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (new_dir, old_dir) = (base.join("n"), base.join("o"));
        ensure_private_dir(&new_dir).unwrap();
        ensure_private_dir(&old_dir).unwrap();
        let (preferred, legacy) = (new_dir.join("d.sock"), old_dir.join("d.sock"));
        // Nothing answers anywhere: the preferred socket, where a daemon will start.
        assert_eq!(live_socket(preferred.clone(), legacy.clone()), preferred);
        let old = UnixListener::bind(&legacy).unwrap();
        assert_eq!(live_socket(preferred.clone(), legacy.clone()), legacy);
        // Not in a private directory: someone else's socket, never used.
        std::fs::set_permissions(&old_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(live_socket(preferred.clone(), legacy.clone()), preferred);
        std::fs::set_permissions(&old_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Once a daemon answers on the preferred socket, it wins.
        let _new = UnixListener::bind(&preferred).unwrap();
        assert_eq!(live_socket(preferred.clone(), legacy.clone()), preferred);
        drop(old);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn default_socket_is_private_and_short() {
        let dir = default_socket_dir();
        assert!(dir.is_absolute());
        // sun_path is 104 bytes on macOS.
        assert!(
            dir.join("thurmd.sock").as_os_str().len() < 100,
            "{}",
            dir.display()
        );
        #[cfg(target_os = "macos")]
        assert!(!dir.starts_with("/tmp"), "{}", dir.display());
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#ff8800"), Some(0xff8800));
        assert_eq!(parse_color("0x0a0b0c"), Some(0x0a0b0c));
        assert_eq!(parse_color("#fff"), Some(0xffffff));
        assert_eq!(parse_color("nope"), None);
    }
}
