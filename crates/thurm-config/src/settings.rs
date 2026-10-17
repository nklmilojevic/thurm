//! The options the settings window shows: every option of the config file with its type, range
//! or choices, label and the comment above it in `default_config.toml`. The window reads
//! [`view`] and writes single values with [`set`], which go to the settings overlay.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};
use toml::{Table, Value};

use crate::{
    Config, DEFAULT_CONFIG_TOML, DEFAULT_THEME, ThemeSpec, config_path, overlay, parse_color,
};

/// What a setting holds, which decides its control.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    /// A whole number, within the bounds when given.
    Int(Option<(f64, f64)>),
    /// A number, within the bounds when given.
    Float(Option<(f64, f64)>),
    Text,
    /// A font family or a face name.
    Font,
    /// `#rrggbb`; unset means the theme's.
    Color,
    /// A theme name, or `light:NAME,dark:NAME`.
    Theme,
    /// Strings, typed comma-separated.
    List,
    /// One of these words.
    Enum(&'static [&'static str]),
    /// A table or a list of tables: shown read-only, edited in the file.
    Table,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Bool => "bool",
            Kind::Int(_) => "int",
            Kind::Float(_) => "float",
            Kind::Text => "text",
            Kind::Font => "font",
            Kind::Color => "color",
            Kind::Theme => "theme",
            Kind::List => "list",
            Kind::Enum(_) => "enum",
            Kind::Table => "table",
        }
    }
}

/// One option of the config file.
pub struct Setting {
    pub key: String,
    pub label: String,
    pub kind: Kind,
}

/// The sections of the window, in order: `(id, label)`.
const SECTIONS: &[(&str, &str)] = &[
    ("appearance", "Appearance"),
    ("font", "Font"),
    ("window", "Window"),
    ("terminal", "Terminal"),
    ("session", "Session"),
    ("security", "Security"),
    ("notifications", "Notifications"),
    ("agents", "Agents"),
    ("ai", "Apple Intelligence"),
    ("programs", "Programs"),
    ("updates", "Updates"),
    ("quick_terminal", "Quick Terminal"),
    ("keys", "Keys"),
    ("remotes", "Remotes"),
];

const NOTICE_STATES: &[(&str, &str)] = &[
    ("working", "Working"),
    ("waiting", "Waiting"),
    ("blocked", "Blocked"),
    ("finished", "Finished"),
    ("failed", "Failed"),
    ("idle", "Idle"),
];

const NOTICE_FIELDS: &[(&str, &str, Kind)] = &[
    ("tab_indicator", "Tab indicator", Kind::Bool),
    ("notify", "Notify", Kind::Bool),
    ("badge", "Badge", Kind::Bool),
    ("sound", "Sound", Kind::Text),
];

const WINDOW_MATERIAL: &[&str] = &["theme", "system", "blend", "modern"];
const OPTION_AS_ALT: &[&str] = &["none", "left", "right", "both"];
const TAB_STYLE: &[&str] = &["native", "sidebar"];
const CURSOR_STYLE: &[&str] = &["block", "beam", "underline"];
const OSC52: &[&str] = &["disabled", "copy", "paste", "copy-paste"];
const QUIT: &[&str] = &["detach", "terminate"];
const UPDATE_CHANNEL: &[&str] = &["auto", "release", "tip"];
const QUICK_POSITION: &[&str] = &["top", "bottom", "left", "right", "center"];
const QUICK_SCREEN: &[&str] = &["mouse", "main"];

/// Every option, in the order the window shows them. The notification matrix follows
/// `notifications.bell_badge`.
const OPTIONS: &[(&str, &str, Kind)] = &[
    ("font.family", "Family", Kind::Font),
    ("font.size", "Size", Kind::Float(Some((4.0, 200.0)))),
    ("font.ligatures", "Ligatures", Kind::Bool),
    ("font.features", "OpenType features", Kind::List),
    (
        "font.line_height",
        "Line height",
        Kind::Float(Some((0.5, 4.0))),
    ),
    (
        "font.letter_spacing",
        "Letter spacing",
        Kind::Float(Some((-20.0, 100.0))),
    ),
    ("font.family_bold", "Bold face", Kind::Font),
    ("font.family_italic", "Italic face", Kind::Font),
    ("font.family_bold_italic", "Bold italic face", Kind::Font),
    ("font.thicken", "Thicken strokes", Kind::Bool),
    (
        "font.thicken_strength",
        "Thickening",
        Kind::Int(Some((0.0, 255.0))),
    ),
    ("font.fallback", "Fallback families", Kind::List),
    ("font.nerd_font_symbols", "Nerd Font symbols", Kind::Bool),
    (
        "window.padding_x",
        "Horizontal padding",
        Kind::Float(Some((0.0, 200.0))),
    ),
    (
        "window.padding_y",
        "Vertical padding",
        Kind::Float(Some((0.0, 200.0))),
    ),
    ("window.opacity", "Opacity", Kind::Float(Some((0.0, 1.0)))),
    (
        "window.blur",
        "Background blur",
        Kind::Int(Some((0.0, 100.0))),
    ),
    ("window.material", "Material", Kind::Enum(WINDOW_MATERIAL)),
    (
        "window.option_as_alt",
        "Option as Alt",
        Kind::Enum(OPTION_AS_ALT),
    ),
    ("window.columns", "Columns", Kind::Int(Some((1.0, 1000.0)))),
    ("window.rows", "Rows", Kind::Int(Some((1.0, 1000.0)))),
    ("window.confirm_close", "Confirm before closing", Kind::Bool),
    (
        "window.quit_after_last_window",
        "Quit after last window",
        Kind::Bool,
    ),
    (
        "window.unfocused_split_dim",
        "Dim unfocused splits",
        Kind::Float(Some((0.0, 1.0))),
    ),
    ("window.auto_balance", "Balance splits", Kind::Bool),
    ("window.pane_headers", "Pane headers", Kind::Bool),
    (
        "window.pane_gap",
        "Gap between splits",
        Kind::Float(Some((0.0, 64.0))),
    ),
    ("window.tab_style", "Tabs", Kind::Enum(TAB_STYLE)),
    (
        "window.sidebar_width",
        "Sidebar width",
        Kind::Float(Some((180.0, 480.0))),
    ),
    (
        "window.sidebar_agents",
        "Agents panel in sidebar",
        Kind::Bool,
    ),
    (
        "window.sidebar_agent_rows",
        "Agent rows before scrolling",
        Kind::Int(Some((1.0, 100.0))),
    ),
    ("cursor.style", "Style", Kind::Enum(CURSOR_STYLE)),
    ("cursor.blink", "Blink", Kind::Bool),
    (
        "cursor.thickness",
        "Beam and underline thickness",
        Kind::Float(Some((0.0, 20.0))),
    ),
    ("colors.theme", "Theme", Kind::Theme),
    ("colors.foreground", "Text color", Kind::Color),
    ("colors.background", "Background color", Kind::Color),
    ("colors.cursor", "Cursor color", Kind::Color),
    ("colors.cursor_text", "Cursor text color", Kind::Color),
    (
        "colors.selection_foreground",
        "Selected text color",
        Kind::Color,
    ),
    (
        "colors.selection_background",
        "Selection color",
        Kind::Color,
    ),
    ("colors.palette", "ANSI palette", Kind::List),
    (
        "colors.bold_is_bright",
        "Bold text uses bright colors",
        Kind::Bool,
    ),
    (
        "colors.improve_contrast",
        "Improve text contrast",
        Kind::Bool,
    ),
    (
        "colors.theme_palette",
        "Force theme palette for text",
        Kind::Bool,
    ),
    (
        "colors.blend_backgrounds",
        "Blend program backgrounds",
        Kind::Bool,
    ),
    (
        "colors.display_p3",
        "Use Display P3 color space",
        Kind::Bool,
    ),
    (
        "terminal.scrollback",
        "Scrollback lines",
        Kind::Int(Some((0.0, 10_000_000.0))),
    ),
    ("terminal.shell", "Shell command", Kind::Table),
    ("terminal.env", "Environment variables", Kind::Table),
    ("terminal.term", "TERM", Kind::Text),
    (
        "terminal.shell_integration",
        "Shell integration",
        Kind::Bool,
    ),
    ("terminal.osc52", "OSC 52 clipboard", Kind::Enum(OSC52)),
    (
        "terminal.kitty_keyboard",
        "Kitty keyboard protocol",
        Kind::Bool,
    ),
    (
        "terminal.kitty_graphics",
        "Kitty graphics protocol",
        Kind::Bool,
    ),
    (
        "terminal.image_memory_mib",
        "Image memory (MiB)",
        Kind::Int(Some((0.0, 16384.0))),
    ),
    ("terminal.word_separators", "Word separators", Kind::Text),
    ("terminal.copy_on_select", "Copy on select", Kind::Bool),
    (
        "terminal.scroll_multiplier",
        "Lines per wheel step",
        Kind::Float(Some((0.01, 100.0))),
    ),
    ("terminal.smooth_scroll", "Smooth scrolling", Kind::Bool),
    ("terminal.tab_completion", "Tab completion", Kind::Bool),
    (
        "session.persist",
        "Restore layout and scrollback",
        Kind::Bool,
    ),
    (
        "session.scrollback_lines",
        "Scrollback saved per pane",
        Kind::Int(Some((0.0, 1_000_000.0))),
    ),
    (
        "session.snapshot_interval_secs",
        "Snapshot interval (seconds)",
        Kind::Int(None),
    ),
    ("session.quit", "Quitting the app", Kind::Enum(QUIT)),
    (
        "session.resume_agents",
        "Resume agents after restart",
        Kind::Bool,
    ),
    (
        "session.daemon_idle_exit_secs",
        "Daemon exits after idle (seconds, 0 = never)",
        Kind::Int(None),
    ),
    (
        "security.auto_secure_input",
        "Secure keyboard entry at password prompts",
        Kind::Bool,
    ),
    (
        "security.secure_input_indicator",
        "Lock badge while secure input is on",
        Kind::Bool,
    ),
    (
        "security.confirm_multiline_paste",
        "Confirm multi-line paste",
        Kind::Bool,
    ),
    ("notifications.enabled", "Notifications", Kind::Bool),
    (
        "notifications.command_finished_secs",
        "Long command finished after (seconds)",
        Kind::Int(None),
    ),
    (
        "notifications.program_notifications",
        "Notifications from programs",
        Kind::Bool,
    ),
    ("notifications.bell_badge", "Bell badge", Kind::Bool),
    ("agents.detect", "Detect agents", Kind::Bool),
    (
        "agents.idle_after_ms",
        "Idle after (milliseconds)",
        Kind::Int(None),
    ),
    (
        "agents.session_titles",
        "Name tabs after agent sessions",
        Kind::Bool,
    ),
    ("agents.define", "Custom agents", Kind::Table),
    ("agents.presets", "Launch presets", Kind::Table),
    ("agents.install_hooks", "Install status hooks", Kind::Bool),
    ("ai.enabled", "Apple Intelligence features", Kind::Bool),
    ("ai.titles", "Name tabs after the task", Kind::Bool),
    ("ai.status", "Detect waiting agents", Kind::Bool),
    (
        "ai.notifications",
        "Summarize agent notifications",
        Kind::Bool,
    ),
    ("ai.explain", "Explain last command", Kind::Bool),
    ("programs.detect", "Detect programs", Kind::Bool),
    ("programs.names", "Show program names", Kind::Bool),
    ("programs.define", "Custom programs", Kind::Table),
    ("push.relay", "Push relay", Kind::Text),
    ("push.code", "Push relay code", Kind::Text),
    ("tailscale.enabled", "Serve over Tailscale", Kind::Bool),
    ("tailscale.hostname", "Tailnet hostname", Kind::Text),
    (
        "tailscale.allow_tailnet_members",
        "Allow tailnet members",
        Kind::Bool,
    ),
    ("tailscale.advertise_tags", "Tailnet tags", Kind::List),
    ("tailscale.auth_key_file", "Auth key file", Kind::Text),
    (
        "tailscale.host_listen",
        "Listen through this Mac's Tailscale",
        Kind::Text,
    ),
    (
        "tailscale.host_allow_identified",
        "Admit every identified caller",
        Kind::Bool,
    ),
    (
        "tailscale.serve_socket",
        "Socket for tailscale serve",
        Kind::Bool,
    ),
    ("updates.channel", "Channel", Kind::Enum(UPDATE_CHANNEL)),
    (
        "updates.check_automatically",
        "Check automatically",
        Kind::Bool,
    ),
    (
        "updates.download_automatically",
        "Download automatically",
        Kind::Bool,
    ),
    ("quick_terminal.hotkey", "Shortcut", Kind::Text),
    (
        "quick_terminal.position",
        "Edge",
        Kind::Enum(QUICK_POSITION),
    ),
    ("quick_terminal.size", "Size", Kind::Float(Some((0.0, 1.0)))),
    (
        "quick_terminal.autohide",
        "Hide when focus moves away",
        Kind::Bool,
    ),
    ("quick_terminal.screen", "Screen", Kind::Enum(QUICK_SCREEN)),
    (
        "quick_terminal.animation_duration",
        "Slide animation (seconds)",
        Kind::Float(Some((0.0, 5.0))),
    ),
    (
        "quick_terminal.opacity",
        "Opacity",
        Kind::Float(Some((0.0, 1.0))),
    ),
    ("keybindings", "Key bindings", Kind::Table),
    ("remote", "Remote hosts", Kind::Table),
];

/// Every option the window shows.
pub fn schema() -> Vec<Setting> {
    let mut out = Vec::new();
    for (key, label, kind) in OPTIONS {
        out.push(Setting {
            key: key.to_string(),
            label: label.to_string(),
            kind: *kind,
        });
        if *key == "notifications.bell_badge" {
            for (state, state_label) in NOTICE_STATES {
                for (field, field_label, kind) in NOTICE_FIELDS {
                    out.push(Setting {
                        key: format!("notifications.{state}.{field}"),
                        label: format!("{state_label}: {field_label}"),
                        kind: *kind,
                    });
                }
            }
        }
    }
    out
}

/// The window's view of the config at `config`: every setting with its value, default, where
/// the value comes from (`"default"`, `"file"` or `"overlay"`) and its comment, plus the
/// files, the warnings about the overlay and an error when `config.toml` does not load.
pub fn view_at(config: &Path) -> Json {
    let overlay_path = overlay::path_beside(config);
    let base_text = std::fs::read_to_string(config).unwrap_or_default();
    let overlay_text = overlay::read(&overlay_path).unwrap_or_default();
    let defaults = serde_json::to_value(Config::default()).unwrap_or(Json::Null);
    let (cfg, warnings, error) = match Config::load_with_warnings_from(config) {
        Ok((cfg, warnings)) => (
            serde_json::to_value(&cfg).unwrap_or(Json::Null),
            warnings,
            None,
        ),
        Err(e) => (defaults.clone(), vec![], Some(e.message)),
    };
    let base: Table = toml::from_str(&base_text).unwrap_or_default();
    let applied = overlay::merge(&base_text, &overlay_text)
        .map(|merged| merged.applied)
        .unwrap_or_default();
    let docs = scan(DEFAULT_CONFIG_TOML).docs;
    let settings: Vec<Json> = schema()
        .iter()
        .map(|s| {
            let path: Vec<&str> = s.key.split('.').collect();
            let warning = warnings
                .iter()
                .find(|w| w.starts_with(&format!("settings.toml: {} ", s.key)))
                .cloned();
            // A whole notification row in the overlay also sets each of its cells.
            let from_overlay = applied.iter().any(|a| {
                *a == s.key
                    || a.starts_with(&format!("{}.", s.key))
                    || s.key.starts_with(&format!("{a}."))
            });
            let source = if from_overlay {
                "overlay"
            } else if lookup_toml(&base, &path).is_some() {
                "file"
            } else {
                "default"
            };
            let default = if s.key == "colors.theme" {
                json!(DEFAULT_THEME)
            } else {
                lookup_json(&defaults, &path).cloned().unwrap_or(Json::Null)
            };
            let mut item = json!({
                "key": s.key,
                "section": section_of(&s.key),
                "label": s.label,
                "kind": s.kind.name(),
                "value": lookup_json(&cfg, &path).cloned().unwrap_or(Json::Null),
                "default": default,
                "source": source,
                "file_value": lookup_toml(&base, &path)
                    .and_then(|v| serde_json::to_value(v).ok())
                    .unwrap_or(Json::Null),
                "doc": doc_for(&docs, &s.key),
                "warning": warning,
            });
            if let Kind::Enum(options) = s.kind {
                item["options"] = json!(options);
            }
            let range = match s.kind {
                Kind::Int(range) | Kind::Float(range) => range,
                _ => None,
            };
            if let Some((min, max)) = range {
                item["min"] = json!(min);
                item["max"] = json!(max);
            }
            if let ["notifications", state, field] = path.as_slice()
                && NOTICE_STATES.iter().any(|(name, _)| name == state)
            {
                item["row"] = json!(state);
                item["column"] = json!(field);
                if item["doc"].as_str() == Some("") {
                    item["doc"] = json!(doc_for(&docs, "notifications.working"));
                }
            }
            item
        })
        .collect();
    json!({
        "config_path": config.display().to_string(),
        "overlay_path": overlay_path.display().to_string(),
        "config_read_only": crate::config_read_only(config),
        "overlay_exists": overlay_path.exists(),
        "sections": SECTIONS.iter().map(|(id, label)| json!({ "id": id, "label": label })).collect::<Vec<_>>(),
        "settings": settings,
        "warnings": warnings,
        "error": error,
    })
}

/// [`view_at`] for the config file of this user.
pub fn view() -> Json {
    view_at(&config_path())
}

/// Sets the option `key` to `value` in the settings overlay of the config at `config`, checked
/// as the config would load it. `null` removes the overlay's value, so the file's or the
/// default's shows again. The config file itself is never written.
pub fn set_at(config: &Path, key: &str, value: &Json) -> Result<(), String> {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        ["notifications", state] | ["notifications", state, _] if is_notice_state(state) => {
            return set_notice(config, key, state, value);
        }
        _ => {}
    }
    let setting = schema()
        .into_iter()
        .find(|s| s.key == key)
        .ok_or_else(|| format!("{key:?} is not a setting"))?;
    let literal = match value {
        Json::Null => None,
        _ => Some(literal(setting.kind, value).map_err(|e| format!("{key}: {e}"))?),
    };
    overlay::write(config, key, literal.as_deref())
}

fn is_notice_state(name: &str) -> bool {
    NOTICE_STATES.iter().any(|(state, _)| *state == name)
}

/// A cell of the notification matrix, or a whole row (`notifications.STATE`). A row is kept
/// whole in the overlay, since serde fills a missing cell with the wrong default: a cell's value
/// goes in with the row's other cells as they are now, and a reset (null) takes the whole row
/// back to the file's.
fn set_notice(config: &Path, key: &str, state: &str, value: &Json) -> Result<(), String> {
    let row = format!("notifications.{state}");
    // The overlay holds whole rows, so a row is only ever reset; its cells are set one by one.
    if key == row && !value.is_null() {
        return Err(format!("{key}: set its cells, or null to reset the row"));
    }
    if value.is_null() {
        return overlay::write(config, &row, None);
    }
    let prefix = format!("{row}.");
    let field = key.strip_prefix(prefix.as_str()).unwrap_or_default();
    if NOTICE_FIELDS.iter().all(|(name, _, _)| *name != field) {
        return Err(format!("{key:?} is not a setting"));
    }
    let (cfg, _) = Config::load_with_warnings_from(config).map_err(|e| e.to_string())?;
    let current = serde_json::to_value(&cfg).unwrap_or(Json::Null);
    let mut cells = Vec::new();
    for (name, _, kind) in NOTICE_FIELDS {
        let new = if *name == field {
            value.clone()
        } else {
            lookup_json(&current, &["notifications", state, *name])
                .cloned()
                .unwrap_or(Json::Null)
        };
        let cell = literal(*kind, &new).map_err(|e| format!("{key}: {e}"))?;
        cells.push(format!("{name} = {cell}"));
    }
    overlay::write(config, &row, Some(&format!("{{ {} }}", cells.join(", "))))
}

/// [`set_at`] for the config file of this user.
pub fn set(key: &str, value: &Json) -> Result<(), String> {
    set_at(&config_path(), key, value)
}

/// The settings overlay of this user, created (with a short header) if it does not exist.
pub fn open_overlay() -> std::io::Result<PathBuf> {
    overlay::ensure(&config_path())
}

/// `value` as the TOML literal the option of `kind` takes. Numbers keep two decimals.
fn literal(kind: Kind, value: &Json) -> Result<String, String> {
    let text = || value.as_str().ok_or_else(|| "expected text".to_owned());
    match kind {
        Kind::Bool => value
            .as_bool()
            .map(|b| b.to_string())
            .ok_or_else(|| "expected true or false".to_owned()),
        Kind::Int(range) => {
            let n = value.as_i64().ok_or("expected a whole number")?;
            within(n as f64, range)?;
            Ok(n.to_string())
        }
        Kind::Float(range) => {
            let x = value
                .as_f64()
                .filter(|x| x.is_finite())
                .ok_or("expected a number")?;
            within(x, range)?;
            let x = (x * 100.0).round() / 100.0;
            within(x, range)?;
            Ok(format!("{x:?}"))
        }
        Kind::Text | Kind::Font => Ok(toml_edit::Value::from(text()?).to_string()),
        Kind::Color => {
            let color = text()?;
            parse_color(color).ok_or("expected a color such as #1b2029")?;
            Ok(toml_edit::Value::from(color).to_string())
        }
        Kind::Theme => {
            let spec = ThemeSpec::parse(text()?);
            spec.validate()?;
            Ok(toml_edit::Value::from(spec.to_string()).to_string())
        }
        Kind::List => {
            let items: Vec<String> = match value {
                Json::Array(items) => items
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .ok_or("expected a list of text")
                    })
                    .collect::<Result<_, _>>()?,
                _ => text()?
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect(),
            };
            Ok(toml_edit::Value::Array(items.iter().map(String::as_str).collect()).to_string())
        }
        Kind::Enum(options) => {
            let word = text()?;
            if options.contains(&word) {
                Ok(toml_edit::Value::from(word).to_string())
            } else {
                Err(format!("expected one of {}", options.join(", ")))
            }
        }
        Kind::Table => Err("edit this one in the file".to_owned()),
    }
}

fn within(x: f64, range: Option<(f64, f64)>) -> Result<(), String> {
    match range {
        Some((lo, hi)) if !(lo..=hi).contains(&x) => Err(format!("must be from {lo} to {hi}")),
        _ => Ok(()),
    }
}

fn section_of(key: &str) -> &'static str {
    match key.split('.').next() {
        Some("colors" | "cursor") => "appearance",
        Some("keybindings") => "keys",
        Some("remote" | "push" | "tailscale") => "remotes",
        Some(top) => SECTIONS
            .iter()
            .find(|(id, _)| *id == top)
            .map_or("", |&(id, _)| id),
        None => "",
    }
}

fn lookup_toml<'a>(table: &'a Table, path: &[&str]) -> Option<&'a Value> {
    match path {
        [] => None,
        [last] => table.get(*last),
        [first, rest @ ..] => match table.get(*first)? {
            Value::Table(child) => lookup_toml(child, rest),
            _ => None,
        },
    }
}

fn lookup_json<'a>(value: &'a Json, path: &[&str]) -> Option<&'a Json> {
    path.iter().try_fold(value, |v, key| v.get(*key))
}

/// The comments of the default config, by the dotted path of the option or table they sit
/// above, and the paths the file sets (active or commented out).
#[derive(Default)]
struct Scan {
    docs: BTreeMap<String, String>,
    /// Tables and their headers, active or commented out.
    present: BTreeSet<String>,
    /// Options (`key = value` lines, active or commented out), by dotted path.
    options: BTreeSet<String>,
}

fn scan(text: &str) -> Scan {
    let lines: Vec<&str> = text.lines().collect();
    let mut scan = Scan::default();
    let mut section = String::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let body = trimmed.strip_prefix('#').map_or(trimmed, str::trim_start);
        if let Some(rest) = body.strip_prefix('[') {
            let name = rest
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or_default()
                .trim();
            let valid = name.split('.').all(|part| {
                !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            });
            if valid && !name.is_empty() {
                section = name.to_owned();
                scan.present.insert(section.clone());
                scan.docs
                    .entry(section.clone())
                    .or_insert_with(|| doc_above(&lines, i));
            }
            continue;
        }
        if let Some(name) = assigned(body) {
            let path = if section.is_empty() {
                name.to_owned()
            } else {
                format!("{section}.{name}")
            };
            scan.options.insert(path.clone());
            scan.docs
                .entry(path.clone())
                .or_insert_with(|| doc_above(&lines, i));
            scan.present.insert(path);
        }
    }
    scan
}

/// The key of a `key = value` line.
fn assigned(body: &str) -> Option<&str> {
    let end = body.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
    let (name, rest) = body.split_at(end);
    (!name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && rest.trim_start().starts_with('='))
    .then_some(name)
}

/// The comment block directly above line `index`, as one paragraph.
fn doc_above(lines: &[&str], index: usize) -> String {
    let mut block = Vec::new();
    for line in lines[..index].iter().rev() {
        let Some(rest) = line.trim().strip_prefix('#') else {
            break;
        };
        let rest = rest.trim_start();
        if rest.starts_with('[') || assigned(rest).is_some() {
            break;
        }
        block.push(rest);
    }
    block
        .iter()
        .rev()
        .flat_map(|l| l.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The comment of `key`, or of the nearest table above it that has one.
fn doc_for(docs: &BTreeMap<String, String>, key: &str) -> String {
    let mut path = key;
    loop {
        if let Some(doc) = docs.get(path).filter(|d| !d.is_empty()) {
            return doc.clone();
        }
        match path.rsplit_once('.') {
            Some((parent, _)) => path = parent,
            None => return String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(name: &str, text: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("thurm-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, text).unwrap();
        config
    }

    fn leaf_keys(table: &Table, prefix: &str, out: &mut BTreeSet<String>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                Value::Table(child) => leaf_keys(child, &path, out),
                _ => {
                    out.insert(path);
                }
            }
        }
    }

    #[test]
    fn schema_covers_default_config() {
        let defaults: Table = toml::from_str(DEFAULT_CONFIG_TOML).unwrap();
        let mut options = BTreeSet::new();
        leaf_keys(&defaults, "", &mut options);
        options.remove("config_version");
        let keys: Vec<String> = schema().into_iter().map(|s| s.key).collect();
        let unique: BTreeSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "a key has two entries");
        for key in &options {
            assert!(
                keys.contains(key),
                "{key} is in default_config.toml but has no settings entry"
            );
        }
        let text = scan(DEFAULT_CONFIG_TOML);
        // Every option the file mentions, active or commented out, has an entry: except the
        // examples inside a table entry (`[[remote]]`'s fields) and the parts of an inline table.
        let tables: Vec<String> = schema()
            .into_iter()
            .filter(|s| s.kind == Kind::Table)
            .map(|s| s.key)
            .collect();
        for key in text.options.iter().filter(|k| *k != "config_version") {
            let documented = keys.contains(key)
                || keys.iter().any(|k| k.starts_with(&format!("{key}.")))
                || tables.iter().any(|t| key.starts_with(&format!("{t}.")));
            assert!(
                documented,
                "{key} is in default_config.toml but has no settings entry"
            );
        }
        for key in &keys {
            let parent = key.rsplit_once('.').map(|(p, _)| p);
            let in_file = text.options.contains(key)
                || text.present.contains(key)
                || parent.is_some_and(|p| text.options.contains(p));
            assert!(
                in_file,
                "{key} has a settings entry but is not in default_config.toml"
            );
        }
    }

    #[test]
    fn every_setting_is_an_option_of_the_config() {
        let defaults = serde_json::to_value(Config::default()).unwrap();
        for s in schema() {
            let path: Vec<&str> = s.key.split('.').collect();
            assert!(
                lookup_json(&defaults, &path).is_some(),
                "{} is not an option of Config",
                s.key
            );
        }
        assert!(schema().iter().all(|s| s.key != "config_version"));
    }

    #[test]
    fn values_are_written_to_the_overlay_as_their_kind() {
        let config = temp_config("write", DEFAULT_CONFIG_TOML);
        set_at(&config, "font.size", &json!(15)).unwrap();
        set_at(&config, "window.tab_style", &json!("native")).unwrap();
        set_at(
            &config,
            "colors.theme",
            &json!("light:catppuccin-latte,dark:catppuccin-mocha"),
        )
        .unwrap();
        set_at(&config, "font.fallback", &json!("SF Mono, Menlo")).unwrap();
        set_at(&config, "notifications.working.notify", &json!(true)).unwrap();
        set_at(&config, "colors.background", &json!("#101010")).unwrap();
        set_at(&config, "terminal.scroll_multiplier", &json!(2.5)).unwrap();
        let (cfg, warnings) = Config::load_with_warnings_from(&config).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfg.font.size, 15.0);
        assert_eq!(cfg.window.tab_style, crate::TabStyle::Native);
        assert_eq!(cfg.font.fallback, ["SF Mono", "Menlo"]);
        assert!(cfg.notifications.working.notify);
        assert_eq!(cfg.colors.background.as_deref(), Some("#101010"));
        assert_eq!(cfg.terminal.scroll_multiplier, 2.5);
        assert_eq!(cfg.theme_spec().light, "catppuccin-latte");

        for (key, value) in [
            ("font.size", json!("big")),
            ("font.size", json!(1000)),
            ("window.tab_style", json!("diagonal")),
            ("font.nope", json!(1)),
            ("colors.background", json!("red")),
            ("colors.theme", json!("no-such-theme")),
            ("keybindings", json!({})),
        ] {
            assert!(set_at(&config, key, &value).is_err(), "{key} = {value}");
        }
        set_at(&config, "font.size", &Json::Null).unwrap();
        let (cfg, _) = Config::load_with_warnings_from(&config).unwrap();
        assert_eq!(cfg.font.size, 16.0);
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            DEFAULT_CONFIG_TOML
        );
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn numbers_are_saved_with_two_decimals() {
        let config = temp_config("decimals", DEFAULT_CONFIG_TOML);
        set_at(
            &config,
            "window.unfocused_split_dim",
            &json!(0.4302884615384616),
        )
        .unwrap();
        set_at(&config, "terminal.scroll_multiplier", &json!(1.006)).unwrap();
        let text = std::fs::read_to_string(overlay::path_beside(&config)).unwrap();
        assert!(text.contains("unfocused_split_dim = 0.43\n"), "{text}");
        assert!(text.contains("scroll_multiplier = 1.01\n"), "{text}");
        assert!(set_at(&config, "terminal.scroll_multiplier", &json!(0.005)).is_err());
        assert!(set_at(&config, "window.opacity", &json!(1.004)).is_err());
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn view_says_where_each_value_comes_from() {
        let config = temp_config("view", "[font]\nsize = 14.0\n");
        set_at(&config, "font.family", &json!("Menlo")).unwrap();
        let view = view_at(&config);
        let find = |key: &str| {
            view["settings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["key"] == key)
                .unwrap()
                .clone()
        };
        assert_eq!(find("font.size")["source"], "file");
        assert_eq!(find("font.size")["value"], 14.0);
        assert_eq!(find("font.family")["source"], "overlay");
        assert_eq!(find("font.family")["value"], "Menlo");
        assert_eq!(find("font.ligatures")["source"], "default");
        assert_eq!(find("colors.theme")["default"], DEFAULT_THEME);
        assert_eq!(find("notifications.waiting.notify")["row"], "waiting");
        assert_eq!(find("notifications.waiting.notify")["column"], "notify");
        assert_eq!(find("window.tab_style")["options"], json!(TAB_STYLE));
        assert_eq!(find("window.opacity")["min"], 0.0);
        assert_eq!(find("font.size")["section"], "font");
        assert_eq!(view["overlay_exists"], true);
        assert_eq!(view["config_read_only"], false);
        assert!(view["error"].is_null());
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_notification_row_is_written_whole_or_not_at_all() {
        let config = temp_config("row", DEFAULT_CONFIG_TOML);
        assert!(set_at(&config, "notifications.waiting.sound", &json!(5)).is_err());
        assert!(set_at(&config, "notifications.waiting.typo", &json!(true)).is_err());
        assert!(
            !overlay::path_beside(&config).exists(),
            "a refused cell wrote something"
        );
        set_at(&config, "notifications.waiting.notify", &json!(false)).unwrap();
        let text = std::fs::read_to_string(overlay::path_beside(&config)).unwrap();
        assert!(
            text.contains("badge = true") && text.contains("sound = \"default\""),
            "{text}"
        );
        let (cfg, warnings) = Config::load_with_warnings_from(&config).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!cfg.notifications.waiting.notify && cfg.notifications.waiting.badge);
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_dropped_overlay_setting_is_not_shown_as_the_overlays() {
        let config = temp_config("dropped", "[font]\nsize = 14.0\n");
        std::fs::write(overlay::path_beside(&config), "[font]\nsize = 1.0\n").unwrap();
        let view = view_at(&config);
        let size = view["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == "font.size")
            .unwrap()
            .clone();
        assert_eq!(size["source"], "file");
        assert!(
            size["warning"].as_str().unwrap().contains("font.size"),
            "{size}"
        );
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_notification_row_takes_only_null() {
        let config = temp_config("row-null", "");
        set_at(&config, "notifications.waiting.notify", &json!(false)).unwrap();
        let overlay = overlay::path_beside(&config);
        let before = std::fs::read_to_string(&overlay).unwrap();
        assert!(set_at(&config, "notifications.waiting", &json!({ "notify": true })).is_err());
        assert!(set_at(&config, "notifications.waiting", &json!(true)).is_err());
        assert_eq!(std::fs::read_to_string(&overlay).unwrap(), before);
        set_at(&config, "notifications.waiting", &Json::Null).unwrap();
        assert!(
            !std::fs::read_to_string(&overlay)
                .unwrap()
                .contains("waiting")
        );
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_saved_notification_cell_shows_its_row_as_the_overlays() {
        let config = temp_config("row-source", "");
        set_at(&config, "notifications.waiting.sound", &json!("none")).unwrap();
        let view = view_at(&config);
        let sources: Vec<(String, String)> = view["settings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| {
                let key = s["key"].as_str()?;
                let state = key.strip_prefix("notifications.")?.split('.').next()?;
                (key.matches('.').count() == 2).then(|| {
                    (
                        state.to_owned(),
                        s["source"].as_str().unwrap_or("").to_owned(),
                    )
                })
            })
            .collect();
        assert!(sources.iter().any(|(state, _)| state == "waiting"));
        for (state, source) in &sources {
            let expected = if state == "waiting" {
                "overlay"
            } else {
                "default"
            };
            assert_eq!(source, expected, "{state}");
        }
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn every_setting_has_a_comment_in_the_default_config() {
        let config = temp_config("docs", DEFAULT_CONFIG_TOML);
        let view = view_at(&config);
        let undocumented: Vec<&str> = view["settings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["doc"].as_str() == Some(""))
            .filter_map(|s| s["key"].as_str())
            .collect();
        assert!(
            undocumented.is_empty(),
            "no comment above: {undocumented:?}"
        );
        std::fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn docs_come_from_the_comments_above_each_option() {
        let docs = scan(DEFAULT_CONFIG_TOML).docs;
        assert_eq!(
            doc_for(&docs, "font.ligatures"),
            "Programming ligatures (=>, !=, ->, ...). Needs a font that ships them."
        );
        assert!(doc_for(&docs, "notifications.working").starts_with("What an agent does"));
        assert!(doc_for(&docs, "remote").starts_with("Remote workspaces"));
    }
}
