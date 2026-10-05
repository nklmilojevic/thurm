//! Keep duplicate prompt protection when the daemon replaces its process image.

use serde::{Deserialize, Serialize};
use thurm_proto::AgentState;

use super::AgentTracker;

#[derive(Serialize, Deserialize, Debug)]
pub struct PendingPromptHandoff {
    agent: AgentState,
    pgrp: u32,
    birth: u64,
    reported: bool,
}

impl AgentTracker {
    pub fn pending_prompt_handoff(&self) -> Option<PendingPromptHandoff> {
        if !self.prompt.is_pending() {
            return None;
        }
        let pgrp = self.fg_pgrp?;
        Some(PendingPromptHandoff {
            agent: self.current.clone()?,
            pgrp,
            birth: crate::procinfo::process_birth(pgrp)?,
            reported: self.public_report_active(),
        })
    }

    pub fn restore_pending_prompt(&mut self, saved: PendingPromptHandoff) {
        if self.fg_pgrp != Some(saved.pgrp)
            || crate::procinfo::process_birth(saved.pgrp) != Some(saved.birth)
        {
            return;
        }
        if saved.reported && !self.public_report_active() {
            return;
        }
        // Public reporting state is restored first. A legacy hook also needs its identity
        // restored so the first monitor tick does not clear the pending prompt.
        if saved.agent.hooked && !self.public_report_active() {
            self.apply_hook(
                Some(saved.pgrp),
                &saved.agent.kind,
                &saved.agent.name,
                "session-start",
                saved.agent.session_id.clone(),
                None,
            );
            if let Some(hook) = &mut self.hook {
                hook.status = saved.agent.status;
                hook.turns = saved.agent.turns;
                hook.turn_ms = saved.agent.turn_ms;
            }
        }
        if !self.public_report_active() {
            self.set(Some(saved.agent));
        }
        self.prompt.restore_pending();
    }
}
