//! The server driven by the app's own client, against the in-process ssh server of
//! `thurm_ssh::testing` (shells are a local `sh`; mosh is the local `mosh-server`, when there).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use thurm_client::Client;
use thurm_proto::{CreatePane, Event, PaneSize, Request, Response};
use thurm_ssh::testing::{TestKey, TestServer, serve as ssh_server, target};

use super::*;

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
