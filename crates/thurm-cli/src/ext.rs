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
/// another host to `require`. Bounded: a script in a large directory brings what fits.
fn lua_modules(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    const MAX_FILES: usize = 200;
    const MAX_BYTES: usize = 4 << 20;
    let mut out = std::collections::BTreeMap::new();
    let mut bytes = 0;
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut paths: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            let hidden = path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'));
            // Only what is really in the directory: a symlink may point at a file of the
            // user's own that must not leave the machine.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if hidden || meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if depth < 3 {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !meta.is_file() || path.extension().is_none_or(|e| e != "lua") {
                continue;
            }
            // Within the budget, read no more than it allows.
            let room = MAX_BYTES - bytes;
            if out.len() >= MAX_FILES {
                return out;
            }
            // A file larger than what is left is not sent: skipped without reading it.
            if meta.len() > room as u64 {
                continue;
            }
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            let mut text = String::new();
            if std::io::Read::read_to_string(
                &mut std::io::Read::take(file, room as u64 + 1),
                &mut text,
            )
            .is_err()
                || text.len() > room
            {
                // Not text, or it grew while read: left out.
                continue;
            }
            if let Ok(rel) = path.strip_prefix(dir) {
                bytes += text.len();
                out.insert(rel.display().to_string(), text);
            }
        }
    }
    out
}

/// `thurm do FILE [ARGS]`: runs a Lua script in the daemon, printing what it logs.
pub fn script(remote: Option<&str>, file: &str, args: Vec<String>, json: bool) -> R {
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
        (Some(_), Some(d)) => (None, lua_modules(std::path::Path::new(&d))),
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

#[cfg(test)]
mod tests {
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
}
