//! Byte streams through the daemon connection ([`CAP_STREAMS`](crate::CAP_STREAMS)).
//!
//! A stream takes two connections. On the normal one, a request reserves the stream and the
//! daemon answers with a single-use ticket ([`Request::DialBegin`](crate::Request::DialBegin) →
//! [`Response::Ticket`](crate::Response::Ticket)). A second connection to the same socket says
//! `Hello`, presents the ticket ([`Request::StreamOpen`](crate::Request::StreamOpen)) and, once
//! the daemon answers `Ok`, both sides send only [`StreamFrame`]s on it, in the length framing
//! of [`codec`](crate::codec).
//!
//! Frames rather than raw bytes: the tailnet path is a WebSocket relay, and neither it nor an
//! ssh socket forward is sure to carry a socket half-close. [`StreamFrame::End`] is the
//! half-close, and [`StreamFrame::Close`] says why a stream ended, on every transport. The
//! daemon sends `Close(Ok)` once both directions have ended, then closes the connection.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// The most bytes one [`StreamFrame::Data`] carries.
pub const MAX_DATA: usize = 64 * 1024;

/// The largest frame either side reads: a full `Data` and its header, or a close reason.
pub const MAX_FRAME: usize = MAX_DATA + 64;

/// How long a ticket stays valid unless used.
pub const TICKET_TTL_MS: u32 = 60_000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum StreamFrame {
    /// Bytes, at most [`MAX_DATA`].
    Data(#[serde(with = "crate::bytes")] Vec<u8>),
    /// The sender sends no more data; the other direction goes on.
    End,
    /// The stream is over in both directions.
    Close(StreamClose),
}

/// Why a stream ended.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum StreamClose {
    /// Both directions ended.
    Ok,
    /// One side gave up.
    Aborted,
    /// The target could not be reached (nothing listens on the port).
    Refused(String),
    /// Reading or writing the target failed.
    Internal(String),
}

impl std::fmt::Display for StreamClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamClose::Ok => f.write_str("closed"),
            StreamClose::Aborted => f.write_str("aborted"),
            StreamClose::Refused(why) => write!(f, "refused: {why}"),
            StreamClose::Internal(why) => write!(f, "failed: {why}"),
        }
    }
}

/// Writes one frame.
pub fn write_frame<W: Write>(w: &mut W, frame: &StreamFrame) -> io::Result<()> {
    w.write_all(&crate::codec::encode(frame)?)?;
    w.flush()
}

/// Writes `data` as one [`StreamFrame::Data`] without copying it (at most [`MAX_DATA`]).
pub fn write_data<W: Write>(w: &mut W, data: &[u8]) -> io::Result<()> {
    #[derive(Serialize)]
    enum Borrowed<'a> {
        Data(#[serde(with = "crate::bytes")] &'a [u8]),
    }
    debug_assert!(data.len() <= MAX_DATA);
    w.write_all(&crate::codec::encode(&Borrowed::Data(data))?)?;
    w.flush()
}

/// Reads one frame; `Ok(None)` when the connection ends between frames. A frame over
/// [`MAX_FRAME`], or one that doesn't decode, is an error.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<StreamFrame>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stream frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    crate::codec::decode(&buf).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = [
            StreamFrame::Data(b"hello".to_vec()),
            StreamFrame::End,
            StreamFrame::Close(StreamClose::Ok),
            StreamFrame::Close(StreamClose::Aborted),
            StreamFrame::Close(StreamClose::Refused("nothing on 9".into())),
            StreamFrame::Close(StreamClose::Internal("broken pipe".into())),
        ];
        let mut wire = Vec::new();
        for f in &frames {
            write_frame(&mut wire, f).unwrap();
        }
        let mut r = wire.as_slice();
        for f in &frames {
            assert_eq!(read_frame(&mut r).unwrap().as_ref(), Some(f));
        }
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn borrowed_data_is_the_same_frame() {
        let data = vec![7u8; MAX_DATA];
        let mut borrowed = Vec::new();
        write_data(&mut borrowed, &data).unwrap();
        let mut owned = Vec::new();
        write_frame(&mut owned, &StreamFrame::Data(data.clone())).unwrap();
        assert_eq!(borrowed, owned);
        assert!(borrowed.len() - 4 <= MAX_FRAME);
        assert_eq!(
            read_frame(&mut borrowed.as_slice()).unwrap(),
            Some(StreamFrame::Data(data))
        );
    }

    #[test]
    fn an_oversized_frame_is_refused() {
        let mut wire = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        wire.resize(4 + MAX_FRAME + 1, 0);
        let e = read_frame(&mut wire.as_slice()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }
}
