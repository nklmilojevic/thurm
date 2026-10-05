use super::*;

fn upgrade(env: &Env, daemon: &Daemon, client: &Client) -> Arc<Client> {
    let mut request = env.socket.as_os_str().to_owned();
    request.push(".upgrade");
    std::fs::write(PathBuf::from(request), env!("CARGO_BIN_EXE_thurmd")).unwrap();
    assert_eq!(
        unsafe { libc::kill(daemon.child.id() as i32, libc::SIGUSR2) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while client.is_alive() {
        assert!(
            Instant::now() < deadline,
            "old daemon connection stayed open"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    env.connect().0
}

fn pane_info(client: &Client, pane: PaneId) -> PaneInfo {
    match client.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(info) => info,
        other => panic!("{other:?}"),
    }
}

#[test]
fn upgrade_keeps_pending_prompts_for_hooks_and_public_reports() {
    for public in [false, true] {
        let env = Env::new(if public {
            "upgrade-pending-report"
        } else {
            "upgrade-pending-hook"
        });
        let daemon = env.start();
        let (client, _) = env.connect();
        let pane = create(&client, &env.dir);
        client
            .request(Request::Input {
                pane,
                data: b"/bin/cat\r".to_vec(),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid = loop {
            if let Some(process) = pane_info(&client, pane).foreground
                && process.name == "cat"
            {
                break process.pid;
            }
            assert!(Instant::now() < deadline, "cat did not start");
            std::thread::sleep(Duration::from_millis(20));
        };
        let report = |client: &Client, status: AgentStatus, sequence: u64| {
            client
                .request(Request::AgentReport {
                    pane,
                    report: AgentReport {
                        agent: "test".into(),
                        owner_pid: pid,
                        instance: "pending-upgrade".into(),
                        sequence,
                        status,
                        session_id: Some("pending-session".into()),
                        message: None,
                        resume_argv: None,
                    },
                })
                .unwrap();
        };
        let hook_event = |client: &Client, event: &str| {
            client
                .request(Request::AgentHook {
                    pane,
                    agent: "test".into(),
                    event: event.into(),
                    session_id: Some("pending-session".into()),
                    message: None,
                    transcript_path: None,
                    pgrp: Some(pid),
                })
                .unwrap();
        };
        if public {
            report(&client, AgentStatus::Idle, 1);
        } else {
            hook_event(&client, "session-start");
        }
        let prompt = |client: &Client, text: &str| {
            client.request(Request::AgentPrompt {
                pane,
                text: text.into(),
                wait: false,
                timeout_ms: 1000,
            })
        };
        assert!(matches!(
            prompt(&client, "accepted-before-upgrade"),
            Ok(Response::AgentPrompt(AgentPromptOutcome::Submitted))
        ));
        wait_match(&client, pane, "accepted-before-upgrade");
        let mut client = client;
        for _ in 0..2 {
            client = upgrade(&env, &daemon, &client);
            // The monitor must also keep the pending state after it recomputes agent status.
            std::thread::sleep(Duration::from_millis(600));
            assert_eq!(
                pane_info(&client, pane)
                    .agent
                    .unwrap()
                    .session_id
                    .as_deref(),
                Some("pending-session")
            );
            let error = prompt(&client, "duplicate-must-not-arrive").unwrap_err();
            assert!(error.to_string().contains("already pending"), "{error}");
            assert!(!capture(&client, pane).contains("duplicate-must-not-arrive"));
        }
        if public {
            report(&client, AgentStatus::Working, 2);
            report(&client, AgentStatus::Done, 3);
        } else {
            hook_event(&client, "prompt-submit");
            hook_event(&client, "stop");
        }
        assert!(matches!(
            prompt(&client, "next-turn-after-upgrade"),
            Ok(Response::AgentPrompt(AgentPromptOutcome::Submitted))
        ));
    }
}

#[test]
fn upgrade_restores_size_of_disconnected_terminal_attachment() {
    let env = Env::new("upgrade-attach-size");
    let daemon = env.start();
    let (client, _) = env.connect();
    let pane = create(&client, &env.dir);
    let original = pane_info(&client, pane).size;
    let (attached, _) = env.connect();
    let temporary = PaneSize {
        cols: original.cols + 11,
        rows: original.rows + 7,
        ..original
    };
    attached
        .request(Request::AttachTerminal {
            pane,
            size: temporary,
        })
        .unwrap();
    assert_eq!(pane_info(&client, pane).size, temporary);
    let client = upgrade(&env, &daemon, &client);
    assert_eq!(pane_info(&client, pane).size, original);
    client
        .request(Request::Input {
            pane,
            data: b"printf 'RESTORED:'; stty size\r".to_vec(),
        })
        .unwrap();
    wait_match(
        &client,
        pane,
        &format!("RESTORED:{} {}", original.rows, original.cols),
    );
    client
        .request(Request::AttachTerminal {
            pane,
            size: temporary,
        })
        .unwrap();
    client.request(Request::DetachTerminal { pane }).unwrap();
    assert_eq!(pane_info(&client, pane).size, original);
}
