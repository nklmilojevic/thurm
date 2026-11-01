//! A file descriptor sent along with a few bytes over a unix socket (`SCM_RIGHTS`).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// Sends `bytes`, with `fd` attached.
pub fn send(stream: &UnixStream, bytes: &[u8], fd: RawFd) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd);
    }
    loop {
        let n = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
        if n >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Receives up to `buf.len()` bytes, and the file descriptor sent with them if any. 0 bytes and
/// no descriptor: the other side hung up.
pub fn recv(stream: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    let n = loop {
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    let mut fd = None;
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let raw = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const RawFd);
                fd = Some(OwnedFd::from_raw_fd(raw));
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n, fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn a_descriptor_crosses_the_socket() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut pipe_r, mut pipe_w) = UnixStream::pair().unwrap();
        send(&a, b"p7", pipe_w.as_raw_fd()).unwrap();
        let mut buf = [0; 16];
        let (n, fd) = recv(&b, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"p7");
        // Writing through the received descriptor reaches the original's peer.
        let mut got = UnixStream::from(fd.unwrap());
        got.write_all(b"hi").unwrap();
        drop(got);
        pipe_w.flush().unwrap();
        drop(pipe_w);
        let mut s = String::new();
        pipe_r.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hi");

        drop(a);
        assert_eq!(recv(&b, &mut buf).unwrap().0, 0);
    }
}
