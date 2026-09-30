//! Unix socket server: one reader and one writer thread per client connection.

use std::io::BufWriter;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use thurm_proto::codec;
use thurm_proto::{Envelope, ServerMessage};

use crate::daemon::Daemon;

pub fn serve(daemon: Arc<Daemon>, listener: UnixListener) {
    for stream in listener.incoming() {
        if daemon.shutdown.load(Ordering::Relaxed) {
            break;
        }
        match stream {
            Ok(stream) => {
                let daemon = daemon.clone();
                let _ = std::thread::Builder::new()
                    .name("client".into())
                    .spawn(move || handle_client(daemon, stream));
            }
            Err(e) => log::warn!("accept failed: {e}"),
        }
    }
}

fn handle_client(daemon: Arc<Daemon>, stream: UnixStream) {
    if !peer_is_same_user(&stream) {
        log::warn!("rejecting connection from another user");
        return;
    }
    let (client, rx) = daemon.add_client();
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("clone stream: {e}");
            daemon.remove_client(client.id);
            return;
        }
    };
    let counter = client.clone();
    let writer = std::thread::Builder::new()
        .name(format!("client-{}-writer", client.id))
        .spawn(move || {
            let mut w = BufWriter::with_capacity(256 * 1024, write_stream);
            let written = |msg: &ServerMessage| {
                counter.queued.fetch_sub(
                    crate::daemon::message_weight(msg).min(counter.queued.load(Ordering::Relaxed)),
                    Ordering::Relaxed,
                );
            };
            while let Ok(msg) = rx.recv() {
                if write_msg(&mut w, &msg).is_err() {
                    break;
                }
                written(&msg);
                // Drain whatever else is queued before flushing.
                while let Ok(msg) = rx.try_recv() {
                    if write_msg(&mut w, &msg).is_err() {
                        return;
                    }
                    written(&msg);
                }
                if std::io::Write::flush(&mut w).is_err() {
                    break;
                }
            }
        });

    let mut reader = std::io::BufReader::new(stream);
    loop {
        match codec::read_frame(&mut reader) {
            Ok(Some(frame)) => match codec::decode::<Envelope>(&frame) {
                Ok(env) => daemon.handle(&client, env.id, env.request),
                // A request this daemon doesn't know (a newer client): answer it with an
                // error, and keep the connection (the next frame is intact).
                Err(e) => match codec::envelope_id(&frame) {
                    Some(id) => {
                        log::debug!("client {}: undecodable request {id}: {e}", client.id);
                        if id != 0 {
                            client.send(ServerMessage::Response {
                                id,
                                result: Err(format!(
                                    "this daemon ({}) does not know the request",
                                    thurm_proto::BUILD
                                )),
                            });
                        }
                    }
                    None => {
                        log::debug!("client {} sent garbage: {e}", client.id);
                        break;
                    }
                },
            },
            Ok(None) => break,
            Err(e) => {
                log::debug!("client {} read error: {e}", client.id);
                break;
            }
        }
    }
    daemon.remove_client(client.id);
    drop(client);
    if let Ok(w) = writer {
        let _ = w.join();
    }
}

fn write_msg<W: std::io::Write>(w: &mut W, msg: &ServerMessage) -> std::io::Result<()> {
    let buf = codec::encode(msg)?;
    w.write_all(&buf)
}

/// Only accept clients running as the daemon's user (the socket dir is 0700 too).
fn peer_is_same_user(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let me = unsafe { libc::getuid() };
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        if libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        ) == 0
        {
            return cred.uid == me;
        }
        false
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        if libc::getpeereid(fd, &mut uid, &mut gid) == 0 {
            return uid == me;
        }
        false
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (fd, me);
        true
    }
}
