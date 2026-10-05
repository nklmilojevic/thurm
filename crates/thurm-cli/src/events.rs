//! JSON Lines for state changes. Terminal output is not part of this stream.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::Duration;

use thurm_client::{Client, ConnectOptions};
use thurm_proto::{Event, PaneId};

pub fn run(remote: Option<&str>, pane: Option<PaneId>) -> super::R {
    if let Some(name) = remote {
        drop(super::remote::connect_remote(name)?);
    }
    let (tx, rx) = sync_channel(256);
    let lost = Arc::new(AtomicBool::new(false));
    let disconnected = Arc::new(AtomicBool::new(false));
    let overflow = lost.clone();
    let closed = disconnected.clone();
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
            if selected(&event, pane) && tx.try_send(event).is_err() {
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

fn selected(event: &Event, pane: Option<PaneId>) -> bool {
    let id = match event {
        Event::PaneInfo(info) => info.id,
        Event::PaneExited { pane, .. } | Event::PaneClosed { pane } => *pane,
        Event::ConfigReloaded | Event::LayoutChanged => return pane.is_none(),
        _ => return false,
    };
    pane.is_none_or(|wanted| wanted == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_panes_and_excludes_terminal_data() {
        assert!(selected(&Event::PaneClosed { pane: 4 }, Some(4)));
        assert!(!selected(&Event::PaneClosed { pane: 4 }, Some(5)));
        assert!(selected(&Event::LayoutChanged, None));
        assert!(!selected(&Event::LayoutChanged, Some(4)));
        assert!(!selected(
            &Event::Output {
                pane: 4,
                data: b"private output".to_vec()
            },
            None
        ));
    }
}
