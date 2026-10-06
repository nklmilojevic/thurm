//! `thurm` — control Thurm from the command line.
//!
//! Designed to be usable by coding agents: every command works on the current pane by default
//! (`$THURM_PANE_ID`), prints plain text, supports `--json`, and uses exit codes
//! (0 ok, 1 error, 124 wait timeout).

mod events;
mod layouts;
mod attach;
mod agent;
mod agent_prompt;
mod hook_pgrp;
mod remote;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use thurm_client::{Client, ClientError, ConnectOptions, find_daemon};
use thurm_config::hooks;
use thurm_proto::{
    CaptureOpts, CreatePane, ENV_PANE_ID, PaneId, PaneInfo, PaneSize, Request, Response, ScrollCmd,
    SplitDir, UiCommand, WaitCondition, WaitOutcome,
};

#[derive(Parser)]
#[command(
    name = "thurm",
    version,
    about = "Control Thurm terminal panes (tabs, splits, agents, capture, wait)",
    after_help = "Exit status: 0 success, 1 error, 2 `wait`: the pane exited (without --exit), \
                  3 `daemon status`: not running, 4 `daemon status`: running but incompatible, \
                  124 `wait`: timed out."
)]
struct Cli {
    /// Machine readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    /// Talk to this `[[remote]]` host's daemon, through the tunnel Thurm keeps open to it.
    #[arg(long, global = true, value_name = "NAME")]
    remote: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

/// `--remote` is in effect: pane ids are the remote daemon's, so `$THURM_PANE_ID` (a pane of
/// this machine) is not a default.
static REMOTE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn remote_mode() -> bool {
    REMOTE.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    /// Rename the workspace a pane is in (default: this one). With `new-tab --window`, which
    /// prints the new pane's id: `thurm workspace rename --pane $(thurm new-tab --window) api`.
    Rename {
        #[arg(short, long)]
        pane: Option<PaneId>,
        #[arg(required = true)]
        name: Vec<String>,
    },
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Add a host: checks the connection, offers to install Thurm there, writes `[[remote]]`.
    Add {
        name: String,
        /// ssh target: a Host alias, user@host, or ssh://user@host:port.
        target: String,
        /// The daemon's socket there (default: what `thurm socket-path` prints there).
        #[arg(long)]
        socket: Option<String>,
        /// Answer yes to every question.
        #[arg(long, short = 'y', conflicts_with = "no")]
        yes: bool,
        /// Answer no to every question.
        #[arg(long)]
        no: bool,
    },
    /// List hosts and their connection state.
    #[command(alias = "ls")]
    List,
    /// Forget a host (its daemon and panes keep running there).
    #[command(alias = "rm")]
    Remove { name: String },
    /// Connection state of one host (or all).
    Status { name: Option<String> },
    /// Install this build of Thurm on a host, and upgrade its daemon in place.
    Install {
        name: String,
        /// Answer yes to every question (a daemon restart that stops programs still asks).
        #[arg(long, short = 'y', conflicts_with = "no")]
        yes: bool,
        /// Answer no to every question.
        #[arg(long)]
        no: bool,
    },
    /// Check what a host needs (Thurm, lingering, PATH, agents, sign-in, hooks), and fix it.
    Doctor {
        /// Default: every host.
        name: Option<String>,
        /// Offer the fix for every problem found.
        #[arg(long)]
        fix: bool,
        /// With --fix: run every fix without asking (fixes that need a terminal are listed;
        /// a daemon restart or an installer script still asks).
        #[arg(long, short = 'y', requires = "fix")]
        yes: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Dir {
    Right,
    Down,
    Left,
    Up,
}

impl From<Dir> for SplitDir {
    fn from(d: Dir) -> Self {
        match d {
            Dir::Right => SplitDir::Right,
            Dir::Down => SplitDir::Down,
            Dir::Left => SplitDir::Left,
            Dir::Up => SplitDir::Up,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Attach to a pane. Press Ctrl-] then d to detach.
    Attach {
        #[arg(short, long)]
        pane: PaneId,
    },
    /// Inspect or report agent state.
    Agent {
        #[command(subcommand)]
        action: agent::AgentCmd,
    },
    /// List panes.
    #[command(alias = "ls")]
    List,
    /// Print the id of the pane this command runs in.
    PaneId,
    /// Show details about a pane.
    Info {
        #[arg(short, long)]
        pane: Option<PaneId>,
    },
    /// Open a new tab running COMMAND (default: shell).
    NewTab {
        #[arg(long)]
        cwd: Option<String>,
        /// Open in a new workspace (in the background) instead of a tab.
        #[arg(long)]
        window: bool,
        /// Keep the pane open after the command exits.
        #[arg(long)]
        hold: bool,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Split a pane and run COMMAND in the new half (default: shell).
    Split {
        #[arg(short, long, value_enum, default_value = "right")]
        dir: Dir,
        /// Pane to split (default: current pane).
        #[arg(short, long)]
        pane: Option<PaneId>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        hold: bool,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Fork the agent session of a pane (Claude Code / Codex, needs hooks) into a new split or
    /// tab: `claude --resume <id> --fork-session`, `codex fork <id>`.
    Fork {
        /// Pane running the agent (default: current pane).
        #[arg(short, long)]
        pane: Option<PaneId>,
        /// Split direction; a new tab when omitted.
        #[arg(short, long, value_enum)]
        split: Option<Dir>,
    },
    /// Completions for what is typed at a pane's prompt (what Tab offers in the app).
    Complete {
        #[arg(short, long)]
        pane: Option<PaneId>,
    },
    /// Processes running in panes and the TCP ports they listen on (dev servers, ...).
    Procs {
        #[arg(short, long)]
        pane: Option<PaneId>,
        /// Only processes with listening ports.
        #[arg(long)]
        ports: bool,
    },
    /// Launch a coding agent preset (see `thurm presets`) in a new tab or split.
    Launch {
        preset: String,
        #[arg(short, long, value_enum)]
        split: Option<Dir>,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// List agent launch presets.
    Presets,
    /// Type text into a pane (Enter is appended unless --no-enter). Reads stdin with `-`.
    Send {
        #[arg(short, long)]
        pane: Option<PaneId>,
        #[arg(long)]
        no_enter: bool,
        /// Send as a bracketed paste (safe for multi-line text).
        #[arg(long)]
        paste: bool,
        text: Vec<String>,
    },
    /// Send keys: Enter, Tab, Escape, Up, Down, Left, Right, Backspace, C-c, C-d, M-x, ...
    SendKeys {
        #[arg(short, long)]
        pane: Option<PaneId>,
        keys: Vec<String>,
    },
    /// Print the screen (or scrollback) of a pane.
    Capture {
        #[arg(short, long)]
        pane: Option<PaneId>,
        /// Last N lines including scrollback.
        #[arg(short = 'n', long)]
        lines: Option<u32>,
        /// Whole scrollback.
        #[arg(short = 'S', long)]
        scrollback: bool,
        /// Keep colors (ANSI escapes).
        #[arg(short = 'e', long)]
        ansi: bool,
    },
    /// Explain what happened in the pane's last finished command, with the on-device model
    /// (needs `ai.enabled = true`).
    Explain {
        #[arg(short, long)]
        pane: Option<PaneId>,
    },
    /// Wait for a condition. Exit code 124 on timeout.
    Wait {
        #[arg(short, long)]
        pane: Option<PaneId>,
        /// No output for this many milliseconds.
        #[arg(long, value_name = "MS")]
        idle: Option<u64>,
        /// The shell is back at a prompt (needs shell integration).
        #[arg(long)]
        prompt: bool,
        /// The pane's process exited.
        #[arg(long)]
        exit: bool,
        /// A regex matches the visible screen.
        #[arg(long, value_name = "REGEX")]
        r#match: Option<String>,
        /// The agent in the pane stopped working (idle or waiting for input).
        #[arg(long, alias = "until-free")]
        agent_free: bool,
        /// The agent finished its turn (needs `thurm hooks install`).
        #[arg(long)]
        agent_done: bool,
        /// The agent is waiting for input (permission prompt, question).
        #[arg(long)]
        agent_waiting: bool,
        /// Give up after this many seconds.
        #[arg(short, long, value_name = "SECS")]
        timeout: Option<f64>,
    },
    /// List panes running coding agents and their status.
    Agents,
    /// Focus a pane in the GUI.
    Focus { pane: PaneId },
    /// Close (kill) a pane.
    Close { pane: Option<PaneId> },
    /// Set the tab title of a pane (empty to reset).
    Title {
        #[arg(short, long)]
        pane: Option<PaneId>,
        title: Vec<String>,
    },
    /// The app's workspaces.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceCmd,
    },
    /// Scroll a pane's viewport.
    Scroll {
        #[arg(short, long)]
        pane: Option<PaneId>,
        #[arg(value_parser = ["up", "down", "top", "bottom", "prev-prompt", "next-prompt"])]
        to: String,
    },
    /// Clear a pane's scrollback.
    Clear {
        #[arg(short, long)]
        pane: Option<PaneId>,
    },
    /// Show a desktop notification from this pane (OSC 777).
    Notify { title: String, body: Vec<String> },
    /// Print the saved window/tab/split layout as JSON.
    Layout {
        #[command(subcommand)]
        action: Option<layouts::LayoutCmd>,
    },
    /// Stream state changes as JSON Lines.
    Events {
        #[arg(long)]
        pane: Option<PaneId>,
    },
    /// Reload ~/.config/thurm/config.toml.
    Reload,
    /// Change a setting in config.toml and reload: `thurm set window.tab_style sidebar`.
    /// Strings may be given bare; other values are TOML (`true`, `240.0`).
    Set { key: String, value: String },
    /// List themes, or set one: `thurm theme nord`, `thurm theme light:catppuccin-latte,dark:catppuccin-mocha`.
    Theme { spec: Option<String> },
    /// Write a session snapshot now.
    Save,
    /// Daemon control.
    Daemon {
        /// install-launchd / uninstall-launchd (macOS), install-systemd / uninstall-systemd
        /// (Linux): start the daemon at login, so sessions are there before the app is opened.
        /// upgrade: replace a daemon left running by an older Thurm with this one's, keeping
        /// every pane running.
        #[arg(value_parser = ["status", "start", "stop", "upgrade", "install-launchd", "uninstall-launchd", "install-systemd", "uninstall-systemd"])]
        action: String,
    },
    /// Print the config file path.
    ConfigPath,
    /// Print the daemon socket path (what `thurmd` and `thurm` use by default).
    SocketPath,
    /// What this `thurm` is (build, protocol, socket) as JSON; read by the app over ssh.
    #[command(hide = true)]
    RemoteInfo,
    /// Remote workspaces: thurmd on other machines, reached with ssh.
    Remote {
        #[command(subcommand)]
        action: RemoteCmd,
    },
    /// Hand the repository here to an agent on a remote host: pushes a snapshot (uncommitted
    /// changes included) to a worktree there and opens a tab running PRESET in it. Committed
    /// results come back as refs/remotes/thurm-<host>/agent/<name>. Needs --remote.
    Handoff {
        /// Agent preset to start (see `thurm --remote NAME presets`); a shell when omitted.
        #[arg(long)]
        preset: Option<String>,
        /// Branch to create: agent/<name> (default: a generated name).
        #[arg(long)]
        branch: Option<String>,
        /// Repository (default: the current directory).
        #[arg(long)]
        path: Option<std::path::PathBuf>,
        /// List handoffs.
        #[arg(long, conflicts_with_all = ["fetch", "cleanup"])]
        list: bool,
        /// Fetch a handoff's committed work now.
        #[arg(long, value_name = "ID", conflicts_with = "cleanup")]
        fetch: Option<String>,
        /// Remove a handoff's worktree on its host (after a final fetch).
        #[arg(long, value_name = "ID")]
        cleanup: Option<String>,
        /// With --cleanup: remove even with uncommitted or unfetched work.
        #[arg(long, requires = "cleanup")]
        force: bool,
    },
    /// Install, remove or check the agent hooks that report status to Thurm
    /// (Claude Code: ~/.claude/settings.json).
    Hooks {
        #[arg(value_parser = ["install", "uninstall", "status"])]
        action: String,
        /// Only this agent (claude, codex). Default: every agent whose config directory exists.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Called by agent hooks: report an agent event for this pane. Reads the hook's JSON on
    /// stdin. Silent, and always succeeds, so it can never break the agent.
    #[command(hide = true)]
    AgentHook {
        agent: Option<String>,
        event: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("thurm: {e}");
            ExitCode::FAILURE
        }
    }
}

fn connect(spawn: bool) -> Result<std::sync::Arc<Client>, ClientError> {
    let daemon = if spawn { find_daemon() } else { None };
    Client::connect(
        ConnectOptions {
            spawn_daemon: daemon.as_deref(),
            client_name: "thurm-cli",
            ..Default::default()
        },
        |_| {},
        || {},
    )
}

fn agent_hook(agent: Option<String>, event: Option<String>) {
    let (Some(agent), Some(event)) = (agent, event) else {
        return;
    };
    let Some(pane) = std::env::var(ENV_PANE_ID).ok().and_then(|v| v.parse().ok()) else {
        return;
    };
    let mut input = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().take(1 << 20).read_to_string(&mut input);
    }
    let payload: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    let field = |k: &str| payload.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let event = hooks::hook_event(&event, &payload);
    let Ok(c) = connect(false) else { return };
    let _ = c.request(Request::AgentHook {
        pane,
        agent,
        event,
        session_id: field("session_id"),
        // The prompt, on prompt-submit: it names the session when the agent doesn't.
        message: field("message").or_else(|| field("prompt")),
        transcript_path: field("transcript_path"),
        pgrp: Some(hook_pgrp::agent_pgrp()),
    });
}

fn hooks_cmd(action: &str, only: Option<&str>, json: bool) -> R {
    let agents: Vec<&hooks::HookAgent> = match only {
        Some(k) => hooks::AGENTS.iter().filter(|a| a.kind == k).collect(),
        None => hooks::AGENTS.iter().collect(),
    };
    if agents.is_empty() {
        return Err(format!("unknown agent {:?} (claude, codex)", only.unwrap_or("")).into());
    }
    let mut ok = true;
    let mut report = Vec::new();
    let mut seen = 0;
    for agent in agents {
        let path = hooks::settings_path(agent);
        let present = path.parent().is_some_and(|d| d.is_dir());
        // Without --agent, skip agents that aren't installed (no config directory).
        if only.is_none() && !present && !(json && action == "status") {
            continue;
        }
        seen += 1;
        let current = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display()).into()),
        };
        let total = agent.events.len();
        if action == "status" {
            let n = hooks::installed(agent, &current);
            if json {
                report.push(serde_json::json!({
                    "agent": agent.kind, "name": agent.name, "installed": n, "total": total,
                    "present": present, "path": path.display().to_string(),
                }));
                continue;
            }
            println!(
                "{}: {n}/{total} hooks installed ({})",
                agent.name,
                path.display()
            );
            ok &= n == total;
            continue;
        }
        let install = action == "install";
        if hooks::write_at(agent, &path, install)? == hooks::Written::Unchanged {
            println!("{}: nothing to change ({})", agent.name, path.display());
            continue;
        }
        println!(
            "{}: hooks {} ({}). Restart running sessions to pick them up.",
            agent.name,
            if install { "installed" } else { "removed" },
            path.display()
        );
    }
    if seen == 0 {
        // Nothing was said otherwise: an agent that never ran has no config directory yet.
        eprintln!(
            "thurm: no agent found (no ~/.claude or ~/.codex); run the agent once, or pass --agent claude"
        );
        return Ok(ExitCode::FAILURE);
    }
    if json && action == "status" {
        println!("{}", serde_json::Value::Array(report));
        return Ok(ExitCode::SUCCESS);
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn current_pane(p: Option<PaneId>) -> Result<PaneId, String> {
    if let Some(p) = p {
        return Ok(p);
    }
    if remote_mode() {
        return Err(
            "with --remote, name the remote pane with --pane (see `thurm --remote NAME list`)"
                .into(),
        );
    }
    std::env::var(ENV_PANE_ID)
        .ok()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("not running inside Thurm (no ${ENV_PANE_ID}); pass --pane"))
}

type R = Result<ExitCode, Box<dyn std::error::Error>>;

fn run(cli: Cli) -> R {
    let json = cli.json;
    let remote = cli.remote.clone();
    if remote.is_some() {
        REMOTE.store(true, std::sync::atomic::Ordering::Relaxed);
        if matches!(
            cli.cmd,
            Cmd::PaneId
                | Cmd::ConfigPath
                | Cmd::SocketPath
                | Cmd::RemoteInfo
                | Cmd::Notify { .. }
                | Cmd::Daemon { .. }
                | Cmd::AgentHook { .. }
                | Cmd::Hooks { .. }
                | Cmd::Remote { .. }
                | Cmd::Theme { .. }
                | Cmd::Set { .. }
                | Cmd::Reload
        ) {
            return Err(
                "--remote works with commands that talk to a daemon's panes \
                        (agents, list, capture, send, wait, launch, ...) and handoff"
                    .into(),
            );
        }
    }
    match cli.cmd {
        Cmd::Events { pane } => events::run(remote.as_deref(), pane),
        Cmd::Attach { pane } => attach::run(pane, remote.as_deref(), json),
        Cmd::SocketPath => {
            println!("{}", thurm_config::socket_path().display());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::RemoteInfo => remote::remote_info(),
        Cmd::Remote { action } => match action {
            RemoteCmd::Add {
                name,
                target,
                socket,
                yes,
                no,
            } => remote::add(remote::AddOptions {
                name,
                target,
                socket,
                assume: if yes {
                    Some(true)
                } else if no {
                    Some(false)
                } else {
                    None
                },
            }),
            RemoteCmd::List => remote::list(json),
            RemoteCmd::Remove { name } => remote::remove(&name),
            RemoteCmd::Status { name } => remote::status(name.as_deref(), json),
            RemoteCmd::Install { name, yes, no } => {
                remote::install_cmd(&name, if yes { Some(true) } else { no.then_some(false) })
            }
            RemoteCmd::Doctor { name, fix, yes } => {
                remote::doctor_cmd(name.as_deref(), fix, yes.then_some(true), json)
            }
        },
        Cmd::Handoff {
            preset,
            branch,
            path,
            list,
            fetch,
            cleanup,
            force,
        } => remote::handoff_cmd(
            remote::HandoffOptions {
                remote,
                preset,
                branch,
                path,
                list,
                fetch,
                cleanup,
                force,
            },
            json,
        ),
        Cmd::PaneId => {
            println!("{}", current_pane(None)?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::ConfigPath => {
            println!("{}", thurm_config::config_path().display());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Notify { title, body } => {
            let body = body.join(" ");
            let seq = format!(
                "\x1b]777;notify;{};{}\x1b\\",
                sanitize(&title),
                sanitize(&body)
            );
            // Write to the controlling terminal so it works even with stdout redirected.
            match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
                Ok(mut tty) => tty.write_all(seq.as_bytes())?,
                Err(_) => std::io::stdout().write_all(seq.as_bytes())?,
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Daemon { action } => daemon_cmd(&action, json),
        Cmd::AgentHook { agent, event } => {
            agent_hook(agent, event);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Hooks { action, agent } => hooks_cmd(&action, agent.as_deref(), json),
        Cmd::Theme { spec: None } => {
            let cfg = thurm_config::Config::load()?;
            let current = cfg.theme_spec();
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "current": current, "themes": thurm_config::theme_list() })
                );
            } else {
                for t in thurm_config::theme_list() {
                    let mark = if t.name == current.light || t.name == current.dark {
                        "*"
                    } else {
                        " "
                    };
                    let kind = if t.dark { "dark" } else { "light" };
                    println!("{mark} {:<20} {kind}", t.name);
                }
                println!("\ncurrent: {current}");
            }
            Ok(ExitCode::SUCCESS)
        }
        cmd => {
            let client = match &remote {
                Some(name) => remote::connect_remote(name)?,
                None => connect(true)?,
            };
            run_connected(&client, cmd, json)
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() && *c != ';').collect()
}

fn daemon_cmd(action: &str, json: bool) -> R {
    match action {
        "status" => match connect(false) {
            Ok(c) => {
                let hello = c.hello.lock().clone();
                let panes = match c.request(Request::ListPanes)? {
                    Response::Panes(p) => p.len(),
                    _ => 0,
                };
                if let Some(Response::Hello {
                    daemon_pid,
                    restored,
                    build,
                    ..
                }) = hello
                {
                    let current = build == thurm_proto::BUILD;
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"running": true, "pid": daemon_pid, "panes": panes, "restored": restored, "build": build, "current": current})
                        );
                    } else {
                        println!(
                            "thurmd {build} running (pid {daemon_pid}), {panes} panes{}",
                            if restored { ", restored session" } else { "" }
                        );
                        if !current {
                            println!(
                                "this thurm is {}; `thurm daemon upgrade` switches the daemon to it",
                                thurm_proto::BUILD
                            );
                        }
                    }
                }
                Ok(ExitCode::SUCCESS)
            }
            Err(ClientError::Daemon(e)) => {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"running": true, "compatible": false, "error": e})
                    );
                } else {
                    println!(
                        "thurmd running but incompatible ({e}); `thurm daemon upgrade` replaces it"
                    );
                }
                Ok(ExitCode::from(4))
            }
            Err(_) => {
                if json {
                    println!("{}", serde_json::json!({"running": false}));
                } else {
                    println!("thurmd not running");
                }
                Ok(ExitCode::from(3))
            }
        },
        "start" => {
            connect(true)?;
            println!("thurmd running");
            Ok(ExitCode::SUCCESS)
        }
        "stop" => {
            match connect(false) {
                Ok(c) => {
                    c.request(Request::Shutdown { kill_panes: true })?;
                }
                // A daemon from another Thurm version refuses the handshake: signal it.
                Err(ClientError::Daemon(_)) => {
                    let pid = thurm_client::terminate_daemon(&thurm_config::socket_path())?;
                    println!("thurmd (pid {pid}, another version) stopped (session saved)");
                    return Ok(ExitCode::SUCCESS);
                }
                Err(e) => return Err(e.into()),
            }
            println!("thurmd stopped (session saved)");
            Ok(ExitCode::SUCCESS)
        }
        "upgrade" => {
            let daemon = find_daemon().ok_or("thurmd not found next to thurm or on PATH")?;
            let socket = thurm_config::socket_path();
            match thurm_client::upgrade_daemon(&socket, &daemon) {
                Ok(pid) => println!(
                    "thurmd {} running (pid {pid}); panes kept",
                    thurm_proto::BUILD
                ),
                Err(thurm_client::UpgradeError::TooOld(v)) => {
                    let pid = thurm_client::terminate_daemon(&socket)?;
                    connect(true)?;
                    println!(
                        "thurmd (pid {pid}, protocol {v}) predates in-place upgrades: restarted \
                         it (layout, scrollback and working directories restored)"
                    );
                }
                Err(e) => return Err(e.into()),
            }
            Ok(ExitCode::SUCCESS)
        }
        "install-launchd" => {
            let daemon = find_daemon().ok_or("thurmd not found next to thurm or on PATH")?;
            let path = launchd_plist_path();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let log = thurm_config::state_dir().join("thurmd.log");
            // The default socket is left to the daemon to work out at each login (it moved
            // once already); an explicit THURM_SOCKET is passed on.
            let socket = std::env::var_os("THURM_SOCKET").map(std::path::PathBuf::from);
            std::fs::write(&path, launchd_plist(&daemon, socket.as_deref(), &log))?;
            let domain = format!("gui/{}", unsafe { libc_getuid() });
            // Replace a previous registration, then load (starts it now and at every login).
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &domain])
                .arg(&path)
                .status();
            let ok = std::process::Command::new("launchctl")
                .args(["bootstrap", &domain])
                .arg(&path)
                .status()
                .is_ok_and(|s| s.success());
            println!(
                "{} {} ({})",
                LAUNCHD_LABEL,
                if ok {
                    "installed and started"
                } else {
                    "installed; load it with launchctl"
                },
                path.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        "uninstall-launchd" => {
            let path = launchd_plist_path();
            let domain = format!("gui/{}", unsafe { libc_getuid() });
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &domain])
                .arg(&path)
                .status();
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    println!("{LAUNCHD_LABEL} removed; the running daemon keeps its sessions")
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("{LAUNCHD_LABEL} is not installed")
                }
                Err(e) => return Err(e.into()),
            }
            Ok(ExitCode::SUCCESS)
        }
        "install-systemd" => {
            let daemon = find_daemon().ok_or("thurmd not found next to thurm or on PATH")?;
            let path = systemd_unit_path();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let socket = std::env::var_os("THURM_SOCKET").map(std::path::PathBuf::from);
            std::fs::write(&path, systemd_unit(&daemon, socket.as_deref())?)?;
            let systemctl = |args: &[&str]| {
                std::process::Command::new("systemctl")
                    .arg("--user")
                    .args(args)
                    .status()
                    .is_ok_and(|s| s.success())
            };
            if !systemctl(&["daemon-reload"]) {
                return Err(format!(
                    "wrote {}, but systemctl --user daemon-reload failed",
                    path.display()
                )
                .into());
            }
            // Enabled for the next login; a daemon running now keeps serving its sessions.
            if !systemctl(&["enable", SYSTEMD_UNIT]) {
                eprintln!(
                    "thurm: wrote {}, but systemctl --user enable {SYSTEMD_UNIT} failed; \
                     enable it yourself to start thurmd at login",
                    path.display()
                );
                return Ok(ExitCode::FAILURE);
            }
            println!("{SYSTEMD_UNIT} installed and enabled ({})", path.display());
            Ok(ExitCode::SUCCESS)
        }
        "uninstall-systemd" => {
            let path = systemd_unit_path();
            let systemctl = |args: &[&str]| {
                std::process::Command::new("systemctl")
                    .arg("--user")
                    .args(args)
                    .status()
                    .is_ok_and(|s| s.success())
            };
            if !path.exists() {
                println!("{SYSTEMD_UNIT} is not installed");
                return Ok(ExitCode::SUCCESS);
            }
            // Disabled first: removing the file would leave the login symlink behind.
            if !systemctl(&["disable", SYSTEMD_UNIT]) {
                return Err(format!(
                    "systemctl --user disable {SYSTEMD_UNIT} failed; {} is still installed",
                    path.display()
                )
                .into());
            }
            std::fs::remove_file(&path)?;
            if !systemctl(&["daemon-reload"]) {
                return Err("removed the unit, but systemctl --user daemon-reload failed".into());
            }
            println!("{SYSTEMD_UNIT} removed; the running daemon keeps its sessions");
            Ok(ExitCode::SUCCESS)
        }
        _ => unreachable!(),
    }
}

const SYSTEMD_UNIT: &str = "thurmd.service";

fn systemd_unit_path() -> std::path::PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    config.join("systemd/user").join(SYSTEMD_UNIT)
}

/// A user unit that starts `thurmd` at login, in the foreground. Not restarted, so
/// `thurm daemon stop` stops it until the next login (like the macOS LaunchAgent).
fn systemd_unit(daemon: &std::path::Path, socket: Option<&std::path::Path>) -> Result<String, String> {
    let mut exec = format!("{} --foreground", systemd_quote(daemon)?);
    if let Some(s) = socket {
        exec.push_str(&format!(" --socket {}", systemd_quote(s)?));
    }
    Ok(format!(
        "[Unit]\nDescription=Thurm session daemon\n\n[Service]\nType=simple\nExecStart={exec}\nRestart=no\n\n[Install]\nWantedBy=default.target\n"
    ))
}

/// One `ExecStart=` item: double-quoted, with systemd's escapes, `%` specifiers and `$`
/// variable expansion turned off. Control characters are refused: a line break would end the
/// directive and start another.
fn systemd_quote(path: &std::path::Path) -> Result<String, String> {
    // Not lossy: a replaced byte would name another file.
    let s = path
        .to_str()
        .ok_or_else(|| format!("{path:?}: a systemd unit needs a UTF-8 path"))?;
    if s.chars().any(char::is_control) {
        return Err(format!("{path:?}: control characters cannot go in a systemd unit"));
    }
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}

const LAUNCHD_LABEL: &str = "com.thurm.daemon";

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

fn launchd_plist_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

/// A LaunchAgent that starts `thurmd` at login. It runs in the foreground (launchd tracks the
/// pid) and is not kept alive, so `thurm daemon stop` stops it until the next login.
fn launchd_plist(
    daemon: &std::path::Path,
    socket: Option<&std::path::Path>,
    log: &std::path::Path,
) -> String {
    let esc = |p: &std::path::Path| {
        p.display()
            .to_string()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>--foreground</string>{}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
</dict>
</plist>
"#,
        esc(daemon),
        socket.map_or_else(String::new, |s| format!(
            "\n        <string>--socket</string>\n        <string>{}</string>",
            esc(s)
        )),
        esc(log)
    )
}

fn pane_size_default() -> PaneSize {
    let cfg = thurm_config::Config::load().unwrap_or_default();
    PaneSize {
        cols: cfg.window.columns,
        rows: cfg.window.rows,
        ..Default::default()
    }
}

fn run_connected(c: &Client, cmd: Cmd, json: bool) -> R {
    match cmd {
        Cmd::Agent { action } => return agent::run(c, action, json),
        Cmd::List => {
            let panes = list(c)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&panes_json(&panes))?);
            } else {
                let me = std::env::var(ENV_PANE_ID)
                    .ok()
                    .and_then(|v| v.parse::<PaneId>().ok());
                println!(
                    "{:<5} {:<8} {:<9} {:<24} {:<18} CWD",
                    "ID", "PID", "SIZE", "TITLE", "AGENT"
                );
                for p in panes {
                    let agent = p
                        .agent
                        .as_ref()
                        .map(|a| format!("{} ({:?})", a.name, a.status))
                        .unwrap_or_default();
                    println!(
                        "{:<5} {:<8} {:<9} {:<24} {:<18} {}",
                        format!("{}{}", p.id, if Some(p.id) == me { "*" } else { "" }),
                        p.pid.map(|p| p.to_string()).unwrap_or_default(),
                        format!("{}x{}", p.size.cols, p.size.rows),
                        truncate(&p.title, 24),
                        truncate(&agent, 18),
                        p.cwd.unwrap_or_default()
                    );
                }
            }
        }
        Cmd::Info { pane } => {
            let pane = current_pane(pane)?;
            match c.request(Request::PaneInfo { pane })? {
                Response::PaneInfo(p) => {
                    println!("{}", serde_json::to_string_pretty(&pane_json(&p))?)
                }
                other => return Err(format!("unexpected response {other:?}").into()),
            }
        }
        Cmd::NewTab {
            cwd,
            window,
            hold,
            command,
        } => {
            let inherit = current_pane(None).ok();
            let pane = create(c, command, cwd, inherit, hold, None)?;
            ui(
                c,
                UiCommand::NewTab {
                    pane,
                    new_window: window,
                },
            );
            print_pane(pane, json);
        }
        Cmd::Split {
            dir,
            pane,
            cwd,
            hold,
            command,
        } => {
            let target = current_pane(pane).ok();
            let new = create(c, command, cwd, target, hold, None)?;
            ui(
                c,
                UiCommand::Split {
                    target,
                    pane: new,
                    dir: dir.into(),
                },
            );
            print_pane(new, json);
        }
        Cmd::Complete { pane } => {
            let pane = current_pane(pane)?;
            let Response::Completions(c) = c.request(Request::Complete { pane })? else {
                return Err("unexpected response".into());
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "word": c.word,
                        "items": c.items.iter().map(|i| serde_json::json!({
                            "text": i.text, "kind": format!("{:?}", i.kind), "description": i.description,
                        })).collect::<Vec<_>>(),
                    })
                );
            } else {
                println!("word: {:?}", c.word);
                for i in &c.items {
                    println!(
                        "{:<40} {:<10} {}",
                        i.text,
                        format!("{:?}", i.kind),
                        i.description.as_deref().unwrap_or("")
                    );
                }
            }
        }
        Cmd::Procs { pane, ports } => {
            let Response::Processes(list) = c.request(Request::Processes { pane })? else {
                return Err("unexpected response".into());
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!(list
                    .iter()
                    .map(|p| serde_json::json!({
                        "pane": p.pane,
                        "processes": p.processes.iter().map(|e| serde_json::json!({
                            "pid": e.pid, "ppid": e.ppid, "command": e.command, "ports": e.ports,
                        })).collect::<Vec<_>>(),
                    }))
                    .collect::<Vec<_>>()))?
                );
            } else {
                println!("{:<5} {:<7} {:<14} COMMAND", "PANE", "PID", "PORTS");
                for p in &list {
                    for e in p.processes.iter().filter(|e| !ports || !e.ports.is_empty()) {
                        let ps = e
                            .ports
                            .iter()
                            .map(u16::to_string)
                            .collect::<Vec<_>>()
                            .join(",");
                        let cmd: String = e.command.chars().take(100).collect();
                        println!("{:<5} {:<7} {:<14} {cmd}", p.pane, e.pid, ps);
                    }
                }
            }
        }
        Cmd::Fork { pane, split } => {
            let src = current_pane(pane)?;
            let size = pane_info(c, src)
                .map(|i| i.size)
                .unwrap_or_else(pane_size_default);
            let new = match c.request(Request::CreatePane(CreatePane {
                size,
                fork_from: Some(src),
                ..Default::default()
            }))? {
                Response::PaneCreated { pane } => pane,
                other => return Err(format!("unexpected response: {other:?}").into()),
            };
            match split {
                Some(d) => ui(
                    c,
                    UiCommand::Split {
                        target: Some(src),
                        pane: new,
                        dir: d.into(),
                    },
                ),
                None => ui(
                    c,
                    UiCommand::NewTab {
                        pane: new,
                        new_window: false,
                    },
                ),
            }
            print_pane(new, json);
        }
        Cmd::Launch { preset, split, cwd } => {
            let target = current_pane(None).ok();
            let new = create(c, Vec::new(), cwd, target, false, Some(preset))?;
            match split {
                Some(d) => ui(
                    c,
                    UiCommand::Split {
                        target,
                        pane: new,
                        dir: d.into(),
                    },
                ),
                None => ui(
                    c,
                    UiCommand::NewTab {
                        pane: new,
                        new_window: false,
                    },
                ),
            }
            print_pane(new, json);
        }
        Cmd::Presets => {
            if let Response::AgentPresets(p) = c.request(Request::ListAgentPresets)? {
                if json {
                    let v: Vec<_> = p
                        .iter()
                        .map(|p| serde_json::json!({"name": p.name, "command": p.command}))
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else if p.is_empty() {
                    println!(
                        "no agent CLIs found on PATH (configure [agents] presets in the config)"
                    );
                } else {
                    for p in p {
                        println!("{:<20} {}", p.name, p.command.join(" "));
                    }
                }
            }
        }
        Cmd::Send {
            pane,
            no_enter,
            paste,
            text,
        } => {
            let pane = current_pane(pane)?;
            let mut s = if text.len() == 1 && text[0] == "-" {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf
            } else {
                text.join(" ")
            };
            if paste {
                c.request(Request::Paste { pane, text: s })?;
                if !no_enter {
                    c.request(Request::Input {
                        pane,
                        data: b"\r".to_vec(),
                    })?;
                }
            } else {
                if !no_enter {
                    s.push('\r');
                }
                c.request(Request::Input {
                    pane,
                    data: s.into_bytes(),
                })?;
            }
        }
        Cmd::SendKeys { pane, keys } => {
            let pane = current_pane(pane)?;
            let mut data = Vec::new();
            for k in &keys {
                data.extend(key_bytes(k).ok_or_else(|| format!("unknown key {k:?}"))?);
            }
            c.request(Request::Input { pane, data })?;
        }
        Cmd::Explain { pane } => {
            let pane = current_pane(pane)?;
            if let Response::Text(t) = c.request(Request::Explain { pane })? {
                if json {
                    println!("{}", serde_json::json!({"pane": pane, "text": t}));
                } else {
                    println!("{t}");
                }
            }
        }
        Cmd::Capture {
            pane,
            lines,
            scrollback,
            ansi,
        } => {
            let pane = current_pane(pane)?;
            if let Response::Text(t) = c.request(Request::Capture {
                pane,
                opts: CaptureOpts {
                    lines,
                    scrollback,
                    ansi,
                },
            })? {
                if json {
                    println!("{}", serde_json::json!({"pane": pane, "text": t}));
                } else {
                    print!("{t}");
                }
            }
        }
        Cmd::Wait {
            pane,
            idle,
            prompt,
            exit,
            r#match,
            agent_free,
            agent_done,
            agent_waiting,
            timeout,
        } => {
            let pane = current_pane(pane)?;
            let until = if let Some(ms) = idle {
                WaitCondition::Idle { quiet_ms: ms }
            } else if prompt {
                WaitCondition::Prompt
            } else if exit {
                WaitCondition::Exit
            } else if let Some(regex) = r#match {
                WaitCondition::Match { regex }
            } else if agent_free {
                WaitCondition::AgentFree
            } else if agent_done {
                WaitCondition::AgentStatus(thurm_proto::AgentStatus::Done)
            } else if agent_waiting {
                WaitCondition::AgentStatus(thurm_proto::AgentStatus::NeedsInput)
            } else {
                WaitCondition::Idle { quiet_ms: 1000 }
            };
            let timeout_ms = timeout.map(|s| (s * 1000.0) as u64);
            let outcome = match c.request(Request::Wait {
                pane,
                until,
                timeout_ms,
            })? {
                Response::Wait(o) => o,
                other => return Err(format!("unexpected response {other:?}").into()),
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({"pane": pane, "outcome": format!("{outcome:?}")})
                );
            }
            return Ok(match outcome {
                WaitOutcome::Satisfied => ExitCode::SUCCESS,
                WaitOutcome::Timeout => {
                    if !json {
                        eprintln!("thurm: timed out");
                    }
                    ExitCode::from(124)
                }
                WaitOutcome::Exited { code } => {
                    if !json {
                        eprintln!(
                            "thurm: pane exited ({})",
                            code.map(|c| c.to_string()).unwrap_or("?".into())
                        );
                    }
                    if exit {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(2)
                    }
                }
            });
        }
        Cmd::Agents => {
            let panes: Vec<PaneInfo> = list(c)?.into_iter().filter(|p| p.agent.is_some()).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&panes_json(&panes))?);
            } else if panes.is_empty() {
                println!("no agents running");
            } else {
                for p in panes {
                    let a = p.agent.unwrap();
                    println!(
                        "{:<5} {:<16} {:<11} {}",
                        p.id,
                        a.name,
                        format!("{:?}", a.status),
                        p.cwd.unwrap_or_default()
                    );
                }
            }
        }
        Cmd::Focus { pane } => {
            c.request(Request::Ui(UiCommand::Focus { pane }))?;
        }
        Cmd::Close { pane } => {
            let pane = current_pane(pane)?;
            c.request(Request::ClosePane { pane })?;
        }
        Cmd::Title { pane, title } => {
            let pane = current_pane(pane)?;
            let t = title.join(" ");
            c.request(Request::Ui(UiCommand::SetTabTitle {
                pane,
                title: (!t.is_empty()).then_some(t),
            }))?;
        }
        Cmd::Workspace {
            action: WorkspaceCmd::Rename { pane, name },
        } => {
            let pane = current_pane(pane)?;
            let name = name.join(" ").trim().to_owned();
            if name.is_empty() {
                return Err("workspace name is empty".into());
            }
            c.request(Request::Ui(UiCommand::RenameWorkspace { pane, name }))?;
        }
        Cmd::Scroll { pane, to } => {
            let pane = current_pane(pane)?;
            let scroll = match to.as_str() {
                "up" => ScrollCmd::PageUp,
                "down" => ScrollCmd::PageDown,
                "top" => ScrollCmd::Top,
                "bottom" => ScrollCmd::Bottom,
                "prev-prompt" => ScrollCmd::PrevPrompt,
                _ => ScrollCmd::NextPrompt,
            };
            c.request(Request::Scroll { pane, scroll })?;
        }
        Cmd::Clear { pane } => {
            let pane = current_pane(pane)?;
            c.request(Request::ClearScrollback { pane })?;
        }
        Cmd::Layout { action } => return layouts::run(c, action),
        Cmd::Reload => {
            c.request(Request::ReloadConfig)?;
            println!("configuration reloaded");
        }
        Cmd::Set { key, value } => {
            let literal = thurm_config::value_literal(&value);
            let resp = c.request(Request::SetSetting {
                key: key.clone(),
                value: literal,
            })?;
            println!("{key} = {value}");
            // The daemon writes its own config file; say so when it isn't ours.
            if let Response::Text(path) = resp
                && std::path::Path::new(&path) != thurm_config::config_path()
            {
                println!("(in {path}, the running daemon's config file)");
            }
        }
        Cmd::Theme { spec: Some(spec) } => {
            c.request(Request::SetTheme { spec: spec.clone() })?;
            println!("theme set to {spec}");
        }
        Cmd::Save => {
            c.request(Request::SaveSnapshot)?;
            println!("session saved");
        }
        Cmd::Attach { .. }
        | Cmd::PaneId
        | Cmd::ConfigPath
        | Cmd::SocketPath
        | Cmd::RemoteInfo
        | Cmd::Remote { .. }
        | Cmd::Handoff { .. }
        | Cmd::Notify { .. }
        | Cmd::Daemon { .. }
        | Cmd::Events { .. }
        | Cmd::AgentHook { .. }
        | Cmd::Hooks { .. }
        | Cmd::Theme { spec: None } => unreachable!(),
    }
    Ok(ExitCode::SUCCESS)
}

fn ui(c: &Client, cmd: UiCommand) {
    let Err(e) = c.request(Request::Ui(cmd.clone())) else {
        return;
    };
    // No window is open: start the app.
    #[cfg(target_os = "macos")]
    let launched = std::process::Command::new("open")
        .args(["-b", "com.thurm.terminal"])
        .status()
        .is_ok_and(|s| s.success());
    #[cfg(target_os = "linux")]
    let launched = (std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some())
        && std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("thurm-gtk")))
            .filter(|p| p.is_file())
            .or_else(|| thurm_config::which("thurm-gtk"))
            .is_some_and(|app| {
                std::process::Command::new(app)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .is_ok()
            });
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let launched = false;
    // The app adopts panes it doesn't know as a background workspace: send the command again
    // once it is up, so the pane goes where it says (its own workspace, a tab).
    if launched {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if c.request(Request::Ui(cmd.clone())).is_ok() {
                return;
            }
        }
    }
    eprintln!("thurm: pane created but not shown: {e}");
}

fn pane_info(c: &Client, pane: PaneId) -> Option<PaneInfo> {
    match c.request(Request::PaneInfo { pane }).ok()? {
        Response::PaneInfo(i) => Some(i),
        _ => None,
    }
}

fn create(
    c: &Client,
    command: Vec<String>,
    cwd: Option<String>,
    inherit: Option<PaneId>,
    hold: bool,
    agent_preset: Option<String>,
) -> Result<PaneId, Box<dyn std::error::Error>> {
    let cwd = cwd.or_else(|| {
        // Outside Thurm, default to the caller's directory (not a path on a remote host).
        (inherit.is_none() && !remote_mode())
            .then(|| {
                std::env::current_dir()
                    .ok()
                    .map(|d| d.display().to_string())
            })
            .flatten()
    });
    let req = CreatePane {
        command: (!command.is_empty()).then_some(command),
        cwd,
        env: Vec::new(),
        size: pane_size_default(),
        agent_preset,
        inherit_cwd_from: inherit,
        hold,
        fork_from: None,
    };
    match c.request(Request::CreatePane(req))? {
        Response::PaneCreated { pane } => Ok(pane),
        other => Err(format!("unexpected response {other:?}").into()),
    }
}

fn print_pane(pane: PaneId, json: bool) {
    if json {
        println!("{}", serde_json::json!({"pane": pane}));
    } else {
        println!("{pane}");
    }
}

fn list(c: &Client) -> Result<Vec<PaneInfo>, ClientError> {
    match c.request(Request::ListPanes)? {
        Response::Panes(p) => Ok(p),
        _ => Ok(Vec::new()),
    }
}

fn pane_json(p: &PaneInfo) -> serde_json::Value {
    serde_json::json!({
        "id": p.id,
        "title": p.title,
        "cwd": p.cwd,
        "pid": p.pid,
        "alive": p.alive,
        "exit_code": p.exit_code,
        "cols": p.size.cols,
        "rows": p.size.rows,
        "foreground": p.foreground.as_ref().map(|f| serde_json::json!({"pid": f.pid, "name": f.name, "argv": f.argv})),
        "agent": p.agent.as_ref().map(|a| serde_json::json!({"name": a.name, "kind": a.kind, "status": format!("{:?}", a.status), "topic": a.topic})),
        "at_prompt": p.at_prompt,
        "last_exit_status": p.last_exit_status,
        "idle_ms": p.idle_ms,
        "password_input": p.password_input,
        "restored": p.restored,
        "programs": p.programs,
    })
}

fn panes_json(panes: &[PaneInfo]) -> serde_json::Value {
    serde_json::Value::Array(panes.iter().map(pane_json).collect())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('…');
        t
    }
}

/// tmux-style key names to bytes.
fn key_bytes(k: &str) -> Option<Vec<u8>> {
    let named: &[(&str, &[u8])] = &[
        ("Enter", b"\r"),
        ("Return", b"\r"),
        ("Tab", b"\t"),
        ("BTab", b"\x1b[Z"),
        ("Escape", b"\x1b"),
        ("Esc", b"\x1b"),
        ("Space", b" "),
        ("Backspace", b"\x7f"),
        ("BSpace", b"\x7f"),
        ("Up", b"\x1b[A"),
        ("Down", b"\x1b[B"),
        ("Right", b"\x1b[C"),
        ("Left", b"\x1b[D"),
        ("Home", b"\x1b[H"),
        ("End", b"\x1b[F"),
        ("PageUp", b"\x1b[5~"),
        ("PPage", b"\x1b[5~"),
        ("PageDown", b"\x1b[6~"),
        ("NPage", b"\x1b[6~"),
        ("Delete", b"\x1b[3~"),
        ("DC", b"\x1b[3~"),
        ("Insert", b"\x1b[2~"),
    ];
    if let Some((_, b)) = named.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)) {
        return Some(b.to_vec());
    }
    if let Some(rest) = k.strip_prefix("C-").or_else(|| k.strip_prefix("c-")) {
        let mut chars = rest.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            return None;
        };
        let b = match ch.to_ascii_lowercase() {
            c @ 'a'..='z' => c as u8 - b'a' + 1,
            ' ' | '@' | '2' => 0,
            '[' => 0x1b,
            '\\' => 0x1c,
            ']' => 0x1d,
            '^' | '6' => 0x1e,
            '_' | '-' | '/' => 0x1f,
            '?' => 0x7f,
            _ => return None,
        };
        return Some(vec![b]);
    }
    if let Some(rest) = k.strip_prefix("M-") {
        let mut v = vec![0x1b];
        v.extend(key_bytes(rest).unwrap_or_else(|| rest.as_bytes().to_vec()));
        return Some(v);
    }
    // Literal text.
    Some(k.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_quotes_paths() {
        let daemon = std::path::Path::new("/opt/My Apps/thurm \"x\"/thurmd");
        let socket = std::path::Path::new("/run/a b/100%$HOME\\.sock");
        let unit = systemd_unit(daemon, Some(socket)).unwrap();
        assert!(unit.contains(
            "ExecStart=\"/opt/My Apps/thurm \\\"x\\\"/thurmd\" --foreground \
             --socket \"/run/a b/100%%$$HOME\\\\.sock\"\n"
        ));
        assert!(systemd_unit(std::path::Path::new("/usr/bin/thurmd"), None)
            .unwrap()
            .contains("ExecStart=\"/usr/bin/thurmd\" --foreground\n"));
        let injected = std::path::Path::new("/tmp/x\nExecStartPre=/bin/evil");
        assert!(systemd_unit(daemon, Some(injected)).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let latin1 = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9/thurmd"));
            assert!(systemd_unit(latin1, None).is_err());
        }
    }

    #[test]
    fn launchd_plist_is_valid() {
        let daemon = std::path::Path::new("/Applications/Thurm & Co.app/Contents/Helpers/thurmd");
        let log = std::path::Path::new("/tmp/log");
        let explicit = launchd_plist(daemon, Some(std::path::Path::new("/x/thurmd.sock")), log);
        assert!(explicit.contains("<string>--socket</string>"));
        // The default socket is left to the daemon.
        let default = launchd_plist(daemon, None, log);
        assert!(!default.contains("--socket"));
        let dir = std::env::temp_dir().join(format!("thurm-plist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for s in [explicit, default] {
            assert!(s.contains("Thurm &amp; Co.app"));
            assert!(s.contains("<string>--foreground</string>"));
            let f = dir.join("x.plist");
            std::fs::write(&f, &s).unwrap();
            let ok = std::process::Command::new("plutil")
                .arg("-lint")
                .arg(&f)
                .output();
            if let Ok(out) = ok {
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stdout)
                );
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keys() {
        assert_eq!(key_bytes("C-c"), Some(vec![3]));
        assert_eq!(key_bytes("enter"), Some(b"\r".to_vec()));
        assert_eq!(key_bytes("M-x"), Some(b"\x1bx".to_vec()));
        assert_eq!(key_bytes("M-Up"), Some(b"\x1b\x1b[A".to_vec()));
        assert_eq!(key_bytes("hello"), Some(b"hello".to_vec()));
        assert_eq!(key_bytes("C-ab"), None);
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let c = Cli::try_parse_from(["thurm", "split", "--dir", "down", "--", "htop", "-d", "5"])
            .unwrap();
        match c.cmd {
            Cmd::Split { command, .. } => assert_eq!(command, vec!["htop", "-d", "5"]),
            _ => panic!(),
        }
        assert!(Cli::try_parse_from(["thurm", "wait", "--agent-free", "-t", "30"]).is_ok());
        let c = Cli::try_parse_from(["thurm", "--remote", "devbox", "agents"]).unwrap();
        assert_eq!(c.remote.as_deref(), Some("devbox"));
        let c = Cli::try_parse_from([
            "thurm",
            "wait",
            "--remote",
            "devbox",
            "--agent-done",
            "-p",
            "3",
        ])
        .unwrap();
        assert_eq!(c.remote.as_deref(), Some("devbox"));
        assert!(
            Cli::try_parse_from(["thurm", "remote", "add", "devbox", "me@devbox", "-y"]).is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "thurm", "handoff", "--remote", "devbox", "--preset", "claude", "--branch",
                "agent/x"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["thurm", "handoff", "--list", "--fetch", "x"]).is_err());
    }

    #[test]
    fn flags_that_need_another_flag() {
        use clap::Parser;
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("thurm").chain(args.iter().copied()))
        };
        assert!(parse(&["remote", "doctor", "--yes"]).is_err());
        assert!(parse(&["remote", "doctor", "--fix", "--yes"]).is_ok());
        assert!(parse(&["handoff", "--force"]).is_err());
        assert!(parse(&["handoff", "--cleanup", "x", "--force"]).is_ok());
        assert!(parse(&["remote", "install", "box", "--no"]).is_ok());
        assert!(parse(&["remote", "install", "box", "--yes", "--no"]).is_err());
    }
}
