//! `thurmd` — the Thurm session daemon.
//!
//! Owns every PTY and terminal state so shells survive the GUI quitting or crashing, and
//! snapshots layout + scrollback to disk so sessions come back after a reboot.

mod agents;
mod ai;
mod complete;
mod daemon;
mod git;
mod persist;
mod procinfo;
mod pty;
mod server;
mod shell;
mod transcript;
mod upgrade;

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use thurm_config::Config;

struct Args {
    daemonize: bool,
    foreground: bool,
    socket: Option<PathBuf>,
    no_restore: bool,
    /// Hand-off file from the image we replace (see `upgrade.rs`).
    adopt: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut args = Args {
        daemonize: false,
        foreground: false,
        socket: None,
        no_restore: false,
        adopt: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--daemonize" | "-d" => args.daemonize = true,
            "--foreground" | "-f" => {
                args.daemonize = false;
                args.foreground = true;
            }
            "--adopt" => args.adopt = it.next().map(PathBuf::from),
            "--handoff-check" => {
                upgrade::print_check();
                std::process::exit(0);
            }
            "--socket" => args.socket = it.next().map(PathBuf::from),
            "--no-restore" => args.no_restore = true,
            "--version" | "-V" => {
                println!("thurmd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "thurmd {}\n\nUSAGE: thurmd [--daemonize] [--socket PATH] [--no-restore]\n\n\
                     Normally started automatically by Thurm.app or the thurm CLI.\n\
                     SIGUSR2 replaces the running daemon with the thurmd named in\n\
                     <socket>.upgrade (else its own path) without stopping any pane.",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("thurmd: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    args
}

fn main() {
    let args = parse_args();
    let config = Config::load().unwrap_or_else(|e| {
        eprintln!("thurmd: config error, using defaults: {e}");
        Config::default()
    });
    let socket = args.socket.unwrap_or_else(thurm_config::socket_path);
    let state_dir = thurm_config::state_dir();
    let _ = std::fs::create_dir_all(&state_dir);

    // Before binding: a SIGTERM that arrives once clients can find us must save the session,
    // not kill us with the default action.
    let term = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        let _ = signal_hook::flag::register(sig, term.clone());
    }

    let upgrade_requested = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGUSR2, upgrade_requested.clone());

    let log_path = state_dir.join("thurmd.log");
    let handoff = args.adopt.as_deref().map(upgrade::load);
    let (listener, handoff, log_to_file) = match handoff {
        // Replacing ourselves: already daemonized, and the socket never stopped listening.
        Some(Ok(h)) => {
            let listener = unsafe { UnixListener::from_raw_fd(h.listener_fd) };
            let _ = pty::set_cloexec(listener.as_raw_fd(), true);
            let log_to_file = h.log_to_file;
            init_logging(&log_path, log_to_file);
            (listener, Some(h), log_to_file)
        }
        adopt => {
            if let Some(Err(e)) = &adopt {
                // The panes' fds are lost with the file; the snapshot saved before the
                // upgrade brings the session back. The old listener still holds the socket,
                // so skip the single-instance check and bind afresh.
                eprintln!("thurmd: cannot adopt the previous daemon's panes: {e}");
            } else if UnixStream::connect(&socket).is_ok() {
                // Single instance: if a daemon answers on the socket, we're done.
                eprintln!("thurmd: already running on {}", socket.display());
                return;
            }
            let listener = match bind(&socket) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("thurmd: cannot bind {}: {e}", socket.display());
                    std::process::exit(1);
                }
            };
            if args.daemonize {
                daemonize(&log_path);
            }
            let log_to_file = args.daemonize || (adopt.is_some() && !args.foreground);
            init_logging(&log_path, log_to_file);
            (listener, None, log_to_file)
        }
    };
    let listener_fd = listener.as_raw_fd();
    // After daemonizing: this is the process that owns the socket (clients stop us through it).
    let pid_path = thurm_client_pid_path(&socket);
    let _ = std::fs::write(&pid_path, format!("{}\n", std::process::id()));
    log::info!(
        "thurmd {} ({}) listening on {}",
        env!("CARGO_PKG_VERSION"),
        thurm_proto::BUILD,
        socket.display()
    );

    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let daemon = daemon::Daemon::new(config, socket.clone(), state_dir.clone(), term.clone());
    if let Some(h) = handoff {
        daemon.adopt(h);
    } else if !args.no_restore {
        daemon.restore_session();
    }

    {
        let d = daemon.clone();
        std::thread::Builder::new()
            .name("git".into())
            .spawn(move || d.git_worker())
            .expect("thread");
        let d = daemon.clone();
        std::thread::Builder::new()
            .name("ai".into())
            .spawn(move || d.ai_worker())
            .expect("thread");
        let d = daemon.clone();
        std::thread::Builder::new()
            .name("monitor".into())
            .spawn(move || d.monitor())
            .expect("thread");
        let d = daemon.clone();
        std::thread::Builder::new()
            .name("accept".into())
            .spawn(move || server::serve(d, listener))
            .expect("thread");
    }

    // Main thread: wait for a shutdown request or a signal.
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if term.load(Ordering::Relaxed) {
            log::info!("signal received, saving session");
            daemon.save_session();
            break;
        }
        if daemon.shutdown.load(Ordering::Relaxed) {
            break;
        }
        if upgrade_requested.swap(false, Ordering::Relaxed) {
            // Returns only when the upgrade could not happen; we carry on as before.
            upgrade::perform(&daemon, listener_fd, &socket, &state_dir, log_to_file);
        }
        // Socket file deleted or replaced: another daemon took over or the user cleaned up.
        if !socket.exists() {
            log::warn!("socket removed, exiting");
            daemon.save_session();
            break;
        }
    }
    daemon.shutdown.store(true, Ordering::Relaxed);
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&pid_path);
    log::info!("bye");
    // Dropping panes hangs up the shells; exit without waiting for client threads.
    std::process::exit(0);
}

/// `<socket>.pid`, read by `thurm_client::terminate_daemon`.
fn thurm_client_pid_path(socket: &Path) -> std::path::PathBuf {
    let mut p = socket.as_os_str().to_owned();
    p.push(".pid");
    p.into()
}

fn bind(socket: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(socket);
    let l = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// Classic double fork so the daemon outlives whoever launched it.
fn daemonize(log_path: &Path) {
    unsafe {
        match libc::fork() {
            -1 => {
                eprintln!("thurmd: fork failed");
                std::process::exit(1);
            }
            0 => {}
            _ => std::process::exit(0),
        }
        libc::setsid();
        match libc::fork() {
            -1 => std::process::exit(1),
            0 => {}
            _ => libc::_exit(0),
        }
        let _ = libc::chdir(c"/".as_ptr());
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
        }
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            use std::os::fd::IntoRawFd;
            let fd = f.into_raw_fd();
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
        }
    }
}

fn init_logging(log_path: &Path, to_file: bool) {
    let mut b =
        env_logger::Builder::from_env(env_logger::Env::new().filter_or("THURM_LOG", "info"));
    b.format(|buf, rec| {
        writeln!(
            buf,
            "{} {:5} {}",
            persist::now_secs(),
            rec.level(),
            rec.args()
        )
    });
    if to_file
        && let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
    {
        b.target(env_logger::Target::Pipe(Box::new(f)));
    }
    let _ = b.try_init();
}
