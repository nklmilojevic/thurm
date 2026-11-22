//! `thurm hooks`: install the agent hooks that report status to Thurm (`thurm agent-hook`).
//!
//! Claude Code (`settings.json`) and Codex (`~/.codex/hooks.json`) share one hook-map format;
//! Pi loads a standalone extension (`~/.pi/agent/extensions/thurm.js`); Antigravity CLI uses
//! named hooks in `~/.gemini/config/hooks.json`. Each runs `thurm agent-hook <agent> <event>`
//! and is a no-op outside Thurm panes, so the same config works in any terminal.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// How an agent stores Thurm's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookFormat {
    /// JSON hook map (`settings.json` / `hooks.json`).
    JsonMap,
    /// Standalone Pi extension (`~/.pi/agent/extensions/thurm.js`).
    PiExtension,
    /// Named hooks map (`~/.gemini/config/hooks.json`, with flat entries for non-tool events).
    AntigravityHooks,
}

/// The Pi extension file installed into `~/.pi/agent/extensions/thurm.js`.
pub const PI_EXTENSION: &str = include_str!("pi_extension.js");
const PI_MARKER: &str = "// thurm-pi-extension:";

/// An agent whose hooks we manage.
pub struct HookAgent {
    /// Agent kind as the daemon knows it ("claude").
    pub kind: &'static str,
    pub name: &'static str,
    /// How hooks are persisted in [`Self::settings_path`].
    pub format: HookFormat,
    /// Hook events it supports → our event names.
    pub events: &'static [(&'static str, &'static str)],
    /// Resolves the file that holds this agent's hooks.
    pub settings: fn() -> PathBuf,
    /// Recognizes a permission prompt among a notification's payload fields.
    pub permission: fn(&Value) -> bool,
    /// Whether the SessionStart hook injects its context as JSON (`additionalContext`)
    /// instead of plain text.
    pub context_json: bool,
}

pub const AGENTS: &[HookAgent] = &[
    HookAgent {
        kind: "claude",
        name: "Claude Code",
        format: HookFormat::JsonMap,
        events: CLAUDE_EVENTS,
        settings: claude_settings,
        permission: claude_permission,
        context_json: false,
    },
    HookAgent {
        kind: "codex",
        name: "Codex",
        format: HookFormat::JsonMap,
        events: CODEX_EVENTS,
        settings: codex_settings,
        permission: no_permission,
        context_json: false,
    },
    HookAgent {
        kind: "pi",
        name: "Pi",
        format: HookFormat::PiExtension,
        events: PI_EVENTS,
        settings: pi_settings,
        permission: no_permission,
        context_json: false,
    },
    HookAgent {
        kind: "agy",
        name: "Antigravity",
        format: HookFormat::AntigravityHooks,
        events: AGY_EVENTS,
        settings: agy_settings,
        permission: no_permission,
        context_json: false,
    },
];

/// Antigravity CLI hook event → our event name (all non-tool events, flat entry shape).
pub const AGY_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("PreInvocation", "prompt-submit"),
    ("Stop", "stop"),
];

/// Pi extension event → our event name.
pub const PI_EVENTS: &[(&str, &str)] = &[
    ("session_start", "session-start"),
    ("session_info_changed", "session-info"),
    ("before_agent_start", "prompt-submit"),
    ("ui_prompt_start", "notification"),
    ("ui_prompt_end", "tool-complete"),
    ("agent_settled", "stop"),
    ("session_shutdown", "session-end"),
];

/// Codex only has these (no notification / tool events).
pub const CODEX_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("Stop", "stop"),
];

/// Claude Code hook event → our event name.
pub const CLAUDE_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("Notification", "notification"),
    ("PostToolUse", "tool-complete"),
    ("Stop", "stop"),
    ("SessionEnd", "session-end"),
];

impl HookAgent {
    /// Our event name for this agent's native event (either spelling is accepted).
    pub fn normalize(&self, e: &str) -> String {
        self.events
            .iter()
            .find(|(native, ours)| native.eq_ignore_ascii_case(e) || ours.eq_ignore_ascii_case(e))
            .map(|(_, ours)| (*ours).to_owned())
            .unwrap_or_else(|| e.to_ascii_lowercase())
    }

    /// Our event name for a native event and its payload: a notification that names a
    /// permission prompt is `permission-prompt`.
    pub fn hook_event(&self, event: &str, payload: &Value) -> String {
        let event = self.normalize(event);
        if event == "notification" && (self.permission)(payload) {
            "permission-prompt".into()
        } else {
            event
        }
    }

    /// The file holding this agent's hooks.
    pub fn settings_path(&self) -> PathBuf {
        (self.settings)()
    }

    /// Whether this agent's configuration directory exists on this machine.
    pub fn is_present(&self) -> bool {
        match self.format {
            HookFormat::JsonMap => self.settings_path().parent().is_some_and(Path::is_dir),
            HookFormat::PiExtension => {
                pi_agent_dir().is_dir()
                    || (std::env::var_os("PI_CODING_AGENT_DIR")
                        .filter(|d| !d.is_empty())
                        .is_none()
                        && dirs_home().join(".pi").is_dir())
            }
            HookFormat::AntigravityHooks => {
                dirs_home().join(".gemini/antigravity-cli").is_dir()
                    || dirs_home().join(".gemini/antigravity").is_dir()
            }
        }
    }

    /// What the SessionStart hook prints for the agent to add `context` to the session.
    pub fn session_output(&self, context: &str) -> String {
        if self.format == HookFormat::AntigravityHooks {
            json!({
                "injectSteps": [{ "systemMessage": { "systemMessage": context } }]
            })
            .to_string()
        } else if self.context_json {
            json!({ "hookSpecificOutput": { "additionalContext": context } }).to_string()
        } else {
            context.to_owned()
        }
    }

    /// Whether its hooks see a turn start (`prompt-submit`), and so can be trusted for the
    /// whole status. Without one, the screen still tells when it works or asks.
    pub fn reports_turns(&self) -> bool {
        self.events.iter().any(|(_, ours)| *ours == "prompt-submit")
    }
}

/// Whether the hooks of agent `kind` report its whole status (see [`HookAgent::reports_turns`]).
/// Agents without managed hooks report through hooks of their own making: trusted.
pub fn reports_turns(kind: &str) -> bool {
    AGENTS
        .iter()
        .find(|a| a.kind == kind)
        .is_none_or(HookAgent::reports_turns)
}

/// A config directory under an optional env-var override, else `~/<dir>`.
fn config_dir(env: &str, dir: &str) -> PathBuf {
    std::env::var_os(env)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs_home().join(dir))
}

fn claude_settings() -> PathBuf {
    config_dir("CLAUDE_CONFIG_DIR", ".claude").join("settings.json")
}

fn codex_settings() -> PathBuf {
    config_dir("CODEX_HOME", ".codex").join("hooks.json")
}

fn pi_agent_dir() -> PathBuf {
    config_dir("PI_CODING_AGENT_DIR", ".pi/agent")
}

fn pi_settings() -> PathBuf {
    pi_agent_dir().join("extensions").join("thurm.js")
}

fn agy_settings() -> PathBuf {
    dirs_home().join(".gemini/config/hooks.json")
}

/// Claude Code signals a permission prompt as a notification with this type.
fn claude_permission(payload: &Value) -> bool {
    payload.get("notification_type").and_then(Value::as_str) == Some("permission_prompt")
}

/// Default: a notification is never a permission prompt.
fn no_permission(_: &Value) -> bool {
    false
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Shell command for one hook. Runs the `thurm` next to the daemon (`$THURM_BIN_DIR`, set in
/// every pane) or from PATH, and does nothing outside Thurm.
fn command(kind: &str, event: &str) -> String {
    format!(
        r#"[ -n "$THURM_PANE_ID" ] && "${{THURM_BIN_DIR:+$THURM_BIN_DIR/}}thurm" agent-hook {kind} {event}; true"#
    )
}

fn is_ours(hook: &Value, kind: &str) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains("thurm") && c.contains(&format!("agent-hook {kind}")))
}

fn is_ours_pi_extension(content: &str) -> bool {
    content.contains(PI_MARKER)
}

fn apply_antigravity(agent: &HookAgent, settings: &str, install: bool) -> Result<String, String> {
    let mut root: Value = if settings.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(settings).map_err(|e| format!("hooks.json: {e}"))?
    };
    let obj = root.as_object_mut().ok_or("hooks.json is not an object")?;
    if install {
        let group = obj
            .entry("thurm")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or("\"thurm\" in hooks.json is not an object")?;
        for (native, ours) in agent.events {
            let list = group
                .entry(*native)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| format!("thurm.{native} is not an array"))?;
            list.retain(|h| !is_ours(h, agent.kind));
            list.push(json!({
                "type": "command",
                "command": command(agent.kind, ours),
                "timeout": 5
            }));
        }
    } else if let Some(group) = obj.get_mut("thurm").and_then(Value::as_object_mut) {
        for (native, _) in agent.events {
            if let Some(list) = group.get_mut(*native).and_then(Value::as_array_mut) {
                list.retain(|h| !is_ours(h, agent.kind));
                if list.is_empty() {
                    group.remove(*native);
                }
            }
        }
        if group.is_empty() {
            obj.remove("thurm");
        }
    }
    let mut out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    out.push('\n');
    Ok(out)
}

/// `settings` (the file's text, empty if missing) with our hooks added (`install`) or
/// removed. Other settings and other hooks are left alone.
pub fn apply(agent: &HookAgent, settings: &str, install: bool) -> Result<String, String> {
    if agent.format == HookFormat::PiExtension {
        if !settings.is_empty() && !is_ours_pi_extension(settings) {
            let verb = if install { "replacing" } else { "removing" };
            return Err(format!("thurm.js exists and isn't Thurm's; not {verb} it"));
        }
        return Ok(if install {
            PI_EXTENSION.to_owned()
        } else {
            String::new()
        });
    }
    if agent.format == HookFormat::AntigravityHooks {
        return apply_antigravity(agent, settings, install);
    }
    let mut root: Value = if settings.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(settings).map_err(|e| format!("settings.json: {e}"))?
    };
    let obj = root
        .as_object_mut()
        .ok_or("settings.json is not an object")?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("\"hooks\" is not an object")?;
    for (claude_event, ours) in agent.events {
        let list = hooks
            .entry(*claude_event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{claude_event} is not an array"))?;
        for entry in list.iter_mut() {
            if let Some(hs) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                hs.retain(|h| !is_ours(h, agent.kind));
            }
        }
        list.retain(|e| {
            e.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|h| !h.is_empty())
        });
        if install {
            list.push(
                json!({ "hooks": [{ "type": "command", "command": command(agent.kind, ours), "timeout": 5 }] }),
            );
        }
        if list.is_empty() {
            hooks.remove(*claude_event);
        }
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    let mut out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    out.push('\n');
    Ok(out)
}

/// How many of our hook events `settings` has.
pub fn installed(agent: &HookAgent, settings: &str) -> usize {
    if agent.format == HookFormat::PiExtension {
        return if settings == PI_EXTENSION {
            agent.events.len()
        } else {
            0
        };
    }
    let Ok(root) = serde_json::from_str::<Value>(settings) else {
        return 0;
    };
    if agent.format == HookFormat::AntigravityHooks {
        return agent
            .events
            .iter()
            .filter(|(e, _)| {
                root.pointer(&format!("/thurm/{e}"))
                    .and_then(Value::as_array)
                    .is_some_and(|l| l.iter().any(|x| is_ours(x, agent.kind)))
            })
            .count();
    }
    agent
        .events
        .iter()
        .filter(|(e, _)| {
            root.pointer(&format!("/hooks/{e}"))
                .and_then(Value::as_array)
                .is_some_and(|l| {
                    l.iter().any(|entry| {
                        entry
                            .get("hooks")
                            .and_then(Value::as_array)
                            .is_some_and(|h| h.iter().any(|x| is_ours(x, agent.kind)))
                    })
                })
        })
        .count()
}

/// What [`write`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Unchanged,
    Changed,
}

fn write_pi_extension(agent: &HookAgent, path: &Path, install: bool) -> Result<Written, String> {
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    let existed = path.exists() || std::fs::symlink_metadata(path).is_ok();
    let current = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io(e)),
    };
    if existed && !is_ours_pi_extension(&current) {
        let verb = if install { "replacing" } else { "removing" };
        return Err(format!(
            "{} exists and isn't Thurm's; not {verb} it",
            path.display()
        ));
    }
    if !install {
        if !existed {
            return Ok(Written::Unchanged);
        }
        std::fs::remove_file(path).map_err(io)?;
        return Ok(Written::Changed);
    }
    let updated = apply(agent, &current, true).map_err(|e| format!("{}: {e}", path.display()))?;
    if updated == current {
        return Ok(Written::Unchanged);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    crate::replace_file(path, updated.as_bytes()).map_err(io)?;
    Ok(Written::Changed)
}

/// Adds (`install`) or removes our hooks in `agent`'s file at `path`, keeping a one-time backup
/// of what was there (`*.json.thurm-backup`) and replacing the file atomically.
pub fn write_at(agent: &HookAgent, path: &Path, install: bool) -> Result<Written, String> {
    if agent.format == HookFormat::PiExtension {
        return write_pi_extension(agent, path, install);
    }
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    let current = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io(e)),
    };
    let updated = apply(agent, &current, install)?;
    if updated == current {
        return Ok(Written::Unchanged);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    if !current.is_empty() {
        let backup = path.with_extension("json.thurm-backup");
        if !backup.exists() {
            // The settings may hold tokens: the backup is private.
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            options
                .open(&backup)
                .and_then(|mut f| f.write_all(current.as_bytes()))
                .map_err(io)?;
        }
    }
    // A symlinked settings file stays a link, and the file keeps its permissions.
    crate::replace_file(path, updated.as_bytes()).map_err(io)?;
    Ok(Written::Changed)
}

/// The hook agent a command line starts (`claude …`, `/path/to/codex …`), if any.
pub fn agent_for_command(command: &[String]) -> Option<&'static HookAgent> {
    let program = Path::new(command.first()?).file_name()?.to_str()?;
    AGENTS.iter().find(|a| a.kind == program)
}

/// Installs `agent`'s hooks unless they all are there already. `Ok(true)`: the file changed.
pub fn ensure_at(agent: &HookAgent, path: &Path) -> Result<bool, String> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if installed(agent, &current) == agent.events.len() {
        return Ok(false);
    }
    write_at(agent, path, true).map(|w| w == Written::Changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent_and_keeps_other_settings() {
        let existing = r#"{
  "model": "opus",
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "say done" }] }]
  },
  "permissions": { "allow": ["Bash(ls)"] }
}"#;
        let once = apply(&AGENTS[0], existing, true).unwrap();
        let twice = apply(&AGENTS[0], &once, true).unwrap();
        assert_eq!(once, twice);
        assert_eq!(installed(&AGENTS[0], &once), CLAUDE_EVENTS.len());
        let v: Value = serde_json::from_str(&once).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls)");
        // The user's own Stop hook survives, ours is added next to it.
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "say done");
        // Key order is preserved.
        assert!(once.find("\"model\"").unwrap() < once.find("\"permissions\"").unwrap());

        let removed = apply(&AGENTS[0], &once, false).unwrap();
        assert_eq!(installed(&AGENTS[0], &removed), 0);
        let v: Value = serde_json::from_str(&removed).unwrap();
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert!(v["hooks"].get("SessionStart").is_none());
    }

    #[test]
    fn fresh_file_and_full_removal() {
        let out = apply(&AGENTS[0], "", true).unwrap();
        assert!(out.contains("agent-hook claude stop"));
        assert_eq!(apply(&AGENTS[0], &out, false).unwrap().trim(), "{}");
        assert!(apply(&AGENTS[0], "[1]", true).is_err());
    }

    #[test]
    fn codex_and_claude_entries_are_independent() {
        let codex = &AGENTS[1];
        let both = apply(codex, &apply(&AGENTS[0], "", true).unwrap(), true).unwrap();
        assert_eq!(installed(codex, &both), CODEX_EVENTS.len());
        assert_eq!(installed(&AGENTS[0], &both), CLAUDE_EVENTS.len());
        // Removing Codex leaves Claude's entries alone.
        let claude_only = apply(codex, &both, false).unwrap();
        assert_eq!(installed(codex, &claude_only), 0);
        assert_eq!(installed(&AGENTS[0], &claude_only), CLAUDE_EVENTS.len());
        assert!(both.contains("agent-hook codex stop"));
    }

    #[test]
    fn ensure_installs_once_and_backs_up() {
        let dir = std::env::temp_dir().join(format!("thurm-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(".claude/settings.json");
        // No directory yet (the agent never ran): created.
        assert!(ensure_at(&AGENTS[0], &path).unwrap());
        assert!(!ensure_at(&AGENTS[0], &path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(installed(&AGENTS[0], &text), CLAUDE_EVENTS.len());
        assert!(!path.with_extension("json.thurm-backup").exists());

        // An existing file is kept as the backup.
        std::fs::write(&path, "{\"model\": \"opus\"}").unwrap();
        assert!(ensure_at(&AGENTS[0], &path).unwrap());
        let backup = std::fs::read_to_string(path.with_extension("json.thurm-backup")).unwrap();
        assert!(backup.contains("opus"));
        assert_eq!(
            write_at(&AGENTS[0], &path, false).unwrap(),
            Written::Changed
        );
        assert_eq!(
            write_at(&AGENTS[0], &path, false).unwrap(),
            Written::Unchanged
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewrites_keep_links_and_private_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("thurm-hooks-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        // A private settings file, linked from where the agent reads it.
        let real = dir.join("dotfiles/settings.json");
        std::fs::write(&real, r#"{"apiKeyHelper": "secret"}"#).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(write_at(&AGENTS[0], &link, true).unwrap(), Written::Changed);
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            installed(&AGENTS[0], &std::fs::read_to_string(&real).unwrap()),
            CLAUDE_EVENTS.len()
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&real), 0o600);
        assert_eq!(mode(&link.with_extension("json.thurm-backup")), 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn agents_from_command_lines() {
        let cmd = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            agent_for_command(&cmd(&["claude", "--resume"])).map(|a| a.kind),
            Some("claude")
        );
        assert_eq!(
            agent_for_command(&cmd(&["/opt/bin/codex"])).map(|a| a.kind),
            Some("codex")
        );
        assert_eq!(
            agent_for_command(&cmd(&["pi", "--continue"])).map(|a| a.kind),
            Some("pi")
        );
        assert_eq!(
            agent_for_command(&cmd(&["agy", "--continue"])).map(|a| a.kind),
            Some("agy")
        );
        assert!(agent_for_command(&cmd(&["bash"])).is_none());
        assert!(agent_for_command(&[]).is_none());
    }

    #[test]
    fn command_is_guarded() {
        let c = command("claude", "stop");
        assert!(c.starts_with(r#"[ -n "$THURM_PANE_ID" ]"#));
        assert!(c.ends_with("; true"));
        assert_eq!(AGENTS[0].normalize("UserPromptSubmit"), "prompt-submit");
        assert_eq!(AGENTS[0].normalize("stop"), "stop");
    }

    #[test]
    fn permission_prompts_are_told_apart() {
        let claude = &AGENTS[0];
        let payload = |t: &str| json!({ "notification_type": t, "message": "…" });
        assert_eq!(
            claude.hook_event("Notification", &payload("permission_prompt")),
            "permission-prompt"
        );
        assert_eq!(
            claude.hook_event("notification", &payload("idle_prompt")),
            "notification"
        );
        assert_eq!(
            claude.hook_event("Notification", &json!({})),
            "notification"
        );
        // Only notifications are reclassified.
        assert_eq!(
            claude.hook_event("Stop", &payload("permission_prompt")),
            "stop"
        );
    }

    #[test]
    fn an_agent_brings_its_own_settings_and_classifier() {
        fn settings() -> PathBuf {
            PathBuf::from("/tmp/example/hooks.json")
        }
        fn permission(payload: &Value) -> bool {
            payload.get("kind") == Some(&json!("ask"))
        }
        const EVENTS: &[(&str, &str)] =
            &[("SessionStart", "session-start"), ("Ask", "notification")];
        let agent = HookAgent {
            kind: "example",
            name: "Example",
            format: HookFormat::JsonMap,
            events: EVENTS,
            settings,
            permission,
            context_json: false,
        };
        assert_eq!(
            agent.settings_path(),
            PathBuf::from("/tmp/example/hooks.json")
        );
        assert_eq!(agent.normalize("session-start"), "session-start");
        assert_eq!(agent.normalize("Ask"), "notification");
        assert_eq!(
            agent.hook_event("Ask", &json!({ "kind": "ask" })),
            "permission-prompt"
        );
        assert_eq!(
            agent.hook_event("Ask", &json!({ "kind": "note" })),
            "notification"
        );
        assert!(!agent.reports_turns());
        assert_eq!(agent.session_output("in pane 7"), "in pane 7");
        let agent = HookAgent {
            context_json: true,
            ..agent
        };
        let out: Value = serde_json::from_str(&agent.session_output("in \"pane\" 7")).unwrap();
        assert_eq!(
            out.pointer("/hookSpecificOutput/additionalContext"),
            Some(&json!("in \"pane\" 7"))
        );
        assert!(reports_turns("claude"));
        assert!(reports_turns("codex"));
        assert!(reports_turns("pi"));
        assert!(reports_turns("agy"));
    }

    #[test]
    fn antigravity_hooks_use_flat_named_entries_and_inject_steps() {
        let agy = AGENTS.iter().find(|a| a.kind == "agy").unwrap();
        let existing = r#"{
  "my-linter": {
    "PostToolUse": [
      { "matcher": "run_command", "hooks": [{ "type": "command", "command": "./lint.sh" }] }
    ]
  }
}"#;
        let once = apply(agy, existing, true).unwrap();
        let twice = apply(agy, &once, true).unwrap();
        assert_eq!(once, twice);
        assert_eq!(installed(agy, &once), AGY_EVENTS.len());
        let v: Value = serde_json::from_str(&once).unwrap();
        // Existing user hook stays untouched.
        assert_eq!(
            v["my-linter"]["PostToolUse"][0]["matcher"],
            json!("run_command")
        );
        // Non-tool events use flat command objects directly (not nested under `hooks`).
        let pre = &v["thurm"]["PreInvocation"][0];
        assert_eq!(pre["type"], json!("command"));
        assert!(
            pre["command"]
                .as_str()
                .unwrap()
                .contains("agent-hook agy prompt-submit")
        );
        assert!(pre.get("hooks").is_none());

        // SessionStart output uses Antigravity's injectSteps contract.
        let out: Value = serde_json::from_str(&agy.session_output("in pane 3")).unwrap();
        assert_eq!(
            out.pointer("/injectSteps/0/systemMessage/systemMessage"),
            Some(&json!("in pane 3"))
        );

        // Uninstall removes only our "thurm" hook group.
        let removed = apply(agy, &once, false).unwrap();
        assert_eq!(installed(agy, &removed), 0);
        let v: Value = serde_json::from_str(&removed).unwrap();
        assert!(v.get("thurm").is_none());
        assert!(v.get("my-linter").is_some());
    }

    #[test]
    fn pi_extension_install_upgrade_remove_and_protect_other() {
        let pi = AGENTS.iter().find(|a| a.kind == "pi").unwrap();
        let dir = std::env::temp_dir().join(format!("thurm-pi-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("extensions/thurm.js");

        // Fresh install creates directories and writes the extension.
        assert_eq!(installed(pi, ""), 0);
        assert!(ensure_at(pi, &path).unwrap());
        assert!(!ensure_at(pi, &path).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(installed(pi, &text), PI_EVENTS.len());
        assert!(text.contains(PI_MARKER));

        // Outdated version of our extension is replaced.
        std::fs::write(
            &path,
            "// thurm-pi-extension: 0\nexport default () => {};\n",
        )
        .unwrap();
        assert_eq!(installed(pi, &std::fs::read_to_string(&path).unwrap()), 0);
        assert!(ensure_at(pi, &path).unwrap());
        assert_eq!(
            installed(pi, &std::fs::read_to_string(&path).unwrap()),
            PI_EVENTS.len()
        );

        // Uninstall removes the file completely and is idempotent.
        assert_eq!(write_at(pi, &path, false).unwrap(), Written::Changed);
        assert!(!path.exists());
        assert_eq!(write_at(pi, &path, false).unwrap(), Written::Unchanged);

        // A user's own thurm.js without our marker is left untouched (including zero-byte files).
        std::fs::write(&path, "export default function () {}\n").unwrap();
        assert!(write_at(pi, &path, true).is_err());
        assert!(write_at(pi, &path, false).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "export default function () {}\n"
        );
        std::fs::write(&path, "").unwrap();
        assert!(write_at(pi, &path, true).is_err());
        assert!(write_at(pi, &path, false).is_err());
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
