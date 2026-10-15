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

/// A client leaving gives the size back only if the pane had its size: another one that wants
/// the same size does not own it.
#[test]
fn a_client_with_the_same_size_leaving_keeps_the_owners_size() {
    let env = Env::new("size-owner");
    let _daemon = env.start();
    let (mac, _) = env.connect();
    let pane = create(&mac, &env.dir);
    let caps = [thurm_proto::CAP_ACTIVE_SIZE];
    let (mut b, hello) = raw_hello(&env, PROTOCOL_VERSION, &caps);
    assert!(hello.is_ok(), "{hello:?}");
    let (mut c, hello) = raw_hello(&env, PROTOCOL_VERSION, &caps);
    assert!(hello.is_ok(), "{hello:?}");
    let (mut a, hello) = raw_hello(&env, PROTOCOL_VERSION, &[]);
    assert!(hello.is_ok(), "{hello:?}");
    for (s, size) in [
        (&mut b, size(80, 24)),
        (&mut c, size(120, 40)),
        (&mut a, size(80, 24)),
    ] {
        let r = raw_request(s, 2, Request::Resize { pane, size });
        assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
    }
    assert_eq!(pane_size(&mac, pane), (80, 24));
    drop(b);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(pane_size(&mac, pane), (80, 24), "B never owned the size");
    drop(a);
    drop(c);
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
    // Other events (pane and agent updates) can come first: wait for the one we want.
    let got = |rx: &Receiver<Event>, wait: Duration| {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(Event::Ui(UiCommand::Focus { .. })) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    };
    assert!(
        got(&mac_events, Duration::from_secs(5)),
        "the Mac app should get the UI command"
    );
    assert!(
        !got(&phone_events, Duration::from_secs(1)),
        "the phone is not a desktop window"
    );

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

/// An `Env` with push on (`[push] relay` set; each device names its own relay).
fn push_env(name: &str) -> Env {
    let env = Env::new(name);
    let path = env.dir.join("config/config.toml");
    let mut config = std::fs::read_to_string(&path).unwrap();
    config.push_str("\n[push]\nrelay = \"https://push.example.net\"\n");
    std::fs::write(path, config).unwrap();
    env
}

fn register(c: &Client, device: &str, relay: &str, name: &str) {
    let r = c.request(Request::RegisterPushDevice {
        device: device.into(),
        relay: relay.into(),
        secret: "s".repeat(64),
        host: "studio".into(),
        name: name.into(),
    });
    assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
}

/// Runs `/bin/cat` in `pane` and waits for it, so agent hooks have a process to belong to.
fn run_cat(c: &Client, pane: PaneId) {
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
}

/// The next `/v1/push` post within `wait`, skipping Live Activity posts.
fn next_push(posts: &Receiver<(String, String)>, wait: Duration) -> Option<serde_json::Value> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let (head, body) = posts.recv_timeout(left).ok()?;
        if head.starts_with("POST /v1/push ") {
            return Some(serde_json::from_str(&body).unwrap());
        }
    }
}

/// A relay that answers 200 to every request and hands over each one's headers and body.
fn fake_relay() -> (String, Receiver<(String, String)>) {
    use std::io::{BufRead, BufReader, Read};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (tx, rx) = unbounded();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut body = vec![0; len];
            let _ = reader.read_exact(&mut body);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            let _ = tx.send((head, String::from_utf8_lossy(&body).into_owned()));
        }
    });
    (url, rx)
}

#[test]
fn permission_prompts_reach_registered_phones_when_the_app_is_closed() {
    let env = push_env("push");
    let _daemon = env.start();
    let (mac, _) = env.connect();
    let (relay, posts) = fake_relay();
    register(&mac, "dev-1", &relay, "iPhone");

    let pane = create(&mac, &env.dir);
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

    let (head, body) = posts
        .recv_timeout(Duration::from_secs(10))
        .expect("nothing reached the relay");
    assert!(head.starts_with("POST /v1/push "), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("x-thurm-signature:"), "{head}");
    assert!(lower.contains("x-thurm-timestamp:"), "{head}");
    let b: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(b["device"], "dev-1");
    assert_eq!(b["host"], "studio");
    assert_eq!(b["pane"], pane);
    assert!(b["permission"].is_u64(), "{body}");
    assert_eq!(b["body"], "asks for permission");

    // With the app open, the phone hears it live instead.
    let (phone, phone_events) = mobile(&env);
    register(&phone, "dev-1", &relay, "iPhone");
    hook(&mac, pane, "stop", None);
    hook(&mac, pane, "prompt-submit", None);
    hook(&mac, pane, "permission-prompt", None);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if let Event::Notify {
            permission: Some(_),
            ..
        } = phone_events
            .recv_timeout(left)
            .expect("the open app got no notice")
        {
            break;
        }
    }
    assert!(
        posts.recv_timeout(Duration::from_secs(1)).is_err(),
        "pushed while the app was open"
    );
}

#[test]
fn followed_agents_post_live_activity_updates_until_the_pane_closes() {
    let env = push_env("activity");
    let _daemon = env.start();
    let (mac, _) = env.connect();
    let (relay, posts) = fake_relay();
    let r = mac.request(Request::RegisterPushDevice {
        device: "dev-1".into(),
        relay,
        secret: "s".repeat(64),
        host: "studio".into(),
        name: "iPhone".into(),
    });
    assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
    let pane = create(&mac, &env.dir);
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

    // Following needs a registered device and a hex token.
    let bad = mac.request(Request::FollowAgent {
        device: "nobody".into(),
        pane,
        token: "ab12".into(),
    });
    assert!(bad.is_err(), "{bad:?}");
    let r = mac.request(Request::FollowAgent {
        device: "dev-1".into(),
        pane,
        token: "ab12cd".into(),
    });
    assert!(matches!(r, Ok(Response::Ok)), "{r:?}");

    let activity = |what: &str| -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (head, body) = posts
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {what}"));
            if head.starts_with("POST /v1/activity ") {
                return serde_json::from_str(&body).unwrap();
            }
        }
    };
    let first = activity("first update");
    assert_eq!(first["event"], "update");
    assert_eq!(first["token"], "ab12cd");
    assert_eq!(first["state"]["status"], "working");
    assert_eq!(first["state"]["agent"], "Claude Code");

    hook(&mac, pane, "permission-prompt", None);
    let asked = activity("permission update");
    assert_eq!(asked["state"]["status"], "permission");
    assert!(asked["state"]["permission"].is_u64());

    mac.request(Request::ClosePane { pane }).unwrap();
    let end = activity("end");
    assert_eq!(end["event"], "end");
    assert_eq!(end["state"]["status"], "ended");
    assert_eq!(end["state"]["agent"], "Claude Code");
}

/// iOS suspends the app without closing its connection: a phone that stops talking is dropped,
/// and the pane it had resized goes back to the Mac's size.
#[test]
fn a_silent_phone_is_dropped_and_gives_the_pane_back() {
    let env = Env::new("silent-phone");
    let _daemon = env.start_with(&[("THURM_MOBILE_SILENCE_MS", Path::new("800"))]);
    let (mac, _) = env.connect();
    let pane = create(&mac, &env.dir);
    mac.request(Request::Resize {
        pane,
        size: size(120, 40),
    })
    .unwrap();

    let caps = [thurm_proto::CAP_MOBILE, thurm_proto::CAP_ACTIVE_SIZE];
    let (mut phone, hello) = raw_hello(&env, PROTOCOL_VERSION, &caps);
    assert!(hello.is_ok(), "{hello:?}");
    let r = raw_request(
        &mut phone,
        2,
        Request::Resize {
            pane,
            size: size(50, 30),
        },
    );
    assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
    assert_eq!(pane_size(&mac, pane), (50, 30));

    // Pinging keeps it: still the phone's size after longer than the silence limit.
    for id in 3..6 {
        std::thread::sleep(Duration::from_millis(400));
        let r = raw_request(&mut phone, id, Request::Ping);
        assert!(matches!(r, Ok(Response::Ok)), "{r:?}");
    }
    assert_eq!(pane_size(&mac, pane), (50, 30));

    // Then silence, with the connection still open.
    let deadline = Instant::now() + Duration::from_secs(5);
    while pane_size(&mac, pane) != (120, 40) {
        assert!(Instant::now() < deadline, "the silent phone kept the pane");
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(phone);
}

/// The Mac app says Hello again on its connection (to read the daemon's version) without
/// listing capabilities: its active size must survive that, or the phone keeps the pane.
#[test]
fn a_second_hello_keeps_the_clients_capabilities() {
    let env = Env::new("second-hello");
    let _daemon = env.start();
    let (mac, _) = env.connect();
    let r = mac.request(Request::Hello {
        client: "Thurm.app".into(),
        version: PROTOCOL_VERSION,
        ui: true,
        capabilities: vec![],
    });
    assert!(matches!(r, Ok(Response::Hello { .. })), "{r:?}");
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
    drop(phone);
    let deadline = Instant::now() + Duration::from_secs(5);
    while pane_size(&mac, pane) != (120, 40) {
        assert!(
            Instant::now() < deadline,
            "the Mac did not get its size back"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One phone open, another in a pocket: only the open one goes without the push.
#[test]
fn only_the_phone_with_the_app_open_misses_the_push() {
    let env = push_env("push-two-phones");
    let _daemon = env.start();
    let (mac, _) = env.connect();
    let (relay, posts) = fake_relay();
    let (phone, _phone_events) = mobile(&env);
    register(&phone, "dev-open", &relay, "iPhone");
    register(&mac, "dev-pocket", &relay, "iPad");
    let pane = create(&mac, &env.dir);
    run_cat(&mac, pane);
    hook(&mac, pane, "prompt-submit", None);
    hook(&mac, pane, "permission-prompt", None);
    let b = next_push(&posts, Duration::from_secs(10)).expect("the iPad got nothing");
    assert_eq!(b["device"], "dev-pocket");
    assert!(
        next_push(&posts, Duration::from_secs(1)).is_none(),
        "the open iPhone was pushed too"
    );
}
