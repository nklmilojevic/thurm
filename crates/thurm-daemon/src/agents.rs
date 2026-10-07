//! Coding-agent detection and status tracking.
//!
//! Agents run in plain PTYs; we look at the pane's foreground process to recognize them and
//! at the screen / output activity to infer whether they are working, idle or waiting for the
//! user. Programs' own notifications (OSC 9/99/777) also flag "needs input".
//!
//! Agents with hooks installed (`thurm hooks install`) report their state directly through
//! `thurm agent-hook`; while the hooked agent is in the foreground that state wins over the
//! screen heuristics, and adds a session id, a Done state and turn timing.
//!
//! A program that reports its own status (OSC 7501, see [`program`]) is believed over both.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thurm_config::AgentDef;
use thurm_proto::{AgentState, AgentStatus, ProcessInfo};

use crate::transcript::TitleReader;

pub mod hook_handoff;
pub mod program;
pub mod prompt_handoff;
pub mod reporting;

#[derive(Default, Debug)]
pub struct AgentTracker {
    current: Option<AgentState>,
    pub prompt: crate::agent_prompt::PromptTracker,
    /// Kind of the last agent seen (kept after it exits, used for resume on restore).
    pub last_kind: Option<String>,
    /// Set by a program notification; cleared when the user types.
    attention_flag: bool,
    since: Option<Instant>,
    hook: Option<HookState>,
    /// The pane's foreground process group, as last seen (monitor tick or hook event).
    fg_pgrp: Option<u32>,
    /// The pane's window title, as last seen.
    title: Option<String>,
    /// The agent (process group, kind) that showed its working spinner in the title: while it
    /// runs, the title says when it works.
    title_spun: Option<(Option<u32>, String)>,
    /// Show the transcript's session title as the topic (`agents.session_titles`).
    pub titles: bool,
    /// Bumped whenever the agent or its status changes: what the model said about one
    /// status (a request, a finished turn) must not stick to the next.
    episode: u64,
    ai: AiNotes,
    report_owner: Option<reporting::ReportOwner>,
    report_resume: Option<Vec<String>>,
    retired_report_instances: std::collections::HashSet<String>,
    /// What the pane's programs reported about themselves, when it decides the status.
    program: Option<program::ProgramLead>,
    /// The current state stands for a program only known from its report.
    from_program: bool,
    /// The current state before the program's report overrides it: what to fall back to once
    /// the report is gone.
    plain: Option<AgentState>,
}

/// What the on-device model said about the current agent session (`[ai]`).
#[derive(Default, Debug)]
struct AiNotes {
    /// Generated title, when the agent gives the session none.
    topic: Option<String>,
    title_pending: bool,
    title_attempts: u8,
    /// The user typed since the last turn started, and a turn ran after that.
    typed: bool,
    turn_seen: bool,
    /// The session's first prompt (hooks' prompt-submit).
    prompt: Option<String>,
    /// Screen (hash) the status fallback asked about, and the answer once it came.
    status_asked: Option<u64>,
    waiting: Option<(u64, bool)>,
    /// What the agent asks for or did, for one episode: (episode, its status, text).
    detail: Option<(u64, AgentStatus, String)>,
}

/// Something to ask the model about the pane's agent (see [`AgentTracker::ai_wants`]).
#[derive(Debug, Clone, PartialEq)]
pub enum AiWant {
    /// A title for the session, from its first prompt (if known) and the screen.
    Title { prompt: Option<String> },
    /// Whether the (quiet, unhooked) agent waits for the user, for the screen with this hash.
    Status { hash: u64 },
}

#[derive(Debug, Clone)]
struct HookState {
    kind: String,
    name: String,
    status: AgentStatus,
    session_id: Option<String>,
    message: Option<String>,
    turn_started: Option<Instant>,
    turn_ms: Option<u64>,
    turns: u32,
    transcript: Option<TitleReader>,
    /// The user answered a request (NeedsInput → Working). No hook confirms it until the
    /// tool finishes, so a quiet screen without a working pattern means Idle instead.
    answered: bool,
    /// Foreground process group when the hook last reported: the agent's own. Another
    /// program run later in the pane (`git pull`) is not the agent.
    pgrp: Option<u32>,
    /// The permission prompt showing (see [`AgentTracker::permission_prompt`]).
    permission: Option<u64>,
}

impl AgentTracker {
    pub fn state(&self) -> Option<&AgentState> {
        self.current.as_ref()
    }

    /// The program asked for attention (OSC notification) — sticky until user input.
    pub fn flag_attention(&mut self) {
        if self.current.is_some() {
            self.attention_flag = true;
        }
    }

    pub fn user_input(&mut self) {
        self.attention_flag = false;
        // Whatever the keys did (answered, moved off the default choice), the prompt the
        // notification names is not certain to be showing as it was.
        if let Some(h) = &mut self.hook {
            h.permission = None;
        }
        self.ai.typed = true;
        self.acknowledge();
    }

    /// Input that answers a request: Enter, a digit or y/n (a menu choice), Esc (declining).
    /// Moving through a menu with the arrow keys doesn't.
    pub fn user_answer(&mut self, bytes: &[u8]) {
        self.user_input();
        let answers = bytes.iter().any(|b| matches!(b, b'\r' | b'\n'))
            || matches!(bytes, [b'0'..=b'9' | b'y' | b'Y' | b'n' | b'N' | 0x1b]);
        if let Some(h) = &mut self.hook
            && h.status == AgentStatus::NeedsInput
            && answers
        {
            // Claude Code's next hook (PostToolUse) only comes once the approved tool finished.
            h.status = AgentStatus::Working;
            h.message = None;
            h.answered = true;
        }
    }

    /// The hooked agent's permission prompt, while it shows untouched: what a notification
    /// offers to answer.
    pub fn permission_prompt(&self) -> Option<u64> {
        let h = self.hook.as_ref()?;
        let shown = self
            .current
            .as_ref()
            .is_some_and(|s| s.hooked && s.kind == h.kind && s.status == AgentStatus::NeedsInput);
        h.permission
            .filter(|_| shown && h.status == AgentStatus::NeedsInput)
    }

    /// Answer permission prompt `prompt` if it still shows: the bytes to type, Enter for the
    /// dialog's default choice (allow once) or Esc (decline).
    pub fn answer_permission(&mut self, prompt: u64, allow: bool) -> Option<&'static [u8]> {
        if self.permission_prompt() != Some(prompt) {
            return None;
        }
        let keys: &'static [u8] = if allow { b"\r" } else { b"\x1b" };
        self.prompt.interrupted();
        self.user_answer(keys);
        Some(keys)
    }

    /// Current episode (see `episode`), to tag a question to the model with.
    pub fn episode(&self) -> u64 {
        self.episode
    }

    /// The model's title for the session (`None`: it saw no task, try again after a turn).
    pub fn set_ai_topic(&mut self, topic: Option<String>) {
        self.ai.title_pending = false;
        if topic.is_some() {
            self.ai.topic = topic;
        } else {
            self.ai.turn_seen = false;
        }
    }

    /// The model's answer to [`AiWant::Status`].
    pub fn set_ai_waiting(&mut self, hash: u64, waiting: bool) {
        self.ai.waiting = Some((hash, waiting));
    }

    /// What the agent asks for / did in `episode`. Ignored once the status moved on.
    pub fn set_ai_detail(&mut self, episode: u64, text: String) -> bool {
        let Some(state) = self.current.as_ref().filter(|_| episode == self.episode) else {
            return false;
        };
        self.ai.detail = Some((episode, state.status, text));
        true
    }

    /// What to ask the model now (each thing once): a title for a session that has none
    /// after its first turn, and (`status`) whether a quiet agent without hooks waits for
    /// the user on the screen `tail` shows. `program_title` is the title the agent set.
    pub fn ai_wants(
        &mut self,
        titles: bool,
        status: bool,
        tail: &str,
        program_title: Option<&str>,
    ) -> Vec<AiWant> {
        let Some(state) = self.current.as_ref().filter(|_| !self.from_program) else {
            return Vec::new();
        };
        let tail = current_activity(&state.kind, tail);
        let mut out = Vec::new();
        if titles
            && state.topic.is_none()
            && self.ai.turn_seen
            && !self.ai.title_pending
            && self.ai.title_attempts < 3
            && state.status != AgentStatus::Working
            && generic_title(program_title, state)
        {
            self.ai.title_pending = true;
            self.ai.title_attempts += 1;
            out.push(AiWant::Title {
                prompt: self.ai.prompt.clone(),
            });
        }
        if status
            && !state.hooked
            && self.program.is_none()
            && state.status == AgentStatus::Idle
            && !tail.is_empty()
        {
            let hash = screen_hash(tail);
            if self.ai.status_asked != Some(hash) {
                self.ai.status_asked = Some(hash);
                out.push(AiWant::Status { hash });
            }
        }
        out
    }

    /// The user saw the pane: a finished turn is no longer news.
    pub fn acknowledge(&mut self) {
        if let Some(h) = &mut self.hook
            && h.status == AgentStatus::Done
        {
            h.status = AgentStatus::Idle;
        }
    }

    /// Session id of the current (or last hooked) agent.
    pub fn session_id(&self) -> Option<&str> {
        self.hook.as_ref().and_then(|h| h.session_id.as_deref())
    }

    /// The pane's foreground process group now, for hooks that come between monitor ticks:
    /// an agent can start and finish before the next one.
    pub fn saw_foreground(&mut self, pgrp: Option<u32>) {
        self.fg_pgrp = pgrp;
    }

    /// Whether the title's spinner decides the current agent's status.
    fn title_decides(&self) -> bool {
        self.title_spun.as_ref().is_some_and(|(pgrp, kind)| {
            *pgrp == self.fg_pgrp && self.current.as_ref().is_some_and(|c| &c.kind == kind)
        })
    }

    /// The pane's window title now, read by the next `update`.
    pub fn saw_title(&mut self, title: Option<&str>) {
        if self.title.as_deref() != title {
            self.title = title.map(str::to_owned);
        }
    }

    /// Apply an agent hook event, sent from process group `pgrp` (the agent's). The next
    /// `update` (or `refresh`) reflects it.
    pub fn apply_hook(
        &mut self,
        pgrp: Option<u32>,
        kind: &str,
        name: &str,
        event: &str,
        session_id: Option<String>,
        message: Option<String>,
    ) {
        if matches!(event, "session-start" | "session-end") {
            self.prompt.reset();
        }
        if event == "session-end" {
            self.hook = None;
            return;
        }
        let h = match &mut self.hook {
            Some(h) if h.kind == kind => h,
            _ => self.hook.insert(HookState {
                kind: kind.to_owned(),
                name: name.to_owned(),
                status: AgentStatus::Idle,
                session_id: None,
                message: None,
                turn_started: None,
                turn_ms: None,
                turns: 0,
                transcript: None,
                answered: false,
                pgrp: None,
                permission: None,
            }),
        };
        h.answered = false;
        h.permission = None;
        if pgrp.is_some() {
            h.pgrp = pgrp;
        }
        if session_id.is_some() {
            h.session_id = session_id;
        }
        match event {
            "session-start" => {
                h.status = AgentStatus::Idle;
                h.message = None;
                // A new session (`/clear`): its own title.
                self.ai = AiNotes::default();
            }
            "prompt-submit" => {
                self.prompt.started();
                h.status = AgentStatus::Working;
                h.message = None;
                h.turn_started = Some(Instant::now());
                self.ai.turn_seen = true;
                // The message is the prompt here: the first one names the session.
                if self.ai.prompt.is_none() {
                    self.ai.prompt = message.filter(|m| !m.trim().is_empty());
                }
            }
            "tool-complete" => {
                h.status = AgentStatus::Working;
                h.message = None;
            }
            // Claude Code's reminder after a minute of idling asks for nothing.
            "notification" if message.as_deref().is_some_and(is_idle_reminder) => {
                if h.status != AgentStatus::Done {
                    h.status = AgentStatus::Idle;
                }
                h.message = None;
            }
            "notification" => {
                h.status = AgentStatus::NeedsInput;
                h.message = message;
            }
            "permission-prompt" => {
                h.status = AgentStatus::NeedsInput;
                h.message = message;
                h.permission = Some(next_prompt_id());
            }
            "stop" => {
                h.status = AgentStatus::Done;
                h.message = None;
                h.turns += 1;
                h.turn_ms = h
                    .turn_started
                    .take()
                    .map(|t| t.elapsed().as_millis().min(u64::MAX as u128) as u64);
            }
            _ => {}
        }
    }

    /// Follow the hooked session's transcript for its title (`None` stops showing it).
    pub fn read_transcript(&mut self, path: Option<&str>) {
        let Some(h) = &mut self.hook else { return };
        let Some(path) = path else {
            h.transcript = None;
            return;
        };
        // A new path means a new session (`/clear`, `/resume`): its own title.
        if !h.transcript.as_ref().is_some_and(|t| t.is(path)) {
            h.transcript = Some(TitleReader::new(path));
        }
        if let Some(t) = &mut h.transcript {
            t.poll();
            // The hook event is newer than anything the transcript said so far.
            t.take_interrupted();
        }
    }

    /// Esc fires no hook: a turn the user interrupted only shows up in the transcript.
    fn check_interrupt(&mut self) {
        let Some(h) = &mut self.hook else { return };
        if h.status != AgentStatus::Working {
            return;
        }
        let Some(t) = &mut h.transcript else { return };
        t.poll();
        if t.take_interrupted() {
            h.status = AgentStatus::Idle;
            h.message = None;
            h.turn_started = None;
        }
    }

    /// Re-derive the state from the hook alone (between monitor ticks). Returns
    /// `Some(new_state)` when it changed.
    pub fn refresh(&mut self) -> Option<Option<AgentState>> {
        if self.from_program && self.program.is_none() {
            // The program has nothing more to say; the next `update` sees what runs.
            self.from_program = false;
            return self.set(None);
        }
        // A state that stands for a program is the program's latest report alone.
        let base = match self.plain.clone().filter(|_| !self.from_program) {
            Some(c) => Some(c),
            None => self.hook_state().filter(|_| self.hook_in_foreground()),
        };
        let Some(mut base) = base else {
            let next = self.program_state();
            self.from_program = next.is_some();
            return next.and_then(|n| self.set(Some(n)));
        };
        if !base.hooked {
            // Re-derived by `with_ai`.
            base.message = None;
        }
        let next = Some(self.with_ai(self.with_hook(base)));
        self.set(next)
    }

    /// Whether the hooked agent's process group is the pane's foreground.
    fn hook_in_foreground(&self) -> bool {
        self.hook
            .as_ref()
            .is_some_and(|h| h.pgrp.is_some() && h.pgrp == self.fg_pgrp)
    }

    /// A state from the hook alone, for agents whose process we don't recognize.
    fn hook_state(&self) -> Option<AgentState> {
        let h = self.hook.as_ref()?;
        Some(AgentState {
            name: h.name.clone(),
            kind: h.kind.clone(),
            status: h.status,
            session_id: None,
            message: None,
            turn_ms: None,
            turns: 0,
            hooked: true,
            topic: None,
            permission: None,
        })
    }

    fn with_hook(&self, mut state: AgentState) -> AgentState {
        if let Some(h) = self
            .hook
            .as_ref()
            .filter(|h| h.kind == state.kind && h.pgrp == self.fg_pgrp)
        {
            state.status = h.status;
            state.session_id = h.session_id.clone();
            state.message = h.message.clone();
            state.turn_ms = h.turn_ms;
            state.turns = h.turns;
            state.hooked = true;
            state.permission = h.permission.filter(|_| h.status == AgentStatus::NeedsInput);
            state.topic = h
                .transcript
                .as_ref()
                .filter(|_| self.titles)
                .and_then(|t| t.title())
                .map(str::to_owned);
        }
        state
    }

    /// What the model added: a title when the agent gave none, the current episode's detail.
    fn with_ai(&self, mut state: AgentState) -> AgentState {
        if state.topic.is_none() {
            state.topic = self.ai.topic.clone();
        }
        // Computed before `set` bumps the episode: the status tells a new one apart.
        if let Some((episode, status, text)) = &self.ai.detail
            && *episode == self.episode
            && *status == state.status
        {
            state.message = Some(text.clone());
        }
        state
    }

    /// Make `plain`, with the program's report over it, the current state.
    fn set(&mut self, plain: Option<AgentState>) -> Option<Option<AgentState>> {
        let next = plain.clone().map(|s| self.with_program(s));
        self.plain = plain;
        self.prompt.observe(self.fg_pgrp, next.as_ref());
        if let Some(n) = &next {
            self.last_kind = Some(n.kind.clone());
        }
        let key = |s: &Option<AgentState>| s.as_ref().map(|s| (s.kind.clone(), s.status));
        let prompt = |s: &Option<AgentState>| s.as_ref().and_then(|s| s.permission);
        // Another permission prompt is another request, even with the status unchanged.
        let new_prompt = prompt(&next).is_some() && prompt(&next) != prompt(&self.current);
        if key(&next) != key(&self.current) || new_prompt {
            self.episode += 1;
            self.ai.detail = None;
            if next
                .as_ref()
                .is_some_and(|n| n.status == AgentStatus::Working)
                && self.ai.typed
            {
                self.ai.typed = false;
                self.ai.turn_seen = true;
            }
        }
        if next != self.current {
            self.current = next.clone();
            self.since = Some(Instant::now());
            Some(next)
        } else {
            None
        }
    }

    /// Recompute the state. Returns `Some(new_state)` when it changed.
    pub fn update(
        &mut self,
        defs: &[AgentDef],
        fg: Option<&ProcessInfo>,
        screen_tail: &str,
        idle: Duration,
        idle_after: Duration,
    ) -> Option<Option<AgentState>> {
        self.check_interrupt();
        self.fg_pgrp = fg.map(|p| p.pid);
        let def = fg.and_then(|p| defs.iter().find(|d| d.matches(&p.name, &p.argv)));
        // Hooked agent running under a process name we don't know (a wrapper, `node …`):
        // trust the hook while the process group that sent it is in the foreground.
        let unknown_hooked = def.is_none()
            && self.hook_in_foreground()
            && fg.is_some_and(|p| !crate::procinfo::is_shell(&p.name));
        if unknown_hooked {
            self.from_program = false;
            let next = self.hook_state().map(|s| self.with_ai(self.with_hook(s)));
            return self.set(next);
        }
        if let Some(h) = &mut self.hook
            && h.answered
            && idle >= idle_after
            && !def.is_some_and(|d| d.working.iter().any(|w| screen_tail.contains(w.as_str())))
        {
            // Answered, but nothing runs: declined, or done without a hook.
            h.answered = false;
            h.status = AgentStatus::Idle;
        }
        let spinning = def.is_some_and(|d| {
            self.title
                .as_deref()
                .is_some_and(|t| d.title_spinner(t).is_some())
        });
        let agent = def.map(|d| (self.fg_pgrp, d.kind.clone()));
        if spinning {
            self.title_spun = agent.clone();
        }
        // Neither output (the user's typing echoed, a redrawn status line) nor working text
        // left on the screen says anything once the title does.
        let title_decides = agent.is_some() && self.title_spun == agent;
        let next = def.map(|d| {
            let screen_tail = current_activity(&d.kind, screen_tail);
            let attention =
                self.attention_flag || d.attention.iter().any(|a| screen_tail.contains(a.as_str()));
            let working = d.working.iter().any(|w| screen_tail.contains(w.as_str()));
            // The model read this very screen and saw a question no pattern caught.
            let waiting = !working
                && idle >= idle_after
                && self
                    .ai
                    .waiting
                    .is_some_and(|(h, w)| w && h == screen_hash(screen_tail));
            let status = if attention || waiting {
                AgentStatus::NeedsInput
            } else if spinning || (!title_decides && (working || idle < idle_after)) {
                AgentStatus::Working
            } else {
                AgentStatus::Idle
            };
            self.with_ai(self.with_hook(AgentState {
                name: d.name.clone(),
                kind: d.kind.clone(),
                status,
                session_id: None,
                message: None,
                turn_ms: None,
                turns: 0,
                hooked: false,
                topic: None,
                permission: None,
            }))
        });
        if next.is_none() {
            self.attention_flag = false;
            self.title_spun = None;
            // The next agent here is a new session.
            if self.current.is_some() {
                self.ai = AiNotes::default();
            }
            // The hooked agent left the foreground: its session is over for this pane, but
            // keep the id for resume until another agent reports.
            if let Some(h) = &mut self.hook
                && self.current.as_ref().is_some_and(|c| c.kind == h.kind)
            {
                h.status = AgentStatus::Idle;
                h.message = None;
            }
        }
        // No agent runs, but a program said what it does.
        let next = next.or_else(|| self.program_state());
        self.from_program = def.is_none() && next.is_some();
        self.set(next)
    }
}

/// "42s", "3m 05s", "1h 02m".
pub fn human_duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// "Claude is waiting for your input": sent after a minute without input, not for a request.
/// Identifies a permission prompt. Unique across daemon restarts and in-place upgrades, whose
/// trackers start afresh while notifications naming older prompts may still be answered: the
/// clock in microseconds, kept increasing.
fn next_prompt_id() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64);
    let prev = LAST
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |l| {
            Some(now.max(l + 1))
        })
        .unwrap_or_default();
    now.max(prev + 1)
}

fn is_idle_reminder(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("waiting for your input")
}

/// Whether the title the agent set says nothing about the session ("✳ Claude Code", none).
pub fn generic_title(title: Option<&str>, agent: &AgentState) -> bool {
    let Some(t) = title else { return true };
    let t = t.trim_start_matches(|c: char| !c.is_alphanumeric()).trim();
    t.is_empty() || t.eq_ignore_ascii_case(&agent.name) || t.eq_ignore_ascii_case(&agent.kind)
}

pub fn screen_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Codex starts each activity with an unindented bullet. Earlier activities can
/// contain approval requests that are already complete.
pub fn current_activity<'a>(agent: &str, screen: &'a str) -> &'a str {
    if agent.eq_ignore_ascii_case("codex")
        && let Some(start) = screen.rfind("\n• ")
    {
        &screen[start + 1..]
    } else {
        screen
    }
}

/// The last `n` non-empty lines of the screen.
pub fn tail(screen: &str, n: usize) -> String {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(name: &str) -> ProcessInfo {
        ProcessInfo {
            pid: 1,
            name: name.into(),
            argv: vec![name.into()],
        }
    }

    fn proc_in(name: &str, pgrp: u32) -> ProcessInfo {
        ProcessInfo {
            pid: pgrp,
            ..proc(name)
        }
    }

    #[test]
    fn a_turn_between_monitor_ticks_is_seen() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        // The last tick saw the shell; the agent (`node …`, group 10) started after it.
        t.update(
            &defs,
            Some(&proc_in("fish", 5)),
            "",
            Duration::ZERO,
            idle_after,
        );
        t.saw_foreground(Some(10));
        t.apply_hook(
            Some(10),
            "claude",
            "Claude Code",
            "prompt-submit",
            None,
            None,
        );
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Working);
        t.saw_foreground(Some(10));
        t.apply_hook(Some(10), "claude", "Claude Code", "stop", None, None);
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Done);
    }

    #[test]
    fn a_stale_hook_does_not_claim_other_programs() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        // A hooked agent under a name we don't know (`node …`), in process group 10.
        let node = proc_in("node", 10);
        t.update(&defs, Some(&node), "> ", quiet, idle_after);
        t.apply_hook(
            Some(10),
            "claude",
            "Claude Code",
            "session-start",
            Some("abc".into()),
            None,
        );
        assert!(t.refresh().unwrap().unwrap().hooked);

        // It exits without session-end; the hook stays for resume.
        assert_eq!(
            t.update(&defs, Some(&proc_in("fish", 5)), "", quiet, idle_after),
            Some(None)
        );
        assert_eq!(t.session_id(), Some("abc"));
        // Focusing the pane doesn't bring it back.
        assert_eq!(t.refresh(), None);
        // Nor does the next program run there.
        assert_eq!(
            t.update(
                &defs,
                Some(&proc_in("git", 11)),
                "",
                Duration::ZERO,
                idle_after
            ),
            None
        );
        assert_eq!(t.refresh(), None);
        // A hook the agent sent before it exited, arriving late, names the agent's group.
        t.apply_hook(Some(10), "claude", "Claude Code", "stop", None, None);
        assert_eq!(t.refresh(), None);
        assert_eq!(
            t.update(&defs, Some(&proc_in("git", 11)), "", quiet, idle_after),
            None
        );
    }

    #[test]
    fn status_transitions() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        // Not an agent.
        assert_eq!(
            t.update(&defs, Some(&proc("vim")), "", Duration::ZERO, idle_after),
            None
        );
        // Claude working (recent output).
        let s = t.update(
            &defs,
            Some(&proc("claude")),
            "✻ Thinking… (esc to interrupt)",
            Duration::from_secs(5),
            idle_after,
        );
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::Working);
        // Permission prompt.
        let s = t.update(
            &defs,
            Some(&proc("claude")),
            "Do you want to proceed?\n❯ 1. Yes",
            Duration::from_secs(5),
            idle_after,
        );
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::NeedsInput);
        // Idle.
        let s = t.update(
            &defs,
            Some(&proc("claude")),
            "> ",
            Duration::from_secs(5),
            idle_after,
        );
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::Idle);
        // Notification makes it sticky until input.
        t.flag_attention();
        let s = t.update(
            &defs,
            Some(&proc("claude")),
            "> ",
            Duration::from_secs(5),
            idle_after,
        );
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::NeedsInput);
        t.user_input();
        let s = t.update(
            &defs,
            Some(&proc("claude")),
            "> ",
            Duration::from_secs(5),
            idle_after,
        );
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::Idle);
        // Exits.
        assert_eq!(
            t.update(&defs, Some(&proc("zsh")), "", Duration::ZERO, idle_after),
            Some(None)
        );
        assert_eq!(t.last_kind.as_deref(), Some("claude"));
    }

    #[test]
    fn title_spinner_decides_once_seen() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let claude = proc("claude");
        let status = |t: &AgentTracker| t.state().unwrap().status;
        // Before the spinner shows, output activity still counts (a title-less agent).
        t.saw_title(Some("✳ Claude Code"));
        t.update(&defs, Some(&claude), "❯ ", Duration::ZERO, idle_after);
        assert_eq!(status(&t), AgentStatus::Working);
        t.saw_title(Some("◐ Fix the sidebar"));
        t.update(
            &defs,
            Some(&claude),
            "✽ Hyperspacing… (12s)",
            Duration::from_secs(5),
            idle_after,
        );
        assert_eq!(status(&t), AgentStatus::Working);
        // The user types at the prompt: echoed output, but the title says idle.
        t.saw_title(Some("✳ Fix the sidebar"));
        t.update(
            &defs,
            Some(&claude),
            "❯ do it for",
            Duration::ZERO,
            idle_after,
        );
        assert_eq!(status(&t), AgentStatus::Idle);
        // Permission prompts still win.
        t.update(
            &defs,
            Some(&claude),
            "Do you want to proceed?",
            Duration::ZERO,
            idle_after,
        );
        assert_eq!(status(&t), AgentStatus::NeedsInput);
        // The next agent in the pane starts over.
        t.update(&defs, Some(&proc("fish")), "", Duration::ZERO, idle_after);
        t.update(&defs, Some(&claude), "❯ ", Duration::ZERO, idle_after);
        assert_eq!(status(&t), AgentStatus::Working);
    }

    #[test]
    fn idle_title_beats_working_text_left_on_screen() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        let claude = proc("claude");
        t.saw_title(Some("◑ Task"));
        t.update(
            &defs,
            Some(&claude),
            "(esc to interrupt)",
            quiet,
            idle_after,
        );
        assert_eq!(t.state().unwrap().status, AgentStatus::Working);
        t.saw_title(Some("✳ Task"));
        t.update(
            &defs,
            Some(&claude),
            "(esc to interrupt)\n❯ ",
            quiet,
            idle_after,
        );
        assert_eq!(t.state().unwrap().status, AgentStatus::Idle);
    }

    #[test]
    fn spinner_history_belongs_to_one_agent() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        t.saw_title(Some("◐ Task"));
        t.update(&defs, Some(&proc_in("claude", 10)), "", quiet, idle_after);
        t.saw_title(Some("✳ Task"));
        t.update(
            &defs,
            Some(&proc_in("claude", 10)),
            "",
            Duration::ZERO,
            idle_after,
        );
        assert_eq!(t.state().unwrap().status, AgentStatus::Idle);
        // pi replaces it before a tick sees the shell: its output counts again.
        t.saw_title(Some("pi"));
        t.update(
            &defs,
            Some(&proc_in("pi", 20)),
            "",
            Duration::ZERO,
            idle_after,
        );
        assert_eq!(t.state().unwrap().kind, "pi");
        assert_eq!(t.state().unwrap().status, AgentStatus::Working);
        // So does another Claude Code in a new process group.
        t.saw_title(Some("✳ Claude Code"));
        t.update(
            &defs,
            Some(&proc_in("claude", 30)),
            "",
            Duration::ZERO,
            idle_after,
        );
        assert_eq!(t.state().unwrap().status, AgentStatus::Working);
    }

    #[test]
    fn codex_title_spinner_and_pi_screen() {
        let defs = thurm_config::builtin_agents();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        let mut t = AgentTracker::default();
        t.saw_title(Some("⠙ thurm"));
        t.update(&defs, Some(&proc("codex")), "›", quiet, idle_after);
        assert_eq!(t.state().unwrap().status, AgentStatus::Working);
        t.saw_title(Some("thurm"));
        t.update(
            &defs,
            Some(&proc("codex")),
            "› typing",
            Duration::ZERO,
            idle_after,
        );
        assert_eq!(t.state().unwrap().status, AgentStatus::Idle);

        let mut t = AgentTracker::default();
        t.update(
            &defs,
            Some(&proc("pi")),
            "── ⠴ Working ───",
            quiet,
            idle_after,
        );
        assert_eq!(t.state().unwrap().kind, "pi");
        assert_eq!(t.state().unwrap().status, AgentStatus::Working);
        t.update(&defs, Some(&proc("pi")), "> ", quiet, idle_after);
        assert_eq!(t.state().unwrap().status, AgentStatus::Idle);
    }

    #[test]
    fn esc_interrupt_ends_a_hooked_turn() {
        let path =
            std::env::temp_dir().join(format!("thurm-agent-esc-{}.jsonl", std::process::id()));
        let interrupt = "{\"type\":\"user\",\"interruptedMessageId\":\"msg_1\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[Request interrupted by user]\"}]}}\n";
        // An earlier turn was interrupted too: that must not end the new one.
        std::fs::write(&path, interrupt).unwrap();
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        let claude = proc("claude");
        t.update(&defs, Some(&claude), "> ", quiet, idle_after);
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "prompt-submit",
            None,
            None,
        );
        t.read_transcript(path.to_str());
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Working);
        assert!(
            t.update(&defs, Some(&claude), "> ", quiet, idle_after)
                .is_none()
        );

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut f, interrupt.as_bytes()).unwrap();
        let s = t.update(&defs, Some(&claude), "> ", quiet, idle_after);
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::Idle);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hooks_override_screen_heuristics() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        let claude = proc("claude");
        t.update(&defs, Some(&claude), "> ", quiet, idle_after);
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "session-start",
            Some("abc-123".into()),
            None,
        );
        let s = t.refresh().unwrap().unwrap();
        assert!(s.hooked);
        assert_eq!(s.session_id.as_deref(), Some("abc-123"));
        assert_eq!(s.status, AgentStatus::Idle);

        // Working per hook even though the screen is quiet.
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "prompt-submit",
            None,
            None,
        );
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Working);
        let s = t.update(&defs, Some(&claude), "> ", quiet, idle_after);
        assert!(
            s.is_none(),
            "screen heuristics must not flip a hooked Working to Idle"
        );

        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "notification",
            None,
            Some("Claude needs your permission".into()),
        );
        let s = t.refresh().unwrap().unwrap();
        assert_eq!(s.status, AgentStatus::NeedsInput);
        assert_eq!(s.message.as_deref(), Some("Claude needs your permission"));

        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "tool-complete",
            None,
            None,
        );
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Working);
        t.apply_hook(Some(1), "claude", "Claude Code", "stop", None, None);
        let s = t.refresh().unwrap().unwrap();
        assert_eq!(s.status, AgentStatus::Done);
        assert_eq!(s.turns, 1);
        assert!(s.turn_ms.is_some());

        // Typing acknowledges Done.
        t.user_input();
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Idle);

        // Agent exits: state gone, session id kept for resume.
        assert_eq!(
            t.update(&defs, Some(&proc("zsh")), "", Duration::ZERO, idle_after),
            Some(None)
        );
        assert_eq!(t.session_id(), Some("abc-123"));
        t.apply_hook(Some(1), "claude", "Claude Code", "session-end", None, None);
        assert_eq!(t.session_id(), None);
    }

    #[test]
    fn ai_titles_after_the_first_turn() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        let codex = proc("codex");
        t.update(&defs, Some(&codex), "> ", quiet, idle_after);
        // Nothing typed yet: the welcome screen names no task.
        assert!(t.ai_wants(true, false, "> ", None).is_empty());
        t.user_input();
        t.update(
            &defs,
            Some(&codex),
            "> fix the login",
            Duration::ZERO,
            idle_after,
        );
        // Not while the turn runs.
        assert!(t.ai_wants(true, false, "", None).is_empty());
        t.update(&defs, Some(&codex), "> done", quiet, idle_after);
        // The agent's own title names the task: nothing to add.
        assert!(t.ai_wants(true, false, "", Some("⠋ Fix login")).is_empty());
        assert_eq!(
            t.ai_wants(true, false, "", Some("codex")),
            vec![AiWant::Title { prompt: None }]
        );
        // Asked once.
        assert!(t.ai_wants(true, false, "", None).is_empty());
        t.set_ai_topic(Some("Fix login bug".into()));
        let s = t.refresh().unwrap().unwrap();
        assert_eq!(s.topic.as_deref(), Some("Fix login bug"));
        assert!(t.ai_wants(true, false, "", None).is_empty());
        // A new agent session starts without it.
        t.update(&defs, Some(&proc("zsh")), "", quiet, idle_after);
        let s = t.update(&defs, Some(&codex), "> ", quiet, idle_after);
        assert_eq!(s.unwrap().unwrap().topic, None);
    }

    #[test]
    fn ai_title_uses_the_hooked_prompt() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let codex = proc("codex");
        t.update(
            &defs,
            Some(&codex),
            "> ",
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        t.apply_hook(
            Some(1),
            "codex",
            "Codex",
            "prompt-submit",
            None,
            Some("add CSV export".into()),
        );
        t.apply_hook(Some(1), "codex", "Codex", "stop", None, None);
        t.refresh();
        assert_eq!(
            t.ai_wants(true, false, "", None),
            vec![AiWant::Title {
                prompt: Some("add CSV export".into())
            }]
        );
        // No task seen: tried again after the next turn.
        t.set_ai_topic(None);
        assert!(t.ai_wants(true, false, "", None).is_empty());
        t.apply_hook(
            Some(1),
            "codex",
            "Codex",
            "prompt-submit",
            None,
            Some("and tests".into()),
        );
        t.apply_hook(Some(1), "codex", "Codex", "stop", None, None);
        t.refresh();
        // The first prompt still names the session.
        assert_eq!(
            t.ai_wants(true, false, "", None),
            vec![AiWant::Title {
                prompt: Some("add CSV export".into())
            }]
        );
    }

    #[test]
    fn completed_codex_approval_does_not_need_input() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let codex = proc("codex");
        let quiet = Duration::from_secs(5);
        let idle_after = Duration::from_secs(1);
        let pending = "• I will push the fix.\nWould you like to run the following command?\n  › 1. Yes, proceed";
        t.update(&defs, Some(&codex), pending, quiet, idle_after);
        assert_eq!(t.state().unwrap().status, AgentStatus::NeedsInput);
        t.set_ai_detail(t.episode(), "Asks to run git push".into());
        let completed = include_str!("../tests/fixtures/codex-completed-approval.txt");
        t.update(&defs, Some(&codex), completed, quiet, idle_after);
        assert_eq!(t.state().unwrap().status, AgentStatus::Idle);
        assert_eq!(t.state().unwrap().message, None);

        // The model and the status check use the same current activity.
        let wants = t.ai_wants(false, true, completed, None);
        let [AiWant::Status { hash }] = wants[..] else {
            panic!("{wants:?}")
        };
        assert_eq!(hash, screen_hash(current_activity("codex", completed)));
        t.set_ai_waiting(hash, false);
        t.update(&defs, Some(&codex), completed, quiet, idle_after);
        assert_eq!(t.state().unwrap().status, AgentStatus::Idle);

        let working = format!("{pending}\n• Running git push (esc to interrupt)");
        t.update(&defs, Some(&codex), &working, quiet, idle_after);
        assert_eq!(t.state().unwrap().status, AgentStatus::Working);

        // A new approval request after the result still needs input.
        let next = format!("{completed}\n{pending}");
        t.update(&defs, Some(&codex), &next, quiet, idle_after);
        assert_eq!(t.state().unwrap().status, AgentStatus::NeedsInput);
    }

    #[test]
    fn current_activity_keeps_the_current_question_and_its_choices() {
        let question = "• Which database should I use?\n  • Postgres\n  • SQLite\n› ";
        let screen = format!("• Read the configuration.\n{question}");
        assert_eq!(current_activity("codex", &screen), question);
        assert_eq!(current_activity("Codex", &screen), question);
        assert_eq!(current_activity("claude", &screen), screen);
        assert_eq!(current_activity("codex", "Allow?\n(y/n)"), "Allow?\n(y/n)");
    }

    #[test]
    fn ai_status_fallback() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        let codex = proc("codex");
        let screen = "Run `cargo test`? Press y to allow, n to deny";
        let s = t.update(&defs, Some(&codex), screen, quiet, idle_after);
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::Idle);
        let wants = t.ai_wants(false, true, screen, None);
        let [AiWant::Status { hash }] = wants[..] else {
            panic!("{wants:?}")
        };
        assert_eq!(hash, screen_hash(screen));
        // Once per screen.
        assert!(t.ai_wants(false, true, screen, None).is_empty());
        t.set_ai_waiting(hash, true);
        let s = t.update(&defs, Some(&codex), screen, quiet, idle_after);
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::NeedsInput);
        // Another screen: back to the patterns, and a new question.
        let s = t.update(&defs, Some(&codex), "> ", quiet, idle_after);
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::Idle);
        assert_eq!(t.ai_wants(false, true, "> ", None).len(), 1);
        // Off: never asked.
        let mut t = AgentTracker::default();
        t.update(&defs, Some(&codex), screen, quiet, idle_after);
        assert!(t.ai_wants(false, false, screen, None).is_empty());
    }

    #[test]
    fn ai_detail_belongs_to_one_episode() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let claude = proc("claude");
        t.update(
            &defs,
            Some(&claude),
            "> ",
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "notification",
            None,
            Some("Claude needs your permission to use Bash".into()),
        );
        t.refresh();
        let episode = t.episode();
        assert!(t.set_ai_detail(episode, "Wants to run `rm -rf build`".into()));
        let s = t.refresh().unwrap().unwrap();
        assert_eq!(s.message.as_deref(), Some("Wants to run `rm -rf build`"));

        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "tool-complete",
            None,
            None,
        );
        let s = t.refresh().unwrap().unwrap();
        assert_eq!(s.status, AgentStatus::Working);
        assert_eq!(s.message, None);
        // A late answer for the old request.
        assert!(!t.set_ai_detail(episode, "stale".into()));
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "notification",
            None,
            Some("Claude needs your permission to use Edit".into()),
        );
        let s = t.refresh().unwrap().unwrap();
        assert_eq!(
            s.message.as_deref(),
            Some("Claude needs your permission to use Edit")
        );
    }

    #[test]
    fn answering_a_request_resumes_the_turn() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let claude = proc("claude");
        t.update(
            &defs,
            Some(&claude),
            "> ",
            Duration::from_secs(5),
            idle_after,
        );
        let ask = |t: &mut AgentTracker| {
            t.apply_hook(
                Some(1),
                "claude",
                "Claude Code",
                "prompt-submit",
                None,
                None,
            );
            t.apply_hook(
                Some(1),
                "claude",
                "Claude Code",
                "notification",
                None,
                Some("Claude needs your permission to use Bash".into()),
            );
            assert_eq!(
                t.refresh().unwrap().unwrap().status,
                AgentStatus::NeedsInput
            );
        };
        ask(&mut t);
        // Moving through the menu isn't an answer.
        t.user_answer(b"\x1b[B");
        assert!(t.refresh().is_none());
        t.user_answer(b"\r");
        let s = t.refresh().unwrap().unwrap();
        assert_eq!((s.status, s.message), (AgentStatus::Working, None));
        // The approved command runs: still working.
        let running = "⏺ Bash(./macos/build.sh)\n✻ Running… (esc to interrupt)";
        assert!(
            t.update(
                &defs,
                Some(&claude),
                running,
                Duration::from_secs(5),
                idle_after
            )
            .is_none()
        );
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "tool-complete",
            None,
            None,
        );
        t.refresh();
        assert_eq!(t.state().unwrap().status, AgentStatus::Working);

        // Declined: nothing runs, so it goes idle once the screen is quiet.
        ask(&mut t);
        t.user_answer(b"3");
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Working);
        let s = t.update(
            &defs,
            Some(&claude),
            "> ",
            Duration::from_secs(5),
            idle_after,
        );
        assert_eq!(s.unwrap().unwrap().status, AgentStatus::Idle);
    }

    #[test]
    fn idle_reminder_is_not_a_request() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let claude = proc("claude");
        t.update(
            &defs,
            Some(&claude),
            "> ",
            Duration::from_secs(5),
            Duration::from_secs(1),
        );
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "prompt-submit",
            None,
            None,
        );
        t.apply_hook(Some(1), "claude", "Claude Code", "stop", None, None);
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "notification",
            None,
            Some("Claude is waiting for your input".into()),
        );
        let s = t.refresh().unwrap().unwrap();
        assert_eq!((s.status, s.message), (AgentStatus::Done, None));
        t.user_input();
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "notification",
            None,
            Some("Claude is waiting for your input".into()),
        );
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Idle);
    }

    #[test]
    fn generic_titles() {
        let a = AgentState {
            name: "Claude Code".into(),
            kind: "claude".into(),
            status: AgentStatus::Idle,
            session_id: None,
            message: None,
            turn_ms: None,
            turns: 0,
            hooked: false,
            topic: None,
            permission: None,
        };
        assert!(generic_title(None, &a));
        assert!(generic_title(Some("✳ Claude Code"), &a));
        assert!(generic_title(Some("claude"), &a));
        assert!(!generic_title(Some("✳ Fix login bug"), &a));
    }

    #[test]
    fn permission_prompts_are_answered_only_while_untouched() {
        let defs = thurm_config::builtin_agents();
        let mut t = AgentTracker::default();
        let idle_after = Duration::from_millis(1500);
        let quiet = Duration::from_secs(5);
        let claude = proc("claude");
        t.update(&defs, Some(&claude), "> ", quiet, idle_after);
        let prompt = |t: &mut AgentTracker| {
            t.apply_hook(
                Some(1),
                "claude",
                "Claude Code",
                "permission-prompt",
                None,
                Some("Claude needs your permission to use Bash".into()),
            );
            t.refresh();
            t.permission_prompt().unwrap()
        };

        // A plain notification (a question, an elicitation) offers no answer.
        t.apply_hook(Some(1), "claude", "Claude Code", "notification", None, None);
        t.refresh();
        assert_eq!(t.permission_prompt(), None);

        let first = prompt(&mut t);
        assert_eq!(t.state().unwrap().status, AgentStatus::NeedsInput);
        assert_eq!(t.answer_permission(first, true), Some(&b"\r"[..]));
        assert_eq!(t.refresh().unwrap().unwrap().status, AgentStatus::Working);
        assert_eq!(t.answer_permission(first, true), None, "answered once");

        // An old notification can't answer the next prompt.
        let second = prompt(&mut t);
        assert_ne!(first, second);
        assert_eq!(t.answer_permission(first, false), None);
        assert_eq!(t.answer_permission(second, false), Some(&b"\x1b"[..]));

        // Keys typed in the pane (even an arrow off the default choice) retire it.
        let third = prompt(&mut t);
        t.user_answer(b"\x1b[B");
        assert_eq!(t.state().unwrap().status, AgentStatus::NeedsInput);
        assert_eq!(t.answer_permission(third, true), None);

        // A tracker starting afresh (an in-place upgrade) never reuses a number that an old
        // notification may still carry.
        let mut fresh = AgentTracker::default();
        fresh.update(&defs, Some(&claude), "> ", quiet, idle_after);
        assert!(prompt(&mut fresh) > third);

        // So does any later hook.
        let fourth = prompt(&mut t);
        t.apply_hook(
            Some(1),
            "claude",
            "Claude Code",
            "tool-complete",
            None,
            None,
        );
        t.refresh();
        assert_eq!(t.answer_permission(fourth, true), None);
    }

    #[test]
    fn permission_answer_invalidates_the_submitted_turn() {
        let defs = thurm_config::builtin_agents();
        let mut tracker = AgentTracker::default();
        tracker.update(
            &defs, Some(&proc("claude")), "> ",
            Duration::from_secs(5), Duration::from_millis(1500),
        );
        tracker.apply_hook(Some(1), "claude", "Claude Code", "session-start", None, None);
        tracker.refresh();
        let token = tracker.prompt.reserve(tracker.state().unwrap().status)
            .unwrap();
        tracker.apply_hook(Some(1), "claude", "Claude Code", "prompt-submit", None, None);
        tracker.refresh();
        tracker.apply_hook(
            Some(1), "claude", "Claude Code", "permission-prompt", None, None,
        );
        tracker.refresh();
        let prompt = tracker.permission_prompt().unwrap();
        assert!(tracker.answer_permission(prompt.wrapping_add(1), true).is_none());
        assert!(tracker.prompt.outcome(token, AgentStatus::NeedsInput).is_ok());
        assert_eq!(tracker.answer_permission(prompt, true), Some(&b"\r"[..]));
        tracker.apply_hook(Some(1), "claude", "Claude Code", "stop", None, None);
        tracker.refresh();
        assert!(tracker.prompt.outcome(token, tracker.state().unwrap().status)
            .is_err());
    }

    #[test]
    fn tail_lines() {
        assert_eq!(tail("a\n\nb\nc\n\n", 2), "b\nc");
    }
}
