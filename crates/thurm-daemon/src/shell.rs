//! Building the command line and environment for a new pane, including automatic shell
//! integration injection (zsh via ZDOTDIR, bash via --rcfile, fish via XDG_DATA_DIRS).

use std::path::{Path, PathBuf};

use thurm_config::{Config, Osc52Mode};
use thurm_proto::{ENV_CLIP_SHIMS, ENV_PANE_ID, ENV_SOCKET, PaneId, PaneSize};

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
    /// Passed to the shell integration, which puts it in front of its `$PATH` reports.
    pub token: &'a str,
}

/// Ghostty release matching the pinned libghostty-vt (1.3.2-dev).
const GHOSTTY_VERSION: &str = "1.3.2";

/// iTerm2's feature reporting string (<https://iterm2.com/feature-reporting/>), which
/// anstyle-progress (cargo) reads for OSC 9;4. Only what Thurm handles: no DECSLRM (attach
/// snapshots don't restore the margins), title stacks (libghostty-vt ignores them), overline,
/// sixel or iTerm2's inline files, and no Unicode width version (not pinned down).
fn term_features(cfg: &Config) -> String {
    let osc52_copy = matches!(cfg.terminal.osc52, Osc52Mode::Copy | Osc52Mode::CopyPaste);
    let notify = cfg.notifications.enabled && cfg.notifications.program_notifications;
    [
        ("T3", true),
        ("Cw", osc52_copy),
        ("M", true),
        ("Sc7", true),
        ("U", true),
        ("Ts2", true),
        ("B", true),
        ("F", true),
        ("Gs", true),
        ("Sy", true),
        ("H", true),
        ("No", notify),
        ("P", true),
    ]
    .into_iter()
    .filter_map(|(code, on)| on.then_some(code))
    .collect()
}

pub fn spawn_options(l: PaneLaunch<'_>) -> SpawnOptions {
    let shims = clip_shims(
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        cfg!(target_os = "linux"),
    );
    spawn_options_with(l, shims, |k| std::env::var(k).ok())
}

/// [`spawn_options`], with the clipboard tools' directory (if installed) and the daemon's own
/// environment (`inherited`) given.
fn spawn_options_with(
    l: PaneLaunch<'_>,
    shims: Option<PathBuf>,
    inherited: impl Fn(&str) -> Option<String>,
) -> SpawnOptions {
    let cfg = l.config;
    let mut env: Vec<(String, String)> = vec![
        ("TERM".into(), cfg.terminal.term.clone()),
        ("COLORTERM".into(), "truecolor".into()),
        // Thurm's terminal is libghostty-vt, so programs get told it is Ghostty: they gate
        // features on it (Claude Code only sends OSC 9;4 progress to Ghostty and iTerm2).
        // `[terminal.env]` can override them. Thurm itself goes by the THURM_* variables.
        ("TERM_PROGRAM".into(), "ghostty".into()),
        ("TERM_PROGRAM_VERSION".into(), GHOSTTY_VERSION.into()),
        ("TERM_FEATURES".into(), term_features(cfg)),
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
    // Last: on the PATH and display the pane ends up with, `[terminal.env]` and the client's
    // variables included.
    let shims = add_clip_shims(&mut env, shims, inherited);

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
        let line = exec_line(&line, shims);
        return SpawnOptions {
            program: shell,
            args: vec!["-l".into(), "-c".into(), line],
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
        env.push(("THURM_SHELL_TOKEN".into(), l.token.to_owned()));
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

/// The clipboard tools `thurm remote doctor` installed under `home`, for panes on a Linux host:
/// there `xclip` and the rest reach the app's clipboard instead (see [`add_clip_shims`]).
fn clip_shims(home: Option<&Path>, linux: bool) -> Option<PathBuf> {
    let dir = home?.join(thurm_proto::CLIP_SHIMS_DIR);
    (linux && dir.is_dir()).then_some(dir)
}

/// Puts `shims` first on the PATH of a pane whose `env` (later entries win over earlier ones
/// and over `inherited`, the daemon's own) has no display: a pane with X11 or Wayland keeps the
/// real tools. Returns whether it did.
fn add_clip_shims(
    env: &mut Vec<(String, String)>,
    shims: Option<PathBuf>,
    inherited: impl Fn(&str) -> Option<String>,
) -> bool {
    let value = |k: &str| {
        env.iter()
            .rev()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .or_else(|| inherited(k))
    };
    let display = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|v| value(v).is_some_and(|d| !d.is_empty()));
    let Some(dir) = shims.filter(|_| !display) else {
        return false;
    };
    let dir = dir.display().to_string();
    let path = match value("PATH").filter(|p| !p.is_empty()) {
        Some(p) => format!("{dir}:{p}"),
        None => dir.clone(),
    };
    env.push(("PATH".into(), path));
    env.push((ENV_CLIP_SHIMS.into(), dir));
    true
}

/// What a pane's command runs through when the clipboard tools apply, after the login shell's
/// profile (which may set PATH or a display anew): the shims leave PATH, and come back first
/// only while there is no display. POSIX sh, so the same in every user shell; no single quote.
const CLIP_SHIMS_SH: &str = concat!(
    // Rebuilt entry by entry, empty ones (the current directory) kept where they were.
    "set -f; p=; n=0; IFS=:; ",
    "for d in $PATH; do [ \"$d\" = \"$THURM_CLIP_SHIMS\" ] && continue; ",
    "if [ $n = 0 ]; then p=$d; else p=\"$p:$d\"; fi; n=1; done; ",
    "unset IFS; set +f; case $PATH in *:) p=\"$p:\";; esac; ",
    "if [ -z \"$DISPLAY$WAYLAND_DISPLAY\" ]; then ",
    "if [ $n = 0 ]; then p=$THURM_CLIP_SHIMS; else p=\"$THURM_CLIP_SHIMS:$p\"; fi; ",
    // With a display and nothing but the shims on PATH, PATH stays: an empty one finds nothing.
    "elif [ $n = 0 ]; then p=$PATH; fi; ",
    "PATH=$p; export PATH; exec \"$@\"",
);

/// The `-c` line a pane's explicit command runs with in the user's shell: `exec` of it, through
/// [`CLIP_SHIMS_SH`] when the clipboard tools apply (`shims`). Single-quoted, the step reads the
/// same to sh, bash, zsh, fish and csh, which quote `line` as POSIX sh does already.
fn exec_line(line: &str, shims: bool) -> String {
    if shims {
        format!("exec /bin/sh -c '{CLIP_SHIMS_SH}' sh {line}")
    } else {
        format!("exec {line}")
    }
}

/// Directory holding the `thurm` CLI next to this daemon (`Thurm.app/Contents/Helpers`).
/// The shell integration appends it to PATH so agents in panes can drive the terminal.
pub fn cli_dir() -> Option<PathBuf> {
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
            token: "abc",
        });
        assert_eq!(opts.program, "/bin/bash");
        assert!(
            opts.env
                .iter()
                .any(|(k, v)| k == "THURM_SHELL_TOKEN" && v == "abc")
        );
        assert_eq!(opts.args[0], "--rcfile");
        assert!(
            opts.env
                .iter()
                .any(|(k, v)| k == "THURM_PANE_ID" && v == "7")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn env_of(cfg: &Config) -> Vec<(String, String)> {
        let sock = PathBuf::from("/tmp/x.sock");
        spawn_options(PaneLaunch {
            id: 1,
            command: None,
            cwd: None,
            extra_env: vec![],
            size: PaneSize::default(),
            config: cfg,
            socket: &sock,
            integration_dir: None,
            token: "",
        })
        .env
    }

    fn last<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn term_features_env() {
        let env = env_of(&Config::default());
        assert_eq!(last(&env, "TERM_FEATURES"), Some("T3CwMSc7UTs2BFGsSyHNoP"));

        let mut cfg = Config::default();
        cfg.terminal.osc52 = Osc52Mode::Paste;
        cfg.notifications.program_notifications = false;
        assert_eq!(term_features(&cfg), "T3MSc7UTs2BFGsSyHP");
        cfg.terminal.osc52 = Osc52Mode::CopyPaste;
        cfg.notifications.program_notifications = true;
        cfg.notifications.enabled = false;
        assert_eq!(term_features(&cfg), "T3CwMSc7UTs2BFGsSyHP");

        cfg.terminal
            .env
            .insert("TERM_FEATURES".into(), String::new());
        assert_eq!(last(&env_of(&cfg), "TERM_FEATURES"), Some(""));
    }

    #[test]
    fn clipboard_tools_only_on_a_linux_host() {
        let home = std::env::temp_dir().join(format!("thurm-shims-{}", std::process::id()));
        assert_eq!(clip_shims(Some(&home), true), None, "not installed");
        let dir = home.join(thurm_proto::CLIP_SHIMS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(clip_shims(Some(&home), true), Some(dir));
        assert_eq!(clip_shims(Some(&home), false), None, "macOS has pbcopy");
        assert_eq!(clip_shims(None, true), None);
        let _ = std::fs::remove_dir_all(home);
    }

    /// The pane's options with the shims at `/s`, `terminal_env` in the config, `extra` from
    /// the client and `inherited` from the daemon.
    fn with_shims(
        command: Option<Vec<String>>,
        terminal_env: &[(&str, &str)],
        extra: &[(&str, &str)],
        inherited: &[(&str, &str)],
    ) -> SpawnOptions {
        let mut cfg = Config::default();
        for (k, v) in terminal_env {
            cfg.terminal.env.insert((*k).into(), (*v).into());
        }
        let sock = PathBuf::from("/tmp/x.sock");
        let inherited: Vec<(String, String)> = inherited
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        spawn_options_with(
            PaneLaunch {
                id: 1,
                command,
                cwd: None,
                extra_env: extra
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect(),
                size: PaneSize::default(),
                config: &cfg,
                socket: &sock,
                integration_dir: None,
                token: "",
            },
            Some(PathBuf::from("/s")),
            |k| {
                inherited
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.clone())
            },
        )
    }

    #[test]
    fn clipboard_tools_come_first_on_the_path_the_pane_ends_up_with() {
        let path = |o: &SpawnOptions| last(&o.env, "PATH").map(str::to_owned);
        let daemon = [("PATH", "/usr/bin")];
        assert_eq!(
            path(&with_shims(None, &[], &[], &daemon)).as_deref(),
            Some("/s:/usr/bin")
        );
        // `[terminal.env]` and the client's variables set PATH after the daemon's.
        let o = with_shims(None, &[("PATH", "/conf/bin")], &[], &daemon);
        assert_eq!(path(&o).as_deref(), Some("/s:/conf/bin"));
        let o = with_shims(
            None,
            &[("PATH", "/conf/bin")],
            &[("PATH", "/client/bin")],
            &daemon,
        );
        assert_eq!(path(&o).as_deref(), Some("/s:/client/bin"));
        assert_eq!(last(&o.env, ENV_CLIP_SHIMS), Some("/s"));
        assert_eq!(
            path(&with_shims(None, &[], &[], &[])).as_deref(),
            Some("/s")
        );
        // A display, wherever it comes from, keeps the real tools; an empty one is none.
        for (conf, client, daemon) in [
            (&[("DISPLAY", ":0")][..], &[][..], &[][..]),
            (&[], &[("WAYLAND_DISPLAY", "wayland-0")], &[]),
            (&[], &[], &[("DISPLAY", ":1")]),
        ] {
            let o = with_shims(None, conf, client, daemon);
            assert_eq!(
                last(&o.env, ENV_CLIP_SHIMS),
                None,
                "{conf:?} {client:?} {daemon:?}"
            );
            assert_eq!(last(&o.env, "PATH"), None);
        }
        let o = with_shims(None, &[("DISPLAY", "")], &[], &[("DISPLAY", ":1")]);
        assert_eq!(
            last(&o.env, ENV_CLIP_SHIMS),
            Some("/s"),
            "DISPLAY emptied in the config"
        );
    }

    /// With a display and nothing but the shims on PATH, the step leaves PATH as it was: an
    /// empty one would find no command at all.
    #[test]
    fn the_step_never_leaves_path_empty() {
        let line = exec_line(
            &["/bin/sh", "-c", "printf %s \"$PATH\""]
                .map(shell_quote)
                .join(" "),
            true,
        );
        for (path, display, expected) in [
            ("/s", ":0", "/s"),
            ("/s:/s", "", "/s"),
            ("/s:/a", ":0", "/a"),
            ("/s", "", "/s"),
        ] {
            let out = std::process::Command::new("/bin/sh")
                .args(["-c", &line])
                .env_clear()
                .env("PATH", path)
                .env("DISPLAY", display)
                .env("THURM_CLIP_SHIMS", "/s")
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                expected,
                "{path} {display:?}"
            );
        }
    }

    #[test]
    fn an_explicit_command_puts_the_tools_first_after_the_profile() {
        let o = with_shims(Some(vec!["xclip".into(), "-o".into()]), &[], &[], &[]);
        assert!(o.args[2].ends_with("' sh xclip -o"), "{:?}", o.args);
        assert_eq!(exec_line("x", false), "exec x");
        assert!(!CLIP_SHIMS_SH.contains('\''), "single-quoted as a whole");
        // Run by each shell there is, with another xclip first on PATH, the shims twice in it,
        // and with and without a display the profile would have set.
        let dir = std::env::temp_dir().join(format!("thurm-exec-line-{}", std::process::id()));
        let (shims, real) = (dir.join("shi*ms"), dir.join("real"));
        for d in [&shims, &real] {
            std::fs::create_dir_all(d).unwrap();
            let tool = d.join("xclip");
            std::fs::write(&tool, "#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!(
            ":{r}:{s}:{s}:/usr/bin:/bin",
            r = real.display(),
            s = shims.display()
        );
        let line = exec_line(
            &["/bin/sh", "-c", "command -v xclip; echo \"$PATH\""]
                .map(shell_quote)
                .join(" "),
            true,
        );
        let mut ran = 0;
        for shell in ["sh", "bash", "zsh", "dash", "fish", "tcsh", "csh"]
            .iter()
            .filter_map(|n| {
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                    .chain([PathBuf::from("/bin")])
                    .map(|d| d.join(n))
                    .find(|p| p.is_file())
            })
        {
            for display in [
                None,
                Some(("DISPLAY", ":9")),
                Some(("WAYLAND_DISPLAY", "w-0")),
            ] {
                let mut cmd = std::process::Command::new(&shell);
                cmd.args(["-c", &line])
                    .env_clear()
                    .env("PATH", &path)
                    .env("THURM_CLIP_SHIMS", &shims)
                    .env("HOME", &dir)
                    .env("ZDOTDIR", &dir)
                    // As under a login shell that set up a NixOS or nix-darwin environment:
                    // their system files otherwise set PATH anew for every shell.
                    .env("__NIX_DARWIN_SET_ENVIRONMENT_DONE", "1")
                    .env("__NIXOS_SET_ENVIRONMENT_DONE", "1");
                if let Some((k, v)) = display {
                    cmd.env(k, v);
                }
                let out = cmd.output().unwrap();
                let out = String::from_utf8_lossy(&out.stdout).into_owned();
                let mut lines = out.lines();
                let (found, new_path) = (lines.next().unwrap_or(""), lines.next().unwrap_or(""));
                let s = shims.display().to_string();
                let entries = new_path.split(':').filter(|e| *e == s).count();
                // fish spells it `.` when it reads PATH: the same directory.
                assert_eq!(
                    new_path
                        .split(':')
                        .filter(|e| e.is_empty() || *e == ".")
                        .count(),
                    1,
                    "{shell:?}: the empty entry kept: {new_path}"
                );
                if display.is_none() {
                    assert_eq!(
                        found,
                        shims.join("xclip").display().to_string(),
                        "{shell:?}"
                    );
                    assert_eq!(entries, 1, "{shell:?}: once on PATH: {new_path}");
                } else {
                    assert_eq!(
                        found,
                        real.join("xclip").display().to_string(),
                        "{shell:?} {display:?}"
                    );
                    assert_eq!(entries, 0, "{shell:?} {display:?}: off PATH: {new_path}");
                }
            }
            ran += 1;
        }
        assert!(ran > 0, "no shell to run the line with");
        let _ = std::fs::remove_dir_all(dir);
    }
}
