//! A pane's terminal, as the daemon holds it: raw mode, its size, and restoring it.

use std::io;
use std::os::fd::{AsRawFd, RawFd};

/// Puts the terminal on `fd` in raw mode; returns its settings before, for [`restore`].
pub fn make_raw(fd: RawFd) -> io::Result<libc::termios> {
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut raw = saved;
    unsafe { libc::cfmakeraw(&mut raw) };
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(saved)
}

pub fn restore(fd: RawFd, saved: &libc::termios) {
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, saved) };
}

/// Columns and rows of the terminal on `fd`.
pub fn size(fd: &impl AsRawFd) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) } != 0
        || ws.ws_col == 0
        || ws.ws_row == 0
    {
        return None;
    }
    Some((ws.ws_col, ws.ws_row))
}

pub fn is_tty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}
