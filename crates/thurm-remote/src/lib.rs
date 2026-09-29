//! Remote workspaces: the app attaches to `thurmd` on other machines through the system
//! `ssh` (see `specs/remote-workspaces.md`).
//!
//! - [`ssh`]: the hardened ssh invocations.
//! - [`tunnel`]: one supervised `ssh -N -L` per host, reconnecting with backoff, and the
//!   state file `thurm --remote` reads.
//! - [`install`]: putting the app's exact build on a host, and upgrading its daemon in place.
//! - [`doctor`]: what a host needs (lingering, PATH, agents, hooks), and the fixes.
//! - [`handoff`]: giving a local repository to a remote agent through git.
//! - [`policy`]: what remote panes may do on this Mac (clipboard, links).

pub mod doctor;
pub mod handoff;
pub mod install;
pub mod policy;
pub mod ssh;
pub mod tunnel;

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use ssh::{Ssh, SshError};

/// What `thurm remote-info` prints on a host (JSON), so the app can tell whether that host
/// runs its build without a daemon connection.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ThurmInfo {
    pub build: String,
    pub protocol: u32,
    #[serde(default)]
    pub hot_upgrade_protocol: u32,
    /// The daemon socket `thurm` and `thurmd` default to there.
    pub socket: String,
    /// Rust target triple of that `thurm`.
    #[serde(default)]
    pub target: String,
}

impl ThurmInfo {
    /// This build's.
    pub fn current() -> ThurmInfo {
        ThurmInfo {
            build: thurm_proto::BUILD.to_owned(),
            protocol: thurm_proto::PROTOCOL_VERSION,
            hot_upgrade_protocol: thurm_proto::HOT_UPGRADE_PROTOCOL,
            socket: thurm_config::socket_path().display().to_string(),
            target: current_target().to_owned(),
        }
    }
}

/// The Rust target this code was built for (`thurm_proto::BUILD` names the version).
pub fn current_target() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "musl"
    )) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "unknown"
    }
}

/// A host as one ssh round trip sees it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct HostInfo {
    /// `uname -s`: "Linux", "Darwin".
    pub os: String,
    /// `uname -m`: "x86_64", "aarch64", "arm64".
    pub arch: String,
    /// Where Thurm installs into (the home directory).
    pub home: String,
    /// Path of `thurm` there, if any.
    pub thurm: Option<String>,
    /// Its `remote-info`; `None` for a `thurm` too old to have one.
    pub info: Option<ThurmInfo>,
    pub nix: bool,
    pub git: bool,
    /// systemd's `Linger` for the user ("yes"/"no"), when logind is there.
    pub linger: Option<String>,
}

impl HostInfo {
    /// Our release artifact for the host, when there is one.
    pub fn artifact_target(&self) -> Option<&'static str> {
        match (self.os.as_str(), self.arch.as_str()) {
            ("Linux", "x86_64" | "amd64") => Some("x86_64-unknown-linux-musl"),
            ("Linux", "aarch64" | "arm64") => Some("aarch64-unknown-linux-musl"),
            ("Darwin", "arm64" | "aarch64") => Some("aarch64-apple-darwin"),
            _ => None,
        }
    }

    /// The host runs exactly this build of `thurm`.
    pub fn runs_our_build(&self) -> bool {
        self.info
            .as_ref()
            .is_some_and(|i| i.build == thurm_proto::BUILD)
    }

    /// The host's `thurm` speaks our protocol (its daemon will accept us).
    pub fn speaks_our_protocol(&self) -> bool {
        self.info
            .as_ref()
            .is_some_and(|i| i.protocol == thurm_proto::PROTOCOL_VERSION)
    }
}

const PROBE: &str = concat!(
    "TH=\"${THURM_HOME:-$HOME}\"; ",
    "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
    "echo \"os=$(uname -s)\"; echo \"arch=$(uname -m)\"; echo \"home=$TH\"; ",
    "command -v nix >/dev/null 2>&1 && echo nix=1; ",
    "command -v git >/dev/null 2>&1 && echo git=1; ",
    "command -v loginctl >/dev/null 2>&1 && echo \"linger=$(loginctl show-user \"$(id -un)\" -p Linger --value 2>/dev/null)\"; ",
    "if command -v thurm >/dev/null 2>&1; then echo \"thurm=$(command -v thurm)\"; ",
    "echo \"info=$(thurm remote-info 2>/dev/null | head -n 1)\"; fi; true"
);

/// One ssh round trip: platform, install state and build of the host.
pub fn probe(ssh: &Ssh) -> Result<HostInfo, SshError> {
    let out = ssh.run(PROBE, &[], None, Duration::from_secs(30))?;
    Ok(parse_probe(&out))
}

fn parse_probe(out: &str) -> HostInfo {
    let mut h = HostInfo::default();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k {
            "os" => h.os = v.to_owned(),
            "arch" => h.arch = v.to_owned(),
            "home" => h.home = v.to_owned(),
            "nix" => h.nix = true,
            "git" => h.git = true,
            "linger" if !v.is_empty() => h.linger = Some(v.to_owned()),
            "thurm" if !v.is_empty() => h.thurm = Some(v.to_owned()),
            "info" => h.info = serde_json::from_str(v).ok(),
            _ => {}
        }
    }
    h
}

/// Starts the host's daemon on `socket` unless one runs there already (`thurmd` exits at
/// once when the socket answers).
pub fn start_daemon(ssh: &Ssh, socket: &str) -> Result<(), SshError> {
    const START: &str = concat!(
        "TH=\"${THURM_HOME:-$HOME}\"; ",
        "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
        "D=\"$(dirname \"$(command -v thurm)\")\"; ",
        "if [ -x \"$D/thurmd\" ]; then T=\"$D/thurmd\"; else T=\"$(command -v thurmd)\"; fi; ",
        "[ -n \"$T\" ] || { echo \"thurmd is not installed\" >&2; exit 3; }; ",
        "exec \"$T\" --daemonize --socket \"$1\" </dev/null >/dev/null 2>&1"
    );
    ssh.run(START, &[socket], None, Duration::from_secs(30))
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_parsed() {
        let info = ThurmInfo::current();
        let out = format!(
            "os=Linux\narch=aarch64\nhome=/home/me\nnix=1\ngit=1\nlinger=no\nthurm=/home/me/.local/bin/thurm\ninfo={}\n",
            serde_json::to_string(&info).unwrap()
        );
        let h = parse_probe(&out);
        assert_eq!(h.os, "Linux");
        assert_eq!(h.artifact_target(), Some("aarch64-unknown-linux-musl"));
        assert!(h.nix && h.git);
        assert_eq!(h.linger.as_deref(), Some("no"));
        assert!(h.runs_our_build() && h.speaks_our_protocol());
        // A thurm without `remote-info`: installed, build unknown.
        let old = parse_probe("os=Darwin\narch=arm64\nhome=/Users/me\nthurm=/x/thurm\ninfo=\n");
        assert!(old.thurm.is_some() && old.info.is_none() && !old.speaks_our_protocol());
        assert_eq!(old.artifact_target(), Some("aarch64-apple-darwin"));
        assert_eq!(
            parse_probe("os=FreeBSD\narch=amd64\n").artifact_target(),
            None
        );
    }

    #[test]
    fn scripts_are_safe_to_quote() {
        ssh::quote(PROBE).unwrap();
        assert!(PROBE.contains(ssh::PRELUDE));
        assert!(PROBE.contains(ssh::REMOTE_PATH));
        assert!(PROBE.contains(ssh::REMOTE_BIN_DIR));
    }
}
