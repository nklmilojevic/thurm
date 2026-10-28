//! Unix socket server: one reader and one writer thread per client connection.

use std::io::{BufWriter, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

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

/// `MOBILE_SILENCE`, or `THURM_MOBILE_SILENCE_MS` (tests).
fn mobile_silence() -> Duration {
    std::env::var("THURM_MOBILE_SILENCE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(thurm_proto::MOBILE_SILENCE)
}

/// Whether a client the writer put a message out to within `silence` of `now` is still there.
/// A phone whose session loop is busy with output can't ping, but its connection is alive.
fn written_within(last_written: Instant, now: Instant, silence: Duration) -> bool {
    now.saturating_duration_since(last_written) < silence
}

/// Reads how a connection starts: an ingress preamble (`thurm_proto::tailnet`), or the first
/// frame, returned whole so it isn't lost.
fn read_start<R: Read>(
    r: &mut R,
) -> std::io::Result<(Option<thurm_proto::Principal>, Option<Vec<u8>>)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len == thurm_proto::tailnet::INGRESS_MAGIC {
        return Ok((Some(thurm_proto::tailnet::read_ingress(r)?), None));
    }
    let len = len as usize;
    if len > codec::MAX_MESSAGE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut frame = vec![0u8; len];
    r.read_exact(&mut frame)?;
    Ok((None, Some(frame)))
}

fn handle_client(daemon: Arc<Daemon>, stream: UnixStream) {
    if !thurm_config::peer_may_use_daemon(&stream) {
        log::warn!("rejecting connection from another user");
        return;
    }
    let mut reader = std::io::BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("clone stream: {e}");
            return;
        }
    });
    // A tailnet caller's connection starts with who it is (from the Tailscale helper); any
    // other starts with its first frame.
    let (principal, first_frame) = match read_start(&mut reader) {
        Ok(start) => start,
        Err(e) => {
            log::debug!("connection dropped before its first message: {e}");
            return;
        }
    };
    let principal = principal.unwrap_or_else(|| {
        let uid = thurm_config::peer_uid(&stream);
        thurm_proto::Principal {
            display: uid.map_or_else(|| "local".into(), |u| format!("uid {u}")),
            uid,
            ..Default::default()
        }
    });
    let (client, rx) = daemon.add_client();
    if principal.kind == thurm_proto::PrincipalKind::Tailscale {
        log::info!(
            "client {} is {} over the tailnet ({})",
            client.id,
            principal.display,
            principal.via.as_deref().unwrap_or("?")
        );
    }
    *client.principal.lock() = Some(principal);
    *client.socket.lock() = stream.try_clone().ok();
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("clone stream: {e}");
            daemon.remove_client(client.id);
            return;
        }
    };
    // Weak: the client owns `rx`'s sender, and the writer only stops once every sender is gone.
    let counter = Arc::downgrade(&client);
    let writer = std::thread::Builder::new()
        .name(format!("client-{}-writer", client.id))
        .spawn(move || {
            let mut w = BufWriter::with_capacity(256 * 1024, write_stream);
            let written = |msg: &ServerMessage| {
                let Some(counter) = counter.upgrade() else {
                    return;
                };
                counter.queued.fetch_sub(
                    crate::daemon::message_weight(msg).min(counter.queued.load(Ordering::Relaxed)),
                    Ordering::Relaxed,
                );
                *counter.last_written.lock() = Instant::now();
            };
            'write: while let Ok(msg) = rx.recv() {
                if write_msg(&mut w, &msg).is_err() {
                    break;
                }
                written(&msg);
                let mut batch = 1;
                // Drain whatever else is queued before flushing.
                while let Ok(msg) = rx.try_recv() {
                    if write_msg(&mut w, &msg).is_err() {
                        break 'write;
                    }
                    written(&msg);
                    batch += 1;
                }
                if std::io::Write::flush(&mut w).is_err() {
                    break;
                }
                if let Some(counter) = counter.upgrade() {
                    let pending = &counter.unflushed;
                    let flushed = |n: usize| Some(n.saturating_sub(batch));
                    let _ = pending.try_update(Ordering::AcqRel, Ordering::Acquire, flushed);
                }
            }
            // Nothing still queued will reach the socket, so a shutdown must not wait for it.
            // Dropped first: later sends then fail, and reset the count themselves.
            drop(rx);
            if let Some(counter) = counter.upgrade() {
                counter.unflushed.store(0, Ordering::Release);
            }
        });

    let mut first_frame = first_frame;
    let mut silence_limited = false;
    loop {
        // A phone that stopped talking (suspended, out of range) is gone: it pings while open.
        if !silence_limited && client.mobile.load(Ordering::Relaxed) {
            silence_limited = true;
            let _ = reader.get_ref().set_read_timeout(Some(mobile_silence()));
        }
        let frame = match first_frame.take() {
            Some(frame) => Ok(Some(frame)),
            None => codec::read_frame(&mut reader),
        };
        match frame {
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
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if written_within(
                    *client.last_written.lock(),
                    Instant::now(),
                    mobile_silence(),
                ) {
                    continue;
                }
                log::info!("client {} (mobile) went quiet; dropping it", client.id);
                break;
            }
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
