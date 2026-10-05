//! End-to-end: run the real daemon with real PTYs and drive it through the client library.

#[path = "e2e/agent_prompt.rs"]
mod agent_prompt;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use thurm_client::{Client, ConnectOptions};
use thurm_proto::*;

struct Env {
    dir: PathBuf,
    socket: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("thurm-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join("config/config.toml"),
            r#"
            [terminal]
            shell = ["/bin/sh"]
            [session]
            snapshot_interval_secs = 1
            "#,
        )
        .unwrap();
        let socket = dir.join("d.sock");
        Env { dir, socket }
    }

    /// Like `new`, with `shell` as the configured shell command.
    fn with_shell(name: &str, shell: &[&str]) -> Env {
        let env = Env::new(name);
        let shell = shell
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            env.dir.join("config/config.toml"),
            format!("[terminal]\nshell = [{shell}]\n[session]\nsnapshot_interval_secs = 1\n"),
        )
        .unwrap();
        env
    }

    fn start(&self) -> Daemon {
        self.start_with(&[])
    }

    /// Like `start`, with extra environment variables for the daemon.
    fn start_with(&self, vars: &[(&str, &Path)]) -> Daemon {
        let child = Command::new(env!("CARGO_BIN_EXE_thurmd"))
            .args(["--foreground", "--socket"])
            .arg(&self.socket)
            .env("THURM_CONFIG_DIR", self.dir.join("config"))
            .env("THURM_STATE_DIR", self.dir.join("state"))
            .env("SHELL", "/bin/sh")
            .env("THURM_LOG", "debug")
            // Agents' settings (hooks installed on launch) stay in the test directory.
            .env("CLAUDE_CONFIG_DIR", self.dir.join("claude"))
            .env("CODEX_HOME", self.dir.join("codex"))
            .envs(vars.iter().copied())
            .spawn()
            .expect("spawn daemon");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::os::unix::net::UnixStream::connect(&self.socket).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Daemon { child }
    }

    fn connect(&self) -> (Arc<Client>, Receiver<Event>) {
        let (tx, rx) = unbounded();
        let c = Client::connect(
            ConnectOptions {
                socket: self.socket.clone(),
                spawn_daemon: None,
                client_name: "e2e",
                ui: true,
            },
            move |ev| {
                let _ = tx.send(ev);
            },
            || {},
        )
        .expect("connect");
        (c, rx)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Daemon {
    child: Child,
}

impl Daemon {
    fn wait_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "daemon did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn create(c: &Client, cwd: &Path) -> PaneId {
    match c
        .request(Request::CreatePane(CreatePane {
            cwd: Some(cwd.display().to_string()),
            size: PaneSize {
                cols: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    }
}

fn wait_match(c: &Client, pane: PaneId, regex: &str) {
    let r = c
        .request(Request::Wait {
            pane,
            until: WaitCondition::Match {
                regex: regex.into(),
            },
            timeout_ms: Some(10_000),
        })
        .unwrap();
    assert!(
        matches!(r, Response::Wait(WaitOutcome::Satisfied)),
        "waiting for {regex:?}: {r:?}\n{}",
        capture(c, pane)
    );
}

fn capture(c: &Client, pane: PaneId) -> String {
    match c.request(Request::Capture {
        pane,
        opts: CaptureOpts {
            lines: None,
            scrollback: true,
            ansi: false,
        },
    }) {
        Ok(Response::Text(t)) => t,
        other => format!("{other:?}"),
    }
}

#[test]
fn panes_frames_persistence_and_restore() {
    let env = Env::new("main");
    let mut daemon = env.start();
    let (c, events) = env.connect();
    assert!(matches!(
        c.hello.lock().clone(),
        Some(Response::Hello {
            restored: false,
            ..
        })
    ));

    let pane = create(&c, &env.dir);
    c.request(Request::Subscribe { pane }).unwrap();
    c.request(Request::Input {
        pane,
        data: b"echo hello-$((40+2)); printf '\\033]0;custom-title\\007'\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "hello-42");

    // The subscriber gets the pane's state, then its output stream: a copy of the terminal
    // built from them shows what the pane shows.
    let cfg = thurm_term::EngineConfig::from_config(&thurm_config::Config::default(), true);
    let mut copy: Option<thurm_term::Terminal> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = false;
    while Instant::now() < deadline && !seen {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Attach {
                pane: p,
                size,
                state,
            }) => {
                assert_eq!(p, pane);
                let mut t = thurm_term::Terminal::new(size, cfg.clone());
                t.replay(&state);
                copy = Some(t);
            }
            Ok(Event::Output { pane: p, data }) => {
                assert_eq!(p, pane);
                copy.as_mut().expect("output before attach").advance(&data);
            }
            _ => {}
        }
        seen = copy
            .as_ref()
            .is_some_and(|t| t.screen_text().contains("hello-42"));
    }
    assert!(seen, "the local copy never showed the output");

    // Pane info: cwd from the process, title from OSC 0.
    std::thread::sleep(Duration::from_millis(800));
    let info = match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(i) => i,
        other => panic!("{other:?}"),
    };
    assert!(info.alive);
    assert_eq!(info.title, "custom-title");
    assert_eq!(
        std::fs::canonicalize(info.cwd.as_deref().unwrap()).unwrap(),
        std::fs::canonicalize(&env.dir).unwrap()
    );
    assert_eq!(info.size.cols, 80);

    // Key encoding through the daemon.
    c.request(Request::Key {
        pane,
        key: KeyEvent {
            key: Key::Char('c'),
            mods: mods::CTRL,
            action: KeyAction::Press,
            text: String::new(),
            shifted: None,
            base_layout: None,
        },
    })
    .unwrap();

    // Resize is reflected in the PTY.
    c.request(Request::Resize {
        pane,
        size: PaneSize {
            cols: 100,
            rows: 30,
            cell_width: 8,
            cell_height: 16,
        },
    })
    .unwrap();
    c.request(Request::Input {
        pane,
        data: b"stty size\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "30 100");

    // Layout round trip.
    let layout = Layout {
        windows: vec![WindowLayout {
            frame: None,
            tabs: vec![TabLayout {
                title: None,
                root: LayoutNode::local(pane),
                focused: pane,
                zoomed: None,
                handoff: None,
            }],
            selected_tab: 0,
            fullscreen: false,
            workspace: 0,
        }],
        workspaces: vec![],
        quick: None,
    };
    c.request(Request::SetLayout {
        json: serde_json_string(&layout),
    })
    .unwrap();
    c.request(Request::SaveSnapshot).unwrap();
    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    drop(c);
    daemon.wait_exit();

    // Restart: the pane comes back with the same id, cwd, scrollback and layout.
    let _daemon = env.start();
    let (c, _events) = env.connect();
    assert!(matches!(
        c.hello.lock().clone(),
        Some(Response::Hello { restored: true, .. })
    ));
    let panes = match c.request(Request::ListPanes).unwrap() {
        Response::Panes(p) => p,
        other => panic!("{other:?}"),
    };
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].id, pane);
    assert!(panes[0].restored);
    let text = capture(&c, pane);
    assert!(text.contains("hello-42"), "{text}");
    assert!(text.contains("session restored"), "{text}");
    match c.request(Request::GetLayout).unwrap() {
        Response::Layout(Some(l)) => assert!(l.contains(&format!("\"id\":{pane}"))),
        other => panic!("{other:?}"),
    }
    // New panes don't reuse the restored id.
    let second = create(&c, &env.dir);
    assert!(second > pane);

    // The restored shell works.
    c.request(Request::Input {
        pane,
        data: b"echo alive-again\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "alive-again");
}

#[test]
fn exit_close_capture_and_wait() {
    let env = Env::new("exit");
    let _daemon = env.start();
    let (c, events) = env.connect();

    let pane = create(&c, &env.dir);
    c.request(Request::Input {
        pane,
        data: b"printf 'a\\nb\\nc\\n'; sleep 0.3; echo done\r".to_vec(),
    })
    .unwrap();
    let r = c
        .request(Request::Wait {
            pane,
            until: WaitCondition::Idle { quiet_ms: 700 },
            timeout_ms: Some(10_000),
        })
        .unwrap();
    assert!(matches!(r, Response::Wait(WaitOutcome::Satisfied)));
    assert!(capture(&c, pane).contains("done"));

    // Timeout path.
    let r = c
        .request(Request::Wait {
            pane,
            until: WaitCondition::Match {
                regex: "never-appears".into(),
            },
            timeout_ms: Some(300),
        })
        .unwrap();
    assert!(matches!(r, Response::Wait(WaitOutcome::Timeout)));

    // Exiting the shell removes the pane and emits events.
    c.request(Request::Input {
        pane,
        data: b"exit 3\r".to_vec(),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut exited, mut closed) = (None, false);
    while Instant::now() < deadline && !(exited.is_some() && closed) {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::PaneExited { pane: p, code }) if p == pane => exited = Some(code),
            Ok(Event::PaneClosed { pane: p }) if p == pane => closed = true,
            _ => {}
        }
    }
    assert_eq!(exited, Some(Some(3)));
    assert!(closed);
    assert!(c.request(Request::PaneInfo { pane }).is_err());

    // Explicit close.
    let p2 = create(&c, &env.dir);
    c.request(Request::ClosePane { pane: p2 }).unwrap();
    assert!(c.request(Request::PaneInfo { pane: p2 }).is_err());

    // Ui commands need a UI client: we are one, so it is broadcast back to us.
    let p3 = create(&c, &env.dir);
    c.request(Request::Ui(UiCommand::NewTab {
        pane: p3,
        new_window: false,
    }))
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = false;
    while Instant::now() < deadline && !got {
        got = matches!(events.recv_timeout(Duration::from_millis(200)), Ok(Event::Ui(UiCommand::NewTab { pane, .. })) if pane == p3);
    }
    assert!(got);
}

#[test]
fn password_prompt_is_detected() {
    let env = Env::new("pw");
    let _daemon = env.start();
    let (c, _events) = env.connect();
    let pane = create(&c, &env.dir);
    c.request(Request::Input {
        pane,
        data: b"stty -echo; echo ready; sleep 5\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "ready");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Response::PaneInfo(i) = c.request(Request::PaneInfo { pane }).unwrap()
            && i.password_input
        {
            break;
        }
        assert!(Instant::now() < deadline, "password mode not detected");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn serde_json_string(l: &Layout) -> String {
    // thurm-proto re-exports serde_json only through to_json.
    thurm_proto::to_json(l)
}

fn agent(c: &Client, pane: PaneId) -> Option<AgentState> {
    match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(i) => i.agent,
        other => panic!("{other:?}"),
    }
}

/// The pane's foreground process group: where a hook the agent runs comes from.
fn fg_pgrp(c: &Client, pane: PaneId) -> Option<u32> {
    match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(i) => i.foreground.map(|p| p.pid),
        other => panic!("{other:?}"),
    }
}

fn hook(c: &Client, pane: PaneId, event: &str, session: Option<&str>) {
    let r = c.request(Request::AgentHook {
        pane,
        agent: "claude".into(),
        event: event.into(),
        session_id: session.map(str::to_owned),
        message: None,
        transcript_path: None,
        pgrp: fg_pgrp(c, pane),
    });
    assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
}

#[test]
fn agent_hooks_drive_status_and_wait() {
    let env = Env::new("hooks");
    let _d = env.start();
    let (c, rx) = env.connect();
    let pane = create(&c, &env.dir);
    // A foreground program that isn't a known agent: the hook alone identifies it. The system
    // cat: a Nix dev shell's is coreutils' multi-call binary, which runs as `coreutils`.
    c.request(Request::Input {
        pane,
        data: b"/bin/cat\r".to_vec(),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let fg = match c.request(Request::PaneInfo { pane }).unwrap() {
            Response::PaneInfo(i) => i.foreground.map(|p| p.name),
            _ => None,
        };
        if fg.as_deref() == Some("cat") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cat never became the foreground ({fg:?})"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    hook(&c, pane, "session-start", Some("sess-42"));
    hook(&c, pane, "prompt-submit", None);
    let a = agent(&c, pane).expect("hooked agent");
    assert_eq!(
        (a.status, a.kind.as_str(), a.hooked),
        (AgentStatus::Working, "claude", true)
    );
    assert_eq!(a.session_id.as_deref(), Some("sess-42"));
    // The monitor's screen heuristics (every 500 ms) must not override the hook.
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(agent(&c, pane).unwrap().status, AgentStatus::Working);

    let waiter = {
        let c = c.clone();
        std::thread::spawn(move || {
            c.request(Request::Wait {
                pane,
                until: WaitCondition::AgentStatus(AgentStatus::Done),
                timeout_ms: Some(5000),
            })
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    hook(&c, pane, "stop", None);
    assert!(matches!(
        waiter.join().unwrap(),
        Ok(Response::Wait(WaitOutcome::Satisfied))
    ));
    let a = agent(&c, pane).unwrap();
    assert_eq!((a.status, a.turns), (AgentStatus::Done, 1));

    // The session's title (from its transcript) names the pane.
    let transcript = env.dir.join("session.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"user\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Fix login bug\"}\n",
    )
    .unwrap();
    let r = c.request(Request::AgentHook {
        pane,
        agent: "claude".into(),
        event: "stop".into(),
        session_id: None,
        message: None,
        transcript_path: Some(transcript.to_string_lossy().into_owned()),
        pgrp: fg_pgrp(&c, pane),
    });
    assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
    assert_eq!(
        agent(&c, pane).unwrap().topic.as_deref(),
        Some("Fix login bug")
    );
    match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(i) => assert_eq!(i.title, "Fix login bug"),
        other => panic!("{other:?}"),
    }
    // The agent repainting its own title (a spinner frame) doesn't flip the tab to it.
    while rx.try_recv().is_ok() {}
    c.request(Request::Input {
        pane,
        data: "\x1b]0;◑ Fix login bug\x07title-set\r".as_bytes().to_vec(),
    })
    .unwrap();
    // Once in the echo, once in what cat wrote back.
    wait_match(&c, pane, "title-set(.|\\n)*title-set");
    let until = Instant::now() + Duration::from_millis(700);
    while let Ok(ev) = rx.recv_deadline(until) {
        if let Event::PaneInfo(i) = ev
            && i.id == pane
        {
            assert_eq!(i.title, "Fix login bug");
        }
    }

    // Back at the shell: the agent is gone.
    c.request(Request::Input {
        pane,
        data: vec![0x04],
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while agent(&c, pane).is_some() {
        assert!(Instant::now() < deadline, "agent state not cleared");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn permission_prompt_answered_from_notification() {
    let env = Env::new("permission");
    let _d = env.start();
    let (c, rx) = env.connect();
    let pane = create(&c, &env.dir);
    c.request(Request::Subscribe { pane }).unwrap();
    // A stand-in agent that shows what it is typed (the tty echoes Esc as `^[`).
    c.request(Request::Input {
        pane,
        data: b"/bin/cat\r".to_vec(),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(c.request(Request::PaneInfo { pane }), Ok(Response::PaneInfo(i))
        if i.foreground.as_ref().is_some_and(|f| f.name == "cat"))
    {
        assert!(Instant::now() < deadline, "cat never became the foreground");
        std::thread::sleep(Duration::from_millis(100));
    }
    hook(&c, pane, "prompt-submit", None);

    let prompt = || {
        hook(&c, pane, "permission-prompt", None);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if let Event::Notify {
                pane: p,
                permission: Some(prompt),
                ..
            } = rx.recv_timeout(left).expect("no notification with answers")
                && p == pane
            {
                return prompt;
            }
        }
    };
    let answer = |prompt, allow| {
        c.request(Request::AnswerPermission {
            pane,
            prompt,
            allow,
        })
    };

    // Already waiting on another request: the prompt still gets its own notification.
    hook(&c, pane, "notification", None);
    assert_eq!(agent(&c, pane).unwrap().status, AgentStatus::NeedsInput);
    let first = prompt();
    assert_eq!(agent(&c, pane).unwrap().permission, Some(first));
    assert!(matches!(answer(first, true), Ok(Response::Ok)));
    let a = agent(&c, pane).unwrap();
    assert_eq!((a.status, a.permission), (AgentStatus::Working, None));
    assert!(answer(first, true).is_err(), "a prompt is answered once");

    hook(&c, pane, "tool-complete", None);
    let second = prompt();
    assert!(answer(first, false).is_err(), "an old notification");
    assert!(matches!(answer(second, false), Ok(Response::Ok)));
    wait_match(&c, pane, r"\^\[");
}

#[test]
fn terminate_daemon_by_socket_peer() {
    let env = Env::new("terminate");
    let mut d = env.start();
    assert_eq!(thurm_client::daemon_pid(&env.socket), Some(d.child.id()));
    // Our own child stays a zombie until reaped, so reap it while terminate_daemon waits
    // (real daemons are reparented to launchd, which reaps them).
    let socket = env.socket.clone();
    let stopper = std::thread::spawn(move || thurm_client::terminate_daemon(&socket));
    d.wait_exit();
    let pid = stopper.join().unwrap().expect("terminated");
    assert_eq!(pid, d.child.id());
    assert!(std::os::unix::net::UnixStream::connect(&env.socket).is_err());
    // It saved the session on the way out.
    assert!(env.dir.join("state/session.json").exists());
}

/// Open descriptors of process `pid`.
fn open_fds(pid: u32) -> usize {
    if let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        return dir.count();
    }
    let out = Command::new("lsof")
        .args(["-n", "-P", "-p", &pid.to_string()])
        .output()
        .expect("lsof");
    // Minus the header line.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .count()
        .saturating_sub(1)
}

#[test]
fn disconnected_clients_release_their_sockets() {
    let env = Env::new("client-leak");
    let d = env.start();
    let cycle = || {
        for _ in 0..20 {
            let (c, _rx) = env.connect();
            c.request(Request::ListPanes).unwrap();
            drop(c);
            drop(std::os::unix::net::UnixStream::connect(&env.socket).unwrap());
        }
    };
    cycle();
    std::thread::sleep(Duration::from_millis(300));
    let before = open_fds(d.child.id());
    cycle();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now = open_fds(d.child.id());
        if now <= before + 4 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "40 closed connections left {} descriptors open",
            now - before
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn launching_an_agent_installs_its_hooks() {
    let env = Env::new("launch-hooks");
    // A stand-in `claude` (hooks are chosen by the program's name).
    let bin = env.dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(&claude, "#!/bin/sh\necho agent up; sleep 5\n").unwrap();
    std::fs::set_permissions(&claude, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let _d = env.start();
    let (c, _rx) = env.connect();
    let settings = env.dir.join("claude/settings.json");
    // A shell installs nothing.
    create(&c, &env.dir);
    assert!(!settings.exists());
    let pane = match c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec![claude.display().to_string()]),
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    wait_match(&c, pane, "agent up");
    let text = std::fs::read_to_string(&settings).expect("hooks written before the agent ran");
    assert!(text.contains("agent-hook claude stop"), "{text}");
    assert!(!env.dir.join("codex/hooks.json").exists());
}

#[test]
fn fork_agent_session() {
    let env = Env::new("fork");
    // A stand-in "claude" whose fork command just prints the session it was given.
    std::fs::write(
        env.dir.join("config/config.toml"),
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [[agents.define]]
        kind = "claude"
        name = "Claude Code"
        processes = ["claude"]
        fork_session = ["sh", "-c", "echo forked {session}; sleep 5"]
        "#,
    )
    .unwrap();
    let _d = env.start();
    let (c, _rx) = env.connect();
    let pane = create(&c, &env.dir);

    // Without a hooked session there is nothing to fork.
    let r = c.request(Request::CreatePane(CreatePane {
        fork_from: Some(pane),
        ..Default::default()
    }));
    assert!(
        matches!(&r, Err(e) if e.to_string().contains("no agent")),
        "{r:?}"
    );

    c.request(Request::Input {
        pane,
        data: b"cat\r".to_vec(),
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(700));
    hook(&c, pane, "session-start", Some("sess-42"));
    let forked = match c
        .request(Request::CreatePane(CreatePane {
            fork_from: Some(pane),
            size: PaneSize {
                cols: 80,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    wait_match(&c, forked, "forked sess-42");
}

#[test]
fn panes_killed_with_the_daemon_are_restored() {
    // Quitting the app that spawned the daemon signals the daemon and every shell at once;
    // shells dying during that teardown must not drop out of the saved session.
    let env = Env::new("teardown");
    let mut daemon = env.start();
    let (c, events) = env.connect();
    let pane = create(&c, &env.dir);
    c.request(Request::Input {
        pane,
        data: b"echo before-teardown\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "before-teardown");
    // A shell that traps the signal and exits by itself, with an ordinary status. Not the
    // interactive shell: dash runs traps only once its prompt's read returns. `exec` keeps the
    // pane's pid; the quotes keep the echoed input from matching before the trap is set.
    let trapped = create(&c, &env.dir);
    c.request(Request::Input {
        pane: trapped,
        data:
            b"exec sh -c 'trap \"exit 0\" TERM; echo trap-\"set\"; while :; do sleep 0.05; done'\r"
                .to_vec(),
    })
    .unwrap();
    wait_match(&c, trapped, "trap-set");
    let pid = |pane| match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(i) => i.pid.expect("shell pid") as libc::pid_t,
        other => panic!("{other:?}"),
    };
    let (shell, trapped_shell) = (pid(pane), pid(trapped));
    // The shells go first, the worst order: the daemon sees them die before the signal lands.
    unsafe {
        libc::kill(-shell, libc::SIGKILL);
        libc::kill(shell, libc::SIGKILL);
        libc::kill(trapped_shell, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut closed = Vec::new();
    while closed.len() < 2 && Instant::now() < deadline {
        if let Ok(Event::PaneClosed { pane: p }) = events.recv_timeout(Duration::from_millis(100)) {
            closed.push(p);
        }
    }
    closed.sort();
    assert_eq!(
        closed,
        vec![pane, trapped],
        "both panes close for the clients at once"
    );
    unsafe {
        libc::kill(daemon.child.id() as libc::pid_t, libc::SIGTERM);
    }
    drop(c);
    daemon.wait_exit();

    let _daemon = env.start();
    let (c, _events) = env.connect();
    let panes = match c.request(Request::ListPanes).unwrap() {
        Response::Panes(p) => p,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        panes.iter().map(|p| p.id).collect::<Vec<_>>(),
        vec![pane, trapped]
    );
    assert!(capture(&c, pane).contains("before-teardown"));
}

#[test]
fn prompt_is_not_duplicated_by_resizing() {
    // fish redraws its prompt on SIGWINCH; the reflowed old prompt (with the padding before
    // the right prompt) must not stay behind as copies.
    let Some(fish) = [
        "/run/current-system/sw/bin/fish",
        "/opt/homebrew/bin/fish",
        "/usr/local/bin/fish",
    ]
    .into_iter()
    .map(String::from)
    .chain(
        std::env::var("HOME")
            .ok()
            .map(|h| format!("{h}/.nix-profile/bin/fish")),
    )
    .find(|p| Path::new(p).exists()) else {
        eprintln!("fish not installed; skipping");
        return;
    };
    let integration = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../shell-integration/fish/vendor_conf.d/thurm.fish"
    );
    let init = format!(
        "function fish_prompt; printf '~/dev/personal/home  main +/- ❯ '; end; \
         function fish_right_prompt; printf 'RPROMPT nix-shell-env at 14:45:43'; end; \
         function fish_greeting; end; source {integration}; \
         for i in (seq 1 60); printf 'output %s: %s\\n' $i (string repeat -n 120 x); end"
    );
    // Interactive fish with its own startup files but an empty user config (REFLOW_USER_CONFIG=1:
    // the user's real one, with their prompt, instead).
    let xdg = std::env::temp_dir().join(format!("thurm-e2e-fish-config-{}", std::process::id()));
    std::fs::create_dir_all(&xdg).unwrap();
    let xdg_arg = format!("XDG_CONFIG_HOME={}", xdg.display());
    let env = if std::env::var_os("REFLOW_USER_CONFIG").is_some() {
        Env::with_shell(
            "reflow",
            &[&fish, "-i", "-C", &format!("source {integration}")],
        )
    } else {
        Env::with_shell(
            "reflow",
            &["/usr/bin/env", &xdg_arg, &fish, "-i", "-C", &init],
        )
    };
    let _daemon = env.start();
    let (c, events) = env.connect();
    let cwd = if std::env::var_os("REFLOW_USER_CONFIG").is_some() {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    } else {
        env.dir.clone()
    };
    let pane = create(&c, &cwd);
    c.request(Request::Resize {
        pane,
        size: PaneSize {
            cols: 193,
            rows: 24,
            cell_width: 8,
            cell_height: 16,
        },
    })
    .unwrap();
    wait_match(&c, pane, "❯");
    c.request(Request::Input {
        pane,
        data: b"cd kubernetes/apps/q".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "apps/q");
    let gap: u64 = std::env::var("REFLOW_GAP_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(250);
    c.request(Request::Subscribe { pane }).unwrap();
    let mut copy: Option<thurm_term::Terminal> = None;
    let mut view = thurm_term::ClientView::new();
    // Split beside and below, then close each split and restore the full pane.
    for (cols, rows) in [
        (95, 24),
        (95, 12),
        (193, 12),
        (193, 24),
        (95, 24),
        (193, 24),
        (95, 24),
    ] {
        c.request(Request::Resize {
            pane,
            size: PaneSize {
                cols,
                rows,
                cell_width: 8,
                cell_height: 16,
            },
        })
        .unwrap();
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(gap) {
            match events.recv_timeout(Duration::from_millis(1)) {
                Ok(Event::Attach { size, state, .. }) => {
                    let mut t =
                        thurm_term::Terminal::new(size, thurm_term::EngineConfig::default());
                    t.replay(&state);
                    copy = Some(t);
                }
                Ok(Event::Output { data, .. }) => {
                    if std::env::var_os("REFLOW_TRACE").is_some() {
                        eprintln!(
                            "output {:?}: {:?}",
                            start.elapsed(),
                            String::from_utf8_lossy(&data)
                        );
                    }
                    copy.as_mut().unwrap().advance(&data);
                }
                Ok(Event::Resized { size, .. }) => copy.as_mut().unwrap().resize(size),
                _ => {}
            }
            let Some(t) = copy.as_mut() else { continue };
            if t.sync_deadline().is_some_and(|d| d <= Instant::now()) {
                t.flush_sync();
            }
            if let Some(frame) = t.snapshot(pane, &mut view) {
                let screen = t.screen_text();
                assert!(
                    screen.contains('❯'),
                    "empty prompt in a visible frame:\n{screen}"
                );
                if std::env::var_os("REFLOW_TRACE").is_some() {
                    eprintln!(
                        "resize {cols}x{rows}: {:?}, frame {} cols, prompt {:?}",
                        start.elapsed(),
                        frame.frame.cols,
                        screen
                            .lines()
                            .filter(|l| l.contains('❯') || l.contains("at "))
                            .collect::<Vec<_>>()
                    );
                }
                if std::env::var_os("REFLOW_USER_CONFIG").is_none() {
                    assert_eq!(
                        screen.matches("RPROMPT").count(),
                        1,
                        "incomplete right prompt in a visible frame:\n{screen}"
                    );
                }
            }
        }
        if std::env::var_os("REFLOW_USER_CONFIG").is_none() {
            let screen = capture(&c, pane);
            assert_eq!(
                screen.matches("RPROMPT").count(),
                1,
                "right prompt changed after resize to {cols}:\n{screen}"
            );
            assert!(
                screen.lines().any(|line| {
                    line.contains("❯ cd kubernetes/apps/q")
                        && line.contains("RPROMPT nix-shell-env at 14:45:43")
                }),
                "right prompt is on a separate row after resize to {cols}:\n{screen}"
            );
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    let screen = capture(&c, pane);
    assert_eq!(
        screen.matches("❯").count(),
        1,
        "prompt duplicated:\n{screen}"
    );
    // (The capture breaks wrapped rows too.)
    assert!(
        screen.replace('\n', "").contains("❯ cd kubernetes/apps/q"),
        "{screen}"
    );
    let _ = std::fs::remove_dir_all(&xdg);
}

/// A stand-in for thurm-intelligence: canned answers picked by the task's instructions.
const FAKE_MODEL: &str = r#"#!/bin/sh
echo '{"ready":true}'
while read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *choices*) text="no" ;;
    *"name terminal tabs"*) text="Fix login bug" ;;
    *"just finished a turn"*) text="Refactored auth; tests pass" ;;
    *"may be waiting for the user"*) text="Wants to run rm -rf build" ;;
    *"read terminal output"*) text="The config file is missing." ;;
    *) text="?" ;;
  esac
  echo "{\"id\":$id,\"text\":\"$text\"}"
done
"#;

fn next_notification(rx: &Receiver<Event>, pane: PaneId) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("no notification") {
            Event::Notify {
                pane: p,
                title,
                body,
                ..
            } if p == pane => return (title, body),
            _ => {}
        }
    }
}

#[test]
fn on_device_model_features() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("ai");
    std::fs::write(
        env.dir.join("config/config.toml"),
        "[terminal]\nshell = [\"/bin/sh\"]\n[ai]\nenabled = true\n",
    )
    .unwrap();
    let model = env.dir.join("fake-model");
    std::fs::write(&model, FAKE_MODEL).unwrap();
    std::fs::set_permissions(&model, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _d = env.start_with(&[("THURM_INTELLIGENCE", &model)]);
    let (c, rx) = env.connect();
    let pane = create(&c, &env.dir);
    c.request(Request::Input {
        pane,
        data: b"cat\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, r"cat");
    std::thread::sleep(Duration::from_millis(700));
    let hook = |event: &str, message: Option<&str>| {
        let r = c.request(Request::AgentHook {
            pane,
            agent: "codex".into(),
            event: event.into(),
            session_id: None,
            message: message.map(str::to_owned),
            transcript_path: None,
            pgrp: fg_pgrp(&c, pane),
        });
        assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
    };
    hook("session-start", None);
    hook("prompt-submit", Some("the login fails, fix it"));

    // What the agent asks for, in the notification and its state.
    hook("notification", Some("Codex needs approval"));
    assert_eq!(
        next_notification(&rx, pane),
        ("Codex".into(), "Wants to run rm -rf build".into())
    );
    assert_eq!(
        agent(&c, pane).unwrap().message.as_deref(),
        Some("Wants to run rm -rf build")
    );

    // A finished turn is summed up, then the session gets a title.
    hook("stop", None);
    let (_, body) = next_notification(&rx, pane);
    assert!(
        body.starts_with("Refactored auth; tests pass (finished after"),
        "{body}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while agent(&c, pane).and_then(|a| a.topic).as_deref() != Some("Fix login bug") {
        assert!(Instant::now() < deadline, "no generated title");
        std::thread::sleep(Duration::from_millis(100));
    }
    match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(i) => assert_eq!(i.title, "Fix login bug"),
        other => panic!("{other:?}"),
    }

    // Explain the last command, found by its prompt marks.
    c.request(Request::Input {
        pane,
        data: vec![0x04],
    })
    .unwrap();
    c.request(Request::Input {
        pane,
        data: br"printf '\033]133;A\007$ \033]133;B\007make\r\nconfig.toml: not found\r\n\033]133;A\007$ \033]133;B\007'"
            .iter()
            .copied()
            .chain(*b"\r")
            .collect(),
    })
    .unwrap();
    wait_match(&c, pane, r"(?m)^config\.toml: not found");
    std::thread::sleep(Duration::from_millis(300));
    match c.request(Request::Explain { pane }) {
        Ok(Response::Text(t)) => assert_eq!(t, "The config file is missing."),
        other => panic!("{other:?}\n{}", capture(&c, pane)),
    }
}

#[test]
fn explain_needs_the_model_turned_on() {
    let env = Env::new("ai-off");
    let _d = env.start();
    let (c, _rx) = env.connect();
    let pane = create(&c, &env.dir);
    let err = c.request(Request::Explain { pane }).unwrap_err();
    assert!(err.to_string().contains("[ai]"), "{err}");
}

#[test]
fn in_place_upgrade_keeps_panes_running() {
    let env = Env::new("upgrade");
    let daemon = env.start();
    let (c, _events) = env.connect();
    let hello_pid = match c.hello.lock().clone() {
        Some(Response::Hello {
            daemon_pid, build, ..
        }) => {
            assert_eq!(build, BUILD);
            daemon_pid
        }
        other => panic!("{other:?}"),
    };
    let pane = create(&c, &env.dir);
    c.request(Request::Input {
        pane,
        data: b"KEPT=kept-$((6*7)); echo before-upgrade\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "before-upgrade");
    let shell_pid = match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(info) => info.pid,
        other => panic!("{other:?}"),
    };
    c.request(Request::SetLayout {
        json: "{\"windows\":[]}".into(),
    })
    .unwrap();

    // What an update does: name the new binary, then SIGUSR2.
    let mut req = env.socket.as_os_str().to_owned();
    req.push(".upgrade");
    std::fs::write(PathBuf::from(req), env!("CARGO_BIN_EXE_thurmd")).unwrap();
    assert_eq!(
        unsafe { libc::kill(daemon.child.id() as i32, libc::SIGUSR2) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while c.is_alive() {
        assert!(Instant::now() < deadline, "old image kept the connection");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(c);

    let (c, _events) = env.connect();
    match c.hello.lock().clone() {
        // Same process (exec), so launchd and the pid file stay right.
        Some(Response::Hello { daemon_pid, .. }) => assert_eq!(daemon_pid, hello_pid),
        other => panic!("{other:?}"),
    }
    match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(info) => {
            assert!(info.alive);
            assert_eq!(info.pid, shell_pid, "the shell was replaced");
        }
        other => panic!("{other:?}"),
    }
    // Scrollback came along, and the shell (with its variable) is the same one.
    assert!(capture(&c, pane).contains("before-upgrade"));
    c.request(Request::Input {
        pane,
        data: b"echo after-$KEPT\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "after-kept-42");
    match c.request(Request::GetLayout).unwrap() {
        Response::Layout(Some(l)) => assert!(l.contains("windows")),
        other => panic!("{other:?}"),
    }
    // New panes still get fresh ids.
    assert!(create(&c, &env.dir) > pane);
}

#[test]
fn in_place_upgrade_loses_no_output() {
    let env = Env::new("upgrade-output");
    let daemon = env.start();
    let (c, _events) = env.connect();
    let pane = create(&c, &env.dir);
    // 4000 numbered lines in bursts, with the upgrade in the middle of them.
    c.request(Request::Input {
        pane,
        data: b"for i in $(seq 1 4000); do echo line-$i; [ $((i % 200)) -eq 0 ] && sleep 0.05; [ $i -eq 1000 ] && sleep 1; done; echo ALL-DONE\r"
            .to_vec(),
    })
    .unwrap();
    // The loop holds at line 1000 for a second; the upgrade comes once the bursts have
    // resumed. (Polled in the scrollback: Wait only sees the screen, and a debug build parses
    // slowly enough to miss a moment there.)
    let deadline = Instant::now() + Duration::from_secs(20);
    while !capture(&c, pane).contains("line-1000\n") {
        assert!(Instant::now() < deadline, "line 1000 never came");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(1100));
    let mut req = env.socket.as_os_str().to_owned();
    req.push(".upgrade");
    std::fs::write(PathBuf::from(req), env!("CARGO_BIN_EXE_thurmd")).unwrap();
    assert_eq!(
        unsafe { libc::kill(daemon.child.id() as i32, libc::SIGUSR2) },
        0
    );
    // The old image first drains what it read, up to 30 s in a slow debug build.
    let deadline = Instant::now() + Duration::from_secs(45);
    while c.is_alive() {
        assert!(Instant::now() < deadline, "old image kept the connection");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(c);
    let (c, _events) = env.connect();
    // A debug build parses slowly: wait in the scrollback, with room to spare.
    let deadline = Instant::now() + Duration::from_secs(60);
    let text = loop {
        let text = capture(&c, pane);
        if text.lines().any(|l| l.trim_end() == "ALL-DONE") {
            break text;
        }
        assert!(Instant::now() < deadline, "output never finished\n{text}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let numbers: Vec<u32> = text
        .lines()
        .filter_map(|l| l.trim_end().strip_prefix("line-")?.parse().ok())
        .collect();
    let missing: Vec<u32> = (1..=4000).filter(|n| !numbers.contains(n)).collect();
    assert!(
        missing.is_empty(),
        "lost {} lines: {:?}",
        missing.len(),
        &missing[..missing.len().min(20)]
    );
    assert!(
        numbers.windows(2).all(|w| w[0] < w[1]),
        "lines out of order"
    );
}

#[test]
fn upgrade_to_a_broken_binary_is_refused() {
    let env = Env::new("upgrade-refused");
    let daemon = env.start();
    let (c, _events) = env.connect();
    let pane = create(&c, &env.dir);
    let mut req = env.socket.as_os_str().to_owned();
    req.push(".upgrade");
    std::fs::write(PathBuf::from(req), "/usr/bin/false").unwrap();
    unsafe { libc::kill(daemon.child.id() as i32, libc::SIGUSR2) };
    std::thread::sleep(Duration::from_millis(500));
    // Still the same connection, and the pane still works.
    assert!(c.is_alive());
    c.request(Request::Input {
        pane,
        data: b"echo still-here\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "still-here");
}

#[test]
fn completion_uses_only_the_path_the_shell_reported() {
    let env = Env::with_shell("shellpath", &["/bin/bash"]);
    let home = env.dir.join("home");
    let bin = env.dir.join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("thurmzzcmd");
    std::fs::write(&tool, "#!/bin/sh\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _daemon = env.start_with(&[("HOME", &home)]);
    let (c, _events) = env.connect();
    let pane = create(&c, &env.dir);
    // The token is not in the environment of programs the shell runs.
    let line = format!(
        "echo token=[$THURM_SHELL_TOKEN]; export PATH={}:$PATH\r",
        bin.display()
    );
    c.request(Request::Input {
        pane,
        data: line.into_bytes(),
    })
    .unwrap();
    wait_match(&c, pane, r"token=\[\]");
    // Program output claiming another PATH is ignored.
    c.request(Request::Input {
        pane,
        data: b"printf '\\033]633;P;ThurmPath=/nonexistent\\007'; echo printed\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "(?m)^printed");
    c.request(Request::Input {
        pane,
        data: b"thurmzz".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "(?m)thurmzz$");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match c.request(Request::Complete { pane }).unwrap() {
            Response::Completions(comp) if comp.items.iter().any(|i| i.text == "thurmzzcmd") => {
                break;
            }
            other => assert!(
                Instant::now() < deadline,
                "no completion: {other:?}\n{}",
                capture(&c, pane)
            ),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn unknown_requests_are_answered_not_dropped() {
    use std::io::Write;
    let env = Env::new("unknown-request");
    let _daemon = env.start();
    let mut s = std::os::unix::net::UnixStream::connect(&env.socket).unwrap();
    // A request of a newer protocol: id 7, then request variant 999 (postcard varints), which
    // this daemon doesn't have.
    let frame = [7u8, 0xE7, 0x07];
    let mut buf = (frame.len() as u32).to_le_bytes().to_vec();
    buf.extend_from_slice(&frame);
    s.write_all(&buf).unwrap();
    // Then a known one on the same connection.
    codec::write_message(
        &mut s,
        &Envelope {
            id: 8,
            request: Request::ListPanes,
        },
    )
    .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut r = std::io::BufReader::new(s);
    let mut answered = Vec::new();
    while answered.len() < 2 {
        match codec::read_message::<_, ServerMessage>(&mut r).unwrap() {
            Some(ServerMessage::Response { id, result }) => answered.push((id, result.is_ok())),
            Some(ServerMessage::Event(_)) => {}
            None => panic!("the daemon dropped the connection"),
        }
    }
    assert_eq!(answered, vec![(7, false), (8, true)]);
}

/// Spawns another daemon on `env`'s socket and returns whether it exited by itself in time.
fn second_daemon_exits(env: &Env) -> bool {
    let mut child = Command::new(env!("CARGO_BIN_EXE_thurmd"))
        .args(["--foreground", "--socket"])
        .arg(&env.socket)
        .env("THURM_CONFIG_DIR", env.dir.join("config"))
        .env("THURM_STATE_DIR", env.dir.join("state2"))
        .spawn()
        .expect("spawn daemon");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

#[test]
fn one_daemon_per_socket() {
    let env = Env::new("single");
    // A socket directory given with --socket keeps its permissions.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&env.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut daemon = env.start();
    let (c, _events) = env.connect();
    let pane = create(&c, &env.dir);
    assert!(second_daemon_exits(&env), "a second daemon kept running");
    // The first one still has the socket and its pane.
    assert!(matches!(
        c.request(Request::PaneInfo { pane }).unwrap(),
        Response::PaneInfo(_)
    ));
    let (c2, _e2) = env.connect();
    assert!(matches!(
        c2.request(Request::PaneInfo { pane }).unwrap(),
        Response::PaneInfo(_)
    ));
    let mode = std::fs::metadata(&env.dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);

    // Through an in-place upgrade the lock stays held.
    let mut req = env.socket.as_os_str().to_owned();
    req.push(".upgrade");
    std::fs::write(PathBuf::from(req), env!("CARGO_BIN_EXE_thurmd")).unwrap();
    assert_eq!(
        unsafe { libc::kill(daemon.child.id() as i32, libc::SIGUSR2) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while c.is_alive() {
        assert!(Instant::now() < deadline, "old image kept the connection");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop((c, c2));
    let (c, _events) = env.connect();
    assert!(matches!(
        c.request(Request::PaneInfo { pane }).unwrap(),
        Response::PaneInfo(_)
    ));
    assert!(
        second_daemon_exits(&env),
        "a second daemon started after the upgrade"
    );
    assert!(daemon.child.try_wait().unwrap().is_none());
}

#[test]
fn daemons_started_together_leave_one() {
    let env = Env::new("together");
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_thurmd"))
            .args(["--foreground", "--socket"])
            .arg(&env.socket)
            .env("THURM_CONFIG_DIR", env.dir.join("config"))
            .env("THURM_STATE_DIR", env.dir.join("state"))
            .spawn()
            .expect("spawn daemon")
    };
    let mut children = [spawn(), spawn(), spawn()];
    std::thread::sleep(Duration::from_secs(2));
    let running = children
        .iter_mut()
        .map(|c| c.try_wait().unwrap().is_none())
        .filter(|&alive| alive)
        .count();
    for c in &mut children {
        let _ = c.kill();
        let _ = c.wait();
    }
    assert_eq!(running, 1);
}

#[test]
fn starts_on_a_fresh_default_socket() {
    let env = Env::new("fresh-default");
    // Short: a socket path must fit in 104 bytes, and CI's temporary directory is long.
    let runtime = PathBuf::from(format!("/tmp/thurm-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&runtime);
    std::fs::create_dir_all(&runtime).unwrap();
    // No --socket: the default directory is created (private) under XDG_RUNTIME_DIR.
    let mut child = Command::new(env!("CARGO_BIN_EXE_thurmd"))
        .arg("--foreground")
        .env("XDG_RUNTIME_DIR", &runtime)
        .env_remove("THURM_SOCKET")
        .env("THURM_CONFIG_DIR", env.dir.join("config"))
        .env("THURM_STATE_DIR", env.dir.join("state"))
        .spawn()
        .expect("spawn daemon");
    let uid = unsafe { libc::getuid() };
    let dir = runtime.join(format!("thurm-{uid}"));
    let socket = dir.join("thurmd.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "the daemon exited instead of starting"
        );
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&runtime);
    assert_eq!(mode, 0o700);
}

// macOS starts the account's login shell through login(1), not $SHELL.
#[cfg(target_os = "linux")]
#[test]
fn bash_prompt_command_array_keeps_output_on_resize() {
    // systemd's profile script (OSC 3008) appends to a PROMPT_COMMAND array; bash 5.1+ runs
    // every element. Output must stay output: libghostty-vt clears a prompt on resize.
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let env = Env::new("bash-prompt-array");
    std::fs::write(
        env.dir.join("config/config.toml"),
        "[session]\nsnapshot_interval_secs = 1\n",
    )
    .unwrap();
    let home = env.dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".bash_profile"),
        "PS1='[test]$ '\n\
         _ctx() { printf '\\e]3008;start=x;type=shell\\e\\\\'; }\n\
         [ -n \"$(declare -p PROMPT_COMMAND 2>/dev/null)\" ] || PROMPT_COMMAND+=('')\n\
         PROMPT_COMMAND+=(_ctx)\n",
    )
    .unwrap();
    let _daemon = env.start_with(&[("SHELL", Path::new("/bin/bash")), ("HOME", &home)]);
    let (c, _events) = env.connect();
    let pane = create(&c, &env.dir);
    let resize = |rows| {
        c.request(Request::Resize {
            pane,
            size: PaneSize {
                cols: 53,
                rows,
                cell_width: 10,
                cell_height: 22,
            },
        })
        .unwrap();
    };
    resize(31);
    wait_match(&c, pane, r"\[test\]\$");
    c.request(Request::Input {
        pane,
        data: b"printf 'out%s\\n' 1 2 3\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, r"out3\n\[test\]\$");
    resize(15);
    std::thread::sleep(Duration::from_millis(500));
    let after = capture(&c, pane);
    assert!(after.contains("out2"), "{after}");
}

#[test]
fn terminal_attachment_owns_input_and_restores_size() {
    let env = Env::new("terminal-attachment");
    let _daemon = env.start();
    let (ui, _) = env.connect();
    let pane = create(&ui, &env.dir);
    let original = match ui.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(info) => info.size,
        other => panic!("{other:?}"),
    };
    let (owner, events) = env.connect();
    let attached_size = PaneSize {
        cols: 93,
        rows: 31,
        ..original
    };
    owner
        .request(Request::AttachTerminal {
            pane,
            size: attached_size,
        })
        .unwrap();
    let state = events
        .iter()
        .find_map(|event| match event {
            Event::Attach { size, state, .. } => {
                assert_eq!(size, attached_size);
                Some(state)
            }
            _ => None,
        })
        .unwrap();
    assert!(!state.is_empty());
    for request in [
        Request::AttachTerminal {
            pane,
            size: original,
        },
        Request::Input {
            pane,
            data: b"echo forbidden\r".to_vec(),
        },
        Request::Paste {
            pane,
            text: "forbidden".into(),
        },
        Request::Resize {
            pane,
            size: original,
        },
        Request::DetachTerminal { pane },
        Request::AnswerPermission {
            pane,
            prompt: 1,
            allow: true,
        },
    ] {
        assert!(ui.request(request).is_err());
    }
    owner
        .request(Request::Input {
            pane,
            data: b"echo attached-$((42+1))\r".to_vec(),
        })
        .unwrap();
    wait_match(&owner, pane, "attached-43");
    owner.request(Request::DetachTerminal { pane }).unwrap();
    assert!(
        matches!(ui.request(Request::PaneInfo { pane }).unwrap(), Response::PaneInfo(info) if info.size == original)
    );
    ui.request(Request::Input {
        pane,
        data: b"echo released-$((42+2))\r".to_vec(),
    })
    .unwrap();
    wait_match(&ui, pane, "released-44");
    owner
        .request(Request::AttachTerminal {
            pane,
            size: attached_size,
        })
        .unwrap();
    drop(owner);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(ui.request(Request::PaneInfo { pane }).unwrap(), Response::PaneInfo(info) if info.size == original)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "attachment was not released after disconnect"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    ui.request(Request::AttachTerminal {
        pane,
        size: attached_size,
    })
    .unwrap();
    ui.request(Request::DetachTerminal { pane }).unwrap();
}

fn reporting_pane(c: &Client, cwd: &Path) -> (PaneId, u32) {
    let pane = match c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec!["/bin/cat".into()]),
            cwd: Some(cwd.display().to_string()),
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Response::PaneInfo(info) = c.request(Request::PaneInfo { pane }).unwrap()
            && let Some(process) = info.foreground
            && process.name == "cat"
        {
            return (pane, process.pid);
        }
        assert!(Instant::now() < deadline, "reporting process did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn agent_reporting_checks_ownership_order_and_release() {
    let env = Env::new("report-order");
    let _daemon = env.start();
    let (c, _) = env.connect();
    let (pane, owner_pid) = reporting_pane(&c, &env.dir);
    let mut report = AgentReport {
        agent: "example".into(),
        owner_pid,
        instance: "process-a".into(),
        sequence: 1,
        status: AgentStatus::Idle,
        session_id: Some("session-42".into()),
        message: None,
        resume_argv: None,
    };
    let send = |report: &AgentReport| {
        c.request(Request::AgentReport {
            pane,
            report: report.clone(),
        })
    };
    report.owner_pid = std::process::id();
    assert!(
        send(&report).is_err(),
        "a process outside the pane cannot report"
    );
    report.owner_pid = owner_pid;
    send(&report).unwrap();
    report.sequence = 3;
    report.status = AgentStatus::Working;
    send(&report).unwrap();
    report.sequence = 2;
    report.status = AgentStatus::Done;
    assert!(send(&report).is_err());
    assert_eq!(agent(&c, pane).unwrap().status, AgentStatus::Working);
    let explanation = match c.request(Request::AgentExplain { pane }).unwrap() {
        Response::AgentExplanation(value) => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(explanation.source, "report");
    assert_eq!(explanation.foreground.unwrap().pid, owner_pid);
    assert_eq!(explanation.report_sequence, Some(3));
    // A legacy hook cannot replace state owned by the public interface.
    hook(&c, pane, "stop", Some("wrong-session"));
    assert_eq!(agent(&c, pane).unwrap().kind, "example");
    c.request(Request::AgentRelease {
        pane,
        owner_pid,
        instance: report.instance.clone(),
        sequence: 4,
    })
    .unwrap();
    assert!(agent(&c, pane).is_none());
    report.sequence = 5;
    assert!(send(&report).is_err());
}

#[test]
fn agent_reporting_restores_exact_arguments_without_shell_expansion() {
    let env = Env::new("report-resume");
    let mut daemon = env.start();
    let (c, _) = env.connect();
    let (pane, owner_pid) = reporting_pane(&c, &env.dir);
    let marker = env.dir.join("must-not-exist");
    let literal = format!("session 'quoted' $(touch {}) ; *", marker.display());
    let script = env.dir.join("resume.sh");
    std::fs::write(&script, "printf 'RESUMED:%s\\n' \"$1\"\nexec /bin/cat\n").unwrap();
    let argv = vec![
        "/bin/sh".into(),
        script.display().to_string(),
        literal.clone(),
    ];
    c.request(Request::AgentReport {
        pane,
        report: AgentReport {
            agent: "example".into(),
            owner_pid,
            instance: "process-a".into(),
            sequence: 1,
            status: AgentStatus::Idle,
            session_id: Some("session-42".into()),
            message: None,
            resume_argv: Some(argv.clone()),
        },
    })
    .unwrap();
    c.request(Request::SaveSnapshot).unwrap();
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.dir.join("state/session.json")).unwrap())
            .unwrap();
    assert_eq!(snapshot["panes"][0]["agent_session"], "session-42");
    assert_eq!(
        snapshot["panes"][0]["agent_resume"],
        serde_json::json!(argv)
    );
    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);
    let _restored = env.start();
    let (c, _) = env.connect();
    wait_match(&c, pane, "RESUMED:");
    let text = capture(&c, pane);
    // Long terminal lines can wrap, but the literal shell operators must remain.
    assert!(text.contains("session 'quoted' $(touch"), "{text}");
    assert!(text.contains("; *"), "{text}");
    assert!(
        !marker.exists(),
        "resume arguments were evaluated by a shell"
    );
}

#[test]
fn agent_reporting_survives_in_place_upgrade() {
    let env = Env::new("report-upgrade");
    let daemon = env.start();
    let (c, _) = env.connect();
    let (pane, owner_pid) = reporting_pane(&c, &env.dir);
    let mut report = AgentReport {
        agent: "example".into(),
        owner_pid,
        instance: "process-a".into(),
        sequence: 9,
        status: AgentStatus::Working,
        session_id: Some("session-42".into()),
        message: None,
        resume_argv: Some(vec!["/bin/cat".into()]),
    };
    c.request(Request::AgentReport {
        pane,
        report: report.clone(),
    })
    .unwrap();
    let mut request_path = env.socket.as_os_str().to_owned();
    request_path.push(".upgrade");
    std::fs::write(PathBuf::from(request_path), env!("CARGO_BIN_EXE_thurmd")).unwrap();
    assert_eq!(
        unsafe { libc::kill(daemon.child.id() as i32, libc::SIGUSR2) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while c.is_alive() {
        assert!(
            Instant::now() < deadline,
            "upgrade did not close the connection"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(c);
    let (c, _) = env.connect();
    assert_eq!(agent(&c, pane).unwrap().status, AgentStatus::Working);
    report.sequence = 8;
    report.status = AgentStatus::Done;
    assert!(
        c.request(Request::AgentReport {
            pane,
            report: report.clone()
        })
        .is_err()
    );
    c.request(Request::SaveSnapshot).unwrap();
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.dir.join("state/session.json")).unwrap())
            .unwrap();
    assert_eq!(
        snapshot["panes"][0]["agent_resume"],
        serde_json::json!(["/bin/cat"])
    );
    report.sequence = 10;
    c.request(Request::AgentReport { pane, report }).unwrap();
    assert_eq!(agent(&c, pane).unwrap().status, AgentStatus::Done);
}

/// Run only inside the pane created by the owner-exit test.
#[test]
fn agent_reporting_child_fixture() {
    let Ok(path) = std::env::var("THURM_REPORT_TEST_OWNER") else {
        return;
    };
    let mut owner = Command::new("/bin/sleep").arg("60").spawn().unwrap();
    std::fs::write(path, owner.id().to_string()).unwrap();
    std::thread::spawn(move || {
        let _ = owner.wait();
    });
    let mut input = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
}

#[test]
fn agent_reporting_owner_exit_releases_a_live_group() {
    let env = Env::new("report-owner-exit");
    let _daemon = env.start();
    let (c, _) = env.connect();
    let owner_path = env.dir.join("owner.pid");
    let pane = match c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec![
                std::env::current_exe().unwrap().display().to_string(),
                "--exact".into(),
                "agent_reporting_child_fixture".into(),
                "--nocapture".into(),
            ]),
            env: vec![(
                "THURM_REPORT_TEST_OWNER".into(),
                owner_path.display().to_string(),
            )],
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !owner_path.exists() {
        assert!(Instant::now() < deadline, "owner process did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let owner_pid: u32 = std::fs::read_to_string(&owner_path)
        .unwrap()
        .parse()
        .unwrap();
    let mut report = AgentReport {
        agent: "example".into(),
        owner_pid,
        instance: "child-a".into(),
        sequence: 1,
        status: AgentStatus::Idle,
        session_id: None,
        message: None,
        resume_argv: None,
    };
    c.request(Request::AgentReport {
        pane,
        report: report.clone(),
    })
    .unwrap();
    assert_eq!(unsafe { libc::kill(owner_pid as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while agent(&c, pane).is_some() {
        assert!(Instant::now() < deadline, "dead owner kept agent state");
        std::thread::sleep(Duration::from_millis(30));
    }
    let leader = match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(info) => {
            assert!(info.alive);
            info.pid.unwrap()
        }
        other => panic!("{other:?}"),
    };
    assert_ne!(owner_pid, leader);
    report.owner_pid = leader;
    report.instance = "leader-b".into();
    c.request(Request::AgentReport { pane, report }).unwrap();
    assert_eq!(agent(&c, pane).unwrap().kind, "example");
}
