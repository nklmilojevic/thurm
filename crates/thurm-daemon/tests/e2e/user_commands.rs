//! `[[commands]]` run on request and `[[on]]` hooks run on events.

use super::*;

/// An env whose config has `extra` after the usual settings.
fn env_with(name: &str, extra: &str) -> Env {
    let env = Env::new(name);
    std::fs::write(
        env.dir.join("config/config.toml"),
        format!(
            "[terminal]\nshell = [\"/bin/sh\"]\n[session]\nsnapshot_interval_secs = 1\n{extra}"
        ),
    )
    .unwrap();
    env
}

fn wait_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "{} was not written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn commands_run_in_the_pane_and_report_their_failure() {
    let base = Env::new("user-commands-base");
    let out = base.dir.join("out");
    let env = env_with(
        "user-commands",
        &format!(
            r#"
            [[commands]]
            name = "Record"
            exec = ["sh", "-c", "printf '%s|%s' \"$THURM_PANE_ID\" \"$(pwd -P)\" > \"$1\"", "sh", {out:?}]

            [[commands]]
            name = "Broken"
            exec = ["sh", "-c", "echo nope >&2; exit 2"]
            "#
        ),
    );
    let _d = env.start();
    let (c, rx) = env.connect();
    let pane = create(&c, &env.dir);

    let r = c.request(Request::RunCommand {
        name: "Record".into(),
        pane: Some(pane),
    });
    assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
    let dir = env.dir.canonicalize().unwrap();
    assert_eq!(wait_file(&out), format!("{pane}|{}", dir.display()));

    c.request(Request::RunCommand {
        name: "Broken".into(),
        pane: Some(pane),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("no failure notification") {
            Event::Notify {
                pane: p,
                title,
                body,
                ..
            } if title == "Broken failed" => {
                assert_eq!(p, pane);
                assert_eq!(body, "exit 2: nope");
                break;
            }
            _ => {}
        }
    }

    let r = c.request(Request::RunCommand {
        name: "Missing".into(),
        pane: Some(pane),
    });
    assert!(r.is_err_and(|e| e.to_string().contains("Missing")));
}

#[test]
fn a_hook_gets_the_event_on_stdin() {
    let base = Env::new("user-hooks-base");
    let out = base.dir.join("event.json");
    let env = env_with(
        "user-hooks",
        &format!(
            r#"
            [[on]]
            event = "pane_exited"
            exec = ["sh", "-c", "printf '%s ' \"$THURM_EVENT\" > \"$1\"; cat >> \"$1\"", "sh", {out:?}]
            "#
        ),
    );
    let _d = env.start();
    let (c, _rx) = env.connect();
    let pane = match c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec!["sh".into(), "-c".into(), "exit 3".into()]),
            cwd: Some(env.dir.display().to_string()),
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };

    let text = wait_file(&out);
    let (name, json) = text.split_once(' ').unwrap();
    assert_eq!(name, "pane_exited");
    // Written in two steps: wait for the JSON's end.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut json = json.to_owned();
    while !json.ends_with('\n') {
        assert!(Instant::now() < deadline, "incomplete: {json:?}");
        std::thread::sleep(Duration::from_millis(50));
        json = std::fs::read_to_string(&out).unwrap()[name.len() + 1..].to_owned();
    }
    let event: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(event["event"], "pane_exited");
    assert_eq!(event["exit_code"], 3);
    assert_eq!(event["pane"]["id"], pane);
}
