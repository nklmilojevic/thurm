//! Exclusive terminal attachment, released when its connection closes.

use super::*;

/// [`Daemon::typing_frozen`]'s error: input held back while an upgrade captures the panes.
const TYPING_FROZEN: &str = "typing waits for the upgrade";
/// Longest a held-back input waits for the upgrade (which takes milliseconds) before failing.
const TYPING_FROZEN_LIMIT: Duration = Duration::from_secs(10);

impl Daemon {
    pub(super) fn with_input_pane<T>(
        &self,
        client: u64,
        pane: PaneId,
        f: impl FnOnce(&Arc<Pane>, &mut PaneState) -> T,
    ) -> Result<T, String> {
        self.with_pane(pane, |p, st| {
            if st
                .terminal_attachment
                .is_some_and(|(owner, _)| owner != client)
            {
                return Err("pane input belongs to an attached terminal".into());
            }
            Ok(f(p, st))
        })?
    }

    /// Holds back typing (what sets `typed`) into a pane waiting to resume a program while an
    /// upgrade hands the pane over: the first input would cancel the resume, after `typed` was
    /// captured (see `set_upgrading`). Called under the pane's lock, which `handoff` takes
    /// after setting the flag, before anything of the input takes effect. The caller drops the
    /// lock and retries through [`Daemon::typing_after_upgrade`].
    pub(super) fn typing_frozen(&self, pane: PaneId, st: &PaneState) -> Result<(), String> {
        if !st.typed
            && self.upgrading.load(Ordering::SeqCst)
            && (st.pending_input.is_some()
                || self.resume_tickets.lock().iter().any(|t| t.pane == pane))
        {
            return Err(TYPING_FROZEN.into());
        }
        Ok(())
    }

    /// Runs `typing` (input under the pane's lock that checks [`Daemon::typing_frozen`]) and,
    /// while an upgrade holds it back, waits for the upgrade outside the lock and tries again.
    /// The input is never refused for the hand-off: keystrokes come without waiting for a
    /// reply, so an error would drop them unseen. If the upgrade gives up, the input goes
    /// through here (and cancels the resume); if it goes ahead, this image ends before the
    /// input reached the shell, as for any request in flight at the exec, and the next image
    /// resumes the program with nothing typed: what the hand-off said.
    pub(super) fn typing_after_upgrade<T>(
        &self,
        mut typing: impl FnMut() -> Result<T, String>,
    ) -> Result<T, String> {
        let deadline = Instant::now() + TYPING_FROZEN_LIMIT;
        loop {
            match typing() {
                Err(e) if e == TYPING_FROZEN => {}
                other => return other,
            }
            while self.upgrading.load(Ordering::SeqCst) {
                if Instant::now() >= deadline {
                    return Err("the daemon is restarting: try again".into());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    pub(super) fn attach_terminal(
        &self,
        client: &Client,
        pane: PaneId,
        size: PaneSize,
    ) -> Result<Response, String> {
        self.with_pane(pane, |_, st| {
            if !st.info.alive {
                return Err("cannot attach: the pane process has exited".into());
            }
            if st.terminal_attachment.is_some() {
                return Err("a terminal is already attached to this pane".into());
            }
            if st.pending_input.is_some() || st.agent.prompt.is_pending() {
                return Err("cannot attach while pane input is pending".into());
            }
            let original = st.info.size;
            let size = sanitize_size(size);
            st.pty
                .resize(size)
                .map_err(|e| format!("cannot resize pane: {e}"))?;
            st.agent.prompt.interrupted();
            st.terminal_attachment = Some((client.id, original));
            st.term.resize(size);
            st.info.size = size;
            self.send_subscribers(st, Event::Resized { pane, size });
            let state = st.attach_state();
            client.send(ServerMessage::Event(Event::Attach { pane, size, state }));
            st.subscribers.insert(client.id, Subscriber::default());
            Ok(Response::Ok)
        })?
    }

    pub(super) fn release_terminal(&self, st: &mut PaneState, client: u64) {
        if let Some((owner, size)) = st.terminal_attachment
            && owner == client
        {
            st.terminal_attachment = None;
            if let Err(e) = st.pty.resize(size) {
                log::warn!("restore pane size after detach: {e}");
            }
            st.term.resize(size);
            st.info.size = size;
            self.send_subscribers(
                st,
                Event::Resized {
                    pane: st.info.id,
                    size,
                },
            );
        }
    }

    pub(super) fn detach_terminal(&self, client: u64, pane: PaneId) -> Result<Response, String> {
        self.with_pane(pane, |_, st| {
            if st
                .terminal_attachment
                .is_some_and(|(owner, _)| owner != client)
            {
                return Err("this connection does not own the terminal attachment".into());
            }
            st.subscribers.remove(&client);
            self.release_terminal(st, client);
            Ok(Response::Ok)
        })?
    }
}
