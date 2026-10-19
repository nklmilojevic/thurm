//! JSON Lines for state changes. Terminal output is not part of this stream.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::Duration;

use clap::ValueEnum;
use thurm_client::{Client, ConnectOptions};
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

pub fn run(remote: Option<&str>, pane: Option<PaneId>, kinds: &[Kind]) -> super::R {
    if let Some(name) = remote {
        drop(super::remote::connect_remote(name)?);
    }
    let (tx, rx) = sync_channel(256);
    let lost = Arc::new(AtomicBool::new(false));
    let disconnected = Arc::new(AtomicBool::new(false));
    let overflow = lost.clone();
    let closed = disconnected.clone();
    let kinds = kinds.to_vec();
    let socket = remote
        .map(thurm_config::remote_socket_path)
        .unwrap_or_else(thurm_config::socket_path);
    let client = Client::connect(
        ConnectOptions {
            socket,
            client_name: "thurm-events",
            ..Default::default()
        },
        move |event| {
            if selected(&event, pane, &kinds) && tx.try_send(event).is_err() {
                overflow.store(true, Ordering::Release);
            }
        },
        move || {
            closed.store(true, Ordering::Release);
        },
    )?;
    // The connection receives metadata events without a terminal subscription.
    let _client = client;
    let mut out = std::io::stdout().lock();
    loop {
        if lost.load(Ordering::Acquire) {
            return Err("event buffer overflow; reconnect and read current state".into());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                let line = thurm_proto::to_json(&event);
                if let Err(e) = writeln!(out, "{line}").and_then(|_| out.flush()) {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        return Ok(std::process::ExitCode::SUCCESS);
                    }
                    return Err(e.into());
                }
            }
            Err(RecvTimeoutError::Timeout) if !disconnected.load(Ordering::Acquire) => {}
            Err(_) => {
                return Err("event connection closed; reconnect and read current state".into());
            }
        }
    }
}

/// Filtered here, not in the daemon: every connection already receives these events, and an
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
