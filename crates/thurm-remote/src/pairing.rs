//! Pairing the iOS app with `[[remote]]` hosts (see `specs/pair-remotes.md`): what the Mac's
//! pairing code says about each host, and the phone's key in each host's `authorized_keys`.
//!
//! Host keys come from this machine's own known hosts, never from the network: the Mac already
//! trusts them, and the phone checks the host against exactly those fingerprints.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use thurm_config::RemoteConfig;

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

/// A device name goes to a remote shell as an argument: one word, no quotes or backslashes.
pub fn validate_device_name(device: &str) -> Result<(), String> {
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
    Ssh(SshError),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingError::Invalid(m) => f.write_str(m),
            PairingError::NotWritable { .. } => f.write_str("authorized_keys is not writable"),
            PairingError::Ssh(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for PairingError {}

/// Exit status of [`EDIT`] when `authorized_keys` can't be written.
const NOT_WRITABLE: i32 = 4;

/// Replaces the device's key in `authorized_keys`: drops the lines ending in ` thurm-ios:$2`,
/// appends `$1` (when given) with Thurm's options, through a temporary file and `mv`. Every other
/// line stays as it is. A link (managed elsewhere) or an unreadable file is not touched.
const EDIT: &str = concat!(
    "TH=\"${THURM_HOME:-$HOME}\"; D=\"$TH/.ssh\"; F=\"$D/authorized_keys\"; T=\"$F.thurm-$$\"; N=\"\n\"; ",
    "nw() { [ -e /etc/NIXOS ] && echo nixos >&2; echo \"authorized_keys is not writable\" >&2; exit 4; }; ",
    "if [ -z \"$1\" ] && [ ! -e \"$F\" ] && [ ! -L \"$F\" ]; then exit 0; fi; ",
    "if [ -L \"$F\" ]; then nw; fi; ",
    "if [ -e \"$F\" ] && { [ ! -w \"$F\" ] || [ ! -r \"$F\" ]; }; then nw; fi; ",
    "umask 077; mkdir -p \"$D\" 2>/dev/null || nw; chmod 700 \"$D\" 2>/dev/null; ",
    "{ if [ -f \"$F\" ]; then while IFS= read -r L || [ -n \"$L\" ]; do ",
    "case \"$L\" in *\" thurm-ios:$2\") ;; *) printf \"%s$N\" \"$L\" ;; esac; done < \"$F\" || { rm -f \"$T\"; exit 1; }; fi; ",
    "if [ -n \"$1\" ]; then printf \"%s$N\" \"restrict,port-forwarding $1 thurm-ios:$2\"; fi; ",
    "} > \"$T\" || { rm -f \"$T\"; nw; }; ",
    "chmod 600 \"$T\" && mv -f \"$T\" \"$F\" || { rm -f \"$T\"; nw; }"
);

/// Prints the device names of the `thurm-ios:` keys in `authorized_keys`, one per line.
const LIST: &str = concat!(
    "TH=\"${THURM_HOME:-$HOME}\"; F=\"$TH/.ssh/authorized_keys\"; N=\"\n\"; ",
    "[ -r \"$F\" ] || exit 0; ",
    "while IFS= read -r L || [ -n \"$L\" ]; do C=\"${L##* }\"; ",
    "case \"$C\" in thurm-ios:?*) printf \"%s$N\" \"${C#thurm-ios:}\" ;; esac; done < \"$F\""
);

fn edit(ssh: &dyn RemoteExec, key: &str, device: &str) -> Result<(), PairingError> {
    validate_device_name(device).map_err(PairingError::Invalid)?;
    match ssh.run(EDIT, &[key, device], None, Duration::from_secs(30)) {
        Ok(_) => Ok(()),
        Err(SshError::Remote { code, stderr }) if code == NOT_WRITABLE => {
            Err(PairingError::NotWritable {
                nixos: stderr.lines().any(|l| l.trim() == "nixos"),
            })
        }
        Err(e) => Err(PairingError::Ssh(e)),
    }
}

/// Adds the device key (or replaces the device's earlier one) in the host's `authorized_keys`.
pub fn authorize(ssh: &dyn RemoteExec, key: &DeviceKey) -> Result<(), PairingError> {
    edit(ssh, &key.key, &key.device)
}

/// Removes the device's keys from the host's `authorized_keys`.
pub fn revoke(ssh: &dyn RemoteExec, device: &str) -> Result<(), PairingError> {
    edit(ssh, "", device)
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
        "users.users.<name>.openssh.authorizedKeys.keys = [\n  \"{}\"\n];",
        key.line()
    )
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
    struct Local(std::path::PathBuf);

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
        let local = Local(home.clone());
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

        // Other lines (no trailing newline, odd characters) stay as they are.
        let other = "command=\"echo \\\"hi\\\" %s\",no-pty ssh-ed25519 AAAA  me@laptop\n  # a comment\n\nssh-rsa BBBB x thurm-ios:iPhone-old";
        std::fs::write(&file, other).unwrap();
        authorize(&local, &key).unwrap();
        authorize(&local, &key).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, format!("{other}\n{}\n", key.line()));
        assert_eq!(devices(&local).unwrap(), ["iPhone-old", "iPhone"]);

        // Another device of a similar name is not this one.
        revoke(&local, "iPhone").unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{other}\n")
        );
        revoke(&local, "iPhone-old").unwrap();
        assert!(devices(&local).unwrap().is_empty());
        assert!(
            std::fs::read_dir(home.join(".ssh")).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".thurm-")),
            "no temporary file left"
        );

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
        let _ = std::fs::remove_dir_all(&home);
    }
}
