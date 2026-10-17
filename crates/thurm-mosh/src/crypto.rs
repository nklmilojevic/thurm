//! Datagrams: `nonce (8, big endian) || OCB(key, 0000 || nonce, plaintext) || tag (16)`. The
//! nonce's top bit is the direction (set from the server), the rest a sequence number that
//! never repeats for a key. The plaintext starts with two 16-bit timestamps.

use aes::Aes128;
use base64::Engine as _;
use ocb3::Ocb3;
use ocb3::aead::{Aead, KeyInit};

use crate::Error;

/// The nonce bit of datagrams the server sends.
pub const TO_CLIENT: u64 = 1 << 63;
const SEQ_MASK: u64 = !TO_CLIENT;
const TAG: usize = 16;
/// No timestamp to send back.
pub const NO_TIMESTAMP: u16 = u16::MAX;

/// A session key, as `mosh-server` prints it: 22 characters of unpadded base64.
pub fn parse_key(printed: &str) -> Result<[u8; 16], String> {
    let printed = printed.trim();
    let raw = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(printed.trim_end_matches('='))
        .map_err(|e| format!("the session key is not base64: {e}"))?;
    raw.try_into()
        .map_err(|raw: Vec<u8>| format!("the session key is {} bytes, not 16", raw.len()))
}

pub struct Crypto {
    cipher: Ocb3<Aes128>,
}

/// One decrypted datagram.
#[derive(Debug, PartialEq, Eq)]
pub struct Packet {
    pub seq: u64,
    pub timestamp: u16,
    pub timestamp_reply: u16,
    pub payload: Vec<u8>,
}

impl Crypto {
    pub fn new(key: &[u8; 16]) -> Crypto {
        Crypto {
            cipher: Ocb3::new(key.into()),
        }
    }

    fn nonce(n: u64) -> ocb3::Nonce<ocb3::aead::consts::U12> {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&n.to_be_bytes());
        nonce.into()
    }

    /// The datagram for `packet` going the way `direction` says ([`TO_CLIENT`] or 0).
    pub fn seal(&self, direction: u64, packet: &Packet) -> Vec<u8> {
        let n = direction | (packet.seq & SEQ_MASK);
        let mut plain = Vec::with_capacity(4 + packet.payload.len());
        plain.extend_from_slice(&packet.timestamp.to_be_bytes());
        plain.extend_from_slice(&packet.timestamp_reply.to_be_bytes());
        plain.extend_from_slice(&packet.payload);
        let sealed = self
            .cipher
            .encrypt(&Self::nonce(n), plain.as_slice())
            .expect("OCB encryption of a datagram-sized message");
        let mut out = Vec::with_capacity(8 + sealed.len());
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(&sealed);
        out
    }

    /// Decrypts a datagram that must be going the way `direction` says.
    pub fn open(&self, direction: u64, datagram: &[u8]) -> Result<Packet, Error> {
        if datagram.len() < 8 + TAG + 4 {
            return Err(Error::Packet("too short"));
        }
        let n = u64::from_be_bytes(datagram[..8].try_into().unwrap());
        if n & TO_CLIENT != direction {
            return Err(Error::Packet("wrong direction"));
        }
        let plain = self
            .cipher
            .decrypt(&Self::nonce(n), &datagram[8..])
            .map_err(|_| Error::Packet("does not decrypt"))?;
        Ok(Packet {
            seq: n & SEQ_MASK,
            timestamp: u16::from_be_bytes([plain[0], plain[1]]),
            timestamp_reply: u16::from_be_bytes([plain[2], plain[3]]),
            payload: plain[4..].to_vec(),
        })
    }
}
