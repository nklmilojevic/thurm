//! Coding-agent definitions used for detection, status tracking, launching and resuming.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AgentDef {
    /// Stable identifier ("claude").
    pub kind: String,
    /// Display name ("Claude Code").
    pub name: String,
    /// Executable basenames of the foreground process that identify the agent.
    pub processes: Vec<String>,
    /// Substrings of the foreground process' argv that identify the agent (for agents that
    /// run as `node …/cli.js` or `python -m …`).
    pub argv: Vec<String>,
    /// Screen text meaning "the agent is working" (e.g. "esc to interrupt").
    pub working: Vec<String>,
    /// Screen text meaning "the agent waits for the user" (permission prompts, questions).
    pub attention: Vec<String>,
    /// Command used by launch presets.
    pub launch: Option<Vec<String>>,
    /// Command that resumes the last conversation in the same directory, used when restoring
    /// a session after a reboot.
    pub resume: Option<Vec<String>>,
    /// Resumes one specific session; `{session}` is replaced with the id reported by the
    /// agent's hooks. Preferred over `resume` when the id is known.
    pub resume_session: Option<Vec<String>>,
    /// Starts a copy of one session (`{session}` as above), for "Fork Agent Session".
    pub fork_session: Option<Vec<String>>,
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

pub fn builtin_agents() -> Vec<AgentDef> {
    vec![
        AgentDef {
            kind: "claude".into(),
            name: "Claude Code".into(),
            processes: s(&["claude"]),
            argv: s(&["@anthropic-ai/claude-code", "claude-code/cli"]),
            working: s(&["esc to interrupt", "ctrl+c to interrupt"]),
            attention: s(&[
                "Do you want to proceed?",
                "Do you want to make this edit",
                "Do you want to create",
                "Do you want to allow",
                "❯ 1. Yes",
                "Would you like to proceed?",
            ]),
            launch: Some(s(&["claude"])),
            resume: Some(s(&["claude", "--continue"])),
            resume_session: Some(s(&["claude", "--resume", "{session}"])),
            fork_session: Some(s(&["claude", "--resume", "{session}", "--fork-session"])),
        },
        AgentDef {
            kind: "codex".into(),
            name: "Codex".into(),
            processes: s(&["codex"]),
            argv: s(&["@openai/codex"]),
            working: s(&["esc to interrupt", "Esc to interrupt"]),
            attention: s(&[
                "Allow command?",
                "Would you like to run the following command?",
                "Would you like to make the following edits?",
                "Yes, proceed",
            ]),
            launch: Some(s(&["codex"])),
            resume: Some(s(&["codex", "resume", "--last"])),
            resume_session: Some(s(&["codex", "resume", "{session}"])),
            fork_session: Some(s(&["codex", "fork", "{session}"])),
        },
        AgentDef {
            kind: "gemini".into(),
            name: "Gemini CLI".into(),
            processes: s(&["gemini"]),
            argv: s(&["@google/gemini-cli"]),
            working: s(&["esc to cancel"]),
            attention: s(&[
                "Allow execution",
                "Apply this change?",
                "Waiting for user confirmation",
            ]),
            launch: Some(s(&["gemini"])),
            resume: None,
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "aider".into(),
            name: "Aider".into(),
            processes: s(&["aider"]),
            argv: s(&["aider"]),
            working: s(&[]),
            attention: s(&["(Y)es/(N)o", "[Yes]:"]),
            launch: Some(s(&["aider"])),
            resume: Some(s(&["aider", "--restore-chat-history"])),
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "opencode".into(),
            name: "OpenCode".into(),
            processes: s(&["opencode"]),
            argv: s(&["opencode-ai"]),
            working: s(&["esc interrupt", "esc to interrupt"]),
            attention: s(&["Permission required", "Allow once"]),
            launch: Some(s(&["opencode"])),
            resume: Some(s(&["opencode", "--continue"])),
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "goose".into(),
            name: "Goose".into(),
            processes: s(&["goose"]),
            argv: s(&[]),
            working: s(&[]),
            attention: s(&["Allow?"]),
            launch: Some(s(&["goose", "session"])),
            resume: Some(s(&["goose", "session", "--resume"])),
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "cursor".into(),
            name: "Cursor Agent".into(),
            processes: s(&["cursor-agent"]),
            argv: s(&["cursor-agent"]),
            working: s(&["ctrl+c to stop"]),
            attention: s(&["Run this command?", "(y) (enter)"]),
            launch: Some(s(&["cursor-agent"])),
            resume: Some(s(&["cursor-agent", "resume"])),
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "amp".into(),
            name: "Amp".into(),
            processes: s(&["amp"]),
            argv: s(&["@sourcegraph/amp"]),
            working: s(&["Esc to cancel"]),
            attention: s(&["Approve", "Allow"]),
            launch: Some(s(&["amp"])),
            resume: None,
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "copilot".into(),
            name: "Copilot CLI".into(),
            processes: s(&["copilot"]),
            argv: s(&["@github/copilot"]),
            working: s(&["Esc to cancel"]),
            attention: s(&["Do you want to run this command?", "Allow"]),
            launch: Some(s(&["copilot"])),
            resume: Some(s(&["copilot", "--continue"])),
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "qwen".into(),
            name: "Qwen Code".into(),
            processes: s(&["qwen"]),
            argv: s(&["@qwen-code/qwen-code"]),
            working: s(&["esc to cancel"]),
            attention: s(&["Allow execution", "Apply this change?"]),
            launch: Some(s(&["qwen"])),
            resume: None,
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "crush".into(),
            name: "Crush".into(),
            processes: s(&["crush"]),
            argv: s(&[]),
            working: s(&[]),
            attention: s(&["Allow", "Permission"]),
            launch: Some(s(&["crush"])),
            resume: None,
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "droid".into(),
            name: "Factory Droid".into(),
            processes: s(&["droid"]),
            argv: s(&[]),
            working: s(&["esc to stop"]),
            attention: s(&["Allow", "Approve"]),
            launch: Some(s(&["droid"])),
            resume: None,
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "kiro".into(),
            name: "Kiro CLI".into(),
            processes: s(&["kiro-cli", "q"]),
            argv: s(&[]),
            working: s(&[]),
            attention: s(&["Allow this action?"]),
            launch: Some(s(&["kiro-cli", "chat"])),
            resume: Some(s(&["kiro-cli", "chat", "--resume"])),
            resume_session: None,
            fork_session: None,
        },
        AgentDef {
            kind: "grok".into(),
            name: "Grok CLI".into(),
            processes: s(&["grok"]),
            argv: s(&["@vibe-kit/grok-cli"]),
            working: s(&[]),
            attention: s(&["Do you want to proceed"]),
            launch: Some(s(&["grok"])),
            resume: None,
            resume_session: None,
            fork_session: None,
        },
    ]
}

impl AgentDef {
    /// Does this definition describe the given process?
    pub fn matches(&self, name: &str, argv: &[String]) -> bool {
        let base = name.rsplit('/').next().unwrap_or(name);
        if self.processes.iter().any(|p| p == base) {
            return true;
        }
        // Interpreters: check argv[0] basename and argv substrings.
        if let Some(first) = argv.first() {
            let first = first.rsplit('/').next().unwrap_or(first);
            if self.processes.iter().any(|p| p == first) {
                return true;
            }
        }
        if self.argv.is_empty() {
            return false;
        }
        let interp = matches!(
            base,
            "node" | "bun" | "deno" | "python" | "python3" | "uv" | "npx"
        ) || base.starts_with("python");
        interp
            && argv
                .iter()
                .skip(1)
                .any(|a| self.argv.iter().any(|needle| a.contains(needle.as_str())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(kind: &str) -> AgentDef {
        builtin_agents()
            .into_iter()
            .find(|d| d.kind == kind)
            .unwrap()
    }

    #[test]
    fn matches_binaries_and_interpreters() {
        let claude = def("claude");
        assert!(claude.matches("claude", &[]));
        assert!(claude.matches("/usr/local/bin/claude", &["claude".into()]));
        assert!(claude.matches(
            "node",
            &[
                "node".into(),
                "/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js".into()
            ]
        ));
        assert!(!claude.matches("node", &["node".into(), "server.js".into()]));
        assert!(!claude.matches("vim", &["vim".into(), "@anthropic-ai/claude-code".into()]));
        let aider = def("aider");
        assert!(aider.matches(
            "python3.12",
            &["python3.12".into(), "/usr/bin/aider".into()]
        ));
    }
}
