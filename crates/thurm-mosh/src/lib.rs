//! A mosh client: the State Synchronization Protocol over UDP to a `mosh-server`, without the
//! terminal side (the caller feeds the host's screen diffs to its own emulator).
//!
//! Written from the protocol's wire format, not from mosh's sources:
//!
//! - [`bootstrap`]: start `mosh-server new` over ssh and read its `MOSH CONNECT <port> <key>`.
//! - [`crypto`]: datagrams sealed with AES-128-OCB under the session key; the nonce is the
//!   direction bit and a 63-bit sequence number.
//! - [`fragment`]: an instruction is zlib-compressed and cut into numbered fragments.
//! - [`wire`]: the protobuf messages (transport instruction, user and host messages).
//! - [`ssp`]: both ends keep numbered states and send diffs between them; acks and heartbeats.
//! - [`session`]: the UDP driver, which can move to a new socket (roaming) at any time.

pub mod bootstrap;
pub mod crypto;
pub mod fragment;
pub mod session;
pub mod ssp;
pub mod wire;

pub use session::{MoshEvent, MoshSession, Status};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A datagram that does not decrypt, is too short, or comes from our own direction.
    Packet(&'static str),
    /// A message that does not parse.
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Packet(m) => write!(f, "mosh packet: {m}"),
            Error::Protocol(m) => write!(f, "mosh protocol: {m}"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
