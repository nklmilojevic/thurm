//! The ports programs in panes listen on ([`thurm_proto::CAP_PORTS`]), and the watcher that
//! reports their changes ([`Event::PortsChanged`]) to clients that asked
//! ([`thurm_proto::CAP_PORT_EVENTS`]).
//!
//! One thread looks at every pane at once: one process table and one socket listing per look.
//! It runs only while such a client is connected, looks every 10 s, and doubles the wait while
//! nothing changes, up to 5 min. A new pane, a command starting or finishing, a
//! [`thurm_proto::Request::Ports`], or any change brings it back to 10 s.

use std::collections::HashMap;
use std::sync::Weak;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use thurm_proto::{Event, PaneId, PanePorts, PortEntry};

use crate::procinfo;

/// The first wait between looks, and the wait after anything changed.
pub const MIN_INTERVAL: Duration = Duration::from_secs(10);
/// The longest wait while nothing changes.
pub const MAX_INTERVAL: Duration = Duration::from_secs(300);
/// At most one event a second per pane.
pub const PER_PANE: Duration = Duration::from_secs(1);

/// The wait before the next look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    interval: Duration,
    min: Duration,
}

impl Backoff {
    pub fn new(min: Duration) -> Backoff {
        Backoff { interval: min, min }
    }

    /// After a look: the wait before the next. The shortest if anything changed; otherwise the
    /// last wait, and twice that next time.
    pub fn looked(&mut self, changed: bool) -> Duration {
        if changed {
            self.interval = self.min;
            return self.min;
        }
        let wait = self.interval;
        self.interval = (self.interval * 2).min(MAX_INTERVAL.max(self.min));
        wait
    }

    /// Something happened that may have changed ports: look again soon.
    pub fn poke(&mut self) {
        self.interval = self.min;
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }
}

/// What one look found for a pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub ports: Vec<PortEntry>,
    pub complete: bool,
}

#[derive(Debug)]
struct Known {
    /// The list last sent (or, for a pane that never had ports, the empty list not sent).
    sent: Seen,
    sent_at: Option<Instant>,
}

/// The lists sent so far, and what to send after a look.
#[derive(Debug, Default)]
pub struct Tracker {
    panes: HashMap<PaneId, Known>,
}

impl Tracker {
    /// Forget what was sent: the next look sends every pane's non-empty list (a new client
    /// subscribed, and needs them all).
    pub fn reset(&mut self) {
        self.panes.clear();
    }

    /// The events a look that found `now_seen` (every live pane) at `now` makes, and when to
    /// look again for a change held back by the per-pane rate. Panes missing from `now_seen`
    /// have exited or closed: one empty list for each that had ports.
    pub fn update(
        &mut self,
        now: Instant,
        now_seen: HashMap<PaneId, Seen>,
    ) -> (Vec<Event>, Option<Instant>) {
        let mut events = Vec::new();
        let mut retry: Option<Instant> = None;
        let mut gone: Vec<PaneId> = self
            .panes
            .keys()
            .filter(|p| !now_seen.contains_key(p))
            .copied()
            .collect();
        gone.sort_unstable();
        for pane in gone {
            let known = self.panes.remove(&pane).expect("listed");
            if !known.sent.ports.is_empty() && known.sent_at.is_some() {
                events.push(Event::PortsChanged {
                    pane,
                    ports: Vec::new(),
                    complete: true,
                });
            }
        }
        let mut seen: Vec<(PaneId, Seen)> = now_seen.into_iter().collect();
        seen.sort_unstable_by_key(|(p, _)| *p);
        for (pane, mut s) in seen {
            s.ports.sort();
            s.ports.dedup();
            match self.panes.get_mut(&pane) {
                None => {
                    let send = !s.ports.is_empty();
                    if send {
                        events.push(event(pane, &s));
                    }
                    self.panes.insert(
                        pane,
                        Known {
                            sent: s,
                            sent_at: send.then_some(now),
                        },
                    );
                }
                Some(k) if k.sent == s => {}
                Some(k) => {
                    // Never sent anything, and still nothing to say.
                    if k.sent_at.is_none() && s.ports.is_empty() {
                        k.sent = s;
                        continue;
                    }
                    if let Some(at) = k.sent_at
                        && now < at + PER_PANE
                    {
                        let when = at + PER_PANE;
                        retry = Some(retry.map_or(when, |r| r.min(when)));
                        continue;
                    }
                    events.push(event(pane, &s));
                    k.sent = s;
                    k.sent_at = Some(now);
                }
            }
        }
        (events, retry)
    }
}

fn event(pane: PaneId, s: &Seen) -> Event {
    Event::PortsChanged {
        pane,
        ports: s.ports.clone(),
        complete: s.complete,
    }
}

/// Every pane's ports from one look at the system: `roots` are the panes' shell pids. `None`
/// when the look failed (no process table, no socket listing).
pub fn look(roots: &[(PaneId, u32)]) -> Option<HashMap<PaneId, Seen>> {
    let table = procinfo::process_table();
    if table.is_empty() {
        return None;
    }
    let trees: Vec<(PaneId, Vec<procinfo::ProcRow>)> = roots
        .iter()
        .map(|&(id, pid)| (id, procinfo::descendants(&table, pid)))
        .collect();
    let pids: Vec<u32> = trees
        .iter()
        .flat_map(|(_, t)| t.iter().map(|p| p.0))
        .collect();
    let found = procinfo::listeners(&pids)?;
    Some(
        trees
            .into_iter()
            .map(|(pane, tree)| (pane, seen_in(&tree, &found)))
            .collect(),
    )
}

fn seen_in(tree: &[procinfo::ProcRow], found: &procinfo::Listening) -> Seen {
    let mut ports = Vec::new();
    for (pid, _, command) in tree {
        for l in found.by_pid.get(pid).into_iter().flatten() {
            ports.push(PortEntry {
                port: l.port,
                address: l.address.clone(),
                pid: *pid,
                command: command.clone(),
            });
        }
    }
    ports.sort();
    ports.dedup();
    Seen {
        ports,
        complete: !tree.iter().any(|p| found.unread.contains(&p.0)),
    }
}

/// `look`'s lists as [`thurm_proto::Response::Ports`] has them.
pub fn as_response(seen: HashMap<PaneId, Seen>) -> Vec<PanePorts> {
    let mut out: Vec<PanePorts> = seen
        .into_iter()
        .map(|(pane, s)| PanePorts {
            pane,
            ports: s.ports,
            complete: s.complete,
        })
        .collect();
    out.sort_by_key(|p| p.pane);
    out
}

/// What the daemon gives the watcher.
pub trait Host: Send + Sync + 'static {
    /// Whether a connected client wants port events.
    fn wants_ports(&self) -> bool;
    /// Every live pane and its shell's pid.
    fn pane_roots(&self) -> Vec<(PaneId, u32)>;
    /// Sends `event` to the clients that want port events.
    fn send_ports(&self, event: Event);
    fn watcher(&self) -> &Watcher;
}

#[derive(Default)]
struct State {
    started: bool,
    poked: bool,
    /// A client subscribed: send it every list.
    resync: bool,
}

/// The watcher's handle on the daemon.
#[derive(Default)]
pub struct Watcher {
    state: Mutex<State>,
    wake: Condvar,
}

impl Watcher {
    /// Something that may change ports happened: look soon (if the watcher runs).
    pub fn poke(&self) {
        let mut st = self.state.lock();
        if st.started {
            st.poked = true;
            self.wake.notify_all();
        }
    }

    /// A client that wants port events connected: start the watcher if it isn't running, and
    /// send every pane's list again.
    pub fn subscribed<H: Host>(&self, host: Weak<H>) {
        let mut st = self.state.lock();
        st.resync = true;
        st.poked = true;
        self.wake.notify_all();
        if st.started {
            return;
        }
        st.started = true;
        let spawned = std::thread::Builder::new()
            .name("ports".into())
            .spawn(move || run(host));
        if let Err(e) = spawned {
            log::warn!("cannot watch ports: {e}");
            st.started = false;
        }
    }
}

fn min_interval() -> Duration {
    std::env::var("THURM_PORTS_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(MIN_INTERVAL)
}

fn run<H: Host>(host: Weak<H>) {
    let mut tracker = Tracker::default();
    let mut backoff = Backoff::new(min_interval());
    let mut next = Instant::now();
    loop {
        let Some(h) = host.upgrade() else { return };
        let w = h.watcher();
        {
            let mut st = w.state.lock();
            loop {
                if !h.wants_ports() {
                    // Nobody listens: forget what was sent, and sleep until someone does.
                    tracker.reset();
                    st.poked = false;
                    w.wake.wait(&mut st);
                    continue;
                }
                if st.poked {
                    st.poked = false;
                    backoff.poke();
                    break;
                }
                let now = Instant::now();
                if now >= next {
                    break;
                }
                w.wake.wait_for(&mut st, next - now);
            }
            if std::mem::take(&mut st.resync) {
                tracker.reset();
            }
        }
        let roots = h.pane_roots();
        let changed = match look(&roots) {
            Some(seen) => {
                let (events, retry) = tracker.update(Instant::now(), seen);
                let changed = !events.is_empty() || retry.is_some();
                for ev in events {
                    h.send_ports(ev);
                }
                let wait = backoff.looked(changed);
                next = retry.map_or(Instant::now() + wait, |r| r.min(Instant::now() + wait));
                changed
            }
            // A failed look sends nothing; try again at the shortest wait.
            None => {
                backoff.poke();
                next = Instant::now() + backoff.interval();
                false
            }
        };
        log::trace!("ports: looked at {} panes, changed {changed}", roots.len());
        drop(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(port: u16, pid: u32) -> PortEntry {
        PortEntry {
            port,
            address: "127.0.0.1".into(),
            pid,
            command: "vite".into(),
        }
    }

    fn seen(ports: &[u16]) -> Seen {
        Seen {
            ports: ports.iter().map(|&p| entry(p, 7)).collect(),
            complete: true,
        }
    }

    fn ports_of(ev: &Event) -> (PaneId, Vec<u16>, bool) {
        match ev {
            Event::PortsChanged {
                pane,
                ports,
                complete,
            } => (*pane, ports.iter().map(|p| p.port).collect(), *complete),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn backoff_doubles_while_nothing_changes() {
        let mut b = Backoff::new(MIN_INTERVAL);
        let waits: Vec<u64> = (0..7).map(|_| b.looked(false).as_secs()).collect();
        assert_eq!(waits, [10, 20, 40, 80, 160, 300, 300]);
        // A change: straight back to the shortest wait, and doubling again from there.
        assert_eq!(b.looked(true).as_secs(), 10);
        assert_eq!(b.looked(false).as_secs(), 10);
        assert_eq!(b.looked(false).as_secs(), 20);
        // A poke (a new pane, a command, a Ports request) too.
        b.looked(false);
        b.looked(false);
        b.poke();
        assert_eq!(b.looked(false).as_secs(), 10);
    }

    #[test]
    fn whole_lists_and_quiet_empty_panes() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        // A pane whose first list is empty says nothing.
        let (ev, _) = t.update(t0, HashMap::from([(1, seen(&[])), (2, seen(&[5173]))]));
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(2, vec![5173], true)]
        );
        // Nothing changed: nothing sent.
        let (ev, _) = t.update(
            t0 + Duration::from_secs(10),
            HashMap::from([(1, seen(&[])), (2, seen(&[5173]))]),
        );
        assert!(ev.is_empty());
        // A new port: the whole list again, sorted.
        let (ev, _) = t.update(
            t0 + Duration::from_secs(20),
            HashMap::from([(1, seen(&[8080])), (2, seen(&[9000, 5173]))]),
        );
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(1, vec![8080], true), (2, vec![5173, 9000], true)]
        );
        // A partial read is news too.
        let (ev, _) = t.update(
            t0 + Duration::from_secs(30),
            HashMap::from([
                (
                    1,
                    Seen {
                        complete: false,
                        ..seen(&[8080])
                    },
                ),
                (2, seen(&[5173, 9000])),
            ]),
        );
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(1, vec![8080], false)]
        );
        // Pane 2 exits: one empty list; pane 1 stops listening: an empty list as well.
        let (ev, _) = t.update(
            t0 + Duration::from_secs(40),
            HashMap::from([(1, seen(&[]))]),
        );
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(2, vec![], true), (1, vec![], true)]
        );
        // A pane that never listed anything leaves quietly.
        let mut t = Tracker::default();
        t.update(t0, HashMap::from([(3, seen(&[]))]));
        let (ev, _) = t.update(t0 + Duration::from_secs(10), HashMap::new());
        assert!(ev.is_empty());
    }

    #[test]
    fn at_most_one_event_a_second_per_pane() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.update(t0, HashMap::from([(1, seen(&[3000])), (2, seen(&[4000]))]));
        // Within the second: held back, with a time to look again.
        let (ev, retry) = t.update(
            t0 + Duration::from_millis(400),
            HashMap::from([(1, seen(&[3000, 3001])), (2, seen(&[4000]))]),
        );
        assert!(ev.is_empty());
        assert_eq!(retry, Some(t0 + PER_PANE));
        // Once it has passed, the newest whole list.
        let (ev, retry) = t.update(
            t0 + PER_PANE,
            HashMap::from([(1, seen(&[3000, 3001, 3002])), (2, seen(&[4000]))]),
        );
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(1, vec![3000, 3001, 3002], true)]
        );
        assert_eq!(retry, None);
    }

    #[test]
    fn reset_sends_every_list_again() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.update(t0, HashMap::from([(1, seen(&[3000])), (2, seen(&[]))]));
        t.reset();
        let (ev, _) = t.update(t0, HashMap::from([(1, seen(&[3000])), (2, seen(&[]))]));
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(1, vec![3000], true)]
        );
    }

    #[test]
    fn a_look_at_this_process() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id();
        let seen = look(&[(9, me)]).expect("a look");
        assert!(
            seen[&9].ports.iter().any(|p| p.port == port && p.pid == me),
            "{seen:?}"
        );
        assert!(seen[&9].complete);
    }
}
