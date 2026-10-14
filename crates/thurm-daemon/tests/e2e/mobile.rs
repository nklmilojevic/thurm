use std::io::Write as _;
use std::os::unix::net::UnixStream;

use super::*;

fn mobile(env: &Env) -> (Arc<Client>, Receiver<Event>) {
    let (tx, rx) = unbounded();
    let stream = thurm_client::connect_daemon(&env.socket).unwrap();
    let c = Client::from_stream(
        stream,
        "e2e-ios",
        false,
        &[CAP_MOBILE],
        move |ev| {
            let _ = tx.send(ev);
        },
        || {},
    )
    .expect("connect");
    (c, rx)
}

fn size(cols: u16, rows: u16) -> PaneSize {
    PaneSize {
        cols,
        rows,
        cell_width: 8,
        cell_height: 16,
    }
}

fn pane_size(c: &Client, pane: PaneId) -> (u16, u16) {
    match c.request(Request::PaneInfo { pane }).unwrap() {
        Response::PaneInfo(info) => (info.size.cols, info.size.rows),
        other => panic!("{other:?}"),
    }
}

/// A raw connection saying Hello with `version` and `capabilities`, for peers the client library
/// can't be (older, newer, without active sizes).
fn raw_hello(
    env: &Env,
    version: u32,
    capabilities: &[&str],
) -> (UnixStream, Result<Response, String>) {
    let mut s = UnixStream::connect(&env.socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let hello = Request::Hello {
        client: "e2e-raw".into(),
        version,
        ui: false,
        capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
    };
    let r = raw_request(&mut s, 1, hello);
    (s, r)
}

fn raw_request(s: &mut UnixStream, id: u64, request: Request) -> Result<Response, String> {
    s.write_all(&codec::encode(&Envelope { id, request }).unwrap())
        .unwrap();
    loop {
        let frame = codec::read_frame(s).unwrap().expect("daemon closed");
        if let ServerMessage::Response { id: got, result } = codec::decode(&frame).unwrap()
            && got == id
        {
            return result;
        }
    }
}

#[test]
fn hello_accepts_the_compatibility_window() {
    let env = Env::new("hello-window");
    let _daemon = env.start();
    let (_, newer) = raw_hello(&env, PROTOCOL_VERSION + 1, &[]);
    assert!(
        matches!(newer, Ok(Response::Hello { version, .. }) if version == PROTOCOL_VERSION),
        "{newer:?}"
    );
    let (_, older) = raw_hello(&env, MIN_COMPATIBLE_PROTOCOL - 1, &[]);
    assert!(
        matches!(&older, Err(e) if e.contains("protocol mismatch")),
        "{older:?}"
    );
}

#[test]
fn pane_takes_the_size_of_the_client_that_used_it_last() {
    let env = Env::new("active-size");
    let _daemon = env.start();
    let (mac, _) = env.connect();
    let (phone, _) = mobile(&env);
    let pane = create(&mac, &env.dir);

    mac.request(Request::Resize {
        pane,
        size: size(120, 40),
    })
    .unwrap();
    phone
        .request(Request::Resize {
            pane,
            size: size(50, 30),
        })
        .unwrap();
    assert_eq!(pane_size(&mac, pane), (50, 30));

    // Back at the Mac: focusing the pane gives it the Mac's size again, without a Resize.
    mac.request(Request::Focus {
        pane,
        focused: true,
    })
    .unwrap();
    assert_eq!(pane_size(&mac, pane), (120, 40));

    // Typing on the phone takes it back.
    phone
        .request(Request::Input {
            pane,
            data: b" ".to_vec(),
        })
        .unwrap();
    assert_eq!(pane_size(&mac, pane), (50, 30));

    // The phone leaves (backgrounded): the Mac's size returns.
    drop(phone);
    let deadline = Instant::now() + Duration::from_secs(5);
    while pane_size(&mac, pane) != (120, 40) {
        assert!(Instant::now() < deadline, "the Mac's size did not return");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn clients_without_active_sizes_keep_last_resize_winning() {
    let env = Env::new("legacy-size");
    let _daemon = env.start();
    let (mac, _) = env.connect();
    let pane = create(&mac, &env.dir);
    mac.request(Request::Resize {
        pane,
        size: size(120, 40),
    })
    .unwrap();

    let (mut old, _) = raw_hello(&env, PROTOCOL_VERSION, &[]);
    let r = raw_request(
        &mut old,
        2,
        Request::Resize {
            pane,
            size: size(70, 20),
        },
    );
    assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
    assert_eq!(pane_size(&mac, pane), (70, 20));

    // Its input does not give the pane back to anyone, and leaving changes nothing either.
    let _ = raw_request(
        &mut old,
        3,
        Request::Input {
            pane,
            data: b" ".to_vec(),
        },
    );
    drop(old);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(pane_size(&mac, pane), (70, 20));
}

#[test]
fn mobile_clients_get_notifications_but_no_window_commands() {
    let env = Env::new("mobile-events");
    let _daemon = env.start();
    let (mac, mac_events) = env.connect();
    let (phone, phone_events) = mobile(&env);
    let pane = create(&mac, &env.dir);

    // An agent asks for permission: the phone hears about it and can answer.
    mac.request(Request::Input {
        pane,
        data: b"/bin/cat\r".to_vec(),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(mac.request(Request::PaneInfo { pane }), Ok(Response::PaneInfo(i))
        if i.foreground.as_ref().is_some_and(|f| f.name == "cat"))
    {
        assert!(Instant::now() < deadline, "cat never became the foreground");
        std::thread::sleep(Duration::from_millis(100));
    }
    hook(&mac, pane, "prompt-submit", None);
    hook(&mac, pane, "permission-prompt", None);
    let deadline = Instant::now() + Duration::from_secs(5);
    let prompt = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if let Event::Notify {
            pane: p,
            permission: Some(prompt),
            ..
        } = phone_events.recv_timeout(left).expect("no notification")
            && p == pane
        {
            break prompt;
        }
    };
    let r = phone.request(Request::AnswerPermission {
        pane,
        prompt,
        allow: true,
    });
    assert!(matches!(r, Ok(Response::Ok)), "{r:?}");

    phone
        .request(Request::Ui(UiCommand::Focus { pane }))
        .unwrap();
    let got = |rx: &Receiver<Event>| {
        rx.recv_timeout(Duration::from_secs(1))
            .into_iter()
            .chain(rx.try_iter())
            .any(|e| matches!(e, Event::Ui(UiCommand::Focus { .. })))
    };
    assert!(got(&mac_events), "the Mac app should get the UI command");
    assert!(!got(&phone_events), "the phone is not a desktop window");

    // A phone alone is no open window.
    drop(mac);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match phone.request(Request::Ui(UiCommand::Focus { pane })) {
            Err(e) if e.to_string().contains("no Thurm window is open") => break,
            other => {
                assert!(Instant::now() < deadline, "{other:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
