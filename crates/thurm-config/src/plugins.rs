//! Finding plugins in the plugins directory.
//!
//! A plugin is one of:
//! - a directory with `plugin.toml` (its `main` Lua file, or the program `exec` runs), or with
//!   `init.lua` and no manifest;
//! - a single `NAME.lua`;
//! - a single executable file, run as a program (its name without the extension).
//!
//! Its name is the directory's or the file's: it names it in `[plugins]`, the CLI and logs.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The file describing a plugin directory.
pub const MANIFEST: &str = "plugin.toml";

/// The name `init.lua` beside the config file runs under.
pub const INIT: &str = "init";

/// The name scripts (`thurm do`) run under: no plugin takes it.
pub const SCRIPT: &str = "script";

#[derive(Clone, Debug, PartialEq)]
pub enum PluginKind {
    /// Lua, run inside the daemon: its main file.
    Lua { main: PathBuf },
    /// A program the daemon starts and talks to over its standard input and output.
    Process { exec: Vec<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct FoundPlugin {
    pub name: String,
    pub version: String,
    pub description: String,
    /// The plugin's directory, or its file when it is only one.
    pub path: PathBuf,
    /// Where it runs: the directory, or the one holding the file.
    pub dir: PathBuf,
    pub kind: PluginKind,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Manifest {
    /// Shown in `thurm plugin list`; the directory names the plugin.
    name: Option<String>,
    version: String,
    description: String,
    /// The Lua file to run, relative to the directory.
    main: Option<String>,
    /// The program to run and its arguments; a first word with a `/`, or naming a file in the
    /// directory, is relative to it.
    exec: Option<Vec<String>>,
}

/// The plugins in `dir`, sorted by name, and what was wrong with the entries that are not.
pub fn discover(dir: &Path) -> (Vec<FoundPlugin>, Vec<String>) {
    let mut found: Vec<FoundPlugin> = Vec::new();
    let mut warnings = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (found, warnings);
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if file_name.starts_with('.') || file_name.ends_with('~') {
            continue;
        }
        match read(&path) {
            Ok(Some(p)) => {
                if p.name == INIT || p.name == SCRIPT {
                    warnings.push(format!(
                        "{}: {:?} names {} already; rename it",
                        path.display(),
                        p.name,
                        if p.name == INIT {
                            "init.lua beside config.toml"
                        } else {
                            "the scripts thurm do runs"
                        }
                    ));
                } else if !thurm_proto::ext::valid_plugin_name(&p.name) {
                    warnings.push(format!(
                        "{}: a plugin's name has only letters, digits, - and _",
                        path.display()
                    ));
                } else if found.iter().any(|f| f.name == p.name) {
                    warnings.push(format!(
                        "{}: another plugin is called {:?} already",
                        path.display(),
                        p.name
                    ));
                } else {
                    found.push(p);
                }
            }
            Ok(None) => {}
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    (found, warnings)
}

/// The plugin at `path`, `None` when it is not one (a README beside the plugins).
fn read(path: &Path) -> Result<Option<FoundPlugin>, String> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        let manifest_path = path.join(MANIFEST);
        let manifest: Manifest = match std::fs::read_to_string(&manifest_path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{MANIFEST}: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Manifest::default(),
            Err(e) => return Err(format!("{MANIFEST}: {e}")),
        };
        let kind = match (&manifest.exec, &manifest.main) {
            (Some(_), Some(_)) => return Err(format!("{MANIFEST} has both main and exec")),
            (Some(exec), None) => {
                let mut exec = exec.clone();
                let Some(program) = exec.first_mut() else {
                    return Err(format!("{MANIFEST}: exec is empty"));
                };
                let local = path.join(&*program);
                if !Path::new(program.as_str()).is_absolute()
                    && (program.contains('/') || local.is_file())
                {
                    *program = local.display().to_string();
                }
                PluginKind::Process { exec }
            }
            (None, Some(main)) => PluginKind::Lua {
                main: path.join(main),
            },
            (None, None) if path.join("init.lua").is_file() => PluginKind::Lua {
                main: path.join("init.lua"),
            },
            (None, None) if manifest_path.is_file() => {
                return Err(format!(
                    "{MANIFEST} has neither main nor exec, and no init.lua"
                ));
            }
            (None, None) => return Ok(None),
        };
        if let PluginKind::Lua { main } = &kind
            && !main.is_file()
        {
            return Err(format!("{} is missing", main.display()));
        }
        let _ = manifest.name;
        return Ok(Some(FoundPlugin {
            name: file_name,
            version: manifest.version,
            description: manifest.description,
            path: path.to_owned(),
            dir: path.to_owned(),
            kind,
        }));
    }
    if !meta.is_file() {
        return Ok(None);
    }
    let dir = path.parent().unwrap_or(Path::new("/")).to_owned();
    let stem = path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if path.extension().is_some_and(|e| e == "lua") {
        return Ok(Some(FoundPlugin {
            name: stem,
            version: String::new(),
            description: String::new(),
            path: path.to_owned(),
            dir,
            kind: PluginKind::Lua {
                main: path.to_owned(),
            },
        }));
    }
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        return Ok(Some(FoundPlugin {
            name: stem,
            version: String::new(),
            description: String::new(),
            path: path.to_owned(),
            dir,
            kind: PluginKind::Process {
                exec: vec![path.display().to_string()],
            },
        }));
    }
    Ok(None)
}

/// The files of a new plugin `name`: (path in its directory, contents, executable). A Lua
/// plugin, or with `program` one that runs as a program (in Python).
pub fn scaffold(name: &str, program: bool) -> Vec<(&'static str, String, bool)> {
    let fill = |text: &str| text.replace("NAME", name);
    if program {
        vec![
            (
                MANIFEST,
                "version = \"0.1.0\"\ndescription = \"\"\nexec = [\"plugin.py\"]\n".into(),
                false,
            ),
            (
                "plugin.py",
                fill(include_str!("plugin_templates/plugin.py")),
                true,
            ),
        ]
    } else {
        vec![
            (
                MANIFEST,
                "version = \"0.1.0\"\ndescription = \"\"\nmain = \"init.lua\"\n".into(),
                false,
            ),
            (
                "init.lua",
                fill(include_str!("plugin_templates/init.lua")),
                false,
            ),
        ]
    }
}

/// lua-language-server's settings for code beside `types`, the directory with the `thurm`
/// types.
pub fn luarc(types: &Path) -> String {
    let v = serde_json::json!({
        "runtime.version": "Lua 5.4",
        "workspace.library": [types.display().to_string()],
        "diagnostics.globals": ["thurm"],
    });
    let mut out = serde_json::to_string_pretty(&v).unwrap_or_default();
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("thurm-plugins-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn every_kind_of_plugin_is_found() {
        let dir = temp("kinds");
        std::fs::create_dir(dir.join("git-tools")).unwrap();
        std::fs::write(
            dir.join("git-tools/plugin.toml"),
            "version = \"1.2.0\"\ndescription = \"Git\"\nmain = \"main.lua\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("git-tools/main.lua"), "").unwrap();
        std::fs::create_dir(dir.join("bare")).unwrap();
        std::fs::write(dir.join("bare/init.lua"), "").unwrap();
        std::fs::create_dir(dir.join("watcher")).unwrap();
        std::fs::write(
            dir.join("watcher/plugin.toml"),
            "exec = [\"run.py\", \"--x\"]\n",
        )
        .unwrap();
        executable(&dir.join("watcher/run.py"));
        std::fs::write(dir.join("single.lua"), "").unwrap();
        executable(&dir.join("tool.sh"));
        std::fs::write(dir.join("README.md"), "").unwrap();
        std::fs::create_dir(dir.join("empty")).unwrap();
        std::fs::create_dir(dir.join(".git")).unwrap();

        let (found, warnings) = discover(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["bare", "git-tools", "single", "tool", "watcher"]);
        assert_eq!(found[1].version, "1.2.0");
        assert_eq!(
            found[1].kind,
            PluginKind::Lua {
                main: dir.join("git-tools/main.lua")
            }
        );
        assert_eq!(
            found[4].kind,
            PluginKind::Process {
                exec: vec![
                    dir.join("watcher/run.py").display().to_string(),
                    "--x".into()
                ]
            }
        );
        assert_eq!(found[3].dir, dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn broken_entries_are_reported_and_skipped() {
        let dir = temp("broken");
        std::fs::create_dir(dir.join("both")).unwrap();
        std::fs::write(
            dir.join("both/plugin.toml"),
            "main = \"a.lua\"\nexec = [\"x\"]\n",
        )
        .unwrap();
        std::fs::create_dir(dir.join("typo")).unwrap();
        std::fs::write(dir.join("typo/plugin.toml"), "mian = \"a.lua\"\n").unwrap();
        std::fs::create_dir(dir.join("missing")).unwrap();
        std::fs::write(dir.join("missing/plugin.toml"), "main = \"gone.lua\"\n").unwrap();
        std::fs::write(dir.join("init.lua"), "").unwrap();
        std::fs::write(dir.join("script.lua"), "").unwrap();
        std::fs::write(dir.join("bad name.lua"), "").unwrap();
        std::fs::write(dir.join("dup.lua"), "").unwrap();
        std::fs::create_dir(dir.join("dup")).unwrap();
        std::fs::write(dir.join("dup/init.lua"), "").unwrap();
        let (found, warnings) = discover(&dir);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(warnings.len(), 7, "{warnings:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
