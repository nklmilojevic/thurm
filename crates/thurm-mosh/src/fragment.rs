//! A compressed instruction travels as fragments: `id (8, big endian) || num (2, big endian,
//! top bit set on the last) || bytes`. Only the newest instruction is assembled: fragments of a
//! newer one start over, those of an older one (reordered, late) are dropped.

use crate::Error;

/// Fragment payload size. A datagram adds 38 bytes to it (fragment header, timestamps, nonce,
/// tag) and IPv6 and UDP 48 more: 1086 bytes, under IPv6's minimum MTU of 1280 with room for
/// extension headers and tunnels (VPNs, mobile networks), so nothing relies on fragmentation.
pub const MAX_FRAGMENT: usize = 1000;
/// What a datagram adds to a fragment's payload.
pub const OVERHEAD: usize = HEADER + 4 + 8 + 16;
const FINAL: u16 = 1 << 15;
const HEADER: usize = 10;

/// `data` (an instruction, compressed) as fragments of instruction `id`.
pub fn split(id: u64, data: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(MAX_FRAGMENT).collect()
    };
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let num = i as u16 | if i == last { FINAL } else { 0 };
            let mut f = Vec::with_capacity(HEADER + chunk.len());
            f.extend_from_slice(&id.to_be_bytes());
            f.extend_from_slice(&num.to_be_bytes());
            f.extend_from_slice(chunk);
            f
        })
        .collect()
}

#[derive(Default)]
pub struct Assembly {
    id: Option<u64>,
    parts: Vec<Option<Vec<u8>>>,
    /// How many parts there are, once the final one has come.
    total: Option<usize>,
    have: usize,
}

impl Assembly {
    /// Adds a fragment; returns the whole instruction when this completed it.
    pub fn add(&mut self, fragment: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        if fragment.len() < HEADER {
            return Err(Error::Packet("fragment too short"));
        }
        let id = u64::from_be_bytes(fragment[..8].try_into().unwrap());
        let raw = u16::from_be_bytes([fragment[8], fragment[9]]);
        let (num, last) = ((raw & !FINAL) as usize, raw & FINAL != 0);
        if self.id.is_some_and(|current| id < current) {
            return Ok(None);
        }
        if self.id != Some(id) {
            *self = Assembly {
                id: Some(id),
                ..Default::default()
            };
        }
        if self.parts.len() <= num {
            self.parts.resize(num + 1, None);
        }
        if self.parts[num].is_none() {
            self.parts[num] = Some(fragment[HEADER..].to_vec());
            self.have += 1;
        }
        if last {
            self.total = Some(num + 1);
        }
        if self.total == Some(self.have) && self.parts.len() == self.have {
            let whole = self.parts.drain(..).flatten().flatten().collect();
            *self = Assembly {
                id: Some(id),
                ..Default::default()
            };
            return Ok(Some(whole));
        }
        Ok(None)
    }
}
