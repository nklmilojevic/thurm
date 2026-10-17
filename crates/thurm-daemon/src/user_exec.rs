//! Commands of the user's own that the daemon runs: `[[commands]]` picked in the palette or on
//! a key, and `[[on]]` hooks on events.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::{pty, shell};

/// How much of a failed command's stderr is kept for the log and the notification.
const STDERR_TAIL: usize = 4096;

/// A command to start: `argv` runs through the user's login shell (so their PATH applies), in
/// `cwd`, with `env` added to the daemon's environment and `stdin` written to it.
pub struct Launch<'a> {
    pub argv: &'a [String],
    pub cwd: Option<&'a str>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
}

/// Starts `launch` and returns once it runs. A thread waits for it; when it fails, `on_failure`
/// gets the exit status and the end of its stderr.
pub fn start(launch: Launch, on_failure: impl FnOnce(String) + Send + 'static) -> io::Result<()> {
    let mut cmd = command(&launch);
    cmd.stdin(if launch.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    let mut child = pty::spawn_locked(&mut cmd)?;
    let stdin = child.stdin.take().zip(launch.stdin);
    let mut stderr = child.stderr.take();
    std::thread::Builder::new()
        .name("user-command".into())
        .spawn(move || {
            if let Some((mut pipe, data)) = stdin {
                // A command that ignores its stdin closes the pipe: that is not a failure.
                let _ = pipe.write_all(&data);
            }
            let mut err = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_end(&mut err);
            }
            match child.wait() {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    let start = err.len().saturating_sub(STDERR_TAIL);
                    let tail = String::from_utf8_lossy(&err[start..]).trim().to_owned();
                    on_failure(match status.code() {
                        Some(c) if tail.is_empty() => format!("exit {c}"),
                        Some(c) => format!("exit {c}: {tail}"),
                        None => format!("killed by a signal {tail}").trim().to_owned(),
                    });
                }
                Err(e) => on_failure(e.to_string()),
            }
        })?;
    Ok(())
}

fn command(launch: &Launch) -> Command {
    let mut words = launch.argv.iter().enumerate().map(|(i, a)| {
        // The program may be written from home, like a path in the config.
        if i == 0 && a.starts_with("~/") {
            shell::shell_quote(&thurm_config::expand_home(a).display().to_string())
        } else {
            shell::shell_quote(a)
        }
    });
    let program = words.next().unwrap_or_default();
    let args: Vec<String> = words.collect();
    // The login shell sets the user's PATH; the `thurm` CLI next to the daemon goes after it,
    // as it does in panes. `env` keeps the line the same in every shell, fish included.
    let path = match shell::cli_dir() {
        Some(dir) => format!(
            "PATH=\"$PATH\":{}",
            shell::shell_quote(&dir.display().to_string())
        ),
        None => "PATH=\"$PATH\"".to_owned(),
    };
    let line = std::iter::once(format!("exec env {path} {program}"))
        .chain(args)
        .collect::<Vec<_>>()
        .join(" ");
    let mut cmd = Command::new(shell::user_shell());
    cmd.args(["-l", "-c", &line]);
    let cwd = launch
        .cwd
        .map(thurm_config::expand_home)
        .filter(|p| p.is_dir())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.envs(launch.env.iter().map(|(k, v)| (k, v)));
    // Its own process group: a signal for the daemon's group is not for it.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn run(argv: &[&str], env: Vec<(String, String)>, stdin: Option<&str>) -> Option<String> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        // The failure callback holds the only sender: a command that succeeds drops it unused,
        // which ends the wait.
        let (tx, rx) = mpsc::channel();
        start(
            Launch {
                argv: &argv,
                cwd: None,
                env,
                stdin: stdin.map(|s| s.as_bytes().to_vec()),
            },
            move |e| {
                let _ = tx.send(Some(e));
            },
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(20)).ok().flatten()
    }

    #[test]
    fn a_failure_reports_its_status_and_stderr() {
        let e = run(&["sh", "-c", "echo broke >&2; exit 3"], vec![], None).unwrap();
        assert_eq!(e, "exit 3: broke");
    }

    #[test]
    fn the_command_gets_its_env_and_stdin_and_quoting_survives() {
        let dir = std::env::temp_dir().join(format!("thurm-user-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        let script = "read line; printf '%s|%s|%s' \"$line\" \"$THURM_EVENT\" \"$1\" > \"$2\"";
        let e = run(
            &[
                "sh",
                "-c",
                script,
                "sh",
                "it's $HOME",
                out.to_str().unwrap(),
            ],
            vec![("THURM_EVENT".into(), "agent_finished".into())],
            Some("{\"event\":1}\n"),
        );
        assert_eq!(e, None);
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "{\"event\":1}|agent_finished|it's $HOME"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
