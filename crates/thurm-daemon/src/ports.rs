//! The ports programs in panes listen on ([`thurm_proto::CAP_PORTS`]), and the watcher that
//! reports their changes ([`Event::PortsChanged`]) to clients that asked
//! ([`thurm_proto::CAP_PORT_EVENTS`]).
//!
//! One thread looks at every pane at once: one process table and one socket listing per look.
//! It runs only while such a client is connected, looks every 10 s, and doubles the wait while
//! nothing changes, up to 5 min. A new pane, a command starting or finishing, a
//! [`thurm_proto::Request::Ports`], or any change brings it back to 10 s: the next look within
//! 10 s of the last, not at once. A client that joins gets its lists at once.

use std::collections::HashMap;
use std::sync::Arc;
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
    /// Forget what was sent (nobody listens any more): the next look sends every pane's
    /// non-empty list.
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
            let known = &self.panes[&pane];
            // Its empty list too waits for the second to pass; the pane stays known till then.
            if says_something(&known.sent)
                && let Some(at) = known.sent_at
                && now < at + PER_PANE
            {
                let when = at + PER_PANE;
                retry = Some(retry.map_or(when, |r| r.min(when)));
                continue;
            }
            let known = self.panes.remove(&pane).expect("listed");
            // Anything but an empty complete list (ports, or an incomplete look, sent or told a
            // joiner) leaves something behind: the empty complete list takes it down.
            if says_something(&known.sent) {
                events.push(Event::PortsChanged {
                    pane,
                    ports: Vec::new(),
                    complete: true,
                    initial: false,
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
                    let send = says_something(&s);
                    if send {
                        events.push(event(pane, &s, false));
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
                    if k.sent_at.is_none() && !says_something(&s) {
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
                    events.push(event(pane, &s, false));
                    k.sent = s;
                    k.sent_at = Some(now);
                }
            }
        }
        (events, retry)
    }
}

/// `pane`'s list `s`: a change, or (`initial`) what a joining client is told is there already.
/// Whether `s` tells a client anything: ports, or that part of the pane could not be read. An
/// empty complete list (no ports, all read) is what a pane nobody heard of has anyway.
fn says_something(s: &Seen) -> bool {
    !s.ports.is_empty() || !s.complete
}

fn event(pane: PaneId, s: &Seen, initial: bool) -> Event {
    Event::PortsChanged {
        pane,
        ports: s.ports.clone(),
        complete: s.complete,
        initial,
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
    /// Sends `event` to the clients that want port events, but those in `except`.
    fn send_ports(&self, event: Event, except: &[u64]);
    /// Sends `event` to client `client` alone.
    fn send_ports_to(&self, client: u64, event: Event);
    /// The look that sent `joined` their first lists (if they had any) is done: from now on they
    /// hear changes like everyone.
    fn served(&self, _joined: &[u64]) {}
    fn watcher(&self) -> &Watcher;
    /// One look at the system ([`look`]).
    fn look(&self, roots: &[(PaneId, u32)]) -> Option<HashMap<PaneId, Seen>> {
        look(roots)
    }
}

#[derive(Default)]
struct State {
    started: bool,
    poked: bool,
    /// Clients subscribed since the last look: each gets every list, on its own.
    joined: Vec<u64>,
    /// Asleep until someone listens.
    idle: bool,
}

/// What one look sends: `broadcast` to the clients that had subscribed before it (by the per-pane
/// rules, as changes), and to each of `joined` its own first lists, every pane's as seen now
/// (empty ones too: they are the joiner's whole picture), marked `initial`. A
/// client joining doesn't make the others hear a list again, nor sooner than a second after the
/// last one.
#[derive(Debug)]
pub struct Delivery {
    pub broadcast: Vec<Event>,
    pub initial: Vec<(u64, Event)>,
    pub retry: Option<Instant>,
}

pub fn deliver(
    tracker: &mut Tracker,
    now: Instant,
    seen: HashMap<PaneId, Seen>,
    joined: &[u64],
) -> Delivery {
    // Copied only for a client that joined: most looks have none.
    let mut first: Vec<(PaneId, Seen)> = seen
        .iter()
        .filter(|_| !joined.is_empty())
        .map(|(p, s)| {
            let mut s = s.clone();
            s.ports.sort();
            s.ports.dedup();
            (*p, s)
        })
        .collect();
    first.sort_unstable_by_key(|(p, _)| *p);
    let (broadcast, retry) = tracker.update(now, seen);
    let initial = joined
        .iter()
        .flat_map(|&c| first.iter().map(move |(p, s)| (c, event(*p, s, true))))
        .collect();
    Delivery {
        broadcast,
        initial,
        retry,
    }
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

    /// Whether the watcher runs and sleeps until someone listens.
    #[cfg(test)]
    pub fn idle(&self) -> bool {
        let st = self.state.lock();
        st.started && st.idle
    }

    /// Client `client` waits for its first lists again (a look left it out): the next look,
    /// soon, sends them. Only while the watcher runs.
    pub fn rejoin(&self, client: u64) {
        let mut st = self.state.lock();
        if st.started {
            st.joined.push(client);
            st.poked = true;
            self.wake.notify_all();
        }
    }

    /// Client `client`, which wants port events, connected: start the watcher if it isn't
    /// running, and send that client every pane's list.
    /// The watcher holds the daemon (which lives as long as the process) for as long as it runs.
    pub fn subscribed<H: Host>(&self, host: Arc<H>, client: u64) {
        let mut st = self.state.lock();
        st.joined.push(client);
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

/// When to look after a poke: the time already set, if sooner; else the shortest wait after the
/// last look (now, if there was none yet or it is that long ago).
fn after_poke(next: Instant, last_look: Option<Instant>, interval: Duration) -> Instant {
    match last_look {
        Some(last) => next.min(last + interval),
        None => next,
    }
}

/// Test builds: `var` as a number of milliseconds, for a step that takes a while on purpose (a
/// look, a report). Release builds never read it.
pub(crate) fn test_delay(var: &str) -> Option<Duration> {
    if !cfg!(debug_assertions) {
        return None;
    }
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
}

fn min_interval() -> Duration {
    interval_from(std::env::var("THURM_PORTS_INTERVAL_MS").ok().as_deref())
}

/// The shortest wait between looks, from `THURM_PORTS_INTERVAL_MS` (tests); never under 50 ms,
/// so a zero there doesn't make the watcher run `ps` and `lsof` back to back.
fn interval_from(ms: Option<&str>) -> Duration {
    ms.and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(MIN_INTERVAL)
        .max(Duration::from_millis(50))
}

fn run<H: Host>(h: Arc<H>) {
    let mut tracker = Tracker::default();
    let mut backoff = Backoff::new(min_interval());
    let mut next = Instant::now();
    let mut last_look: Option<Instant> = None;
    // Clients whose first look failed: served by the next one, at its time.
    let mut waiting: Vec<u64> = Vec::new();
    loop {
        let w = h.watcher();
        let joined = {
            let mut st = w.state.lock();
            loop {
                if !h.wants_ports() {
                    // Nobody listens: forget what was sent, and sleep until someone does.
                    tracker.reset();
                    st.poked = false;
                    st.idle = true;
                    w.wake.wait(&mut st);
                    continue;
                }
                st.idle = false;
                // A client that joined waits for its lists: look now.
                if !st.joined.is_empty() {
                    break;
                }
                // Something that may change ports: the next look comes within the shortest
                // wait of the last one, not at once (commands run back to back would each make
                // one).
                if st.poked {
                    st.poked = false;
                    backoff.poke();
                    next = after_poke(next, last_look, backoff.interval());
                    continue;
                }
                let now = Instant::now();
                if now >= next {
                    break;
                }
                w.wake.wait_for(&mut st, next - now);
            }
            let mut joined = std::mem::take(&mut waiting);
            joined.append(&mut st.joined);
            joined
        };
        // Tests: a look that takes a while, for what happens while it is under way.
        if let Some(delay) = test_delay("THURM_PORTS_LOOK_DELAY_MS") {
            std::thread::sleep(delay);
        }
        let roots = h.pane_roots();
        last_look = Some(Instant::now());
        let changed = match h.look(&roots) {
            Some(seen) => {
                let d = deliver(&mut tracker, Instant::now(), seen, &joined);
                let changed = !d.broadcast.is_empty() || d.retry.is_some();
                for ev in d.broadcast {
                    h.send_ports(ev, &joined);
                }
                for (client, ev) in d.initial {
                    h.send_ports_to(client, ev);
                }
                h.served(&joined);
                let retry = d.retry;
                let wait = backoff.looked(changed);
                next = retry.map_or(Instant::now() + wait, |r| r.min(Instant::now() + wait));
                changed
            }
            // A failed look sends nothing; try again at the shortest wait, the clients that
            // joined still waiting for their lists (not put back with the new joiners: those
            // are looked at once, and a look failing again would then never wait).
            None => {
                waiting = joined;
                backoff.poke();
                next = Instant::now() + backoff.interval();
                false
            }
        };
        log::trace!("ports: looked at {} panes, changed {changed}", roots.len());
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
                ..
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
    fn an_unreadable_pane_is_told_and_its_end_too() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let unread = Seen {
            ports: vec![],
            complete: false,
        };
        // Nothing found, but not all of it read: said, so no one takes its ports for gone.
        let (ev, _) = t.update(t0, HashMap::from([(1, unread.clone())]));
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(1, vec![], false)]
        );
        // A pane that listed a port, then could not be read, then went: its end is said.
        let (ev, _) = t.update(
            t0 + PER_PANE,
            HashMap::from([(1, unread.clone()), (2, seen(&[5173]))]),
        );
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(2, vec![5173], true)]
        );
        let (ev, _) = t.update(t0 + PER_PANE * 2, HashMap::from([(2, unread)]));
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(1, vec![], true), (2, vec![], false)]
        );
        let (ev, _) = t.update(t0 + PER_PANE * 3, HashMap::new());
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(2, vec![], true)]
        );
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
    fn a_pane_gone_within_the_second_says_so_after_it() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        let (ev, _) = t.update(t0, HashMap::from([(1, seen(&[3000]))]));
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(1, vec![3000], true)]
        );
        // Gone 300 ms later: held back, with a time to look again.
        let (ev, retry) = t.update(t0 + Duration::from_millis(300), HashMap::new());
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(retry, Some(t0 + PER_PANE));
        // Then its empty list, once.
        let (ev, retry) = t.update(t0 + PER_PANE, HashMap::new());
        assert_eq!(
            ev.iter().map(ports_of).collect::<Vec<_>>(),
            [(1, vec![], true)]
        );
        assert_eq!(retry, None);
        let (ev, _) = t.update(t0 + PER_PANE * 2, HashMap::new());
        assert!(ev.is_empty());
    }

    #[test]
    fn a_client_joining_gets_its_lists_without_repeating_them_to_the_others() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        // Client 1 subscribes; the first look gives it the lists.
        let d = deliver(
            &mut t,
            t0,
            HashMap::from([(1, seen(&[3000])), (2, seen(&[]))]),
            &[1],
        );
        assert_eq!(
            d.broadcast.iter().map(ports_of).collect::<Vec<_>>(),
            [(1, vec![3000], true)]
        );
        assert_eq!(
            d.initial
                .iter()
                .map(|(c, e)| (*c, ports_of(e)))
                .collect::<Vec<_>>(),
            [(1, (1, vec![3000], true)), (1, (2, vec![], true))],
            "every pane's list, the empty one too"
        );
        // Client 2 joins 300 ms later: it gets the list; nothing goes to everyone again.
        let d = deliver(
            &mut t,
            t0 + Duration::from_millis(300),
            HashMap::from([(1, seen(&[3000])), (2, seen(&[]))]),
            &[2],
        );
        assert!(d.broadcast.is_empty(), "{:?}", d.broadcast);
        assert_eq!(
            d.initial
                .iter()
                .map(|(c, e)| (*c, ports_of(e)))
                .collect::<Vec<_>>(),
            [(2, (1, vec![3000], true)), (2, (2, vec![], true))]
        );
        // And the per-pane second still holds for a change right after.
        let d = deliver(
            &mut t,
            t0 + Duration::from_millis(600),
            HashMap::from([(1, seen(&[3000, 3001]))]),
            &[],
        );
        assert!(d.broadcast.is_empty());
        assert_eq!(d.retry, Some(t0 + PER_PANE));
    }

    /// A daemon whose looks always fail, counting them.
    struct Failing {
        watcher: Watcher,
        looks: std::sync::atomic::AtomicUsize,
    }

    impl Host for Failing {
        fn wants_ports(&self) -> bool {
            true
        }
        fn pane_roots(&self) -> Vec<(PaneId, u32)> {
            vec![(1, 1)]
        }
        fn send_ports(&self, _: Event, _: &[u64]) {}
        fn send_ports_to(&self, _: u64, _: Event) {}
        fn watcher(&self) -> &Watcher {
            &self.watcher
        }
        fn look(&self, _: &[(PaneId, u32)]) -> Option<HashMap<PaneId, Seen>> {
            self.looks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            None
        }
    }

    #[test]
    fn a_failing_look_is_not_retried_at_once_for_a_joiner() {
        let h = Arc::new(Failing {
            watcher: Watcher::default(),
            looks: Default::default(),
        });
        h.watcher().subscribed(h.clone(), 7);
        std::thread::sleep(Duration::from_millis(300));
        // One look for the joiner, then the shortest wait (10 s) before the next.
        let looks = h.looks.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(looks, 1, "{looks} looks in 300 ms");
    }

    #[test]
    fn a_poke_brings_the_next_look_within_the_shortest_wait_not_now() {
        let t0 = Instant::now();
        let min = Duration::from_secs(10);
        // Waiting 5 min after a look at t0: a poke brings it to t0 + 10 s.
        assert_eq!(
            after_poke(t0 + Duration::from_secs(300), Some(t0), min),
            t0 + min
        );
        // Already sooner than that: unchanged.
        let soon = t0 + Duration::from_secs(3);
        assert_eq!(after_poke(soon, Some(t0), min), soon);
        // Many pokes in a row (commands back to back) never make it sooner than t0 + 10 s.
        let mut next = t0 + Duration::from_secs(300);
        for _ in 0..100 {
            next = after_poke(next, Some(t0), min);
        }
        assert_eq!(next, t0 + min);
        // No look yet: the first one is not put off.
        assert_eq!(after_poke(t0, None, min), t0);
    }

    #[test]
    fn the_watch_never_spins() {
        assert_eq!(interval_from(None), MIN_INTERVAL);
        assert_eq!(interval_from(Some("200")), Duration::from_millis(200));
        assert_eq!(interval_from(Some("0")), Duration::from_millis(50));
        assert_eq!(interval_from(Some("junk")), MIN_INTERVAL);
    }

    fn initial_of(ev: &Event) -> bool {
        match ev {
            Event::PortsChanged { initial, .. } => *initial,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn only_a_joiners_first_lists_are_initial() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        // Client 1 joins: its lists are what was there; the broadcast to others is a change.
        let d = deliver(
            &mut t,
            t0,
            HashMap::from([(1, seen(&[3000])), (2, seen(&[4000]))]),
            &[1],
        );
        assert!(
            d.initial.iter().all(|(_, e)| initial_of(e)),
            "{:?}",
            d.initial
        );
        assert_eq!(d.initial.len(), 2);
        assert!(
            d.broadcast.iter().all(|e| !initial_of(e)),
            "{:?}",
            d.broadcast
        );
        // Client 2 joins later: initial lists for it alone, nothing to the others.
        let d = deliver(
            &mut t,
            t0 + PER_PANE,
            HashMap::from([(1, seen(&[3000])), (2, seen(&[4000]))]),
            &[2],
        );
        assert!(d.broadcast.is_empty());
        assert!(d.initial.iter().all(|(c, e)| *c == 2 && initial_of(e)));
        // Then a change, and a pane that goes: never initial, for anyone.
        let d = deliver(
            &mut t,
            t0 + PER_PANE * 2,
            HashMap::from([(1, seen(&[3000, 3001]))]),
            &[],
        );
        assert!(d.initial.is_empty());
        assert_eq!(d.broadcast.len(), 2);
        assert!(
            d.broadcast.iter().all(|e| !initial_of(e)),
            "{:?}",
            d.broadcast
        );
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
