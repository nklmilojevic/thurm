//! The extension API: actions provided by clients and the config, events, services.

use serde_json::{Value, json};
use thurm_proto::ext::{method, notice};

use super::*;

/// A client of the extension API: its notices come on the receiver as (name, payload).
fn ext_client(env: &Env, name: &'static str) -> (Arc<Client>, Receiver<(String, Value)>) {
    let (tx, rx) = unbounded();
    let c = Client::connect_with(
        ConnectOptions {
            socket: env.socket.clone(),
            spawn_daemon: None,
            client_name: name,
            ui: false,
        },
        &[CAP_EXT],
        move |ev| {
            if let Event::Ext { name, payload } = ev {
                let _ = tx.send((name, serde_json::from_str(&payload).unwrap()));
            }
        },
        || {},
    )
    .expect("connect");
    (c, rx)
}

fn call(c: &Client, method: &str, params: Value) -> Result<Value, String> {
    match c.request(Request::Ext {
        method: method.into(),
        params: params.to_string(),
    }) {
        Ok(Response::Text(t)) => Ok(serde_json::from_str(&t).unwrap()),
        Ok(other) => panic!("{other:?}"),
        Err(e) => Err(e.to_string()),
    }
}

/// The next notice called `name`, skipping others.
fn next(rx: &Receiver<(String, Value)>, name: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let (n, payload) = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("no {name} notice"));
        if n == name {
            return payload;
        }
    }
}

fn action_names(c: &Client) -> Vec<String> {
    call(c, method::ACTIONS_LIST, Value::Null).unwrap()["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_client_provides_an_action_another_runs() {
    let env = Env::new("ext-actions");
    std::fs::write(
        env.dir.join("config/config.toml"),
        "[terminal]\nshell = [\"/bin/sh\"]\n\n[[commands]]\nname = \"Deploy\"\nexec = [\"true\"]\nkey = \"cmd+shift+d\"\n",
    )
    .unwrap();
    let _d = env.start();
    let (provider, provider_rx) = ext_client(&env, "provider");
    let (user, user_rx) = ext_client(&env, "user");

    // The config's commands are actions, with their keys.
    let list = call(&user, method::ACTIONS_LIST, Value::Null).unwrap();
    assert_eq!(list["actions"][0]["name"], "Deploy");
    assert_eq!(list["actions"][0]["source"], "config");
    assert_eq!(list["bindings"][0]["key"], "cmd+shift+d");

    call(
        &provider,
        method::ACTIONS_REGISTER,
        json!({ "actions": [{ "name": "test.echo", "title": "Echo", "args": { "type": "object" } }] }),
    )
    .unwrap();
    next(&user_rx, notice::ACTIONS_CHANGED);
    assert_eq!(action_names(&user), ["Deploy", "test.echo"]);
    // A name another client provides is refused.
    let taken = call(
        &user,
        method::ACTIONS_REGISTER,
        json!({ "actions": [{ "name": "test.echo" }] }),
    )
    .unwrap_err();
    assert!(taken.contains("provider"), "{taken}");

    // The provider carries it out on its own thread and answers.
    let answering = std::thread::spawn({
        let provider = provider.clone();
        move || {
            let invoke = next(&provider_rx, notice::ACTION_INVOKE);
            assert_eq!(invoke["name"], "test.echo");
            assert_eq!(invoke["ctx"]["origin"], "user");
            assert_eq!(invoke["ctx"]["depth"], 1);
            call(
                &provider,
                method::ACTIONS_RESULT,
                json!({ "id": invoke["id"], "result": { "echo": invoke["args"]["say"] } }),
            )
            .unwrap();
            let failing = next(&provider_rx, notice::ACTION_INVOKE);
            call(
                &provider,
                method::ACTIONS_RESULT,
                json!({ "id": failing["id"], "error": "no can do" }),
            )
            .unwrap();
            provider_rx
        }
    });
    let result = call(
        &user,
        method::ACTIONS_RUN,
        json!({ "name": "test.echo", "args": { "say": "hi" } }),
    )
    .unwrap();
    assert_eq!(result, json!({ "echo": "hi" }));
    let failed = call(&user, method::ACTIONS_RUN, json!({ "name": "test.echo" })).unwrap_err();
    assert!(failed.contains("no can do"), "{failed}");
    let provider_rx = answering.join().unwrap();

    // A provider that leaves takes its actions along; a run waiting on it ends.
    let waiting = std::thread::spawn({
        let user = user.clone();
        move || call(&user, method::ACTIONS_RUN, json!({ "name": "test.echo" }))
    });
    // It has the run, and leaves without answering.
    next(&provider_rx, notice::ACTION_INVOKE);
    drop(provider);
    let gone = waiting.join().unwrap().unwrap_err();
    assert!(gone.contains("went away"), "{gone}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while action_names(&user) != ["Deploy"] {
        assert!(Instant::now() < deadline, "{:?}", action_names(&user));
        std::thread::sleep(Duration::from_millis(50));
    }
    let missing = call(&user, method::ACTIONS_RUN, json!({ "name": "test.echo" })).unwrap_err();
    assert!(missing.contains("no action"), "{missing}");
}

#[test]
fn subscribers_hear_events_and_services_answer() {
    let env = Env::new("ext-events");
    let _d = env.start();
    let (c, rx) = ext_client(&env, "listener");
    call(
        &c,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["pane_created", "pane_exited", "demo.ping"] }),
    )
    .unwrap();
    assert!(call(&c, method::EVENTS_SUBSCRIBE, json!({ "names": ["nope"] })).is_err());

    let pane = match c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec!["sh".into(), "-c".into(), "sleep 0.3; exit 5".into()]),
            cwd: Some(env.dir.display().to_string()),
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    let created = next(&rx, notice::EVENT);
    assert_eq!(created["event"], "pane_created");
    assert_eq!(created["pane"]["id"], pane);
    let exited = next(&rx, notice::EVENT);
    assert_eq!(exited["event"], "pane_exited");
    assert_eq!(exited["exit_code"], 5);

    // A plugin's own events need a dot.
    assert!(call(&c, method::EVENTS_EMIT, json!({ "name": "ping" })).is_err());
    call(
        &c,
        method::EVENTS_EMIT,
        json!({ "name": "demo.ping", "payload": { "n": 1 } }),
    )
    .unwrap();
    let ping = next(&rx, notice::EVENT);
    assert_eq!(ping["event"], "demo.ping");
    assert_eq!(ping["n"], 1);
    assert_eq!(ping["source"], "listener");

    // Any request, as JSON.
    let panes = call(&c, method::REQUEST, json!({ "request": "ListPanes" })).unwrap();
    assert!(panes["Panes"].is_array(), "{panes}");
    assert!(call(&c, method::REQUEST, json!({ "request": { "Hello": {} } })).is_err());

    let out = call(
        &c,
        method::EXEC,
        json!({ "argv": ["sh", "-c", "cat; echo \"$X\""], "stdin": "in\n", "env": { "X": "y" } }),
    )
    .unwrap();
    assert_eq!(out["stdout"], "in\ny\n");
    assert_eq!(out["ok"], true);

    // Asking the user needs an app that shows it.
    let none = call(&c, method::UI_CONFIRM, json!({ "title": "Sure?" })).unwrap_err();
    assert!(none.contains("window"), "{none}");
    assert!(call(&c, "no.such.method", Value::Null).is_err());
}

#[test]
fn an_app_answers_what_plugins_ask() {
    let env = Env::new("ext-ui");
    let _d = env.start();
    let (tx, app_rx) = unbounded();
    let app = Client::connect_with(
        ConnectOptions {
            socket: env.socket.clone(),
            spawn_daemon: None,
            client_name: "app",
            ui: true,
        },
        &[CAP_EXT, CAP_EXT_UI],
        move |ev| {
            if let Event::Ext { name, payload } = ev {
                let _ = tx.send((name, serde_json::from_str::<Value>(&payload).unwrap()));
            }
        },
        || {},
    )
    .unwrap();
    let (plugin, _rx) = ext_client(&env, "plugin");
    let asking = std::thread::spawn(move || {
        call(
            &plugin,
            method::UI_PICK,
            json!({ "title": "Branch", "items": [{ "id": "a", "title": "main" }, { "id": "b", "title": "dev" }] }),
        )
    });
    let req = next(&app_rx, notice::UI_REQUEST);
    assert_eq!(req["kind"], "pick");
    assert_eq!(req["items"][1]["title"], "dev");
    call(
        &app,
        method::UI_REPLY,
        json!({ "id": req["id"], "result": { "index": 1, "id": "b" } }),
    )
    .unwrap();
    assert_eq!(
        asking.join().unwrap().unwrap(),
        json!({ "index": 1, "id": "b" })
    );
}

/// Plugins' questions go to the desktop app on this machine, never to a tailnet caller, a
/// phone or a client without a UI, whatever they say in `Hello`.
#[test]
fn only_a_local_desktop_app_is_asked() {
    use thurm_proto::tailnet::write_ingress;
    let env = Env::new("ext-ui-trust");
    let _d = env.start();
    let claim = |ingress: Option<Principal>, ui: bool, caps: &[&str]| {
        let stream = std::os::unix::net::UnixStream::connect(&env.socket).unwrap();
        if let Some(p) = ingress {
            let mut s = stream.try_clone().unwrap();
            write_ingress(&mut s, &p).unwrap();
        }
        let (tx, rx) = unbounded();
        let c = Client::from_stream(
            stream,
            "claimer",
            ui,
            caps,
            move |ev| {
                if let Event::Ext { name, .. } = ev {
                    let _ = tx.send(name);
                }
            },
            || {},
        )
        .unwrap();
        (c, rx)
    };
    let tailnet = Principal {
        kind: PrincipalKind::Tailscale,
        display: "someone@example.com".into(),
        ..Default::default()
    };
    let _remote = claim(Some(tailnet), true, &[CAP_EXT, CAP_EXT_UI]);
    let _phone = claim(None, true, &[CAP_EXT, CAP_EXT_UI, CAP_MOBILE]);
    let _headless = claim(None, false, &[CAP_EXT, CAP_EXT_UI]);
    let (plugin, _rx) = ext_client(&env, "plugin");
    let e = call(&plugin, method::UI_INPUT, json!({ "title": "Password?" })).unwrap_err();
    assert!(e.contains("window"), "{e}");
    for (_, rx) in [&_remote, &_phone, &_headless] {
        assert!(rx.try_iter().all(|n| n != notice::UI_REQUEST));
    }
}

/// A command line may hold a secret: the user's own `[[on]]` hook gets it, a subscriber does
/// not.
#[test]
fn a_command_line_reaches_hooks_but_not_subscribers() {
    let env = Env::new("ext-command-line");
    let out = env.dir.join("hook.json");
    std::fs::write(
        env.dir.join("config/config.toml"),
        format!(
            "[terminal]\nshell = [\"/bin/sh\"]\n\n[[on]]\nevent = \"command_finished\"\nexec = [\"sh\", \"-c\", \"cat > \\\"$1\\\"\", \"sh\", {out:?}]\n"
        ),
    )
    .unwrap();
    let _d = env.start();
    let (c, rx) = ext_client(&env, "listener");
    call(
        &c,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["command_finished"] }),
    )
    .unwrap();
    c.request(Request::CreatePane(CreatePane {
        command: Some(vec![
            "sh".into(),
            "-c".into(),
            "printf '\\033]633;C\\007\\033]633;E;login --password hunter2\\007\\033]633;D;0\\007'; sleep 2".into(),
        ]),
        cwd: Some(env.dir.display().to_string()),
        ..Default::default()
    }))
    .unwrap();
    let event = next(&rx, notice::EVENT);
    assert_eq!(event["event"], "command_finished");
    assert_eq!(event["exit_code"], 0);
    assert!(!event.to_string().contains("hunter2"), "{event}");
    let deadline = Instant::now() + Duration::from_secs(20);
    let hook = loop {
        if let Ok(text) = std::fs::read_to_string(&out)
            && text.ends_with('\n')
        {
            break text;
        }
        assert!(Instant::now() < deadline, "the hook did not run");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(hook.contains("login --password hunter2"), "{hook}");
}

/// A daemon outside an app bundle cannot tell its app from another program of the same
/// user: unless told to trust them, it asks no client what plugins ask.
#[test]
fn an_unverified_app_is_not_asked() {
    let env = Env::new("ext-ui-unverified");
    let _d = env.start_with(&[("THURM_TRUST_UI_CLIENTS", Path::new("0"))]);
    let (tx, rx) = unbounded();
    let _app = Client::connect_with(
        ConnectOptions {
            socket: env.socket.clone(),
            spawn_daemon: None,
            client_name: "app",
            ui: true,
        },
        &[CAP_EXT, CAP_EXT_UI],
        move |ev| {
            if let Event::Ext { name, .. } = ev {
                let _ = tx.send(name);
            }
        },
        || {},
    )
    .unwrap();
    let (plugin, _prx) = ext_client(&env, "plugin");
    let e = call(&plugin, method::UI_CONFIRM, json!({ "title": "Sure?" })).unwrap_err();
    assert!(e.contains("window"), "{e}");
    assert!(rx.try_iter().all(|n| n != notice::UI_REQUEST));
}

/// The extension API is for this machine's clients: a tailnet caller may only read the lists.
#[test]
fn a_tailnet_caller_cannot_run_programs() {
    use thurm_proto::tailnet::write_ingress;
    let env = Env::new("ext-tailnet-exec");
    let _d = env.start();
    let stream = std::os::unix::net::UnixStream::connect(&env.socket).unwrap();
    let mut s = stream.try_clone().unwrap();
    write_ingress(
        &mut s,
        &Principal {
            kind: PrincipalKind::Tailscale,
            display: "someone@example.com".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let c = Client::from_stream(stream, "remote", false, &[CAP_EXT], |_| {}, || {}).unwrap();
    for (m, p) in [
        (method::ACTIONS_RUN, json!({ "name": "x" })),
        (method::ACTIONS_REGISTER, json!({ "actions": [] })),
        (method::EVENTS_SUBSCRIBE, json!({ "names": ["bell"] })),
        (method::EXEC, json!({ "argv": ["true"] })),
        (method::SCRIPT_RUN, json!({ "source": "return 1" })),
        (method::PLUGINS_DISABLE, json!({ "name": "x" })),
    ] {
        let e = call(&c, m, p).unwrap_err();
        assert!(e.contains("tailnet"), "{m}: {e}");
    }
    // Reading what there is still works.
    assert!(call(&c, method::ACTIONS_LIST, Value::Null).is_ok());
}
