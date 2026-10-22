//! Which panes had keyboard focus in a UI, most recent first: what `thurm focused` answers when
//! an agent is asked about "the pane I'm looking at".

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use thurm_proto::{FocusRecord, PaneId};

/// How many panes it remembers.
const KEEP: usize = 16;

#[derive(Default)]
pub struct FocusLog {
    /// Most recent first.
    recent: VecDeque<(PaneId, Instant)>,
    /// The pane each client has focused now.
    now: HashMap<u64, PaneId>,
}

impl FocusLog {
    /// A UI's `Focus` request.
    pub fn focus(&mut self, client: u64, pane: PaneId, focused: bool, at: Instant) {
        if focused {
            self.recent.retain(|(p, _)| *p != pane);
            self.recent.push_front((pane, at));
            self.recent.truncate(KEEP);
            self.now.insert(client, pane);
        } else if self.now.get(&client) == Some(&pane) {
            self.now.remove(&client);
        }
    }

    pub fn client_gone(&mut self, client: u64) {
        self.now.remove(&client);
    }

    /// The panes that still exist, most recent first.
    pub fn history(&self, exists: impl Fn(PaneId) -> bool, at: Instant) -> Vec<FocusRecord> {
        self.recent
            .iter()
            .filter(|(p, _)| exists(*p))
            .map(|&(pane, since)| FocusRecord {
                pane,
                focused: self.now.values().any(|p| *p == pane),
                since_ms: at.saturating_duration_since(since).as_millis() as u64,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn most_recent_first_and_focused_now() {
        let t = Instant::now();
        let mut log = FocusLog::default();
        log.focus(1, 10, true, t);
        log.focus(1, 10, false, t);
        log.focus(1, 11, true, t + Duration::from_secs(1));
        // Refocused: it moves to the front instead of appearing twice.
        log.focus(1, 10, true, t + Duration::from_secs(2));
        let h = log.history(|_| true, t + Duration::from_secs(3));
        assert_eq!(
            h,
            vec![
                FocusRecord {
                    pane: 10,
                    focused: true,
                    since_ms: 1000
                },
                FocusRecord {
                    pane: 11,
                    focused: false,
                    since_ms: 2000
                },
            ]
        );
    }

    #[test]
    fn losing_focus_and_leaving_clear_focused_now() {
        let t = Instant::now();
        let mut log = FocusLog::default();
        log.focus(1, 10, true, t);
        log.focus(2, 20, true, t);
        // A late blur of a pane the client already left behind changes nothing.
        log.focus(1, 30, true, t);
        log.focus(1, 10, false, t);
        log.client_gone(2);
        let h = log.history(|p| p != 10, t);
        assert_eq!(
            h.iter().map(|r| (r.pane, r.focused)).collect::<Vec<_>>(),
            vec![(30, true), (20, false)]
        );
    }

    #[test]
    fn keeps_a_bounded_history() {
        let t = Instant::now();
        let mut log = FocusLog::default();
        for p in 0..40 {
            log.focus(1, p, true, t);
        }
        let h = log.history(|_| true, t);
        assert_eq!(h.len(), KEEP);
        assert_eq!(h[0].pane, 39);
    }
}
