//! Commands of the user's own that the daemon runs: `[[commands]]` picked in the palette or on
//! a key, and `[[on]]` hooks on events.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use thurm_proto::ext::ExecResult;

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
            // Its own thread: a command that logs before it reads its stdin would otherwise
            // wait on a full stderr pipe while this one waits on a full stdin pipe.
            let writer = stdin.map(|(mut pipe, data)| {
                std::thread::spawn(move || {
                    // A command that ignores its stdin closes the pipe: that is not a failure.
                    let _ = pipe.write_all(&data);
                })
            });
            let err = stderr.as_mut().map(tail).unwrap_or_default();
            if let Some(w) = writer {
                let _ = w.join();
            }
            match child.wait() {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    let tail = String::from_utf8_lossy(&err).trim().to_owned();
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

/// The most of each of stdout and stderr that [`run`] keeps.
const CAPTURE_MAX: usize = 16 << 20;

/// How long a finished program's output may stay open (a program it left running holds it)
/// before that program is stopped.
const LEFT_BEHIND: Duration = Duration::from_secs(1);

/// How long a command stopped with SIGTERM has before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Runs `launch` to its end and returns what it printed. Past `timeout` its process group gets
/// SIGTERM, then SIGKILL after [`KILL_GRACE`].
/// `cancel` is asked as it runs: once true, the program is stopped the same way.
pub fn run(
    launch: Launch,
    timeout: Option<Duration>,
    cancel: &dyn Fn() -> bool,
) -> io::Result<ExecResult> {
    let mut cmd = command(&launch);
    cmd.stdin(if launch.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = pty::spawn_locked(&mut cmd)?;
    let writer = child
        .stdin
        .take()
        .zip(launch.stdin)
        .map(|(mut pipe, data)| {
            std::thread::spawn(move || {
                let _ = pipe.write_all(&data);
            })
        });
    // Each reader keeps what it read where `run` can take it even if the pipe never ends (a
    // program left behind holds it open), and says when it is done.
    let reader = |r: Option<Box<dyn Read + Send>>| {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done) = crossbeam_channel::bounded::<()>(1);
        let out = kept.clone();
        std::thread::spawn(move || {
            // Read to the end, keeping the first CAPTURE_MAX bytes: a pipe closed early would
            // stop the program, or leave it blocked on a full pipe.
            if let Some(mut r) = r {
                let mut buf = [0u8; 64 * 1024];
                loop {
                    match r.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut out = out.lock();
                            let room = CAPTURE_MAX.saturating_sub(out.len());
                            out.extend_from_slice(&buf[..n.min(room)]);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            }
            drop(done_tx);
        });
        (kept, done)
    };
    let stdout = reader(
        child
            .stdout
            .take()
            .map(|r| Box::new(r) as Box<dyn Read + Send>),
    );
    let stderr = reader(
        child
            .stderr
            .take()
            .map(|r| Box::new(r) as Box<dyn Read + Send>),
    );
    let pid = child.id() as libc::pid_t;
    let started = Instant::now();
    let mut timed_out = false;
    let mut killed_at: Option<Instant> = None;
    let mut stopping = false;
    // Until it is reaped, the program's pid, and so its group's id, stay its own: signals to
    // the group are safe.
    while !exited(pid) {
        let now = Instant::now();
        let expired = timeout.is_some_and(|t| now.duration_since(started) >= t);
        if killed_at.is_none() && !stopping && (expired || cancel()) {
            timed_out = expired;
            stopping = true;
            killed_at = Some(now);
            unsafe { libc::kill(-pid, libc::SIGTERM) };
        }
        if killed_at.is_some_and(|at| now.duration_since(at) >= KILL_GRACE) {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            killed_at = None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // A program it started in the background may hold the output open: it gets a moment to
    // finish, then goes with the rest of the group.
    let ended = |deadline: Instant| {
        use crossbeam_channel::RecvTimeoutError::Disconnected;
        matches!(stdout.1.recv_deadline(deadline), Err(Disconnected))
            && matches!(stderr.1.recv_deadline(deadline), Err(Disconnected))
    };
    if !ended(Instant::now() + LEFT_BEHIND) {
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let _ = ended(Instant::now() + LEFT_BEHIND);
    }
    let status = child.wait()?;
    // Not joined: a program left behind may never read its input.
    drop(writer);
    let text = |kept: &Arc<Mutex<Vec<u8>>>| String::from_utf8_lossy(&kept.lock()).into_owned();
    use std::os::unix::process::ExitStatusExt;
    Ok(ExecResult {
        ok: status.success(),
        code: status.code(),
        signal: status.signal(),
        stdout: text(&stdout.0),
        stderr: text(&stderr.0),
        timed_out,
    })
}

/// Whether child `pid` has exited, without reaping it.
fn exited(pid: libc::pid_t) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) } != 0 {
        // Interrupted, or the like: asked again on the next look. Only "no such child" means
        // it is gone already.
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
    }
    #[cfg(target_os = "linux")]
    let found = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let found = info.si_pid;
    found != 0
}

/// The last [`STDERR_TAIL`] bytes `r` gives before its end: a command can log for as long as
/// it runs, and only its last words go into the report.
fn tail(r: &mut impl Read) -> Vec<u8> {
    let mut kept = std::collections::VecDeque::with_capacity(STDERR_TAIL);
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                kept.extend(&buf[..n]);
                let over = kept.len().saturating_sub(STDERR_TAIL);
                kept.drain(..over);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    kept.into()
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
    fn only_the_end_of_stderr_is_kept() {
        let long: Vec<u8> = (0..20_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let kept = tail(&mut &long[..]);
        assert_eq!(kept, long[long.len() - STDERR_TAIL..]);
        assert_eq!(tail(&mut &b"short"[..]), b"short");
    }

    /// A command that fills its stderr before it reads its stdin still finishes.
    #[test]
    fn a_noisy_command_reading_a_large_stdin_does_not_hang() {
        let script = "head -c 200000 /dev/zero | tr '\\0' x >&2; cat > /dev/null; exit 1";
        let payload = "y".repeat(300_000);
        let e = run(&["sh", "-c", script], vec![], Some(&payload)).unwrap();
        assert!(e.starts_with("exit 1: xxx"), "{}", &e[..e.len().min(40)]);
        assert!(e.len() <= STDERR_TAIL + 10);
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

    fn launch<'a>(argv: &'a [String], stdin: Option<&str>) -> Launch<'a> {
        Launch {
            argv,
            cwd: None,
            env: vec![("THURM_X".into(), "x".into())],
            stdin: stdin.map(|s| s.as_bytes().to_vec()),
        }
    }

    #[test]
    fn run_returns_the_output_and_status() {
        let argv: Vec<String> = ["sh", "-c", "cat; echo \"$THURM_X\"; echo err >&2; exit 4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = super::run(launch(&argv, Some("in\n")), None, &|| false).unwrap();
        assert_eq!(r.stdout, "in\nx\n");
        assert_eq!(r.stderr, "err\n");
        assert_eq!((r.ok, r.code, r.timed_out), (false, Some(4), false));
    }

    #[test]
    fn run_reads_past_the_capture_limit_to_the_end() {
        let script = format!(
            "head -c {} /dev/zero; echo done >&2; exit 7",
            CAPTURE_MAX + 1_000_000
        );
        let argv: Vec<String> = ["sh", "-c", &script]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r = super::run(launch(&argv, None), Some(Duration::from_secs(60)), &|| {
            false
        })
        .unwrap();
        assert_eq!(r.stdout.len(), CAPTURE_MAX);
        assert_eq!(
            (r.code, r.timed_out, r.stderr.as_str()),
            (Some(7), false, "done\n")
        );
    }

    #[test]
    fn run_ends_when_the_program_does_though_it_left_one_running() {
        let dir = std::env::temp_dir().join(format!("thurm-left-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let script = format!("sleep 30 & echo $! > {}; echo hi", pidfile.display());
        let argv: Vec<String> = ["sh", "-c", &script]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let at = Instant::now();
        let r = super::run(launch(&argv, None), None, &|| false).unwrap();
        assert_eq!((r.stdout.as_str(), r.code), ("hi\n", Some(0)));
        assert!(at.elapsed() < Duration::from_secs(10), "{:?}", at.elapsed());
        // The one left behind was stopped with its group.
        let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // Gone, or a zombie nobody reaps (a container's init may not).
        let state = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&state.stdout).trim().to_owned();
        assert!(
            state.is_empty() || state.starts_with('Z'),
            "sleep {pid} still runs: {state}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn run_stops_a_command_when_cancelled() {
        let argv: Vec<String> = ["sh", "-c", "sleep 30"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let at = Instant::now();
        let cancel = || at.elapsed() > Duration::from_millis(300);
        let r = super::run(launch(&argv, None), None, &cancel).unwrap();
        assert!(!r.ok && !r.timed_out);
        assert!(at.elapsed() < Duration::from_secs(10), "{:?}", at.elapsed());
    }

    #[test]
    fn run_stops_a_command_at_its_timeout() {
        let argv: Vec<String> = ["sh", "-c", "echo started; sleep 30"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let at = Instant::now();
        let r = super::run(
            launch(&argv, None),
            Some(Duration::from_millis(300)),
            &|| false,
        )
        .unwrap();
        assert!(r.timed_out);
        assert!(!r.ok);
        assert_eq!(r.stdout, "started\n");
        assert!(at.elapsed() < Duration::from_secs(10));
    }
}
