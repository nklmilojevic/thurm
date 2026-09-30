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
    if !thurm_config::peer_is_same_user(&stream) {
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
    let writer = std::thread::Builder::new()
        .name(format!("client-{}-writer", client.id))
        .spawn(move || {
            let mut w = BufWriter::with_capacity(256 * 1024, write_stream);
            while let Ok(msg) = rx.recv() {
                if write_msg(&mut w, &msg).is_err() {
                    break;
                }
                // Drain whatever else is queued before flushing.
                while let Ok(msg) = rx.try_recv() {
                    if write_msg(&mut w, &msg).is_err() {
                        return;
                    }
                }
                if std::io::Write::flush(&mut w).is_err() {
                    break;
                }
            }
        });

    let mut reader = std::io::BufReader::new(stream);
    loop {
        match codec::read_message::<_, Envelope>(&mut reader) {
            Ok(Some(env)) => daemon.handle(&client, env.id, env.request),
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
