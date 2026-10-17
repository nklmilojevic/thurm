//! The protobuf messages, encoded by hand (a handful of fields; no code generation):
//!
//! - transport instruction: `protocol_version` 1, `old_num` 2, `new_num` 3, `ack_num` 4,
//!   `throwaway_num` 5 (varints), `diff` 6, `chaff` 7 (bytes);
//! - user message: repeated instruction 1, each holding a keystroke (2: `keys` 4) or a resize
//!   (3: `width` 5, `height` 6);
//! - host message: repeated instruction 1, each holding host bytes (2: `hoststring` 4), a
//!   resize (3) or an echo ack (7: `echo_ack_num` 8).
//!
//! Instructions are zlib-compressed on the wire ([`compress`]).

use std::io::{Read as _, Write as _};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use crate::Error;

pub const PROTOCOL_VERSION: u64 = 2;
/// Larger decompressed instructions are refused (a full screen diff is far smaller).
const MAX_INSTRUCTION: u64 = 16 << 20;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub protocol_version: u64,
    pub old_num: u64,
    pub new_num: u64,
    pub ack_num: u64,
    pub throwaway_num: u64,
    pub diff: Vec<u8>,
}

/// What the user did, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserEvent {
    Keys(Vec<u8>),
    Resize { width: u16, height: u16 },
}

/// What the host sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// Output for the terminal: bytes that turn the previous screen into the new one.
    Bytes(Vec<u8>),
    Resize {
        width: i64,
        height: i64,
    },
    EchoAck(u64),
}

const VARINT: u8 = 0;
const LEN: u8 = 2;

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_key(out: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(out, u64::from(field << 3 | u32::from(wire)));
}

fn put_uint(out: &mut Vec<u8>, field: u32, v: u64) {
    put_key(out, field, VARINT);
    put_varint(out, v);
}

fn put_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    put_key(out, field, LEN);
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// A protobuf field: its number and value.
enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    /// 32 and 64-bit fixed fields: none of ours, skipped.
    Fixed,
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, Error> {
        let mut v = 0u64;
        for i in 0..10 {
            let (&b, rest) = self
                .buf
                .split_first()
                .ok_or_else(|| Error::Protocol("truncated varint".into()))?;
            self.buf = rest;
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(Error::Protocol("varint too long".into()))
    }

    fn take(&mut self, n: u64) -> Result<&'a [u8], Error> {
        let n = usize::try_from(n).map_err(|_| Error::Protocol("length".into()))?;
        if n > self.buf.len() {
            return Err(Error::Protocol("truncated field".into()));
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn next(&mut self) -> Result<Option<(u64, Field<'a>)>, Error> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = match key & 7 {
            0 => Field::Varint(self.varint()?),
            1 => {
                self.take(8)?;
                Field::Fixed
            }
            2 => {
                let n = self.varint()?;
                Field::Bytes(self.take(n)?)
            }
            5 => {
                self.take(4)?;
                Field::Fixed
            }
            w => return Err(Error::Protocol(format!("wire type {w}"))),
        };
        Ok(Some((key >> 3, field)))
    }
}

fn fields(
    buf: &[u8],
    mut each: impl FnMut(u64, Field<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut r = Reader { buf };
    while let Some((n, f)) = r.next()? {
        each(n, f)?;
    }
    Ok(())
}

impl Instruction {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.diff.len() + 32);
        put_uint(&mut out, 1, self.protocol_version);
        put_uint(&mut out, 2, self.old_num);
        put_uint(&mut out, 3, self.new_num);
        put_uint(&mut out, 4, self.ack_num);
        put_uint(&mut out, 5, self.throwaway_num);
        put_bytes(&mut out, 6, &self.diff);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Instruction, Error> {
        let mut i = Instruction::default();
        fields(buf, |n, f| {
            match (n, f) {
                (1, Field::Varint(v)) => i.protocol_version = v,
                (2, Field::Varint(v)) => i.old_num = v,
                (3, Field::Varint(v)) => i.new_num = v,
                (4, Field::Varint(v)) => i.ack_num = v,
                (5, Field::Varint(v)) => i.throwaway_num = v,
                (6, Field::Bytes(b)) => i.diff = b.to_vec(),
                _ => {}
            }
            Ok(())
        })?;
        Ok(i)
    }
}

/// The user message carrying `events`.
pub fn encode_user(events: &[UserEvent]) -> Vec<u8> {
    let mut out = Vec::new();
    for ev in events {
        let mut inner = Vec::new();
        let mut instruction = Vec::new();
        match ev {
            UserEvent::Keys(keys) => {
                put_bytes(&mut inner, 4, keys);
                put_bytes(&mut instruction, 2, &inner);
            }
            UserEvent::Resize { width, height } => {
                put_uint(&mut inner, 5, u64::from(*width));
                put_uint(&mut inner, 6, u64::from(*height));
                put_bytes(&mut instruction, 3, &inner);
            }
        }
        put_bytes(&mut out, 1, &instruction);
    }
    out
}

/// The events of a host message, in order.
pub fn decode_host(buf: &[u8]) -> Result<Vec<HostEvent>, Error> {
    let mut events = Vec::new();
    fields(buf, |n, f| {
        let (1, Field::Bytes(instruction)) = (n, f) else {
            return Ok(());
        };
        fields(instruction, |n, f| {
            let Field::Bytes(inner) = f else {
                return Ok(());
            };
            match n {
                2 => fields(inner, |n, f| {
                    if let (4, Field::Bytes(b)) = (n, f) {
                        events.push(HostEvent::Bytes(b.to_vec()));
                    }
                    Ok(())
                }),
                3 => {
                    let (mut width, mut height) = (0, 0);
                    fields(inner, |n, f| {
                        match (n, f) {
                            // int32: negative values come as ten-byte varints.
                            (5, Field::Varint(v)) => width = v as i64,
                            (6, Field::Varint(v)) => height = v as i64,
                            _ => {}
                        }
                        Ok(())
                    })?;
                    events.push(HostEvent::Resize { width, height });
                    Ok(())
                }
                7 => fields(inner, |n, f| {
                    if let (8, Field::Varint(v)) = (n, f) {
                        events.push(HostEvent::EchoAck(v));
                    }
                    Ok(())
                }),
                _ => Ok(()),
            }
        })
    })?;
    Ok(events)
}

pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut z = ZlibEncoder::new(
        Vec::with_capacity(data.len() / 2 + 16),
        Compression::default(),
    );
    z.write_all(data).expect("writing to a Vec");
    z.finish().expect("writing to a Vec")
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    ZlibDecoder::new(data)
        .take(MAX_INSTRUCTION + 1)
        .read_to_end(&mut out)
        .map_err(|e| Error::Protocol(format!("zlib: {e}")))?;
    if out.len() as u64 > MAX_INSTRUCTION {
        return Err(Error::Protocol("instruction too large".into()));
    }
    Ok(out)
}
