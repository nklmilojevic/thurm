//! Length-prefixed postcard framing: `u32` little-endian length followed by the payload.

use std::io::{self, Read, Write};

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Event, ServerMessage};

/// Upper bound for a single message; guards against garbage on the socket.
pub const MAX_MESSAGE: usize = 256 * 1024 * 1024;

/// Events of at least this many bytes are deflated for a client that asked for it
/// ([`crate::CAP_DEFLATE`]): smaller ones don't pay for the compressor.
pub const DEFLATE_MIN: usize = 1024;

/// The most a compressed event may inflate to, and the largest event deflated (a bigger one goes
/// plain). Output is at most a parse slice (256 KiB, see the daemon's `PARSE_SLICE`) and a
/// replay (`Attach`) is a pane's screen and scrollback, which is larger only with a big
/// scrollback; that goes plain. A peer's compressed frame can be a few KiB and inflate to
/// 1000 times that, so the bound keeps a client from allocating more than this per frame.
pub const MAX_INFLATED: usize = 8 * 1024 * 1024;

pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let payload =
        postcard::to_stdvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(frame(&payload))
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// The frame for `msg` to a client that does (`deflate`) or doesn't have [`crate::CAP_DEFLATE`].
/// An event of at least [`DEFLATE_MIN`] bytes and at most [`MAX_INFLATED`] goes as an
/// [`Event::Compressed`] when that is smaller; anything else goes as [`encode`] would send it.
pub fn encode_frame(msg: &ServerMessage, deflate: bool) -> io::Result<Vec<u8>> {
    let payload =
        postcard::to_stdvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if deflate
        && matches!(msg, ServerMessage::Event(_))
        && (DEFLATE_MIN..=MAX_INFLATED).contains(&payload.len())
    {
        let data = deflated(&payload)?;
        if data.len() < payload.len() {
            return encode(&ServerMessage::Event(Event::Compressed { data }));
        }
    }
    Ok(frame(&payload))
}

/// Decodes a message frame, unwrapping an [`Event::Compressed`]. Fails on a frame this build
/// can't read, on a compressed event inside a compressed one, and on one that inflates past
/// [`MAX_INFLATED`].
pub fn decode_message(frame: &[u8]) -> io::Result<ServerMessage> {
    let msg: ServerMessage = decode(frame)?;
    let ServerMessage::Event(Event::Compressed { data }) = msg else {
        return Ok(msg);
    };
    let inner: ServerMessage = decode(&inflated(&data, MAX_INFLATED)?)?;
    if matches!(inner, ServerMessage::Event(Event::Compressed { .. })) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "compressed message inside a compressed one",
        ));
    }
    Ok(inner)
}

fn deflated(payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut enc = DeflateEncoder::new(Vec::with_capacity(payload.len() / 2), Compression::fast());
    enc.write_all(payload)?;
    enc.finish()
}

/// Inflates raw deflate `data`; fails when it comes to more than `limit` bytes.
fn inflated(data: &[u8], limit: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    DeflateDecoder::new(data)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    Ok(out)
}

pub fn write_message<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let buf = encode(msg)?;
    w.write_all(&buf)?;
    w.flush()
}

/// Read one message's bytes (without decoding). `Ok(None)` on a clean EOF at a boundary.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
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
    Ok(Some(buf))
}

/// Decodes a message read with [`read_frame`].
pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> io::Result<T> {
    postcard::from_bytes(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The request id at the start of an `Envelope` frame whose request didn't decode (a request
/// of a newer protocol), so it can be answered with an error.
pub fn envelope_id(frame: &[u8]) -> Option<u64> {
    postcard::take_from_bytes::<u64>(frame)
        .ok()
        .map(|(id, _)| id)
}

/// The request id of a `ServerMessage::Response` frame whose result didn't decode (a response
/// of a newer protocol), so the request waiting for it can be failed.
pub fn response_id(frame: &[u8]) -> Option<u64> {
    // Variant 0 is `Response`, whose first field is the id.
    let (variant, rest) = postcard::take_from_bytes::<u32>(frame).ok()?;
    if variant != 0 {
        return None;
    }
    postcard::take_from_bytes::<u64>(rest)
        .ok()
        .map(|(id, _)| id)
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

    /// A notification without a sound of its own (a finished command, a program's OSC 9)
    /// decodes, as it does from an older daemon's `Some`.
    #[test]
    fn notify_roundtrips_with_and_without_a_sound() {
        for sound in [None, Some("Glass".to_owned())] {
            let out = ServerMessage::Event(Event::Notify {
                pane: 2,
                title: "make finished".into(),
                body: "after 20s".into(),
                permission: None,
                sound: sound.clone(),
            });
            let bytes = encode(&out).unwrap();
            let back: ServerMessage = read_message(&mut &bytes[..]).unwrap().unwrap();
            match back {
                ServerMessage::Event(Event::Notify {
                    pane: 2, sound: s, ..
                }) => assert_eq!(s, sound),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn eof_is_none() {
        let empty: &[u8] = &[];
        let r: Option<Envelope> = read_message(&mut &empty[..]).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn unknown_requests_keep_their_id() {
        // An Envelope whose request variant this build doesn't have: id 7, variant 999.
        let mut frame = Vec::new();
        frame.extend_from_slice(&postcard::to_stdvec(&7u64).unwrap());
        frame.extend_from_slice(&postcard::to_stdvec(&999u32).unwrap());
        assert!(decode::<crate::Envelope>(&frame).is_err());
        assert_eq!(envelope_id(&frame), Some(7));
        let mut buf = (frame.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&frame);
        assert_eq!(read_frame(&mut buf.as_slice()).unwrap(), Some(frame));
    }

    #[test]
    fn unknown_responses_keep_their_id() {
        let known = crate::ServerMessage::Response {
            id: 42,
            result: Ok(crate::Response::Ok),
        };
        let frame = postcard::to_stdvec(&known).unwrap();
        assert_eq!(response_id(&frame), Some(42));
        // Variant 0 (Response), id 9, then a result this build can't decode.
        assert_eq!(response_id(&[0, 9, 0, 0xE7, 0x07]), Some(9));
        let event = crate::ServerMessage::Event(crate::Event::Bell { pane: 1 });
        assert_eq!(response_id(&postcard::to_stdvec(&event).unwrap()), None);
    }

    #[test]
    fn protocol_18_daemons_without_pane_program_are_refused() {
        let ours: Vec<String> = CAPABILITIES.iter().map(|c| (*c).to_owned()).collect();
        assert!(hello_compatible(PROTOCOL_VERSION, &ours));
        let before = ["hello-capabilities", "temp-file", "remote-layout"].map(String::from);
        assert!(!hello_compatible(18, &before));
        assert!(!hello_compatible(17, &ours));
    }

    fn output(len: usize) -> ServerMessage {
        // Terminal output: repetitive, so it deflates.
        ServerMessage::Event(Event::Output {
            pane: 4,
            data: b"\x1b[32mok\x1b[0m line of output\r\n"
                .iter()
                .copied()
                .cycle()
                .take(len)
                .collect(),
        })
    }

    fn payload(frame: &[u8]) -> &[u8] {
        &frame[4..]
    }

    #[test]
    fn deflated_events_round_trip_through_decode_message() {
        let attach = ServerMessage::Event(Event::Attach {
            pane: 4,
            size: PaneSize {
                cols: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
            state: b"\x1b[H".repeat(4096),
        });
        for msg in [attach, output(64 * 1024)] {
            let frame = encode_frame(&msg, true).unwrap();
            let plain = encode_frame(&msg, false).unwrap();
            assert!(frame.len() < plain.len());
            assert!(matches!(
                decode::<ServerMessage>(payload(&frame)),
                Ok(ServerMessage::Event(Event::Compressed { .. }))
            ));
            let back = decode_message(payload(&frame)).unwrap();
            let expect = decode_message(payload(&plain)).unwrap();
            assert_eq!(
                postcard::to_stdvec(&back).unwrap(),
                postcard::to_stdvec(&expect).unwrap()
            );
        }
    }

    #[test]
    fn small_responses_and_clients_without_the_capability_get_plain_frames() {
        let response = ServerMessage::Response {
            id: 3,
            result: Ok(Response::Ok),
        };
        for msg in [output(DEFLATE_MIN - 100), response, output(64 * 1024)] {
            assert_eq!(encode_frame(&msg, false).unwrap(), encode(&msg).unwrap());
        }
        assert_eq!(
            encode_frame(&output(DEFLATE_MIN - 100), true).unwrap(),
            encode(&output(DEFLATE_MIN - 100)).unwrap(),
        );
    }

    #[test]
    fn a_compressed_event_inside_a_compressed_one_is_rejected() {
        let inner = payload(&encode_frame(&output(8192), true).unwrap()).to_vec();
        assert!(matches!(
            decode::<ServerMessage>(&inner),
            Ok(ServerMessage::Event(Event::Compressed { .. }))
        ));
        let nested = encode(&ServerMessage::Event(Event::Compressed {
            data: deflated(&inner).unwrap(),
        }))
        .unwrap();
        assert!(decode_message(payload(&nested)).is_err());
    }

    #[test]
    fn an_oversized_inflation_is_rejected() {
        let data = deflated(&vec![0u8; 64 * 1024]).unwrap();
        assert!(inflated(&data, 1024).is_err());
        assert_eq!(inflated(&data, 64 * 1024).unwrap().len(), 64 * 1024);
    }

    /// A small frame that inflates past the bound is refused, before a client holds it all.
    #[test]
    fn a_deflate_bomb_is_refused() {
        let bomb = deflated(&vec![0u8; MAX_INFLATED + 1]).unwrap();
        assert!(bomb.len() < 64 * 1024, "{}", bomb.len());
        let frame = encode(&ServerMessage::Event(Event::Compressed { data: bomb })).unwrap();
        assert!(decode_message(payload(&frame)).is_err());
    }

    const SCROLLBACK: &[u8] = b"\x1b[32mscrollback line\r\n";

    /// A big replay still round-trips: the daemon deflates it up to the bound.
    #[test]
    fn a_large_attach_round_trips_compressed() {
        let attach = ServerMessage::Event(Event::Attach {
            pane: 4,
            size: PaneSize {
                cols: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
            state: SCROLLBACK.repeat((MAX_INFLATED - 1024) / SCROLLBACK.len()),
        });
        let frame = encode_frame(&attach, true).unwrap();
        assert!(matches!(
            decode::<ServerMessage>(payload(&frame)),
            Ok(ServerMessage::Event(Event::Compressed { .. }))
        ));
        let back = decode_message(payload(&frame)).unwrap();
        assert_eq!(
            postcard::to_stdvec(&back).unwrap(),
            postcard::to_stdvec(&attach).unwrap()
        );
    }

    /// An event past the bound goes plain, so the receiver never refuses a legitimate one.
    #[test]
    fn an_event_past_the_bound_is_sent_plain() {
        let attach = ServerMessage::Event(Event::Attach {
            pane: 4,
            size: PaneSize {
                cols: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
            state: SCROLLBACK.repeat(MAX_INFLATED / SCROLLBACK.len() + 1),
        });
        let frame = encode_frame(&attach, true).unwrap();
        assert_eq!(frame, encode(&attach).unwrap());
    }

    #[test]
    fn placed_floats_leave_older_ui_commands_as_they_were() {
        // An older app decodes `OpenFloating` by its variant index: the new commands come after it.
        let open = UiCommand::OpenFloating {
            pane: 5,
            target: None,
            background: false,
        };
        assert_eq!(postcard::to_allocvec(&open).unwrap(), [14, 5, 0, 0]);

        let place = UiCommand::PlaceFloating {
            pane: 5,
            place: FloatPlace::Cells {
                anchor: 2,
                col: -1,
                row: 3,
                cols: 40,
                rows: 10,
            },
        };
        let bytes = postcard::to_allocvec(&place).unwrap();
        assert_eq!(bytes[0], 16);
        assert_eq!(postcard::from_bytes::<UiCommand>(&bytes).unwrap(), place);
        // The app reads UI commands as JSON.
        assert_eq!(
            serde_json::to_string(&place).unwrap(),
            r#"{"PlaceFloating":{"pane":5,"place":{"Cells":{"anchor":2,"col":-1,"row":3,"cols":40,"rows":10}}}}"#
        );
    }
}
