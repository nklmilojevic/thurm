//! Byte streams through the daemon connection (see [`thurm_proto::stream`]): reserve one with
//! [`Client::dial_begin`](crate::Client::dial_begin), then [`open`] it with the ticket on a
//! connection of its own.

use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use thurm_proto::stream::{self, MAX_DATA, StreamClose, StreamFrame};
use thurm_proto::{CAP_STREAMS, Envelope, PROTOCOL_VERSION, Request, Response, ServerMessage};
use thurm_proto::{codec, hello_compatible};

use crate::{ClientError, UNREADABLE_ANSWER};

/// The most the Hello and the `StreamOpen` take together.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// A reserved stream ([`Response::Ticket`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub ticket: String,
    /// How long the ticket stays valid unused.
    pub expires_ms: u32,
}

/// Opens the stream `ticket` reserved, on a new connection to the daemon at `socket`.
pub fn open(socket: &Path, ticket: &str) -> Result<(StreamReader, StreamWriter), ClientError> {
    let s = crate::connect_daemon(socket).map_err(|e| ClientError::Connect(e.to_string()))?;
    open_on(s, ticket)
}

/// Like [`open`], on a connection already made: the daemon socket, or one end of a pair that a
/// transport relays to the daemon. The connection must be new (nothing sent on it yet but an
/// ingress preamble). It is shut down when opening fails.
pub fn open_on(
    stream: UnixStream,
    ticket: &str,
) -> Result<(StreamReader, StreamWriter), ClientError> {
    let opened = handshake(&stream, ticket);
    if opened.is_err() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    let reader = opened?;
    let writer = StreamWriter {
        socket: stream,
        ended: false,
        closed: false,
    };
    Ok((reader, writer))
}

fn handshake(stream: &UnixStream, ticket: &str) -> Result<StreamReader, ClientError> {
    stream.set_read_timeout(Some(OPEN_TIMEOUT))?;
    stream.set_write_timeout(Some(OPEN_TIMEOUT))?;
    let mut r = BufReader::new(stream.try_clone()?);
    let hello = ask(
        stream,
        &mut r,
        1,
        Request::Hello {
            client: "stream".into(),
            version: PROTOCOL_VERSION,
            ui: false,
            capabilities: vec![CAP_STREAMS.into()],
        },
    )?;
    let Response::Hello {
        version,
        capabilities,
        ..
    } = hello
    else {
        return Err(ClientError::Daemon(UNREADABLE_ANSWER.into()));
    };
    if !hello_compatible(version, &capabilities) {
        return Err(ClientError::Daemon(format!(
            "protocol mismatch: daemon {version}, client {PROTOCOL_VERSION}"
        )));
    }
    if !capabilities.iter().any(|c| c == CAP_STREAMS) {
        return Err(ClientError::Daemon(NO_STREAMS.into()));
    }
    let opened = ask(
        stream,
        &mut r,
        2,
        Request::StreamOpen {
            ticket: ticket.to_owned(),
        },
    )?;
    // Only Ok says the daemon switched the connection to frames.
    if !matches!(opened, Response::Ok) {
        return Err(ClientError::Daemon(UNREADABLE_ANSWER.into()));
    }
    // macOS refuses socket options once the peer has closed; reading then ends anyway.
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(None);
    Ok(StreamReader {
        socket: r,
        buf: Vec::new(),
        pos: 0,
        ended: false,
        closed: None,
    })
}

/// What a daemon without [`CAP_STREAMS`] is told.
pub(crate) const NO_STREAMS: &str =
    "this Thurm daemon carries no streams yet; update Thurm on this host";

/// Sends `request` as `id` and waits for its answer, passing over events.
fn ask(
    mut w: &UnixStream,
    r: &mut BufReader<UnixStream>,
    id: u64,
    request: Request,
) -> Result<Response, ClientError> {
    codec::write_message(&mut w, &Envelope { id, request })?;
    loop {
        let frame = match codec::read_frame(r) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Err(ClientError::Disconnected),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ClientError::Timeout);
            }
            Err(e) => return Err(e.into()),
        };
        match codec::decode_message(&frame) {
            Ok(ServerMessage::Response { id: got, result }) if got == id => {
                return result.map_err(ClientError::Daemon);
            }
            Ok(_) => {}
            Err(_) if codec::response_id(&frame) == Some(id) => {
                return Err(ClientError::Daemon(UNREADABLE_ANSWER.into()));
            }
            Err(_) => {}
        }
    }
}

/// The error a stream that closed with `why` reads as.
fn closed_error(why: &StreamClose) -> io::Error {
    let kind = match why {
        StreamClose::Ok => io::ErrorKind::UnexpectedEof,
        StreamClose::Aborted => io::ErrorKind::ConnectionAborted,
        StreamClose::Refused(_) => io::ErrorKind::ConnectionRefused,
        StreamClose::Internal(_) => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("stream {why}"))
}

/// The bytes the daemon's end sends. Reads 0 once that end has finished; an error when the
/// stream was refused, aborted or failed.
pub struct StreamReader {
    socket: BufReader<UnixStream>,
    buf: Vec<u8>,
    pos: usize,
    ended: bool,
    closed: Option<StreamClose>,
}

impl StreamReader {
    /// The next frame; a connection that ends without a `Close` is an error.
    fn frame(&mut self) -> io::Result<StreamFrame> {
        stream::read_frame(&mut self.socket)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the stream's connection ended without closing it",
            )
        })
    }

    /// Reads to the stream's close, dropping data not read yet: `Ok` when both ends finished,
    /// the reason otherwise. For a caller that sent everything and needs to know it was taken.
    pub fn wait_closed(mut self) -> io::Result<()> {
        loop {
            if let Some(why) = &self.closed {
                return match why {
                    StreamClose::Ok => Ok(()),
                    why => Err(closed_error(why)),
                };
            }
            match self.frame()? {
                StreamFrame::Close(why) => self.closed = Some(why),
                StreamFrame::Data(_) | StreamFrame::End => {}
            }
        }
    }
}

impl Read for StreamReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let n = out.len().min(self.buf.len() - self.pos);
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.ended || out.is_empty() {
                return Ok(0);
            }
            if let Some(why) = &self.closed {
                return Err(closed_error(why));
            }
            match self.frame()? {
                StreamFrame::Data(data) => {
                    self.buf = data;
                    self.pos = 0;
                }
                StreamFrame::End => self.ended = true,
                StreamFrame::Close(StreamClose::Ok) => {
                    self.ended = true;
                    self.closed = Some(StreamClose::Ok);
                }
                StreamFrame::Close(why) => self.closed = Some(why),
            }
        }
    }
}

/// The bytes sent to the daemon's end. [`StreamWriter::finish`] ends them; dropping the
/// writer without finishing aborts the stream, so a cut-off transfer never looks whole.
pub struct StreamWriter {
    socket: UnixStream,
    ended: bool,
    closed: bool,
}

impl StreamWriter {
    /// Sends no more data (`End`); the other direction goes on.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if !self.ended {
            self.ended = true;
            stream::write_frame(&mut self.socket, &StreamFrame::End)?;
        }
        Ok(())
    }

    /// Ends the stream in both directions, saying `why`. A reason too long for one frame is
    /// refused (`InvalidInput`), and the stream is aborted instead.
    pub fn close(mut self, why: StreamClose) -> io::Result<()> {
        stream::write_frame(&mut self.socket, &StreamFrame::Close(why))?;
        self.closed = true;
        Ok(())
    }
}

impl Write for StreamWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.ended || self.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let n = buf.len().min(MAX_DATA);
        if n == 0 {
            return Ok(0);
        }
        stream::write_data(&mut self.socket, &buf[..n])?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for StreamWriter {
    fn drop(&mut self) {
        if !self.ended && !self.closed {
            let _ =
                stream::write_frame(&mut self.socket, &StreamFrame::Close(StreamClose::Aborted));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_proto::{CAP_ACTIVE_SIZE, Event};

    fn hello_answer(id: u64, capabilities: &[&str]) -> ServerMessage {
        ServerMessage::Response {
            id,
            result: Ok(Response::Hello {
                version: PROTOCOL_VERSION,
                daemon_pid: 1,
                restored: false,
                build: "fixture".into(),
                capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
            }),
        }
    }

    /// A daemon that answers the Hello (an event before and after it), takes the ticket, and
    /// then runs `then` on the stream.
    fn fake_daemon(
        ticket: &'static str,
        then: impl FnOnce(&mut UnixStream) + Send + 'static,
    ) -> (UnixStream, std::thread::JoinHandle<()>) {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            assert!(matches!(hello.request, Request::Hello { ui: false, .. }));
            let event = ServerMessage::Event(Event::PaneClosed { pane: 9 });
            codec::write_message(&mut peer, &event).unwrap();
            codec::write_message(
                &mut peer,
                &hello_answer(hello.id, &[CAP_ACTIVE_SIZE, CAP_STREAMS]),
            )
            .unwrap();
            codec::write_message(&mut peer, &event).unwrap();
            let open: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            let Request::StreamOpen { ticket: got } = open.request else {
                panic!("{:?}", open.request);
            };
            assert_eq!(got, ticket);
            codec::write_message(
                &mut peer,
                &ServerMessage::Response {
                    id: open.id,
                    result: Ok(Response::Ok),
                },
            )
            .unwrap();
            then(&mut peer);
        });
        (ours, server)
    }

    #[test]
    fn opening_passes_over_events_and_carries_frames_both_ways() {
        let (ours, server) = fake_daemon("t1", |peer| {
            stream::write_frame(peer, &StreamFrame::Data(b"hel".to_vec())).unwrap();
            stream::write_frame(peer, &StreamFrame::Data(b"lo".to_vec())).unwrap();
            stream::write_frame(peer, &StreamFrame::End).unwrap();
            let mut got = Vec::new();
            loop {
                match stream::read_frame(peer).unwrap().unwrap() {
                    StreamFrame::Data(d) => got.extend(d),
                    StreamFrame::End => break,
                    other => panic!("{other:?}"),
                }
            }
            assert_eq!(got.len(), 3 * MAX_DATA + 1);
            stream::write_frame(peer, &StreamFrame::Close(StreamClose::Ok)).unwrap();
        });
        let (mut r, mut w) = open_on(ours, "t1").unwrap();
        let mut got = String::new();
        r.read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello");
        w.write_all(&vec![1u8; 3 * MAX_DATA + 1]).unwrap();
        w.finish().unwrap();
        assert!(w.write(b"late").is_err());
        r.wait_closed().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn a_refused_stream_reads_as_refused() {
        let (ours, server) = fake_daemon("t2", |peer| {
            let why = StreamClose::Refused("nothing on 9".into());
            stream::write_frame(peer, &StreamFrame::Close(why)).unwrap();
        });
        let (mut r, _w) = open_on(ours, "t2").unwrap();
        let e = r.read(&mut [0; 8]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
        assert!(e.to_string().contains("nothing on 9"), "{e}");
        server.join().unwrap();
    }

    #[test]
    fn dropping_an_unfinished_writer_aborts() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ours, server) = fake_daemon("t3", move |peer| {
            tx.send(stream::read_frame(peer).unwrap()).unwrap();
        });
        let (_r, w) = open_on(ours, "t3").unwrap();
        drop(w);
        assert_eq!(
            rx.recv().unwrap(),
            Some(StreamFrame::Close(StreamClose::Aborted))
        );
        server.join().unwrap();
    }

    #[test]
    fn a_close_reason_too_long_aborts_instead() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ours, server) = fake_daemon("t7", move |peer| {
            tx.send(stream::read_frame(peer).unwrap()).unwrap();
        });
        let (_r, w) = open_on(ours, "t7").unwrap();
        let why = StreamClose::Internal("x".repeat(MAX_DATA * 2));
        assert_eq!(
            w.close(why).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            rx.recv().unwrap(),
            Some(StreamFrame::Close(StreamClose::Aborted))
        );
        server.join().unwrap();
    }

    #[test]
    fn a_daemon_without_streams_is_refused_before_the_ticket() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            codec::write_message(&mut peer, &hello_answer(hello.id, &[CAP_ACTIVE_SIZE])).unwrap();
            // Shut down on refusal: nothing more comes.
            assert!(codec::read_frame(&mut peer).unwrap().is_none());
        });
        let Err(ClientError::Daemon(e)) = open_on(ours, "t4") else {
            panic!("opened without streams");
        };
        assert_eq!(e, NO_STREAMS);
        server.join().unwrap();
    }

    #[test]
    fn an_answer_other_than_ok_does_not_open_the_stream() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            let caps = [CAP_ACTIVE_SIZE, CAP_STREAMS];
            codec::write_message(&mut peer, &hello_answer(hello.id, &caps)).unwrap();
            let open: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            codec::write_message(
                &mut peer,
                &ServerMessage::Response {
                    id: open.id,
                    result: Ok(Response::Panes(Vec::new())),
                },
            )
            .unwrap();
            // Shut down on refusal: no frame follows.
            assert!(codec::read_frame(&mut peer).unwrap().is_none());
        });
        let Err(ClientError::Daemon(e)) = open_on(ours, "t6") else {
            panic!("opened on an answer other than Ok");
        };
        assert_eq!(e, UNREADABLE_ANSWER);
        server.join().unwrap();
    }

    #[test]
    fn a_refused_ticket_is_the_daemons_error() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let hello: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            let caps = [CAP_ACTIVE_SIZE, CAP_STREAMS];
            codec::write_message(&mut peer, &hello_answer(hello.id, &caps)).unwrap();
            let open: Envelope = codec::read_message(&mut peer).unwrap().unwrap();
            codec::write_message(
                &mut peer,
                &ServerMessage::Response {
                    id: open.id,
                    result: Err("unknown or used stream ticket".into()),
                },
            )
            .unwrap();
        });
        let Err(ClientError::Daemon(e)) = open_on(ours, "t5") else {
            panic!("opened with a refused ticket");
        };
        assert!(e.contains("unknown"), "{e}");
        server.join().unwrap();
    }
}
