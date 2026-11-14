//! Commands over the extension API: `thurm action`.

use std::process::ExitCode;

use clap::Subcommand;
use serde_json::{Value, json};
use thurm_client::Client;
use thurm_proto::ext::{self, method};
use thurm_proto::{ENV_PANE_ID, PaneId, Request, Response};

type R = Result<ExitCode, Box<dyn std::error::Error>>;

#[derive(Subcommand)]
pub enum ActionCmd {
    /// List the actions (the config's commands, what plugins and the app provide) and their keys.
    List,
    /// Run an action, for a pane (default: this one, else the focused one), and print what it
    /// returned. ARGS is a JSON value: `thurm action run git.checkout '{"branch":"main"}'`.
    Run {
        name: String,
        args: Option<String>,
        #[arg(short, long)]
        pane: Option<PaneId>,
    },
}

/// Calls `method` of the daemon's extension API.
pub fn call(c: &Client, method: &str, params: Value) -> Result<Value, Box<dyn std::error::Error>> {
    match c.request(Request::Ext {
        method: method.into(),
        params: params.to_string(),
    })? {
        Response::Text(t) => Ok(serde_json::from_str(&t)?),
        other => Err(format!("unexpected response {other:?}").into()),
    }
}

pub fn action(c: &Client, cmd: ActionCmd, json: bool) -> R {
    match cmd {
        ActionCmd::List => {
            let list: ext::Actions =
                serde_json::from_value(call(c, method::ACTIONS_LIST, Value::Null)?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(ExitCode::SUCCESS);
            }
            let keys = |name: &str| {
                list.bindings
                    .iter()
                    .filter(|b| b.action == name)
                    .map(|b| b.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let width = list
                .actions
                .iter()
                .map(|a| a.name.chars().count())
                .max()
                .unwrap_or(0)
                .clamp(4, 40);
            println!("{:<width$}  {:<16} {:<14} TITLE", "NAME", "SOURCE", "KEY");
            for a in &list.actions {
                println!(
                    "{:<width$}  {:<16} {:<14} {}",
                    a.name,
                    a.source,
                    keys(&a.name),
                    a.title()
                );
            }
        }
        ActionCmd::Run { name, args, pane } => {
            let args: Value = match args.as_deref() {
                None => Value::Null,
                // A bare word is a string, as `thurm set` takes them.
                Some(a) => serde_json::from_str(a).unwrap_or_else(|_| Value::String(a.into())),
            };
            let pane = pane.or_else(|| std::env::var(ENV_PANE_ID).ok()?.parse().ok());
            let result = call(
                c,
                method::ACTIONS_RUN,
                json!({ "name": name, "args": args, "pane": pane }),
            )?;
            match result {
                Value::Null => {}
                Value::String(s) if !json => println!("{s}"),
                other => println!("{}", serde_json::to_string_pretty(&other)?),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The Lua files under `dir` (a few levels deep), by path relative to it, for a script run on
/// another host to `require`. Only regular files really in the directory go: every name is
/// opened relative to its opened parent without following a symlink, so one swapped in while
/// the directory is read cannot send a file from elsewhere. `dir` is canonical: its path is
/// opened component by component the same way. Bounded: what fits in 4 MiB, from the first
/// 10000 names the walk reads (a huge directory is not read to its end).
fn lua_modules(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    const MAX_FILES: usize = 200;
    const MAX_BYTES: usize = 4 << 20;
    const MAX_NAMES: usize = 10_000;
    let mut out = std::collections::BTreeMap::new();
    let Ok(root) = open_dir_path(dir) else {
        return out;
    };
    let mut bytes = 0;
    let mut names_left = MAX_NAMES;
    let mut stack = vec![(root, String::new(), 0)];
    while let Some((fd, prefix, depth)) = stack.pop() {
        if names_left == 0 {
            break;
        }
        let names = dir_names(&fd, names_left);
        names_left -= names.len();
        for name in names {
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let rel = format!("{prefix}{}", name.to_string_lossy());
            let is_lua = rel.ends_with(".lua");
            // A directory, or a file: nothing else, and never through a symlink.
            let Ok(entry) = open_nofollow(Some(&fd), &name, false) else {
                continue;
            };
            let entry = std::fs::File::from(entry);
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                if depth < 3 {
                    stack.push((entry.into(), format!("{rel}/"), depth + 1));
                }
                continue;
            }
            if !meta.is_file() || !is_lua {
                continue;
            }
            if out.len() >= MAX_FILES {
                return out;
            }
            // Within the budget, read no more than it allows; a larger file is left out.
            let room = MAX_BYTES - bytes;
            if meta.len() > room as u64 {
                continue;
            }
            let mut text = String::new();
            let read = std::io::Read::read_to_string(
                &mut std::io::Read::take(entry, room as u64 + 1),
                &mut text,
            );
            if read.is_err() || text.len() > room {
                continue;
            }
            bytes += text.len();
            out.insert(rel, text);
        }
    }
    out
}

/// Opens `name` (relative to the directory `at`, or as a path) without following a symlink in
/// its last part; `dir`: only a directory.
fn open_nofollow(
    at: Option<&std::os::fd::OwnedFd>,
    name: &std::ffi::OsStr,
    dir: bool,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(name.as_bytes())?;
    let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    if dir {
        flags |= libc::O_DIRECTORY;
    }
    let base = at.map_or(libc::AT_FDCWD, |f| f.as_raw_fd());
    let fd = unsafe { libc::openat(base, c.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    // O_NONBLOCK only kept a FIFO from blocking the open: reads of a regular file don't care.
    Ok(fd)
}

/// Opens the directory at absolute `path` one component at a time from `/`, none followed
/// through a symlink: a component swapped for one fails the walk.
fn open_dir_path(path: &std::path::Path) -> std::io::Result<std::os::fd::OwnedFd> {
    if !path.is_absolute() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let mut fd = open_nofollow(None, std::ffi::OsStr::new("/"), true)?;
    for part in path.components() {
        match part {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => fd = open_nofollow(Some(&fd), name, true)?,
            // A canonical path has no `.` or `..`.
            _ => return Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }
    Ok(fd)
}

/// The names in the opened directory `fd`, sorted: no more than `limit` of them.
fn dir_names(fd: &std::os::fd::OwnedFd, limit: usize) -> Vec<std::ffi::OsString> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let mut names = Vec::new();
    let dup = unsafe { libc::dup(fd.as_raw_fd()) };
    if dup < 0 {
        return names;
    }
    let d = unsafe { libc::fdopendir(dup) };
    if d.is_null() {
        unsafe { libc::close(dup) };
        return names;
    }
    // The copy shares the read position with `fd`: start from the top.
    unsafe { libc::rewinddir(d) };
    while names.len() < limit {
        let e = unsafe { libc::readdir(d) };
        if e.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*e).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(std::ffi::OsStr::from_bytes(bytes).to_owned());
        }
    }
    unsafe { libc::closedir(d) };
    names.sort();
    names
}

/// `thurm do FILE [ARGS]`: runs a Lua script in the daemon, printing what it logs.
/// `with_modules`: with `--remote`, send the `.lua` files beside the script for `require`
/// (only when asked: they may be anything the directory holds).
pub fn script(
    remote: Option<&str>,
    file: &str,
    args: Vec<String>,
    with_modules: bool,
    json: bool,
) -> R {
    use std::io::Read;
    let (source, name, dir) = if file == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        (s, "stdin".to_owned(), None)
    } else {
        let path = std::path::Path::new(file);
        let source =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {file}: {e}"))?;
        let dir = path
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(|d| d.display().to_string()));
        (source, file.to_owned(), dir)
    };
    let socket = match remote {
        Some(name) => {
            drop(crate::remote::connect_remote(name)?);
            thurm_config::remote_socket_path(name)
        }
        None => {
            // Starts the daemon when it is not running.
            drop(crate::connect(true)?);
            thurm_config::socket_path()
        }
    };
    let stream = thurm_client::connect_daemon(&socket)
        .map_err(|e| format!("cannot connect to {}: {e}", socket.display()))?;
    let client = Client::from_stream(
        stream,
        "thurm-do",
        false,
        &[],
        move |event| {
            let thurm_proto::Event::Ext { name, payload } = event else {
                return;
            };
            if name != ext::notice::SCRIPT_LOG {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(&payload) else {
                return;
            };
            let message = v["message"].as_str().unwrap_or_default();
            if json {
                println!("{}", json!({ "log": v["level"], "message": message }));
            } else if v["level"] == "info" {
                println!("{message}");
            } else {
                eprintln!("{message}");
            }
        },
        || {},
    )?;
    let pane: Option<PaneId> = std::env::var(ENV_PANE_ID).ok().and_then(|v| v.parse().ok());
    // On another host the script's directory is not there: its modules go along.
    let (dir, modules) = match (remote, dir) {
        (Some(_), Some(d)) if with_modules => (None, lua_modules(std::path::Path::new(&d))),
        (Some(_), _) => (None, Default::default()),
        (_, dir) => (dir, Default::default()),
    };
    let run = ext::RunScript {
        source,
        name,
        args,
        dir,
        modules,
        pane: if remote.is_some() { None } else { pane },
    };
    let result = call(&client, method::SCRIPT_RUN, serde_json::to_value(run)?)?;
    match result {
        Value::Null => {}
        Value::String(s) if !json => println!("{s}"),
        other if json => println!("{}", json!({ "result": other })),
        other => println!("{}", serde_json::to_string_pretty(&other)?),
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Subcommand)]
pub enum PluginCmd {
    /// List plugins: init.lua and those in the plugins directory, with their state.
    List,
    /// Make a new plugin in the plugins directory, ready to edit: Lua, or with --program one
    /// that runs as a program (a Python example).
    New {
        name: String,
        #[arg(long)]
        program: bool,
    },
    /// Install a plugin from a git repository into the plugins directory. A plugin runs with
    /// your rights: install only what you trust.
    Install {
        url: String,
        /// The plugin's name (default: the repository's).
        #[arg(long)]
        name: Option<String>,
    },
    /// Update installed plugins from their repositories (git pull), or only NAME.
    Update { name: Option<String> },
    /// Delete a plugin's files.
    Remove {
        name: String,
        #[arg(short, long)]
        yes: bool,
    },
    /// Start a disabled plugin, and keep it out of `plugins.disabled`.
    Enable { name: String },
    /// Stop a plugin, and add it to `plugins.disabled`.
    Disable { name: String },
    /// Start plugins again, picking up changes to their files (all, or only NAME).
    Reload { name: Option<String> },
    /// Print the end of a plugin's log: what it logged and wrote to stderr, and its errors.
    Logs {
        name: String,
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
    },
    /// Write the `thurm` API's types for lua-language-server, and a .luarc.json for init.lua,
    /// into the config directory.
    Types,
}

impl PluginCmd {
    /// Works on this machine's files, not through a daemon.
    pub fn local(&self) -> bool {
        matches!(
            self,
            PluginCmd::New { .. }
                | PluginCmd::Install { .. }
                | PluginCmd::Update { .. }
                | PluginCmd::Remove { .. }
                | PluginCmd::Types
        )
    }
}

const LUA_TYPES: &str = include_str!("../../thurm-lua/types/thurm.lua");

/// Writes the `thurm` types into the config directory; returns their directory.
fn write_types() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let dir = thurm_config::config_dir().join("types");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("thurm.lua"), LUA_TYPES)?;
    Ok(dir)
}

/// Fails when a plugin called `name` exists already (a directory, a `.lua` file, or a program
/// of that stem), or something is at `target`.
fn name_free(
    dir: &std::path::Path,
    name: &str,
    target: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let (found, _) = thurm_config::plugins::discover(dir);
    if let Some(p) = found.iter().find(|p| p.name == name) {
        return Err(format!(
            "a plugin called {name:?} exists already: {}",
            p.path.display()
        )
        .into());
    }
    if target.exists() {
        return Err(format!("{} exists already", target.display()).into());
    }
    Ok(())
}

/// Asks the daemon, when one runs, to look at the plugins again.
fn reload_running(name: Option<&str>) {
    if let Ok(c) = crate::connect(false) {
        let params = match name {
            Some(n) => json!({ "name": n }),
            None => Value::Null,
        };
        if let Err(e) = call(&c, method::PLUGINS_RELOAD, params) {
            eprintln!("thurm: {e}");
        }
    }
}

fn find(name: &str) -> Result<thurm_config::plugins::FoundPlugin, Box<dyn std::error::Error>> {
    let (found, _) = thurm_config::plugins::discover(&thurm_config::plugins_dir());
    found.into_iter().find(|p| p.name == name).ok_or_else(|| {
        format!(
            "no plugin named {name:?} in {}",
            thurm_config::plugins_dir().display()
        )
        .into()
    })
}

fn git(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("git")
        .args(args)
        .status()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !status.success() {
        return Err(format!("git {} failed", args.first().unwrap_or(&"")).into());
    }
    Ok(())
}

/// The plugin name a repository URL suggests: its last part, without `.git` and a `thurm-`
/// prefix.
fn name_from_url(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default();
    let last = last.strip_suffix(".git").unwrap_or(last);
    last.strip_prefix("thurm-plugin-")
        .or_else(|| last.strip_prefix("thurm-"))
        .unwrap_or(last)
        .to_owned()
}

/// `thurm plugin` commands that work on this machine's files.
pub fn plugin_local(cmd: PluginCmd, json: bool) -> R {
    let dir = thurm_config::plugins_dir();
    match cmd {
        PluginCmd::New { name, program } => {
            if !ext::valid_plugin_name(&name)
                || name == thurm_config::plugins::INIT
                || name == thurm_config::plugins::SCRIPT
            {
                return Err(
                    format!("{name:?}: a plugin's name has only letters, digits, - and _").into(),
                );
            }
            let target = dir.join(&name);
            name_free(&dir, &name, &target)?;
            let made = || -> Result<(), Box<dyn std::error::Error>> {
                std::fs::create_dir_all(&target)?;
                for (file, text, executable) in thurm_config::plugins::scaffold(&name, program) {
                    let path = target.join(file);
                    std::fs::write(&path, text)?;
                    if executable {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
                    }
                }
                if !program {
                    let types = write_types()?;
                    std::fs::write(
                        target.join(".luarc.json"),
                        thurm_config::plugins::luarc(&types),
                    )?;
                }
                Ok(())
            };
            // Half made, it goes: the name was free, so the directory is this command's.
            if let Err(e) = made() {
                let _ = std::fs::remove_dir_all(&target);
                return Err(e);
            }
            println!("{}", target.display());
            reload_running(Some(&name));
        }
        PluginCmd::Install { url, name } => {
            let name = name.unwrap_or_else(|| name_from_url(&url));
            if !ext::valid_plugin_name(&name)
                || name == thurm_config::plugins::INIT
                || name == thurm_config::plugins::SCRIPT
            {
                return Err(format!("{name:?} cannot name a plugin; pick one with --name").into());
            }
            let target = dir.join(&name);
            name_free(&dir, &name, &target)?;
            eprintln!(
                "thurm: plugins run with your rights and can do anything you can: install only \
                 plugins you trust."
            );
            std::fs::create_dir_all(&dir)?;
            // A clone that failed part way goes: the name was free, so what is there is its.
            if let Err(e) = git(&[
                "clone",
                "--depth",
                "1",
                "--",
                &url,
                &target.display().to_string(),
            ]) {
                let _ = std::fs::remove_dir_all(&target);
                return Err(e);
            }
            // Not a plugin: it goes again, rather than sitting in the plugins directory.
            if find(&name).is_err() {
                let _ = std::fs::remove_dir_all(&target);
                return Err(format!(
                    "{url} is not a Thurm plugin (no plugin.toml or init.lua at its top)"
                )
                .into());
            }
            println!("{}", target.display());
            reload_running(Some(&name));
        }
        PluginCmd::Update { name } => {
            let (found, _) = thurm_config::plugins::discover(&dir);
            let mut updated = 0;
            for p in found
                .iter()
                .filter(|p| name.as_deref().is_none_or(|n| n == p.name))
            {
                if !p.path.join(".git").exists() {
                    if name.is_some() {
                        return Err(format!("{} was not installed from git", p.name).into());
                    }
                    continue;
                }
                eprintln!("{}:", p.name);
                git(&["-C", &p.path.display().to_string(), "pull", "--ff-only"])?;
                // Running its new files now: a later pull that fails leaves this one current.
                reload_running(Some(&p.name));
                updated += 1;
            }
            if name.is_some() && updated == 0 {
                find(name.as_deref().unwrap_or_default())?;
            }
        }
        PluginCmd::Remove { name, yes } => {
            let p = find(&name)?;
            if !yes {
                use std::io::IsTerminal;
                if !std::io::stdin().is_terminal() {
                    return Err("pass --yes to remove without asking".into());
                }
                eprint!("Delete {}? [y/N] ", p.path.display());
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !answer.trim().eq_ignore_ascii_case("y") {
                    return Ok(ExitCode::FAILURE);
                }
            }
            if p.path.is_dir() {
                std::fs::remove_dir_all(&p.path)?;
            } else {
                std::fs::remove_file(&p.path)?;
            }
            reload_running(None);
        }
        PluginCmd::Types => {
            let types = write_types()?;
            let luarc = thurm_config::config_dir().join(".luarc.json");
            if !luarc.exists() {
                std::fs::write(&luarc, thurm_config::plugins::luarc(&types))?;
            }
            if json {
                println!(
                    "{}",
                    json!({ "types": types.join("thurm.lua"), "luarc": luarc })
                );
            } else {
                println!("{}", types.join("thurm.lua").display());
            }
        }
        _ => unreachable!("not a local command"),
    }
    Ok(ExitCode::SUCCESS)
}

/// `thurm plugin` commands that go through a daemon.
pub fn plugin(c: &Client, cmd: PluginCmd, json: bool) -> R {
    match cmd {
        PluginCmd::List => {
            let list: Vec<ext::PluginInfo> =
                serde_json::from_value(call(c, method::PLUGINS_LIST, Value::Null)?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(ExitCode::SUCCESS);
            }
            if list.is_empty() {
                println!(
                    "No plugins. `thurm plugin new NAME` makes one in {}",
                    thurm_config::plugins_dir().display()
                );
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "{:<20} {:<8} {:<9} {:<9} ACTIONS",
                "NAME", "KIND", "STATE", "VERSION"
            );
            for p in &list {
                println!(
                    "{:<20} {:<8} {:<9} {:<9} {}",
                    p.name,
                    p.kind,
                    p.state,
                    p.version,
                    p.actions.join(", ")
                );
                if let Some(e) = &p.error {
                    println!("  {e}");
                }
            }
        }
        PluginCmd::Enable { name } => {
            call(c, method::PLUGINS_ENABLE, json!({ "name": name }))?;
        }
        PluginCmd::Disable { name } => {
            call(c, method::PLUGINS_DISABLE, json!({ "name": name }))?;
        }
        PluginCmd::Reload { name } => {
            let params = name.map_or(Value::Null, |n| json!({ "name": n }));
            call(c, method::PLUGINS_RELOAD, params)?;
        }
        PluginCmd::Logs { name, lines } => {
            let r = call(
                c,
                method::PLUGINS_LOGS,
                json!({ "name": name, "lines": lines }),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r["lines"])?);
                return Ok(ExitCode::SUCCESS);
            }
            for line in r["lines"].as_array().into_iter().flatten() {
                println!("{}", line.as_str().unwrap_or_default());
            }
        }
        _ => unreachable!("a local command"),
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    /// A name swapped for a symlink after the directory was listed is not opened.
    #[test]
    fn a_swapped_in_symlink_is_not_followed() {
        let dir = std::env::temp_dir().join(format!("thurm-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.with_extension("secret.lua");
        std::fs::write(&secret, "token").unwrap();
        let dir = dir.canonicalize().unwrap();
        let root = super::open_dir_path(&dir).unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("m.lua")).unwrap();
        assert!(super::open_nofollow(Some(&root), std::ffi::OsStr::new("m.lua"), false).is_err());
        // A directory on the way swapped for a symlink fails the walk.
        std::fs::create_dir(dir.join("real")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("via")).unwrap();
        assert!(super::open_dir_path(&dir.join("via")).is_err());
        assert!(super::open_dir_path(&dir.join("real")).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(&secret).unwrap();
    }

    #[test]
    fn a_new_plugin_takes_no_name_in_use() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("thurm-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("solo.lua"), "").unwrap();
        std::fs::write(dir.join("tool.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dir.join("tool.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        for taken in ["solo", "tool"] {
            assert!(
                super::name_free(&dir, taken, &dir.join(taken)).is_err(),
                "{taken}"
            );
        }
        assert!(super::name_free(&dir, "fresh", &dir.join("fresh")).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_remote_script_brings_its_lua_modules() {
        let dir = std::env::temp_dir().join(format!("thurm-modules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib/deep")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("main.lua"), "a").unwrap();
        std::fs::write(dir.join("lib/util.lua"), "b").unwrap();
        std::fs::write(dir.join("lib/deep/init.lua"), "c").unwrap();
        std::fs::write(dir.join("lib/notes.txt"), "d").unwrap();
        std::fs::write(dir.join(".git/hook.lua"), "e").unwrap();
        // A link out of the directory stays home, file or directory.
        let secret = std::env::temp_dir().join(format!("thurm-secret-{}.lua", std::process::id()));
        std::fs::write(&secret, "return 'token'").unwrap();
        std::os::unix::fs::symlink(&secret, dir.join("lib/private.lua")).unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), dir.join("outside")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let m = super::lua_modules(&dir);
        assert_eq!(
            m.keys().map(String::as_str).collect::<Vec<_>>(),
            ["lib/deep/init.lua", "lib/util.lua", "main.lua"]
        );
        // A file over the budget is left out, unread.
        std::fs::write(dir.join("zbig.lua"), vec![b'x'; (4 << 20) + 1]).unwrap();
        assert_eq!(super::lua_modules(&dir).len(), 3);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(&secret).unwrap();
    }

    /// A directory's names are read only up to the limit.
    #[test]
    fn a_listing_stops_at_its_limit() {
        let dir = std::env::temp_dir().join(format!("thurm-listing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..20 {
            std::fs::write(dir.join(format!("{i}.lua")), "").unwrap();
        }
        let fd = super::open_dir_path(&dir.canonicalize().unwrap()).unwrap();
        assert_eq!(super::dir_names(&fd, 5).len(), 5);
        assert_eq!(super::dir_names(&fd, 100).len(), 20);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_repository_names_its_plugin() {
        assert_eq!(
            super::name_from_url("https://github.com/a/thurm-git-tools.git"),
            "git-tools"
        );
        assert_eq!(super::name_from_url("git@github.com:a/thurm-plugin-x"), "x");
        assert_eq!(
            super::name_from_url("https://example.com/a/zoxide/"),
            "zoxide"
        );
    }
}
