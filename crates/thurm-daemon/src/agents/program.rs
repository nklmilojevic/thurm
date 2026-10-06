//! What programs say about themselves (OSC 7501, `thurm_term::program_status`). A program
//! that reports its state knows it best: the report wins over hooks and the screen, and a
//! program no agent definition knows (`brew`, `terraform`) shows up like an agent while it
//! has something to say.

use thurm_config::AgentDef;
use thurm_proto::{AgentState, AgentStatus, ProgramRecord, ProgramState, Progress, ProgressState};

use super::AgentTracker;

/// The record that speaks for the pane (see [`lead`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramLead {
    pub status: AgentStatus,
    pub kind: String,
    pub name: String,
    pub message: Option<String>,
    /// The root record's title.
    pub topic: Option<String>,
}

/// What news a record is: a request first, then a failure, running work, a result, rest.
fn rank(r: &ProgramRecord) -> Option<u8> {
    match r.state {
        _ if r.seen => None,
        ProgramState::Blocked => Some(4),
        ProgramState::Error => Some(3),
        ProgramState::Working => Some(2),
        ProgramState::Done => Some(1),
        ProgramState::Idle => Some(0),
    }
}

/// The record to show for the pane: the most pressing one, and of those the one updated last
/// (`records` is least recently updated first). Results the user has seen say nothing.
pub fn lead(records: &[ProgramRecord], defs: &[AgentDef]) -> Option<ProgramLead> {
    let (_, r) = records
        .iter()
        .enumerate()
        .filter_map(|(i, r)| Some(((rank(r)?, i), r)))
        .max_by_key(|(key, _)| *key)?;
    let app = r.app.as_deref();
    // `claude` or `claude-code` for "Claude Code".
    let def = app.and_then(|a| {
        defs.iter()
            .find(|d| d.kind == a || d.name.replace(' ', "-").eq_ignore_ascii_case(a))
    });
    let message = match (r.id.is_empty(), &r.title, &r.message) {
        (false, Some(t), Some(m)) => Some(format!("{t}: {m}")),
        (false, Some(t), None) => Some(t.clone()),
        (_, _, m) => m.clone(),
    };
    Some(ProgramLead {
        status: match r.state {
            ProgramState::Idle => AgentStatus::Idle,
            ProgramState::Working => AgentStatus::Working,
            ProgramState::Done => AgentStatus::Done,
            ProgramState::Blocked => AgentStatus::NeedsInput,
            ProgramState::Error => AgentStatus::Error,
        },
        kind: def.map_or_else(|| app.unwrap_or("program").to_owned(), |d| d.kind.clone()),
        name: def.map_or_else(|| app.unwrap_or("Program").to_owned(), |d| d.name.clone()),
        message,
        topic: records
            .iter()
            .find(|r| r.id.is_empty())
            .and_then(|r| r.title.clone()),
    })
}

/// Progress for the pane's progress bar: the most recently updated record that gave one.
/// A blocked program's progress is paused.
pub fn progress(records: &[ProgramRecord]) -> Option<Progress> {
    records.iter().rev().find_map(|r| {
        let state = match r.state {
            ProgramState::Working => ProgressState::Normal,
            ProgramState::Blocked => ProgressState::Paused,
            _ => return None,
        };
        r.progress.map(|p| Progress {
            state,
            percent: Some(p),
        })
    })
}

impl AgentTracker {
    /// The pane's program status records changed (or were acknowledged).
    pub fn set_programs(&mut self, records: &[ProgramRecord], defs: &[AgentDef]) {
        self.program = lead(records, defs);
    }

    /// Whether a program's own report decides the status now.
    pub fn program_reported(&self) -> bool {
        self.program.is_some()
    }

    /// A state for a program that reports but isn't otherwise known as an agent.
    pub(super) fn program_state(&self) -> Option<AgentState> {
        let p = self.program.as_ref()?;
        Some(AgentState {
            name: p.name.clone(),
            kind: p.kind.clone(),
            status: p.status,
            session_id: None,
            message: None,
            turn_ms: None,
            turns: 0,
            hooked: true,
            topic: None,
            permission: None,
        })
    }

    pub(super) fn with_program(&self, mut state: AgentState) -> AgentState {
        if let Some(p) = &self.program {
            state.status = p.status;
            state.message = p.message.clone();
            state.hooked = true;
            if p.status != AgentStatus::NeedsInput {
                state.permission = None;
            }
            if state.topic.is_none() {
                state.topic = p.topic.clone();
            }
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use thurm_proto::{BlockedKind, ProcessInfo};
    use thurm_term::program_status::{ProgramStatus, parse};

    fn rec(id: &str, state: ProgramState) -> ProgramRecord {
        ProgramRecord {
            id: id.into(),
            state,
            kind: None,
            progress: None,
            app: Some("deploy".into()),
            title: None,
            message: None,
            seen: false,
        }
    }

    #[test]
    fn the_most_pressing_record_leads() {
        let mut blocked = rec("eu-west", ProgramState::Blocked);
        blocked.kind = Some(BlockedKind::Permission);
        blocked.title = Some("EU West".into());
        blocked.message = Some("Approve deploy?".into());
        let records = vec![
            rec("", ProgramState::Working),
            blocked,
            rec("us-east", ProgramState::Done),
        ];
        let l = lead(&records, &[]).unwrap();
        assert_eq!(l.status, AgentStatus::NeedsInput);
        assert_eq!(l.message.as_deref(), Some("EU West: Approve deploy?"));
        assert_eq!((l.kind.as_str(), l.name.as_str()), ("deploy", "deploy"));
        // Equal news: the latest report.
        let mut a = rec("a", ProgramState::Working);
        a.message = Some("old".into());
        let mut b = rec("b", ProgramState::Working);
        b.message = Some("new".into());
        assert_eq!(lead(&[a, b], &[]).unwrap().message.as_deref(), Some("new"));
    }

    #[test]
    fn seen_results_say_nothing() {
        let mut done = rec("", ProgramState::Done);
        done.seen = true;
        assert_eq!(lead(std::slice::from_ref(&done), &[]), None);
        let idle = rec("x", ProgramState::Idle);
        assert_eq!(lead(&[done, idle], &[]).unwrap().status, AgentStatus::Idle);
    }

    #[test]
    fn known_apps_take_their_agent_name() {
        let defs = thurm_config::builtin_agents();
        for app in ["claude", "claude-code"] {
            let mut r = rec("", ProgramState::Error);
            r.app = Some(app.into());
            let l = lead(&[r], &defs).unwrap();
            assert_eq!((l.kind.as_str(), l.status), ("claude", AgentStatus::Error));
            assert_eq!(l.name, "Claude Code");
        }
        let mut r = rec("", ProgramState::Idle);
        r.app = None;
        assert_eq!(lead(&[r], &defs).unwrap().name, "Program");
    }

    #[test]
    fn progress_of_the_latest_record_that_has_one() {
        let mut a = rec("a", ProgramState::Working);
        a.progress = Some(10);
        let mut b = rec("b", ProgramState::Blocked);
        b.progress = Some(70);
        let c = rec("c", ProgramState::Working);
        assert_eq!(
            progress(&[a.clone(), b, c]),
            Some(Progress {
                state: ProgressState::Paused,
                percent: Some(70)
            })
        );
        assert_eq!(progress(&[a]).unwrap().percent, Some(10));
    }

    fn records(t: &mut ProgramStatus, reports: &[&str]) -> Vec<ProgramRecord> {
        for r in reports {
            t.apply(parse(r.as_bytes()).unwrap());
        }
        t.records()
    }

    #[test]
    fn a_reporting_program_shows_until_its_result_is_seen() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let mut s = ProgramStatus::default();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        let brew = ProcessInfo {
            pid: 7,
            name: "brew".into(),
            argv: vec!["brew".into()],
        };
        // "Password required to install updates"
        let recs = records(
            &mut s,
            &[
                "state=blocked:kind=auth:app=brew:msg=UGFzc3dvcmQgcmVxdWlyZWQgdG8gaW5zdGFsbCB1cGRhdGVz",
            ],
        );
        t.set_programs(&recs, &defs);
        let a = t.refresh().unwrap().unwrap();
        assert_eq!(
            (a.name.as_str(), a.status),
            ("brew", AgentStatus::NeedsInput)
        );
        assert_eq!(
            a.message.as_deref(),
            Some("Password required to install updates")
        );
        // Monitor ticks keep it, and the screen has no say.
        assert_eq!(
            t.update(&defs, Some(&brew), "", Duration::ZERO, idle_after),
            None
        );
        t.set_programs(&records(&mut s, &["state=done:app=brew"]), &defs);
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Done);
        // The result outlives the program and the prompt.
        s.prompt_started();
        t.set_programs(&s.records(), &defs);
        assert_eq!(t.refresh(), None);
        let shell = ProcessInfo {
            pid: 5,
            name: "fish".into(),
            argv: vec!["fish".into()],
        };
        assert_eq!(t.update(&defs, Some(&shell), "", quiet, idle_after), None);
        // Seen, it is gone.
        s.acknowledge();
        t.set_programs(&s.records(), &defs);
        assert_eq!(t.refresh(), Some(None));
        assert_eq!(t.update(&defs, Some(&shell), "", quiet, idle_after), None);
    }

    #[test]
    fn a_detected_agent_reporting_its_status_is_believed() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let mut s = ProgramStatus::default();
        let idle_after = Duration::from_millis(1500);
        let claude = ProcessInfo {
            pid: 9,
            name: "claude".into(),
            argv: vec!["claude".into()],
        };
        // Fresh output would read as working.
        let a = t
            .update(&defs, Some(&claude), "", Duration::ZERO, idle_after)
            .unwrap()
            .unwrap();
        assert_eq!(a.status, AgentStatus::Working);
        t.set_programs(&records(&mut s, &["state=idle:app=claude-code"]), &defs);
        let a = t.refresh().unwrap().unwrap();
        assert_eq!((a.kind.as_str(), a.status), ("claude", AgentStatus::Idle));
        assert_eq!(
            t.update(&defs, Some(&claude), "", Duration::ZERO, idle_after),
            None
        );
        t.set_programs(&records(&mut s, &["state=error:app=claude-code"]), &defs);
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Error);
        // Seen, the screen speaks again.
        s.acknowledge();
        t.set_programs(&s.records(), &defs);
        assert_eq!(
            t.update(&defs, Some(&claude), "", Duration::ZERO, idle_after)
                .unwrap()
                .unwrap()
                .status,
            AgentStatus::Working
        );
    }

    #[test]
    fn a_cleared_report_gives_the_detected_state_back() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let mut s = ProgramStatus::default();
        let idle_after = Duration::from_millis(1500);
        let claude = ProcessInfo {
            pid: 9,
            name: "claude".into(),
            argv: vec!["claude".into()],
        };
        t.update(&defs, Some(&claude), "", Duration::ZERO, idle_after);
        // "Approve?"
        t.set_programs(
            &records(&mut s, &["state=blocked:app=claude:msg=QXBwcm92ZT8="]),
            &defs,
        );
        let a = t.refresh().unwrap().unwrap();
        assert_eq!(
            (a.status, a.message.as_deref()),
            (AgentStatus::NeedsInput, Some("Approve?"))
        );
        t.set_programs(&records(&mut s, &["state=clear"]), &defs);
        let a = t.refresh().unwrap().unwrap();
        assert_eq!(
            (a.status, a.message, a.hooked),
            (AgentStatus::Working, None, false)
        );
    }
}
