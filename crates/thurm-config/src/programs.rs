//! Programs other than agents that panes show with their own icon and name (Neovim, Node,
//! htop) when they run in the foreground: most by what kind of program they are, a few with
//! their own mark.

mod icons;

use serde::{Deserialize, Serialize};
use thurm_proto::ProgramMatch;

/// Icons drawn as a kind of program; the apps pick the symbol (`SF Symbols` on macOS).
pub const CATEGORIES: &[&str] = &[
    "editor",
    "runtime",
    "git",
    "build",
    "container",
    "cluster",
    "database",
    "monitor",
    "multiplexer",
    "remote",
    "files",
    "docs",
];

/// Sofka's cat, drawn after its app icon: ears, face, and the eyes cut out.
const SOFKA_PATH: &str = "M0 0.75L6.333 7.083L17.667 7.083L24 0.75L24 21.917C24 22.65 23.4 23.25 22.667 23.25L1.333 23.25C0.6 23.25 0 22.65 0 21.917ZM9.5 14.583C9.5 13.34 8.493 12.333 7.25 12.333C6.007 12.333 5 13.34 5 14.583C5 15.826 6.007 16.833 7.25 16.833C8.493 16.833 9.5 15.826 9.5 14.583ZM18.833 14.583C18.833 13.34 17.826 12.333 16.583 12.333C15.34 12.333 14.333 13.34 14.333 14.583C14.333 15.826 15.34 16.833 16.583 16.833C17.826 16.833 18.833 15.826 18.833 14.583Z";

/// Programs with their own mark: 24x24 absolute M/L/C/Z path data.
pub fn marks() -> impl Iterator<Item = (&'static str, &'static str)> {
    icons::ICONS.iter().copied().chain([("sofka", SOFKA_PATH)])
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ProgramDef {
    /// Stable identifier ("neovim").
    pub id: String,
    /// Display name ("Neovim"), which replaces the process name in pane titles.
    pub name: String,
    /// Executable basenames of the foreground process; a trailing `*` matches any suffix
    /// (`python*` for `python3.12`).
    pub processes: Vec<String>,
    /// Substrings of an interpreter's arguments that identify the program (a `node …/vite.js`).
    pub argv: Vec<String>,
    /// A category (`CATEGORIES`), a mark (`marks`), or on macOS an SF Symbol name.
    pub icon: String,
    /// The sidebar tile's color, `#rrggbb` (default: the neutral tile).
    pub color: Option<String>,
    /// The color of the icon on the tile (default: white).
    pub ink: Option<String>,
}

impl ProgramDef {
    fn runs(&self, executable: &str) -> bool {
        self.processes.iter().any(|p| process_matches(p, executable))
    }

    /// Whether an interpreter's arguments name this program (`node …/vite/bin/vite.js`).
    fn interpreted_by(&self, argv: &[String]) -> bool {
        argv.iter()
            .skip(1)
            .any(|a| self.argv.iter().any(|needle| a.contains(needle.as_str())))
    }

    pub fn to_match(&self) -> ProgramMatch {
        ProgramMatch {
            id: self.id.clone(),
            name: self.name.clone(),
            icon: self.icon.clone(),
            color: self.color.as_deref().and_then(crate::parse_color),
            ink: self.ink.as_deref().and_then(crate::parse_color),
        }
    }
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn process_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name
            .get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix)),
        None => pattern.eq_ignore_ascii_case(name),
    }
}

/// The definition for the foreground process. `name` is its executable, resolved through
/// symlinks (a `vim` linked to Neovim is Neovim): what it was started as (argv[0]) does not
/// decide. An interpreter is the program its arguments name, if any.
pub fn identify<'a>(defs: &'a [ProgramDef], name: &str, argv: &[String]) -> Option<&'a ProgramDef> {
    let executable = executable_name(name);
    let interpreted = is_interpreter(executable)
        .then(|| defs.iter().find(|d| d.interpreted_by(argv)))
        .flatten();
    interpreted.or_else(|| defs.iter().find(|d| d.runs(executable)))
}

/// The program a path or process name runs: its basename, without a login shell's `-` or
/// the `.…-wrapped` Nix's wrappers give it (`.nvim-wrapped` is `nvim`).
pub fn executable_name(name: &str) -> &str {
    unwrapped(basename(name.trim_start_matches('-')))
}

fn unwrapped(name: &str) -> &str {
    name.strip_prefix('.')
        .and_then(|n| n.strip_suffix("-wrapped"))
        .unwrap_or(name)
}

fn is_interpreter(executable: &str) -> bool {
    matches!(executable, "node" | "bun" | "deno" | "ruby" | "uv" | "npx")
        || process_matches("python*", executable)
}

fn def(id: &str, name: &str, processes: &[&str], icon: &str) -> ProgramDef {
    ProgramDef {
        id: id.into(),
        name: name.into(),
        processes: processes.iter().map(|s| s.to_string()).collect(),
        icon: icon.into(),
        ..Default::default()
    }
}

fn mark(id: &str, name: &str, processes: &[&str], color: &str, ink: &str) -> ProgramDef {
    ProgramDef {
        color: Some(color.into()),
        ink: Some(ink.into()),
        ..def(id, name, processes, id)
    }
}

/// One entry per name: pane titles show it in place of the process name.
pub fn builtin_programs() -> Vec<ProgramDef> {
    vec![
        mark("neovim", "Neovim", &["nvim"], "#57a143", "#ffffff"),
        mark("vim", "Vim", &["vim", "vi", "gvim"], "#019733", "#ffffff"),
        mark("sofka", "Sofka", &["sofka"], "#1e1e2e", "#b4befe"),
        def("helix", "Helix", &["hx"], "editor"),
        def("emacs", "Emacs", &["emacs", "emacs-*"], "editor"),
        def("kakoune", "Kakoune", &["kak"], "editor"),
        def("nano", "nano", &["nano"], "editor"),
        def("micro", "micro", &["micro"], "editor"),
        def("lazygit", "Git Changes", &["lazygit", "gitui"], "git"),
        def("tig", "tig", &["tig"], "git"),
        def("git", "Git", &["git"], "git"),
        def("node", "Node", &["node"], "runtime"),
        def("bun", "Bun", &["bun"], "runtime"),
        def("deno", "Deno", &["deno"], "runtime"),
        def("python", "Python", &["python*"], "runtime"),
        def("ipython", "IPython", &["ipython*"], "runtime"),
        def("uv", "uv", &["uv", "uvx"], "runtime"),
        def("ruby", "Ruby", &["ruby"], "runtime"),
        def("irb", "irb", &["irb"], "runtime"),
        def("lua", "Lua", &["lua", "luajit"], "runtime"),
        def("cargo", "Cargo", &["cargo"], "build"),
        def("bacon", "bacon", &["bacon"], "build"),
        def("go", "Go", &["go"], "build"),
        def("make", "make", &["make", "gmake"], "build"),
        def("just", "just", &["just"], "build"),
        def("docker", "Docker", &["docker"], "container"),
        def("lazydocker", "lazydocker", &["lazydocker"], "container"),
        def("podman", "Podman", &["podman"], "container"),
        def("k9s", "k9s", &["k9s"], "cluster"),
        def("kubectl", "kubectl", &["kubectl", "kubecolor"], "cluster"),
        def("helm", "Helm", &["helm"], "cluster"),
        def("psql", "psql", &["psql"], "database"),
        def("pgcli", "pgcli", &["pgcli"], "database"),
        def("mysql", "MySQL", &["mysql"], "database"),
        def("redis", "redis-cli", &["redis-cli"], "database"),
        def("sqlite", "SQLite", &["sqlite3"], "database"),
        def("mongosh", "mongosh", &["mongosh"], "database"),
        def("htop", "htop", &["htop"], "monitor"),
        def("btop", "btop", &["btop"], "monitor"),
        def("top", "top", &["top"], "monitor"),
        def("bottom", "bottom", &["btm"], "monitor"),
        def("glances", "glances", &["glances"], "monitor"),
        def("tmux", "tmux", &["tmux"], "multiplexer"),
        def("zellij", "Zellij", &["zellij"], "multiplexer"),
        def("screen", "screen", &["screen"], "multiplexer"),
        def("ssh", "SSH", &["ssh"], "remote"),
        def("mosh", "mosh", &["mosh", "mosh-client"], "remote"),
        def("yazi", "Yazi", &["yazi"], "files"),
        def("ranger", "ranger", &["ranger"], "files"),
        def("lf", "lf", &["lf"], "files"),
        def("nnn", "nnn", &["nnn"], "files"),
        def("man", "man", &["man"], "docs"),
        def("less", "less", &["less"], "docs"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn identifies_by_process_and_argv0() {
        let defs = builtin_programs();
        let id = |name: &str, a: &[&str]| identify(&defs, name, &argv(a)).map(|d| d.id.as_str());
        assert_eq!(id("nvim", &["nvim", "package.json"]), Some("neovim"));
        assert_eq!(id("/opt/homebrew/bin/nvim", &[]), Some("neovim"));
        assert_eq!(id("python3.12", &["python3.12", "manage.py"]), Some("python"));
        assert_eq!(id("Python", &["/usr/bin/python3"]), Some("python"));
        assert_eq!(id("lazygit", &["lazygit"]), Some("lazygit"));
        assert_eq!(id("-fish", &["-fish"]), None);
        assert_eq!(id("cat", &["cat", "notes.txt"]), None);
        // "go" is a whole name, not a prefix.
        assert_eq!(id("gopls", &["gopls"]), None);
    }

    #[test]
    fn the_executable_decides_over_what_it_was_started_as() {
        let defs = builtin_programs();
        let id = |name: &str, a: &[&str]| identify(&defs, name, &argv(a)).map(|d| d.id.as_str());
        // `vim` and `vi` linked to Neovim, and Nix's wrapper around it.
        assert_eq!(id("nvim", &["vim", "notes.md"]), Some("neovim"));
        assert_eq!(id("/nix/store/x-neovim/bin/nvim", &["/home/u/.nix-profile/bin/vi"]), Some("neovim"));
        assert_eq!(id(".nvim-wrapped", &["nvim"]), Some("neovim"));
        assert_eq!(id("vim", &["vim"]), Some("vim"));
        // Started under a known name, it still is the executable it runs (`exec -a nvim sleep`).
        assert_eq!(id("sleep", &["nvim", "5"]), None);
    }

    #[test]
    fn sofka_eyes_wind_against_the_face() {
        // Nonzero filling cuts a subpath out only when it winds the other way.
        let areas: Vec<f64> = SOFKA_PATH
            .split('Z')
            .filter(|p| !p.is_empty())
            .map(|sub| {
                let n: Vec<f64> = sub
                    .split(|c: char| c.is_ascii_alphabetic() || c == ' ')
                    .filter_map(|v| v.parse().ok())
                    .collect();
                let pts: Vec<(f64, f64)> = n.chunks(2).map(|c| (c[0], c[1])).collect();
                let next = pts.iter().cycle().skip(1);
                pts.iter().zip(next).map(|(a, b)| a.0 * b.1 - b.0 * a.1).sum::<f64>() / 2.0
            })
            .collect();
        assert_eq!(areas.len(), 3);
        assert!(areas[0] > 0.0 && areas[1] < 0.0 && areas[2] < 0.0, "{areas:?}");
    }

    #[test]
    fn argv_substrings_match_interpreted_programs() {
        let mut defs = vec![ProgramDef {
            id: "vite".into(),
            argv: argv(&["/vite/bin/"]),
            ..Default::default()
        }];
        defs.extend(builtin_programs());
        let id = |name: &str, a: &[&str]| identify(&defs, name, &argv(a)).map(|d| d.id.as_str());
        assert_eq!(id("node", &["node", "/app/node_modules/vite/bin/vite.js"]), Some("vite"));
        assert_eq!(id("node", &["node", "server.js"]), Some("node"));
        // Only interpreters: an editor opening that file is still the editor.
        assert_eq!(id("nvim", &["nvim", "/app/node_modules/vite/bin/vite.js"]), Some("neovim"));
    }

    #[test]
    fn every_builtin_has_a_known_icon() {
        for d in builtin_programs() {
            let mark = marks().any(|(id, _)| id == d.icon);
            assert!(mark || CATEGORIES.contains(&d.icon.as_str()), "{}", d.id);
            // Marks sit on their own color; categories on the neutral tile.
            assert_eq!(d.to_match().color.is_some(), mark, "{}", d.id);
        }
    }

    #[test]
    fn non_ascii_names_do_not_panic_prefix_patterns() {
        assert!(!process_matches("python*", "pyé"));
        assert!(process_matches("python*", "Python3"));
    }
}
