use super::*;

fn caller(env: &Env) -> Arc<Client> {
    Client::connect(
        ConnectOptions {
            socket: env.socket.clone(),
            spawn_daemon: None,
            client_name: "layout-caller",
            ui: false,
        },
        |_| {},
        || {},
    )
    .unwrap()
}

fn apply(
    client: Arc<Client>,
    pane: PaneId,
    timeout_ms: u64,
) -> std::sync::mpsc::Receiver<Result<Response, thurm_client::ClientError>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let tab = TabLayout {
            title: Some("test layout".into()),
            root: LayoutNode::Pane {
                id: pane,
                host: None,
            },
            focused: pane,
            zoomed: None,
            handoff: None,
        };
        tx.send(client.request(Request::ApplyLayout {
            json: serde_json::to_string(&tab).unwrap(),
            timeout_ms,
        }))
        .unwrap();
    });
    rx
}

fn next_layout(events: &Receiver<Event>) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if let Event::Ui(UiCommand::OpenLayout { request_id, .. }) = event {
            return request_id;
        }
    }
}

fn outcome(
    result: std::sync::mpsc::Receiver<Result<Response, thurm_client::ClientError>>,
) -> LayoutResult {
    let Response::LayoutResult(result) = result
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap()
    else {
        panic!("expected a layout result");
    };
    result
}

#[test]
fn layout_waits_for_the_selected_ui_and_propagates_rejection() {
    let env = Env::new("layout-confirm");
    let _daemon = env.start();
    let (ui, events) = env.connect();
    let (other_ui, other_events) = env.connect();
    let client = caller(&env);
    let pane = create(&client, &env.dir);
    for error in [None, Some("could not decode the layout".to_owned())] {
        let result = apply(client.clone(), pane, 5000);
        let request_id = next_layout(&events);
        assert!(result.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(
            !other_events
                .try_iter()
                .any(|e| matches!(e, Event::Ui(UiCommand::OpenLayout { .. })))
        );
        assert!(
            other_ui
                .request(Request::LayoutApplied {
                    request_id,
                    error: None
                })
                .is_err()
        );
        assert!(
            other_ui
                .request(Request::CommitLayout { request_id })
                .is_err()
        );
        if error.is_none() {
            assert!(
                ui.request(Request::LayoutApplied {
                    request_id,
                    error: None
                })
                .is_err()
            );
            ui.request(Request::CommitLayout { request_id }).unwrap();
        }
        ui.request(Request::LayoutApplied {
            request_id,
            error: error.clone(),
        })
        .unwrap();
        let result = outcome(result);
        match error {
            None => assert!(result.committed && result.error.is_none()),
            Some(error) => assert!(!result.committed && result.error == Some(error)),
        }
        assert!(
            ui.request(Request::LayoutApplied {
                request_id,
                error: None
            })
            .is_err()
        );
    }
}

#[test]
fn layout_timeout_rejects_late_confirmation() {
    let env = Env::new("layout-timeout");
    let _daemon = env.start();
    let (ui, events) = env.connect();
    let client = caller(&env);
    let pane = create(&client, &env.dir);
    let result = apply(client, pane, 100);
    let request_id = next_layout(&events);
    let result = outcome(result);
    assert!(!result.committed);
    assert!(result.error.unwrap().contains("timed out"));
    assert!(ui.request(Request::CommitLayout { request_id }).is_err());
    assert!(
        ui.request(Request::LayoutApplied {
            request_id,
            error: None
        })
        .is_err()
    );
}

#[test]
fn layout_fails_when_the_selected_ui_disconnects() {
    let env = Env::new("layout-disconnect");
    let _daemon = env.start();
    let (ui, events) = env.connect();
    let client = caller(&env);
    let pane = create(&client, &env.dir);
    let result = apply(client, pane, 30_000);
    next_layout(&events);
    drop(ui);
    let result = outcome(result);
    assert!(!result.committed);
    assert!(result.error.unwrap().contains("disconnected"));
}

#[test]
fn layout_tries_another_desktop_when_the_first_has_no_window() {
    let env = Env::new("layout-no-window");
    let _daemon = env.start();
    let (first, first_events) = env.connect();
    let (second, second_events) = env.connect();
    let client = caller(&env);
    let pane = create(&client, &env.dir);
    for error in [None, Some(LAYOUT_NO_WINDOW.to_owned())] {
        let result = apply(client.clone(), pane, 5000);
        let first_id = next_layout(&first_events);
        first
            .request(Request::LayoutApplied {
                request_id: first_id,
                error: Some(LAYOUT_NO_WINDOW.into()),
            })
            .unwrap();
        let second_id = next_layout(&second_events);
        assert_ne!(first_id, second_id);
        assert!(result.try_recv().is_err());
        assert!(
            first
                .request(Request::LayoutApplied {
                    request_id: second_id,
                    error: None
                })
                .is_err()
        );
        assert!(
            first
                .request(Request::LayoutApplied {
                    request_id: first_id,
                    error: None
                })
                .is_err()
        );
        assert!(
            first
                .request(Request::CommitLayout {
                    request_id: second_id
                })
                .is_err()
        );
        if error.is_none() {
            second
                .request(Request::CommitLayout {
                    request_id: second_id,
                })
                .unwrap();
        }
        second
            .request(Request::LayoutApplied {
                request_id: second_id,
                error: error.clone(),
            })
            .unwrap();
        let result = outcome(result);
        if error.is_none() {
            assert!(result.committed && result.error.is_none());
        } else {
            assert!(!result.committed && result.error.as_deref() == Some(LAYOUT_NO_WINDOW));
        }
    }
}

#[test]
fn committed_layout_is_preserved_when_confirmation_is_late_or_ui_disconnects() {
    for disconnect in [false, true] {
        let env = Env::new(if disconnect {
            "layout-committed-disconnect"
        } else {
            "layout-committed-timeout"
        });
        let _daemon = env.start();
        let (ui, events) = env.connect();
        let client = caller(&env);
        let pane = create(&client, &env.dir);
        let result = apply(client.clone(), pane, if disconnect { 30_000 } else { 1000 });
        let request_id = next_layout(&events);
        ui.request(Request::CommitLayout { request_id }).unwrap();
        if disconnect {
            drop(ui);
        }
        let result = outcome(result);
        assert!(result.committed);
        assert!(result.error.unwrap().contains(if disconnect {
            "disconnected"
        } else {
            "timed out"
        }));
        assert!(matches!(
            client.request(Request::PaneInfo { pane }),
            Ok(Response::PaneInfo(_))
        ));
    }
}
