//! JSON Lines for state changes. Terminal output is not part of this stream.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};

use clap::ValueEnum;
use thurm_client::Client;
use thurm_proto::{Event, PaneId};

/// The event types the stream carries, named as in its JSON.
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum Kind {
    #[value(name = "PaneInfo", alias = "pane_info")]
    PaneInfo,
    #[value(name = "PaneExited", alias = "pane_exited")]
    PaneExited,
    #[value(name = "PaneClosed", alias = "pane_closed")]
    PaneClosed,
    #[value(name = "ConfigReloaded", alias = "config_reloaded")]
    ConfigReloaded,
    #[value(name = "LayoutChanged", alias = "layout_changed")]
    LayoutChanged,
}

/// Events not yet written: at most this many, besides one `PaneInfo` per pane.
const BUFFER: usize = 256;

#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
    lost: bool,
    closed: bool,
}

impl Queue {
    /// A pane's newer `PaneInfo` replaces the one still waiting, at the end: a busy pane
    /// (progress, prompts) shows its latest state instead of overflowing the buffer.
    fn push(&mut self, event: Event) {
        if let Event::PaneInfo(info) = &event {
            self.events
                .retain(|e| !matches!(e, Event::PaneInfo(old) if old.id == info.id));
        }
        if self.events.len() >= BUFFER {
            self.lost = true;
        } else {
            self.events.push_back(event);
        }
    }
}

pub fn run(remote: Option<&str>, pane: Option<PaneId>, kinds: &[Kind]) -> super::R {
    if let Some(name) = remote {
        drop(super::remote::connect_remote(name)?);
    }
    let queue = Arc::new((Mutex::new(Queue::default()), Condvar::new()));
    let incoming = queue.clone();
    let closed = queue.clone();
    let kinds = kinds.to_vec();
    let socket = remote
        .map(thurm_config::remote_socket_path)
        .unwrap_or_else(thurm_config::socket_path);
    let stream = thurm_client::connect_daemon(&socket)
        .map_err(|e| format!("cannot connect to {}: {e}", socket.display()))?;
    // An older daemon ignores the capability and sends only the PaneInfo changes it sends
    // every client.
    let client = Client::from_stream(
        stream,
        "thurm-events",
        false,
        &[thurm_proto::CAP_STATE_EVENTS],
        move |event| {
            if selected(&event, pane, &kinds) {
                let (queue, ready) = &*incoming;
                queue.lock().unwrap().push(event);
                ready.notify_one();
            }
        },
        move || {
            let (queue, ready) = &*closed;
            queue.lock().unwrap().closed = true;
            ready.notify_one();
        },
    )?;
    // The connection receives metadata events without a terminal subscription.
    let _client = client;
    let mut out = std::io::stdout().lock();
    let (queue, ready) = &*queue;
    loop {
        let event = {
            let mut q = queue.lock().unwrap();
            while q.events.is_empty() && !q.lost && !q.closed {
                q = ready.wait(q).unwrap();
            }
            if q.lost {
                return Err("event buffer overflow; reconnect and read current state".into());
            }
            match q.events.pop_front() {
                Some(event) => event,
                None => {
                    return Err("event connection closed; reconnect and read current state".into());
                }
            }
        };
        let line = thurm_proto::to_json(&event);
        if let Err(e) = writeln!(out, "{line}").and_then(|_| out.flush()) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(std::process::ExitCode::SUCCESS);
            }
            return Err(e.into());
        }
    }
}

/// Filtered here, not in the daemon: the daemon sends every event type to the connection, and an
/// older daemon keeps working.
fn selected(event: &Event, pane: Option<PaneId>, kinds: &[Kind]) -> bool {
    let (kind, id) = match event {
        Event::PaneInfo(info) => (Kind::PaneInfo, Some(info.id)),
        Event::PaneExited { pane, .. } => (Kind::PaneExited, Some(*pane)),
        Event::PaneClosed { pane } => (Kind::PaneClosed, Some(*pane)),
        Event::ConfigReloaded => (Kind::ConfigReloaded, None),
        Event::LayoutChanged => (Kind::LayoutChanged, None),
        _ => return false,
    };
    let pane_ok = match (pane, id) {
        (None, _) => true,
        (Some(wanted), Some(id)) => wanted == id,
        (Some(_), None) => false,
    };
    pane_ok && (kinds.is_empty() || kinds.contains(&kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_panes_and_excludes_terminal_data() {
        assert!(selected(&Event::PaneClosed { pane: 4 }, Some(4), &[]));
        assert!(!selected(&Event::PaneClosed { pane: 4 }, Some(5), &[]));
        assert!(selected(&Event::LayoutChanged, None, &[]));
        assert!(!selected(&Event::LayoutChanged, Some(4), &[]));
        assert!(!selected(
            &Event::Output {
                pane: 4,
                data: b"private output".to_vec()
            },
            None,
            &[]
        ));
    }

    #[test]
    fn filters_event_types() {
        let kinds = [Kind::PaneClosed, Kind::LayoutChanged];
        assert!(selected(&Event::PaneClosed { pane: 4 }, None, &kinds));
        assert!(selected(&Event::LayoutChanged, None, &kinds));
        assert!(!selected(&Event::ConfigReloaded, None, &kinds));
        assert!(!selected(
            &Event::PaneExited {
                pane: 4,
                code: Some(0)
            },
            None,
            &kinds
        ));
        assert!(!selected(&Event::PaneClosed { pane: 4 }, Some(5), &kinds));
        assert!(!selected(&Event::Bell { pane: 4 }, None, &[Kind::PaneInfo]));
    }

    fn info(id: PaneId, at_prompt: bool) -> Event {
        Event::PaneInfo(thurm_proto::PaneInfo {
            id,
            at_prompt,
            ..Default::default()
        })
    }

    #[test]
    fn queue_keeps_each_panes_latest_info_in_order() {
        let mut q = Queue::default();
        q.push(info(1, false));
        q.push(info(2, false));
        q.push(Event::PaneExited {
            pane: 1,
            code: None,
        });
        q.push(info(1, true));
        let ids: Vec<_> = q
            .events
            .iter()
            .map(|e| match e {
                Event::PaneInfo(i) => format!("info {} {}", i.id, i.at_prompt),
                Event::PaneExited { pane, .. } => format!("exited {pane}"),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ids, ["info 2 false", "exited 1", "info 1 true"]);
        for _ in 0..10_000 {
            q.push(info(3, true));
        }
        assert!(!q.lost);
        for _ in 0..BUFFER {
            q.push(Event::LayoutChanged);
        }
        assert!(q.lost);
    }

    #[test]
    fn type_names_match_the_json() {
        for (kind, event) in [
            (Kind::PaneInfo, Event::PaneInfo(Default::default())),
            (
                Kind::PaneExited,
                Event::PaneExited {
                    pane: 1,
                    code: None,
                },
            ),
            (Kind::PaneClosed, Event::PaneClosed { pane: 1 }),
            (Kind::ConfigReloaded, Event::ConfigReloaded),
            (Kind::LayoutChanged, Event::LayoutChanged),
        ] {
            let json: serde_json::Value =
                serde_json::from_str(&thurm_proto::to_json(&event)).unwrap();
            let name = kind.to_possible_value().unwrap();
            let tag = match &json {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Object(o) => o.keys().next().unwrap().clone(),
                _ => unreachable!(),
            };
            assert_eq!(name.get_name(), tag);
            assert!(name.matches(&tag.to_lowercase(), true));
        }
        for alias in ["pane_info", "layout_changed", "panecLOSED"] {
            assert!(Kind::from_str(alias, true).is_ok(), "{alias}");
        }
        assert!(Kind::from_str("Output", true).is_err());
    }
}
