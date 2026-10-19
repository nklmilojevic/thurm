//! `thurm events` ([`CAP_STATE_EVENTS`]) hears the state changes that come from pane output.

use super::*;

fn plain_client(env: &Env, capabilities: &[&str]) -> (Arc<Client>, Receiver<Event>) {
    let (tx, rx) = unbounded();
    let stream = thurm_client::connect_daemon(&env.socket).unwrap();
    let c = Client::from_stream(
        stream,
        "e2e-events",
        false,
        capabilities,
        move |ev| {
            let _ = tx.send(ev);
        },
        || {},
    )
    .expect("connect");
    (c, rx)
}

/// Events for `pane` up to and including its `PaneClosed`.
fn until_closed(rx: &Receiver<Event>, pane: PaneId) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let ev = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("no PaneClosed; saw {seen:?}"));
        let done = matches!(ev, Event::PaneClosed { pane: p } if p == pane);
        seen.push(ev);
        if done {
            return seen;
        }
    }
}

#[test]
fn state_events_clients_get_output_driven_pane_info_only() {
    let env = Env::new("state-events");
    let _d = env.start();
    let (c, _rx) = env.connect();
    let (_events, events_rx) = plain_client(&env, &[CAP_STATE_EVENTS]);
    let (_plain, plain_rx) = plain_client(&env, &[]);
    let pane = create(&c, &env.dir);
    // A clipboard copy (UI only, whoever asks) and then a progress report.
    c.request(Request::Input {
        pane,
        data: b"printf '\\033]52;c;c2VjcmV0\\a\\033]9;4;1;40\\a'; echo $((40 + 2))-done\r".to_vec(),
    })
    .unwrap();
    // Only the output has it, wherever the shell put it (typed before its prompt or after).
    wait_match(&c, pane, "42-done");
    c.request(Request::ClosePane { pane }).unwrap();

    let progress = |events: &[Event]| {
        events.iter().any(|e| {
            matches!(e, Event::PaneInfo(i) if i.id == pane
                && i.progress.as_ref().is_some_and(|p| p.percent == Some(40)))
        })
    };
    let private = |events: &[Event]| {
        events.iter().any(|e| {
            matches!(
                e,
                Event::ClipboardStore { .. } | Event::Notify { .. } | Event::Output { .. }
            )
        })
    };
    let seen = until_closed(&events_rx, pane);
    assert!(progress(&seen), "{seen:?}");
    assert!(!private(&seen), "{seen:?}");
    let seen = until_closed(&plain_rx, pane);
    assert!(!progress(&seen), "{seen:?}");
    assert!(!private(&seen), "{seen:?}");
}
