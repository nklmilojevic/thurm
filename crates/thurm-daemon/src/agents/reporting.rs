//! Ordered reports from an agent process and detection evidence.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use thurm_config::AgentDef;
use thurm_proto::{AgentExplanation, AgentReport, AgentStatus, PaneId, ProcessInfo};

use super::AgentTracker;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ReportOwner {
    pid: u32,
    birth: u64,
    pgrp: u32,
    instance: String,
    agent: String,
    sequence: u64,
    released: bool,
}

/// Public report state that must survive an in-place daemon upgrade.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReportHandoff {
    owner: ReportOwner,
    retired: std::collections::HashSet<String>,
    report: Option<AgentReport>,
    name: String,
    turns: u32,
    turn_ms: Option<u64>,
    turn_elapsed_ms: Option<u64>,
}

pub fn validate_resume(argv: &[String]) -> Result<(), String> {
    if argv.is_empty() || argv[0].is_empty() || argv.len() > 128 {
        return Err("resume arguments need an executable and at most 128 entries".into());
    }
    if argv.iter().map(String::len).sum::<usize>() > 32768
        || argv.iter().any(|a| a.chars().any(char::is_control))
    {
        return Err("resume arguments exceed 32768 bytes or contain control characters".into());
    }
    Ok(())
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

impl AgentTracker {
    /// Remove a report when its owner exits, even if its process group stays alive.
    pub fn invalidate_dead_report_owner(&mut self) -> bool {
        let birth = self
            .report_owner
            .as_ref()
            .and_then(|o| crate::procinfo::process_birth(o.pid));
        self.check_report_birth(birth)
    }

    fn check_report_birth(&mut self, birth: Option<u64>) -> bool {
        let Some(owner) = self.report_owner.as_mut() else {
            return false;
        };
        if owner.released || birth == Some(owner.birth) {
            return false;
        }
        owner.released = true;
        self.report_resume = None;
        let owns_hook = self
            .hook
            .as_ref()
            .is_some_and(|h| h.kind == owner.agent && h.pgrp == Some(owner.pgrp));
        if owns_hook {
            self.apply_hook(None, "", "", "session-end", None, None);
            self.set(None);
        }
        true
    }

    pub fn report_handoff(&self) -> Option<ReportHandoff> {
        let owner = self.report_owner.as_ref()?;
        let hook = self
            .hook
            .as_ref()
            .filter(|h| h.kind == owner.agent && h.pgrp == Some(owner.pgrp));
        Some(ReportHandoff {
            owner: owner.clone(),
            retired: self.retired_report_instances.clone(),
            report: hook.map(|h| AgentReport {
                agent: owner.agent.clone(),
                owner_pid: owner.pid,
                instance: owner.instance.clone(),
                sequence: owner.sequence,
                status: h.status,
                session_id: h.session_id.clone(),
                message: h.message.clone(),
                resume_argv: self.report_resume.clone(),
            }),
            name: hook.map_or_else(|| owner.agent.clone(), |h| h.name.clone()),
            turns: hook.map_or(0, |h| h.turns),
            turn_ms: hook.and_then(|h| h.turn_ms),
            turn_elapsed_ms: hook
                .and_then(|h| h.turn_started)
                .map(|at| at.elapsed().as_millis().min(u64::MAX as u128) as u64),
        })
    }

    pub fn restore_report_handoff(&mut self, handoff: ReportHandoff) {
        self.retired_report_instances = handoff.retired;
        self.report_owner = Some(handoff.owner);
        if self.invalidate_dead_report_owner()
            || !self.report_owner.as_ref().is_some_and(|o| !o.released)
        {
            return;
        }
        if let Some(report) = handoff.report {
            let pgrp = self.report_owner.as_ref().map(|o| o.pgrp);
            self.apply_hook(
                pgrp,
                &report.agent,
                &handoff.name,
                "session-start",
                report.session_id,
                None,
            );
            if let Some(h) = &mut self.hook {
                h.status = report.status;
                h.message = report.message;
                h.turns = handoff.turns;
                h.turn_ms = handoff.turn_ms;
                h.turn_started = handoff
                    .turn_elapsed_ms
                    .and_then(|ms| Instant::now().checked_sub(Duration::from_millis(ms)));
            }
            self.report_resume = report
                .resume_argv
                .filter(|argv| validate_resume(argv).is_ok());
            self.refresh();
        }
    }

    pub fn public_report_active(&self) -> bool {
        self.report_owner
            .as_ref()
            .is_some_and(|o| !o.released && Some(o.pgrp) == self.fg_pgrp)
    }

    /// Validate ownership before changing any state. The daemon reads the process birth.
    pub fn apply_report(
        &mut self,
        report: &AgentReport,
        birth: u64,
        pgrp: u32,
        name: &str,
    ) -> Result<(), String> {
        if report.owner_pid == 0 || Some(pgrp) != self.fg_pgrp {
            return Err("report owner is not in the pane's foreground process group".into());
        }
        if !identifier(&report.agent) || !identifier(&report.instance) || report.sequence == 0 {
            return Err("agent and instance must be valid IDs; sequence must start at one".into());
        }
        if report.session_id.as_ref().is_some_and(|s| !identifier(s))
            || report.message.as_ref().is_some_and(|s| s.len() > 8192)
        {
            return Err("invalid session ID or message exceeds 8192 bytes".into());
        }
        if let Some(argv) = &report.resume_argv {
            validate_resume(argv)?;
            if report.session_id.is_none() {
                return Err("resume arguments require a session ID".into());
            }
        }
        if self.retired_report_instances.contains(&report.instance) {
            return Err("this report instance was replaced".into());
        }
        let same = self.report_owner.as_ref().is_some_and(|o| {
            o.pid == report.owner_pid && o.birth == birth && o.instance == report.instance
        });
        if let Some(owner) = &self.report_owner {
            if same {
                if owner.released || report.sequence <= owner.sequence {
                    return Err("report was released or its sequence is stale".into());
                }
                if owner.agent != report.agent {
                    return Err("agent kind cannot change within a process instance".into());
                }
            } else if owner.instance == report.instance
                || (owner.pid == report.owner_pid && owner.birth == birth)
                || (!owner.released && owner.pgrp == pgrp)
            {
                return Err("another process instance owns reporting for this pane".into());
            }
        }
        if !same {
            if self.retired_report_instances.len() >= 4096 {
                return Err("report instance limit reached for this pane".into());
            }
            if let Some(owner) = &self.report_owner {
                self.retired_report_instances.insert(owner.instance.clone());
            }
            self.hook = None;
            self.set(None);
            self.report_resume = None;
            self.apply_hook(Some(pgrp), &report.agent, name, "session-start", None, None);
        }
        let old_session = self.session_id().map(str::to_owned);
        if report.session_id.is_some() && report.session_id != old_session {
            self.report_resume = None;
            if same {
                self.hook = None;
                self.set(None);
                self.apply_hook(Some(pgrp), &report.agent, name, "session-start", None, None);
            }
        }
        let event = match report.status {
            AgentStatus::Working
                if self
                    .hook
                    .as_ref()
                    .is_some_and(|h| h.status == AgentStatus::NeedsInput) =>
            {
                "tool-complete"
            }
            AgentStatus::Working
                if self
                    .hook
                    .as_ref()
                    .is_none_or(|h| h.status != AgentStatus::Working) =>
            {
                "prompt-submit"
            }
            AgentStatus::NeedsInput => "notification",
            AgentStatus::Done
                if self
                    .hook
                    .as_ref()
                    .is_none_or(|h| h.status != AgentStatus::Done) =>
            {
                "stop"
            }
            _ => "report",
        };
        self.apply_hook(
            Some(pgrp),
            &report.agent,
            name,
            event,
            report.session_id.clone(),
            report.message.clone(),
        );
        if let Some(hook) = &mut self.hook {
            hook.status = report.status;
            hook.message = report.message.clone();
        }
        if let Some(argv) = &report.resume_argv {
            self.report_resume = Some(argv.clone());
        }
        self.report_owner = Some(ReportOwner {
            pid: report.owner_pid,
            birth,
            pgrp,
            instance: report.instance.clone(),
            agent: report.agent.clone(),
            sequence: report.sequence,
            released: false,
        });
        Ok(())
    }

    pub fn release_report(
        &mut self,
        pid: u32,
        birth: u64,
        instance: &str,
        sequence: u64,
    ) -> Result<(), String> {
        let owner = self
            .report_owner
            .as_mut()
            .ok_or("no process owns reporting for this pane")?;
        if owner.pid != pid
            || owner.birth != birth
            || owner.instance != instance
            || Some(owner.pgrp) != self.fg_pgrp
        {
            return Err("release does not match the report owner".into());
        }
        if owner.released || sequence <= owner.sequence {
            return Err("release was already applied or its sequence is stale".into());
        }
        owner.sequence = sequence;
        owner.released = true;
        self.apply_hook(None, "", "", "session-end", None, None);
        self.report_resume = None;
        self.set(None);
        Ok(())
    }

    pub fn resume_argv(&self) -> Option<&[String]> {
        self.public_report_active()
            .then_some(self.report_resume.as_deref())
            .flatten()
    }

    pub fn explain(
        &self,
        pane: PaneId,
        defs: &[AgentDef],
        foreground: Option<ProcessInfo>,
        screen: &str,
        idle: Duration,
        idle_after: Duration,
    ) -> AgentExplanation {
        let def = foreground
            .as_ref()
            .and_then(|p| defs.iter().find(|d| d.matches(&p.name, &p.argv)));
        let active_hook =
            self.hook_in_foreground() && self.current.as_ref().is_some_and(|s| s.hooked);
        let mut rules = Vec::new();
        if let (Some(def), Some(process)) = (def, foreground.as_ref()) {
            let basename = process.name.rsplit('/').next().unwrap_or(&process.name);
            for name in &def.processes {
                if name == basename
                    || process
                        .argv
                        .first()
                        .is_some_and(|a| a.rsplit('/').next() == Some(name))
                {
                    rules.push(format!("{}.processes: {name}", def.kind));
                }
            }
            let interpreter = matches!(
                basename,
                "node" | "bun" | "deno" | "python" | "python3" | "uv" | "npx"
            ) || basename.starts_with("python");
            for pattern in &def.argv {
                if interpreter && process.argv.iter().skip(1).any(|a| a.contains(pattern)) {
                    rules.push(format!("{}.argv: {pattern}", def.kind));
                }
            }
            let activity = super::current_activity(&def.kind, screen);
            for pattern in &def.attention {
                if activity.contains(pattern) {
                    rules.push(format!("{}.attention: {pattern}", def.kind));
                }
            }
            for pattern in &def.working {
                if activity.contains(pattern) {
                    rules.push(format!("{}.working: {pattern}", def.kind));
                }
            }
        }
        if self.attention_flag {
            rules.push("program notification requested attention".into());
        }
        let source = if self.current.is_none() {
            "none"
        } else if active_hook && self.public_report_active() {
            "report"
        } else if active_hook {
            "hook"
        } else if self.attention_flag {
            "notification"
        } else if idle >= idle_after
            && def.is_none_or(|d| {
                let activity = super::current_activity(&d.kind, screen);
                !d.attention
                    .iter()
                    .chain(&d.working)
                    .any(|p| activity.contains(p))
            })
            && self.ai.waiting.is_some_and(|(hash, waiting)| {
                waiting
                    && hash
                        == super::screen_hash(
                            def.map_or(screen, |d| super::current_activity(&d.kind, screen)),
                        )
            })
            && self
                .current
                .as_ref()
                .is_some_and(|s| s.status == AgentStatus::NeedsInput)
        {
            "ai"
        } else {
            "screen-and-activity"
        };
        if source == "hook" || source == "report" {
            rules.push("foreground hook state overrides screen and output activity".into());
        } else {
            rules.push(
                if idle < idle_after {
                    "output is inside the idle interval"
                } else {
                    "output is outside the idle interval"
                }
                .into(),
            );
        }
        let owner = self
            .report_owner
            .as_ref()
            .filter(|_| self.public_report_active());
        AgentExplanation {
            pane,
            state: self.current.clone(),
            foreground,
            source: source.into(),
            rules,
            screen_tail: screen.chars().take(4096).collect(),
            idle_ms: idle.as_millis().min(u64::MAX as u128) as u64,
            idle_after_ms: idle_after.as_millis().min(u64::MAX as u128) as u64,
            report_instance: owner.map(|o| o.instance.clone()),
            report_sequence: owner.map(|o| o.sequence),
            report_owner_pid: owner.map(|o| o.pid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> AgentReport {
        AgentReport {
            agent: "example".into(),
            owner_pid: 42,
            instance: "instance-a".into(),
            sequence: 1,
            status: AgentStatus::Idle,
            session_id: Some("session-a".into()),
            message: None,
            resume_argv: Some(vec![
                "example".into(),
                "--resume".into(),
                "session-a".into(),
            ]),
        }
    }

    fn tracker() -> AgentTracker {
        let mut tracker = AgentTracker::default();
        tracker.saw_foreground(Some(42));
        tracker
    }

    #[test]
    fn reports_reject_old_sequences_without_changes() {
        let mut tracker = tracker();
        let mut report = report();
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        tracker.refresh();
        report.sequence = 3;
        report.status = AgentStatus::Working;
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        tracker.refresh();
        for sequence in [0, 1, 2, 3] {
            report.sequence = sequence;
            report.status = AgentStatus::Done;
            assert!(tracker.apply_report(&report, 100, 42, "Example").is_err());
            assert_eq!(tracker.state().unwrap().status, AgentStatus::Working);
        }
    }

    #[test]
    fn ownership_is_bound_to_process_instance_kind_and_foreground() {
        let mut tracker = tracker();
        let report = report();
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        let mut other = report.clone();
        other.sequence = 2;
        other.instance = "instance-b".into();
        assert!(tracker.apply_report(&other, 100, 42, "Example").is_err());
        other = report.clone();
        other.sequence = 2;
        other.agent = "other".into();
        assert!(tracker.apply_report(&other, 100, 42, "Other").is_err());
        assert!(tracker.apply_report(&report, 101, 42, "Example").is_err());
        tracker.saw_foreground(Some(99));
        assert!(tracker.apply_report(&report, 100, 42, "Example").is_err());
    }

    #[test]
    fn released_instance_cannot_report_again() {
        let mut tracker = tracker();
        let mut report = report();
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        tracker.refresh();
        assert!(tracker.release_report(42, 100, "wrong", 2).is_err());
        assert!(
            tracker
                .release_report(42, 101, &report.instance, 2)
                .is_err()
        );
        assert!(
            tracker
                .release_report(42, 100, &report.instance, 1)
                .is_err()
        );
        tracker
            .release_report(42, 100, &report.instance, 2)
            .unwrap();
        assert!(tracker.state().is_none());
        assert!(tracker.resume_argv().is_none());
        report.sequence = 3;
        assert!(tracker.apply_report(&report, 100, 42, "Example").is_err());
    }

    #[test]
    fn replacement_removes_previous_session_and_rejects_retired_instance() {
        let mut tracker = tracker();
        let first = report();
        tracker.apply_report(&first, 100, 42, "Example").unwrap();
        tracker.refresh();
        tracker.saw_foreground(Some(99));
        let mut next = report();
        next.owner_pid = 99;
        next.instance = "instance-b".into();
        next.agent = "other".into();
        next.resume_argv = None;
        next.session_id = None;
        tracker.apply_report(&next, 200, 99, "Other").unwrap();
        tracker.refresh();
        assert_eq!(tracker.state().unwrap().kind, "other");
        assert!(tracker.session_id().is_none());
        assert!(tracker.resume_argv().is_none());
        tracker.saw_foreground(Some(42));
        assert!(tracker.apply_report(&first, 100, 42, "Example").is_err());
    }

    #[test]
    fn session_change_discards_previous_resume_arguments() {
        let mut tracker = tracker();
        let mut report = report();
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        assert_eq!(tracker.resume_argv(), report.resume_argv.as_deref());
        report.sequence = 2;
        report.session_id = Some("session-b".into());
        report.resume_argv = None;
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        assert!(tracker.resume_argv().is_none());
        assert_eq!(tracker.session_id(), Some("session-b"));
    }

    #[test]
    fn bad_resume_arguments_do_not_claim_reporting() {
        let mut tracker = tracker();
        let mut report = report();
        for argv in [
            vec![],
            vec!["".into()],
            vec!["example\nother".into()],
            vec!["example".into(), "a\0b".into()],
        ] {
            report.resume_argv = Some(argv);
            assert!(tracker.apply_report(&report, 100, 42, "Example").is_err());
            assert!(!tracker.public_report_active());
        }
        let argv = vec!["example".into(), "$(touch /tmp/never-run); 'quoted'".into()];
        assert!(validate_resume(&argv).is_ok());
    }

    #[test]
    fn repeated_done_reports_do_not_add_finished_turns() {
        let mut tracker = tracker();
        let mut report = report();
        report.status = AgentStatus::Working;
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        report.sequence = 2;
        report.status = AgentStatus::Done;
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        tracker.refresh();
        assert_eq!(tracker.state().unwrap().turns, 1);
        report.sequence = 3;
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        tracker.refresh();
        assert_eq!(tracker.state().unwrap().turns, 1);
    }

    #[test]
    fn approval_continuation_keeps_the_same_turn() {
        let mut tracker = tracker();
        let mut report = report();
        report.status = AgentStatus::Working;
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        let started = tracker.hook.as_ref().unwrap().turn_started;
        report.sequence = 2;
        report.status = AgentStatus::NeedsInput;
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        report.sequence = 3;
        report.status = AgentStatus::Working;
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        assert_eq!(tracker.hook.as_ref().unwrap().turn_started, started);
        assert_eq!(tracker.hook.as_ref().unwrap().turns, 0);
    }

    #[test]
    fn reported_permission_continuation_completes_the_reserved_prompt() {
        let mut tracker = tracker();
        let mut report = report();
        tracker.apply_report(&report, 100, 42, "Example").unwrap();
        tracker.refresh();
        let token = tracker.prompt.reserve(AgentStatus::Idle).unwrap();
        for status in [
            AgentStatus::Working,
            AgentStatus::NeedsInput,
            AgentStatus::Working,
            AgentStatus::Done,
        ] {
            report.sequence += 1;
            report.status = status;
            tracker.apply_report(&report, 100, 42, "Example").unwrap();
            tracker.refresh();
        }
        assert_eq!(
            tracker.prompt.outcome(token, AgentStatus::Done).unwrap(),
            Some(thurm_proto::AgentPromptOutcome::Completed)
        );
    }

    #[test]
    fn dead_owner_cannot_keep_a_live_process_group() {
        for birth in [None, Some(101)] {
            let mut tracker = tracker();
            let report = report();
            tracker.apply_report(&report, 100, 42, "Example").unwrap();
            tracker.refresh();
            assert!(tracker.check_report_birth(birth));
            assert!(tracker.state().is_none());
            assert!(tracker.resume_argv().is_none());
            assert!(!tracker.public_report_active());
            let mut next = report.clone();
            next.owner_pid = 43;
            next.instance = "instance-b".into();
            tracker.apply_report(&next, 102, 42, "Example").unwrap();
            assert!(tracker.apply_report(&report, 100, 42, "Example").is_err());
        }
    }

    #[test]
    fn explanation_has_actual_process_rules_and_report_source() {
        let mut tracker = tracker();
        let defs = thurm_config::builtin_agents();
        let fg = ProcessInfo {
            pid: 42,
            name: "claude".into(),
            argv: vec!["claude".into()],
        };
        let screen = "Would you like to proceed?";
        tracker.update(
            &defs,
            Some(&fg),
            screen,
            Duration::from_secs(10),
            Duration::from_secs(2),
        );
        let explanation = tracker.explain(
            7,
            &defs,
            Some(fg.clone()),
            screen,
            Duration::from_secs(10),
            Duration::from_secs(2),
        );
        assert_eq!(explanation.source, "screen-and-activity");
        assert!(
            explanation
                .rules
                .contains(&"claude.processes: claude".into())
        );
        assert!(
            explanation
                .rules
                .contains(&"claude.attention: Would you like to proceed?".into())
        );
        assert_eq!(explanation.state.unwrap().status, AgentStatus::NeedsInput);
        let mut report = report();
        report.agent = "claude".into();
        tracker
            .apply_report(&report, 100, 42, "Claude Code")
            .unwrap();
        tracker.refresh();
        let explanation = tracker.explain(
            7,
            &defs,
            Some(fg),
            screen,
            Duration::ZERO,
            Duration::from_secs(2),
        );
        assert_eq!(explanation.source, "report");
        assert_eq!(explanation.report_sequence, Some(1));
        assert_eq!(explanation.state.unwrap().status, AgentStatus::Idle);
    }
}
