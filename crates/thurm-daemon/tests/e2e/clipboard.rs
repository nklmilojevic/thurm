//! Clipboard tools in a pane: copies go to the apps, pastes ask the app that last focused the
//! pane.

use std::io::Write;
use std::os::unix::net::UnixStream;

use super::*;

/// A pane's clipboard tool: no UI, like `thurm clip`.
fn tool(env: &Env) -> Arc<Client> {
    let stream = thurm_client::connect_daemon(&env.socket).unwrap();
    Client::from_stream(stream, "e2e-clip", false, &[], |_| {}, || {}).expect("connect")
}

fn get(
    c: &Arc<Client>,
    pane: PaneId,
    mime: &str,
) -> std::thread::JoinHandle<Result<Vec<u8>, String>> {
    let c = c.clone();
    let mime = mime.to_owned();
    std::thread::spawn(
        move || match c.request(Request::ClipboardGet { pane, mime }) {
            Ok(Response::ClipboardData { data }) => Ok(data),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e.to_string()),
        },
    )
}

/// The next clipboard query on `rx`, skipping other events.
fn query(rx: &Receiver<Event>) -> Option<(PaneId, u64, String)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let Event::ClipboardQuery { pane, id, mime } = ev {
            return Some((pane, id, mime));
        }
    }
    None
}

fn no_query(rx: &Receiver<Event>) {
    while let Ok(ev) = rx.recv_timeout(Duration::from_millis(300)) {
        assert!(!matches!(ev, Event::ClipboardQuery { .. }), "{ev:?}");
    }
}

/// An `Env` whose daemon has `terminal.osc52 = osc52`: copies and pastes need it to allow them.
fn clip_env(name: &str, osc52: &str) -> Env {
    let env = Env::new(name);
    std::fs::write(
        env.dir.join("config/config.toml"),
        format!("[terminal]\nshell = [\"/bin/sh\"]\nosc52 = \"{osc52}\"\n"),
    )
    .unwrap();
    env
}

fn focus(c: &Client, pane: PaneId) {
    c.request(Request::Focus {
        pane,
        focused: true,
    })
    .unwrap();
}

fn answer(c: &Client, id: u64, data: Option<Vec<u8>>) -> Result<Response, String> {
    c.request(Request::ClipboardAnswer { id, data })
        .map_err(|e| e.to_string())
}

#[test]
fn a_paste_asks_the_app_that_focused_the_pane_last() {
    let env = clip_env("clip-route", "copy-paste");
    let _daemon = env.start();
    let (a, a_rx) = env.connect();
    let (b, b_rx) = env.connect();
    let pane = create(&a, &env.dir);
    let other = create(&a, &env.dir);
    focus(&a, pane);
    focus(&b, pane);
    focus(&a, other);
    let tool = tool(&env);

    let paste = get(&tool, pane, CLIP_TEXT);
    let (p, id, mime) = query(&b_rx).expect("b focused the pane last");
    assert_eq!((p, mime.as_str()), (pane, CLIP_TEXT));
    no_query(&a_rx);
    assert!(
        answer(&a, id, Some(b"not mine".to_vec())).is_err(),
        "only the app asked answers"
    );
    answer(&b, id, Some(b"hello".to_vec())).unwrap();
    assert_eq!(paste.join().unwrap(), Ok(b"hello".to_vec()));
    assert!(answer(&b, id, None).is_err(), "answered once");

    // Images come back as they are.
    let png = vec![0x89, b'P', b'N', b'G', 0, 1, 2, 255];
    let paste = get(&tool, other, CLIP_PNG);
    let (_, id, mime) = query(&a_rx).expect("a focused the other pane");
    assert_eq!(mime, CLIP_PNG);
    answer(&a, id, Some(png.clone())).unwrap();
    assert_eq!(paste.join().unwrap(), Ok(png));

    // The app that looked last is gone: another one is asked.
    drop(b);
    std::thread::sleep(Duration::from_millis(200));
    let paste = get(&tool, pane, CLIP_TARGETS);
    let (_, id, _) = query(&a_rx).expect("falls back to a connected app");
    answer(&a, id, Some(format!("{CLIP_TEXT}\n").into_bytes())).unwrap();
    assert_eq!(paste.join().unwrap(), Ok(b"text/plain\n".to_vec()));

    let Err(e) = get(&tool, pane, "image/jpeg").join().unwrap() else {
        panic!("only text and png");
    };
    assert!(e.contains("unsupported"), "{e}");
    assert!(get(&tool, u64::MAX, CLIP_TEXT).join().unwrap().is_err());
}

#[test]
fn a_refusal_reaches_the_tool() {
    let env = clip_env("clip-deny", "copy-paste");
    let _daemon = env.start();
    let (app, rx) = env.connect();
    let pane = create(&app, &env.dir);
    focus(&app, pane);
    let tool = tool(&env);
    let paste = get(&tool, pane, CLIP_TEXT);
    let (_, id, _) = query(&rx).unwrap();
    answer(&app, id, None).unwrap();
    let e = paste.join().unwrap().unwrap_err();
    assert!(e.contains("not shared"), "{e}");
}

#[test]
fn a_paste_gives_up_when_the_app_says_nothing() {
    let env = clip_env("clip-timeout", "copy-paste");
    let _daemon = env.start_with(&[("THURM_CLIP_TIMEOUT_MS", Path::new("400"))]);
    let (app, rx) = env.connect();
    let pane = create(&app, &env.dir);
    focus(&app, pane);
    let tool = tool(&env);
    let started = Instant::now();
    let paste = get(&tool, pane, CLIP_TEXT);
    let (_, id, _) = query(&rx).unwrap();
    let e = paste.join().unwrap().unwrap_err();
    assert!(e.contains("did not answer"), "{e}");
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(
        answer(&app, id, Some(b"late".to_vec())).is_err(),
        "too late"
    );
}

#[test]
fn clipboards_over_32_mib_are_refused() {
    let env = clip_env("clip-cap", "copy-paste");
    let _daemon = env.start();
    let (app, rx) = env.connect();
    let pane = create(&app, &env.dir);
    focus(&app, pane);
    let tool = tool(&env);
    let paste = get(&tool, pane, CLIP_PNG);
    let (_, id, _) = query(&rx).unwrap();
    assert!(answer(&app, id, Some(vec![0; CLIP_MAX + 1])).is_err());
    let e = paste.join().unwrap().unwrap_err();
    assert!(e.contains("32 MiB"), "{e}");

    let e = tool
        .request(Request::ClipboardSet {
            pane,
            text: "x".repeat(CLIP_MAX + 1),
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("32 MiB"), "{e}");
}

/// A client sending pastes without waiting for answers gets the ones over the limit refused at
/// once, and the app is asked only for those let through.
#[test]
fn a_flood_of_pastes_is_bounded() {
    let env = clip_env("clip-flood", "copy-paste");
    let _daemon = env.start_with(&[("THURM_CLIP_TIMEOUT_MS", Path::new("20000"))]);
    let (app, rx) = env.connect();
    let pane = create(&app, &env.dir);
    focus(&app, pane);
    let mut s = UnixStream::connect(&env.socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let hello = Request::Hello {
        client: "e2e-flood".into(),
        version: PROTOCOL_VERSION,
        ui: false,
        capabilities: CAPABILITIES.iter().map(|c| (*c).to_owned()).collect(),
    };
    let mut frames = codec::encode(&Envelope {
        id: 1,
        request: hello,
    })
    .unwrap();
    const SENT: u64 = 200;
    for id in 2..2 + SENT {
        frames.extend(
            codec::encode(&Envelope {
                id,
                request: Request::ClipboardGet {
                    pane,
                    mime: CLIP_TEXT.into(),
                },
            })
            .unwrap(),
        );
    }
    let started = Instant::now();
    s.write_all(&frames).unwrap();
    let mut refused = 0;
    while refused < SENT - 64 {
        let frame = codec::read_frame(&mut s).unwrap().expect("daemon closed");
        if let ServerMessage::Response { result: Err(e), .. } = codec::decode(&frame).unwrap() {
            assert!(e.contains("too many"), "{e}");
            refused += 1;
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "refused at once, not after the answer timeout"
    );
    let mut asked = 0;
    while query(&rx).is_some() {
        asked += 1;
        if asked == 64 {
            break;
        }
    }
    assert_eq!(asked, 64);
    no_query(&rx);
}

/// Runs `req` from `c` on a thread of its own: a tool waiting for the app's answer.
fn ask(c: &Arc<Client>, req: Request) -> std::thread::JoinHandle<Result<Response, String>> {
    let c = c.clone();
    std::thread::spawn(move || c.request(req).map_err(|e| e.to_string()))
}

/// The next copy or clear the app is asked to do, skipping other events.
fn write_of(rx: &Receiver<Event>) -> Option<(PaneId, u64, Option<String>)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        match ev {
            Event::ClipboardWrite { pane, id, text } => return Some((pane, id, text)),
            Event::ClipboardStore { text, .. } => panic!("stored {text:?} with no answer"),
            _ => {}
        }
    }
    None
}

fn nothing_asked(rx: &Receiver<Event>) {
    while let Ok(ev) = rx.recv_timeout(Duration::from_millis(300)) {
        assert!(
            !matches!(
                ev,
                Event::ClipboardQuery { .. }
                    | Event::ClipboardWrite { .. }
                    | Event::ClipboardStore { .. }
            ),
            "{ev:?}"
        );
    }
}

/// A copy and a clear go to the app that focused the pane last, and the tool hears whether that
/// app took them: this Mac's own policy may refuse what the host allows.
#[test]
fn copies_and_clears_are_answered_by_the_app() {
    let env = clip_env("clip-copy", "copy-paste");
    let _daemon = env.start();
    let (a, a_rx) = env.connect();
    let (b, b_rx) = env.connect();
    let pane = create(&a, &env.dir);
    focus(&a, pane);
    focus(&b, pane);
    let tool = tool(&env);
    for (req, text) in [
        (
            Request::ClipboardSet {
                pane,
                text: "copied".into(),
            },
            Some("copied".to_string()),
        ),
        (Request::ClipboardClear { pane }, None),
    ] {
        let done = ask(&tool, req.clone());
        let (p, id, asked) = write_of(&b_rx).expect("b focused the pane last");
        assert_eq!((p, asked), (pane, text.clone()));
        answer(&b, id, Some(Vec::new())).unwrap();
        assert!(matches!(done.join().unwrap(), Ok(Response::Ok)));

        let refused = ask(&tool, req);
        let (_, id, _) = write_of(&b_rx).unwrap();
        answer(&b, id, None).unwrap();
        let e = refused.join().unwrap().unwrap_err();
        assert!(e.contains("terminal.osc52"), "{text:?}: {e}");
    }
    nothing_asked(&a_rx);
    for req in [
        Request::ClipboardSet {
            pane: u64::MAX,
            text: "x".into(),
        },
        Request::ClipboardClear { pane: u64::MAX },
    ] {
        assert!(tool.request(req).is_err());
    }
}

#[test]
fn this_hosts_osc52_setting_applies_to_clipboard_tools() {
    for (osc52, copy, paste) in [
        ("disabled", false, false),
        ("copy", true, false),
        ("paste", false, true),
        ("copy-paste", true, true),
    ] {
        let env = clip_env(&format!("clip-osc52-{osc52}"), osc52);
        let _daemon = env.start();
        let (app, rx) = env.connect();
        let pane = create(&app, &env.dir);
        focus(&app, pane);
        let tool = tool(&env);
        for req in [
            Request::ClipboardSet {
                pane,
                text: "x".into(),
            },
            Request::ClipboardClear { pane },
        ] {
            let done = ask(&tool, req);
            if copy {
                let (_, id, _) = write_of(&rx).expect("asked");
                answer(&app, id, Some(Vec::new())).unwrap();
                assert!(done.join().unwrap().is_ok(), "{osc52}");
            } else {
                let e = done.join().unwrap().unwrap_err();
                assert!(e.contains("terminal.osc52"), "{osc52}: {e}");
                nothing_asked(&rx);
            }
        }
        for mime in [CLIP_TEXT, CLIP_TARGETS] {
            let paste_thread = get(&tool, pane, mime);
            if paste {
                let (_, id, _) = query(&rx).expect("asked");
                answer(&app, id, Some(b"y".to_vec())).unwrap();
                assert!(paste_thread.join().unwrap().is_ok(), "{osc52} {mime}");
            } else {
                let e = paste_thread.join().unwrap().unwrap_err();
                assert!(e.contains("terminal.osc52"), "{osc52} {mime}: {e}");
                no_query(&rx);
            }
        }
    }
}

/// Clipboard contents on their way through the daemon are bounded in all: an answer or a copy
/// over what is left is refused, and what was held is given back once written.
#[test]
fn clipboard_data_held_at_once_is_bounded() {
    let env = clip_env("clip-held", "copy-paste");
    let _daemon = env.start_with(&[("THURM_CLIP_HELD_BYTES", Path::new("1000"))]);
    let (app, rx) = env.connect();
    let pane = create(&app, &env.dir);
    focus(&app, pane);
    let tool = tool(&env);
    for _ in 0..3 {
        let paste = get(&tool, pane, CLIP_PNG);
        let (_, id, _) = query(&rx).unwrap();
        answer(&app, id, Some(vec![1; 600])).unwrap();
        assert_eq!(
            paste.join().unwrap(),
            Ok(vec![1; 600]),
            "given back after each"
        );
    }
    let paste = get(&tool, pane, CLIP_PNG);
    let (_, id, _) = query(&rx).unwrap();
    assert!(answer(&app, id, Some(vec![1; 1001])).is_err());
    let e = paste.join().unwrap().unwrap_err();
    assert!(e.contains("too much clipboard data"), "{e}");
    let e = tool
        .request(Request::ClipboardSet {
            pane,
            text: "x".repeat(1001),
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("too much clipboard data"), "{e}");
    let done = ask(
        &tool,
        Request::ClipboardSet {
            pane,
            text: "x".repeat(1000),
        },
    );
    let (_, id, _) = write_of(&rx).unwrap();
    answer(&app, id, Some(Vec::new())).unwrap();
    assert!(matches!(done.join().unwrap(), Ok(Response::Ok)));
}

/// A raw connection that says Hello and then sends `requests`, reading nothing back.
fn stalled(env: &Env, ui: bool, requests: Vec<Request>) -> UnixStream {
    let mut s = UnixStream::connect(&env.socket).unwrap();
    let hello = Request::Hello {
        client: "e2e-stalled".into(),
        version: PROTOCOL_VERSION,
        ui,
        capabilities: CAPABILITIES.iter().map(|c| (*c).to_owned()).collect(),
    };
    for (id, request) in std::iter::once(hello).chain(requests).enumerate() {
        s.write_all(
            &codec::encode(&Envelope {
                id: id as u64 + 1,
                request,
            })
            .unwrap(),
        )
        .unwrap();
    }
    s
}

const MIB: usize = 1024 * 1024;

/// Pastes answered for a tool that does not read them stay counted until written: past the
/// limit, more answers are refused, though each one is queued as soon as it arrives.
#[test]
fn answers_queued_for_a_tool_that_does_not_read_are_bounded() {
    let env = clip_env("clip-stalled-tool", "copy-paste");
    let held = (3 * MIB).to_string();
    let _daemon = env.start_with(&[("THURM_CLIP_HELD_BYTES", Path::new(&held))]);
    let (app, rx) = env.connect();
    let pane = create(&app, &env.dir);
    focus(&app, pane);
    let gets = (0..5)
        .map(|_| Request::ClipboardGet {
            pane,
            mime: CLIP_PNG.into(),
        })
        .collect();
    let _tool = stalled(&env, false, gets);
    let mut taken = 0;
    for _ in 0..5 {
        let (_, id, _) = query(&rx).expect("asked");
        if answer(&app, id, Some(vec![7; MIB])).is_ok() {
            taken += 1;
        }
        // Let the waiting thread queue it for the tool.
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(taken, 3, "3 MiB queued for a tool that reads none of it");
}

/// Copies queued for an app that does not read them stay counted: past the limit, a tool's copy
/// is refused at once instead of queued.
#[test]
fn copies_queued_for_an_app_that_does_not_read_are_bounded() {
    let env = clip_env("clip-stalled-app", "copy-paste");
    let held = (3 * MIB).to_string();
    let _daemon = env.start_with(&[
        ("THURM_CLIP_HELD_BYTES", Path::new(&held)),
        ("THURM_CLIP_TIMEOUT_MS", Path::new("300")),
    ]);
    let tool = tool(&env);
    let pane = create(&tool, &env.dir);
    let _app = stalled(
        &env,
        true,
        vec![Request::Focus {
            pane,
            focused: true,
        }],
    );
    std::thread::sleep(Duration::from_millis(200));
    for i in 0..3 {
        let e = tool
            .request(Request::ClipboardSet {
                pane,
                text: "c".repeat(MIB),
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains("did not answer"), "copy {i}: {e}");
    }
    let started = Instant::now();
    let e = tool
        .request(Request::ClipboardSet {
            pane,
            text: "c".repeat(MIB),
        })
        .unwrap_err()
        .to_string();
    assert!(e.contains("too much clipboard data"), "{e}");
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "refused at once"
    );
}

/// A plugin's `request` of a clipboard tool's request is refused with a reason (JSON would make
/// its up to 32 MiB many times that), and the requests of its own still work for that client.
#[test]
fn clipboard_requests_are_not_taken_through_request() {
    let env = clip_env("clip-plugin", "copy-paste");
    let _daemon = env.start();
    let (app, rx) = env.connect();
    let pane = create(&app, &env.dir);
    focus(&app, pane);
    let plugin = tool(&env);
    for req in [
        Request::ClipboardGet {
            pane,
            mime: CLIP_TEXT.into(),
        },
        Request::ClipboardSet {
            pane,
            text: "x".into(),
        },
        Request::ClipboardClear { pane },
        Request::ClipboardAnswer {
            id: 1,
            data: Some(vec![0; 16]),
        },
    ] {
        let e = plugin
            .request(Request::Ext {
                method: ext::method::REQUEST.into(),
                params: serde_json::json!({ "request": req }).to_string(),
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains("not through `request`"), "{e}");
    }
    nothing_asked(&rx);
    let copied = ask(
        &plugin,
        Request::ClipboardSet {
            pane,
            text: "direct".into(),
        },
    );
    let (_, id, _) = write_of(&rx).expect("asked");
    answer(&app, id, Some(Vec::new())).unwrap();
    assert!(copied.join().unwrap().is_ok());
}

/// Only programs on this machine use the clipboard tools' requests: a phone sending them for a
/// pane is refused, and no app is asked.
#[test]
fn a_phone_cannot_use_the_tools_requests() {
    let env = clip_env("clip-phone-asks", "copy-paste");
    let _daemon = env.start();
    let (app, rx) = env.connect();
    let pane = create(&app, &env.dir);
    focus(&app, pane);
    let stream = thurm_client::connect_daemon(&env.socket).unwrap();
    let phone =
        Client::from_stream(stream, "e2e-ios", false, &[CAP_MOBILE], |_| {}, || {}).unwrap();
    for req in [
        Request::ClipboardGet {
            pane,
            mime: CLIP_TEXT.into(),
        },
        Request::ClipboardSet {
            pane,
            text: "x".into(),
        },
        Request::ClipboardClear { pane },
    ] {
        let e = phone.request(req).unwrap_err().to_string();
        assert!(e.contains("programs on this machine"), "{e}");
    }
    nothing_asked(&rx);
}

/// An app from before clipboard tools, a UI without the capability, that focuses `pane`. Raw:
/// [`Client`] would add the capabilities of this build.
fn old_app(env: &Env, pane: PaneId) -> UnixStream {
    let mut old = UnixStream::connect(&env.socket).unwrap();
    let hello = Request::Hello {
        client: "e2e-old-app".into(),
        version: PROTOCOL_VERSION,
        ui: true,
        capabilities: vec![CAP_ACTIVE_SIZE.into()],
    };
    old.write_all(
        &codec::encode(&Envelope {
            id: 1,
            request: hello,
        })
        .unwrap(),
    )
    .unwrap();
    let focus_old = Request::Focus {
        pane,
        focused: true,
    };
    old.write_all(
        &codec::encode(&Envelope {
            id: 2,
            request: focus_old,
        })
        .unwrap(),
    )
    .unwrap();
    old
}

#[test]
fn older_apps_and_phones_are_never_asked() {
    let env = clip_env("clip-old", "copy-paste");
    let _daemon = env.start();
    let (app, _) = env.connect();
    let pane = create(&app, &env.dir);
    drop(app);
    let mut old = old_app(&env, pane);
    // A phone, which says it can (its build has the capability) but is not a desktop UI.
    let stream = thurm_client::connect_daemon(&env.socket).unwrap();
    let (tx, phone_rx) = unbounded();
    let phone = Client::from_stream(
        stream,
        "e2e-ios",
        true,
        &[CAP_MOBILE],
        move |ev| {
            let _ = tx.send(ev);
        },
        || {},
    )
    .unwrap();
    focus(&phone, pane);
    std::thread::sleep(Duration::from_millis(200));
    let e = get(&tool(&env), pane, CLIP_TEXT)
        .join()
        .unwrap()
        .unwrap_err();
    assert!(e.contains("no Thurm app"), "{e}");
    // Nor are copies and clears, which go out as another event.
    for req in [
        Request::ClipboardSet {
            pane,
            text: "copied".into(),
        },
        Request::ClipboardClear { pane },
    ] {
        let e = ask(&tool(&env), req).join().unwrap().unwrap_err();
        assert!(e.contains("no Thurm app"), "{e}");
    }
    nothing_asked(&phone_rx);
    old.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    while let Ok(Some(frame)) = codec::read_frame(&mut old) {
        let msg: ServerMessage = codec::decode(&frame).unwrap();
        assert!(
            !matches!(
                msg,
                ServerMessage::Event(
                    Event::ClipboardQuery { .. }
                        | Event::ClipboardWrite { .. }
                        | Event::ClipboardStore { .. }
                )
            ),
            "{msg:?}"
        );
    }
}

/// A phone or an app from before clipboard tools looking at the pane after the desktop app did
/// leaves that app the one asked: neither of them could be.
#[test]
fn a_phone_or_older_app_looking_last_leaves_the_app_asked() {
    let env = clip_env("clip-not-last", "copy-paste");
    let _daemon = env.start();
    let (a, a_rx) = env.connect();
    let (b, b_rx) = env.connect();
    let pane = create(&a, &env.dir);
    focus(&a, pane);
    focus(&b, pane);
    let (tx, phone_rx) = unbounded();
    let phone = Client::from_stream(
        thurm_client::connect_daemon(&env.socket).unwrap(),
        "e2e-ios",
        true,
        &[CAP_MOBILE],
        move |ev| {
            let _ = tx.send(ev);
        },
        || {},
    )
    .unwrap();
    focus(&phone, pane);
    let _old = old_app(&env, pane);
    std::thread::sleep(Duration::from_millis(200));
    let paste = get(&tool(&env), pane, CLIP_TEXT);
    let Some((_, id, _)) = query(&b_rx) else {
        panic!("b is still the app that looked last: {:?}", paste.join());
    };
    answer(&b, id, Some(b"from b".to_vec())).unwrap();
    assert_eq!(paste.join().unwrap(), Ok(b"from b".to_vec()));
    no_query(&a_rx);
    nothing_asked(&phone_rx);
}

/// The clipboard tools stay first on PATH though the user's startup files put another `xclip`
/// in front of them: zsh reads `.zshrc` after the integration loads, fish its `config.fish`.
/// The daemon starts with a clean environment, so the shells read only the files written here.
#[test]
fn clipboard_tools_come_first_after_the_startup_files() {
    use std::os::unix::fs::PermissionsExt;
    let on_path = |name: &str| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    };
    let shells: Vec<String> = ["zsh", "bash", "fish"]
        .into_iter()
        .filter_map(on_path)
        .map(|p| p.display().to_string())
        .collect();
    // zsh twice: GLOB_SUBST changes how it reads a substituted value in a pattern.
    // zsh twice more: GLOB_SUBST changes how it reads a substituted value in a pattern. And
    // each shell once with a startup file that sets a display: its own tools then work.
    let cases: Vec<(&String, &str, bool)> = shells
        .iter()
        .flat_map(|sh| {
            let opts: &[&str] = if sh.ends_with("/zsh") {
                &["", "setopt globsubst\n"]
            } else {
                &[""]
            };
            opts.iter()
                .map(move |o| (sh, *o, false))
                .chain([(sh, "", true)])
        })
        .collect();
    let mut tested: Vec<String> = Vec::new();
    for (i, (shell, zsh_opts, display)) in cases.into_iter().enumerate() {
        let name = shell.rsplit('/').next().unwrap();
        let env = Env::new(&format!("clip-path-{name}-{i}"));
        let home = env.dir.join("home");
        // A glob character in the shims' path, and a directory its pattern would match: the
        // shims must be compared literally.
        let shims = env.dir.join("shims*x");
        let lookalike = env.dir.join("shims1x");
        let real = home.join("real");
        for dir in [&shims, &lookalike, &real, &home.join(".config/fish")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for (dir, name) in [
            (&shims, "xclip"),
            (&real, "xclip"),
            (&lookalike, "lookalike-tool"),
        ] {
            let tool = dir.join(name);
            std::fs::write(&tool, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The user's startup files put another xclip first, keep the look-alike directory and
        // an empty entry (the current directory), and name the shims twice side by side and
        // once more further down.
        let entries = format!(
            "{}::{}:{s}:{s}:$PATH:{s}",
            real.display(),
            lookalike.display(),
            s = shims.display()
        );
        let set_display = if display { "export DISPLAY=:9\n" } else { "" };
        let rc = format!("PS1='$ '\nexport PATH=\"{entries}\"\n{set_display}");
        std::fs::write(home.join(".zshrc"), format!("{zsh_opts}{rc}")).unwrap();
        std::fs::write(home.join(".bashrc"), &rc).unwrap();
        // The integration goes in through XDG_DATA_DIRS, which this machine's system-wide fish
        // setup may replace before fish reads it: then it is loaded here, as fish would have, so
        // the fish cases run everywhere fish is. Loaded already, it does nothing a second time.
        std::fs::write(
            home.join(".config/fish/config.fish"),
            format!(
                "source $THURM_SHELL_INTEGRATION/fish/vendor_conf.d/thurm.fish\n\
                 set -gx PATH {:?} . {:?} {s:?} {s:?} $PATH {s:?}\n{}function fish_prompt; echo -n '$ '; end\n",
                real.display().to_string(),
                lookalike.display().to_string(),
                if display { "set -gx DISPLAY :9\n" } else { "" },
                s = shims.display().to_string()
            ),
        )
        .unwrap();
        std::fs::write(
            env.dir.join("config/config.toml"),
            format!(
                "[terminal]\nshell = [{shell:?}]\n[terminal.env]\nTHURM_CLIP_SHIMS = {:?}\n",
                shims.display().to_string()
            ),
        )
        .unwrap();
        // As under a login shell that set up a NixOS or nix-darwin environment: their system
        // fish setup replaces XDG_DATA_DIRS, where the integration goes in, unless it is done.
        let done = Path::new("1");
        let _daemon = env.start_clean(&[
            ("HOME", &home),
            ("ZDOTDIR", &home),
            ("__NIX_DARWIN_SET_ENVIRONMENT_DONE", done),
            ("__NIXOS_SET_ENVIRONMENT_DONE", done),
        ]);
        let (c, _) = env.connect();
        let Response::PaneCreated { pane } = c
            .request(Request::CreatePane(CreatePane {
                cwd: Some(home.display().to_string()),
                size: PaneSize {
                    cols: 300,
                    rows: 24,
                    cell_width: 8,
                    cell_height: 16,
                },
                ..Default::default()
            }))
            .unwrap()
        else {
            panic!("no pane");
        };
        // Not waiting for a prompt: system-wide startup files may give it any look. A line
        // typed now waits for the shell, which runs it after its first prompt.
        if name == "fish" {
            assert!(
                fish_integration_loaded(&c, pane),
                "the integration did not load in fish"
            );
        }
        // With a display, the real xclip, and the shims off PATH.
        let first = if display { &real } else { &shims };
        let first = regex_escape(&first.join("xclip").display().to_string());
        send_until(&c, pane, b"command -v xclip\r", &format!("(?m)^{first}$"));
        let kept = regex_escape(&lookalike.join("lookalike-tool").display().to_string());
        send_until(
            &c,
            pane,
            b"command -v lookalike-tool\r",
            &format!("(?m)^{kept}$"),
        );
        // Once on PATH (none with a display): the line counts the entries with `s*x`, which
        // only the shims' has.
        send_until(
            &c,
            pane,
            b"sh -c 'echo SHIMS-$(echo \"$PATH\" | tr : \"\\n\" | grep -cF \"s*x\")'\r",
            if display {
                "(?m)^SHIMS-0$"
            } else {
                "(?m)^SHIMS-1$"
            },
        );
        // The empty entry (fish spells it `.`) is still there.
        send_until(
            &c,
            pane,
            b"sh -c 'case \":$PATH:\" in *::*|*:.:*) echo EMPTY-$((1))KEPT;; *) echo EMPTY-LOST;; esac'\r",
            "(?m)^EMPTY-1KEPT$",
        );
        tested.push(name.to_owned());
    }
    assert!(
        tested.iter().any(|s| s != "fish"),
        "no zsh or bash to test with: {shells:?}"
    );
    assert_eq!(
        tested.iter().filter(|s| *s == "fish").count(),
        if shells.iter().any(|s| s.ends_with("/fish")) {
            2
        } else {
            0
        },
        "each fish case ran"
    );
}

/// Whether Thurm's integration runs in the fish of `pane`.
fn fish_integration_loaded(c: &Client, pane: PaneId) -> bool {
    send_until(
        c,
        pane,
        b"set -q __thurm_fish_loaded; and echo THURM-(echo ON); or echo THURM-(echo OFF)\r",
        "(?m)^THURM-(ON|OFF)$",
    );
    capture(c, pane).lines().any(|l| l == "THURM-ON")
}

fn regex_escape(s: &str) -> String {
    s.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '/' || ch == '_' || ch == '-' {
                ch.to_string()
            } else {
                format!("\\{ch}")
            }
        })
        .collect()
}
