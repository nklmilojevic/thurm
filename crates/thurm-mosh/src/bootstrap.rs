//! Starting `mosh-server` over ssh: `mosh-server new` prints `MOSH CONNECT <port> <key>`,
//! detaches, and waits for the client's first datagram.
//!
//! It runs in the user's login shell (`"$SHELL" -lc`), whatever that is (sh, bash, zsh, fish),
//! so a `mosh-server` from Homebrew or Nix is on its PATH; the command line uses nothing those
//! shells read differently. When it is not found there, the usual install locations are tried.

/// Where `mosh-server` usually is when the login shell's PATH misses it.
const FALLBACKS: &[&str] = &[
    "/opt/homebrew/bin/mosh-server",
    "/usr/local/bin/mosh-server",
    "~/.nix-profile/bin/mosh-server",
    "/run/current-system/sw/bin/mosh-server",
    "/nix/var/nix/profiles/default/bin/mosh-server",
];

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `mosh-server`'s path on the host; looked up when unset.
    pub server: Option<String>,
    /// The UDP port (`60001`) or range (`60000:61000`) for the server to listen on.
    pub ports: Option<String>,
}

/// A server no client has reached for this long exits (`MOSH_SERVER_NETWORK_TMOUT`): an app
/// that iOS ended can't come back to its sessions, whose keys it held in memory.
pub const SERVER_TIMEOUT_SECS: u32 = 3 * 24 * 3600;

/// Where the server listens, and the session key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub port: u16,
    pub key: [u8; 16],
    /// The detached server's process, to end it with the session.
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapError {
    /// No `mosh-server` on the host; how to install it there, when the host's system is known.
    NotFound { hint: Option<String> },
    /// The server path or port range has characters the command line can't carry.
    Invalid(String),
    /// `mosh-server` ran and did not start a session: its output.
    Failed(String),
    /// Running the command over ssh failed.
    Exec(String),
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootstrapError::NotFound { hint: Some(h) } => {
                write!(
                    f,
                    "mosh-server is not installed on this host; install it with: {h}"
                )
            }
            BootstrapError::NotFound { hint: None } => {
                f.write_str("mosh-server is not installed on this host")
            }
            BootstrapError::Invalid(m) => f.write_str(m),
            BootstrapError::Failed(out) => write!(f, "mosh-server did not start: {out}"),
            BootstrapError::Exec(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

fn plain(s: &str, extra: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-/+".contains(c) || extra.contains(c))
}

/// The command line that starts `server` (a path or a bare name) with `ports`.
pub fn command(server: &str, ports: Option<&str>, locale: &str) -> String {
    let mut line = format!(
        "env MOSH_SERVER_NETWORK_TMOUT={SERVER_TIMEOUT_SECS} {server} new -s -c 256 \
         -l LANG={locale} -l TERM_PROGRAM=thurm"
    );
    if let Some(p) = ports {
        line.push_str(" -p ");
        line.push_str(p);
    }
    format!("\"$SHELL\" -lc \"{line}\"")
}

/// The port and key from `mosh-server new`'s output.
pub fn parse_connect(output: &str) -> Option<Result<Connect, String>> {
    let line = output
        .lines()
        .find_map(|l| l.trim().strip_prefix("MOSH CONNECT "))?;
    let mut parts = line.split_whitespace();
    let port = parts.next().and_then(|p| p.parse::<u16>().ok());
    let key = parts.next().map(crate::crypto::parse_key);
    let pid = output.lines().find_map(|l| {
        l.trim()
            .strip_prefix("[mosh-server detached, pid = ")?
            .strip_suffix(']')?
            .trim()
            .parse()
            .ok()
    });
    Some(match (port, key) {
        (Some(port), Some(Ok(key))) => Ok(Connect { port, key, pid }),
        (_, Some(Err(e))) => Err(e),
        _ => Err(format!("unexpected MOSH CONNECT line: {line}")),
    })
}

fn not_found(code: Option<u32>, out: &str) -> bool {
    code == Some(127)
        || out.contains("command not found")
        || out.contains("No such file or directory")
        || out.contains("Unknown command")
        || out.contains("not found")
}

fn bad_locale(out: &str) -> bool {
    out.contains("UTF-8") && out.to_ascii_lowercase().contains("locale")
}

fn trimmed(out: &str) -> String {
    let out = out.trim();
    match out.char_indices().nth(500) {
        Some((i, _)) => format!("{}…", &out[..i]),
        None => out.to_owned(),
    }
}

/// How to install mosh on a host, from its `uname -s` and `/etc/os-release`.
pub fn install_hint(uname: &str, os_release: &str) -> Option<String> {
    let field = |name: &str| {
        os_release.lines().find_map(|l| {
            l.strip_prefix(name)
                .and_then(|v| v.strip_prefix('='))
                .map(|v| v.trim_matches('"').to_ascii_lowercase())
        })
    };
    let ids: Vec<String> = [field("ID"), field("ID_LIKE")]
        .into_iter()
        .flatten()
        .flat_map(|v| v.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        .collect();
    let has = |id: &str| ids.iter().any(|i| i == id);
    let hint = match uname.trim() {
        "Darwin" => "brew install mosh",
        "FreeBSD" => "sudo pkg install mosh",
        "OpenBSD" => "doas pkg_add mosh",
        _ if has("nixos") => {
            "add pkgs.mosh to environment.systemPackages (or programs.mosh.enable)"
        }
        _ if has("debian") || has("ubuntu") => "sudo apt install mosh",
        _ if has("fedora") || has("rhel") || has("centos") => "sudo dnf install mosh",
        _ if has("arch") => "sudo pacman -S mosh",
        _ if has("alpine") => "sudo apk add mosh",
        _ if has("suse") || has("opensuse") => "sudo zypper install mosh",
        _ if has("void") => "sudo xbps-install mosh",
        _ if has("gentoo") => "sudo emerge net-misc/mosh",
        _ => return None,
    };
    Some(hint.to_owned())
}

/// What running a command line over ssh gave: its exit code and output (stdout and stderr
/// together).
pub type ExecResult = Result<(Option<u32>, String), String>;

/// Starts a session with `exec`, which runs a command line over ssh.
pub fn start(exec: &dyn Fn(&str) -> ExecResult, opts: &Options) -> Result<Connect, BootstrapError> {
    if let Some(p) = &opts.ports
        && !plain(p, ":")
    {
        return Err(BootstrapError::Invalid(format!(
            "the mosh port range '{p}' is not a port or a range like 60000:61000"
        )));
    }
    let candidates: Vec<&str> = match opts.server.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => {
            if !plain(s, "~") {
                return Err(BootstrapError::Invalid(format!(
                    "the mosh-server path '{s}' has characters other than letters, digits and _.-/+~"
                )));
            }
            vec![s]
        }
        _ => std::iter::once("mosh-server")
            .chain(FALLBACKS.iter().copied())
            .collect(),
    };
    for server in candidates {
        let mut locale = "en_US.UTF-8";
        loop {
            let line = command(server, opts.ports.as_deref(), locale);
            let (code, out) = exec(&line).map_err(BootstrapError::Exec)?;
            if let Some(connect) = parse_connect(&out) {
                return connect.map_err(BootstrapError::Failed);
            }
            if not_found(code, &out) {
                break;
            }
            if bad_locale(&out) && locale != "C.UTF-8" {
                locale = "C.UTF-8";
                continue;
            }
            return Err(BootstrapError::Failed(trimmed(&out)));
        }
    }
    let hint = exec("uname -s; cat /etc/os-release 2>/dev/null")
        .ok()
        .and_then(|(_, out)| {
            let (uname, rest) = out.split_once('\n').unwrap_or((&out, ""));
            install_hint(uname, rest)
        });
    Err(BootstrapError::NotFound { hint })
}
