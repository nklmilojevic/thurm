//! Thurm configuration.
//!
//! Loaded from `$XDG_CONFIG_HOME/thurm/config.toml` (falling back to `~/.config/thurm/config.toml`
//! on every platform, macOS included, like most terminals). Every field has a default, so an
//! empty or missing file is a valid configuration.

mod agents;
mod themes;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use agents::{AgentDef, builtin_agents};
pub use themes::{Theme, builtin_theme, builtin_theme_names, is_own_theme};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
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
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
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
            size: 13.0,
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
    #[default]
    Native,
    /// A vertical sidebar (grouped by repository, with agent status and git info), like tty7.
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
#[serde(default, deny_unknown_fields)]
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
            tab_style: TabStyle::Native,
            sidebar_width: 240.0,
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            detect: true,
            idle_after_ms: 1500,
            session_titles: true,
            define: Vec::new(),
            presets: Vec::new(),
        }
    }
}

/// Apple Intelligence's on-device model (macOS 26+, Apple silicon, Apple Intelligence on).
/// Nothing leaves the Mac, but it is opt-in: `enabled` turns the features below on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("THURM_SOCKET") {
        return PathBuf::from(p);
    }
    let uid = unsafe_uid();
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("thurm-{uid}")).join("thurmd.sock")
}

fn unsafe_uid() -> u32 {
    // Avoid a libc dependency for one call: the uid is only used to namespace the socket dir.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Some(home) = dirs::home_dir()
            && let Ok(meta) = std::fs::metadata(home)
        {
            return meta.uid();
        }
    }
    0
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
        toml::from_str(text).map_err(|e| e.to_string())
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
    Config::parse(&out)?;
    Ok(out)
}

/// A command-line value as a TOML literal: anything TOML parses as a value is used as is
/// (`true`, `240.0`, `"x"`), a bare word becomes a string.
pub fn value_literal(v: &str) -> String {
    if v.parse::<toml_edit::Value>().is_ok() {
        v.to_owned()
    } else {
        format!("{v:?}")
    }
}

/// Persist `spec` as the theme in the config file (created from the default if missing).
pub fn write_theme(spec: &ThemeSpec) -> Result<(), String> {
    write_setting("colors.theme", &format!("{:?}", spec.to_string()))
}

/// Persist one setting (see [`with_setting`]) in the config file.
pub fn write_setting(key: &str, value: &str) -> Result<(), String> {
    let path = ensure_default_config().map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let out = with_setting(&text, key, value)?;
    let tmp = path.with_extension(format!("toml.tmp{}", std::process::id()));
    std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
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
        assert_eq!(
            defs.iter().find(|d| d.kind == "claude").unwrap().name,
            "Claude"
        );
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
        let out = with_setting(DEFAULT_CONFIG_TOML, "window.tab_style", "\"sidebar\"").unwrap();
        assert_eq!(
            Config::parse(&out).unwrap().window.tab_style,
            TabStyle::Sidebar
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
    fn unknown_keys_rejected() {
        assert!(Config::parse("[font]\nfamliy = \"x\"").is_err());
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#ff8800"), Some(0xff8800));
        assert_eq!(parse_color("0x0a0b0c"), Some(0x0a0b0c));
        assert_eq!(parse_color("#fff"), Some(0xffffff));
        assert_eq!(parse_color("nope"), None);
    }
}
