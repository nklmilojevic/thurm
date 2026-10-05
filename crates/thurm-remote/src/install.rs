//! Installing the app's exact build on a remote host, and replacing the host's daemon in
//! place. Always on the user's say-so: background reconnects only report "upgrade needed".
//!
//! - Mac to Mac (Apple silicon): the app's own `thurm` and `thurmd` are copied over.
//! - Linux: the static musl build of the same `BUILD` is downloaded from the release the app
//!   came from and checked against the SHA-256 compiled into the app.
//! - Hosts with Nix: `nix profile install` of the flake's `thurm` at the app's commit.
//!
//! Files land in `~/.local/share/thurm/bin`, and `thurm` is linked into `~/.local/bin` when
//! that directory exists (and holds no other `thurm`), like the local install.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ssh::{Ssh, SshError};
use crate::{HostInfo, ThurmInfo};

/// Where the release this build came from publishes its assets (a GitHub release), set by
/// the release workflow.
pub const DOWNLOAD_BASE: Option<&str> = option_env!("THURM_DOWNLOAD_BASE");
/// The commit this build was made from (for the Nix install).
pub const COMMIT: Option<&str> = option_env!("THURM_COMMIT");
/// SHA-256 of the Linux archives built alongside this app.
const SHA256_X86_64: Option<&str> = option_env!("THURM_SHA256_X86_64_UNKNOWN_LINUX_MUSL");
const SHA256_AARCH64: Option<&str> = option_env!("THURM_SHA256_AARCH64_UNKNOWN_LINUX_MUSL");

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// Copy this Mac's binaries (same platform).
    Copy,
    /// Download the release's musl build.
    Download,
    /// `nix profile install` of the flake output.
    Nix,
}

impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Method::Copy if cfg!(target_os = "macos") => "Copy this Mac's Thurm",
            Method::Copy => "Copy this computer's Thurm",
            Method::Download => "Download the Linux build",
            Method::Nix => "Install with Nix",
        }
    }
}

/// The host's daemon runs and does not speak our build.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DaemonState {
    pub running: bool,
    /// Its protocol, when known.
    pub protocol: Option<u32>,
    pub build: Option<String>,
    /// It can replace itself keeping its panes (protocol ≥ `HOT_UPGRADE_PROTOCOL`).
    pub hot_upgrade: bool,
}

/// What can be done for a host, for the confirmation prompt.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub host: HostInfo,
    /// Possible methods, the recommended one first. Empty: nothing fits (see `problem`).
    pub methods: Vec<Method>,
    pub problem: Option<String>,
    /// The host has this build already (only the daemon may need replacing).
    pub installed: bool,
    pub daemon: Option<DaemonState>,
}

/// Where this process finds `thurm` and `thurmd` to copy (the app passes its
/// `Contents/Helpers`, the CLI its own directory).
pub fn local_binaries(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let t = dir.join("thurm");
    let d = dir.join("thurmd");
    (t.is_file() && d.is_file()).then_some((t, d))
}

/// `thurm-<build>-<target>.tar.gz`, the asset name of a Linux build (`+` is not safe in
/// release asset names).
pub fn artifact_name(build: &str, target: &str) -> String {
    format!("thurm-{}-{target}.tar.gz", build.replace('+', "-"))
}

fn expected_sha256(target: &str) -> Option<&'static str> {
    match target {
        "x86_64-unknown-linux-musl" => SHA256_X86_64,
        "aarch64-unknown-linux-musl" => SHA256_AARCH64,
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

/// `github:OWNER/REPO/<commit>#thurm`, from the release URL this build knows.
pub fn flake_ref() -> Option<String> {
    let base = DOWNLOAD_BASE?;
    let rest = base.strip_prefix("https://github.com/")?;
    let mut parts = rest.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    Some(format!("github:{owner}/{repo}/{}#thurm", COMMIT?))
}

/// Builds match, allowing a Nix build of our commit (`<version>+nix.<short sha>`).
pub fn same_build(ours: &str, theirs: &str) -> bool {
    if ours == theirs {
        return true;
    }
    match (theirs.split_once("+nix."), COMMIT) {
        (Some((_, short)), Some(commit)) => !short.is_empty() && commit.starts_with(short),
        _ => false,
    }
}

/// Our own binaries run there: the same target (macOS), or static Linux binaries of the same
/// architecture (a Linux host has none of our libraries, whatever the target string says).
fn copyable(target: &str, local_bins: Option<&Path>) -> bool {
    let ours = crate::current_target();
    if !target.contains("-linux-") {
        return target == ours;
    }
    let arch = |t: &str| t.split('-').next().unwrap_or("").to_string();
    ours.contains("-linux-")
        && arch(target) == arch(ours)
        && local_bins
            .and_then(local_binaries)
            .is_some_and(|(thurm, thurmd)| is_static_elf(&thurm) && is_static_elf(&thurmd))
}

/// A 64-bit little-endian ELF executable without a program interpreter (statically linked).
fn is_static_elf(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    // Only the ELF header and the program headers: not the whole binary.
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 64];
    if file.read_exact(&mut head).is_err() || &head[..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 {
        return false;
    }
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
    let phoff = u64::from_le_bytes(head[0x20..0x28].try_into().unwrap_or_default());
    let (size, count) = (u16_at(&head, 0x36), u16_at(&head, 0x38));
    // A sane, bounded table (entries are 56 bytes); anything else is no executable of ours.
    if size < 4 || size * count > 64 * 1024 * 1024 || file.seek(SeekFrom::Start(phoff)).is_err() {
        return false;
    }
    let mut table = vec![0u8; size * count];
    if file.read_exact(&mut table).is_err() {
        return false;
    }
    // PT_INTERP: dynamically linked.
    table
        .chunks_exact(size)
        .all(|h| u32::from_le_bytes(h[..4].try_into().unwrap_or_default()) != 3)
}

/// Probes the host and its daemon and lists the ways to put our build there.
pub fn plan(ssh: &Ssh, socket: Option<&str>, local_bins: Option<&Path>) -> Result<Plan, SshError> {
    let host = crate::probe(ssh)?;
    let daemon = daemon_state(ssh, &host, socket)?;
    let installed = host
        .info
        .as_ref()
        .is_some_and(|i| same_build(thurm_proto::BUILD, &i.build));
    let mut methods = Vec::new();
    let mut problem = None;
    match host.artifact_target() {
        Some(t) if copyable(t, local_bins) => {
            if local_bins.and_then(local_binaries).is_some() {
                methods.push(Method::Copy);
            } else {
                problem = Some("this Thurm's own binaries were not found".into());
            }
        }
        Some(t) if t.contains("linux") => {
            let published = DOWNLOAD_BASE.is_some() && expected_sha256(t).is_some();
            if published || std::env::var_os("THURM_ARTIFACT_DIR").is_some() {
                methods.push(Method::Download);
            } else {
                problem = Some(format!(
                    "this development build has no published {t} archive to install"
                ));
            }
        }
        Some(t) => problem = Some(format!("no Thurm build for {t} is available here")),
        None => {
            problem = Some(format!(
                "Thurm does not support {} {} hosts",
                host.os, host.arch
            ))
        }
    }
    if host.nix && flake_ref().is_some() {
        // On Nix hosts, offered first (the host's package manager keeps track of it).
        methods.insert(0, Method::Nix);
        problem = None;
    }
    Ok(Plan {
        host,
        methods,
        problem,
        installed,
        daemon,
    })
}

/// The daemon on `socket` (default: the host's own), as the host's `thurm` sees it.
fn daemon_state(
    ssh: &Ssh,
    host: &HostInfo,
    socket: Option<&str>,
) -> Result<Option<DaemonState>, SshError> {
    if host.thurm.is_none() {
        return Ok(None);
    }
    const STATUS: &str = concat!(
        "TH=\"${THURM_HOME:-$HOME}\"; ",
        "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
        "if [ -n \"$1\" ]; then THURM_SOCKET=\"$1\"; export THURM_SOCKET; fi; ",
        "thurm daemon status --json 2>/dev/null; true"
    );
    let out = ssh.run(
        STATUS,
        &[socket.unwrap_or("")],
        None,
        Duration::from_secs(30),
    )?;
    Ok(out.lines().find_map(parse_daemon_status))
}

fn parse_daemon_status(line: &str) -> Option<DaemonState> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let running = v.get("running")?.as_bool()?;
    if !running {
        return Some(DaemonState {
            running: false,
            protocol: None,
            build: None,
            hot_upgrade: false,
        });
    }
    let build = v.get("build").and_then(|b| b.as_str()).map(str::to_owned);
    let protocol = match v.get("error").and_then(|e| e.as_str()) {
        Some(e) => e
            .split("protocol mismatch: daemon ")
            .nth(1)
            .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse().ok()),
        None => Some(thurm_proto::PROTOCOL_VERSION),
    };
    Some(DaemonState {
        running: true,
        hot_upgrade: protocol.is_some_and(|p| p >= thurm_proto::HOT_UPGRADE_PROTOCOL),
        protocol,
        build,
    })
}

#[derive(Debug)]
pub enum InstallError {
    Ssh(SshError),
    Failed(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Ssh(e) => write!(f, "{e}"),
            InstallError::Failed(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<SshError> for InstallError {
    fn from(e: SshError) -> Self {
        InstallError::Ssh(e)
    }
}

/// Installs our build on the host with `method`. Returns what the host's `thurm` reports
/// afterwards.
pub fn install(
    ssh: &Ssh,
    host: &HostInfo,
    method: Method,
    local_bins: Option<&Path>,
) -> Result<ThurmInfo, InstallError> {
    match method {
        Method::Copy => {
            let (thurm, thurmd) = local_bins.and_then(local_binaries).ok_or_else(|| {
                InstallError::Failed("this Thurm's binaries were not found".into())
            })?;
            upload(ssh, "thurmd", &read(&thurmd)?)?;
            upload(ssh, "thurm", &read(&thurm)?)?;
        }
        Method::Download => {
            let target = host.artifact_target().ok_or_else(|| {
                InstallError::Failed(format!("no build for {} {}", host.os, host.arch))
            })?;
            let (thurm, thurmd) = fetch_artifact(target)?;
            upload(ssh, "thurmd", &thurmd)?;
            upload(ssh, "thurm", &thurm)?;
        }
        Method::Nix => {
            let flake = flake_ref().ok_or_else(|| {
                InstallError::Failed("this build knows no flake to install".into())
            })?;
            const NIX: &str = concat!(
                "TH=\"${THURM_HOME:-$HOME}\"; ",
                "PATH=\"$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH\"; export PATH; ",
                "N=\"nix --extra-experimental-features nix-command --extra-experimental-features flakes\"; ",
                // Build first: a failed fetch or build must not cost the working install.
                "$N build --no-link \"$1\" || exit $?; ",
                "$N profile remove thurm >/dev/null 2>&1; ",
                "$N profile install \"$1\" && rm -f \"$TH/.local/share/thurm/bin/thurm\" \"$TH/.local/share/thurm/bin/thurmd\""
            );
            ssh.run(NIX, &[&flake], None, Duration::from_secs(900))?;
        }
    }
    const FINISH: &str = concat!(
        "TH=\"${THURM_HOME:-$HOME}\"; ",
        "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
        "B=\"$TH/.local/share/thurm/bin/thurm\"; L=\"$TH/.local/bin/thurm\"; ",
        "if [ -x \"$B\" ] && [ -d \"$TH/.local/bin\" ] && { [ ! -e \"$L\" ] || [ -L \"$L\" ]; }; then ln -sfn \"$B\" \"$L\"; fi; ",
        "thurm remote-info"
    );
    let out = ssh.run(FINISH, &[], None, Duration::from_secs(30))?;
    let info: ThurmInfo = json_line(&out)
        .map_err(|e| InstallError::Failed(format!("the installed thurm does not run: {e}")))?;
    if info.protocol != thurm_proto::PROTOCOL_VERSION {
        return Err(InstallError::Failed(format!(
            "the installed thurm speaks protocol {}, not {}",
            info.protocol,
            thurm_proto::PROTOCOL_VERSION
        )));
    }
    Ok(info)
}

/// The first line of `out` that parses as a `T`: the user's shell startup files may print
/// lines of their own before a command's output.
fn json_line<T: serde::de::DeserializeOwned>(out: &str) -> Result<T, String> {
    let mut first_err = None;
    for line in out.lines().filter(|l| l.trim_start().starts_with('{')) {
        match serde_json::from_str(line) {
            Ok(v) => return Ok(v),
            Err(e) => {
                first_err.get_or_insert_with(|| e.to_string());
            }
        }
    }
    Err(first_err.unwrap_or_else(|| "no output".into()))
}

fn read(p: &Path) -> Result<Vec<u8>, InstallError> {
    std::fs::read(p).map_err(|e| InstallError::Failed(format!("{}: {e}", p.display())))
}

/// Writes `data` to `~/.local/share/thurm/bin/<name>` on the host, atomically (a running
/// daemon keeps its old file). The copy is checked on the host (its length, and its SHA-256
/// where `sha256sum` exists) before it replaces anything: an upload cut short must not install
/// a truncated binary.
fn upload(ssh: &Ssh, name: &str, data: &[u8]) -> Result<(), SshError> {
    let len = data.len().to_string();
    let sum = hex(&Sha256::digest(data));
    ssh.run(
        UPLOAD,
        &[name, &len, &sum],
        Some(data),
        Duration::from_secs(600),
    )
    .map(|_| ())
}

/// `$1` the file name, `$2` its length, `$3` its SHA-256 (hex).
const UPLOAD: &str = concat!(
    "TH=\"${THURM_HOME:-$HOME}\"; D=\"$TH/.local/share/thurm/bin\"; N=\"$D/.$1.new\"; ",
    "mkdir -p \"$D\" && cat > \"$N\" || exit 1; ",
    "n=$(wc -c < \"$N\"); ",
    "if [ $n -ne \"$2\" ]; then rm -f \"$N\"; echo \"upload of $1 incomplete: $n of $2 bytes\" >&2; exit 1; fi; ",
    "if command -v sha256sum >/dev/null 2>&1 && [ \"$(sha256sum < \"$N\" | cut -c1-64)\" != \"$3\" ]; then ",
    "rm -f \"$N\"; echo \"upload of $1 is corrupt (checksum mismatch)\" >&2; exit 1; fi; ",
    "chmod 755 \"$N\" && mv -f \"$N\" \"$D/$1\""
);

/// The Linux build's `thurm` and `thurmd`, downloaded and checked, or taken from
/// `THURM_ARTIFACT_DIR` (a directory of archives, for development).
fn fetch_artifact(target: &str) -> Result<(Vec<u8>, Vec<u8>), InstallError> {
    let name = artifact_name(thurm_proto::BUILD, target);
    let archive = match std::env::var_os("THURM_ARTIFACT_DIR") {
        Some(dir) => read(&PathBuf::from(dir).join(&name))?,
        None => {
            let base = DOWNLOAD_BASE.ok_or_else(|| {
                InstallError::Failed("this build has no download location".into())
            })?;
            let expected = expected_sha256(target).ok_or_else(|| {
                InstallError::Failed(format!("this build carries no checksum for {target}"))
            })?;
            let data = download(&format!("{base}/{name}"))?;
            let got = hex(&Sha256::digest(&data));
            if !got.eq_ignore_ascii_case(expected.trim()) {
                return Err(InstallError::Failed(format!(
                    "checksum mismatch for {name}: expected {expected}, got {got}"
                )));
            }
            data
        }
    };
    unpack(&archive)
}

fn download(url: &str) -> Result<Vec<u8>, InstallError> {
    let out = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "--retry",
            "2",
            "--max-time",
            "600",
            url,
        ])
        .output()
        .map_err(|e| InstallError::Failed(format!("cannot run curl: {e}")))?;
    if !out.status.success() {
        return Err(InstallError::Failed(format!(
            "downloading {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// `thurm` and `thurmd` from a `.tar.gz` (with `tar`, which every Mac and Linux has).
fn unpack(archive: &[u8]) -> Result<(Vec<u8>, Vec<u8>), InstallError> {
    let dir = std::env::temp_dir().join(format!(
        "thurm-artifact-{}-{}",
        std::process::id(),
        crate::tunnel::now_secs()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| InstallError::Failed(e.to_string()))?;
    let result = (|| {
        let mut child = std::process::Command::new("tar")
            .args(["-xzf", "-", "-C"])
            .arg(&dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| InstallError::Failed(format!("cannot run tar: {e}")))?;
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().expect("piped");
            stdin
                .write_all(archive)
                .map_err(|e| InstallError::Failed(e.to_string()))?;
        }
        let status = child
            .wait()
            .map_err(|e| InstallError::Failed(e.to_string()))?;
        if !status.success() {
            return Err(InstallError::Failed("the archive does not unpack".into()));
        }
        let pick = |n: &str| -> Result<Vec<u8>, InstallError> {
            let mut v = Vec::new();
            std::fs::File::open(dir.join(n))
                .and_then(|mut f| f.read_to_end(&mut v))
                .map_err(|_| InstallError::Failed(format!("the archive has no {n}")))?;
            Ok(v)
        };
        Ok((pick("thurm")?, pick("thurmd")?))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeError {
    /// The daemon predates in-place upgrades: replacing it stops its panes. Ask, then call
    /// again with `allow_restart`.
    WouldStopPanes {
        protocol: u32,
    },
    Failed(String),
}

impl std::fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpgradeError::WouldStopPanes { protocol } => write!(
                f,
                "the daemon there (protocol {protocol}) cannot be upgraded in place; \
                 restarting it stops the programs in its panes"
            ),
            UpgradeError::Failed(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for UpgradeError {}

/// Replaces the host's daemon with the installed build: in place (SIGUSR2, panes keep
/// running) when it supports that, else a restart when `allow_restart`.
pub fn upgrade_daemon(
    ssh: &Ssh,
    socket: Option<&str>,
    daemon: Option<&DaemonState>,
    allow_restart: bool,
) -> Result<String, UpgradeError> {
    if let Some(d) = daemon
        && d.running
        && !d.hot_upgrade
        && !allow_restart
    {
        return Err(UpgradeError::WouldStopPanes {
            protocol: d.protocol.unwrap_or(0),
        });
    }
    const UPGRADE: &str = concat!(
        "TH=\"${THURM_HOME:-$HOME}\"; ",
        "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH; ",
        "if [ -n \"$1\" ]; then THURM_SOCKET=\"$1\"; export THURM_SOCKET; fi; ",
        "if thurm daemon status >/dev/null 2>&1 || [ $? -eq 4 ]; then thurm daemon upgrade; else echo not running; fi"
    );
    ssh.run(
        UPGRADE,
        &[socket.unwrap_or("")],
        None,
        Duration::from_secs(60),
    )
    .map(|out| out.trim().to_owned())
    .map_err(|e| UpgradeError::Failed(e.to_string()))
}

#[cfg(test)]
mod tests {
    /// A minimal 64-bit ELF with one program header of type `ptype`.
    fn elf(ptype: u32) -> Vec<u8> {
        let mut d = vec![0u8; 64 + 56];
        d[..4].copy_from_slice(b"\x7fELF");
        d[4] = 2;
        d[5] = 1;
        d[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        d[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        d[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        d[64..68].copy_from_slice(&ptype.to_le_bytes());
        d
    }

    #[test]
    fn static_elf_detection() {
        let dir = std::env::temp_dir().join(format!("thurm-elf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (st, dy, txt) = (dir.join("static"), dir.join("dynamic"), dir.join("text"));
        std::fs::write(&st, elf(1)).unwrap();
        std::fs::write(&dy, elf(3)).unwrap();
        std::fs::write(&txt, b"#!/bin/sh\n").unwrap();
        assert!(is_static_elf(&st));
        assert!(!is_static_elf(&dy));
        assert!(!is_static_elf(&txt));
        // A truncated program-header table.
        std::fs::write(&txt, &elf(1)[..80]).unwrap();
        assert!(!is_static_elf(&txt));
        if cfg!(target_os = "linux") {
            assert!(!is_static_elf(Path::new("/bin/sh")));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn linux_copies_need_static_helpers() {
        let dir = std::env::temp_dir().join(format!("thurm-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ours = crate::current_target();
        // Dynamically linked helpers: not even to the very same Linux target.
        std::fs::write(dir.join("thurm"), elf(3)).unwrap();
        std::fs::write(dir.join("thurmd"), elf(3)).unwrap();
        assert!(!copyable("x86_64-unknown-linux-musl", Some(&dir)));
        assert!(!copyable("aarch64-unknown-linux-musl", Some(&dir)));
        if ours.contains("-linux-") {
            assert!(!copyable(ours, Some(&dir)));
            std::fs::write(dir.join("thurm"), elf(1)).unwrap();
            std::fs::write(dir.join("thurmd"), elf(1)).unwrap();
            assert!(copyable(ours, Some(&dir)));
        } else {
            // macOS: the same target is enough.
            assert!(copyable(ours, None));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    #[test]
    fn artifact_names_are_safe() {
        assert_eq!(
            artifact_name("0.2.0+412", "x86_64-unknown-linux-musl"),
            "thurm-0.2.0-412-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            artifact_name("0.2.0-tip.3fa1c2e+413", "aarch64-unknown-linux-musl"),
            "thurm-0.2.0-tip.3fa1c2e-413-aarch64-unknown-linux-musl.tar.gz"
        );
    }

    #[test]
    fn daemon_status_parsed() {
        let d = parse_daemon_status(
            r#"{"running":true,"compatible":false,"error":"protocol mismatch: daemon 12, client 14"}"#,
        )
        .unwrap();
        assert_eq!(d.protocol, Some(12));
        assert!(!d.hot_upgrade);
        let d = parse_daemon_status(
            r#"{"running":true,"compatible":false,"error":"protocol mismatch: daemon 13, client 14"}"#,
        )
        .unwrap();
        assert!(d.hot_upgrade);
        let d = parse_daemon_status(
            r#"{"running":true,"pid":1,"panes":2,"restored":false,"build":"x","current":false}"#,
        )
        .unwrap();
        assert_eq!(d.protocol, Some(thurm_proto::PROTOCOL_VERSION));
        assert_eq!(d.build.as_deref(), Some("x"));
        assert!(!parse_daemon_status(r#"{"running":false}"#).unwrap().running);
        assert!(parse_daemon_status("garbage").is_none());
    }

    #[test]
    fn old_daemons_are_not_restarted_without_asking() {
        let ssh = Ssh::new("never-connected.invalid").unwrap();
        let old = DaemonState {
            running: true,
            protocol: Some(12),
            build: None,
            hot_upgrade: false,
        };
        assert_eq!(
            upgrade_daemon(&ssh, None, Some(&old), false),
            Err(UpgradeError::WouldStopPanes { protocol: 12 })
        );
    }

    #[test]
    fn archives_unpack() {
        let dir = std::env::temp_dir().join(format!("thurm-tar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("thurm"), b"cli").unwrap();
        std::fs::write(dir.join("thurmd"), b"daemon").unwrap();
        let out = std::process::Command::new("tar")
            .args(["-czf", "-", "-C"])
            .arg(&dir)
            .args(["thurm", "thurmd"])
            .output()
            .unwrap();
        let (t, d) = unpack(&out.stdout).unwrap();
        assert_eq!((t.as_slice(), d.as_slice()), (&b"cli"[..], &b"daemon"[..]));
        assert!(unpack(b"not an archive").is_err());
        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn upload_script_is_safe_to_quote() {
        crate::ssh::quote(UPLOAD).unwrap();
    }

    #[test]
    fn json_after_startup_noise() {
        #[derive(serde::Deserialize)]
        struct T {
            a: u32,
        }
        let out = "Welcome to devbox!\n{ \"motd\": broken\n{\"a\": 3}\n";
        assert_eq!(json_line::<T>(out).unwrap().a, 3);
        assert!(json_line::<T>("only text\n").is_err());
    }
}
