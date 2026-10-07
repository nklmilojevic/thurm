//! Keep a hooked agent's session across an in-place daemon upgrade.
//!
//! The session id only arrives with hook events. Without this, an agent that stays idle after
//! an upgrade has none until its next hook, and a restore in that window falls back to "the
//! last session here" (`claude --continue`), which is another pane's when they share a
//! directory.

use serde::{Deserialize, Serialize};
use thurm_proto::AgentStatus;

use super::{AgentTracker, HookState};
use crate::transcript::TitleReader;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HookHandoff {
    kind: String,
    name: String,
    status: AgentStatus,
    session_id: Option<String>,
    message: Option<String>,
    turn_ms: Option<u64>,
    turns: u32,
    transcript: Option<String>,
    pgrp: u32,
    birth: u64,
}

impl AgentTracker {
    pub fn hook_handoff(&self) -> Option<HookHandoff> {
        let h = self.hook.as_ref()?;
        let pgrp = h.pgrp?;
        Some(HookHandoff {
            kind: h.kind.clone(),
            name: h.name.clone(),
            status: h.status,
            session_id: h.session_id.clone(),
            message: h.message.clone(),
            turn_ms: h.turn_ms,
            turns: h.turns,
            transcript: h
                .transcript
                .as_ref()
                .and_then(|t| t.path().to_str())
                .map(str::to_owned),
            pgrp,
            birth: crate::procinfo::process_birth(pgrp)?,
        })
    }

    /// Only while the same agent process still runs: a reused PID is someone else.
    pub fn restore_hook_handoff(&mut self, saved: HookHandoff) {
        if crate::procinfo::process_birth(saved.pgrp) != Some(saved.birth) {
            return;
        }
        let transcript = saved.transcript.map(|path| {
            let mut t = TitleReader::new(path);
            t.poll();
            // Interrupts before the upgrade were already applied to the status.
            t.take_interrupted();
            t
        });
        self.hook = Some(HookState {
            kind: saved.kind,
            name: saved.name,
            status: saved.status,
            session_id: saved.session_id,
            message: saved.message,
            turn_started: None,
            turn_ms: saved.turn_ms,
            turns: saved.turns,
            transcript,
            answered: false,
            pgrp: Some(saved.pgrp),
            permission: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own_pgrp() -> u32 {
        unsafe { libc::getpgrp() as u32 }
    }

    #[test]
    fn session_survives_the_handoff() {
        let pgrp = own_pgrp();
        let mut old = AgentTracker::default();
        old.apply_hook(
            Some(pgrp),
            "claude",
            "Claude Code",
            "session-start",
            Some("abc-123".into()),
            None,
        );
        old.apply_hook(Some(pgrp), "claude", "Claude Code", "stop", None, None);
        let saved = old.hook_handoff().expect("handoff");
        let saved: HookHandoff =
            serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();

        let mut new = AgentTracker::default();
        new.restore_hook_handoff(saved);
        assert_eq!(new.session_id(), Some("abc-123"));
        assert_eq!(new.hook_handoff(), old.hook_handoff());
    }

    #[test]
    fn a_reused_pid_gets_nothing() {
        let pgrp = own_pgrp();
        let mut old = AgentTracker::default();
        old.apply_hook(
            Some(pgrp),
            "claude",
            "Claude Code",
            "session-start",
            Some("abc-123".into()),
            None,
        );
        let mut saved = old.hook_handoff().expect("handoff");
        saved.birth = saved.birth.wrapping_add(1);

        let mut new = AgentTracker::default();
        new.restore_hook_handoff(saved);
        assert_eq!(new.session_id(), None);
    }

    #[test]
    fn no_agent_process_no_handoff() {
        let mut t = AgentTracker::default();
        t.apply_hook(
            None,
            "claude",
            "Claude Code",
            "session-start",
            Some("abc".into()),
            None,
        );
        assert!(t.hook_handoff().is_none());
    }
}
