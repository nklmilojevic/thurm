//! What a host needs for remote workspaces, checked in two ssh round trips (the install
//! plan, then the rest), with the fix for each problem (`thurm remote doctor`, Thurm ›
//! Remotes…).
//!
//! A fix is either done over Thurm's own ssh connection ([`fix`]), or needs you at a
//! terminal on the host (a sudo password, a browser sign-in): [`Check::terminal`] is the
//! script to run there with a tty ([`Ssh::interactive_argv`]).

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::install::{self, Plan};
use crate::ssh::{Ssh, SshError};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ok,
    /// Works, but something is missing (agent status, an agent).
    Warn,
    /// Remote workspaces do not work (well) until it is fixed.
    Fail,
    /// Does not apply to this host.
    Skip,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Fix {
    /// Button title: "Install", "Enable lingering".
    pub label: String,
    /// Asked before running it (it stops programs, it runs an installer from the web).
    pub confirm: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// Stable id for [`fix`]: "thurm", "daemon", "linger", "path", "agent.claude",
    /// "login.claude", "hooks.claude".
    pub id: String,
    pub title: String,
    pub state: State,
    pub detail: String,
    /// What Thurm can do about it over ssh.
    pub fix: Option<Fix>,
    /// A script to run in a terminal on the host instead (or when `fix` fails).
    pub terminal: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub plan: Plan,
    pub checks: Vec<Check>,
}

impl Report {
    /// Nothing left that stops remote workspaces or agent status from working.
    pub fn healthy(&self) -> bool {
        self.checks.iter().all(|c| c.state != State::Fail)
    }

    pub fn check(&self, id: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == id)
    }
}

/// The agents whose hooks Thurm manages, with the installer Thurm may run for them.
struct Agent {
    kind: &'static str,
    name: &'static str,
    /// Official install script (piped to `bash`), for agents that publish one.
    installer: Option<&'static str>,
}

const AGENTS: &[Agent] = &[
    Agent {
        kind: "claude",
        name: "Claude Code",
        installer: Some("https://claude.ai/install.sh"),
    },
    Agent {
        kind: "codex",
        name: "Codex",
        installer: None,
    },
];

/// Everything besides the install plan: PATH link, agents, sign-in and hooks. Agents are also
/// looked up through the login shell (npm and bun install into places a plain ssh command
/// does not have on PATH), except under `THURM_BASE_PATH` (the loopback tests).
const PROBE: &str = concat!(
    "TH=\"${THURM_HOME:-$HOME}\"; ",
    "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
    "echo \"user=$(id -un)\"; ",
    "[ -d \"$TH/.local/bin\" ] && echo localbin=1; ",
    "L=\"$TH/.local/bin/thurm\"; { [ -L \"$L\" ] || [ -x \"$L\" ]; } && echo link=1; ",
    "command -v curl >/dev/null 2>&1 && echo curl=1; ",
    "for a in claude codex; do p=\"$(command -v \"$a\" 2>/dev/null)\"; ",
    "if [ -z \"$p\" ] && [ -z \"$THURM_BASE_PATH\" ]; then p=\"$(\"${SHELL:-sh}\" -lc \"command -v $a\" </dev/null 2>/dev/null | grep ^/ | tail -n 1)\"; fi; ",
    "echo \"agent_$a=$p\"; done; ",
    "{ [ -f \"${CLAUDE_CONFIG_DIR:-$TH/.claude}/.credentials.json\" ] || [ -n \"$ANTHROPIC_API_KEY\" ]; } && echo claude_login=1; ",
    "command -v thurm >/dev/null 2>&1 && echo \"hooks=$(HOME=\"$TH\" thurm hooks status --json 2>/dev/null | head -n 1)\"; ",
    "true"
);

#[derive(Default, Debug, PartialEq)]
struct Probe {
    user: String,
    local_bin: bool,
    link: bool,
    curl: bool,
    /// Agent kind → its path there.
    agents: Vec<(String, String)>,
    claude_login: bool,
    /// `thurm hooks status --json` there.
    hooks: Vec<Value>,
}

fn parse(out: &str) -> Probe {
    let mut p = Probe::default();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k {
            "user" => p.user = v.to_owned(),
            "localbin" => p.local_bin = true,
            "link" => p.link = true,
            "curl" => p.curl = true,
            "claude_login" => p.claude_login = true,
            "hooks" => {
                p.hooks = serde_json::from_str::<Vec<Value>>(v).unwrap_or_default();
            }
            _ => {
                if let Some(kind) = k.strip_prefix("agent_")
                    && !v.is_empty()
                {
                    p.agents.push((kind.to_owned(), v.to_owned()));
                }
            }
        }
    }
    p
}

/// Checks the host. `name` is the `[[remote]]` name, for messages.
pub fn run(
    ssh: &Ssh,
    name: &str,
    socket: Option<&str>,
    local_bins: Option<&Path>,
) -> Result<Report, SshError> {
    let plan = install::plan(ssh, socket, local_bins)?;
    let probe = parse(&ssh.run(PROBE, &[], None, Duration::from_secs(60))?);
    let mut checks = checks(name, &plan, &probe);
    // Right after the daemon: the rest is moot while the app cannot connect.
    let at = checks
        .iter()
        .position(|c| c.id == "daemon")
        .map_or(0, |i| i + 1);
    checks.insert(at, tunnel_check(ssh, &plan, socket));
    Ok(Report { checks, plan })
}

/// Opens the forward the app's tunnel uses and pings the daemon through it. Some ssh servers
/// accept a Unix-socket forward and close it at once (OrbStack's built-in one, Tailscale SSH
/// before 1.98): everything else checks out, and the app still never connects. Starts the
/// daemon when it is not running, as connecting would.
fn tunnel_check(ssh: &Ssh, plan: &Plan, socket: Option<&str>) -> Check {
    let mut c = check(
        "tunnel",
        "Tunnel",
        State::Ok,
        "forwards the daemon's socket",
    );
    let remote = socket
        .map(str::to_owned)
        .or_else(|| plan.host.info.as_ref().map(|i| i.socket.clone()));
    let (true, Some(remote)) = (plan.host.thurm.is_some(), remote) else {
        c.state = State::Skip;
        c.detail = "needs Thurm".into();
        return c;
    };
    if !plan.daemon.as_ref().is_some_and(|d| d.running)
        && let Err(e) = crate::start_daemon(ssh, &remote)
    {
        c.state = State::Fail;
        c.detail = format!("the daemon does not start: {e}");
        return c;
    }
    match try_forward(ssh, &remote) {
        Ok(()) => {}
        Err(Forward::Failed(e)) => {
            c.state = State::Fail;
            c.detail = e;
        }
        Err(Forward::NoAnswer(e)) => {
            c.state = State::Fail;
            // Only when the daemon answers on the host is the forward itself to blame.
            c.detail = if daemon_answers(ssh, &remote) {
                format!(
                    "the daemon answers on the host but not through the forward ({e}): the host's \
                     ssh server likely cannot forward Unix sockets (OrbStack's built-in one and \
                     Tailscale SSH before 1.98 cannot); connect to the host's OpenSSH instead"
                )
            } else {
                format!("the daemon does not answer on {remote} ({e})")
            };
        }
    }
    c
}

/// `thurm daemon status` on the host, for the daemon on `socket`.
const DAEMON_STATUS: &str = concat!(
    "TH=\"${THURM_HOME:-$HOME}\"; ",
    "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
    "THURM_SOCKET=\"$1\" thurm daemon status --json 2>/dev/null | head -n 1"
);

fn daemon_answers(ssh: &Ssh, socket: &str) -> bool {
    ssh.run(DAEMON_STATUS, &[socket], None, Duration::from_secs(30))
        .ok()
        .and_then(|out| serde_json::from_str::<Value>(out.trim()).ok())
        .and_then(|v| v.get("running").and_then(Value::as_bool))
        .unwrap_or(false)
}

enum Forward {
    /// ssh would not set the forward up.
    Failed(String),
    /// The forward is there, the daemon does not answer through it.
    NoAnswer(String),
}

fn try_forward(ssh: &Ssh, remote: &str) -> Result<(), Forward> {
    use std::process::Stdio;
    let local = thurm_config::remote_dir().join(format!("doctor-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&local);
    let mut child = ssh
        .tunnel_command(&local, remote)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Forward::Failed(format!("cannot run ssh: {e}")))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let result = loop {
        if let Ok(Some(_)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = std::io::Read::read_to_string(&mut e, &mut err);
            }
            let last = last_line(&err).unwrap_or("ssh exited").to_owned();
            break Err(Forward::Failed(format!(
                "ssh could not forward the daemon's socket: {last}"
            )));
        }
        if local.exists() {
            break match crate::tunnel::ping(&local, Duration::from_secs(5)) {
                Ok(_) | Err(crate::tunnel::PingError::Protocol(_)) => Ok(()),
                Err(crate::tunnel::PingError::Unreachable(e)) => Err(Forward::NoAnswer(e)),
            };
        }
        if std::time::Instant::now() > deadline {
            break Err(Forward::Failed("timed out opening the forward".into()));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&local);
    result
}

fn check(id: &str, title: &str, state: State, detail: impl Into<String>) -> Check {
    Check {
        id: id.to_owned(),
        title: title.to_owned(),
        state,
        detail: detail.into(),
        fix: None,
        terminal: None,
    }
}

fn offer(label: &str) -> Option<Fix> {
    Some(Fix {
        label: label.to_owned(),
        confirm: None,
    })
}

fn checks(name: &str, plan: &Plan, probe: &Probe) -> Vec<Check> {
    let host = &plan.host;
    let mut out = Vec::new();

    // Thurm itself.
    let mut c = check("thurm", "Thurm", State::Ok, "");
    if plan.installed {
        c.detail = format!("{} is installed", thurm_proto::BUILD);
    } else {
        c.state = State::Fail;
        c.detail = match &host.info {
            Some(i) => format!(
                "{name} has {}; this Mac runs {}",
                i.build,
                thurm_proto::BUILD
            ),
            None if host.thurm.is_some() => format!("{name} has an older Thurm"),
            None => "not installed".into(),
        };
        match plan.methods.first() {
            Some(m) => {
                c.fix = offer("Install");
                c.detail.push_str(&format!(" ({})", m.label()));
            }
            None => {
                if let Some(p) = &plan.problem {
                    c.detail.push_str(&format!(": {p}"));
                }
            }
        }
    }
    out.push(c);
    // The checks below need a `thurm` there, of any build.
    let thurm_there = host.thurm.is_some();

    // Its daemon.
    let mut c = check("daemon", "Daemon", State::Ok, "starts when Thurm connects");
    match &plan.daemon {
        _ if !thurm_there => {
            c.state = State::Skip;
            c.detail = "needs Thurm".into();
        }
        Some(d) if d.running => {
            let ours = d
                .build
                .as_deref()
                .is_some_and(|b| install::same_build(thurm_proto::BUILD, b));
            if ours {
                c.detail = "running".into();
            } else if !plan.installed {
                // Installing Thurm replaces it (see `fix("thurm")`).
                c.state = State::Warn;
                c.detail = format!(
                    "runs {}; replaced when this build is installed",
                    d.build.as_deref().unwrap_or("another build")
                );
            } else {
                c.state = State::Fail;
                c.detail = format!(
                    "runs {} (this Mac: {})",
                    d.build.as_deref().unwrap_or("another build"),
                    thurm_proto::BUILD
                );
                c.fix = Some(Fix {
                    label: if d.hot_upgrade {
                        "Upgrade".into()
                    } else {
                        "Restart".into()
                    },
                    confirm: (!d.hot_upgrade).then(|| {
                        format!(
                            "The daemon on {name} is too old to be replaced in place: restarting it \
                             stops the programs running in its panes."
                        )
                    }),
                });
            }
        }
        _ => {}
    }
    out.push(c);

    // Lingering (systemd): without it logind removes the daemon's socket at logout.
    let user = if probe.user.is_empty() {
        "$(id -un)".to_owned()
    } else {
        probe.user.clone()
    };
    let mut c = check("linger", "Lingering", State::Ok, "on");
    match host.linger.as_deref() {
        None => {
            c.state = State::Skip;
            c.detail = "no systemd-logind".into();
        }
        Some("yes") => {}
        Some(_) => {
            c.state = State::Fail;
            c.detail =
                "off: the daemon's socket goes away when your last session on the host ends".into();
            c.fix = offer("Enable");
            c.terminal = Some(format!("sudo loginctl enable-linger {user}"));
        }
    }
    out.push(c);
    if host.kill_user_processes == Some(true) {
        // Lingering keeps the user manager, not what a session started: the daemon still
        // ends with the last ssh session.
        let mut k = check("kill_user_processes", "Session cleanup", State::Fail, "");
        // No command offered: KillExcludeUsers is a list, and adding a line would replace the
        // exclusions already there.
        k.detail = format!(
            "logind kills your processes when your last session ends (KillUserProcesses=yes): \
             the daemon and its shells with them. Have {user} added to KillExcludeUsers in \
             /etc/systemd/logind.conf, keeping the users listed there"
        );
        out.push(k);
    }

    // `thurm` on PATH in plain ssh sessions (`ssh host thurm …`).
    let own_install = host
        .thurm
        .as_deref()
        .is_some_and(|p| p.contains(crate::ssh::REMOTE_BIN_DIR));
    let mut c = check("path", "Command-line tool", State::Ok, "~/.local/bin/thurm");
    if !thurm_there || !own_install {
        c.state = State::Skip;
        c.detail = if thurm_there {
            "installed by the host's package manager".into()
        } else {
            "needs Thurm".into()
        };
    } else if !probe.link {
        c.state = State::Warn;
        c.detail = if probe.local_bin {
            "not linked into ~/.local/bin".into()
        } else {
            "no ~/.local/bin: `thurm` is not on PATH there".into()
        };
        c.fix = offer("Link");
    }
    out.push(c);

    // Agents, their sign-in and their hooks.
    for agent in AGENTS {
        let path = probe
            .agents
            .iter()
            .find(|(k, _)| k == agent.kind)
            .map(|(_, p)| p.clone());
        let mut c = check(&format!("agent.{}", agent.kind), agent.name, State::Ok, "");
        match &path {
            Some(p) => c.detail = p.clone(),
            None => {
                c.detail = "not installed".into();
                c.state = State::Skip;
                if let Some(url) = agent.installer {
                    c.state = State::Warn;
                    let script = format!("curl -fsSL {url} | bash");
                    if probe.curl {
                        c.fix = Some(Fix {
                            label: "Install".into(),
                            confirm: Some(format!("Run {url} on {name}?")),
                        });
                    } else {
                        c.detail = "not installed (and no curl to install it)".into();
                    }
                    c.terminal = Some(script);
                }
            }
        }
        let found = path.is_some();
        out.push(c);
        if !found {
            continue;
        }

        if agent.kind == "claude" {
            let mut c = check(
                "login.claude",
                "Claude Code sign-in",
                State::Ok,
                "signed in",
            );
            if host.os == "Darwin" {
                c.state = State::Skip;
                c.detail = "kept in the host's keychain".into();
            } else if !probe.claude_login {
                c.state = State::Warn;
                c.detail = "not signed in: run it once there and sign in".into();
                c.terminal = Some(sign_in_script(path.as_deref().unwrap_or("claude")));
            }
            out.push(c);
        }

        let status = probe
            .hooks
            .iter()
            .find(|h| h.get("agent").and_then(Value::as_str) == Some(agent.kind));
        let count = |k: &str| status.and_then(|h| h.get(k)).and_then(Value::as_u64);
        let mut c = check(
            &format!("hooks.{}", agent.kind),
            &format!("{} hooks", agent.name),
            State::Ok,
            "installed",
        );
        match (count("installed"), count("total")) {
            _ if !thurm_there => {
                c.state = State::Skip;
                c.detail = "needs Thurm".into();
            }
            (Some(n), Some(t)) if n == t => {}
            (n, t) => {
                c.state = State::Fail;
                c.detail = match (n, t) {
                    (Some(0), _) | (None, _) => {
                        "not installed: agent status (Working, Needs input) is guessed".into()
                    }
                    (Some(n), Some(t)) => format!("{n} of {t} installed"),
                    _ => "unknown".into(),
                };
                c.fix = offer("Install");
            }
        }
        out.push(c);
    }
    out
}

#[derive(Debug)]
pub enum FixError {
    Ssh(SshError),
    Failed(String),
    /// A check with no fix Thurm can run (or none needed).
    NoFix(String),
}

impl std::fmt::Display for FixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FixError::Ssh(e) => write!(f, "{e}"),
            FixError::Failed(e) | FixError::NoFix(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for FixError {}

impl From<SshError> for FixError {
    fn from(e: SshError) -> Self {
        FixError::Ssh(e)
    }
}

impl From<install::InstallError> for FixError {
    fn from(e: install::InstallError) -> Self {
        FixError::Failed(e.to_string())
    }
}

const LINK: &str = concat!(
    "TH=\"${THURM_HOME:-$HOME}\"; ",
    "B=\"$TH/.local/share/thurm/bin/thurm\"; L=\"$TH/.local/bin/thurm\"; ",
    "[ -x \"$B\" ] || { echo \"Thurm is not installed\" >&2; exit 3; }; ",
    "[ ! -e \"$L\" ] || [ -L \"$L\" ] || { echo \"$L is another file\" >&2; exit 3; }; ",
    "mkdir -p \"$TH/.local/bin\" && ln -sfn \"$B\" \"$L\""
);

/// Without a password prompt: logind lets you linger yourself on most distributions, else
/// sudo when it needs none.
const LINGER: &str = concat!(
    "U=\"$(id -un)\"; ",
    "loginctl enable-linger \"$U\" 2>/dev/null || sudo -n loginctl enable-linger \"$U\" 2>/dev/null; ",
    "[ \"$(loginctl show-user \"$U\" -p Linger --value 2>/dev/null)\" = yes ] || ",
    "{ echo \"enabling lingering needs your password there\" >&2; exit 3; }"
);

const HOOKS: &str = concat!(
    "TH=\"${THURM_HOME:-$HOME}\"; ",
    "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
    "HOME=\"$TH\" thurm hooks install --agent \"$1\""
);

/// Downloaded first, so a failed download fails the fix (`curl | bash` would report the
/// status of a `bash` that read nothing).
const AGENT_INSTALL: &str = concat!(
    "T=\"$(mktemp)\" || exit 1; ",
    "if curl -fsSL \"$1\" -o \"$T\"; then bash \"$T\"; S=$?; else S=$?; fi; ",
    "rm -f \"$T\"; exit $S"
);

/// Runs the fix for check `id` over ssh and returns what it printed. `allow_restart`
/// confirms restarting a daemon too old to be replaced in place.
pub fn fix(
    ssh: &Ssh,
    socket: Option<&str>,
    local_bins: Option<&Path>,
    id: &str,
    allow_restart: bool,
) -> Result<String, FixError> {
    match id {
        "thurm" => {
            let plan = install::plan(ssh, socket, local_bins)?;
            if plan.installed {
                return Ok("Thurm is installed already".into());
            }
            let method = *plan.methods.first().ok_or_else(|| {
                FixError::NoFix(
                    plan.problem
                        .clone()
                        .unwrap_or_else(|| "no way to install Thurm there".into()),
                )
            })?;
            let info = install::install(ssh, &plan.host, method, local_bins)?;
            let mut msg = format!("installed Thurm {}", info.build);
            // A daemon of another build answers nobody now: replace it too when that keeps
            // its panes.
            let plan = install::plan(ssh, socket, local_bins)?;
            if let Some(d) = plan.daemon.as_ref().filter(|d| d.running && d.hot_upgrade) {
                let out = install::upgrade_daemon(ssh, socket, Some(d), false)
                    .map_err(|e| FixError::Failed(e.to_string()))?;
                msg.push_str(&format!("; {out}"));
            }
            Ok(msg)
        }
        "daemon" => {
            let plan = install::plan(ssh, socket, local_bins)?;
            install::upgrade_daemon(ssh, socket, plan.daemon.as_ref(), allow_restart)
                .map_err(|e| FixError::Failed(e.to_string()))
        }
        "linger" => ssh
            .run(LINGER, &[], None, Duration::from_secs(30))
            .map(|_| "lingering is on".into())
            .map_err(|e| match e {
                SshError::Remote { stderr, .. } => FixError::Failed(stderr.trim().to_owned()),
                e => FixError::Ssh(e),
            }),
        "path" => {
            ssh.run(LINK, &[], None, Duration::from_secs(30))?;
            Ok("linked ~/.local/bin/thurm".into())
        }
        _ => {
            if let Some(kind) = id.strip_prefix("hooks.") {
                let agent = AGENTS
                    .iter()
                    .find(|a| a.kind == kind)
                    .ok_or_else(|| FixError::NoFix(format!("unknown agent {kind:?}")))?;
                let out = ssh.run(HOOKS, &[agent.kind], None, Duration::from_secs(60))?;
                return Ok(out.trim().to_owned());
            }
            if let Some(kind) = id.strip_prefix("agent.") {
                let url = AGENTS
                    .iter()
                    .find(|a| a.kind == kind)
                    .and_then(|a| a.installer)
                    .ok_or_else(|| FixError::NoFix(format!("Thurm cannot install {kind}")))?;
                let out = ssh.run(AGENT_INSTALL, &[url], None, Duration::from_secs(600))?;
                return Ok(last_line(&out).unwrap_or("installed").to_owned());
            }
            Err(FixError::NoFix(format!("no fix for {id:?}")))
        }
    }
}

/// `exec` of the agent at `path` there. A path with characters a double-quoted word would
/// expand (or that [`crate::ssh::quote`] refuses) is looked up on PATH instead.
fn sign_in_script(path: &str) -> String {
    let plain = !path.is_empty()
        && path
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || " /._+-@".contains(ch));
    if plain {
        format!("exec \"{path}\"")
    } else {
        concat!(
            "TH=\"${THURM_HOME:-$HOME}\"; ",
            "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
            "exec claude"
        )
        .to_owned()
    }
}

/// The last line with text: an installer's closing message (ANSI colors and all).
fn last_line(out: &str) -> Option<&str> {
    out.lines().map(str::trim).rfind(|l| !l.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostInfo;
    use crate::install::{DaemonState, Method};
    use crate::ssh::{PRELUDE, REMOTE_PATH};

    fn plan(installed: bool) -> Plan {
        Plan {
            host: HostInfo {
                os: "Linux".into(),
                arch: "aarch64".into(),
                home: "/home/me".into(),
                thurm: installed.then(|| "/home/me/.local/share/thurm/bin/thurm".into()),
                linger: Some("no".into()),
                ..Default::default()
            },
            methods: vec![Method::Download],
            problem: None,
            installed,
            daemon: installed.then_some(DaemonState {
                running: true,
                protocol: Some(thurm_proto::PROTOCOL_VERSION),
                build: Some(thurm_proto::BUILD.into()),
                hot_upgrade: true,
            }),
        }
    }

    fn state(r: &[Check], id: &str) -> State {
        r.iter().find(|c| c.id == id).map(|c| c.state).unwrap()
    }

    #[test]
    fn a_fresh_host_needs_everything() {
        let p = parse("user=me\ncurl=1\nagent_claude=\nagent_codex=\n");
        let c = checks("pi", &plan(false), &p);
        assert_eq!(state(&c, "thurm"), State::Fail);
        assert!(c[0].fix.is_some());
        assert_eq!(state(&c, "daemon"), State::Skip);
        assert_eq!(state(&c, "linger"), State::Fail);
        let linger = c.iter().find(|c| c.id == "linger").unwrap();
        assert_eq!(
            linger.terminal.as_deref(),
            Some("sudo loginctl enable-linger me")
        );
        assert_eq!(state(&c, "path"), State::Skip);
        assert_eq!(state(&c, "agent.claude"), State::Warn);
        assert_eq!(state(&c, "agent.codex"), State::Skip);
        // No agent, so nothing about its sign-in or hooks.
        assert!(
            !c.iter()
                .any(|c| c.id.starts_with("hooks.") || c.id.starts_with("login."))
        );
    }

    #[test]
    fn a_host_with_claude_but_no_hooks() {
        let out = concat!(
            "user=me\nlocalbin=1\ncurl=1\nagent_claude=/home/me/.local/bin/claude\nagent_codex=\n",
            "hooks=[{\"agent\":\"claude\",\"installed\":0,\"total\":6,\"present\":false},",
            "{\"agent\":\"codex\",\"installed\":0,\"total\":3,\"present\":false}]\n"
        );
        let p = parse(out);
        assert!(p.local_bin && !p.link);
        let c = checks("pi", &plan(true), &p);
        assert_eq!(state(&c, "thurm"), State::Ok);
        assert_eq!(state(&c, "daemon"), State::Ok);
        assert_eq!(state(&c, "path"), State::Warn);
        assert_eq!(state(&c, "agent.claude"), State::Ok);
        assert_eq!(state(&c, "login.claude"), State::Warn);
        assert_eq!(state(&c, "hooks.claude"), State::Fail);
        assert!(!c.iter().any(|c| c.id == "hooks.codex"));
        let report = Report {
            plan: plan(true),
            checks: c,
        };
        assert!(!report.healthy());
    }

    #[test]
    fn a_ready_host() {
        let out = concat!(
            "user=me\nlocalbin=1\nlink=1\nagent_claude=/usr/bin/claude\nclaude_login=1\n",
            "hooks=[{\"agent\":\"claude\",\"installed\":6,\"total\":6}]\n"
        );
        let mut pl = plan(true);
        pl.host.linger = Some("yes".into());
        let r = Report {
            checks: checks("pi", &pl, &parse(out)),
            plan: pl,
        };
        assert!(r.healthy(), "{:#?}", r.checks);
        assert!(r.checks.iter().all(|c| c.fix.is_none()));
    }

    #[test]
    fn an_old_daemon_asks_before_restarting() {
        let mut pl = plan(true);
        pl.daemon = Some(DaemonState {
            running: true,
            protocol: Some(1),
            build: Some("0.0.1".into()),
            hot_upgrade: false,
        });
        let c = checks("pi", &pl, &Probe::default());
        let d = c.iter().find(|c| c.id == "daemon").unwrap();
        assert_eq!(d.state, State::Fail);
        let f = d.fix.as_ref().unwrap();
        assert_eq!(f.label, "Restart");
        assert!(f.confirm.is_some());
    }

    #[test]
    fn sign_in_scripts_keep_the_path_one_word() {
        assert_eq!(
            sign_in_script("/home/me/.local/bin/claude"),
            "exec \"/home/me/.local/bin/claude\""
        );
        assert_eq!(
            sign_in_script("/home/my name/bin/claude"),
            "exec \"/home/my name/bin/claude\""
        );
        // Anything a shell would expand inside double quotes: looked up on PATH instead.
        for bad in [
            "/x/$(rm -rf ~)/claude",
            "/x/`id`/claude",
            "/x/\"q/claude",
            "/x/'q/claude",
            "",
        ] {
            let s = sign_in_script(bad);
            assert!(
                s.ends_with("exec claude") && s.contains(REMOTE_PATH),
                "{bad}: {s}"
            );
        }
        for p in ["/home/my name/bin/claude", "/x/$(id)/claude"] {
            crate::ssh::quote(&sign_in_script(p)).unwrap();
        }
        crate::ssh::quote(DAEMON_STATUS).unwrap();
    }

    #[test]
    fn installers_are_quoted_by_their_last_words() {
        assert_eq!(
            last_line("Installing…\n✔ Claude Code installed\n\n  \n"),
            Some("✔ Claude Code installed")
        );
        assert_eq!(last_line("\n \n"), None);
    }

    #[test]
    fn a_failed_installer_download_fails() {
        let dir = std::env::temp_dir().join(format!("thurm-installer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("install.sh");
        std::fs::write(&script, "echo installed\n").unwrap();
        let run = |url: &str| {
            std::process::Command::new("sh")
                .args(["-c", AGENT_INSTALL, "thurm", url])
                .output()
                .unwrap()
        };
        let ok = run(&format!("file://{}", script.display()));
        assert!(ok.status.success());
        assert_eq!(String::from_utf8_lossy(&ok.stdout).trim(), "installed");
        let missing = run(&format!("file://{}", dir.join("nothing.sh").display()));
        assert!(
            !missing.status.success(),
            "a download that failed must not look installed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scripts_are_safe_to_quote() {
        for s in [PROBE, LINK, LINGER, HOOKS, AGENT_INSTALL] {
            crate::ssh::quote(s).unwrap_or_else(|e| panic!("{e}"));
        }
        assert!(PROBE.contains(PRELUDE) && PROBE.contains(REMOTE_PATH));
        assert!(HOOKS.contains(REMOTE_PATH));
    }
}
