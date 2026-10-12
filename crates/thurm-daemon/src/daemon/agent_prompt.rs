use super::*;
use crate::agent_prompt::PromptToken;
use thurm_proto::AgentPromptOutcome;

pub(super) struct PromptTicket {
    token: PromptToken,
    process: thurm_proto::ProcessInfo,
    birth: u64,
    deadline: Instant,
}

impl Daemon {
    pub(super) fn submit_agent_prompt(
        &self,
        pane: PaneId,
        text: &str,
        timeout_ms: u64,
    ) -> Result<PromptTicket, String> {
        if !(1..=86_400_000).contains(&timeout_ms) {
            return Err("prompt timeout must be between 1 ms and 24 hours".into());
        }
        if text.trim().is_empty()
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Err(
                "prompt must contain text and no control characters other than newline or tab"
                    .into(),
            );
        }
        let defs = self.agent_defs.read().clone();
        let (detect, idle_after) = {
            let cfg = self.config.read();
            (
                cfg.agents.detect,
                Duration::from_millis(cfg.agents.idle_after_ms),
            )
        };
        self.with_pane(pane, |p, st| {
            if !st.info.alive || st.pending_input.is_some() || st.terminal_attachment.is_some() {
                return Err("pane is not ready for an agent prompt".into());
            }
            let process = st
                .pty
                .foreground_pgrp()
                .and_then(procinfo::process_info)
                .ok_or("cannot identify the foreground process")?;
            if procinfo::is_shell(&process.name) {
                return Err("the foreground process is a shell, not an agent".into());
            }
            let birth = procinfo::process_birth(process.pid)
                .ok_or("cannot identify the foreground process start time")?;
            let tail = agents::tail(&st.term.screen_text(), 20);
            let idle = st.term.last_output.elapsed();
            st.agent.saw_title(st.term.title());
            let invalidated = st.agent.invalidate_dead_report_owner();
            let changed = if detect {
                st.agent
                    .update(&defs, Some(&process), &tail, idle, idle_after)
            } else {
                st.agent.update_report_foreground(Some(&process))
            }
            .is_some();
            if invalidated || changed {
                st.info.agent = st.agent.state().cloned();
                st.info.title = pane_title(st, Some(&process));
                self.broadcast(Event::PaneInfo(st.info.clone()), false);
            }
            let agent = st.agent.state().ok_or("no agent is running in this pane")?;
            let status = agent.status;
            if agent.permission.is_some() || st.pty.password_mode() {
                return Err("the pane has an input request; answer it first".into());
            }
            let mut bytes = st.term.paste(text);
            if text.contains('\n') && !bytes.starts_with(b"\x1b[200~") {
                return Err("the agent must enable bracketed paste for a multiline prompt".into());
            }
            bytes.push(b'\r');
            let token = st.agent.prompt.reserve(status)?;
            st.agent.user_input();
            if p.input.send(bytes).is_err() {
                st.agent.prompt.reset();
                return Err("the pane input channel is closed".into());
            }
            Ok(PromptTicket {
                token,
                process,
                birth,
                deadline: Instant::now() + Duration::from_millis(timeout_ms),
            })
        })?
    }

    pub(super) fn wait_agent_prompt(
        &self,
        client: u64,
        pane: PaneId,
        ticket: PromptTicket,
    ) -> Result<AgentPromptOutcome, String> {
        loop {
            if !self.clients.lock().contains_key(&client) || Instant::now() >= ticket.deadline {
                return Ok(AgentPromptOutcome::Timeout);
            }
            let Some(pane) = self.pane(pane) else {
                return Ok(AgentPromptOutcome::Exited);
            };
            {
                let mut st = pane.state.lock();
                if st.terminal_attachment.is_some() {
                    return Err("pane input belongs to an attached terminal".into());
                }
                if st.agent.invalidate_dead_report_owner() {
                    st.info.agent = st.agent.state().cloned();
                    self.broadcast(Event::PaneInfo(st.info.clone()), false);
                }
                if !st.info.alive {
                    return Ok(AgentPromptOutcome::Exited);
                }
                let process = st.pty.foreground_pgrp().and_then(procinfo::process_info);
                if process.as_ref() != Some(&ticket.process)
                    || procinfo::process_birth(ticket.process.pid) != Some(ticket.birth)
                {
                    return Err("the foreground process changed after prompt submission".into());
                }
                let agent = st
                    .agent
                    .state()
                    .ok_or("the agent exited after prompt submission")?;
                if let Some(outcome) = st.agent.prompt.outcome(ticket.token, agent.status)? {
                    return Ok(outcome);
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
