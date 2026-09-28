//! Length-prefixed postcard framing: `u32` little-endian length followed by the payload.

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Upper bound for a single message; guards against garbage on the socket.
pub const MAX_MESSAGE: usize = 256 * 1024 * 1024;

pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let payload =
        postcard::to_stdvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn write_message<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let buf = encode(msg)?;
    w.write_all(&buf)?;
    w.flush()
}

/// Read one message. Returns `Ok(None)` on a clean EOF at a message boundary.
pub fn read_message<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    postcard::from_bytes(&buf)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn roundtrip_request_and_frame() {
        let env = Envelope {
            id: 7,
            request: Request::Key {
                pane: 3,
                key: KeyEvent {
                    key: Key::Named(NamedKey::F(5)),
                    mods: mods::CTRL | mods::SHIFT,
                    action: KeyAction::Press,
                    text: String::new(),
                    shifted: None,
                    base_layout: None,
                },
            },
        };
        let bytes = encode(&env).unwrap();
        let back: Envelope = read_message(&mut &bytes[..]).unwrap().unwrap();
        assert_eq!(back.id, 7);
        assert!(matches!(back.request, Request::Key { pane: 3, .. }));

        let out = ServerMessage::Event(Event::Output {
            pane: 1,
            data: "e\u{301}\x1b[m".as_bytes().to_vec(),
        });
        let bytes = encode(&out).unwrap();
        let back: ServerMessage = read_message(&mut &bytes[..]).unwrap().unwrap();
        match back {
            ServerMessage::Event(Event::Output { pane: 1, data }) => {
                assert_eq!(data, "e\u{301}\x1b[m".as_bytes());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn eof_is_none() {
        let empty: &[u8] = &[];
        let r: Option<Envelope> = read_message(&mut &empty[..]).unwrap();
        assert!(r.is_none());
    }
}
