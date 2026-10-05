//! State for prompt submission and completion checks.

use thurm_proto::{AgentPromptOutcome, AgentState, AgentStatus};

#[derive(Default, Debug)]
pub struct PromptTracker {
    identity: Option<(Option<u32>, String, Option<String>)>,
    instance: u64,
    activity: u64,
    working: bool,
    pending: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct PromptToken {
    instance: u64,
    activity: u64,
}

impl PromptTracker {
    pub fn reset(&mut self) {
        self.instance = self.instance.wrapping_add(1);
        self.pending = false;
        self.working = false;
    }

    pub fn observe(&mut self, pgrp: Option<u32>, state: Option<&AgentState>) {
        let identity = state.map(|s| (pgrp, s.kind.clone(), s.session_id.clone()));
        if identity != self.identity {
            self.reset();
            self.identity = identity;
        }
        let working = state.is_some_and(|s| s.status == AgentStatus::Working);
        if working && !self.working && state.is_some_and(|s| !s.hooked) {
            self.started();
        }
        self.working = working;
    }

    pub fn interrupted(&mut self) {
        self.instance = self.instance.wrapping_add(1);
    }

    pub fn started(&mut self) {
        self.activity = self.activity.wrapping_add(1);
        self.pending = false;
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }

    pub fn reserve(&mut self, status: AgentStatus) -> Result<PromptToken, String> {
        if self.is_pending() {
            return Err("an agent prompt is already pending".into());
        }
        match status {
            AgentStatus::Working => return Err("the agent is working".into()),
            AgentStatus::NeedsInput => {
                return Err("the agent needs input; answer its request first".into());
            }
            AgentStatus::Idle | AgentStatus::Done => {}
        }
        self.pending = true;
        Ok(PromptToken {
            instance: self.instance,
            activity: self.activity,
        })
    }

    pub fn outcome(
        &self,
        token: PromptToken,
        status: AgentStatus,
    ) -> Result<Option<AgentPromptOutcome>, String> {
        if self.instance != token.instance {
            return Err("the agent instance changed after prompt submission".into());
        }
        let turns = self.activity.wrapping_sub(token.activity);
        if turns > 1 {
            return Err("another agent turn started before the wait completed".into());
        }
        if status == AgentStatus::NeedsInput {
            return Ok(Some(AgentPromptOutcome::NeedsInput));
        }
        if turns == 1 && matches!(status, AgentStatus::Idle | AgentStatus::Done) {
            return Ok(Some(AgentPromptOutcome::Completed));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_proto::AgentState;

    fn agent(status: AgentStatus, session: &str) -> AgentState {
        AgentState {
            name: "Test".into(),
            kind: "test".into(),
            status,
            session_id: Some(session.into()),
            message: None,
            turn_ms: None,
            turns: 0,
            hooked: true,
            topic: None,
            permission: None,
        }
    }

    #[test]
    fn stale_completion_and_concurrent_submission_cannot_pass() {
        let mut tracker = PromptTracker::default();
        tracker.observe(Some(42), Some(&agent(AgentStatus::Done, "one")));
        let token = tracker.reserve(AgentStatus::Done).unwrap();
        assert!(tracker.reserve(AgentStatus::Done).is_err());
        assert_eq!(tracker.outcome(token, AgentStatus::Done), Ok(None));
        assert_eq!(tracker.outcome(token, AgentStatus::Idle), Ok(None));
        tracker.started();
        assert_eq!(
            tracker.outcome(token, AgentStatus::Done),
            Ok(Some(AgentPromptOutcome::Completed))
        );
    }

    #[test]
    fn replacement_process_or_session_cannot_complete_old_wait() {
        for (pid, session) in [(43, "one"), (42, "two")] {
            let mut tracker = PromptTracker::default();
            tracker.observe(Some(42), Some(&agent(AgentStatus::Idle, "one")));
            let token = tracker.reserve(AgentStatus::Idle).unwrap();
            tracker.observe(Some(pid), Some(&agent(AgentStatus::Done, session)));
            tracker.started();
            assert!(tracker.outcome(token, AgentStatus::Done).is_err());
        }
    }

    #[test]
    fn blocked_agents_cannot_accept_prompts() {
        for status in [AgentStatus::NeedsInput, AgentStatus::Working] {
            assert!(PromptTracker::default().reserve(status).is_err());
        }
    }

    #[test]
    fn other_input_invalidates_wait_without_allowing_duplicate_submission() {
        let mut tracker = PromptTracker::default();
        let token = tracker.reserve(AgentStatus::Idle).unwrap();
        tracker.interrupted();
        assert!(tracker.outcome(token, AgentStatus::Done).is_err());
        assert!(tracker.is_pending());
    }

    #[test]
    fn extra_turn_cannot_complete_old_wait() {
        let mut tracker = PromptTracker::default();
        let token = tracker.reserve(AgentStatus::Idle).unwrap();
        tracker.started();
        tracker.started();
        assert!(tracker.outcome(token, AgentStatus::Done).is_err());
    }
}
