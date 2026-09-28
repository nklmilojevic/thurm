//! Building the command line and environment for a new pane, including automatic shell
//! integration injection (zsh via ZDOTDIR, bash via --rcfile, fish via XDG_DATA_DIRS).

use std::path::{Path, PathBuf};

use thurm_config::Config;
use thurm_proto::{ENV_PANE_ID, ENV_SOCKET, PaneId, PaneSize};

use crate::pty::SpawnOptions;

const ZSHENV: &str = include_str!("../../../shell-integration/zsh/.zshenv");
const ZSH: &str = include_str!("../../../shell-integration/zsh/thurm.zsh");
const BASH: &str = include_str!("../../../shell-integration/bash/thurm.bash");
const FISH: &str = include_str!("../../../shell-integration/fish/vendor_conf.d/thurm.fish");

/// Write the integration scripts to `<state>/shell-integration` and return the directory.
pub fn install_integration(state_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = state_dir.join("shell-integration");
    let files: [(&str, &str); 4] = [
        ("zsh/.zshenv", ZSHENV),
        ("zsh/thurm.zsh", ZSH),
        ("bash/thurm.bash", BASH),
        ("fish/vendor_conf.d/thurm.fish", FISH),
    ];
    for (rel, content) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::read_to_string(&path).ok().as_deref() != Some(content) {
            std::fs::write(&path, content)?;
        }
    }
    Ok(dir)
}

/// The user's login shell: the account database first (like Terminal.app and Ghostty), since
/// `$SHELL` is whatever the process that started the app or daemon happened to run in.
pub fn user_shell() -> String {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null()
            && !(*pw).pw_shell.is_null()
            && let Ok(s) = std::ffi::CStr::from_ptr((*pw).pw_shell).to_str()
            && !s.is_empty()
        {
            return s.to_owned();
        }
    }
    if let Ok(s) = std::env::var("SHELL")
        && !s.is_empty()
    {
        return s;
    }
    "/bin/sh".to_owned()
}

fn user_name() -> Option<String> {
    if let Ok(u) = std::env::var("USER") {
        return Some(u);
    }
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*pw).pw_name)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

pub struct PaneLaunch<'a> {
    pub id: PaneId,
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub extra_env: Vec<(String, String)>,
    pub size: PaneSize,
    pub config: &'a Config,
    pub socket: &'a Path,
    pub integration_dir: Option<&'a Path>,
}

/// Ghostty release matching the pinned libghostty-vt (1.3.2-dev).
const GHOSTTY_VERSION: &str = "1.3.2";

pub fn spawn_options(l: PaneLaunch<'_>) -> SpawnOptions {
    let cfg = l.config;
    let mut env: Vec<(String, String)> = vec![
        ("TERM".into(), cfg.terminal.term.clone()),
        ("COLORTERM".into(), "truecolor".into()),
        // Thurm's terminal is libghostty-vt, so programs get told it is Ghostty: they gate
        // features on it (Claude Code only sends OSC 9;4 progress to Ghostty and iTerm2).
        // `[terminal.env]` can override both. Thurm itself goes by the THURM_* variables.
        ("TERM_PROGRAM".into(), "ghostty".into()),
        ("TERM_PROGRAM_VERSION".into(), GHOSTTY_VERSION.into()),
        (ENV_PANE_ID.into(), l.id.to_string()),
        (ENV_SOCKET.into(), l.socket.display().to_string()),
    ];
    if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
        env.push(("LANG".into(), "en_US.UTF-8".into()));
    }
    if let Some(dir) = cli_dir() {
        env.push(("THURM_BIN_DIR".into(), dir.display().to_string()));
    }
    for (k, v) in &cfg.terminal.env {
        env.push((k.clone(), v.clone()));
    }
    env.extend(l.extra_env);

    let cwd = l
        .cwd
        .map(|c| thurm_config::expand_home(&c))
        .filter(|p| p.is_dir())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));

    // Explicit command: run it directly (through the user's shell so PATH from their
    // profile applies and quoting works like in an interactive shell).
    if let Some(cmd) = l.command.filter(|c| !c.is_empty()) {
        let shell = user_shell();
        let line = cmd
            .iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        return SpawnOptions {
            program: shell,
            args: vec!["-l".into(), "-c".into(), format!("exec {line}")],
            argv0: None,
            cwd,
            env,
            size: l.size,
        };
    }

    let (program, mut args) = match &cfg.terminal.shell {
        Some(v) if !v.is_empty() => (v[0].clone(), v[1..].to_vec()),
        _ => (user_shell(), Vec::new()),
    };
    let name = program.rsplit('/').next().unwrap_or(&program).to_owned();
    let mut login = cfg.terminal.shell.is_none();

    if cfg.terminal.shell_integration
        && let Some(dir) = l.integration_dir
    {
        env.push(("THURM_SHELL_INTEGRATION".into(), dir.display().to_string()));
        match name.as_str() {
            "zsh" => {
                if let Ok(orig) = std::env::var("ZDOTDIR") {
                    env.push(("THURM_ORIG_ZDOTDIR".into(), orig));
                }
                env.push(("ZDOTDIR".into(), dir.join("zsh").display().to_string()));
            }
            "bash" if args.is_empty() => {
                // bash ignores --rcfile for login shells; our rcfile emulates login startup.
                if login {
                    env.push(("THURM_BASH_LOGIN".into(), "1".into()));
                }
                login = false;
                args = vec![
                    "--rcfile".into(),
                    dir.join("bash/thurm.bash").display().to_string(),
                    "-i".into(),
                ];
            }
            "fish" => {
                let orig = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
                env.push(("THURM_ORIG_XDG_DATA_DIRS".into(), orig.clone()));
                let base = if orig.is_empty() {
                    "/usr/local/share:/usr/share".to_owned()
                } else {
                    orig
                };
                env.push(("XDG_DATA_DIRS".into(), format!("{}:{base}", dir.display())));
            }
            _ => {}
        }
    }

    // On macOS, go through login(1) like Terminal.app so the session is registered and the
    // shell is a login shell.
    #[cfg(target_os = "macos")]
    if cfg.terminal.shell.is_none()
        && let Some(user) = user_name()
    {
        let exe = if login {
            format!("exec -a -{name} {}", shell_quote(&program))
        } else {
            format!("exec {}", shell_quote(&program))
        };
        let mut script = exe;
        for a in &args {
            script.push(' ');
            script.push_str(&shell_quote(a));
        }
        let hush = std::env::var_os("HOME")
            .map(|h| Path::new(&h).join(".hushlogin").exists())
            .unwrap_or(false);
        let flags = if hush { "-qflp" } else { "-flp" };
        return SpawnOptions {
            program: "/usr/bin/login".into(),
            args: vec![flags.into(), user, "/bin/zsh".into(), "-fc".into(), script],
            argv0: None,
            cwd,
            env,
            size: l.size,
        };
    }
    let _ = user_name;

    SpawnOptions {
        argv0: login.then(|| format!("-{name}")),
        program,
        args,
        cwd,
        env,
        size: l.size,
    }
}

/// Directory holding the `thurm` CLI next to this daemon (`Thurm.app/Contents/Helpers`).
/// The shell integration appends it to PATH so agents in panes can drive the terminal.
fn cli_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    dir.join("thurm").is_file().then_some(dir)
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
    {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn integration_env() {
        let dir = std::env::temp_dir().join(format!("thurm-shell-test-{}", std::process::id()));
        let integ = install_integration(&dir).unwrap();
        assert!(integ.join("zsh/.zshenv").exists());
        let mut cfg = Config::default();
        cfg.terminal.shell = Some(vec!["/bin/bash".into()]);
        let sock = PathBuf::from("/tmp/x.sock");
        let opts = spawn_options(PaneLaunch {
            id: 7,
            command: None,
            cwd: Some("/".into()),
            extra_env: vec![],
            size: PaneSize::default(),
            config: &cfg,
            socket: &sock,
            integration_dir: Some(&integ),
        });
        assert_eq!(opts.program, "/bin/bash");
        assert_eq!(opts.args[0], "--rcfile");
        assert!(
            opts.env
                .iter()
                .any(|(k, v)| k == "THURM_PANE_ID" && v == "7")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
