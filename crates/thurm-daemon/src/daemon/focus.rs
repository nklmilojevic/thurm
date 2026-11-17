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
    /// The client that focused each pane last.
    last: HashMap<PaneId, u64>,
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

    /// The clients with a pane focused now.
    pub fn focused_clients(&self) -> Vec<u64> {
        self.now.keys().copied().collect()
    }

    pub fn client_gone(&mut self, client: u64) {
        self.now.remove(&client);
        self.last.retain(|_, c| *c != client);
    }

    /// `client`, one whose clipboard a tool in `pane` may use, focused it: the one asked now.
    pub fn shown_by(&mut self, client: u64, pane: PaneId) {
        self.last.insert(pane, client);
    }

    /// `pane` is gone: forget who focused it.
    pub fn pane_closed(&mut self, pane: PaneId) {
        self.last.remove(&pane);
    }

    /// The client that focused `pane` last of those [`Self::shown_by`] names, if it is still
    /// connected.
    pub fn last_client(&self, pane: PaneId) -> Option<u64> {
        self.last.get(&pane).copied()
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

    #[test]
    fn last_client_per_pane() {
        let t = Instant::now();
        let mut log = FocusLog::default();
        log.shown_by(1, 10);
        log.shown_by(2, 10);
        log.focus(2, 10, true, t);
        log.focus(2, 10, false, t);
        assert_eq!(
            log.last_client(10),
            Some(2),
            "unfocusing keeps who looked last"
        );
        assert_eq!(log.last_client(11), None);
        log.focus(3, 10, true, t);
        assert_eq!(
            log.last_client(10),
            Some(2),
            "a focus not named by shown_by (a phone's) leaves it"
        );
        log.client_gone(2);
        assert_eq!(log.last_client(10), None);
    }

    #[test]
    fn closed_panes_are_forgotten() {
        let mut log = FocusLog::default();
        for pane in 0..1000 {
            log.shown_by(1, pane);
            log.pane_closed(pane);
        }
        assert!(
            log.last.is_empty(),
            "a connected client's closed panes add nothing"
        );
        log.shown_by(1, 5);
        assert_eq!(log.last_client(5), Some(1));
    }
}
