//! The newer UI commands ([`CAP_UI_LAYOUT`]), focus history and pane modes.

use std::io::Write;
use std::os::unix::net::UnixStream;

use thurm_proto::codec;

use super::*;

/// A UI from before [`CAP_UI_LAYOUT`]: the client library would announce it.
fn old_ui(env: &Env) -> UnixStream {
    let mut s = UnixStream::connect(&env.socket).unwrap();
    let r = send(
        &mut s,
        1,
        Request::Hello {
            client: "e2e-old-app".into(),
            version: PROTOCOL_VERSION,
            ui: true,
            capabilities: vec![CAP_ACTIVE_SIZE.into()],
        },
    );
    assert!(matches!(r, Ok(Response::Hello { .. })), "{r:?}");
    s
}

fn send(s: &mut UnixStream, id: u64, request: Request) -> Result<Response, String> {
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
fn layout_commands_need_a_ui_that_knows_them() {
    let env = Env::new("ui-layout");
    let _d = env.start();
    let mut old = old_ui(&env);
    let Ok(Response::PaneCreated { pane }) = send(
        &mut old,
        2,
        Request::CreatePane(CreatePane {
            cwd: Some(env.dir.display().to_string()),
            ..Default::default()
        }),
    ) else {
        panic!("no pane");
    };
    let zoom = UiCommand::Zoom {
        pane,
        zoom: Some(true),
    };
    // An app from before them would drop the command: refused, so the CLI can say so.
    let err = send(&mut old, 3, Request::Ui(zoom.clone())).unwrap_err();
    assert_eq!(err, UI_TOO_OLD);
    // The older commands still go through.
    send(&mut old, 4, Request::Ui(UiCommand::Focus { pane })).unwrap();

    let (new, new_rx) = env.connect();
    new.request(Request::Ui(zoom.clone())).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = false;
    while Instant::now() < deadline && !got {
        got = matches!(new_rx.recv_timeout(Duration::from_millis(200)), Ok(Event::Ui(ref c)) if *c == zoom);
    }
    assert!(got);
}

#[test]
fn an_older_ui_showing_the_pane_refuses_it() {
    let env = Env::new("ui-mixed");
    let _d = env.start();
    let (new, _new_rx) = env.connect();
    let shown = create(&new, &env.dir);
    let other = create(&new, &env.dir);
    // An older Mac shows `shown` (subscribed to it), next to a current one.
    let mut old = old_ui(&env);
    send(&mut old, 2, Request::Subscribe { pane: shown }).unwrap();

    let zoom = |pane| UiCommand::Zoom {
        pane,
        zoom: Some(true),
    };
    let err = new.request(Request::Ui(zoom(shown))).unwrap_err();
    assert!(err.to_string().contains(UI_TOO_OLD), "{err}");
    let swap = UiCommand::SwapPanes { a: other, b: shown };
    assert!(new.request(Request::Ui(swap)).is_err());

    // A pane only the current UI shows: done, and the older UI is not sent what it can't read.
    new.request(Request::Ui(zoom(other))).unwrap();
    send(&mut old, 3, Request::Unsubscribe { pane: shown }).unwrap();
    old.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    while let Ok(Some(frame)) = codec::read_frame(&mut old) {
        let msg: ServerMessage = codec::decode(&frame).unwrap();
        assert!(
            !matches!(msg, ServerMessage::Event(Event::Ui(_))),
            "{msg:?}"
        );
    }
}

#[test]
fn focus_history_and_pane_modes() {
    let env = Env::new("focus-modes");
    let _d = env.start();
    let (c, _rx) = env.connect();
    let a = create(&c, &env.dir);
    let b = create(&c, &env.dir);
    let focus = |pane, focused| c.request(Request::Focus { pane, focused }).unwrap();
    focus(a, true);
    focus(a, false);
    focus(b, true);
    let Response::FocusHistory(h) = c.request(Request::FocusHistory).unwrap() else {
        panic!("unexpected response");
    };
    assert_eq!(
        h.iter().map(|r| (r.pane, r.focused)).collect::<Vec<_>>(),
        vec![(b, true), (a, false)]
    );

    let modes = |pane| match c.request(Request::PaneModes { pane }).unwrap() {
        Response::PaneModes(m) => m,
        other => panic!("unexpected response {other:?}"),
    };
    assert!(!modes(a).alt_screen);
    c.request(Request::Input {
        pane: a,
        data: b"printf '\\033[?1049h'; echo alt-$((40 + 2))\r".to_vec(),
    })
    .unwrap();
    wait_match(&c, a, "alt-42");
    assert!(modes(a).alt_screen);
    assert!(c.request(Request::PaneModes { pane: 999_999 }).is_err());

    // A closed pane leaves the history.
    c.request(Request::ClosePane { pane: b }).unwrap();
    let Response::FocusHistory(h) = c.request(Request::FocusHistory).unwrap() else {
        panic!("unexpected response");
    };
    assert_eq!(h.iter().map(|r| r.pane).collect::<Vec<_>>(), vec![a]);
}
