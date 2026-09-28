//! PTY creation and child process management.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use thurm_proto::PaneSize;

pub struct SpawnOptions {
    pub program: String,
    pub args: Vec<String>,
    /// argv[0] override (e.g. "-zsh" for a login shell).
    pub argv0: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub size: PaneSize,
}

pub struct Pty {
    master: File,
    pid: libc::pid_t,
    exited: Option<Option<i32>>,
}

fn winsize(size: PaneSize) -> libc::winsize {
    libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.cols.saturating_mul(size.cell_width),
        ws_ypixel: size.rows.saturating_mul(size.cell_height),
    }
}

impl Pty {
    pub fn spawn(opts: SpawnOptions) -> io::Result<Pty> {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let mut ws = winsize(opts.size);
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                // `*const` on macOS, `*mut` on Linux.
                std::ptr::null_mut::<libc::termios>() as _,
                &mut ws as *mut libc::winsize as _,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }

        // UTF-8 input processing.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(master.as_raw_fd(), &mut t) == 0 {
                t.c_iflag |= libc::IUTF8;
                libc::tcsetattr(master.as_raw_fd(), libc::TCSANOW, &t);
            }
        }

        let mut cmd = Command::new(&opts.program);
        cmd.args(&opts.args);
        if let Some(argv0) = &opts.argv0 {
            cmd.arg0(argv0);
        }
        cmd.stdin(slave.try_clone()?);
        cmd.stdout(slave.try_clone()?);
        cmd.stderr(slave.try_clone()?);
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        let cwd = opts
            .cwd
            .as_ref()
            .and_then(|p| CString::new(p.as_os_str().as_encoded_bytes()).ok());
        let slave_fd = slave.as_raw_fd();
        let master_fd = master.as_raw_fd();
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if let Some(dir) = cwd.as_ref() {
                    libc::chdir(dir.as_ptr());
                }
                #[allow(clippy::useless_conversion)]
                if libc::ioctl(slave_fd, libc::TIOCSCTTY.into(), 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(slave_fd);
                libc::close(master_fd);
                for sig in [
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGALRM,
                    libc::SIGPIPE,
                ] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                // Unblock everything the daemon may have blocked.
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|e| {
            io::Error::new(e.kind(), format!("failed to spawn {:?}: {e}", opts.program))
        })?;
        drop(slave);
        Ok(Pty {
            master: File::from(master),
            pid: child.id() as libc::pid_t,
            exited: None,
        })
    }

    /// Takes over a PTY whose master fd and child survived an in-place upgrade (the child is
    /// still ours: exec keeps the pid). `exited` is the status already collected, if any.
    pub fn adopt(fd: RawFd, pid: u32, exited: Option<Option<i32>>) -> io::Result<Pty> {
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(io::Error::last_os_error());
        }
        set_cloexec(fd, true)?;
        Ok(Pty {
            master: unsafe { File::from_raw_fd(fd) },
            pid: pid as libc::pid_t,
            exited,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    pub fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// The exit status collected so far (`Some` once the child is gone).
    pub fn exit_status(&self) -> Option<Option<i32>> {
        self.exited
    }

    /// A handle for the reader thread.
    pub fn reader(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    pub fn writer(&self) -> io::Result<File> {
        self.master.try_clone()
    }

    pub fn resize(&self, size: PaneSize) {
        let ws = winsize(size);
        unsafe {
            libc::ioctl(
                self.master.as_raw_fd(),
                libc::TIOCSWINSZ,
                &ws as *const libc::winsize,
            );
        }
    }

    /// Non-blocking check for the child's exit status. `Some(code)` once exited
    /// (code is `None` when killed by a signal).
    pub fn try_wait(&mut self) -> Option<Option<i32>> {
        if let Some(e) = self.exited {
            return Some(e);
        }
        let mut status = 0;
        let rc = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if rc == self.pid {
            let code = if libc::WIFEXITED(status) {
                Some(libc::WEXITSTATUS(status))
            } else if libc::WIFSIGNALED(status) {
                Some(128 + libc::WTERMSIG(status))
            } else {
                None
            };
            self.exited = Some(code);
            return self.exited;
        }
        if rc == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            self.exited = Some(None);
            return self.exited;
        }
        None
    }

    /// Foreground process group of the terminal (the running job, or the shell itself).
    pub fn foreground_pgrp(&self) -> Option<u32> {
        let pg = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        (pg > 0).then_some(pg as u32)
    }

    /// Echo disabled with canonical mode on: a password prompt (sudo, ssh, gpg...).
    pub fn password_mode(&self) -> bool {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(self.master.as_raw_fd(), &mut t) != 0 {
                return false;
            }
            (t.c_lflag & libc::ECHO) == 0 && (t.c_lflag & libc::ICANON) != 0
        }
    }

    /// Hang up the session: SIGHUP to the process group, SIGKILL later if still alive.
    pub fn hangup(&mut self) {
        if self.try_wait().is_some() {
            return;
        }
        unsafe {
            libc::kill(-self.pid, libc::SIGHUP);
            libc::kill(self.pid, libc::SIGHUP);
        }
        let pid = self.pid;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            unsafe {
                let mut status = 0;
                if libc::waitpid(pid, &mut status, libc::WNOHANG) == 0 {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
            }
        });
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.hangup();
    }
}

/// Set or clear `FD_CLOEXEC` (cleared on the fds an in-place upgrade hands to the new image).
pub fn set_cloexec(fd: RawFd, on: bool) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if on {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Empty non-blocking reads tried before sleeping in `poll` (about 0.5 µs each).
///
/// A PTY hands over about 1 KiB per read, and the writer refills it only after we drained it.
/// Sleeping in a blocking read after every chunk costs a wakeup per KiB; retrying for a few
/// microseconds catches the next chunk instead, which drains a busy PTY about 1.7x faster
/// (11 MB: 72 ms blocking, 42 ms spinning on an M3 Max). An idle pane spins once per burst.
const READ_SPINS: u32 = 32;

/// Make the PTY master non-blocking, for [`read_pty`]. The flag belongs to the open file, so
/// the writer's handle is non-blocking too: [`write_all`] waits in `poll` when it's full.
pub fn set_nonblocking(f: &File) -> io::Result<()> {
    let fd = f.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Wait until `f` is ready for `events` (EINTR is retried by the caller's loop).
fn wait_for(f: &File, events: libc::c_short) -> io::Result<()> {
    let mut p = libc::pollfd {
        fd: f.as_raw_fd(),
        events,
        revents: 0,
    };
    if unsafe { libc::poll(&mut p, 1, -1) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(())
}

/// Read from the PTY master, blocking until data (or EOF) arrives. Retries on EINTR and maps
/// EIO (slave closed) to EOF. On a non-blocking handle, spins briefly before sleeping.
pub fn read_pty(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut empty = 0;
    loop {
        match f.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                empty += 1;
                if empty >= READ_SPINS {
                    empty = 0;
                    wait_for(f, libc::POLLIN)?;
                }
            }
            Err(e) => return Err(e),
        }
    }
}

pub fn write_all(f: &mut File, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        match f.write(data) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => wait_for(f, libc::POLLOUT)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
