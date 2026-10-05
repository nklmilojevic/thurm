use super::*;

fn setup(name: &str) -> (Env, Daemon, Arc<Client>, PaneId) {
    setup_command(name, "/bin/cat\r")
}

fn setup_command(name: &str, command: &str) -> (Env, Daemon, Arc<Client>, PaneId) {
    let env = Env::new(name);
    let daemon = env.start();
    let (client, _) = env.connect();
    let pane = create(&client, &env.dir);
    client
        .request(Request::Input {
            pane,
            data: command.as_bytes().to_vec(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Response::PaneInfo(info) = client.request(Request::PaneInfo { pane }).unwrap()
            && info.foreground.is_some_and(|p| p.name == "cat")
        {
            break;
        }
        assert!(Instant::now() < deadline, "cat did not start");
        std::thread::sleep(Duration::from_millis(25));
    }
    hook(&client, pane, "session-start", Some("prompt-test"));
    (env, daemon, client, pane)
}

fn submit(
    client: &Client,
    pane: PaneId,
    text: &str,
    wait: bool,
    timeout_ms: u64,
) -> Result<Response, thurm_client::ClientError> {
    client.request(Request::AgentPrompt {
        pane,
        text: text.into(),
        wait,
        timeout_ms,
    })
}

#[test]
fn prompt_rejects_approval_without_writing_and_rejects_unsafe_text() {
    let (_env, _daemon, client, pane) = setup("prompt-blocked");
    hook(&client, pane, "permission-prompt", None);
    let before = capture(&client, pane);
    assert!(submit(&client, pane, "must-not-arrive", false, 1000).is_err());
    assert_eq!(capture(&client, pane), before);
    hook(&client, pane, "stop", None);
    for text in ["", "\x1b[201~injected", "first\nsecond"] {
        assert!(submit(&client, pane, text, false, 1000).is_err());
    }
}

#[test]
fn prompt_timeout_does_not_accept_old_done_state() {
    let (_env, _daemon, client, pane) = setup("prompt-timeout");
    hook(&client, pane, "stop", None);
    let start = Instant::now();
    assert!(matches!(
        submit(&client, pane, "never-started", true, 150),
        Ok(Response::AgentPrompt(AgentPromptOutcome::Timeout))
    ));
    assert!(start.elapsed() >= Duration::from_millis(150));
    assert!(submit(&client, pane, "duplicate", false, 1000).is_err());
}

#[test]
fn prompt_and_terminal_attachment_have_one_input_owner() {
    let (_env, _daemon, client, pane) = setup("prompt-attachment");
    client
        .request(Request::AttachTerminal {
            pane,
            size: PaneSize::default(),
        })
        .unwrap();
    assert!(submit(&client, pane, "must-not-arrive", false, 1000).is_err());
    assert!(!capture(&client, pane).contains("must-not-arrive"));
    client.request(Request::DetachTerminal { pane }).unwrap();

    submit(&client, pane, "pending-marker", false, 1000).unwrap();
    assert!(
        client
            .request(Request::AttachTerminal {
                pane,
                size: PaneSize::default()
            })
            .is_err()
    );
    hook(&client, pane, "prompt-submit", None);
    hook(&client, pane, "stop", None);

    let worker = {
        let client = client.clone();
        std::thread::spawn(move || submit(&client, pane, "attach-during-turn", true, 3000))
    };
    wait_match(&client, pane, "attach-during-turn");
    hook(&client, pane, "prompt-submit", None);
    client
        .request(Request::AttachTerminal {
            pane,
            size: PaneSize::default(),
        })
        .unwrap();
    client.request(Request::DetachTerminal { pane }).unwrap();
    hook(&client, pane, "stop", None);
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn prompt_tracks_fast_turn_and_rejects_a_replacement_session() {
    let (_env, _daemon, client, pane) = setup("prompt-turn");
    let worker = {
        let client = client.clone();
        std::thread::spawn(move || submit(&client, pane, "first-turn-marker", true, 3000))
    };
    wait_match(&client, pane, "first-turn-marker");
    hook(&client, pane, "prompt-submit", None);
    hook(&client, pane, "stop", None);
    assert!(matches!(
        worker.join().unwrap(),
        Ok(Response::AgentPrompt(AgentPromptOutcome::Completed))
    ));
    let worker = {
        let client = client.clone();
        std::thread::spawn(move || submit(&client, pane, "second-turn-marker", true, 3000))
    };
    wait_match(&client, pane, "second-turn-marker");
    hook(&client, pane, "session-start", Some("replacement"));
    hook(&client, pane, "prompt-submit", None);
    hook(&client, pane, "stop", None);
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn prompt_wait_rejects_other_keyboard_input() {
    let (_env, _daemon, client, pane) = setup("prompt-input");
    let worker = {
        let client = client.clone();
        std::thread::spawn(move || submit(&client, pane, "input-turn-marker", true, 3000))
    };
    wait_match(&client, pane, "input-turn-marker");
    client
        .request(Request::Input {
            pane,
            data: b"other input".to_vec(),
        })
        .unwrap();
    hook(&client, pane, "prompt-submit", None);
    hook(&client, pane, "stop", None);
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn permission_answer_cannot_complete_the_original_prompt_wait() {
    let (_env, _daemon, client, pane) = setup("prompt-permission-answer");
    let worker = {
        let client = client.clone();
        std::thread::spawn(move || submit(&client, pane, "permission-turn-marker", true, 3000))
    };
    wait_match(&client, pane, "permission-turn-marker");
    hook(&client, pane, "prompt-submit", None);
    hook(&client, pane, "permission-prompt", None);
    let prompt = agent(&client, pane).unwrap().permission.unwrap();
    client
        .request(Request::AnswerPermission {
            pane,
            prompt,
            allow: true,
        })
        .unwrap();
    hook(&client, pane, "stop", None);
    // The wait can observe NeedsInput before the answer. Once answered, it must fail.
    let result = worker.join().unwrap();
    assert!(
        matches!(
            result,
            Err(_) | Ok(Response::AgentPrompt(AgentPromptOutcome::NeedsInput))
        ),
        "{result:?}"
    );
}

#[test]
fn prompt_respects_disabled_automatic_detection() {
    let env = Env::new("prompt-detect-disabled");
    std::fs::write(
        env.dir.join("config/config.toml"),
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [agents]
        detect = false
        idle_after_ms = 1
        [[agents.define]]
        kind = "test-cat"
        name = "Test Cat"
        processes = ["cat"]
        "#,
    )
    .unwrap();
    let _daemon = env.start();
    let (client, _) = env.connect();
    let pane = create(&client, &env.dir);
    client
        .request(Request::Input {
            pane,
            data: b"/bin/cat\r".to_vec(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Response::PaneInfo(info) = client.request(Request::PaneInfo { pane }).unwrap()
            && info.foreground.is_some_and(|p| p.name == "cat")
        {
            break;
        }
        assert!(Instant::now() < deadline, "cat did not start");
        std::thread::sleep(Duration::from_millis(25));
    }
    let result = submit(&client, pane, "must-not-arrive", false, 1000);
    assert!(result.is_err(), "{result:?}");
    assert!(!capture(&client, pane).contains("must-not-arrive"));
}

#[test]
fn terminal_reports_interrupt_prompt_waits_only_when_they_write_input() {
    for enabled in [false, true] {
        for kind in ["mouse", "wheel", "focus"] {
            let name = format!("prompt-{kind}-{enabled}");
            let command = if enabled {
                "printf '\\033[?1000h\\033[?1006h\\033[?1004h'; /bin/cat\r"
            } else {
                "/bin/cat\r"
            };
            let (_env, _daemon, client, pane) = setup_command(&name, command);
            let worker = {
                let client = client.clone();
                std::thread::spawn(move || submit(&client, pane, "report-turn-marker", true, 3000))
            };
            wait_match(&client, pane, "report-turn-marker");
            let request = match kind {
                "mouse" => Request::Mouse {
                    pane,
                    event: MouseEvent {
                        kind: MouseKind::Press,
                        button: MouseButton::Left,
                        col: 1,
                        row: 1,
                        right_half: false,
                        mods: 0,
                        clicks: 1,
                        x: 8,
                        y: 16,
                    },
                },
                "wheel" => Request::Wheel {
                    pane,
                    lines: 1,
                    col: 1,
                    row: 1,
                    mods: 0,
                },
                _ => Request::Focus {
                    pane,
                    focused: false,
                },
            };
            client.request(request).unwrap();
            hook(&client, pane, "prompt-submit", None);
            hook(&client, pane, "stop", None);
            let result = worker.join().unwrap();
            if enabled {
                assert!(result.is_err(), "{kind}: {result:?}");
            } else {
                assert!(
                    matches!(
                        result,
                        Ok(Response::AgentPrompt(AgentPromptOutcome::Completed))
                    ),
                    "{kind}: {result:?}"
                );
            }
        }
    }
}
