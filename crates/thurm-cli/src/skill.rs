//! `thurm skill`: the guide that teaches coding agents to drive Thurm. Agents get a stub skill
//! that runs `thurm skill show`, so the guide always matches the `thurm` they run, here or on a
//! remote host.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use thurm_config::hooks;
use thurm_proto::PaneId;

/// What `thurm skill show` prints.
pub const GUIDE: &str = include_str!("skill.md");
/// The skill installed into agents (and bundled with the app).
pub const STUB: &str = include_str!("../../../skills/thurm/SKILL.md");

#[derive(Subcommand)]
pub enum SkillCmd {
    /// Print the guide the skill gives agents.
    Show,
    /// Install the skill for every agent found: Claude Code (~/.claude/skills), and Codex,
    /// OpenCode and Pi (~/.agents/skills).
    Setup {
        /// Only this agent (claude, agents).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Remove the skill from agents.
    Remove {
        /// Only this agent (claude, agents).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Where the skill is installed, and whether it is current.
    Status {
        /// Only this agent (claude, agents).
        #[arg(long)]
        agent: Option<String>,
    },
}

/// A skills directory an agent reads.
struct Target {
    kind: &'static str,
    name: &'static str,
    /// The skill's directory (`…/skills/thurm`).
    path: PathBuf,
    /// The agent is installed (its config directory exists).
    present: bool,
}

fn targets(home: &Path, claude_dir: Option<PathBuf>) -> Vec<Target> {
    let claude = claude_dir.unwrap_or_else(|| home.join(".claude"));
    let agents = [".agents", ".codex", ".config/opencode", ".pi"];
    vec![
        Target {
            kind: "claude",
            name: "Claude Code",
            path: claude.join("skills/thurm"),
            present: claude.is_dir(),
        },
        Target {
            kind: "agents",
            name: "Codex, OpenCode, Pi",
            path: home.join(".agents/skills/thurm"),
            present: agents.iter().any(|d| home.join(d).is_dir()),
        },
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Missing,
    Current,
    /// Thurm's, from an older version.
    Outdated,
    /// Something else by that name; left alone.
    Other,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Missing => "missing",
            State::Current => "current",
            State::Outdated => "outdated",
            State::Other => "other",
        }
    }
}

/// A link into an app bundle's skill, as Thurm > Integrations made before `thurm skill`.
fn app_link(path: &Path) -> bool {
    std::fs::read_link(path).is_ok_and(|t| {
        t.to_string_lossy()
            .contains(".app/Contents/Resources/skills/thurm")
    })
}

fn state(path: &Path) -> State {
    if std::fs::symlink_metadata(path).is_err() {
        return State::Missing;
    }
    match std::fs::read_to_string(path.join("SKILL.md")) {
        Ok(text) if text == STUB => State::Current,
        Ok(text) if text.contains("thurm-stub:") => State::Outdated,
        _ if app_link(path) => State::Outdated,
        _ => State::Other,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Change {
    Unchanged,
    Changed,
}

fn install(path: &Path) -> Result<Change, String> {
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    match state(path) {
        State::Current => return Ok(Change::Unchanged),
        State::Other => {
            return Err(format!(
                "{} exists and isn't Thurm's; not replacing it",
                path.display()
            ));
        }
        State::Outdated if app_link(path) => std::fs::remove_file(path).map_err(io)?,
        State::Outdated | State::Missing => {}
    }
    std::fs::create_dir_all(path).map_err(io)?;
    std::fs::write(path.join("SKILL.md"), STUB).map_err(io)?;
    Ok(Change::Changed)
}

fn remove(path: &Path) -> Result<Change, String> {
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    match state(path) {
        State::Missing => Ok(Change::Unchanged),
        State::Other => Err(format!("{} isn't Thurm's; not removing it", path.display())),
        State::Current | State::Outdated if std::fs::read_link(path).is_ok() => {
            std::fs::remove_file(path).map_err(io)?;
            Ok(Change::Changed)
        }
        State::Current | State::Outdated => {
            std::fs::remove_file(path.join("SKILL.md")).map_err(io)?;
            // Only when nothing else was put there.
            let _ = std::fs::remove_dir(path);
            Ok(Change::Changed)
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn run(cmd: SkillCmd, json: bool) -> crate::R {
    let (action, only) = match cmd {
        SkillCmd::Show => {
            print!("{GUIDE}");
            return Ok(std::process::ExitCode::SUCCESS);
        }
        SkillCmd::Setup { agent } => ("setup", agent),
        SkillCmd::Remove { agent } => ("remove", agent),
        SkillCmd::Status { agent } => ("status", agent),
    };
    let only = only.map(|k| if k == "codex" { "agents".into() } else { k });
    let claude_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    let all = targets(&home(), claude_dir);
    let chosen: Vec<&Target> = match only.as_deref() {
        Some(k) => all.iter().filter(|t| t.kind == k).collect(),
        None => all.iter().filter(|t| t.present || json).collect(),
    };
    if only.is_some() && chosen.is_empty() {
        return Err(format!(
            "unknown agent {:?} (claude, agents)",
            only.unwrap_or_default()
        )
        .into());
    }
    if chosen.is_empty() {
        eprintln!(
            "thurm: no agent found (no ~/.claude, ~/.agents or ~/.codex); run the agent once, \
             or pass --agent claude"
        );
        return Ok(std::process::ExitCode::FAILURE);
    }
    let mut ok = true;
    if action == "status" {
        let mut report = Vec::new();
        for t in chosen {
            let s = state(&t.path);
            if json {
                report.push(serde_json::json!({
                    "agent": t.kind, "name": t.name, "state": s.name(),
                    "present": t.present, "path": t.path.display().to_string(),
                }));
            } else {
                println!("{}: {} ({})", t.name, s.name(), t.path.display());
            }
            ok &= s == State::Current;
        }
        if json {
            println!("{}", serde_json::Value::Array(report));
            return Ok(std::process::ExitCode::SUCCESS);
        }
    } else {
        for t in chosen {
            let done = if action == "setup" {
                install(&t.path)
            } else {
                remove(&t.path)
            };
            match done {
                Ok(Change::Unchanged) => {
                    println!("{}: nothing to change ({})", t.name, t.path.display())
                }
                Ok(Change::Changed) if action == "setup" => {
                    println!("{}: skill installed ({})", t.name, t.path.display())
                }
                Ok(Change::Changed) => println!("{}: skill removed ({})", t.name, t.path.display()),
                Err(e) => {
                    eprintln!("thurm: {}: {e}", t.name);
                    ok = false;
                }
            }
        }
        if action == "setup" && !json && hooks_missing() {
            println!(
                "Agents learn which pane they run in at session start from Thurm's hooks: \
                 `thurm hooks install`."
            );
        }
    }
    Ok(if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

/// An installed agent lacks some of Thurm's hooks.
fn hooks_missing() -> bool {
    hooks::AGENTS.iter().any(|a| {
        let path = hooks::settings_path(a);
        path.parent().is_some_and(Path::is_dir)
            && hooks::installed(a, &std::fs::read_to_string(&path).unwrap_or_default())
                < a.events.len()
    })
}

/// What an agent is told when its session starts in a Thurm pane (the SessionStart hook's
/// output, which Claude Code and Codex add to the session's context).
pub fn session_context(pane: PaneId, cli: Option<&Path>) -> String {
    let mut out = format!(
        "You're running in a Thurm terminal pane.\n\n\
         ```text\nTHURM_PANE_ID={pane}\n```\n\n\
         `thurm` commands act on this pane by default; pass `--pane ID` for another. The user \
         may be looking at a different pane. Use `thurm` to open tabs and splits, run long \
         commands in their own pane, read other panes' output, and wait for commands or other \
         agents. The thurm skill has the guide; if it isn't installed, run `thurm skill show`.\n"
    );
    if let Some(cli) = cli {
        out.push_str(&format!(
            "\n`thurm` isn't on PATH here: run it as `{}`.\n",
            cli.display()
        ));
    }
    out
}

/// This `thurm`, when a bare `thurm` wouldn't find it.
pub fn cli_off_path() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("thurm").is_file()));
    if on_path {
        return None;
    }
    std::env::current_exe().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("thurm-skill-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn setup_and_remove_round_trip() {
        let home = scratch("roundtrip");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let ts = targets(&home, None);
        assert!(ts[0].present);
        assert!(!ts[1].present);
        let path = &ts[0].path;
        assert_eq!(state(path), State::Missing);
        assert_eq!(install(path), Ok(Change::Changed));
        assert_eq!(state(path), State::Current);
        assert_eq!(install(path), Ok(Change::Unchanged));
        assert_eq!(remove(path), Ok(Change::Changed));
        assert_eq!(state(path), State::Missing);
        assert!(home.join(".claude/skills").is_dir());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn replaces_older_stubs_and_app_links_but_not_other_skills() {
        let home = scratch("replace");
        let path = home.join(".agents/skills/thurm");

        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), "metadata:\n  thurm-stub: \"0\"\n").unwrap();
        assert_eq!(state(&path), State::Outdated);
        assert_eq!(install(&path), Ok(Change::Changed));
        assert_eq!(state(&path), State::Current);
        std::fs::remove_dir_all(&path).unwrap();

        // The link Thurm > Integrations made into the app, holding the full guide.
        let app = home.join("Thurm.app/Contents/Resources/skills/thurm");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("SKILL.md"), "# Driving Thurm\n").unwrap();
        std::os::unix::fs::symlink(&app, &path).unwrap();
        assert_eq!(state(&path), State::Outdated);
        assert_eq!(install(&path), Ok(Change::Changed));
        assert!(std::fs::read_link(&path).is_err());
        assert_eq!(state(&path), State::Current);
        // The app's copy is untouched.
        assert_eq!(
            std::fs::read_to_string(app.join("SKILL.md")).unwrap(),
            "# Driving Thurm\n"
        );
        std::fs::remove_dir_all(&path).unwrap();

        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), "someone else's\n").unwrap();
        assert_eq!(state(&path), State::Other);
        assert!(install(&path).is_err());
        assert!(remove(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("SKILL.md")).unwrap(),
            "someone else's\n"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn remove_keeps_other_files_in_the_directory() {
        let home = scratch("keep");
        let path = home.join(".agents/skills/thurm");
        install(&path).unwrap();
        std::fs::write(path.join("notes.md"), "mine").unwrap();
        assert_eq!(remove(&path), Ok(Change::Changed));
        assert!(path.join("notes.md").is_file());
        assert!(!path.join("SKILL.md").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_stub_points_at_the_guide() {
        assert!(STUB.starts_with("---\nname: thurm\n"));
        assert!(STUB.contains("thurm-stub:"));
        assert!(STUB.contains("thurm skill show"));
        assert!(GUIDE.starts_with("# Driving Thurm"));
    }

    #[test]
    fn session_context_names_the_pane() {
        let text = session_context(42, None);
        assert!(text.contains("THURM_PANE_ID=42"));
        assert!(text.contains("thurm skill show"));
        assert!(!text.contains("isn't on PATH"));
        let text = session_context(42, Some(Path::new("/opt/Thurm.app/Contents/Helpers/thurm")));
        assert!(text.contains("`/opt/Thurm.app/Contents/Helpers/thurm`"));
    }
}
