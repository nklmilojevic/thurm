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
        self.set_size(st, size);
    }

    /// `client` is gone: when the pane had its size, the last user left takes over.
    pub(super) fn forget_size(&self, st: &mut PaneState, client: u64) {
        let Some(gone) = st.wanted.remove(&client) else {
            return;
        };
        if st.terminal_attachment.is_some() || gone.size != st.info.size {
            return;
        }
        if let Some(next) = st.wanted.values().max_by_key(|w| w.used).copied() {
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
