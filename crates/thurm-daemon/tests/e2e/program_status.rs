//! Program Status Protocol (OSC 7501) through the daemon.

use super::*;

fn info(c: &Client, pane: PaneId) -> PaneInfo {
    match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(i) => i,
        other => panic!("{other:?}"),
    }
}

fn wait_info(c: &Client, pane: PaneId, what: &str, f: impl Fn(&PaneInfo) -> bool) -> PaneInfo {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let i = info(c, pane);
        if f(&i) {
            return i;
        }
        assert!(Instant::now() < deadline, "{what}: {i:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run(c: &Client, pane: PaneId, line: &str) {
    c.request(Request::Input {
        pane,
        data: format!("{line}\r").into_bytes(),
    })
    .unwrap();
}

#[test]
fn programs_report_their_status() {
    let env = Env::new("program-status");
    let _d = env.start();
    let (c, rx) = env.connect();
    let pane = create(&c, &env.dir);
    c.request(Request::Subscribe { pane }).unwrap();

    // Feature detection: the reply is the query.
    run(
        &c,
        pane,
        r"stty raw -echo; printf '\033]7501;?\033\\'; dd bs=1 count=10 2>/dev/null | od -An -c; stty sane",
    );
    wait_match(&c, pane, r"033\s+]\s+7\s+5\s+0\s+1\s+;\s+\?\s+033\s+\\");

    // "Apply?"
    run(
        &c,
        pane,
        r"printf '\033]7501;state=blocked:kind=permission:app=terraform:msg=QXBwbHk/\033\\'",
    );
    let i = wait_info(&c, pane, "blocked", |i| {
        i.agent
            .as_ref()
            .is_some_and(|a| a.status == AgentStatus::NeedsInput)
    });
    let a = i.agent.unwrap();
    assert_eq!(
        (a.name.as_str(), a.message.as_deref()),
        ("terraform", Some("Apply?"))
    );
    assert_eq!(i.programs.len(), 1);
    assert_eq!(i.programs[0].kind, Some(BlockedKind::Permission));
    assert_eq!(
        next_notification(&rx, pane),
        ("terraform".into(), "Apply?".into())
    );

    // "Applied", with a worker that shows its progress meanwhile.
    run(
        &c,
        pane,
        r"printf '\033]7501;state=working:app=terraform\033\\\033]7501;state=working:id=w:progress=40\033\\'",
    );
    let i = wait_info(&c, pane, "working worker", |i| {
        i.agent
            .as_ref()
            .is_some_and(|a| a.status == AgentStatus::Working)
    });
    assert_eq!(i.progress.and_then(|p| p.percent), Some(40));
    assert!(
        i.programs
            .iter()
            .all(|r| r.app.as_deref() == Some("terraform"))
    );
    // A request answered within the same read was still one ("Sure?").
    run(
        &c,
        pane,
        r"sleep 0.5; printf '\033]7501;state=blocked:app=terraform:msg=U3VyZT8=\033\\\033]7501;state=working:app=terraform\033\\'",
    );
    assert_eq!(
        next_notification(&rx, pane),
        ("terraform".into(), "Sure?".into())
    );

    // Typing acknowledges results, so this one comes after the last key.
    run(
        &c,
        pane,
        r"sleep 0.5; printf '\033]7501;state=clear:id=w\033\\\033]7501;state=done:app=terraform:msg=QXBwbGllZA==\033\\'",
    );
    let i = wait_info(&c, pane, "done", |i| {
        i.agent
            .as_ref()
            .is_some_and(|a| a.status == AgentStatus::Done)
    });
    assert_eq!(i.progress, None);
    assert_eq!(
        next_notification(&rx, pane),
        ("terraform".into(), "Applied".into())
    );

    // Looking at the pane: the result is seen, and nothing stands for the program anymore.
    c.request(Request::Focus {
        pane,
        focused: true,
    })
    .unwrap();
    let i = wait_info(&c, pane, "acknowledged", |i| i.agent.is_none());
    assert!(i.programs[0].seen);

    // A full reset removes every record.
    run(&c, pane, r"printf '\033]7501;state=error\033\\'");
    wait_info(&c, pane, "error", |i| {
        i.agent
            .as_ref()
            .is_some_and(|a| a.status == AgentStatus::Error)
    });
    run(&c, pane, r"printf '\033c'");
    wait_info(&c, pane, "reset", |i| {
        i.programs.is_empty() && i.agent.is_none()
    });
}

#[test]
fn records_survive_an_upgrade_without_notifying_again() {
    let env = Env::new("program-status-upgrade");
    let daemon = env.start();
    let (c, _) = env.connect();
    let pane = create(&c, &env.dir);
    // "Apply?"
    run(
        &c,
        pane,
        r"printf '\033]7501;state=blocked:app=terraform:msg=QXBwbHk/\033\\'",
    );
    wait_info(&c, pane, "blocked", |i| !i.programs.is_empty());
    let c = super::workflows_upgrade::upgrade(&env, &daemon, &c);
    let (c2, rx) = env.connect();
    let i = info(&c, pane);
    let a = i.agent.expect("restored program state");
    assert_eq!(
        (a.name.as_str(), a.status),
        ("terraform", AgentStatus::NeedsInput)
    );
    assert_eq!(i.programs.len(), 1);
    // Old news.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !rx.try_iter().any(|e| matches!(e, Event::Notify { .. })),
        "notified again"
    );
    drop(c2);
}
