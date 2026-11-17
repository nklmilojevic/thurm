use super::*;

/// A client that hears port changes ([`CAP_PORT_EVENTS`]).
fn subscriber(env: &Env) -> (Arc<Client>, Receiver<Event>) {
    let (tx, rx) = unbounded();
    let c = Client::connect_with(
        ConnectOptions {
            socket: env.socket.clone(),
            spawn_daemon: None,
            client_name: "e2e-ports",
            ui: true,
        },
        &[CAP_PORT_EVENTS],
        move |ev| {
            let _ = tx.send(ev);
        },
        || {},
    )
    .expect("connect");
    (c, rx)
}

/// The next `PortsChanged` for `pane`.
fn next_ports(
    rx: &Receiver<Event>,
    pane: PaneId,
    timeout: Duration,
) -> Option<(Vec<PortEntry>, bool)> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match rx.recv_timeout(left).ok()? {
            Event::PortsChanged {
                pane: p,
                ports,
                complete,
                ..
            } if p == pane => return Some((ports, complete)),
            _ => {}
        }
    }
}

/// Starts a listener on a free loopback port in `pane`; its port.
fn listen_in(c: &Client, pane: PaneId) -> u16 {
    c.request(Request::Input {
        pane,
        data: b"python3 -c 'import socket,time; s=socket.socket(); s.bind((\"127.0.0.1\",0)); s.listen(); print(\"port=%d\" % s.getsockname()[1], flush=True); time.sleep(600)'\n".to_vec(),
    })
    .unwrap();
    wait_match(c, pane, r"port=\d+");
    let text = capture(c, pane);
    let at = text.rfind("port=").unwrap();
    text[at + 5..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

fn has_python() -> bool {
    Command::new("python3")
        .arg("-c")
        .arg("")
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn a_listener_in_a_pane_is_reported_and_its_end_too() {
    if !has_python() {
        eprintln!("note: no python3 here; skipped");
        return;
    }
    let env = Env::new("ports-events");
    let _daemon = env.start_with(&[("THURM_PORTS_INTERVAL_MS", Path::new("200"))]);
    let (c, rx) = subscriber(&env);
    assert!(c.has_capability(CAP_PORTS));
    // A client that did not ask hears nothing of ports.
    let (_plain, plain_rx) = env.connect();
    let pane = create(&c, &env.dir);
    wait_match(&c, pane, r"\$");

    let port = listen_in(&c, pane);
    let (ports, complete) = next_ports(&rx, pane, Duration::from_secs(15)).expect("ports listed");
    assert!(complete);
    let entry = ports
        .iter()
        .find(|p| p.port == port)
        .unwrap_or_else(|| panic!("{port} not in {ports:?}"));
    assert_eq!(entry.address, "127.0.0.1");
    assert!(entry.command.contains("getsockname"), "{entry:?}");

    // Asked directly, the same list.
    match c.request(Request::Ports { pane: Some(pane) }).unwrap() {
        Response::Ports(list) => {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].pane, pane);
            assert!(list[0].ports.iter().any(|p| p.port == port), "{list:?}");
        }
        other => panic!("{other:?}"),
    }

    // The listener stops: the whole (now empty) list.
    c.request(Request::Input {
        pane,
        data: b"\x03".to_vec(),
    })
    .unwrap();
    let (ports, _) = next_ports(&rx, pane, Duration::from_secs(15)).expect("the end listed");
    assert!(ports.is_empty(), "{ports:?}");

    assert!(
        !plain_rx
            .try_iter()
            .any(|e| matches!(e, Event::PortsChanged { .. })),
        "a client without port-events heard of ports"
    );
}

#[test]
fn a_pane_without_ports_says_nothing_and_an_exit_says_empty() {
    if !has_python() {
        eprintln!("note: no python3 here; skipped");
        return;
    }
    let env = Env::new("ports-exit");
    let _daemon = env.start_with(&[("THURM_PORTS_INTERVAL_MS", Path::new("200"))]);
    let (c, rx) = subscriber(&env);
    let quiet = create(&c, &env.dir);
    let busy = create(&c, &env.dir);
    wait_match(&c, quiet, r"\$");
    wait_match(&c, busy, r"\$");
    let port = listen_in(&c, busy);
    let (ports, _) = next_ports(&rx, busy, Duration::from_secs(15)).expect("ports listed");
    assert!(ports.iter().any(|p| p.port == port));
    assert!(next_ports(&rx, quiet, Duration::from_millis(800)).is_none());

    // The pane goes away with its listener: one empty list.
    c.request(Request::ClosePane { pane: busy }).unwrap();
    let (ports, complete) = next_ports(&rx, busy, Duration::from_secs(15)).expect("the end listed");
    assert!(ports.is_empty() && complete, "{ports:?}");
    assert!(next_ports(&rx, busy, Duration::from_millis(800)).is_none());
}

#[test]
fn ports_of_an_unknown_pane_are_an_error() {
    let env = Env::new("ports-unknown");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let e = c.request(Request::Ports { pane: Some(4242) }).unwrap_err();
    assert!(e.to_string().contains("no such pane"), "{e}");
}

/// The next `PortsChanged` for `pane`, with whether it was an initial list.
fn next_marked(rx: &Receiver<Event>, pane: PaneId, timeout: Duration) -> Option<(Vec<u16>, bool)> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        if let Event::PortsChanged {
            pane: p,
            ports,
            initial,
            ..
        } = rx.recv_timeout(left).ok()?
            && p == pane
        {
            return Some((ports.iter().map(|e| e.port).collect(), initial));
        }
    }
}

#[test]
fn a_joining_client_is_told_what_listens_already_as_initial() {
    if !has_python() {
        eprintln!("note: no python3 here; skipped");
        return;
    }
    let env = Env::new("ports-initial");
    let _daemon = env.start_with(&[("THURM_PORTS_INTERVAL_MS", Path::new("200"))]);
    let (first, first_rx) = subscriber(&env);
    let pane = create(&first, &env.dir);
    wait_match(&first, pane, r"\$");
    let port = listen_in(&first, pane);
    // The client watching already hears the server start: a change.
    let (ports, initial) = next_marked(&first_rx, pane, Duration::from_secs(15)).expect("a change");
    assert!(ports.contains(&port));
    assert!(!initial, "a server starting was marked initial");

    // A client that joins now is told it was listening already.
    let (_second, second_rx) = subscriber(&env);
    let (ports, initial) =
        next_marked(&second_rx, pane, Duration::from_secs(15)).expect("its first list");
    assert!(ports.contains(&port));
    assert!(initial, "a joiner's first list was not marked initial");
    // The first client hears nothing of the join.
    assert!(
        next_marked(&first_rx, pane, Duration::from_millis(800)).is_none(),
        "the join was broadcast"
    );

    // The server stops: a change for both, initial for neither.
    first
        .request(Request::Input {
            pane,
            data: b"\x03".to_vec(),
        })
        .unwrap();
    for rx in [&first_rx, &second_rx] {
        let (ports, initial) = next_marked(rx, pane, Duration::from_secs(15)).expect("the stop");
        assert!(ports.is_empty(), "{ports:?}");
        assert!(!initial);
    }
}
