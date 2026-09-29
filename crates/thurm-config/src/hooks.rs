//! `thurm hooks`: install the agent hooks that report status to Thurm (`thurm agent-hook`).
//!
//! Claude Code (`settings.json`) and Codex (`~/.codex/hooks.json`) share one hook-map format;
//! each of our entries runs `thurm agent-hook <agent> <event>` and is a no-op outside Thurm
//! panes, so the same config works in any terminal.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// An agent whose hooks we manage.
pub struct HookAgent {
    /// Agent kind as the daemon knows it ("claude").
    pub kind: &'static str,
    pub name: &'static str,
    /// Hook events it supports → our event names.
    pub events: &'static [(&'static str, &'static str)],
}

pub const AGENTS: &[HookAgent] = &[
    HookAgent {
        kind: "claude",
        name: "Claude Code",
        events: CLAUDE_EVENTS,
    },
    HookAgent {
        kind: "codex",
        name: "Codex",
        events: CODEX_EVENTS,
    },
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

/// Our event name for a Claude hook event name (either spelling is accepted).
pub fn normalize_event(e: &str) -> String {
    CLAUDE_EVENTS
        .iter()
        .find(|(claude, ours)| claude.eq_ignore_ascii_case(e) || ours.eq_ignore_ascii_case(e))
        .map(|(_, ours)| (*ours).to_owned())
        .unwrap_or_else(|| e.to_ascii_lowercase())
}

/// Our event name for a hook event and the JSON payload the agent sent with it: Claude Code's
/// permission dialog (a notification the notification can answer) is `permission-prompt`.
pub fn hook_event(event: &str, payload: &Value) -> String {
    let event = normalize_event(event);
    let permission =
        payload.get("notification_type").and_then(Value::as_str) == Some("permission_prompt");
    if event == "notification" && permission {
        "permission-prompt".into()
    } else {
        event
    }
}

/// The file holding `agent`'s hooks.
pub fn settings_path(agent: &HookAgent) -> PathBuf {
    match agent.kind {
        "codex" => std::env::var_os("CODEX_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs_home().join(".codex"))
            .join("hooks.json"),
        _ => std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs_home().join(".claude"))
            .join("settings.json"),
    }
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

/// `settings` (the file's text, empty if missing) with our hooks added (`install`) or
/// removed. Other settings and other hooks are left alone.
pub fn apply(agent: &HookAgent, settings: &str, install: bool) -> Result<String, String> {
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
    let Ok(root) = serde_json::from_str::<Value>(settings) else {
        return 0;
    };
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

/// Adds (`install`) or removes our hooks in `agent`'s file at `path`, keeping a one-time backup
/// of what was there (`*.json.thurm-backup`) and replacing the file atomically.
pub fn write_at(agent: &HookAgent, path: &Path, install: bool) -> Result<Written, String> {
    let current = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let updated = apply(agent, &current, install)?;
    if updated == current {
        return Ok(Written::Unchanged);
    }
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    if !current.is_empty() {
        let backup = path.with_extension("json.thurm-backup");
        if !backup.exists() {
            std::fs::write(&backup, &current).map_err(io)?;
        }
    }
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, updated).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)?;
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
        assert!(agent_for_command(&cmd(&["bash"])).is_none());
        assert!(agent_for_command(&[]).is_none());
    }

    #[test]
    fn command_is_guarded() {
        let c = command("claude", "stop");
        assert!(c.starts_with(r#"[ -n "$THURM_PANE_ID" ]"#));
        assert!(c.ends_with("; true"));
        assert_eq!(normalize_event("UserPromptSubmit"), "prompt-submit");
        assert_eq!(normalize_event("stop"), "stop");
    }

    #[test]
    fn permission_prompts_are_told_apart() {
        let payload = |t: &str| json!({ "notification_type": t, "message": "…" });
        assert_eq!(
            hook_event("Notification", &payload("permission_prompt")),
            "permission-prompt"
        );
        assert_eq!(
            hook_event("notification", &payload("idle_prompt")),
            "notification"
        );
        assert_eq!(hook_event("Notification", &json!({})), "notification");
        // Only notifications are reclassified.
        assert_eq!(hook_event("Stop", &payload("permission_prompt")), "stop");
    }
}
