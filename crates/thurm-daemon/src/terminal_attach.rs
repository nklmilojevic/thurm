//! Exclusive terminal attachment, released when its connection closes.

use super::*;

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
            if st.pending_input.is_some() {
                return Err("cannot attach while pane startup input is pending".into());
            }
            let original = st.info.size;
            let size = sanitize_size(size);
            st.pty
                .resize(size)
                .map_err(|e| format!("cannot resize pane: {e}"))?;
            st.terminal_attachment = Some((client.id, original));
            st.term.resize(size);
            st.info.size = size;
            self.send_subscribers(st, Event::Resized { pane, size });
            let state = st.term.serialize_state();
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
