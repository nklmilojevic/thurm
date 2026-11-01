//! `thurm-nvim pane SOCKET [ID]`: what each pane runs. It hands its terminal to the daemon,
//! tells it when the terminal resizes (only the pane's foreground process gets SIGWINCH), and
//! exits when the daemon hangs up, which closes the pane.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::{fdpass, tty};

/// What the helper sends the daemon, a byte each.
pub const HELLO: u8 = b'p';
pub const RESIZED: u8 = b'w';

/// The hello: `p`, then the pane's ID if the daemon made the pane (the launcher's has none).
pub fn hello(id: Option<u32>) -> Vec<u8> {
    let mut out = vec![HELLO];
    if let Some(id) = id {
        out.extend_from_slice(id.to_string().as_bytes());
    }
    out
}

/// The ID in a hello: `Ok(None)` for the launcher's pane.
pub fn parse_hello(bytes: &[u8]) -> Result<Option<u32>, ()> {
    match bytes.split_first() {
        Some((&HELLO, [])) => Ok(None),
        Some((&HELLO, id)) => std::str::from_utf8(id)
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Some)
            .ok_or(()),
        _ => Err(()),
    }
}

pub fn run(socket: &Path, id: Option<u32>) -> io::Result<()> {
    let stdin = libc::STDIN_FILENO;
    if !tty::is_tty(stdin) {
        return Err(io::Error::other("not a terminal"));
    }
    // Before the hello, so a resize in between isn't missed.
    let (mut sig_r, sig_w) = UnixStream::pair()?;
    signal_hook::low_level::pipe::register(libc::SIGWINCH, sig_w)?;

    let mut stream = UnixStream::connect(socket)?;
    fdpass::send(&stream, &hello(id), stdin)?;

    let mut buf = [0u8; 64];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: sig_r.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if fds[1].revents != 0 {
            let _ = sig_r.read(&mut buf)?;
            stream.write_all(&[RESIZED])?;
        }
        if fds[0].revents != 0 {
            // Nothing is sent this way: only the hang-up.
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hellos() {
        assert_eq!(parse_hello(&hello(None)), Ok(None));
        assert_eq!(parse_hello(&hello(Some(42))), Ok(Some(42)));
        assert!(parse_hello(b"w").is_err());
        assert!(parse_hello(b"px").is_err());
    }
}
