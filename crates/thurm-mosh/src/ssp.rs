//! The State Synchronization Protocol, without I/O: both ends hold numbered states and send
//! diffs from one the other end has to a newer one.
//!
//! Ours (the user's input) is a log of [`UserEvent`]s; a state is a prefix of it, and a diff is
//! the events between two states. Every instruction diffs from the newest state the host has
//! acknowledged, so a lost datagram costs nothing but a retransmission. Each send, heartbeats
//! included, makes a new state: the host tells the client is alive by new states arriving.
//!
//! The host's states are screens. The caller shows the newest and keeps a snapshot of each one
//! the client acknowledges ([`Engine::take_snapshot_due`]): the host diffs from the newest state
//! it believes the client has, so from the one shown or from one acknowledged; a diff from any
//! other state is dropped and comes again from an acknowledged one.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::Error;
use crate::wire::{self, Instruction, UserEvent};

/// An instruction acknowledging new host output goes out this long after it came.
pub const ACK_DELAY: Duration = Duration::from_millis(100);
/// Something goes out at least this often: it keeps the host sending and NAT mappings open.
pub const HEARTBEAT: Duration = Duration::from_secs(3);
/// Unacknowledged input is sent again after this, doubling up to [`MAX_RTO`].
pub const MIN_RTO: Duration = Duration::from_millis(250);
pub const MAX_RTO: Duration = Duration::from_secs(1);
/// States sent and not yet acknowledged; beyond this the middle ones are forgotten.
const MAX_SENT: usize = 64;
/// Acknowledged host states kept as diff bases at most.
const MAX_ACKED: usize = 32;

struct Sent {
    num: u64,
    /// The state: the user events up to this absolute index.
    upto: u64,
}

/// A host diff to apply: the screen of state `old_num`, plus `diff`, is state `new_num`.
#[derive(Debug, PartialEq, Eq)]
pub struct Apply {
    pub old_num: u64,
    pub new_num: u64,
    /// The host message (see [`wire::decode_host`]).
    pub diff: Vec<u8>,
    /// No diff will be based on an acknowledged state older than this: their snapshots can go.
    pub oldest: u64,
}

pub struct Engine {
    /// User events not yet acknowledged; `log[0]` is event number `log_base`.
    log: VecDeque<UserEvent>,
    log_base: u64,
    /// Front: the newest state the host acknowledged. Back: the newest sent.
    sent: VecDeque<Sent>,
    last_send: Option<Instant>,
    retries: u32,
    /// The newest host state, shown.
    current: u64,
    /// Host states acknowledged (and snapshotted by the caller), oldest first.
    acked: VecDeque<u64>,
    snapshot_due: Option<u64>,
    ack_due: Option<Instant>,
    last_heard: Option<Instant>,
    started: Instant,
}

impl Engine {
    pub fn new(now: Instant) -> Engine {
        Engine {
            log: VecDeque::new(),
            log_base: 0,
            sent: VecDeque::from([Sent { num: 0, upto: 0 }]),
            last_send: None,
            retries: 0,
            current: 0,
            acked: VecDeque::from([0]),
            snapshot_due: None,
            ack_due: None,
            last_heard: None,
            started: now,
        }
    }

    pub fn push(&mut self, ev: UserEvent) {
        let unsent = self.upto() > self.sent.back().unwrap().upto;
        if let (UserEvent::Keys(k), Some(UserEvent::Keys(prev))) = (&ev, self.log.back_mut())
            && unsent
        {
            // Not sent yet: one keystroke event instead of many.
            prev.extend_from_slice(k);
            return;
        }
        self.log.push_back(ev);
    }

    fn upto(&self) -> u64 {
        self.log_base + self.log.len() as u64
    }

    fn rto(&self) -> Duration {
        (MIN_RTO * 2u32.pow(self.retries.min(4))).min(MAX_RTO)
    }

    /// A host state just acknowledged: the caller snapshots its screen (the newest) now.
    pub fn take_snapshot_due(&mut self) -> Option<u64> {
        self.snapshot_due.take()
    }

    /// Whether the host has been heard from since the session started.
    pub fn heard(&self) -> bool {
        self.last_heard.is_some()
    }

    /// How long since the host was last heard from (or since the start, before that).
    pub fn silence(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_heard.unwrap_or(self.started))
    }

    /// Something goes out at the next heartbeat check, whatever else is pending.
    pub fn force_send(&mut self) {
        self.last_send = None;
    }

    /// When [`Engine::poll`] has something to send next.
    pub fn next_deadline(&self, now: Instant) -> Instant {
        let Some(last) = self.last_send else {
            return now;
        };
        if self.upto() > self.sent.back().unwrap().upto {
            return now;
        }
        let mut at = last + HEARTBEAT;
        if self.unacked_input() {
            at = at.min(last + self.rto());
        }
        if let Some(due) = self.ack_due {
            at = at.min(due);
        }
        at.max(now)
    }

    fn unacked_input(&self) -> bool {
        self.sent.back().unwrap().upto > self.sent.front().unwrap().upto
    }

    /// The instruction to send now, if any.
    pub fn poll(&mut self, now: Instant) -> Option<Instruction> {
        let latest = self.sent.back().unwrap();
        let new_input = self.upto() > latest.upto;
        let since = self.last_send.map(|t| now.saturating_duration_since(t));
        let retransmit = self.unacked_input() && since.is_some_and(|s| s >= self.rto());
        let ack = self.ack_due.is_some_and(|due| now >= due);
        let heartbeat = since.is_none_or(|s| s >= HEARTBEAT);
        if !(new_input || retransmit || ack || heartbeat) {
            return None;
        }
        if retransmit && !new_input {
            self.retries += 1;
        }
        let num = latest.num + 1;
        let upto = self.upto();
        self.sent.push_back(Sent { num, upto });
        if self.sent.len() > MAX_SENT {
            // Keep the acknowledged state and the newest; the host acks one of those.
            let newest = self.sent.pop_back().unwrap();
            self.sent.truncate(1);
            self.sent.push_back(newest);
        }
        let base = self.sent.front().unwrap();
        let skip = (base.upto - self.log_base) as usize;
        let events: Vec<UserEvent> = self.log.iter().skip(skip).cloned().collect();
        let base_num = base.num;
        self.last_send = Some(now);
        self.ack_due = None;
        if self.acked.back() != Some(&self.current) {
            self.acked.push_back(self.current);
            self.snapshot_due = Some(self.current);
            while self.acked.len() > MAX_ACKED {
                self.acked.pop_front();
            }
        }
        Some(Instruction {
            protocol_version: wire::PROTOCOL_VERSION,
            old_num: base_num,
            new_num: num,
            ack_num: self.current,
            throwaway_num: base_num,
            diff: if events.is_empty() {
                Vec::new()
            } else {
                wire::encode_user(&events)
            },
        })
    }

    /// Takes in an instruction from the host; returns the diff to apply when it brings a new
    /// newest state.
    pub fn receive(&mut self, now: Instant, inst: Instruction) -> Result<Option<Apply>, Error> {
        if inst.protocol_version != wire::PROTOCOL_VERSION {
            return Err(Error::Protocol(format!(
                "the server speaks protocol version {}, not {}",
                inst.protocol_version,
                wire::PROTOCOL_VERSION
            )));
        }
        self.last_heard = Some(now);
        if let Some(pos) = self.sent.iter().position(|s| s.num == inst.ack_num)
            && pos > 0
        {
            self.sent.drain(..pos);
            let acked = self.sent.front().unwrap().upto;
            let drop = (acked - self.log_base) as usize;
            self.log.drain(..drop);
            self.log_base = acked;
            self.retries = 0;
        }
        if inst.new_num <= self.current
            || (inst.old_num != self.current && !self.acked.contains(&inst.old_num))
        {
            // Already shown, older than what is shown, or based on a state without a snapshot:
            // the host sends it again from one we acknowledged.
            return Ok(None);
        }
        self.acked
            .retain(|&n| n >= inst.throwaway_num || n == inst.old_num);
        self.current = inst.new_num;
        if !inst.diff.is_empty() {
            let due = now + ACK_DELAY;
            self.ack_due = Some(self.ack_due.map_or(due, |d| d.min(due)));
        }
        Ok(Some(Apply {
            old_num: inst.old_num,
            new_num: inst.new_num,
            diff: inst.diff,
            oldest: self.acked.front().copied().unwrap_or(self.current),
        }))
    }
}
