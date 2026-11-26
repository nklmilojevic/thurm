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
    /// Flags among these whose value names a file (Claude's `--settings`): a relative path is
    /// only the same file in the directory the program was launched in, which cannot be
    /// known, so a report with one is not trusted; an absolute path, exactly as given, is.
    pub path_flags: Vec<String>,
    /// Those of `path_flags` that also take the content itself as inline JSON (an object or an
    /// array), which means the same wherever the program starts.
    pub json_flags: Vec<String>,
    /// Require the owning OS process to match `command[0]` (default: `true`).
    pub match_process: bool,
}

impl ResumeRule {
    /// All flags referenced by either `flags` or `preserve_flags`. A flag in both takes its
    /// `preserve_flags` arity: a looser `flags` entry must not shorten a restriction (a `One`
    /// entry would check only the first value of a preserved `Many` list).
    pub fn all_checked_flags(&self) -> BTreeMap<String, FlagArity> {
        let mut all = self.flags.clone();
        for (k, &v) in &self.preserve_flags {
            all.insert(k.clone(), v);
        }
        all
    }

    /// A flag listed in both `flags` and `preserve_flags` with different arities: the rule is
    /// ambiguous about how many values the restriction holds, so it is not used.
    pub fn conflicting_flag(&self) -> Option<&str> {
        self.preserve_flags
            .iter()
            .find(|(k, v)| self.flags.get(*k).is_some_and(|f| f != *v))
            .map(|(k, _)| k.as_str())
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
            path_flags: Vec::new(),
            json_flags: Vec::new(),
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
            // Launch options that restrict what Claude may do: a resume must carry each over.
            preserve_flags: flags(&[
                ("--disallowed-tools", Many),
                ("--disallowedTools", Many),
                ("--mcp-config", Many),
                ("--permission-mode", One),
                ("--restricted", None),
                ("--setting-sources", One),
                ("--settings", One),
                ("--strict-mcp-config", None),
                ("--tools", Many),
            ]),
            path_flags: s(&["--mcp-config", "--settings"]),
            json_flags: s(&["--mcp-config", "--settings"]),
            match_process: true,
        },
        ResumeRule {
            name: "codex".into(),
            enabled: true,
            command: s(&["codex", "resume", "<id>"]),
            dirs: Vec::new(),
            flags: flags(&[
                ("--dangerously-bypass-approvals-and-sandbox", None),
                ("--full-auto", None),
                ("--model", One),
                ("-m", One),
            ]),
            // Sandbox and approval policy, given directly, through a config override or a
            // profile: a resume must carry each over (short forms too, `-sread-only` included).
            preserve_flags: flags(&[
                ("--ask-for-approval", One),
                ("--config", One),
                ("--profile", One),
                ("--sandbox", One),
                ("-a", One),
                ("-c", One),
                ("-p", One),
                ("-s", One),
            ]),
            path_flags: Vec::new(),
            json_flags: Vec::new(),
            match_process: true,
        },
        ResumeRule {
            name: "opencode".into(),
            enabled: true,
            command: s(&["opencode", "<oneof:--session|-s>", "<id>"]),
            dirs: Vec::new(),
            flags: BTreeMap::new(),
            preserve_flags: BTreeMap::new(),
            path_flags: Vec::new(),
            json_flags: Vec::new(),
            match_process: true,
        },
        ResumeRule {
            name: "pi".into(),
            enabled: true,
            command: s(&["pi", "--session", "<id>"]),
            dirs: Vec::new(),
            flags: BTreeMap::new(),
            preserve_flags: BTreeMap::new(),
            path_flags: Vec::new(),
            json_flags: Vec::new(),
            match_process: true,
        },
        ResumeRule {
            name: "agy".into(),
            enabled: true,
            command: s(&["agy", "--conversation", "<id>"]),
            dirs: Vec::new(),
            flags: BTreeMap::new(),
            preserve_flags: BTreeMap::new(),
            path_flags: Vec::new(),
            json_flags: Vec::new(),
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
fn merge_resume_rules(include_defaults: bool, user: &[ResumeRule]) -> Vec<ResumeRule> {
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
                // More flags that name a file: only ever more restrictions. Inline JSON stays
                // as the built-in allows it.
                for flag in &u.path_flags {
                    if !existing.path_flags.contains(flag) {
                        existing.path_flags.push(flag.clone());
                    }
                }
            } else {
                let mut replacement = u.clone();
                if replacement.command.is_empty() {
                    replacement.command = rules[idx].command.clone();
                }
                if replacement.preserve_flags.is_empty() {
                    replacement.preserve_flags = rules[idx].preserve_flags.clone();
                }
                // Never fewer: what names a file in the built-in still does. Inline JSON only
                // where the built-in takes it (an override can't widen it for those flags).
                for flag in &rules[idx].path_flags {
                    if !replacement.path_flags.contains(flag) {
                        replacement.path_flags.push(flag.clone());
                    }
                }
                let builtin_json = rules[idx].json_flags.clone();
                let builtin_paths = rules[idx].path_flags.clone();
                replacement
                    .json_flags
                    .retain(|f| !builtin_paths.contains(f) || builtin_json.contains(f));
                for flag in builtin_json {
                    if !replacement.json_flags.contains(&flag) {
                        replacement.json_flags.push(flag);
                    }
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

/// The trust rules in effect: the built-ins (if `include_defaults`) with the user's overrides
/// and additions. A merged rule giving a flag two arities is left out (see
/// [`resume_rule_warnings`]): checked after merging, so an override's `flags` that disagree
/// with an inherited `preserve_flags` count too.
pub fn resolve_resume_rules(include_defaults: bool, user: &[ResumeRule]) -> Vec<ResumeRule> {
    let mut rules = merge_resume_rules(include_defaults, user);
    rules.retain(|r| r.conflicting_flag().is_none());
    rules
}

/// One warning per rule [`resolve_resume_rules`] leaves out because a flag has two arities.
pub fn resume_rule_warnings(include_defaults: bool, user: &[ResumeRule]) -> Vec<String> {
    merge_resume_rules(include_defaults, user)
        .iter()
        .filter_map(|r| {
            r.conflicting_flag().map(|flag| {
                format!(
                    "resume rule {:?}: flag {flag:?} has different arities in flags and \
                     preserve_flags; rule ignored",
                    r.name
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An override that replaces a built-in rule keeps the built-in's file flags: a relative
    /// path in them stays untrusted.
    #[test]
    fn an_override_keeps_the_builtin_file_flags() {
        let user = ResumeRule {
            name: "claude".into(),
            flags: BTreeMap::from([("--model".to_owned(), FlagArity::One)]),
            ..Default::default()
        };
        let rules = merge_resume_rules(true, std::slice::from_ref(&user));
        let claude = rules.iter().find(|r| r.name == "claude").unwrap();
        assert!(claude.path_flags.iter().any(|f| f == "--settings"));
        assert!(claude.path_flags.iter().any(|f| f == "--mcp-config"));
        // Inline JSON stays as the built-in takes it.
        assert!(claude.json_flags.iter().any(|f| f == "--settings"));

        // An override that only adds a file flag adds it, keeping the rest of the built-in.
        let more = ResumeRule {
            name: "claude".into(),
            path_flags: vec!["--plugin-dir".into()],
            ..Default::default()
        };
        let rules = merge_resume_rules(true, std::slice::from_ref(&more));
        let claude = rules.iter().find(|r| r.name == "claude").unwrap();
        assert!(claude.path_flags.iter().any(|f| f == "--plugin-dir"));
        assert!(claude.path_flags.iter().any(|f| f == "--settings"));
        assert!(!claude.flags.is_empty() && !claude.preserve_flags.is_empty());
        assert!(!claude.json_flags.iter().any(|f| f == "--plugin-dir"));
    }

    #[test]
    fn a_flag_with_two_arities_drops_the_rule() {
        let mut both = ResumeRule {
            name: "tool".into(),
            command: vec!["tool".into()],
            ..Default::default()
        };
        both.flags.insert("--deny".into(), FlagArity::One);
        both.preserve_flags.insert("--deny".into(), FlagArity::Many);
        assert_eq!(both.conflicting_flag(), Some("--deny"));
        assert!(resolve_resume_rules(false, std::slice::from_ref(&both)).is_empty());
        let w = resume_rule_warnings(false, std::slice::from_ref(&both));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("--deny"));

        // The same arity in both maps is fine.
        both.flags.insert("--deny".into(), FlagArity::Many);
        assert_eq!(
            resolve_resume_rules(false, std::slice::from_ref(&both)).len(),
            1
        );
        assert!(resume_rule_warnings(false, std::slice::from_ref(&both)).is_empty());

        // No built-in rule is ambiguous.
        let builtin = builtin_resume_rules();
        assert!(builtin.iter().all(|r| r.conflicting_flag().is_none()));
        // An override's `flags` against a built-in's inherited `preserve_flags`: Claude keeps
        // `--tools` (`Many`), and an override giving it `One` drops the rule.
        let claude = builtin.iter().find(|r| r.name == "claude").unwrap();
        assert_eq!(claude.preserve_flags.get("--tools"), Some(&FlagArity::Many));
        let mut over = ResumeRule {
            name: "claude".into(),
            ..Default::default()
        };
        over.flags.insert("--tools".into(), FlagArity::One);
        assert!(
            !resolve_resume_rules(true, std::slice::from_ref(&over))
                .iter()
                .any(|r| r.name == "claude")
        );
        let w = resume_rule_warnings(true, &[over]);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("claude") && w[0].contains("--tools"), "{w:?}");
    }

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
    fn preserved_arity_wins_over_flags() {
        let rule = ResumeRule {
            command: s(&["x"]),
            flags: flags(&[("--deny", FlagArity::One), ("--model", FlagArity::One)]),
            preserve_flags: flags(&[("--deny", FlagArity::Many)]),
            ..Default::default()
        };
        let all = rule.all_checked_flags();
        assert_eq!(all["--deny"], FlagArity::Many);
        assert_eq!(all["--model"], FlagArity::One);
    }

    #[test]
    fn builtin_rules_preserve_launch_restrictions() {
        let rules = builtin_resume_rules();
        let claude = rules.iter().find(|r| r.name == "claude").unwrap();
        for f in [
            "--tools",
            "--permission-mode",
            "--restricted",
            "--settings",
            "--setting-sources",
        ] {
            assert!(claude.preserve_flags.contains_key(f), "{f}");
        }
        let codex = rules.iter().find(|r| r.name == "codex").unwrap();
        for f in [
            "--sandbox",
            "-s",
            "--ask-for-approval",
            "-a",
            "-c",
            "--config",
        ] {
            assert!(codex.preserve_flags.contains_key(f), "{f}");
        }
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
