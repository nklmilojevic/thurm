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
use thurm_remote::add::{self, AddError, Address, Candidate};
use thurm_remote::doctor::{self, Report, State};
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
            format!("no remote named {name:?} (add one with `thurm remote add {name} <address>`)")
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

/// Asks on the terminal. Without one (and without `--yes`/`--no`) there is nobody to answer:
/// that is an error naming the question, not a silent "no".
fn ask(question: &str, default_yes: bool, assume: Option<bool>) -> Result<bool, String> {
    if let Some(a) = assume {
        return Ok(a);
    }
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "{question}\n(not a terminal, so nobody can answer: rerun with --yes or --no, or in a terminal)"
        ));
    }
    print!("{question} [{}] ", if default_yes { "Y/n" } else { "y/N" });
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return Ok(false);
    }
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    })
}

/// `thurm` and `thurmd` next to this executable (the app's Contents/Helpers, or the
/// install directory), for installing on a Mac.
fn local_bins() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Tells the app (through the local daemon) that the config changed. Returns whether a Thurm
/// window is open to pick it up.
fn reload_app() -> bool {
    match Client::connect(
        ConnectOptions {
            client_name: "thurm-cli",
            ..Default::default()
        },
        |_| {},
        || {},
    ) {
        Ok(c) => {
            let _ = c.request(Request::ReloadConfig);
            c.request(Request::CheckUi).is_ok()
        }
        Err(_) => false,
    }
}

/// The last thing `remote add` says: where the new host shows up.
fn added_sentence(name: &str, app_open: bool) -> String {
    if app_open {
        format!(
            "Thurm connects to {name} now, and opens a workspace on it in the sidebar once it is \
             connected."
        )
    } else {
        format!(
            "Thurm connects to {name} when it next opens, and opens a workspace on it in the \
             sidebar then."
        )
    }
}

pub struct AddOptions {
    pub name: String,
    /// `user@host`, an ssh `Host` alias, `ssh://…`, a `*.ts.net` name, or a `wss://` or `ws://`
    /// address. With `--tailnet`, the ssh target.
    pub address: String,
    pub socket: Option<String>,
    /// `--tailnet`: `Some("")` without a value (the target is the peer's name).
    pub tailnet: Option<String>,
    /// Answer every question (`--yes` / `--no`), for scripts.
    pub assume: Option<bool>,
}

/// An add that failed: its sentence, then what was said.
fn failed(e: AddError) -> Box<dyn std::error::Error> {
    e.with_detail().into()
}

pub fn add(o: AddOptions) -> R {
    thurm_config::validate_remote_name(&o.name)?;
    let cfg = Config::load()?;
    if cfg.remote(&o.name).is_some() {
        return Err(format!("a remote named {:?} exists already", o.name).into());
    }
    if let Some(tailnet) = &o.tailnet {
        return add_tailnet(&o, tailnet, &cfg.remote);
    }
    match add::parse_address(&o.address)? {
        Address::Ssh(target) => add_ssh(&o, &target, &cfg.remote),
        Address::Tailnet(address) => add_tailnet_only(&o, &address, &cfg.remote),
    }
}

/// `thurm remote add NAME user@host`: over ssh, with the install offer and the doctor.
fn add_ssh(o: &AddOptions, target: &str, saved: &[RemoteConfig]) -> R {
    thurm_remote::ssh::prepare_dir()?;
    let ssh = Ssh::new(target)?;
    let classify = |e: thurm_remote::SshError| failed(add::classify_ssh(target, &e));
    println!("Connecting to {target} …");
    let effective = ssh.effective_config().map_err(classify)?;
    thurm_remote::ssh::check_forwards(&effective)?;
    let plan =
        install::plan(&ssh, o.socket.as_deref(), local_bins().as_deref()).map_err(classify)?;
    let host = &plan.host;
    println!("{}: {} {}", o.name, host.os, host.arch);
    let mut candidate = Candidate {
        ssh_target: Some(target.to_owned()),
        tailnet: None,
        daemon_id: host.info.as_ref().and_then(|i| i.daemon_id.clone()),
    };
    // Before installing anything on a host that would be refused.
    add::check_new(&o.name, &candidate, saved).map_err(failed)?;
    let installed = install_flow(&ssh, &o.name, o.socket.as_deref(), plan.clone(), o.assume)?;
    if candidate.daemon_id.is_none() {
        candidate.daemon_id = installed.and_then(|i| i.daemon_id);
        add::check_new(&o.name, &candidate, saved).map_err(failed)?;
    }

    let entry = RemoteConfig {
        name: o.name.clone(),
        host: target.to_owned(),
        socket: o.socket.clone(),
        enabled: true,
        clipboard_read: ClipboardRead::Ask,
        tailnet: None,
        forward_links: true,
        daemon_id: candidate.daemon_id,
    };
    let file = thurm_config::edit_remotes(|text| thurm_config::with_remote_added(text, &entry))?;
    println!("Added [[remote]] {} to {}", o.name, file.display());
    mark_added(&o.name);

    // The rest the host needs (lingering, PATH, agents, hooks), fixed where you agree.
    let report = doctor::run(&ssh, &o.name, o.socket.as_deref(), local_bins().as_deref())?;
    print_report(&o.name, target, &report);
    if !settled(&report) {
        fix_flow(&ssh, &o.name, o.socket.as_deref(), &report, o.assume)?;
    }
    println!("{}", added_sentence(&o.name, reload_app()));
    Ok(ExitCode::SUCCESS)
}

/// The app opens a first workspace on the host once it connects (see
/// [`thurm_config::remote_added_path`]).
fn mark_added(name: &str) {
    if let Err(e) = thurm_config::mark_remote_added(name) {
        eprintln!("thurm: {name} gets no workspace of its own when it connects: {e}");
    }
}

/// `thurm remote add NAME bastion.tail1234.ts.net` (or a `wss://` or `ws://` address): a daemon
/// serving on the tailnet, with no ssh target.
fn add_tailnet_only(o: &AddOptions, address: &str, saved: &[RemoteConfig]) -> R {
    println!("Connecting to {address} over the tailnet …");
    let checked = thurm_remote::tailnet::check(
        address,
        &thurm_config::tailnet_dial_socket_path(),
        thurm_remote::tailnet::CONNECT_TIMEOUT,
    )
    .map_err(|e| failed(add::classify_tailnet(address, e)))?;
    println!("{}: {} ({})", o.name, checked.url, checked.via.label());
    // Without ssh, nothing here can install a build it could talk to.
    if let Some(e) = add::protocol_problem(address, checked.protocol, &checked.build) {
        return Err(failed(e));
    }
    let candidate = add::tailnet_candidate(&checked);
    add::check_new(&o.name, &candidate, saved).map_err(failed)?;
    let entry = RemoteConfig {
        name: o.name.clone(),
        host: String::new(),
        socket: o.socket.clone(),
        enabled: true,
        clipboard_read: ClipboardRead::Ask,
        tailnet: Some(checked.url.clone()),
        forward_links: true,
        daemon_id: candidate.daemon_id,
    };
    let file = thurm_config::edit_remotes(|text| thurm_config::with_remote_added(text, &entry))?;
    println!("Added [[remote]] {} to {}", o.name, file.display());
    mark_added(&o.name);
    println!("{}", added_sentence(&o.name, reload_app()));
    Ok(ExitCode::SUCCESS)
}

/// `thurm remote add NAME TARGET --tailnet [PEER|URL]`: checks the daemon answers over the
/// tailnet and writes the entry. Installing and the doctor stay ssh's (`thurm remote install`).
fn add_tailnet(o: &AddOptions, tailnet: &str, saved: &[RemoteConfig]) -> R {
    let target = o.address.trim();
    let value = if tailnet.is_empty() {
        thurm_remote::tailnet::peer_name(target)
    } else {
        tailnet
    };
    Ssh::new(target)?;
    println!("Connecting to {value} over the tailnet …");
    let checked = thurm_remote::tailnet::check(
        value,
        &thurm_config::tailnet_dial_socket_path(),
        thurm_remote::tailnet::CONNECT_TIMEOUT,
    )
    .map_err(|e| failed(add::classify_tailnet(value, e)))?;
    println!("{}: {} ({})", o.name, checked.url, checked.via.label());
    let candidate = Candidate {
        ssh_target: Some(target.to_owned()),
        ..add::tailnet_candidate(&checked)
    };
    add::check_new(&o.name, &candidate, saved).map_err(failed)?;
    let entry = RemoteConfig {
        name: o.name.clone(),
        host: target.to_owned(),
        socket: o.socket.clone(),
        enabled: true,
        clipboard_read: ClipboardRead::Ask,
        tailnet: Some(checked.url.clone()),
        forward_links: true,
        daemon_id: candidate.daemon_id,
    };
    let file = thurm_config::edit_remotes(|text| thurm_config::with_remote_added(text, &entry))?;
    println!("Added [[remote]] {} to {}", o.name, file.display());
    mark_added(&o.name);
    if checked.protocol != thurm_proto::PROTOCOL_VERSION || checked.build.is_empty() {
        println!(
            "note: its daemon speaks protocol {}, this Thurm {}: install this build there \
             over ssh with `thurm remote install {}`",
            checked.protocol,
            thurm_proto::PROTOCOL_VERSION,
            o.name
        );
    }
    println!("{}", added_sentence(&o.name, reload_app()));
    Ok(ExitCode::SUCCESS)
}

/// `thurm remote edit NAME --ssh TARGET`: gives the host an ssh target (a host added over the
/// tailnet has none), or another one.
pub fn edit(name: &str, ssh: &str) -> R {
    remote_config(name)?;
    let target = ssh.trim();
    Ssh::new(target)?;
    let candidate = Candidate {
        ssh_target: Some(target.to_owned()),
        ..Default::default()
    };
    add::check_edit(name, &candidate, &Config::load()?.remote).map_err(failed)?;
    let literal = serde_json::to_string(target)?;
    thurm_config::edit_remotes(|text| {
        thurm_config::with_remote_setting(text, name, "host", &literal)
    })?;
    reload_app();
    println!("{name}'s ssh target is {target}.");
    Ok(ExitCode::SUCCESS)
}

/// The host's ssh target, or its tailnet address when it has none.
fn host_label(r: &RemoteConfig) -> &str {
    r.ssh_target()
        .or(r.tailnet.as_deref())
        .unwrap_or("no address")
}

/// How a host is reached, for lists.
fn transport_label(r: &RemoteConfig, s: Option<&Status>) -> String {
    if r.tailnet.is_none() {
        return "ssh".into();
    }
    match s.and_then(|s| s.via) {
        Some(thurm_remote::tailnet::Via::Node) => "tailnet (node)".into(),
        Some(thurm_remote::tailnet::Via::System) => "tailnet (app)".into(),
        None => "tailnet".into(),
    }
}

/// Offers to install our build (and replace the daemon) when the host needs it. Returns what was
/// installed.
fn install_flow(
    ssh: &Ssh,
    name: &str,
    socket: Option<&str>,
    plan: install::Plan,
    assume: Option<bool>,
) -> Result<Option<ThurmInfo>, Box<dyn std::error::Error>> {
    let mut installed = None;
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
        )? {
            return Err(format!("{name} needs Thurm {} to connect", thurm_proto::BUILD).into());
        }
        let info: ThurmInfo = install::install(ssh, host, method, local_bins().as_deref())?;
        println!("Installed thurm {} on {name}.", info.build);
        installed = Some(info);
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
        // --yes doesn't stop programs: a restart is asked in person, or left for later.
        let assume = if restart {
            assume.filter(|a| !a)
        } else {
            assume
        };
        if restart && assume.is_none() && !std::io::stdin().is_terminal() {
            println!("{question}\n(not done: a restart is only done when asked in a terminal)");
        } else if ask(&question, !restart, assume)? {
            let out = install::upgrade_daemon(ssh, socket, Some(d), restart)?;
            println!("{out}");
        }
    }
    Ok(installed)
}

/// Nothing left to offer: every problem is fixed or has no fix at all.
fn settled(r: &Report) -> bool {
    r.checks.iter().all(|c| {
        matches!(c.state, State::Ok | State::Skip) || (c.fix.is_none() && c.terminal.is_none())
    })
}

fn print_report(name: &str, target: &str, r: &Report) {
    println!("{name} ({target})");
    let width = r.checks.iter().map(|c| c.title.len()).max().unwrap_or(0);
    for c in &r.checks {
        let mark = match c.state {
            State::Ok => "✓",
            State::Warn => "!",
            State::Fail => "✗",
            State::Skip => "-",
        };
        println!("  {mark} {:<width$}  {}", c.title, c.detail);
    }
}

/// Runs `script` on the host in this terminal (`ssh -t`).
fn run_in_terminal(ssh: &Ssh, script: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let argv = ssh.interactive_argv(script)?;
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()?;
    Ok(status.success())
}

/// Offers the fix for every problem of `report`, then checks again: a fix can bring up new
/// checks (installing an agent adds its sign-in and hooks), offered in the next round.
fn fix_flow(
    ssh: &Ssh,
    name: &str,
    socket: Option<&str>,
    report: &Report,
    assume: Option<bool>,
) -> Result<Report, Box<dyn std::error::Error>> {
    let tty = std::io::stdin().is_terminal();
    let bins = local_bins();
    let mut later: Vec<(String, String)> = Vec::new();
    let mut offered = std::collections::HashSet::new();
    let mut report = report.clone();
    for _ in 0..3 {
        let pending: Vec<_> = report
            .checks
            .iter()
            .filter(|c| !matches!(c.state, State::Ok | State::Skip))
            .filter(|c| offered.insert(c.id.clone()))
            .cloned()
            .collect();
        if pending.is_empty() {
            break;
        }
        for c in &pending {
            if offer_fix(ssh, name, socket, c, assume, tty, bins.as_deref())? {
                continue;
            }
            if let Some(script) = &c.terminal {
                later.push((c.title.clone(), script.clone()));
            }
        }
        report = doctor::run(ssh, name, socket, bins.as_deref())?;
    }
    println!();
    print_report(name, &ssh.target, &report);
    if !later.is_empty() {
        println!("\nThese need you at a terminal on {name}:");
        for (title, script) in later {
            println!("  {title}: ssh -t {} '{script}'", ssh.target);
        }
    }
    Ok(report)
}

/// Offers `c`'s fix, then its terminal script (in person only: --yes and --no leave that
/// for later). Returns whether it was taken care of.
fn offer_fix(
    ssh: &Ssh,
    name: &str,
    socket: Option<&str>,
    c: &doctor::Check,
    assume: Option<bool>,
    tty: bool,
    bins: Option<&Path>,
) -> Result<bool, Box<dyn std::error::Error>> {
    if let Some(f) = &c.fix {
        let question = match &f.confirm {
            Some(q) => format!("{} {q} {}?", c.title, f.label),
            None => format!("{}: {}?", c.title, f.label),
        };
        // Answered for scripts by --yes/--no; with neither and no terminal, only listed. A
        // fix that restarts a daemon (stopping programs) or runs an installer script is not
        // covered by --yes: it is asked in person, or left for later.
        let destructive = f.confirm.is_some() || c.id.starts_with("agent.");
        let yes = match (assume, tty) {
            (Some(true), true) if destructive => ask(&question, false, None)?,
            (Some(true), false) if destructive => {
                println!("  {}: not done with --yes; run it in a terminal", c.title);
                false
            }
            (Some(a), _) => a,
            (None, true) => ask(&question, true, None)?,
            (None, false) => false,
        };
        if yes {
            let restart = f.confirm.is_some();
            match doctor::fix(ssh, socket, bins, &c.id, restart) {
                Ok(out) => {
                    let out = if out.is_empty() { "done" } else { &out };
                    println!("  {}: {out}", c.title);
                    return Ok(true);
                }
                Err(e) => eprintln!("thurm: {}: {e}", c.title),
            }
        }
    }
    let Some(script) = &c.terminal else {
        return Ok(false);
    };
    if tty
        && assume.is_none()
        && ask(
            &format!("{}: run `{script}` on {name} here?", c.title),
            true,
            None,
        )?
    {
        if !run_in_terminal(ssh, script)? {
            eprintln!("thurm: {}: the command failed", c.title);
        }
        return Ok(true);
    }
    Ok(false)
}

pub fn doctor_cmd(name: Option<&str>, fix: bool, assume: Option<bool>, json: bool) -> R {
    let names: Vec<String> = match name {
        Some(n) => vec![remote_config(n)?.name],
        None => Config::load()?
            .remote
            .iter()
            .map(|r| r.name.clone())
            .collect(),
    };
    if json && fix {
        // Fixes ask and report as they go, which JSON output can't carry.
        return Err(
            "--fix does not combine with --json: check with --json, then fix without it".into(),
        );
    }
    if names.is_empty() {
        println!("no remotes (add one with `thurm remote add <name> <address>`)");
        return Ok(ExitCode::SUCCESS);
    }
    thurm_remote::ssh::prepare_dir()?;
    let mut healthy = true;
    let mut reports = Vec::new();
    for (i, n) in names.iter().enumerate() {
        let r = remote_config(n)?;
        let ssh = match thurm_remote::ssh_for(&r) {
            Ok(ssh) => ssh,
            // Asked for by name: that host can't be checked. Among all of them: it is skipped.
            Err(e) if name.is_some() => return Err(e.into()),
            Err(e) => {
                if json {
                    reports.push(serde_json::json!({"name": n, "skipped": e}));
                } else {
                    if i > 0 {
                        println!();
                    }
                    println!("{n} ({})\n  - {e}", host_label(&r));
                }
                continue;
            }
        };
        let mut report = match doctor::run(&ssh, n, r.socket.as_deref(), local_bins().as_deref()) {
            Ok(rep) => rep,
            Err(e) => {
                healthy = false;
                if json {
                    reports.push(serde_json::json!({"name": n, "error": e.to_string()}));
                } else {
                    println!("{n} ({})\n  ✗ ssh  {e}", host_label(&r));
                }
                continue;
            }
        };
        if json {
            reports.push(serde_json::json!({"name": n, "report": report}));
            healthy &= report.healthy();
            continue;
        }
        if i > 0 {
            println!();
        }
        print_report(n, &ssh.target, &report);
        if fix && !settled(&report) {
            println!();
            report = fix_flow(&ssh, n, r.socket.as_deref(), &report, assume)?;
            reload_app();
        } else if !settled(&report) {
            println!("  fix: thurm remote doctor {n} --fix");
        }
        healthy &= report.healthy();
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    }
    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub fn install_cmd(name: &str, assume: Option<bool>) -> R {
    let r = remote_config(name)?;
    let ssh = thurm_remote::ssh_for(&r)?;
    thurm_remote::ssh::prepare_dir()?;
    let plan = install::plan(&ssh, r.socket.as_deref(), local_bins().as_deref())?;
    install_flow(&ssh, name, r.socket.as_deref(), plan, assume)?;
    reload_app();
    Ok(ExitCode::SUCCESS)
}

pub fn remove(name: &str) -> R {
    thurm_config::edit_remotes(|text| thurm_config::with_remote_removed(text, name))?;
    thurm_config::take_remote_added(name);
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
                    "name": r.name, "host": r.host, "enabled": r.enabled, "tailnet": r.tailnet,
                    "state": s.as_ref().map(|s| s.phase.label()),
                    "status": s,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(ExitCode::SUCCESS);
    }
    if rows.is_empty() {
        println!("no remotes (add one with `thurm remote add <name> <address>`)");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<16} {:<32} {:<15} {:<16} BUILD",
        "NAME", "HOST", "VIA", "STATE"
    );
    for (r, s) in rows {
        let state = match (&s, r.enabled) {
            (_, false) => "disabled".to_owned(),
            (Some(s), _) => s.phase.label().to_owned(),
            (None, _) => "app not running".to_owned(),
        };
        let via = transport_label(&r, s.as_ref());
        let build = s.and_then(|s| s.remote_build).unwrap_or_default();
        println!(
            "{:<16} {:<32} {:<15} {:<16} {build}",
            r.name,
            r.ssh_target().unwrap_or("-"),
            via,
            state
        );
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
            serde_json::json!({"name": r.name, "host": r.host, "tailnet": r.tailnet, "status": s})
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("{} ({})", r.name, host_label(&r));
    if let Some(url) = &r.tailnet {
        println!("  via:      {} {url}", transport_label(&r, s.as_ref()));
    }
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
            ssh: thurm_remote::ssh_for(&r)?,
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
    let host = SshHost {
        name: r.name.clone(),
        ssh: thurm_remote::ssh_for(&r)?,
    };
    // The agent's tab opens through the app: it must be connected.
    let client = connect_remote(&name)?;
    let path = match o.path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    // Recorded as soon as the worktree exists on the host: findable for --fetch and
    // --cleanup even when its pane never starts.
    let mut h = handoff::prepare_registered(&host, &path, o.branch.as_deref(), &reg)?;
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

/// `thurm remote ports NAME`: each pane's listening ports and what listens.
pub fn ports_cmd(name: &str, json: bool) -> R {
    let c = connect_remote(name)?;
    if !c.has_capability(thurm_proto::CAP_PORTS) {
        return Err(format!(
            "Thurm on {name} is too old to list ports; `thurm remote install {name}`"
        )
        .into());
    }
    let Response::Ports(list) = c.request(Request::Ports { pane: None })? else {
        return Err("unexpected answer from the daemon".into());
    };
    if json {
        println!("{}", serde_json::to_string(&ports_json(&list))?);
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", ports_table(&list));
    Ok(ExitCode::SUCCESS)
}

fn ports_json(list: &[thurm_proto::PanePorts]) -> serde_json::Value {
    serde_json::json!(
        list.iter()
            .map(|p| serde_json::json!({
                "pane": p.pane,
                "complete": p.complete,
                "ports": p.ports,
            }))
            .collect::<Vec<_>>()
    )
}

fn ports_table(list: &[thurm_proto::PanePorts]) -> String {
    let rows: Vec<_> = list
        .iter()
        .flat_map(|p| p.ports.iter().map(move |e| (p.pane, e)))
        .collect();
    let partial = "(some processes could not be read: their ports are not listed)\n";
    let incomplete = list.iter().any(|p| !p.complete);
    if rows.is_empty() {
        return if incomplete {
            format!("no ports\n{partial}")
        } else {
            "no ports\n".into()
        };
    }
    let mut out = format!(
        "{:<6} {:<6} {:<16} {:<8} COMMAND\n",
        "PANE", "PORT", "ADDRESS", "PID"
    );
    for (pane, e) in rows {
        let command: String = e.command.chars().take(80).collect();
        out.push_str(&format!(
            "{:<6} {:<6} {:<16} {:<8} {command}\n",
            pane, e.port, e.address, e.pid
        ));
    }
    if incomplete {
        out.push_str(partial);
    }
    out
}

/// `thurm remote forwards [NAME]`: the forwards the app has open, as this machine's daemon
/// last heard of them, with the connections each carries.
pub fn forwards_cmd(c: &Client, name: Option<&str>, json: bool) -> R {
    if let Some(name) = name {
        remote_config(name)?;
    }
    let listed = crate::ext::call(
        c,
        thurm_proto::ext::method::FORWARDS_LIST,
        serde_json::Value::Null,
    )?;
    let mut list: Vec<thurm_proto::ext::ForwardInfo> = serde_json::from_value(listed)?;
    list.retain(|f| name.is_none_or(|n| f.host == n));
    list.sort_by(|a, b| (&a.host, a.remote_port, a.id).cmp(&(&b.host, b.remote_port, b.id)));
    if json {
        println!("{}", serde_json::to_string(&list)?);
    } else {
        print!("{}", forwards_table(&list));
    }
    Ok(ExitCode::SUCCESS)
}

fn forwards_table(list: &[thurm_proto::ext::ForwardInfo]) -> String {
    if list.is_empty() {
        return "no forwards\n".into();
    }
    let host = list
        .iter()
        .map(|f| f.host.chars().count())
        .max()
        .unwrap_or(0)
        .max("HOST".len());
    let mut out = format!(
        "{:<host$} {:<6} {:<6} {:<11} URL\n",
        "HOST", "PORT", "LOCAL", "CONNECTIONS"
    );
    for f in list {
        // An app that does not count them says nothing: not the same as none.
        let connections = f
            .connections
            .map_or_else(|| "-".to_owned(), |n| n.to_string());
        out.push_str(&format!(
            "{:<host$} {:<6} {:<6} {:<11} {}\n",
            f.host, f.remote_port, f.local_port, connections, f.url
        ));
    }
    out
}

/// Whether `thurm remote forward` opens the browser: not with `--no-open`, nor with `--json`.
fn opens_browser(open: bool, json: bool) -> bool {
    open && !json
}

/// `thurm remote forward NAME PORT|URL`: opens the port here until interrupted.
pub fn forward_cmd(name: &str, target: &str, local: Option<u16>, open: bool, json: bool) -> R {
    let (port, url) = match target.parse::<u16>() {
        Ok(p) if p > 0 => (p, None),
        _ => {
            let u = thurm_remote::policy::LocalUrl::parse(target).ok_or_else(|| {
                format!("{target} is neither a port nor a link to {name}'s localhost")
            })?;
            (u.port, Some(target))
        }
    };
    thurm_remote::forward::forwardable(&remote_config(name)?)?;
    connect_remote(name)?;
    let forwards = thurm_remote::forward::Forwards::default();
    forwards.check(name)?;
    let (fw, _) = forwards.open_for(name, port, local, url)?;
    let url = url
        .and_then(|u| thurm_remote::forward::rewrite(u, fw.local))
        .unwrap_or_else(|| format!("http://localhost:{}/", fw.local));
    if json {
        println!(
            "{}",
            serde_json::json!({
                "host": name, "remote_port": port, "local_port": fw.local, "url": url,
            })
        );
    } else {
        println!("{url}");
        eprintln!("{name}:{port} → localhost:{} (Ctrl-C to stop)", fw.local);
    }
    let _ = std::io::stdout().flush();
    // JSON is for scripts: they open what they want themselves.
    if opens_browser(open, json) {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        if let Err(e) = std::process::Command::new(opener)
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            eprintln!("could not open {url}: {e}");
        }
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(sig, stop.clone())?;
    }
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = forwards.close(fw.id);
    Ok(ExitCode::SUCCESS)
}

/// `thurm remote-info` (hidden): what the app's install check reads over ssh.
pub fn remote_info() -> R {
    let info = ThurmInfo {
        daemon_id: thurm_config::daemon_id().ok(),
        ..ThurmInfo::current()
    };
    println!("{}", serde_json::to_string(&info)?);
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_proto::{PanePorts, PortEntry};

    #[test]
    fn json_output_opens_no_browser() {
        assert!(opens_browser(true, false));
        assert!(!opens_browser(true, true));
        assert!(!opens_browser(false, false));
        assert!(!opens_browser(false, true));
    }

    #[test]
    fn forwards_print_as_a_table() {
        use thurm_proto::ext::ForwardInfo;
        let forward = |host: &str, remote_port, local_port, connections| ForwardInfo {
            id: 1,
            host: host.into(),
            remote_port,
            local_port,
            url: format!("http://localhost:{local_port}/"),
            connections,
        };
        let t = forwards_table(&[
            forward("devbox", 5173, 5173, Some(2)),
            forward("build-machine", 3000, 13000, None),
        ]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "HOST          PORT   LOCAL  CONNECTIONS URL");
        assert_eq!(
            lines[1],
            "devbox        5173   5173   2           http://localhost:5173/"
        );
        // An app that does not count connections: unknown, not none.
        assert_eq!(
            lines[2],
            "build-machine 3000   13000  -           http://localhost:13000/"
        );
        assert_eq!(forwards_table(&[]), "no forwards\n");
    }

    #[test]
    fn ports_print_as_a_table() {
        let entry = |port, pid, command: &str| PortEntry {
            port,
            address: "127.0.0.1".into(),
            pid,
            command: command.into(),
        };
        let list = vec![
            PanePorts {
                pane: 3,
                ports: vec![entry(5173, 812, "node vite")],
                complete: true,
            },
            PanePorts {
                pane: 7,
                ports: vec![],
                complete: false,
            },
        ];
        let t = ports_table(&list);
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].starts_with("PANE   PORT"), "{t}");
        assert!(lines[1].starts_with("3      5173   127.0.0.1"), "{t}");
        assert!(lines[1].ends_with("node vite"), "{t}");
        assert!(t.contains("could not be read"), "{t}");
        assert_eq!(ports_table(&[]), "no ports\n");
        // Nothing found where something could not be read: not a plain "no ports".
        let unread = [PanePorts {
            pane: 7,
            ports: vec![],
            complete: false,
        }];
        assert_eq!(
            ports_table(&unread),
            "no ports\n(some processes could not be read: their ports are not listed)\n"
        );
        assert_eq!(ports_json(&list)[0]["ports"][0]["port"], 5173);
    }
}
