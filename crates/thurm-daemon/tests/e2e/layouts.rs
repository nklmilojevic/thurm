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
        ui.request(Request::LayoutApplied {
            request_id,
            error: error.clone(),
        })
        .unwrap();
        let result = result.recv_timeout(Duration::from_secs(5)).unwrap();
        match error {
            None => assert!(matches!(result, Ok(Response::Ok))),
            Some(error) => assert!(result.unwrap_err().to_string().contains(&error)),
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
    let error = result
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
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
    let error = result
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("disconnected"), "{error}");
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
        second
            .request(Request::LayoutApplied {
                request_id: second_id,
                error: error.clone(),
            })
            .unwrap();
        let result = result.recv_timeout(Duration::from_secs(5)).unwrap();
        if error.is_none() {
            assert!(matches!(result, Ok(Response::Ok)));
        } else {
            assert!(result.unwrap_err().to_string().contains(LAYOUT_NO_WINDOW));
        }
    }
}
