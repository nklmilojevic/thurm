//! The server driven by the app's own client, against the in-process ssh server of
//! `thurm_ssh::testing` (shells are a local `sh`; mosh is the local `mosh-server`, when there).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use thurm_client::Client;
use thurm_proto::{CreatePane, Event, PaneSize, Request, Response};
use thurm_ssh::testing::{Options, TestKey, TestServer, serve as ssh_server, serve_with, target};

use super::*;

/// Held by the tests that widen `SSH_START_DELAY_MS`, which all servers here read.
static DELAY: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

fn direct_target(server: &TestServer, mode: Mode) -> DirectTarget {
    let t = target(server);
    DirectTarget {
        host: t.host,
        port: t.port,
        user: t.user,
        host_keys: t.host_keys,
        mode,
        mosh_server: None,
        mosh_ports: None,
    }
}

struct App {
    client: Arc<Client>,
    events: mpsc::Receiver<Event>,
    _direct: Direct,
}

fn open(server: &TestServer, key: Arc<TestKey>, mode: Mode) -> App {
    let (stream, direct) = super::serve(direct_target(server, mode), key).unwrap();
    let (tx, events) = mpsc::channel();
    let client = Client::from_stream(
        stream,
        "ios-test",
        false,
        &[thurm_proto::CAP_MOBILE],
        move |ev| {
            let _ = tx.send(ev);
        },
        || {},
    )
    .unwrap();
    App {
        client,
        events,
        _direct: direct,
    }
}

fn size(cols: u16, rows: u16) -> PaneSize {
    PaneSize {
        cols,
        rows,
        cell_width: 8,
        cell_height: 16,
    }
}

impl App {
    fn create(&self) -> PaneId {
        let req = CreatePane {
            command: None,
            cwd: None,
            env: vec![],
            size: size(80, 24),
            agent_preset: None,
            inherit_cwd_from: None,
            hold: false,
            fork_from: None,
        };
        match self.client.request(Request::CreatePane(req)) {
            Ok(Response::PaneCreated { pane }) => pane,
            other => panic!("{other:?}"),
        }
    }

    /// Waits for an event `pick` takes, skipping the others.
    fn wait<T>(&self, what: &str, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ev = self
                .events
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {what}"));
            if let Some(t) = pick(ev) {
                return t;
            }
        }
    }

    /// Waits until `pane`'s output has shown `text`.
    fn wait_output(&self, pane: PaneId, text: &str) {
        let mut seen = Vec::new();
        self.wait(text, |ev| match ev {
            Event::Output { pane: p, data } if p == pane => {
                seen.extend(data);
                String::from_utf8_lossy(&seen).contains(text).then_some(())
            }
            Event::Attach { pane: p, state, .. } if p == pane => {
                seen.extend(state);
                String::from_utf8_lossy(&seen).contains(text).then_some(())
            }
            _ => None,
        });
    }

    fn panes(&self) -> Vec<PaneInfo> {
        match self.client.request(Request::ListPanes) {
            Ok(Response::Panes(p)) => p,
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn serves_ssh_shells_as_panes() {
    let key = TestKey::new(1);
    let server = ssh_server(&key);
    let app = open(&server, key, Mode::Ssh);
    let one = app.create();
    let two = app.create();
    assert_ne!(one, two);
    let panes = app.panes();
    assert_eq!(panes.iter().map(|p| p.id).collect::<Vec<_>>(), [one, two]);
    assert!(
        panes
            .iter()
            .all(|p| p.alive && p.program.as_ref().unwrap().id == "ssh")
    );
    assert_eq!(panes[0].title, "me@127.0.0.1");

    app.client.send(Request::Subscribe { pane: one }).unwrap();
    app.wait("attach", |ev| {
        matches!(ev, Event::Attach { pane, .. } if pane == one).then_some(())
    });
    app.client
        .send(Request::Input {
            pane: one,
            data: b"echo from-one-$((40+2))\n".to_vec(),
        })
        .unwrap();
    app.wait_output(one, "from-one-42");

    app.client
        .send(Request::Resize {
            pane: one,
            size: size(100, 30),
        })
        .unwrap();
    let resized = app.wait("resized", |ev| match ev {
        Event::Resized { pane, size } if pane == one => Some(size),
        _ => None,
    });
    assert_eq!((resized.cols, resized.rows), (100, 30));

    // No window to place panes in: they stay loose.
    assert!(app.client.request(Request::CheckUi).is_err());
    assert!(matches!(
        app.client.request(Request::GetLayout),
        Ok(Response::Layout(None))
    ));

    // A shell that exits closes its pane.
    app.client
        .send(Request::Input {
            pane: two,
            data: b"exit 3\n".to_vec(),
        })
        .unwrap();
    let code = app.wait("exit", |ev| match ev {
        Event::PaneExited { pane, code } if pane == two => Some(code),
        _ => None,
    });
    assert_eq!(code, Some(3));
    app.wait("close", |ev| {
        matches!(ev, Event::PaneClosed { pane } if pane == two).then_some(())
    });

    assert!(matches!(
        app.client.request(Request::ClosePane { pane: one }),
        Ok(Response::Ok)
    ));
    app.wait("close", |ev| {
        matches!(ev, Event::PaneClosed { pane } if pane == one).then_some(())
    });
    assert!(app.panes().is_empty());
    let sizes = server.sizes.lock().unwrap().clone();
    assert!(sizes.contains(&("".into(), 100, 30)), "{sizes:?}");
}

#[test]
fn reports_host_keys_to_trust() {
    let key = TestKey::new(1);
    let server = ssh_server(&key);
    let mut t = direct_target(&server, Mode::Ssh);
    let expected = t.host_keys[0].clone();
    t.host_keys.clear();
    match super::serve(t.clone(), key.clone()) {
        Err(DirectError::UnknownHostKey(k)) => assert_eq!(k, expected),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("served an untrusted host"),
    }
    t.host_keys = vec!["SHA256:other".into()];
    assert!(matches!(
        super::serve(t, key),
        Err(DirectError::HostKeyChanged(_))
    ));
    let t = direct_target(&server, Mode::Ssh);
    assert!(matches!(
        super::serve(t, TestKey::new(2)),
        Err(DirectError::KeyRejected)
    ));
}

/// With `mosh-server` on this machine, a mosh pane; without, the ssh fallback says why.
#[test]
fn serves_mosh_sessions_or_falls_back_to_ssh() {
    let key = TestKey::new(1);
    let server = ssh_server(&key);
    let app = open(&server, key, Mode::Mosh);
    let pane = app.create();
    let info = app.panes().into_iter().find(|p| p.id == pane).unwrap();
    let transport = info.program.unwrap().id;
    app.client.send(Request::Subscribe { pane }).unwrap();
    app.wait("attach", |ev| {
        matches!(ev, Event::Attach { pane: p, .. } if p == pane).then_some(())
    });
    if transport == "ssh" {
        eprintln!("no mosh-server here: checked the fallback only");
        app.client
            .send(Request::Input {
                pane,
                data: b"echo fell-back\n".to_vec(),
            })
            .unwrap();
        app.wait_output(pane, "fell-back");
        return;
    }
    assert_eq!(transport, "mosh");
    app.client
        .send(Request::Input {
            pane,
            data: b"echo over-mosh-$((6*7))\r".to_vec(),
        })
        .unwrap();
    app.wait_output(pane, "over-mosh-42");
    app._direct.wake();
    app.client
        .send(Request::Input {
            pane,
            data: b"echo after-wake\r".to_vec(),
        })
        .unwrap();
    app.wait_output(pane, "after-wake");
    // The shell exits: mosh says so, and the pane closes.
    app.client
        .send(Request::Input {
            pane,
            data: b"exit\r".to_vec(),
        })
        .unwrap();
    app.wait("close", |ev| {
        matches!(ev, Event::PaneClosed { pane: p } if p == pane).then_some(())
    });
}

/// A `mosh-server` that takes a second to start, so a close can come while it does; `None`
/// without mosh-server here.
fn slow_mosh_server(name: &str) -> Option<String> {
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v mosh-server"])
        .output()
        .ok()?;
    let real = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if real.is_empty() {
        return None;
    }
    let path = format!("/tmp/thurm-direct-{name}-{}.sh", std::process::id());
    std::fs::write(&path, format!("#!/bin/sh\nsleep 1\nexec {real} \"$@\"\n")).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    Some(path)
}

/// Whether a mosh-server listening on `port` (as started here) runs.
fn mosh_server_on(port: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", &format!("mosh-server new .*-p {port}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn wait_gone(port: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while mosh_server_on(port) {
        assert!(
            Instant::now() < deadline,
            "the mosh-server on {port} kept running"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Opens a mosh pane through the slow server on `port` in the background; returns the
/// request's outcome and the pane's id once it shows (still starting).
fn start_slowly(
    server: &TestServer,
    key: Arc<TestKey>,
    port: &str,
    slow: String,
) -> (
    App,
    std::thread::JoinHandle<Result<Response, String>>,
    PaneId,
) {
    let mut t = direct_target(server, Mode::MoshOnly);
    t.mosh_server = Some(slow);
    t.mosh_ports = Some(port.into());
    let (stream, direct) = super::serve(t, key).unwrap();
    let (tx, events) = mpsc::channel();
    let client = Client::from_stream(
        stream,
        "ios-test",
        false,
        &[thurm_proto::CAP_MOBILE],
        move |ev| {
            let _ = tx.send(ev);
        },
        || {},
    )
    .unwrap();
    let app = App {
        client,
        events,
        _direct: direct,
    };
    let c = app.client.clone();
    let creating = std::thread::spawn(move || {
        let req = CreatePane {
            command: None,
            cwd: None,
            env: vec![],
            size: size(80, 24),
            agent_preset: None,
            inherit_cwd_from: None,
            hold: false,
            fork_from: None,
        };
        c.request(Request::CreatePane(req))
            .map_err(|e| e.to_string())
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let pane = loop {
        if let Some(p) = app.panes().first() {
            break p.id;
        }
        assert!(Instant::now() < deadline, "the pane never showed");
        std::thread::sleep(Duration::from_millis(10));
    };
    (app, creating, pane)
}

#[test]
fn closing_a_pane_while_it_starts_ends_its_server() {
    let Some(slow) = slow_mosh_server("close") else {
        eprintln!("no mosh-server here; skipped");
        return;
    };
    let port = "60987";
    let key = TestKey::new(1);
    let server = ssh_server(&key);
    let (app, creating, pane) = start_slowly(&server, key, port, slow.clone());
    assert!(matches!(
        app.client.request(Request::ClosePane { pane }),
        Ok(Response::Ok)
    ));
    let outcome = creating.join().unwrap();
    assert!(outcome.is_err(), "{outcome:?}");
    assert!(app.panes().is_empty());
    wait_gone(port);
    let _ = std::fs::remove_file(slow);
}

#[test]
fn disconnecting_while_a_pane_starts_ends_its_server() {
    let Some(slow) = slow_mosh_server("disconnect") else {
        eprintln!("no mosh-server here; skipped");
        return;
    };
    let port = "60988";
    let key = TestKey::new(1);
    let server = ssh_server(&key);
    let (app, creating, _) = start_slowly(&server, key, port, slow.clone());
    // The app goes away while the server is still starting.
    drop(app);
    let _ = creating.join();
    // It starts after all, and is ended then.
    std::thread::sleep(Duration::from_millis(1500));
    wait_gone(port);
    let _ = std::fs::remove_file(slow);
}

/// A `mosh-server` that starts but whose UDP never answers (a firewall dropping it): a socket
/// that is bound and never read.
fn silent_mosh_server(name: &str) -> (String, std::net::UdpSocket) {
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let path = format!("/tmp/thurm-direct-{name}-{}.sh", std::process::id());
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho 'MOSH CONNECT {port} AAAAAAAAAAAAAAAAAAAAAA'\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path, silent)
}

#[test]
fn mosh_without_udp_falls_back_to_ssh() {
    let (fake, _silent) = silent_mosh_server("silent-fallback");
    let key = TestKey::new(1);
    let server = ssh_server(&key);
    let mut t = direct_target(&server, Mode::Mosh);
    t.mosh_server = Some(fake.clone());
    let (stream, direct) = super::serve(t, key).unwrap();
    let (tx, events) = mpsc::channel();
    let client = Client::from_stream(
        stream,
        "ios-test",
        false,
        &[thurm_proto::CAP_MOBILE],
        move |ev| {
            let _ = tx.send(ev);
        },
        || {},
    )
    .unwrap();
    let app = App {
        client,
        events,
        _direct: direct,
    };
    let pane = app.create();
    let info = app.panes().into_iter().find(|p| p.id == pane).unwrap();
    assert_eq!(info.program.unwrap().id, "ssh");
    app.client.send(Request::Subscribe { pane }).unwrap();
    app.wait_output(pane, "no answer from mosh-server over UDP");
    let _ = std::fs::remove_file(fake);
}

#[test]
fn mosh_and_ssh_both_failing_says_both() {
    let (fake, _silent) = silent_mosh_server("silent-both");
    let key = TestKey::new(1);
    let server = serve_with(
        &key,
        Options {
            no_pty: true,
            ..Default::default()
        },
    );
    let mut t = direct_target(&server, Mode::Mosh);
    t.mosh_server = Some(fake.clone());
    let (stream, _direct) = super::serve(t, key).unwrap();
    let client = Client::from_stream(
        stream,
        "ios-test",
        false,
        &[thurm_proto::CAP_MOBILE],
        |_| {},
        || {},
    )
    .unwrap();
    let req = CreatePane {
        command: None,
        cwd: None,
        env: vec![],
        size: size(80, 24),
        agent_preset: None,
        inherit_cwd_from: None,
        hold: false,
        fork_from: None,
    };
    match client.request(Request::CreatePane(req)) {
        Err(e) => {
            let e = e.to_string();
            assert!(e.starts_with("mosh: "), "{e}");
            assert!(e.contains("; then ssh: "), "{e}");
            assert!(e.contains("refused a terminal"), "{e}");
        }
        Ok(r) => panic!("{r:?}"),
    }
    let _ = std::fs::remove_file(fake);
}

#[test]
fn mosh_only_without_udp_fails_the_pane() {
    let (fake, _silent) = silent_mosh_server("silent-only");
    let key = TestKey::new(1);
    let server = ssh_server(&key);
    let mut t = direct_target(&server, Mode::MoshOnly);
    t.mosh_server = Some(fake.clone());
    let (stream, _direct) = super::serve(t, key).unwrap();
    let client = Client::from_stream(
        stream,
        "ios-test",
        false,
        &[thurm_proto::CAP_MOBILE],
        |_| {},
        || {},
    )
    .unwrap();
    let req = CreatePane {
        command: None,
        cwd: None,
        env: vec![],
        size: size(80, 24),
        agent_preset: None,
        inherit_cwd_from: None,
        hold: false,
        fork_from: None,
    };
    match client.request(Request::CreatePane(req)) {
        Err(e) => assert!(e.to_string().contains("no answer from mosh-server"), "{e}"),
        Ok(r) => panic!("{r:?}"),
    }
    assert!(matches!(
        client.request(Request::ListPanes),
        Ok(Response::Panes(p)) if p.is_empty()
    ));
    let _ = std::fs::remove_file(fake);
}

/// A shell asking the terminal something as it starts (a prompt asking for the cursor, say)
/// gets its answer, even when it asks before the pane has the shell.
#[test]
fn a_shell_starting_with_a_terminal_query_gets_the_answer() {
    let key = TestKey::new(1);
    let server = serve_with(
        &key,
        Options {
            shell_init: Some(
                r#"printf '\033[c'; reply=$(dd bs=1 count=3 2>/dev/null); printf 'got:%s\n' "$reply" | cat -v"#
                    .into(),
            ),
            ..Default::default()
        },
    );
    let app = open(&server, key, Mode::Ssh);
    // The query comes before the pane has the shell.
    let _delay = DELAY.lock();
    super::SSH_START_DELAY_MS.store(500, Ordering::Relaxed);
    let pane = app.create();
    super::SSH_START_DELAY_MS.store(0, Ordering::Relaxed);
    app.client.send(Request::Subscribe { pane }).unwrap();
    app.wait_output(pane, "got:^[[?");
}

/// A shell whose channel ends (without an exit status: a dropped connection) before the pane
/// has it leaves an ended pane, not one with a shell installed after the fact.
#[test]
fn a_shell_ending_before_its_pane_has_it_stays_ended() {
    let key = TestKey::new(1);
    let server = serve_with(
        &key,
        Options {
            shell_init: Some("exit 0".into()),
            no_exit_status: true,
            ..Default::default()
        },
    );
    let app = open(&server, key, Mode::Ssh);
    let _delay = DELAY.lock();
    super::SSH_START_DELAY_MS.store(500, Ordering::Relaxed);
    let pane = app.create();
    super::SSH_START_DELAY_MS.store(0, Ordering::Relaxed);
    let info = app.panes().into_iter().find(|p| p.id == pane).unwrap();
    assert!(!info.alive);
    assert!(info.program.is_none(), "{:?}", info.program);
    app.client.send(Request::Subscribe { pane }).unwrap();
    app.wait_output(pane, "[connection lost]");
}

/// One exiting with its status before the pane has it closes the pane.
#[test]
fn a_shell_exiting_before_its_pane_has_it_closes_it() {
    let key = TestKey::new(1);
    let server = serve_with(
        &key,
        Options {
            shell_init: Some("exit 3".into()),
            ..Default::default()
        },
    );
    let app = open(&server, key, Mode::Ssh);
    let _delay = DELAY.lock();
    super::SSH_START_DELAY_MS.store(500, Ordering::Relaxed);
    let req = CreatePane {
        command: None,
        cwd: None,
        env: vec![],
        size: size(80, 24),
        agent_preset: None,
        inherit_cwd_from: None,
        hold: false,
        fork_from: None,
    };
    let created = app.client.request(Request::CreatePane(req));
    super::SSH_START_DELAY_MS.store(0, Ordering::Relaxed);
    assert!(created.is_err(), "{created:?}");
    assert!(app.panes().is_empty());
}

#[test]
fn interleaves_resolved_address_families_and_deduplicates() {
    let v6_a: SocketAddr = "[2001:db8::1]:22".parse().unwrap();
    let v6_b: SocketAddr = "[2001:db8::2]:22".parse().unwrap();
    let v4_a: SocketAddr = "192.0.2.1:22".parse().unwrap();
    let v4_b: SocketAddr = "192.0.2.2:22".parse().unwrap();
    assert_eq!(
        interleave_addrs(vec![v6_a, v6_a, v6_b, v4_a, v4_b, v4_a]),
        vec![v6_a, v4_a, v6_b, v4_b]
    );
    assert_eq!(
        interleave_addrs(vec![v4_a, v4_b, v6_a]),
        vec![v4_a, v6_a, v4_b]
    );
}

#[test]
fn pick_wake_ip_prefers_current_then_same_family_then_nat64() {
    let v4_a: SocketAddr = "192.0.2.1:22".parse().unwrap();
    let v4_b: SocketAddr = "192.0.2.2:22".parse().unwrap();
    let v6_nat64: SocketAddr = "[64:ff9b::192.0.2.1]:22".parse().unwrap();
    // Keeps current IP even if IPv6 is first in the new DNS answer.
    assert_eq!(
        pick_wake_ip(v4_a.ip(), &[v6_nat64, v4_a]),
        Some(v4_a.ip())
    );
    // Prefers same family when host's IPv4 moved.
    assert_eq!(
        pick_wake_ip(v4_a.ip(), &[v6_nat64, v4_b]),
        Some(v4_b.ip())
    );
    // Falls over to NAT64 IPv6 when IPv4 disappears on cellular.
    assert_eq!(
        pick_wake_ip(v4_a.ip(), &[v6_nat64]),
        Some(v6_nat64.ip())
    );
    assert_eq!(pick_wake_ip(v4_a.ip(), &[]), None);
}

#[test]
fn race_candidates_waits_for_sibling_after_auth_error() {
    let bad: SocketAddr = "192.0.2.1:22".parse().unwrap();
    let good: SocketAddr = "192.0.2.2:22".parse().unwrap();
    let won = Arc::new(AtomicBool::new(false));
    let res = race_candidates(
        "dual.example",
        &[bad, good],
        Duration::from_millis(5),
        won,
        move |addr| {
            if addr == bad {
                Err(SessionError::KeyRejected)
            } else {
                std::thread::sleep(Duration::from_millis(30));
                Ok("connected-session")
            }
        },
        |_| {},
    );
    assert_eq!(res.unwrap(), ("connected-session", good.ip()));

    // When every candidate fails and one hit a non-transport auth error, surface the auth error.
    let won = Arc::new(AtomicBool::new(false));
    let key = TestKey::new(1);
    let err = race_candidates::<&str, _, _>(
        "dual.example",
        &[bad, good],
        Duration::from_millis(5),
        won,
        move |addr| {
            if addr == bad {
                Err(SessionError::KeyRejected)
            } else {
                // Force a real transport-level SessionError::Ssh against an unserved loopback port.
                let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let closed_port = ln.local_addr().unwrap().port();
                drop(ln);
                let t = Target {
                    host: "127.0.0.1".into(),
                    port: closed_port,
                    user: "u".into(),
                    host_keys: vec![],
                    socket: None,
                };
                match Session::connect_checked(&t, key.clone(), Duration::from_secs(1)) {
                    Ok(_) => unreachable!(),
                    Err(e) => Err(e),
                }
            }
        },
        |_| {},
    )
    .unwrap_err();
    assert!(matches!(err, DirectError::KeyRejected), "{err:?}");
}

#[test]
fn delayed_reconnect_finishing_after_shutdown_is_discarded() {
    let key = TestKey::new(1);
    let server = ssh_server(&key);
    let t = direct_target(&server, Mode::Ssh);
    let (_stream, direct) = super::serve(t, key).unwrap();
    // Drop the existing SSH session so `session()` must reconnect.
    if let Some(old) = direct.inner.session.lock().current.take() {
        old.disconnect();
    }
    let _delay = DELAY.lock();
    super::RECONNECT_DELAY_MS.store(300, Ordering::Relaxed);
    let inner = direct.inner.clone();
    let reconnecting = std::thread::spawn(move || inner.session());
    // Wait until the worker enters `connecting = true`.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !direct.inner.session.lock().connecting {
        assert!(Instant::now() < deadline, "reconnect never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    direct.inner.shutdown();
    let outcome = reconnecting.join().unwrap();
    super::RECONNECT_DELAY_MS.store(0, Ordering::Relaxed);
    assert!(outcome.is_err(), "late reconnect must not succeed after shutdown");
    assert!(
        direct.inner.session.lock().current.is_none(),
        "late reconnect must not remain installed after shutdown"
    );
}

#[test]
fn racing_key_blocks_sibling_sign_until_auth_completes() {
    struct CountingKey {
        inner: Arc<TestKey>,
        signs: Arc<AtomicU64>,
    }
    impl DeviceKey for CountingKey {
        fn public_sec1(&self) -> Vec<u8> {
            self.inner.public_sec1()
        }
        fn sign(&self, data: &[u8]) -> Result<Vec<u8>, String> {
            self.signs.fetch_add(1, Ordering::SeqCst);
            self.inner.sign(data)
        }
    }

    let first: SocketAddr = "192.0.2.1:22".parse().unwrap();
    let second: SocketAddr = "192.0.2.2:22".parse().unwrap();

    // Case 1: first dial wins after a post-sign handshake delay; overlapping second dial must
    // block until first finishes and cancel without invoking `sign()` a second time.
    let signs = Arc::new(AtomicU64::new(0));
    let counting: Arc<dyn DeviceKey> = Arc::new(CountingKey {
        inner: TestKey::new(1),
        signs: signs.clone(),
    });
    let won = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(AuthGate::new());
    let won_for_dial = won.clone();
    let res = race_candidates(
        "dual.example",
        &[first, second],
        Duration::from_millis(5),
        won,
        move |addr| {
            let racing = Arc::new(RacingKey::new(
                counting.clone(),
                gate.clone(),
                won_for_dial.clone(),
            ));
            if addr == first {
                racing.sign(b"auth-1").unwrap();
                // Simulate waiting for server USERAUTH_SUCCESS after sign() returns.
                std::thread::sleep(Duration::from_millis(60));
                racing.finish(true);
                Ok("first-won")
            } else {
                // Reaches sign() while `first` is still awaiting USERAUTH_SUCCESS.
                let signed = racing.sign(b"auth-2");
                racing.finish(false);
                assert!(signed.is_err(), "sibling should cancel after winner completes");
                Err(SessionError::KeyRejected)
            }
        },
        |_| {},
    );
    assert_eq!(res.unwrap(), ("first-won", first.ip()));
    assert_eq!(signs.load(Ordering::SeqCst), 1);

    // Case 2: first dial fails after signing; second dial must then be released to sign and win.
    let signs = Arc::new(AtomicU64::new(0));
    let counting: Arc<dyn DeviceKey> = Arc::new(CountingKey {
        inner: TestKey::new(1),
        signs: signs.clone(),
    });
    let won = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(AuthGate::new());
    let won_for_dial = won.clone();
    let res = race_candidates(
        "dual.example",
        &[first, second],
        Duration::from_millis(5),
        won,
        move |addr| {
            let racing = Arc::new(RacingKey::new(
                counting.clone(),
                gate.clone(),
                won_for_dial.clone(),
            ));
            if addr == first {
                racing.sign(b"auth-1").unwrap();
                std::thread::sleep(Duration::from_millis(40));
                racing.finish(false);
                Err(SessionError::KeyRejected)
            } else {
                racing.sign(b"auth-2").unwrap();
                racing.finish(true);
                Ok("second-won")
            }
        },
        |_| {},
    );
    assert_eq!(res.unwrap(), ("second-won", second.ip()));
    assert_eq!(signs.load(Ordering::SeqCst), 2);
}
