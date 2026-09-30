//! Running the system `ssh`: the generated config, the hardened options every invocation
//! carries, remote commands and the tunnel command line.
//!
//! Remote commands are `sh -c '<script>' <name> '<arg>'...`, a line every login shell (sh,
//! bash, zsh, fish) parses the same way as long as the script and the arguments contain no
//! single quote and no backslash, which [`quote`] enforces.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Where Thurm installs itself on a remote host, relative to its home.
pub const REMOTE_BIN_DIR: &str = ".local/share/thurm/bin";

/// Prepended to `PATH` in every remote script: Thurm's own install first, then the places a
/// non-interactive ssh session tends to miss (user bins, Nix profiles). `THURM_BASE_PATH`
/// replaces everything after Thurm's own directories (the loopback tests use it to keep this
/// machine's own `thurm` out of the "remote" host).
pub const REMOTE_PATH: &str = "PATH=\"$TH/.local/share/thurm/bin:$TH/.local/bin:${THURM_BASE_PATH:-$HOME/.nix-profile/bin:/etc/profiles/per-user/$USER/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/local/bin:$PATH}\"; export PATH";

/// Start of every remote script. `THURM_HOME` (set by the loopback tests through sshd's
/// `SetEnv`) stands in for the home directory Thurm installs into.
pub const PRELUDE: &str = "TH=\"${THURM_HOME:-$HOME}\"";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshError {
    /// ssh could not run or authenticate; needs the user (host key, key, 2FA...).
    Attention(String),
    /// Network trouble, worth retrying.
    Transient(String),
    /// The remote command ran and failed.
    Remote { code: i32, stderr: String },
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshError::Attention(m) | SshError::Transient(m) => f.write_str(m),
            SshError::Remote { code, stderr } => {
                write!(f, "remote command failed ({code}): {}", stderr.trim())
            }
        }
    }
}

impl std::error::Error for SshError {}

/// How to run ssh for one `[[remote]]` host.
#[derive(Debug, Clone)]
pub struct Ssh {
    /// The destination as given in the config (alias, `user@host` or `ssh://user@host:port`).
    pub target: String,
    /// `-F` file: includes the user's and the system's config, adds nothing.
    pub config: PathBuf,
    /// `ssh` executable.
    pub program: PathBuf,
    /// Extra options placed before the hardening (tests: identity, known hosts, port).
    pub extra: Vec<String>,
}

impl Ssh {
    pub fn new(target: &str) -> Result<Ssh, String> {
        validate_target(target)?;
        Ok(Ssh {
            target: target.to_owned(),
            config: generated_config_path(),
            program: PathBuf::from(std::env::var_os("THURM_SSH").unwrap_or_else(|| "ssh".into())),
            extra: std::env::var("THURM_SSH_OPTIONS")
                .ok()
                .map(|s| s.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
        })
    }

    /// Options every invocation carries. Explicit `-o` options win over the config files
    /// (ssh takes the first value it sees), so these hold whatever the user's Host blocks say.
    pub fn base_args(&self) -> Vec<String> {
        let mut a = vec!["-F".to_owned(), self.config.display().to_string()];
        a.extend(self.extra.iter().cloned());
        for o in [
            "BatchMode=yes",
            "ForwardAgent=no",
            "ForwardX11=no",
            "ForwardX11Trusted=no",
            "PermitLocalCommand=no",
            "ControlMaster=no",
            "ControlPath=none",
            // A Host block's `RemoteCommand` (`tmux new -A`) can't be combined with the
            // commands Thurm runs ("Cannot execute command-line and remote command").
            "RemoteCommand=none",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
            "ConnectTimeout=15",
        ] {
            a.push("-o".into());
            a.push(o.into());
        }
        a
    }

    /// `ssh ... -T host sh -c '<script>' thurm '<arg>'...`, no forwarding of any kind.
    pub fn command(&self, script: &str, args: &[&str]) -> Result<Command, String> {
        let mut line = format!("sh -c {} thurm", quote(script)?);
        for a in args {
            line.push(' ');
            line.push_str(&quote(a)?);
        }
        let mut cmd = Command::new(&self.program);
        cmd.args(self.base_args())
            // A command connection forwards nothing, not even what the user's config asks for.
            .args(["-o", "ClearAllForwardings=yes", "-T"])
            .arg(&self.target)
            .arg(line);
        Ok(cmd)
    }

    /// `ssh … -t host sh -c '<script>' thurm` as an argument vector, for running `script` in a
    /// terminal on this machine (a sudo password, a browser sign-in). Forwards nothing either.
    pub fn interactive_argv(&self, script: &str) -> Result<Vec<String>, String> {
        let mut argv = vec![self.program.display().to_string()];
        argv.extend(self.base_args());
        argv.extend(["-o", "ClearAllForwardings=yes", "-t"].map(str::to_owned));
        argv.push(self.target.clone());
        argv.push(format!("sh -c {} thurm", quote(script)?));
        Ok(argv)
    }

    /// Runs `script` remotely and returns its stdout. `input` is written to its stdin.
    pub fn run(
        &self,
        script: &str,
        args: &[&str],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<String, SshError> {
        let mut cmd = self
            .command(script, args)
            .map_err(|e| SshError::Attention(e.to_string()))?;
        run_command(&mut cmd, input, timeout)
    }

    /// The tunnel: forwards `local` (a Unix socket on this machine) to `remote` (the daemon's
    /// socket there) and nothing else. `ClearAllForwardings` is not used: it drops the `-L`
    /// given on the command line too (checked with OpenSSH 10.5), so [`check_forwards`]
    /// refuses hosts whose config adds remote or dynamic forwards instead. The host's own
    /// `LocalForward`s still come along; without `ExitOnForwardFailure` a port one of them
    /// can't bind is a warning, not a tunnel that fails forever (the tunnel is up once its
    /// socket answers).
    pub fn tunnel_command(&self, local: &Path, remote: &str) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(self.base_args())
            .args([
                "-N",
                "-T",
                "-o",
                "StreamLocalBindUnlink=yes",
                "-o",
                "StreamLocalBindMask=0177",
                "-L",
            ])
            .arg(format!("{}:{remote}", local.display()))
            .arg(&self.target);
        cmd
    }

    /// The effective client config for the host (`ssh -G`), for [`check_forwards`].
    pub fn effective_config(&self) -> Result<String, SshError> {
        let mut cmd = Command::new(&self.program);
        cmd.args(self.base_args()).arg("-G").arg(&self.target);
        run_command(&mut cmd, None, Duration::from_secs(10))
    }

    /// `GIT_SSH_COMMAND` for git talking to this host with the same hardening.
    pub fn git_ssh_command(&self) -> String {
        let mut parts = vec![shell_word(&self.program.display().to_string())];
        parts.extend(self.base_args().iter().map(|a| shell_word(a)));
        parts.push("-o".into());
        parts.push("ClearAllForwardings=yes".into());
        parts.join(" ")
    }

    /// A git URL for `path` (absolute) on this host.
    pub fn git_url(&self, path: &str) -> String {
        if let Some(rest) = self.target.strip_prefix("ssh://") {
            let authority = rest.split('/').next().unwrap_or(rest);
            format!("ssh://{authority}{path}")
        } else {
            format!("{}:{path}", self.target)
        }
    }
}

/// Rejects destinations ssh would take as an option, or that are not one word.
pub fn validate_target(target: &str) -> Result<(), String> {
    if target.is_empty() {
        return Err("empty ssh target".into());
    }
    if target.starts_with('-') {
        return Err(format!("ssh target {target:?} must not start with '-'"));
    }
    if target
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\'' || c == '\\')
    {
        return Err(format!(
            "ssh target {target:?} contains unsupported characters"
        ));
    }
    Ok(())
}

/// Forwardings the host's config would add to the tunnel. A remote (`-R`) or dynamic
/// (`-D`) forward would let the remote host reach into this Mac, so such hosts are refused.
pub fn check_forwards(effective: &str) -> Result<(), String> {
    let bad: Vec<&str> = effective
        .lines()
        .filter(|l| {
            let key = l
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            key == "remoteforward" || key == "dynamicforward"
        })
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the ssh config for this host sets up forwards Thurm does not allow ({}); \
             remove RemoteForward/DynamicForward from its Host block",
            bad.join(", ")
        ))
    }
}

/// `'…'`, refusing what could break out of single quotes in any login shell.
pub fn quote(s: &str) -> Result<String, String> {
    if s.contains('\'') || s.contains('\\') || s.contains('\0') {
        return Err(format!(
            "cannot pass {s:?} to a remote shell (quotes and backslashes are not supported)"
        ));
    }
    Ok(format!("'{s}'"))
}

/// A word for a POSIX shell (`GIT_SSH_COMMAND` is run by `sh`).
fn shell_word(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// `<remote dir>/ssh_config`.
pub fn generated_config_path() -> PathBuf {
    thurm_config::remote_dir().join("ssh_config")
}

/// The config every Thurm ssh reads (`-F`): the user's and the system's, nothing of its own,
/// so Host blocks, ProxyJump and the 1Password `IdentityAgent` apply as usual.
pub fn generated_config() -> String {
    let home = dirs_home();
    format!(
        "# Written by Thurm for its ssh connections to remote workspaces.\n\
         Include \"{}/.ssh/config\"\n\
         Include /etc/ssh/ssh_config\n",
        home.display()
    )
}

/// Creates the remote directory (0700) and the generated ssh config.
pub fn prepare_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = thurm_config::remote_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let path = generated_config_path();
    let want = generated_config();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(want.as_str()) {
        std::fs::write(&path, want)?;
    }
    Ok(dir)
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Runs a prepared ssh command with a deadline; stdout on success.
pub fn run_command(
    cmd: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<String, SshError> {
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| SshError::Attention(format!("cannot run ssh: {e}")))?;
    let stdin = child.stdin.take();
    let data = input.map(<[u8]>::to_vec);
    let writer = std::thread::spawn(move || {
        if let (Some(mut w), Some(d)) = (stdin, data) {
            let _ = w.write_all(&d);
        }
    });
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SshError::Transient(format!(
                    "ssh did not finish within {} s",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(SshError::Transient(e.to_string())),
        }
    };
    let _ = writer.join();
    let stdout = String::from_utf8_lossy(&out_t.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
    match status.code() {
        Some(0) => Ok(stdout),
        Some(255) => Err(classify(&stderr)),
        Some(code) => Err(SshError::Remote { code, stderr }),
        None => Err(SshError::Transient("ssh was killed".into())),
    }
}

/// ssh's own failure (exit 255): does it need the user, or will it pass?
pub fn classify(stderr: &str) -> SshError {
    let text = last_lines(stderr, 4);
    let lower = stderr.to_ascii_lowercase();
    let attention = [
        "permission denied",
        "host key verification failed",
        "no matching host key",
        "remote host identification has changed",
        "passphrase",
        "password",
        "verification code",
        "could not resolve hostname",
        "bad configuration option",
        "bad local forwarding",
        "unsupported channel type",
        "administratively prohibited",
        "too many authentication failures",
        "tailscale ssh requires an additional check",
        "no such identity",
        "check the ssh config",
        "cannot execute command-line and remote command",
    ];
    let message = if text.is_empty() {
        "ssh failed without saying why".to_owned()
    } else {
        text
    };
    if attention.iter().any(|a| lower.contains(a)) {
        let hint = if lower.contains("host key verification failed")
            || lower.contains("identification has changed")
        {
            "\nConnect once with `ssh <host>` in a terminal to check and add the host key."
        } else if lower.contains("unsupported channel type")
            || lower.contains("administratively prohibited")
        {
            "\nThe ssh server refused to forward a Unix socket (Tailscale SSH before 1.98 cannot; \
             use a regular sshd over the tailnet, and AllowStreamLocalForwarding yes)."
        } else if lower.contains("permission denied") || lower.contains("passphrase") {
            "\nThurm connects without prompting (BatchMode): use a key in an agent (the \
             1Password SSH agent works), then retry."
        } else {
            ""
        };
        SshError::Attention(format!("{message}{hint}"))
    } else {
        SshError::Transient(message)
    }
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("Warning: Permanently added"))
        .collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_command_line_quotes_safely() {
        let ssh = Ssh {
            target: "devbox".into(),
            config: "/c".into(),
            program: "ssh".into(),
            extra: vec![],
        };
        let cmd = ssh.command("echo \"$1\"", &["a b"]).unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().unwrap(), "sh -c 'echo \"$1\"' thurm 'a b'");
        assert!(args.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(args.windows(2).any(|w| w == ["-o", "ForwardAgent=no"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["-o", "ClearAllForwardings=yes"])
        );
        assert!(ssh.command("echo 'x'", &[]).is_err());
        assert!(ssh.command("true", &["a\\b"]).is_err());
    }

    #[test]
    fn tunnel_forwards_one_socket_without_agent_or_x11() {
        let ssh = Ssh {
            target: "me@box".into(),
            config: "/c".into(),
            program: "ssh".into(),
            extra: vec![],
        };
        let args: Vec<String> = ssh
            .tunnel_command(Path::new("/l.sock"), "/r.sock")
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let i = args.iter().position(|a| a == "-L").unwrap();
        assert_eq!(args[i + 1], "/l.sock:/r.sock");
        assert_eq!(args.last().unwrap(), "me@box");
        assert_eq!(args.iter().filter(|a| *a == "-L").count(), 1);
        assert!(
            !args
                .iter()
                .any(|a| a == "-R" || a == "-D" || a == "-A" || a == "-X")
        );
        for o in [
            "BatchMode=yes",
            "ForwardAgent=no",
            "ForwardX11=no",
            "RemoteCommand=none",
        ] {
            assert!(args.windows(2).any(|w| w[0] == "-o" && w[1] == o), "{o}");
        }
        // A LocalForward of the user's that can't bind must not take the tunnel down.
        assert!(!args.iter().any(|a| a.starts_with("ExitOnForwardFailure")));
        // First value wins in ssh: the hardening comes before the destination.
        assert!(args.iter().position(|a| a == "BatchMode=yes").unwrap() < args.len() - 1);
    }

    #[test]
    fn targets_validated() {
        assert!(validate_target("devbox").is_ok());
        assert!(validate_target("me@devbox.tail1234.ts.net").is_ok());
        assert!(validate_target("ssh://me@devbox:2222").is_ok());
        assert!(validate_target("-oProxyCommand=evil").is_err());
        assert!(validate_target("a b").is_err());
        assert!(validate_target("").is_err());
    }

    #[test]
    fn git_urls() {
        let mut ssh = Ssh::new("me@box").unwrap();
        assert_eq!(ssh.git_url("/home/me/m.git"), "me@box:/home/me/m.git");
        ssh.target = "ssh://me@box:2222".into();
        assert_eq!(
            ssh.git_url("/home/me/m.git"),
            "ssh://me@box:2222/home/me/m.git"
        );
        ssh.program = "/usr/bin/ssh".into();
        assert!(ssh.git_ssh_command().starts_with("/usr/bin/ssh -F "));
    }

    #[test]
    fn forwards_in_user_config_are_refused() {
        assert!(check_forwards("user me\nhostname box\nlocalforward 8080 [x]:80\n").is_ok());
        assert!(check_forwards("user me\nremoteforward 2222 [localhost]:22\n").is_err());
        assert!(check_forwards("dynamicforward 1080\n").is_err());
    }

    #[test]
    fn ssh_failures_classified() {
        assert!(matches!(
            classify("me@box: Permission denied (publickey)."),
            SshError::Attention(m) if m.contains("BatchMode")
        ));
        assert!(matches!(
            classify("Host key verification failed."),
            SshError::Attention(m) if m.contains("ssh <host>")
        ));
        assert!(matches!(
            classify("ssh: connect to host box port 22: Connection refused"),
            SshError::Transient(_)
        ));
        assert!(matches!(
            classify("channel 2: open failed: unknown channel type: unsupported channel type"),
            SshError::Attention(m) if m.contains("Tailscale")
        ));
    }
}
