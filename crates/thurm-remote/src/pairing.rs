//! Pairing the iOS app with `[[remote]]` hosts (see `specs/pair-remotes.md`): what the Mac's
//! pairing code says about each host, and the phone's key in each host's `authorized_keys`.
//!
//! Host keys come from this machine's own known hosts, never from the network: the Mac already
//! trusts them, and the phone checks the host against exactly those fingerprints.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thurm_client::Client;
use thurm_config::RemoteConfig;
use thurm_proto::{Request, Response};

use crate::ssh::{RemoteExec, Ssh, SshError};

/// Comment marking a device key in `authorized_keys` (as the Mac's `PhonePairing` writes it).
pub const COMMENT_PREFIX: &str = "thurm-ios:";
/// A device may run remote commands and reach the daemon socket: no terminal, agent or X11.
pub const KEY_OPTIONS: &str = "restrict,port-forwarding";

/// A `[[remote]]` host as the pairing code carries it (`PairingPayload` in the iOS app reads
/// `name`, `host`, `port`, `user`, `host_keys` and `socket`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct HostEntry {
    pub name: String,
    /// `ssh -G`'s `hostname`: a default for the address the phone connects to.
    pub host: String,
    pub port: u16,
    pub user: String,
    /// `SHA256:` fingerprints of the host's keys, from this machine's known hosts.
    pub host_keys: Vec<String>,
    pub socket: Option<String>,
    /// The `ProxyJump` or `ProxyCommand` this machine's ssh config uses for the host: the phone
    /// can't, so its address must be one the phone reaches directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Why the host can't be in the code (no known host key, `ssh -G` failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What `ssh -G` says about a host, as far as pairing cares.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Effective {
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub host_key_alias: Option<String>,
    /// `userknownhostsfile` then `globalknownhostsfile`, `~` expanded.
    pub known_hosts: Vec<String>,
    pub proxy: Option<String>,
}

pub fn parse_effective(out: &str, home: &str) -> Effective {
    let mut e = Effective {
        port: 22,
        ..Default::default()
    };
    let (mut user_files, mut global_files) = (Vec::new(), Vec::new());
    let mut proxy_command = None;
    for line in out.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        let set = |v: &str| (!v.is_empty() && v != "none").then(|| v.to_owned());
        match key.to_ascii_lowercase().as_str() {
            "hostname" => e.hostname = value.to_owned(),
            "user" => e.user = value.to_owned(),
            "port" => e.port = value.parse().unwrap_or(22),
            "hostkeyalias" => e.host_key_alias = set(value),
            "proxyjump" => e.proxy = set(value),
            "proxycommand" => proxy_command = set(value),
            "userknownhostsfile" => user_files = files(value, home),
            "globalknownhostsfile" => global_files = files(value, home),
            _ => {}
        }
    }
    e.proxy = e.proxy.or(proxy_command);
    e.known_hosts = user_files.into_iter().chain(global_files).collect();
    e
}

fn files(value: &str, home: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|f| *f != "none")
        .map(|f| match f.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
            None => f.to_owned(),
        })
        .collect()
}

/// The name ssh looks the host up by in known hosts: the alias, else the host name, with the
/// port when it isn't 22 (`[box]:2222`).
pub fn known_hosts_name(e: &Effective) -> String {
    match &e.host_key_alias {
        Some(alias) => alias.clone(),
        None if e.port == 22 => e.hostname.clone(),
        None => format!("[{}]:{}", e.hostname, e.port),
    }
}

/// The fingerprints `ssh-keygen -F <name> -l` prints, without revoked keys and certificate
/// authorities (marked only on the `# Host … found: line N REVOKED|CA` comment before each).
pub fn parse_fingerprints(out: &str) -> (Vec<String>, Vec<String>) {
    let (mut keys, mut revoked) = (Vec::new(), Vec::new());
    let mut marker = "";
    for line in out.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            marker = line.rsplit(' ').next().unwrap_or("");
            continue;
        }
        let Some(fp) = line.split_whitespace().find(|t| t.starts_with("SHA256:")) else {
            continue;
        };
        match marker {
            "REVOKED" => revoked.push(fp.to_owned()),
            "CA" => {}
            _ => keys.push(fp.to_owned()),
        }
        marker = "";
    }
    (keys, revoked)
}

/// The host as the pairing code describes it: `ssh -G` for its address, user and port, and its
/// host keys from this machine's known hosts (hashed or not; that's what `ssh-keygen -F` is for).
pub fn host_entry(remote: &RemoteConfig, ssh: &Ssh) -> HostEntry {
    let mut entry = HostEntry {
        name: remote.name.clone(),
        socket: remote.socket.clone(),
        port: 22,
        ..Default::default()
    };
    let effective = match ssh.effective_config() {
        Ok(out) => parse_effective(&out, &home()),
        Err(e) => {
            entry.error = Some(e.to_string());
            return entry;
        }
    };
    let lookup = known_hosts_name(&effective);
    let (mut keys, mut revoked) = (Vec::new(), Vec::new());
    for file in &effective.known_hosts {
        if !Path::new(file).is_file() {
            continue;
        }
        let out = Command::new("ssh-keygen")
            .args(["-F", &lookup, "-l", "-f", file])
            .output();
        // Exit 1: not in this file.
        if let Ok(out) = out
            && out.status.success()
        {
            let (k, r) = parse_fingerprints(&String::from_utf8_lossy(&out.stdout));
            keys.extend(k);
            revoked.extend(r);
        }
    }
    let mut host_keys: Vec<String> = Vec::new();
    for k in keys {
        if !revoked.contains(&k) && !host_keys.contains(&k) {
            host_keys.push(k);
        }
    }
    if host_keys.is_empty() {
        entry.error = Some(format!(
            "no known host key for {lookup}: connect to it from this Mac once"
        ));
    }
    entry.host = effective.hostname;
    entry.port = effective.port;
    entry.user = effective.user;
    entry.proxy = effective.proxy;
    entry.host_keys = host_keys;
    entry
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

/// A device key line from the iOS app, checked like the Mac's `PhonePairing.validDeviceKey`:
/// an `ecdsa-sha2-nistp256` key with Thurm's comment, on one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceKey {
    /// `ecdsa-sha2-nistp256 AAAA…`
    pub key: String,
    pub device: String,
}

impl DeviceKey {
    pub fn parse(line: &str) -> Result<DeviceKey, String> {
        let line = line.trim();
        let invalid = || "not a Thurm device key (ecdsa-sha2-nistp256 … thurm-ios:<device>)";
        if line.chars().any(char::is_control) {
            return Err(invalid().into());
        }
        let parts: Vec<&str> = line.split(' ').collect();
        let [kind, blob, comment] = parts[..] else {
            return Err(invalid().into());
        };
        let device = comment
            .strip_prefix(COMMENT_PREFIX)
            .ok_or_else(invalid)?
            .to_owned();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(blob)
            .map_err(|_| invalid())?;
        if kind != "ecdsa-sha2-nistp256" || !is_p256_blob(&bytes) {
            return Err(invalid().into());
        }
        validate_device_name(&device)?;
        Ok(DeviceKey {
            key: format!("{kind} {blob}"),
            device,
        })
    }

    /// The line in `authorized_keys`.
    pub fn line(&self) -> String {
        format!("{KEY_OPTIONS} {} {COMMENT_PREFIX}{}", self.key, self.device)
    }
}

/// An ssh `ecdsa-sha2-nistp256` public key blob: the algorithm, the curve and an uncompressed
/// point (104 bytes). Whether the point is on the curve is sshd's to check.
fn is_p256_blob(blob: &[u8]) -> bool {
    let mut rest = blob;
    let mut string = || -> Option<&[u8]> {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let s = rest.get(4..4 + len)?;
        rest = &rest[4 + len..];
        Some(s)
    };
    let ok = string() == Some(b"ecdsa-sha2-nistp256")
        && string() == Some(b"nistp256")
        && string().is_some_and(|p| p.len() == 65 && p[0] == 4);
    ok && rest.is_empty()
}

/// A device name goes to a remote shell as an argument and into `authorized_keys`:
/// 1–128 ASCII letters, digits, `.`, `_`, or `-` (aligned with Mac `PhonePairing.validDeviceKey`).
pub fn validate_device_name(device: &str) -> Result<(), String> {
    if device.is_empty()
        || device.len() > 128
        || !device
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(format!("unsupported device name {device:?}"));
    }
    Ok(())
}

/// Legacy shell-safe validation for revoking previously paired entries whose names predated the
/// strict `[A-Za-z0-9._-]` whitelist (e.g. `iPhone+work`): one word, no quotes or backslashes.
fn validate_legacy_device_name(device: &str) -> Result<(), String> {
    if device.is_empty()
        || device.len() > 128
        || device
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\'' || c == '\\')
    {
        return Err(format!("unsupported device name {device:?}"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    Invalid(String),
    /// `authorized_keys` (or `~/.ssh`) can't be written: a link, read-only, or managed by the
    /// system (NixOS `users.users.<name>.openssh.authorizedKeys`).
    NotWritable {
        nixos: bool,
    },
    /// `no-replace` guard rejected overwriting an existing different key for the same device name.
    WouldReplace,
    /// Push-registration revocation required for SSH key replacement could not be completed.
    PushCleanup(String),
    Ssh(SshError),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingError::Invalid(m) => f.write_str(m),
            PairingError::NotWritable { .. } => f.write_str("authorized_keys is not writable"),
            PairingError::WouldReplace => {
                f.write_str("an existing different key for this device would be replaced")
            }
            PairingError::PushCleanup(m) => f.write_str(m),
            PairingError::Ssh(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for PairingError {}

/// Exit status of [`EDIT`] when `authorized_keys` can't be written.
const NOT_WRITABLE: i32 = 4;
/// Exit status of [`EDIT`] when `$3` (`no-replace`) is set and a different key for `$2` exists.
const WOULD_REPLACE: i32 = 6;

/// Replaces the device's key in `authorized_keys`: on revoke (`$1` empty), drops every line Thurm
/// wrote for device `$2`; on authorize (`$1` non-empty), drops any existing Thurm line matching
/// either device `$2` (key rotation) or key blob `$1` (device rename), so renaming a phone never
/// leaves an unrevoked duplicate line behind. Appends `$1` (when given) with Thurm's options,
/// through a temporary file and `mv`.
/// Every other line stays byte for byte, a last line without a newline included (one is put before
/// the appended key only); a revoke that matches nothing leaves the file alone. A link (managed
/// elsewhere) or an unreadable file is not touched.
const EDIT: &str = concat!(
    // A/V/KB/NR: the key, device, key blob and optional "no-replace" guard (`$1`/`$2` inside
    // `ours` would be the function's args).
    "TH=\"${THURM_HOME:-$HOME}\"; D=\"$TH/.ssh\"; F=\"$D/authorized_keys\"; T=\"$F.thurm-$$\"; N=\"\n\"; A=\"$1\"; V=\"$2\"; NR=\"$3\"; KB=\"${A#ecdsa-sha2-nistp256 }\"; ",
    "nw() { [ -e /etc/NIXOS ] && echo nixos >&2; echo \"authorized_keys is not writable\" >&2; exit 4; }; ",
    "if [ -z \"$1\" ] && [ ! -e \"$F\" ] && [ ! -L \"$F\" ]; then exit 0; fi; ",
    "if [ -L \"$F\" ]; then nw; fi; ",
    "if [ -e \"$F\" ] && { [ ! -w \"$F\" ] || [ ! -r \"$F\" ]; }; then nw; fi; ",
    "umask 077; mkdir -p \"$D\" 2>/dev/null || nw; chmod 700 \"$D\" 2>/dev/null; ",
    // One edit at a time (two at once would each drop the other's key), by one protocol for
    // every editor of the file, from any host sharing it: a symlink to the holder,
    // `<pid>@<host>`, taken by `ln -s` atomically and removed by its holder's exit traps.
    // Where `flock` exists the editor also holds a kernel lock on a file beside it, and only
    // then reclaims a link whose holder on this host is gone: other such editors wait on the
    // flock, others never remove a link not theirs, so the link can't change between the
    // check and the removal. Without `flock`, a link is never taken over; one left by a holder
    // killed outright fails the edit, naming it. Waits THURM_PAIR_LOCK_WAIT seconds;
    // THURM_PAIR_LOCK=link skips flock (tests).
    "W=\"${THURM_PAIR_LOCK_WAIT:-30}\"; I=0; H=\"$(uname -n)\"; K=\"$D/.authorized_keys.thurm-lk\"; FL=; ",
    "busy() { echo \"authorized_keys is being changed by another Thurm$1\" >&2; exit 5; }; ",
    "if [ \"${THURM_PAIR_LOCK:-}\" != link ] && command -v flock >/dev/null 2>&1; then ",
    "exec 9>>\"$D/.authorized_keys.thurm-lock\" || nw; ",
    "until flock -n 9; do I=$((I+1)); [ \"$I\" -lt \"$W\" ] || busy; sleep 1; done; FL=1; fi; ",
    "until ln -s \"$$@$H\" \"$K\" 2>/dev/null; do O=\"$(readlink \"$K\" 2>/dev/null)\"; I=$((I+1)); ",
    "[ \"$I\" -lt \"$W\" ] || busy \"; if none is, remove $K\"; ",
    "if [ -n \"$FL\" ] && [ -n \"$O\" ] && [ \"${O#*@}\" = \"$H\" ] && ! kill -0 \"${O%%@*}\" 2>/dev/null; then ",
    "rm -f \"$K\"; continue; fi; sleep 1; done; ",
    "unlock() { [ \"$(readlink \"$K\" 2>/dev/null)\" = \"$$@$H\" ] && rm -f \"$K\"; }; ",
    "trap unlock EXIT; trap \"exit 1\" INT TERM HUP; ",
    "ours() { case \"$L\" in \"restrict,port-forwarding ecdsa-sha2-nistp256 \"?*) ;; *) return 1 ;; esac; ",
    "R=\"${L#restrict,port-forwarding ecdsa-sha2-nistp256 }\"; B=\"${R%% *}\"; [ -n \"$B\" ] || return 1; ",
    "C=\"${R#\"$B\" }\"; case \"$C\" in *\" \"*) return 1 ;; thurm-ios:?*) ;; *) return 1 ;; esac; ",
    "D0=\"${C#thurm-ios:}\"; if [ -z \"$A\" ]; then [ \"$D0\" = \"$V\" ]; return $?; fi; ",
    "if [ \"$D0\" = \"$V\" ] && [ \"$B\" != \"$KB\" ]; then RK=1; return 0; fi; ",
    "[ \"$D0\" = \"$V\" ] || [ \"$B\" = \"$KB\" ]; }; ",
    // E: the line read had no newline (the last one). P: the last line written has none. M: a
    // line of the device was dropped. RK: an existing different key for the same device was replaced.
    "P=; M=; RK=; { if [ -f \"$F\" ]; then while :; do E=; IFS= read -r L || E=1; ",
    "if [ -n \"$E\" ] && [ -z \"$L\" ]; then break; fi; ",
    "if ours; then M=1; ",
    "elif [ -n \"$E\" ]; then printf \"%s\" \"$L\"; P=1; else printf \"%s$N\" \"$L\"; fi; ",
    "[ -z \"$E\" ] || break; done < \"$F\" || { rm -f \"$T\"; exit 1; }; fi; ",
    "if [ -n \"$1\" ]; then [ -z \"$P\" ] || printf \"$N\"; ",
    "printf \"%s$N\" \"restrict,port-forwarding $1 thurm-ios:$2\"; fi; ",
    "} > \"$T\" || { rm -f \"$T\"; nw; }; ",
    "if [ -n \"$NR\" ] && [ -n \"$RK\" ]; then rm -f \"$T\"; exit 6; fi; ",
    "if [ -z \"$1\" ] && [ -z \"$M\" ]; then rm -f \"$T\"; exit 0; fi; ",
    "chmod 600 \"$T\" && mv -f \"$T\" \"$F\" || { rm -f \"$T\"; nw; }; ",
    "if [ -n \"$RK\" ]; then printf \"replaced$N\"; fi"
);

/// Prints the device names of the keys Thurm wrote in `authorized_keys` (the lines [`EDIT`] would
/// replace), one per line. No file:
/// none; a file it can't read: an error (its keys may still let phones in).
const LIST: &str = concat!(
    "TH=\"${THURM_HOME:-$HOME}\"; F=\"$TH/.ssh/authorized_keys\"; N=\"\n\"; ",
    "[ -e \"$F\" ] || exit 0; ",
    "[ -r \"$F\" ] || { echo \"authorized_keys is not readable\" >&2; exit 4; }; ",
    "while IFS= read -r L || [ -n \"$L\" ]; do ",
    "case \"$L\" in \"restrict,port-forwarding ecdsa-sha2-nistp256 \"?*) ;; *) continue ;; esac; ",
    "R=\"${L#restrict,port-forwarding ecdsa-sha2-nistp256 }\"; B=\"${R%% *}\"; C=\"${R#\"$B\" }\"; ",
    "case \"$C\" in *\" \"*) ;; thurm-ios:?*) printf \"%s$N\" \"${C#thurm-ios:}\" ;; esac; done < \"$F\""
);

fn edit_inner(
    ssh: &dyn RemoteExec,
    key: &str,
    device: &str,
    no_replace: bool,
) -> Result<bool, PairingError> {
    if key.is_empty() {
        validate_legacy_device_name(device).map_err(PairingError::Invalid)?;
    } else {
        validate_device_name(device).map_err(PairingError::Invalid)?;
    }
    let guard = if no_replace { "no-replace" } else { "" };
    match ssh.run(EDIT, &[key, device, guard], None, Duration::from_secs(30)) {
        Ok(out) => Ok(out.lines().any(|l| l.trim() == "replaced")),
        Err(SshError::Remote { code, stderr }) if code == NOT_WRITABLE => {
            Err(PairingError::NotWritable {
                nixos: stderr.lines().any(|l| l.trim() == "nixos"),
            })
        }
        Err(SshError::Remote { code, .. }) if code == WOULD_REPLACE => {
            Err(PairingError::WouldReplace)
        }
        Err(e) => Err(PairingError::Ssh(e)),
    }
}

fn edit(ssh: &dyn RemoteExec, key: &str, device: &str) -> Result<bool, PairingError> {
    edit_inner(ssh, key, device, false)
}

/// Adds the device key (or replaces the device's earlier one) in the host's `authorized_keys`.
pub fn authorize(ssh: &dyn RemoteExec, key: &DeviceKey) -> Result<(), PairingError> {
    authorize_with_status(ssh, key).map(|_| ())
}

/// Like [`authorize`], also reporting whether an existing *different* public key under the same
/// device name was replaced (so callers can revoke the replaced phone's push registration).
pub fn authorize_with_status(ssh: &dyn RemoteExec, key: &DeviceKey) -> Result<bool, PairingError> {
    edit(ssh, &key.key, &key.device)
}

/// What [`authorize_with_push_cleanup`] asks of the host's daemon.
pub trait PushCleanupClient {
    /// `RevokePhone`: drops every phone connection and forgets the push registrations (and Live
    /// Activities) under the phone name `device`. The phones that still hold a key reconnect
    /// and register again; the one whose key went cannot.
    fn revoke_phone(&self, device: &str) -> Result<(), String>;
}

/// A connection to a host's `thurmd` over a short-lived `ssh -N -L` tunnel through the same
/// [`Ssh`] target the key edit goes through, torn down on drop.
pub struct RemoteDaemonSession {
    client: Arc<Client>,
    tunnel: Option<(Child, PathBuf)>,
}

impl Drop for RemoteDaemonSession {
    fn drop(&mut self) {
        self.client.force_close();
        if let Some((mut child, sock)) = self.tunnel.take() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(sock);
        }
    }
}

impl RemoteDaemonSession {
    /// Wraps a connected daemon client (for tests and callers that opened one themselves).
    pub fn from_client(client: Arc<Client>) -> Self {
        Self {
            client,
            tunnel: None,
        }
    }
}

impl PushCleanupClient for RemoteDaemonSession {
    fn revoke_phone(&self, device: &str) -> Result<(), String> {
        match self.client.request_timeout(
            Request::RevokePhone {
                name: device.to_owned(),
            },
            Some(CLEANUP_TIMEOUT),
        ) {
            Ok(Response::Ok) => Ok(()),
            Ok(other) => Err(format!("unexpected daemon response: {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The most a cleanup step (the tunnel coming up, the daemon's Hello, a request) may take.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

/// Opens a daemon client on `socket` with a bounded Hello: a socket that accepts but never
/// answers must not hang pairing.
fn cleanup_client(socket: &Path) -> Result<Arc<Client>, String> {
    let stream = thurm_client::connect_daemon(socket).map_err(|e| e.to_string())?;
    Client::from_stream_timeout(
        stream,
        "thurm-pair-cleanup",
        false,
        &[],
        Some(CLEANUP_TIMEOUT),
        |_| {},
        || {},
    )
    .map_err(|e| e.to_string())
}

/// Connects to the `thurmd` that `ssh` reaches, for push cleanup: through `ssh` itself (never a
/// tunnel or tailnet route the remote may also have, which can lead to another daemon), starting
/// the daemon first if it is stopped so its saved registrations are loaded.
pub fn connect_remote_daemon(
    ssh: &Ssh,
    host_label: &str,
    configured_socket: Option<&str>,
) -> Result<RemoteDaemonSession, String> {
    crate::ssh::prepare_dir().map_err(|e| e.to_string())?;
    let effective = ssh.effective_config().map_err(|e| e.to_string())?;
    crate::ssh::check_forwards(&effective)?;
    let remote_socket = match configured_socket {
        Some(s) => s.to_owned(),
        None => {
            let host = crate::probe(ssh).map_err(|e| e.to_string())?;
            if host.thurm.is_none() {
                return Err(format!("Thurm is not installed on {host_label}"));
            }
            host.info
                .map(|i| i.socket)
                .ok_or_else(|| format!("Thurm on {host_label} is too old; upgrade it first"))?
        }
    };
    crate::start_daemon(ssh, &remote_socket).map_err(|e| e.to_string())?;

    static NEXT_TUNNEL: AtomicU64 = AtomicU64::new(1);
    let seq = NEXT_TUNNEL.fetch_add(1, Ordering::Relaxed);
    let local_sock = thurm_config::remote_dir().join(format!(
        "pair-cleanup-{}-{seq}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&local_sock);
    let mut cmd = ssh.tunnel_command(&local_sock, &remote_socket);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start ssh tunnel to {host_label}: {e}"))?;
    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(mut pipe) = child.stderr.take() {
        let sink = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut s = sink.lock();
                s.push_str(&String::from_utf8_lossy(&buf[..n]));
                if s.len() > 8 * 1024 {
                    let cut = s.len() - 4 * 1024;
                    let cut = (cut..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(0);
                    s.drain(..cut);
                }
            }
        });
    }
    let give_up = |child: &mut Child, why: String| {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&local_sock);
        let detail = stderr.lock().trim().to_owned();
        if detail.is_empty() {
            why
        } else {
            format!("{why} ({detail})")
        }
    };

    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    let mut last_err = String::from("timed out waiting for the remote daemon tunnel");
    loop {
        if local_sock.exists() {
            match cleanup_client(&local_sock) {
                Ok(client) => {
                    return Ok(RemoteDaemonSession {
                        client,
                        tunnel: Some((child, local_sock)),
                    });
                }
                Err(e) => last_err = e,
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(give_up(
                &mut child,
                format!("ssh tunnel to {host_label} exited ({status})"),
            ));
        }
        if Instant::now() >= deadline {
            return Err(give_up(&mut child, last_err));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Phone names on hosts whose key was (or may have been) replaced but whose push cleanup has not
/// been confirmed: written before the key edit, removed once the host's daemon has revoked.
/// Cleanup is a name-wide `RevokePhone`, harmless to repeat, so an entry for an edit that never
/// happened costs nothing but a reconnect.
#[derive(Clone, Debug)]
pub struct PendingPushRevokes {
    path: PathBuf,
}

impl Default for PendingPushRevokes {
    fn default() -> Self {
        Self::new(thurm_config::remote_dir().join("pending-push-revokes.json"))
    }
}

static PENDING_REVOKES_LOCK: Mutex<()> = Mutex::new(());
static PENDING_REVOKES_TMP: AtomicU64 = AtomicU64::new(1);

impl PendingPushRevokes {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn entry(host_key: &str, device: &str) -> String {
        format!("{host_key}\0{device}")
    }

    fn load_unlocked(&self) -> Result<BTreeSet<String>, String> {
        match std::fs::read(&self.path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Writes durably (fsync, then rename): an entry must survive a crash right after.
    fn save_unlocked(&self, set: &BTreeSet<String>) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let seq = PENDING_REVOKES_TMP.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .path
            .with_extension(format!("json.tmp.{}.{seq}", std::process::id()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        f.write_all(&serde_json::to_vec(set).map_err(|e| e.to_string())?)
            .and_then(|()| f.sync_all())
            .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    /// Whether cleanup is pending for `device` on `host_key`. An unreadable record counts as
    /// pending: cleanup runs once more rather than being skipped.
    pub fn contains(&self, host_key: &str, device: &str) -> bool {
        let _guard = PENDING_REVOKES_LOCK.lock();
        self.load_unlocked()
            .map_or(true, |set| set.contains(&Self::entry(host_key, device)))
    }

    pub fn insert(&self, host_key: &str, device: &str) -> Result<(), String> {
        let _guard = PENDING_REVOKES_LOCK.lock();
        let mut set = self.load_unlocked().unwrap_or_default();
        if set.insert(Self::entry(host_key, device)) {
            self.save_unlocked(&set)?;
        }
        Ok(())
    }

    pub fn remove(&self, host_key: &str, device: &str) -> Result<(), String> {
        let _guard = PENDING_REVOKES_LOCK.lock();
        let mut set = self.load_unlocked().unwrap_or_default();
        if set.remove(&Self::entry(host_key, device)) {
            self.save_unlocked(&set)?;
        }
        Ok(())
    }
}

/// Authorizes `key` on `ssh`; when that replaces another key under the same device name, the
/// host's daemon then revokes the name (`RevokePhone`), so the phone that held the old key loses
/// its push registration and its open connections. Returns whether a key was replaced.
///
/// - A new name (or the same key again) needs no daemon, unless an earlier attempt's cleanup is
///   still pending, which this finishes.
/// - Replacing a key reaches the daemon first and changes nothing when it can't.
/// - The pending cleanup is recorded durably before the key edit, so an edit whose reply was
///   lost, or a crash after it, is cleaned up by the next attempt with the same key.
pub fn authorize_with_push_cleanup<F, C>(
    ssh: &dyn RemoteExec,
    key: &DeviceKey,
    host_label: &str,
    pending: &PendingPushRevokes,
    host_key: &str,
    open_daemon: F,
) -> Result<bool, PairingError>
where
    F: FnOnce() -> Result<C, String>,
    C: PushCleanupClient,
{
    let was_pending = pending.contains(host_key, &key.device);
    let cleanup_error = |e: String| {
        PairingError::PushCleanup(format!(
            "replaced {}'s key on {host_label}, but could not revoke the previous phone's \
             notifications ({e}); add the key again to finish",
            key.device
        ))
    };
    match edit_inner(ssh, &key.key, &key.device, true) {
        Ok(_) => {
            if !was_pending {
                return Ok(false);
            }
            let daemon = open_daemon().map_err(cleanup_error)?;
            daemon.revoke_phone(&key.device).map_err(cleanup_error)?;
            let _ = pending.remove(host_key, &key.device);
            Ok(true)
        }
        Err(PairingError::WouldReplace) => {
            let daemon = open_daemon().map_err(|e| {
                PairingError::PushCleanup(format!(
                    "{}'s key on {host_label} was not replaced: its Thurm could not be reached \
                     to revoke the previous phone ({e})",
                    key.device
                ))
            })?;
            pending.insert(host_key, &key.device).map_err(|e| {
                PairingError::PushCleanup(format!(
                    "{}'s key on {host_label} was not replaced: could not record the cleanup \
                     ({e})",
                    key.device
                ))
            })?;
            edit_inner(ssh, &key.key, &key.device, false)?;
            daemon.revoke_phone(&key.device).map_err(cleanup_error)?;
            let _ = pending.remove(host_key, &key.device);
            Ok(true)
        }
        Err(e) => Err(e),
    }
}

/// Removes the device's keys from the host's `authorized_keys`.
pub fn revoke(ssh: &dyn RemoteExec, device: &str) -> Result<(), PairingError> {
    edit(ssh, "", device).map(|_| ())
}

/// The devices with a key in the host's `authorized_keys`.
pub fn devices(ssh: &dyn RemoteExec) -> Result<Vec<String>, SshError> {
    let out = ssh.run(LIST, &[], None, Duration::from_secs(30))?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

/// What to put in the NixOS configuration when the host's keys are managed there.
pub fn nixos_snippet(key: &DeviceKey) -> String {
    format!(
        "users.users.<name>.openssh.authorizedKeys.keys = [\n  {}\n];",
        nix_string(&key.line())
    )
}

/// `s` as a Nix double-quoted string: nothing in it can end the string or interpolate.
fn nix_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace("${", "\\${");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBCX3gCbaYfg5u9YLs2Wl/JueENke93ZHXviQRq40gOlkHjaEa1MveakIBTyg0/VVXe/fFWxV3pteMd9Pe1sUf8g= thurm-ios:iPhone";

    #[test]
    fn effective_config_parsed() {
        let out = "user me\nhostname devbox.tail1234.ts.net\nport 2222\n\
                   globalknownhostsfile /etc/ssh/ssh_known_hosts /etc/ssh/ssh_known_hosts2\n\
                   userknownhostsfile ~/.ssh/known_hosts /Users/me/.ssh/known_hosts2\n\
                   proxyjump bastion\nforwardagent no\n";
        let e = parse_effective(out, "/Users/me/");
        assert_eq!(e.hostname, "devbox.tail1234.ts.net");
        assert_eq!((e.user.as_str(), e.port), ("me", 2222));
        assert_eq!(
            e.known_hosts,
            [
                "/Users/me/.ssh/known_hosts",
                "/Users/me/.ssh/known_hosts2",
                "/etc/ssh/ssh_known_hosts",
                "/etc/ssh/ssh_known_hosts2"
            ]
        );
        assert_eq!(e.proxy.as_deref(), Some("bastion"));
        assert_eq!(known_hosts_name(&e), "[devbox.tail1234.ts.net]:2222");

        let e = parse_effective("hostname box\nport 22\nproxycommand nc %h %p\n", "/h");
        assert_eq!(known_hosts_name(&e), "box");
        assert_eq!(e.proxy.as_deref(), Some("nc %h %p"));
        let e = parse_effective("hostname box\nport 2222\nhostkeyalias pi\n", "/h");
        assert_eq!(
            known_hosts_name(&e),
            "pi",
            "an alias is looked up without the port"
        );
        let e = parse_effective("hostname box\nhostkeyalias none\nproxyjump none\n", "/h");
        assert_eq!((e.host_key_alias, e.proxy, e.port), (None, None, 22));
    }

    #[test]
    fn fingerprints_parsed() {
        // `ssh-keygen -F box -l` of a plain and a hashed file look the same.
        let out = "# Host box found: line 1 \n\
                   box ED25519 SHA256:lBOVIvp/z0nqDwBoCI+aJr9wixgrTa0wELPnaW1olMM\n\
                   # Host box found: line 3 \n\
                   box ECDSA SHA256:q5OE3EiiWpgTWtcXhBrh6XKIX3ZzDNQAwQ/GQikWp3o\n";
        let (keys, revoked) = parse_fingerprints(out);
        assert_eq!(
            keys,
            [
                "SHA256:lBOVIvp/z0nqDwBoCI+aJr9wixgrTa0wELPnaW1olMM",
                "SHA256:q5OE3EiiWpgTWtcXhBrh6XKIX3ZzDNQAwQ/GQikWp3o"
            ]
        );
        assert!(revoked.is_empty());
        let out = "# Host [box]:2222 found: line 2 \n\
                   [box]:2222 RSA SHA256:abc\n\
                   # Host [box]:2222 found: line 4 REVOKED\n\
                   [box]:2222 ED25519 SHA256:old\n\
                   # Host [box]:2222 found: line 5 CA\n\
                   [box]:2222 ECDSA SHA256:ca\n";
        let (keys, revoked) = parse_fingerprints(out);
        assert_eq!(keys, ["SHA256:abc"]);
        assert_eq!(revoked, ["SHA256:old"]);
        assert_eq!(parse_fingerprints(""), (vec![], vec![]));
    }

    /// Against the real `ssh-keygen`: plain, hashed, `[host]:port` and a revoked key.
    #[test]
    fn host_keys_found_in_known_hosts() {
        let dir = std::env::temp_dir().join(format!("thurm-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut pubs = Vec::new();
        for (name, kind) in [("a", "ed25519"), ("b", "ecdsa"), ("c", "ed25519")] {
            let ok = Command::new("ssh-keygen")
                .args(["-q", "-t", kind, "-N", "", "-f"])
                .arg(dir.join(name))
                .status()
                .unwrap()
                .success();
            assert!(ok);
            let p = std::fs::read_to_string(dir.join(format!("{name}.pub"))).unwrap();
            pubs.push(p.split(' ').take(2).collect::<Vec<_>>().join(" "));
        }
        let known = dir.join("known_hosts");
        std::fs::write(
            &known,
            format!(
                "box {a}\nbox {b}\n[box]:2222 {c}\n@revoked [box]:2222 {c}\n",
                a = pubs[0],
                b = pubs[1],
                c = pubs[2]
            ),
        )
        .unwrap();
        let fp = |i: usize| {
            let out = Command::new("ssh-keygen")
                .arg("-lf")
                .arg(dir.join(format!("{}.pub", ["a", "b", "c"][i])))
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout)
                .split(' ')
                .nth(1)
                .unwrap()
                .to_owned()
        };
        let hashed = dir.join("hashed");
        std::fs::copy(&known, &hashed).unwrap();
        assert!(
            Command::new("ssh-keygen")
                .args(["-q", "-H", "-f"])
                .arg(&hashed)
                .output()
                .unwrap()
                .status
                .success()
        );
        let config = dir.join("config");
        let ssh_for = |port: u16, file: &Path| Ssh {
            target: "box".into(),
            config: config.clone(),
            program: "ssh".into(),
            extra: vec![
                "-p".into(),
                port.to_string(),
                "-o".into(),
                format!("UserKnownHostsFile={}", file.display()),
                "-o".into(),
                "GlobalKnownHostsFile=none".into(),
            ],
        };
        std::fs::write(&config, "").unwrap();
        let remote = RemoteConfig {
            name: "box".into(),
            host: "box".into(),
            socket: Some("/run/x.sock".into()),
            enabled: true,
            clipboard_read: Default::default(),
            tailnet: None,
            forward_links: true,
            daemon_id: None,
            icon: None,
        };
        for file in [&known, &hashed] {
            let e = host_entry(&remote, &ssh_for(22, file));
            assert_eq!(e.host_keys, [fp(0), fp(1)], "{}", file.display());
            assert_eq!((e.host.as_str(), e.port), ("box", 22));
            assert_eq!(e.socket.as_deref(), Some("/run/x.sock"));
            assert!(e.error.is_none());
            // The port's own entry, and its key is revoked.
            let e = host_entry(&remote, &ssh_for(2222, file));
            assert!(e.host_keys.is_empty());
            assert!(e.error.unwrap().contains("[box]:2222"));
        }
        std::fs::write(&known, format!("[box]:2222 {}\n", pubs[0])).unwrap();
        assert_eq!(
            host_entry(&remote, &ssh_for(2222, &known)).host_keys,
            [fp(0)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_keys_validated() {
        let k = DeviceKey::parse(&format!("  {KEY}\n")).unwrap();
        assert_eq!(k.device, "iPhone");
        assert!(k.key.starts_with("ecdsa-sha2-nistp256 AAAAE2"));
        assert!(
            k.line()
                .starts_with("restrict,port-forwarding ecdsa-sha2-nistp256 ")
        );
        assert!(k.line().ends_with(" thurm-ios:iPhone"));

        assert!(DeviceKey::parse(&KEY.replace("thurm-ios:", "")).is_err());
        assert!(DeviceKey::parse(&KEY.replace("thurm-ios:iPhone", "thurm-ios:")).is_err());
        assert!(DeviceKey::parse(&format!("command=\"x\" {KEY}")).is_err());
        assert!(DeviceKey::parse(&format!("{KEY}\nssh-ed25519 AAAA x")).is_err());
        assert!(DeviceKey::parse(&KEY.replace("iPhone", "i\tPhone")).is_err());
        assert!(DeviceKey::parse(&KEY.replace("iPhone", "Ann's")).is_err());
        assert!(
            DeviceKey::parse(
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINx8GUwWGlzynTf2livkowYPZiyb97ww3S3Lhj7csFN+ thurm-ios:x"
            )
            .is_err()
        );
        // 104 bytes of something else.
        let garbage = base64::engine::general_purpose::STANDARD.encode([0x41u8; 104]);
        assert!(DeviceKey::parse(&format!("ecdsa-sha2-nistp256 {garbage} thurm-ios:x")).is_err());
        // A key blob with bytes after the point.
        let parts: Vec<&str> = KEY.split(' ').collect();
        let mut blob = base64::engine::general_purpose::STANDARD
            .decode(parts[1])
            .unwrap();
        assert_eq!(blob.len(), 104);
        blob.push(0);
        let long = base64::engine::general_purpose::STANDARD.encode(&blob);
        assert!(DeviceKey::parse(&format!("{} {long} {}", parts[0], parts[2])).is_err());
    }

    /// The edit script, run by `/bin/sh` on a scratch home (the loopback tests run it over ssh).
    /// The home, and environment the remote script would get from sshd.
    struct Local(std::path::PathBuf, Vec<(&'static str, &'static str)>);

    impl RemoteExec for Local {
        fn run(
            &self,
            script: &str,
            args: &[&str],
            _input: Option<&[u8]>,
            _timeout: Duration,
        ) -> Result<String, SshError> {
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .arg("thurm")
                .args(args)
                .env("THURM_HOME", &self.0)
                .envs(self.1.iter().copied())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            match out.status.code() {
                Some(0) => Ok(stdout),
                code => Err(SshError::Remote {
                    code: code.unwrap_or(-1),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                }),
            }
        }
    }

    /// A device name can't end the Nix string or interpolate into the configuration.
    #[test]
    fn nixos_snippet_is_one_string_literal() {
        let base = DeviceKey::parse(KEY).unwrap();
        assert!(DeviceKey::parse(&KEY.replace("iPhone", "a\"${builtins.x}")).is_err());
        let k = DeviceKey {
            key: base.key.clone(),
            device: "a\"${builtins.x}".into(),
        };
        let snippet = nixos_snippet(&k);
        assert!(
            snippet.ends_with(" thurm-ios:a\\\"\\${builtins.x}\"\n];"),
            "{snippet}"
        );
        assert_eq!(nix_string("a\\b\"c${d}$e"), "\"a\\\\b\\\"c\\${d}$e\"");
        assert_eq!(
            nixos_snippet(&base),
            format!(
                "users.users.<name>.openssh.authorizedKeys.keys = [\n  \"{}\"\n];",
                base.line()
            )
        );
    }

    #[test]
    fn renaming_device_deduplicates_key_and_revokes_cleanly() {
        let home = std::env::temp_dir().join(format!("thurm-pair-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let local = Local(home.clone(), vec![]);
        let file = home.join(".ssh/authorized_keys");

        let k1 = DeviceKey::parse(KEY).unwrap();
        // Second distinct P-256 blob for a second uniquely-named phone.
        let mut blob2 = base64::engine::general_purpose::STANDARD
            .decode(KEY.split(' ').nth(1).unwrap())
            .unwrap();
        blob2[103] ^= 0x01;
        let k2 = DeviceKey::parse(&format!(
            "ecdsa-sha2-nistp256 {} thurm-ios:iPhone-9c04",
            base64::engine::general_purpose::STANDARD.encode(&blob2)
        ))
        .unwrap();

        authorize(&local, &k1).unwrap();
        authorize(&local, &k2).unwrap();
        assert_eq!(devices(&local).unwrap(), ["iPhone", "iPhone-9c04"]);

        // Renaming k1 from generic "iPhone" to unique "iPhone-7f3a" removes the old "iPhone" line
        // so revoking "iPhone-7f3a" leaves zero entries for k1.
        let renamed = DeviceKey {
            key: k1.key.clone(),
            device: "iPhone-7f3a".into(),
        };
        authorize(&local, &renamed).unwrap();
        let content = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(!lines.contains(&k1.line().as_str()));
        assert!(lines.contains(&renamed.line().as_str()));
        assert!(lines.contains(&k2.line().as_str()));

        // Revoking "iPhone-7f3a" removes only k1, preserving k2 ("iPhone-9c04").
        revoke(&local, "iPhone-7f3a").unwrap();
        assert_eq!(devices(&local).unwrap(), ["iPhone-9c04"]);
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(!content.contains(&k1.key));
        assert!(content.contains(&k2.line()));
        let _ = std::fs::remove_dir_all(&home);
    }

    const LINK: (&str, &str) = ("THURM_PAIR_LOCK", "link");

    fn has_flock() -> bool {
        Command::new("/bin/sh")
            .args(["-c", "command -v flock"])
            .output()
            .unwrap()
            .status
            .success()
    }

    fn hostname() -> String {
        let out = Command::new("uname").arg("-n").output().unwrap().stdout;
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    /// Edits at the same moment (two Macs adding phones) each keep the other's key: editors
    /// with `flock` and without it mixed on one file (hosts sharing a home), and all without.
    #[test]
    fn concurrent_edits_keep_every_key() {
        for mixed in [true, false] {
            let home =
                std::env::temp_dir().join(format!("thurm-pairlock-{}-{mixed}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).unwrap();
            let names: Vec<String> = (0..8).map(|i| format!("dev{i}")).collect();
            let threads: Vec<_> = names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let home = home.clone();
                    let env = if mixed && i % 2 == 0 {
                        vec![]
                    } else {
                        vec![LINK]
                    };
                    let mut blob = base64::engine::general_purpose::STANDARD
                        .decode(KEY.split(' ').nth(1).unwrap())
                        .unwrap();
                    blob[103] ^= (i as u8) + 1;
                    let k = DeviceKey::parse(&format!(
                        "ecdsa-sha2-nistp256 {} thurm-ios:{name}",
                        base64::engine::general_purpose::STANDARD.encode(&blob)
                    ))
                    .unwrap();
                    std::thread::spawn(move || authorize(&Local(home, env), &k).unwrap())
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            let mut listed = devices(&Local(home.clone(), vec![])).unwrap();
            listed.sort();
            assert_eq!(
                listed, names,
                "no key lost to another edit (mixed: {mixed})"
            );
            let link = home.join(".ssh/.authorized_keys.thurm-lk");
            assert!(link.symlink_metadata().is_err(), "the lock is released");
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    /// A held lock is waited for. One whose holder is gone is reclaimed only by an editor
    /// holding `flock` (two reclaiming at once could each remove the other's fresh lock), and
    /// never one held from another host.
    #[test]
    fn a_held_lock_is_waited_for_and_reclaimed_only_safely() {
        let home = std::env::temp_dir().join(format!("thurm-pairheld-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let link = ssh.join(".authorized_keys.thurm-lk");
        let file = ssh.join("authorized_keys");
        let key = |name: &str| DeviceKey::parse(&KEY.replace("iPhone", name)).unwrap();
        let host = hostname();
        let listed = |name: &str| {
            devices(&Local(home.clone(), vec![]))
                .unwrap()
                .contains(&name.to_owned())
        };

        // Held from another host, released while the edit waits: it goes ahead then, with or
        // without flock.
        for env in [vec![], vec![LINK]] {
            std::os::unix::fs::symlink("1@elsewhere", &link).unwrap();
            let released = link.clone();
            let holder = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                std::fs::remove_file(released).unwrap();
            });
            let name = format!("waited{}", env.len());
            let started = std::time::Instant::now();
            authorize(&Local(home.clone(), env), &key(&name)).unwrap();
            assert!(
                started.elapsed() >= Duration::from_millis(2500),
                "went ahead under a held lock after {:?}",
                started.elapsed()
            );
            holder.join().unwrap();
            assert!(listed(&name));
        }

        // Left by a holder on this host that is gone. Without flock: not taken over; the edit
        // gives up, names the lock, and changes nothing.
        let mut gone = Command::new("true").spawn().unwrap();
        let pid = gone.id();
        gone.wait().unwrap();
        std::os::unix::fs::symlink(format!("{pid}@{host}"), &link).unwrap();
        let before = std::fs::read_to_string(&file).unwrap();
        let env = vec![LINK, ("THURM_PAIR_LOCK_WAIT", "2")];
        let err = authorize(&Local(home.clone(), env), &key("late"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("being changed by another Thurm"), "{err}");
        assert!(err.contains(".authorized_keys.thurm-lk"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        assert!(link.symlink_metadata().is_ok(), "the lock is left alone");

        if has_flock() {
            // With flock: reclaimed at once.
            let started = std::time::Instant::now();
            authorize(&Local(home.clone(), vec![]), &key("reclaimed")).unwrap();
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(listed("reclaimed"));
            assert!(link.symlink_metadata().is_err());

            // Another process holding the flock is waited for.
            let mut holder = Command::new("flock")
                .arg(ssh.join(".authorized_keys.thurm-lock"))
                .args(["sleep", "3"])
                .spawn()
                .unwrap();
            std::thread::sleep(Duration::from_millis(500));
            let started = std::time::Instant::now();
            authorize(&Local(home.clone(), vec![]), &key("flocked")).unwrap();
            assert!(started.elapsed() >= Duration::from_secs(2));
            holder.wait().unwrap();
            assert!(listed("flocked"));
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn scripts_are_safe_to_quote() {
        crate::ssh::quote(EDIT).unwrap();
        crate::ssh::quote(LIST).unwrap();
        assert!(EDIT.contains(crate::ssh::PRELUDE));
        assert!(LIST.contains(crate::ssh::PRELUDE));
    }

    #[test]
    fn authorize_revoke_and_list_keep_other_lines() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::temp_dir().join(format!("thurm-pairhome-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let local = Local(home.clone(), vec![]);
        let file = home.join(".ssh/authorized_keys");
        let key = DeviceKey::parse(KEY).unwrap();

        // No file yet: revoking is a no-op, listing finds nothing, authorizing creates it.
        revoke(&local, "iPhone").unwrap();
        assert!(!file.exists());
        assert!(devices(&local).unwrap().is_empty());
        authorize(&local, &key).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), key.line() + "\n");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(&home.join(".ssh")), 0o700);

        // Other lines (no trailing newline, odd characters) stay as they are; a distinct key
        // under another device name is preserved.
        let other_key = "ecdsa-sha2-nistp256 AAAAotherKeyBlobForDistinctPhone";
        let old = format!("{KEY_OPTIONS} {other_key} thurm-ios:iPhone-old");
        let other = format!(
            "command=\"echo \\\"hi\\\" %s\",no-pty ssh-ed25519 AAAA  me@laptop\n  # a comment\n\n{old}"
        );
        std::fs::write(&file, &other).unwrap();
        authorize(&local, &key).unwrap();
        authorize(&local, &key).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, format!("{other}\n{}\n", key.line()));
        assert_eq!(devices(&local).unwrap(), ["iPhone-old", "iPhone"]);

        // Re-authorizing the SAME key under a new device name replaces the old line so revoking
        // the new name does not leave the same key authorized under its previous name.
        let renamed = DeviceKey {
            key: key.key.clone(),
            device: "iPhone-7f3a".into(),
        };
        authorize(&local, &renamed).unwrap();
        assert_eq!(devices(&local).unwrap(), ["iPhone-old", "iPhone-7f3a"]);
        authorize(&local, &key).unwrap();
        assert_eq!(devices(&local).unwrap(), ["iPhone-old", "iPhone"]);

        // Another device of a similar name is not this one.
        revoke(&local, "iPhone").unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{other}\n")
        );
        revoke(&local, "iPhone-old").unwrap();
        assert!(devices(&local).unwrap().is_empty());

        // Only lines Thurm wrote are the device's: anything that merely ends in
        // ` thurm-ios:iPhone` survives re-pairing and revoking, and isn't listed.
        let lookalikes = format!(
            "# retired: thurm-ios:iPhone\n\
             ssh-ed25519 AAAAC3Nz admin thurm-ios:iPhone\n\
             ssh-ed25519 AAAAC3Nz thurm-ios:iPhone\n\
             {KEY_OPTIONS} {} extra thurm-ios:iPhone\n\
             no-pty {} thurm-ios:iPhone\n",
            key.key, key.key
        );
        std::fs::write(&file, &lookalikes).unwrap();
        assert!(devices(&local).unwrap().is_empty());
        authorize(&local, &key).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{lookalikes}{}\n", key.line())
        );
        assert_eq!(devices(&local).unwrap(), ["iPhone"]);
        revoke(&local, "iPhone").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), lookalikes);
        let inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&file).unwrap());
        revoke(&local, "iPhone").unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&file).unwrap()),
            inode,
            "nothing of the device's left: the file is not rewritten"
        );
        assert!(
            std::fs::read_dir(home.join(".ssh")).unwrap().all(|e| {
                let name = e.unwrap().file_name().to_string_lossy().into_owned();
                // flock's lock file stays; it locks nothing once its holder is gone.
                !name.contains(".thurm-") || name == ".authorized_keys.thurm-lock"
            }),
            "no temporary file or link lock left"
        );

        // A last line without a newline keeps it that way: revoking another device in the
        // middle, and revoking one that isn't there (the file isn't even replaced).
        use std::os::unix::fs::MetadataExt;
        let last = "ssh-ed25519 AAAA one\nssh-rsa BBBB two";
        std::fs::write(
            &file,
            format!(
                "ssh-ed25519 AAAA one\n{KEY_OPTIONS} {} thurm-ios:iPad\nssh-rsa BBBB two",
                key.key
            ),
        )
        .unwrap();
        revoke(&local, "iPad").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), last);
        let inode = std::fs::metadata(&file).unwrap().ino();
        revoke(&local, "nowhere").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), last);
        assert_eq!(std::fs::metadata(&file).unwrap().ino(), inode);

        // An unreadable file is an error, not "no devices": its keys may still let phones in.
        std::fs::write(&file, key.line() + "\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            let err = devices(&local).unwrap_err();
            assert!(err.to_string().contains("not readable"), "{err}");
        }
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

        // Managed elsewhere: a link, or a read-only file.
        std::fs::remove_file(&file).unwrap();
        let managed = home.join("managed_keys");
        std::fs::write(&managed, "ssh-ed25519 AAAA nix\n").unwrap();
        std::os::unix::fs::symlink(&managed, &file).unwrap();
        assert!(matches!(
            authorize(&local, &key),
            Err(PairingError::NotWritable { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&managed).unwrap(),
            "ssh-ed25519 AAAA nix\n"
        );
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, "ssh-ed25519 AAAA x\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
        // root writes read-only files.
        if unsafe { libc::geteuid() } != 0 {
            let err = authorize(&local, &key).unwrap_err();
            assert_eq!(err.to_string(), "authorized_keys is not writable");
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "ssh-ed25519 AAAA x\n"
            );
        }

        assert!(matches!(
            revoke(&local, "a b"),
            Err(PairingError::Invalid(_))
        ));
        assert!(matches!(
            revoke(&local, "a'b"),
            Err(PairingError::Invalid(_))
        ));

        // Legacy paired device names accepted before the stricter whitelist (e.g. `iPhone+work`)
        // are still rejected for new keys, listed by `devices()`, and revocable via `revoke()`.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(DeviceKey::parse(&KEY.replace("iPhone", "iPhone+work")).is_err());
        let legacy_line = format!("{KEY_OPTIONS} {} thurm-ios:iPhone+work\n", key.key);
        let keep_line = "ssh-ed25519 AAAA keep@laptop\n";
        std::fs::write(&file, format!("{keep_line}{legacy_line}")).unwrap();
        assert_eq!(devices(&local).unwrap(), ["iPhone+work"]);
        revoke(&local, "iPhone+work").unwrap();
        assert!(devices(&local).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), keep_line);

        // `authorize_with_status` reports `true` only when a *different* key under the same name
        // is replaced (not on initial add, re-add of the same key, or renaming the same key).
        let mut blob2 = base64::engine::general_purpose::STANDARD
            .decode(KEY.split(' ').nth(1).unwrap())
            .unwrap();
        blob2[103] ^= 0x02;
        let other_same_name = DeviceKey::parse(&format!(
            "ecdsa-sha2-nistp256 {} thurm-ios:iPhone",
            base64::engine::general_purpose::STANDARD.encode(&blob2)
        ))
        .unwrap();
        assert!(!authorize_with_status(&local, &key).unwrap());
        assert!(!authorize_with_status(&local, &key).unwrap());
        let renamed_same = DeviceKey {
            key: key.key.clone(),
            device: "iPhone-new".into(),
        };
        assert!(!authorize_with_status(&local, &renamed_same).unwrap());
        assert!(!authorize_with_status(&local, &key).unwrap());
        assert!(authorize_with_status(&local, &other_same_name).unwrap());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn authorize_with_push_cleanup_fails_closed_records_first_and_retries() {
        use std::sync::atomic::AtomicBool;

        let home = std::env::temp_dir().join(format!("thurm-pair-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let local = Local(home.clone(), vec![]);
        let pending = PendingPushRevokes::new(home.join("pending.json"));
        let file = home.join(".ssh/authorized_keys");

        let k1 = DeviceKey::parse(KEY).unwrap();
        let mut blob2 = base64::engine::general_purpose::STANDARD
            .decode(KEY.split(' ').nth(1).unwrap())
            .unwrap();
        blob2[103] ^= 0x05;
        let k2 = DeviceKey::parse(&format!(
            "ecdsa-sha2-nistp256 {} thurm-ios:iPhone",
            base64::engine::general_purpose::STANDARD.encode(&blob2)
        ))
        .unwrap();

        #[derive(Clone, Default)]
        struct FakeDaemon {
            revoked: Arc<Mutex<Vec<String>>>,
            fail: Arc<AtomicBool>,
            /// What `pending` held when `RevokePhone` came: it is recorded before the edit.
            pending_seen: Arc<Mutex<Vec<bool>>>,
            probe: Option<(PendingPushRevokes, String)>,
        }
        impl PushCleanupClient for FakeDaemon {
            fn revoke_phone(&self, device: &str) -> Result<(), String> {
                if let Some((p, host)) = &self.probe {
                    self.pending_seen.lock().push(p.contains(host, device));
                }
                if self.fail.load(Ordering::SeqCst) {
                    return Err("the daemon went away".into());
                }
                self.revoked.lock().push(device.to_owned());
                Ok(())
            }
        }
        let host = "remote:devbox";
        let no_daemon = || -> Result<FakeDaemon, String> { panic!("no daemon needed") };

        // A new name needs no daemon.
        assert!(!authorize_with_push_cleanup(&local, &k1, "devbox", &pending, host, no_daemon).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), k1.line() + "\n");

        // Replacing it with the daemon out of reach changes nothing.
        let err = authorize_with_push_cleanup(&local, &k2, "devbox", &pending, host, || {
            Err::<FakeDaemon, _>("unreachable".into())
        })
        .unwrap_err();
        assert!(matches!(err, PairingError::PushCleanup(_)), "{err:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), k1.line() + "\n");
        assert!(!pending.contains(host, "iPhone"));

        // The key changes, the revoke fails: the cleanup stays recorded (it was before the edit).
        let fake = FakeDaemon {
            probe: Some((pending.clone(), host.to_owned())),
            ..Default::default()
        };
        fake.fail.store(true, Ordering::SeqCst);
        let err = authorize_with_push_cleanup(&local, &k2, "devbox", &pending, host, || {
            Ok(fake.clone())
        })
        .unwrap_err();
        assert!(matches!(err, PairingError::PushCleanup(_)), "{err:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), k2.line() + "\n");
        assert_eq!(*fake.pending_seen.lock(), [true]);
        assert!(pending.contains(host, "iPhone"));

        // The same key again (nothing left to replace) finishes the cleanup.
        fake.fail.store(false, Ordering::SeqCst);
        assert!(
            authorize_with_push_cleanup(&local, &k2, "devbox", &pending, host, || Ok(fake.clone()))
                .unwrap()
        );
        assert_eq!(*fake.revoked.lock(), ["iPhone"]);
        assert!(!pending.contains(host, "iPhone"));

        // Done: adding it once more needs no daemon.
        assert!(!authorize_with_push_cleanup(&local, &k2, "devbox", &pending, host, no_daemon).unwrap());

        // A replacement whose edit commits but whose reply is lost (an ssh error here) is
        // still cleaned up by the next attempt.
        let lossy = Local(home.clone(), vec![]);
        pending.insert(host, "iPhone").unwrap();
        assert!(
            authorize_with_push_cleanup(&lossy, &k2, "devbox", &pending, host, || Ok(fake.clone()))
                .unwrap()
        );
        assert_eq!(*fake.revoked.lock(), ["iPhone", "iPhone"]);

        let _ = std::fs::remove_dir_all(&home);
    }
}
