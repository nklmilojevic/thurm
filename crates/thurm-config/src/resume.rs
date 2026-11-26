//! Program Resume Protocol (OSC 7577) configuration and trust rules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Whether and how Thurm resumes programs that reported a resume command via OSC 7577.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResumeProgramsMode {
    /// Never resume OSC 7577 reports.
    Off,
    /// Resume only commands allowed by a trust rule (built-in or `[[session.resume_rules]]`).
    #[default]
    Rules,
    /// Resume any verified OSC 7577 report from an unwrapped process of the same user.
    All,
}

/// How many values an allowed flag accepts after the matched command prefix.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FlagArity {
    /// Takes no value (e.g. `--verbose`).
    #[default]
    None,
    /// Takes exactly one value, even if it starts with `-` (e.g. `--model opus`).
    One,
    /// Takes one value if the next argument does not start with `-`.
    Optional,
    /// Takes every following argument up to the next one that starts with `-`.
    Many,
}

/// A trust rule allowing an OSC 7577 resume command (`[[session.resume_rules]]`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct ResumeRule {
    /// Rule name (e.g. `"claude"`, `"nvim"`, `"lazygit"`). Matching a built-in rule's name
    /// overrides or disables (`enabled = false`) it.
    pub name: String,
    /// Set to `false` to disable a built-in rule with the same `name`.
    pub enabled: bool,
    /// Expected start of `argv`. The first element is a bare program name or an absolute path.
    /// Later elements match literals or placeholders:
    /// - `"<uuid>"`: a canonical UUID
    /// - `"<id>"`: 1..=128 ASCII letters/digits/underscores/hyphens starting with alphanumeric
    /// - `"<under:DIR>"`: an absolute path strictly inside `DIR` (`~` expanded)
    /// - `"<oneof:a|b>"`: any one of the `|`-separated literals
    pub command: Vec<String>,
    /// Directories under which the program may be resumed (`~` expanded). Empty allows any.
    pub dirs: Vec<String>,
    /// Extra flags allowed after `command`. When present, each reported flag must also appear
    /// with the same values on the owning process's launch command line.
    pub flags: BTreeMap<String, FlagArity>,
    /// Restricting flags that must survive a resume: every launch occurrence of one of these
    /// flags must be reported again with the same values, and the report may not add any. Also
    /// allowed in reports (no need to repeat them in `flags`).
    pub preserve_flags: BTreeMap<String, FlagArity>,
    /// Require the owning OS process to match `command[0]` (default: `true`).
    pub match_process: bool,
}

impl ResumeRule {
    /// All flags referenced by either `flags` or `preserve_flags`.
    pub fn all_checked_flags(&self) -> BTreeMap<String, FlagArity> {
        let mut all = self.flags.clone();
        for (k, &v) in &self.preserve_flags {
            all.entry(k.clone()).or_insert(v);
        }
        all
    }
}

impl Default for ResumeRule {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            command: Vec::new(),
            dirs: Vec::new(),
            flags: BTreeMap::new(),
            preserve_flags: BTreeMap::new(),
            match_process: true,
        }
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

fn flags(entries: &[(&str, FlagArity)]) -> BTreeMap<String, FlagArity> {
    entries.iter().map(|&(k, v)| (k.to_owned(), v)).collect()
}

/// Built-in trust rules for common agents that support (or are expected to support) OSC 7577.
pub fn builtin_resume_rules() -> Vec<ResumeRule> {
    use FlagArity::*;
    vec![
        ResumeRule {
            name: "claude".into(),
            enabled: true,
            command: s(&["claude", "<oneof:--resume|-r>", "<id>"]),
            dirs: Vec::new(),
            flags: flags(&[
                ("--add-dir", Many),
                ("--agent", One),
                ("--allow-dangerously-skip-permissions", None),
                ("--allowed-tools", Many),
                ("--allowedTools", Many),
                ("--append-system-prompt", One),
                ("--betas", Many),
                ("--brief", None),
                ("--chrome", None),
                ("--dangerously-skip-permissions", None),
                ("--debug", Optional),
                ("--disallowed-tools", Many),
                ("--disallowedTools", Many),
                ("--effort", One),
                ("--fallback-model", One),
                ("--ide", None),
                ("--max-budget-usd", One),
                ("--max-turns", One),
                ("--mcp-config", Many),
                ("--model", One),
                ("--no-chrome", None),
                ("--no-session-persistence", None),
                ("--permission-mode", One),
                ("--permission-prompt-tool", One),
                ("--plugin-dir", Many),
                ("--setting-sources", One),
                ("--settings", One),
                ("--strict-mcp-config", None),
                ("--system-prompt", One),
                ("--tools", Many),
                ("--verbose", None),
                ("-d", Optional),
                ("-w", Optional),
                ("--worktree", Optional),
            ]),
            preserve_flags: flags(&[
                ("--disallowed-tools", Many),
                ("--disallowedTools", Many),
                ("--mcp-config", Many),
                ("--strict-mcp-config", None),
            ]),
            match_process: true,
        },
        ResumeRule {
            name: "codex".into(),
            enabled: true,
            command: s(&["codex", "resume", "<id>"]),
            dirs: Vec::new(),
            flags: BTreeMap::new(),
            preserve_flags: BTreeMap::new(),
            match_process: true,
        },
        ResumeRule {
            name: "opencode".into(),
            enabled: true,
            command: s(&["opencode", "<oneof:--session|-s>", "<id>"]),
            dirs: Vec::new(),
            flags: BTreeMap::new(),
            preserve_flags: BTreeMap::new(),
            match_process: true,
        },
        ResumeRule {
            name: "pi".into(),
            enabled: true,
            command: s(&["pi", "--session", "<id>"]),
            dirs: Vec::new(),
            flags: BTreeMap::new(),
            preserve_flags: BTreeMap::new(),
            match_process: true,
        },
        ResumeRule {
            name: "agy".into(),
            enabled: true,
            command: s(&["agy", "--conversation", "<id>"]),
            dirs: Vec::new(),
            flags: BTreeMap::new(),
            preserve_flags: BTreeMap::new(),
            match_process: true,
        },
    ]
}

/// Effective trust rules: built-ins (when `include_defaults`) merged with user rules.
///
/// A user rule with the same `name` as a built-in rule disables it when `enabled = false`,
/// restricts its `dirs` / `match_process` when `command`, `flags` and `preserve_flags` are
/// omitted, or replaces it when any of them is provided (keeping the built-in's `command` and
/// `preserve_flags` where the user rule leaves them empty).
pub fn resolve_resume_rules(include_defaults: bool, user: &[ResumeRule]) -> Vec<ResumeRule> {
    let mut rules = if include_defaults {
        builtin_resume_rules()
    } else {
        Vec::new()
    };
    for u in user {
        if !u.name.is_empty()
            && let Some(idx) = rules.iter().position(|r| r.name == u.name)
        {
            if !u.enabled {
                rules.remove(idx);
            } else if u.command.is_empty() && u.flags.is_empty() && u.preserve_flags.is_empty() {
                let existing = &mut rules[idx];
                if !u.dirs.is_empty() {
                    existing.dirs = u.dirs.clone();
                }
                existing.match_process = u.match_process;
            } else {
                let mut replacement = u.clone();
                if replacement.command.is_empty() {
                    replacement.command = rules[idx].command.clone();
                }
                if replacement.preserve_flags.is_empty() {
                    replacement.preserve_flags = rules[idx].preserve_flags.clone();
                }
                rules[idx] = replacement;
            }
            continue;
        }
        if u.enabled && !u.command.is_empty() {
            rules.push(u.clone());
        }
    }
    rules.retain(|r| r.enabled && !r.command.is_empty());
    rules
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_builtin_overrides_and_disables() {
        let user = vec![
            ResumeRule {
                name: "codex".into(),
                enabled: false,
                ..Default::default()
            },
            ResumeRule {
                name: "claude".into(),
                dirs: s(&["~/code"]),
                ..Default::default()
            },
            ResumeRule {
                name: "nvim".into(),
                command: s(&["nvim", "-S", "<under:~/.local/state/nvim/sessions>"]),
                ..Default::default()
            },
        ];
        let rules = resolve_resume_rules(true, &user);
        assert!(!rules.iter().any(|r| r.name == "codex"));
        let claude = rules.iter().find(|r| r.name == "claude").unwrap();
        assert_eq!(claude.dirs, s(&["~/code"]));
        assert!(!claude.command.is_empty());
        assert!(!claude.flags.is_empty());
        let nvim = rules.iter().find(|r| r.name == "nvim").unwrap();
        assert_eq!(
            nvim.command,
            s(&["nvim", "-S", "<under:~/.local/state/nvim/sessions>"])
        );
    }

    #[test]
    fn defaults_off_leaves_only_full_user_rules() {
        let user = vec![
            ResumeRule {
                name: "claude".into(),
                dirs: s(&["~/code"]),
                ..Default::default()
            },
            ResumeRule {
                name: "lazygit".into(),
                command: s(&["lazygit"]),
                ..Default::default()
            },
        ];
        let rules = resolve_resume_rules(false, &user);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "lazygit");
    }
}
