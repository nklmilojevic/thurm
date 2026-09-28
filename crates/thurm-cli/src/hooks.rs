//! `thurm hooks`: install the agent hooks that report status to Thurm (`thurm agent-hook`).
//!
//! Claude Code (`settings.json`) and Codex (`~/.codex/hooks.json`) share one hook-map format;
//! each of our entries runs `thurm agent-hook <agent> <event>` and is a no-op outside Thurm
//! panes, so the same config works in any terminal.

use std::path::PathBuf;

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
    fn command_is_guarded() {
        let c = command("claude", "stop");
        assert!(c.starts_with(r#"[ -n "$THURM_PANE_ID" ]"#));
        assert!(c.ends_with("; true"));
        assert_eq!(normalize_event("UserPromptSubmit"), "prompt-submit");
        assert_eq!(normalize_event("stop"), "stop");
    }
}
