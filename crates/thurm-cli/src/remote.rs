//! `thurm remote …`, `thurm --remote NAME …` and `thurm handoff`.
//!
//! The CLI never keeps a tunnel of its own: `--remote` goes through the socket the app
//! forwards (`~/Library/Caches/Thurm/remote/<name>.sock`), and says so when the app is not
//! connected to that host.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use thurm_client::{Client, ClientError, ConnectOptions};
use thurm_config::{ClipboardRead, Config, RemoteConfig};
use thurm_proto::{CreatePane, PaneSize, Request, Response, UiCommand};
use thurm_remote::handoff::{self, Registry, SshHost};
use thurm_remote::install;
use thurm_remote::tunnel::{Phase, Status};
use thurm_remote::{Ssh, ThurmInfo};

type R = Result<ExitCode, Box<dyn std::error::Error>>;

fn remote_config(name: &str) -> Result<RemoteConfig, String> {
    let cfg = Config::load().map_err(|e| e.to_string())?;
    cfg.remote(name).cloned().ok_or_else(|| {
        let known: Vec<&str> = cfg.remote.iter().map(|r| r.name.as_str()).collect();
        if known.is_empty() {
            format!(
                "no remote named {name:?} (add one with `thurm remote add {name} <ssh-target>`)"
            )
        } else {
            format!(
                "no remote named {name:?} (configured: {})",
                known.join(", ")
            )
        }
    })
}

/// A client for `name`'s daemon through the app's tunnel.
pub fn connect_remote(name: &str) -> Result<Arc<Client>, Box<dyn std::error::Error>> {
    remote_config(name)?;
    let socket = thurm_config::remote_socket_path(name);
    let state = Status::read(name);
    if let Some(s) = &state
        && s.phase != Phase::Connected
    {
        return Err(not_connected(name, Some(s)).into());
    }
    let client = Client::connect(
        ConnectOptions {
            socket,
            spawn_daemon: None,
            client_name: "thurm-cli",
            ui: false,
        },
        |_| {},
        || {},
    );
    match client {
        Ok(c) => Ok(c),
        Err(ClientError::Connect(_)) => Err(not_connected(name, state.as_ref()).into()),
        Err(e) => Err(e.into()),
    }
}

fn not_connected(name: &str, s: Option<&Status>) -> String {
    match s {
        Some(s) => {
            let mut m = format!(
                "{name} is not connected in Thurm (state: {})",
                s.phase.label()
            );
            if let Some(msg) = &s.message {
                m.push_str(&format!(": {}", first_line(msg)));
            }
            m
        }
        None => format!(
            "{name} is not connected in Thurm (the app is not running or has not reached it)"
        ),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

fn ask(question: &str, default_yes: bool, assume: Option<bool>) -> bool {
    if let Some(a) = assume {
        return a;
    }
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} [{}] ", if default_yes { "Y/n" } else { "y/N" });
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}

/// `thurm` and `thurmd` next to this executable (the app's Contents/Helpers, or the
/// install directory), for installing on a Mac.
fn local_bins() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Tells the app (through the local daemon) that the config changed.
fn reload_app() {
    if let Ok(c) = Client::connect(
        ConnectOptions {
            client_name: "thurm-cli",
            ..Default::default()
        },
        |_| {},
        || {},
    ) {
        let _ = c.request(Request::ReloadConfig);
    }
}

pub struct AddOptions {
    pub name: String,
    pub target: String,
    pub socket: Option<String>,
    /// Answer every question (`--yes` / `--no`), for scripts.
    pub assume: Option<bool>,
}

pub fn add(o: AddOptions) -> R {
    thurm_config::validate_remote_name(&o.name)?;
    let cfg = Config::load()?;
    if cfg.remote(&o.name).is_some() {
        return Err(format!("a remote named {:?} exists already", o.name).into());
    }
    thurm_remote::ssh::prepare_dir()?;
    let ssh = Ssh::new(&o.target)?;
    println!("Connecting to {} …", o.target);
    let effective = ssh.effective_config()?;
    thurm_remote::ssh::check_forwards(&effective)?;
    let plan = install::plan(&ssh, o.socket.as_deref(), local_bins().as_deref())?;
    let host = &plan.host;
    println!("{}: {} {}", o.name, host.os, host.arch);
    install_flow(&ssh, &o.name, o.socket.as_deref(), plan.clone(), o.assume)?;

    let entry = RemoteConfig {
        name: o.name.clone(),
        host: o.target.clone(),
        socket: o.socket.clone(),
        enabled: true,
        clipboard_read: ClipboardRead::Ask,
    };
    thurm_config::edit_config(|text| thurm_config::with_remote_added(text, &entry))?;
    println!(
        "Added [[remote]] {} to {}",
        o.name,
        thurm_config::config_path().display()
    );

    if host.linger.as_deref() == Some("no") {
        println!(
            "note: lingering is off for your user on {}: the daemon's socket goes away when your \
             last session there ends. Run `loginctl enable-linger $USER` there.",
            o.name
        );
    }
    if ask(
        &format!("Install the Claude Code / Codex hooks on {}?", o.name),
        true,
        o.assume,
    ) {
        const HOOKS: &str = concat!(
            "TH=\"${THURM_HOME:-$HOME}\"; ",
            "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
            "thurm hooks install"
        );
        match ssh.run(HOOKS, &[], None, std::time::Duration::from_secs(60)) {
            Ok(out) => print!("{out}"),
            Err(e) => eprintln!("thurm: installing hooks on {} failed: {e}", o.name),
        }
    }
    reload_app();
    println!(
        "Thurm connects to {} now; it shows as a workspace in the sidebar.",
        o.name
    );
    Ok(ExitCode::SUCCESS)
}

/// Offers to install our build (and replace the daemon) when the host needs it.
fn install_flow(
    ssh: &Ssh,
    name: &str,
    socket: Option<&str>,
    plan: install::Plan,
    assume: Option<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let host = &plan.host;
    if !plan.installed {
        let what = match &host.info {
            Some(i) => format!(
                "{name} has Thurm {} (this is {})",
                i.build,
                thurm_proto::BUILD
            ),
            None if host.thurm.is_some() => format!("{name} has an older Thurm"),
            None => format!("Thurm is not installed on {name}"),
        };
        println!("{what}.");
        let Some(&first) = plan.methods.first() else {
            return Err(plan
                .problem
                .unwrap_or_else(|| "no way to install Thurm there".into())
                .into());
        };
        let method = if plan.methods.len() > 1 && assume.is_none() && std::io::stdin().is_terminal()
        {
            for (i, m) in plan.methods.iter().enumerate() {
                println!("  {}) {}", i + 1, m.label());
            }
            print!("Install how? [1] ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            line.trim()
                .parse::<usize>()
                .ok()
                .and_then(|n| plan.methods.get(n.wrapping_sub(1)).copied())
                .unwrap_or(first)
        } else {
            first
        };
        if !ask(
            &format!(
                "Install Thurm {} on {name} ({})?",
                thurm_proto::BUILD,
                method.label()
            ),
            true,
            assume,
        ) {
            return Err(format!("{name} needs Thurm {} to connect", thurm_proto::BUILD).into());
        }
        let info: ThurmInfo = install::install(ssh, host, method, local_bins().as_deref())?;
        println!("Installed thurm {} on {name}.", info.build);
    }
    if let Some(d) = &plan.daemon
        && d.running
        && !d
            .build
            .as_deref()
            .is_some_and(|b| install::same_build(thurm_proto::BUILD, b))
    {
        let restart = !d.hot_upgrade;
        let question = if restart {
            format!(
                "The daemon on {name} is too old to upgrade in place: restarting it stops the \
                 programs in its panes. Restart it?"
            )
        } else {
            format!("Upgrade the daemon on {name} in place (its panes keep running)?")
        };
        if ask(&question, !restart, assume) {
            let out = install::upgrade_daemon(ssh, socket, Some(d), restart)?;
            println!("{out}");
        }
    }
    Ok(())
}

pub fn install_cmd(name: &str, assume: Option<bool>) -> R {
    let r = remote_config(name)?;
    thurm_remote::ssh::prepare_dir()?;
    let ssh = Ssh::new(&r.host)?;
    let plan = install::plan(&ssh, r.socket.as_deref(), local_bins().as_deref())?;
    install_flow(&ssh, name, r.socket.as_deref(), plan, assume)?;
    reload_app();
    Ok(ExitCode::SUCCESS)
}

pub fn remove(name: &str) -> R {
    thurm_config::edit_config(|text| thurm_config::with_remote_removed(text, name))?;
    reload_app();
    println!("Removed remote {name}. Its daemon and panes keep running there.");
    Ok(ExitCode::SUCCESS)
}

pub fn list(json: bool) -> R {
    let cfg = Config::load()?;
    let rows: Vec<(RemoteConfig, Option<Status>)> = cfg
        .remote
        .iter()
        .map(|r| (r.clone(), Status::read(&r.name)))
        .collect();
    if json {
        let v: Vec<_> = rows
            .iter()
            .map(|(r, s)| {
                serde_json::json!({
                    "name": r.name, "host": r.host, "enabled": r.enabled,
                    "state": s.as_ref().map(|s| s.phase.label()),
                    "status": s,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(ExitCode::SUCCESS);
    }
    if rows.is_empty() {
        println!("no remotes (add one with `thurm remote add <name> <ssh-target>`)");
        return Ok(ExitCode::SUCCESS);
    }
    println!("{:<16} {:<32} {:<16} BUILD", "NAME", "HOST", "STATE");
    for (r, s) in rows {
        let state = match (&s, r.enabled) {
            (_, false) => "disabled".to_owned(),
            (Some(s), _) => s.phase.label().to_owned(),
            (None, _) => "app not running".to_owned(),
        };
        let build = s.and_then(|s| s.remote_build).unwrap_or_default();
        println!("{:<16} {:<32} {:<16} {build}", r.name, r.host, state);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn status(name: Option<&str>, json: bool) -> R {
    let Some(name) = name else {
        return list(json);
    };
    let r = remote_config(name)?;
    let s = Status::read(name);
    if json {
        println!(
            "{}",
            serde_json::json!({"name": r.name, "host": r.host, "status": s})
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("{} ({})", r.name, r.host);
    match s {
        None => println!("  not connected: Thurm is not running (or has not tried yet)"),
        Some(s) => {
            println!("  state:    {}", s.phase.label());
            if let Some(m) = &s.message {
                for (i, line) in m.lines().enumerate() {
                    println!(
                        "  {}{line}",
                        if i == 0 { "message:  " } else { "          " }
                    );
                }
            }
            if let Some(b) = &s.remote_build {
                println!(
                    "  build:    {b}{}",
                    if s.upgrade_available {
                        format!(
                            " (this Mac: {}; `thurm remote install {name}`)",
                            thurm_proto::BUILD
                        )
                    } else {
                        String::new()
                    }
                );
            }
            if let (Some(os), Some(arch)) = (&s.os, &s.arch) {
                println!("  platform: {os} {arch}");
            }
            if let Some(at) = s.retry_at {
                let secs = at.saturating_sub(thurm_remote::tunnel::now_secs());
                println!("  retry in: {secs} s");
            }
            println!("  socket:   {}", s.socket);
            if s.linger.as_deref() == Some("no") {
                println!("  note:     run `loginctl enable-linger $USER` on {name}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub struct HandoffOptions {
    pub remote: Option<String>,
    pub preset: Option<String>,
    pub branch: Option<String>,
    pub path: Option<PathBuf>,
    pub list: bool,
    pub fetch: Option<String>,
    pub cleanup: Option<String>,
    pub force: bool,
}

pub fn handoff_cmd(o: HandoffOptions, json: bool) -> R {
    let reg = Registry::default();
    if o.list {
        let all = reg.list();
        if json {
            println!("{}", serde_json::to_string_pretty(&all)?);
        } else if all.is_empty() {
            println!("no handoffs");
        } else {
            for h in all {
                println!(
                    "{:<48} {:<10} {}{}",
                    h.id,
                    h.fetched
                        .as_deref()
                        .map(|s| &s[..s.len().min(10)])
                        .unwrap_or("-"),
                    h.repo,
                    h.fetch_error
                        .as_deref()
                        .map(|e| format!("  (fetch failed: {})", first_line(e)))
                        .unwrap_or_default()
                );
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(id) = o.fetch.or(o.cleanup.clone()) {
        let mut h = reg
            .get(&id)
            .ok_or_else(|| format!("no handoff {id:?} (see `thurm handoff --list`)"))?;
        let r = remote_config(&h.host)?;
        let host = SshHost {
            name: r.name.clone(),
            ssh: Ssh::new(&r.host)?,
        };
        if o.cleanup.is_some() {
            let out = handoff::cleanup(&host, &mut h, o.force);
            match out {
                Ok(out) => {
                    reg.remove(&h.id)?;
                    println!("{}", out.message);
                }
                Err(e) => {
                    reg.record_fetch(&h)?;
                    return Err(format!("{e}; pass --force to remove it anyway").into());
                }
            }
        } else {
            let res = handoff::fetch(&host, &mut h);
            reg.record_fetch(&h)?;
            let sha = res?;
            println!("{} → {sha}", h.tracking_ref());
        }
        return Ok(ExitCode::SUCCESS);
    }

    let name = o.remote.ok_or("which host? pass --remote NAME")?;
    let r = remote_config(&name)?;
    // The agent's tab opens through the app: it must be connected.
    let client = connect_remote(&name)?;
    let path = match o.path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let host = SshHost {
        name: r.name.clone(),
        ssh: Ssh::new(&r.host)?,
    };
    let mut h = handoff::prepare(&host, &path, o.branch.as_deref())?;
    // The worktree exists on the host now: keep it findable for --fetch and --cleanup even
    // when its pane never starts.
    reg.put(&h)?;
    let stranded = |e: String| format!("{e}; the handoff {} stays for --fetch or --cleanup", h.id);
    let cfg = Config::load().unwrap_or_default();
    let created = client.request(Request::CreatePane(CreatePane {
        cwd: Some(h.worktree.clone()),
        agent_preset: o.preset.clone(),
        size: PaneSize {
            cols: cfg.window.columns,
            rows: cfg.window.rows,
            ..Default::default()
        },
        ..Default::default()
    }));
    let pane = match created {
        Ok(Response::PaneCreated { pane }) => pane,
        Ok(other) => return Err(stranded(format!("unexpected response {other:?}")).into()),
        Err(e) => return Err(stranded(format!("creating the agent's pane: {e}")).into()),
    };
    h.pane = Some(pane);
    reg.update(&h.id, |x| x.pane = Some(pane))?;
    if let Err(e) = client.request(Request::Ui(UiCommand::NewTab {
        pane,
        new_window: false,
    })) {
        eprintln!("thurm: the agent's pane runs but no tab opened: {e}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&h)?);
    } else {
        println!(
            "Handed off {} to {name}: {} in {}{}",
            h.repo,
            h.branch,
            h.worktree,
            if h.wip {
                " (with your uncommitted changes as a WIP commit)"
            } else {
                ""
            }
        );
        println!(
            "Committed work comes back as {} (`thurm handoff --fetch {}`).",
            h.tracking_ref(),
            h.id
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `thurm remote-info` (hidden): what the app's install check reads over ssh.
pub fn remote_info() -> R {
    println!("{}", serde_json::to_string(&ThurmInfo::current())?);
    Ok(ExitCode::SUCCESS)
}
