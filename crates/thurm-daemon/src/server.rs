//! Unix socket server: one reader and one writer thread per client connection.

use std::io::{BufWriter, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
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

/// Whether a client whose socket took bytes within `silence` of `now` is still there. A phone
/// whose session loop is busy with output can't ping, but its connection is alive.
fn written_within(last_written: Instant, now: Instant, silence: Duration) -> bool {
    now.saturating_duration_since(last_written) < silence
}

/// The socket under the writer's buffer: stamps the time on every write that takes bytes, so a
/// large write that is still moving counts as progress.
struct Progress<W> {
    inner: W,
    at: Arc<Mutex<Instant>>,
}

impl<W: Write> Write for Progress<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        if n > 0 {
            *self.at.lock() = Instant::now();
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Reads length-prefixed frames that may arrive in pieces. A partial frame is kept across read
/// timeouts: a `read_exact` that times out has already taken some of the frame.
#[derive(Default)]
struct Frames {
    len: [u8; 4],
    len_read: usize,
    body: Vec<u8>,
    body_read: usize,
}

impl Frames {
    /// The next whole frame, or `Ok(None)` when the connection ends between frames. A timeout is
    /// an error; what was read of the frame stays for the next call.
    fn read<R: Read>(&mut self, r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            if self.len_read < self.len.len() {
                let n = read_some(r, &mut self.len[self.len_read..])?;
                if n == 0 {
                    return if self.len_read == 0 {
                        Ok(None)
                    } else {
                        Err(ended_mid_frame())
                    };
                }
                self.len_read += n;
                if self.len_read == self.len.len() {
                    let len = u32::from_le_bytes(self.len) as usize;
                    if len > codec::MAX_MESSAGE {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "message too large",
                        ));
                    }
                    self.body = vec![0; len];
                }
            } else if self.body_read < self.body.len() {
                let n = read_some(r, &mut self.body[self.body_read..])?;
                if n == 0 {
                    return Err(ended_mid_frame());
                }
                self.body_read += n;
            } else {
                return Ok(Some(std::mem::take(self).body));
            }
        }
    }
}

fn read_some<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match r.read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

fn ended_mid_frame() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "the connection ended in a frame",
    )
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
    let last_written = client.last_written.clone();
    let deflate = client.deflate.clone();
    let writer = std::thread::Builder::new()
        .name(format!("client-{}-writer", client.id))
        .spawn(move || {
            let socket = Progress {
                inner: write_stream,
                at: last_written,
            };
            let mut w = BufWriter::with_capacity(256 * 1024, socket);
            let written = |msg: &ServerMessage| {
                let Some(counter) = counter.upgrade() else {
                    return;
                };
                counter.queued.fetch_sub(
                    crate::daemon::message_weight(msg).min(counter.queued.load(Ordering::Relaxed)),
                    Ordering::Relaxed,
                );
            };
            'write: while let Ok(msg) = rx.recv() {
                if write_msg(&mut w, &msg, &deflate).is_err() {
                    break;
                }
                written(&msg);
                let mut batch = 1;
                // Drain whatever else is queued before flushing.
                while let Ok(msg) = rx.try_recv() {
                    if write_msg(&mut w, &msg, &deflate).is_err() {
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
    let mut frames = Frames::default();
    let mut silence_limited = false;
    loop {
        // A phone that stopped talking (suspended, out of range) is gone: it pings while open.
        if !silence_limited && client.mobile.load(Ordering::Relaxed) {
            silence_limited = true;
            let _ = reader.get_ref().set_read_timeout(Some(mobile_silence()));
        }
        // A silence-limited read may time out mid-frame and go on, so it keeps its place.
        let frame = match first_frame.take() {
            Some(frame) => Ok(Some(frame)),
            None if silence_limited => frames.read(&mut reader),
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

fn write_msg<W: std::io::Write>(
    w: &mut W,
    msg: &ServerMessage,
    deflate: &AtomicBool,
) -> std::io::Result<()> {
    let buf = codec::encode_frame(msg, deflate.load(Ordering::Relaxed))?;
    w.write_all(&buf)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{self, ErrorKind};

    use super::*;

    /// What a scripted connection does next.
    enum Step {
        Bytes(Vec<u8>),
        Timeout,
    }

    /// A connection that yields its steps in order, then ends.
    struct Script(VecDeque<Step>);

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(Step::Timeout) => Err(ErrorKind::TimedOut.into()),
                Some(Step::Bytes(mut bytes)) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    if n < bytes.len() {
                        self.0.push_front(Step::Bytes(bytes.split_off(n)));
                    }
                    Ok(n)
                }
            }
        }
    }

    fn wire(body: &[u8]) -> Vec<u8> {
        let mut wire = (body.len() as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(body);
        wire
    }

    fn timed_out(r: &io::Result<Option<Vec<u8>>>) -> bool {
        matches!(r, Err(e) if e.kind() == ErrorKind::TimedOut)
    }

    #[test]
    fn a_frame_split_across_timeouts_resumes_where_it_stopped() {
        let (one, two) = (wire(b"hello"), wire(b"hi"));
        let mut all = one.clone();
        all.extend_from_slice(&two);
        // The first timeout splits the length, the second the body of the first frame.
        let mut r = Script(VecDeque::from([
            Step::Bytes(all[..2].to_vec()),
            Step::Timeout,
            Step::Bytes(all[2..6].to_vec()),
            Step::Timeout,
            Step::Bytes(all[6..].to_vec()),
        ]));
        let mut frames = Frames::default();
        assert!(timed_out(&frames.read(&mut r)));
        assert!(timed_out(&frames.read(&mut r)));
        assert_eq!(frames.read(&mut r).unwrap(), Some(b"hello".to_vec()));
        assert_eq!(frames.read(&mut r).unwrap(), Some(b"hi".to_vec()));
        assert_eq!(frames.read(&mut r).unwrap(), None);
    }

    #[test]
    fn a_frame_split_right_after_its_length_resumes() {
        let w = wire(b"hello");
        let mut r = Script(VecDeque::from([
            Step::Bytes(w[..4].to_vec()),
            Step::Timeout,
            Step::Bytes(w[4..].to_vec()),
        ]));
        let mut frames = Frames::default();
        assert!(timed_out(&frames.read(&mut r)));
        assert_eq!(frames.read(&mut r).unwrap(), Some(b"hello".to_vec()));
    }

    #[test]
    fn a_connection_ending_inside_a_frame_is_an_error() {
        let w = wire(b"hello");
        let mut r = Script(VecDeque::from([Step::Bytes(w[..7].to_vec())]));
        let mut frames = Frames::default();
        let e = frames.read(&mut r).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn writes_that_take_bytes_stamp_the_socket() {
        let at = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(60)));
        let mut socket = Progress {
            inner: Vec::new(),
            at: at.clone(),
        };
        socket.write_all(b"partial and whole").unwrap();
        assert!(written_within(
            *at.lock(),
            Instant::now(),
            Duration::from_secs(1)
        ));
        assert_eq!(socket.inner, b"partial and whole");
    }
}
