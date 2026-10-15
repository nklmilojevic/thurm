//! Which client's size a pane takes. A pane has one PTY size; with the phone and the Mac
//! showing it, the client that used it last wins, and gets its size back after the other one
//! had it. Clients without [`thurm_proto::CAP_ACTIVE_SIZE`] keep the last `Resize` winning.

use std::sync::atomic::AtomicU64;

use super::*;

/// Orders uses of panes across clients.
static TICK: AtomicU64 = AtomicU64::new(1);

/// The size a client shows a pane at, and when it last used the pane.
#[derive(Clone, Copy, Debug)]
pub struct Wanted {
    pub size: PaneSize,
    pub used: u64,
}

impl Daemon {
    /// A `Resize` from `client`: shown at that size now, it uses the pane.
    pub(super) fn resize_for(
        &self,
        client: &Client,
        pane: PaneId,
        size: PaneSize,
    ) -> Result<(), String> {
        let size = sanitize_size(size);
        self.with_input_pane(client.id, pane, |_, st| {
            if client.active_size.load(Ordering::Relaxed) {
                let used = TICK.fetch_add(1, Ordering::Relaxed);
                st.wanted.insert(client.id, Wanted { size, used });
            }
            st.size_owner = Some(client.id);
            log::info!(
                "pane {pane}: client {} wants {}x{}",
                client.id,
                size.cols,
                size.rows
            );
            self.set_size(st, size);
        })
    }

    /// `client` typed into, focused or answered in the pane: its size applies again.
    pub(super) fn used_by(&self, client: &Client, st: &mut PaneState) {
        if !client.active_size.load(Ordering::Relaxed) {
            return;
        }
        let Some(w) = st.wanted.get_mut(&client.id) else {
            return;
        };
        w.used = TICK.fetch_add(1, Ordering::Relaxed);
        let size = w.size;
        st.size_owner = Some(client.id);
        self.set_size(st, size);
    }

    /// `client` is gone: when the pane had its size, the last user left takes over.
    pub(super) fn forget_size(&self, st: &mut PaneState, client: u64) {
        if st.wanted.remove(&client).is_none() {
            return;
        }
        // Another client wanting the same size does not make it the owner.
        if st.terminal_attachment.is_some() || st.size_owner != Some(client) {
            log::info!(
                "pane {}: client {client} left; it did not set the current {}x{}",
                st.info.id,
                st.info.size.cols,
                st.info.size.rows
            );
            return;
        }
        let next = st
            .wanted
            .iter()
            .max_by_key(|(_, w)| w.used)
            .map(|(id, w)| (*id, *w));
        st.size_owner = next.map(|(id, _)| id);
        let next = next.map(|(_, w)| w);
        log::info!(
            "pane {}: client {client} left; size goes to {}",
            st.info.id,
            next.map_or("(nobody else wants one)".to_owned(), |n| format!(
                "{}x{}",
                n.size.cols, n.size.rows
            ))
        );
        if let Some(next) = next {
            self.set_size(st, next.size);
        }
    }

    pub(super) fn set_size(&self, st: &mut PaneState, size: PaneSize) {
        if st.info.size == size {
            return;
        }
        if let Err(e) = st.pty.resize(size) {
            log::warn!(
                "pane {}: resize to {}x{}: {e}",
                st.info.id,
                size.cols,
                size.rows
            );
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
