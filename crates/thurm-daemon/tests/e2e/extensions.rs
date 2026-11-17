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

/// Without a pane an action runs for the focused one, unless the caller says it has none.
#[test]
fn no_pane_means_no_pane() {
    let env = Env::new("ext-no-pane");
    let _d = env.start();
    let (provider, provider_rx) = ext_client(&env, "provider");
    let (user, _rx) = ext_client(&env, "user");
    call(
        &provider,
        method::ACTIONS_REGISTER,
        json!({ "actions": [{ "name": "ctx.pane" }] }),
    )
    .unwrap();
    let answering = std::thread::spawn({
        let provider = provider.clone();
        move || {
            for _ in 0..2 {
                let invoke = next(&provider_rx, notice::ACTION_INVOKE);
                call(
                    &provider,
                    method::ACTIONS_RESULT,
                    json!({ "id": invoke["id"], "result": invoke["ctx"]["pane"] }),
                )
                .unwrap();
            }
        }
    });
    let pane = create(&user, &env.dir);
    user.request(Request::Focus {
        pane,
        focused: true,
    })
    .unwrap();
    let focused = call(&user, method::ACTIONS_RUN, json!({ "name": "ctx.pane" })).unwrap();
    assert_eq!(focused, json!(pane));
    let none = call(
        &user,
        method::ACTIONS_RUN,
        json!({ "name": "ctx.pane", "no_pane": true }),
    )
    .unwrap();
    assert_eq!(none, Value::Null);
    answering.join().unwrap();
}

/// A provider that runs its own action again, without saying how deep it is, still stops at
/// the limit: the daemon knows what it asked it to carry out.
#[test]
fn actions_running_one_another_stop_without_a_depth() {
    let env = Env::new("ext-depth");
    let _d = env.start();
    let (provider, provider_rx) = ext_client(&env, "provider");
    let (user, _rx) = ext_client(&env, "user");
    call(
        &provider,
        method::ACTIONS_REGISTER,
        json!({ "actions": [{ "name": "loop.again" }] }),
    )
    .unwrap();
    std::thread::spawn({
        let provider = provider.clone();
        move || {
            while let Ok((name, invoke)) = provider_rx.recv() {
                if name != notice::ACTION_INVOKE {
                    continue;
                }
                let provider = provider.clone();
                std::thread::spawn(move || {
                    let reply = match call(
                        &provider,
                        method::ACTIONS_RUN,
                        json!({ "name": "loop.again" }),
                    ) {
                        Ok(v) => json!({ "id": invoke["id"], "result": v }),
                        Err(e) => json!({ "id": invoke["id"], "error": e }),
                    };
                    let _ = call(&provider, method::ACTIONS_RESULT, reply);
                });
            }
        }
    });
    let e = call(&user, method::ACTIONS_RUN, json!({ "name": "loop.again" })).unwrap_err();
    assert!(e.contains("deep"), "{e}");
}

/// A provider that says which action it runs another from is as deep as that one, not as the
/// deepest it carries out: an unrelated chain in progress does not count against it.
#[test]
fn an_invocation_named_sets_the_depth() {
    let env = Env::new("ext-invocation-depth");
    let _d = env.start();
    let (provider, provider_rx) = ext_client(&env, "provider");
    let (user, _rx) = ext_client(&env, "user");
    call(
        &provider,
        method::ACTIONS_REGISTER,
        json!({ "actions": [{ "name": "deep.hold" }, { "name": "deep.leaf" }] }),
    )
    .unwrap();
    // One invocation is held open while an action is run from it, naming it.
    let hold = std::thread::spawn({
        let user = user.clone();
        move || call(&user, method::ACTIONS_RUN, json!({ "name": "deep.hold" }))
    });
    let held = next(&provider_rx, notice::ACTION_INVOKE);
    assert_eq!(held["ctx"]["depth"], 1);
    // An action run from it, naming it: depth 2.
    let leaf = std::thread::spawn({
        let provider = provider.clone();
        let id = held["id"].clone();
        move || {
            call(
                &provider,
                method::ACTIONS_RUN,
                json!({ "name": "deep.leaf", "invocation": id }),
            )
        }
    });
    let invoke = next(&provider_rx, notice::ACTION_INVOKE);
    assert_eq!(invoke["ctx"]["depth"], 2);
    call(
        &provider,
        method::ACTIONS_RESULT,
        json!({ "id": invoke["id"], "result": 1 }),
    )
    .unwrap();
    assert_eq!(leaf.join().unwrap().unwrap(), json!(1));
    call(
        &provider,
        method::ACTIONS_RESULT,
        json!({ "id": held["id"], "result": 0 }),
    )
    .unwrap();
    assert_eq!(hold.join().unwrap().unwrap(), json!(0));
}

#[test]
fn init_lua_provides_actions_and_scripts_run() {
    let env = Env::new("ext-lua");
    let init = env.dir.join("config/init.lua");
    std::fs::write(
        &init,
        r#"
        thurm.action{ name = "lua.greet", run = function(ctx, args)
            return "hello " .. (args or "you") .. " from " .. ctx.origin
        end }
        thurm.bind("cmd+shift+g", "lua.greet")
        "#,
    )
    .unwrap();
    let _d = env.start();
    let (c, rx) = ext_client(&env, "e2e");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !action_names(&c).contains(&"lua.greet".to_owned()) {
        assert!(Instant::now() < deadline, "init.lua did not load");
        std::thread::sleep(Duration::from_millis(50));
    }
    let list = call(&c, method::ACTIONS_LIST, Value::Null).unwrap();
    assert_eq!(list["bindings"][0]["action"], "lua.greet");
    assert_eq!(list["bindings"][0]["source"], "plugin:init");
    let r = call(
        &c,
        method::ACTIONS_RUN,
        json!({ "name": "lua.greet", "args": "e2e" }),
    )
    .unwrap();
    assert_eq!(r, "hello e2e from e2e");

    // A script calls the plugin's action, logs, and returns.
    let r = call(
        &c,
        method::SCRIPT_RUN,
        json!({ "source": "thurm.log('hi', thurm.args[1]); return thurm.run('lua.greet', 'script')", "args": ["x"] }),
    )
    .unwrap();
    assert_eq!(r, "hello script from plugin:script");
    let log = next(&rx, notice::SCRIPT_LOG);
    assert_eq!(log["message"], "hi x");
    let e = call(
        &c,
        method::SCRIPT_RUN,
        json!({ "source": "error('nope')", "name": "s.lua" }),
    )
    .unwrap_err();
    assert!(e.contains("s.lua:1: nope"), "{e}");

    // A broken init.lua leaves the running one as it is.
    std::fs::write(&init, "thurm.action{ name = ").unwrap();
    c.request(Request::ReloadConfig).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(action_names(&c).contains(&"lua.greet".to_owned()));
    // One that compiles but fails as it runs gives way to the one that worked: the app
    // hears why, and the working action is back without the new one's.
    let (_app, app_rx) = {
        let (tx, rx) = unbounded();
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
        (app, rx)
    };
    std::fs::write(
        &init,
        "thurm.action{ name = 'lua.half', run = function() end }\nerror('no')",
    )
    .unwrap();
    c.request(Request::ReloadConfig).unwrap();
    let toast = next(&app_rx, notice::UI_REQUEST);
    assert!(
        toast["text"].as_str().unwrap().contains("init.lua:2: no;"),
        "{toast}"
    );
    // The fallback ran to its end: its key binding, the last thing it sets up, is there, and
    // its action answers.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let list = call(&c, method::ACTIONS_LIST, Value::Null).unwrap();
        let bound = list["bindings"]
            .as_array()
            .is_some_and(|b| b.iter().any(|b| b["action"] == "lua.greet"));
        if bound && action_names(&c) == ["lua.greet"] {
            break;
        }
        assert!(Instant::now() < deadline, "{list}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let r = call(
        &c,
        method::ACTIONS_RUN,
        json!({ "name": "lua.greet", "args": "again" }),
    )
    .unwrap();
    assert_eq!(r, "hello again from e2e");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(action_names(&c), ["lua.greet"]);

    // Reloading runs the new init.lua: its old actions go.
    std::fs::write(
        &init,
        "thurm.action{ name = 'lua.other', run = function() end }",
    )
    .unwrap();
    c.request(Request::ReloadConfig).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while action_names(&c) != ["lua.other"] {
        assert!(Instant::now() < deadline, "{:?}", action_names(&c));
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `thurm do` connects asking for no capabilities: a script still runs, its logs come back,
/// and so does what it returns.
#[test]
fn a_client_without_capabilities_runs_scripts() {
    let env = Env::new("ext-do-plain");
    let _d = env.start();
    let (tx, rx) = unbounded();
    let c = Client::connect_with(
        ConnectOptions {
            socket: env.socket.clone(),
            spawn_daemon: None,
            client_name: "thurm-do",
            ui: false,
        },
        &[],
        move |ev| {
            if let Event::Ext { name, payload } = ev {
                let _ = tx.send((name, serde_json::from_str::<Value>(&payload).unwrap()));
            }
        },
        || {},
    )
    .expect("connect");
    let r = call(
        &c,
        method::SCRIPT_RUN,
        json!({ "source": "thurm.log('working'); return #thurm.panes()" }),
    )
    .unwrap();
    assert_eq!(r, 0);
    let log = next(&rx, notice::SCRIPT_LOG);
    assert_eq!(log["message"], "working");
}

/// The threads of a process.
fn thread_count(pid: u32) -> usize {
    if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        return tasks.count();
    }
    let out = Command::new("ps")
        .args(["-M", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .count()
        .saturating_sub(1)
}

/// Scripts that ended leave nothing running behind them.
#[test]
fn finished_scripts_leave_no_threads() {
    let env = Env::new("ext-script-threads");
    let d = env.start();
    let (c, _rx) = ext_client(&env, "e2e");
    let run = || {
        call(
            &c,
            method::SCRIPT_RUN,
            json!({ "source": "thurm.sleep(0.01); return 1" }),
        )
        .unwrap()
    };
    run();
    std::thread::sleep(Duration::from_millis(300));
    let before = thread_count(d.child.id());
    for _ in 0..30 {
        run();
    }
    std::thread::sleep(Duration::from_millis(500));
    let after = thread_count(d.child.id());
    assert!(
        after <= before + 5,
        "{before} threads before, {after} after"
    );
}

/// Writes plugin `name` from its template into the env's plugins directory.
fn scaffold(env: &Env, name: &str, program: bool) {
    use std::os::unix::fs::PermissionsExt;
    let dir = env.dir.join("config/plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    for (file, text, executable) in thurm_config::plugins::scaffold(name, program) {
        std::fs::write(dir.join(file), text).unwrap();
        if executable {
            std::fs::set_permissions(dir.join(file), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
    }
}

fn plugin<'a>(list: &'a Value, name: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("no {name} in {list}"))
}

/// Polls `plugins.list` until `ok` holds for it.
fn until_plugins(c: &Client, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let list = call(c, method::PLUGINS_LIST, Value::Null).unwrap();
        if ok(&list) {
            return list;
        }
        assert!(Instant::now() < deadline, "{what}: {list}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn plugins_in_the_plugins_directory_run_and_are_managed() {
    let python = Command::new("python3").arg("--version").output().is_ok();
    let env = Env::new("ext-plugins");
    scaffold(&env, "luaplug", false);
    if python {
        scaffold(&env, "pyplug", true);
    }
    let plugins = env.dir.join("config/plugins");
    std::fs::write(
        plugins.join("noisy.lua"),
        "thurm.log('hello log'); thurm.warn('careful')",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let crash = plugins.join("crash.sh");
        std::fs::write(&crash, "#!/bin/sh\necho dying >&2\nexit 3\n").unwrap();
        std::fs::set_permissions(&crash, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _d = env.start();
    let (c, _rx) = ext_client(&env, "e2e");

    let expected: &[&str] = if python {
        &["luaplug.hello", "pyplug.hello"]
    } else {
        &["luaplug.hello"]
    };
    let list = until_plugins(&c, "plugins up", |l| {
        expected.iter().all(|a| {
            l.as_array()
                .unwrap()
                .iter()
                .any(|p| p["actions"].as_array().unwrap().iter().any(|x| x == *a))
        })
    });
    assert_eq!(plugin(&list, "luaplug")["kind"], "lua");
    assert_eq!(plugin(&list, "luaplug")["state"], "running");
    assert_eq!(plugin(&list, "luaplug")["version"], "0.1.0");

    // Actions of both kinds run, and say hello over the app's pane.
    let (_app, app_rx) = app_client(&env);
    call(&c, method::ACTIONS_RUN, json!({ "name": "luaplug.hello" })).unwrap();
    assert_eq!(
        next(&app_rx, notice::UI_REQUEST)["text"],
        "Hello from luaplug"
    );
    if python {
        assert_eq!(plugin(&list, "pyplug")["kind"], "process");
        call(&c, method::ACTIONS_RUN, json!({ "name": "pyplug.hello" })).unwrap();
        assert_eq!(
            next(&app_rx, notice::UI_REQUEST)["text"],
            "Hello from pyplug"
        );
    }

    // A program that keeps exiting waits longer each time: 1 s, then 2 s.
    until_plugins(&c, "crash fails", |l| {
        plugin(l, "crash")["error"]
            .as_str()
            .is_some_and(|e| e.contains("status 3; starting again in 1s"))
    });
    let list = until_plugins(&c, "crash fails again", |l| {
        plugin(l, "crash")["error"]
            .as_str()
            .is_some_and(|e| e.contains("status 3; starting again in 2s"))
    });
    assert_eq!(plugin(&list, "crash")["state"], "failed");
    let logs = call(&c, method::PLUGINS_LOGS, json!({ "name": "crash" })).unwrap();
    assert!(logs["lines"][0] == "dying", "{logs}");
    let logs = call(&c, method::PLUGINS_LOGS, json!({ "name": "noisy" })).unwrap();
    assert_eq!(logs["lines"], json!(["hello log", "[warn] careful"]));

    // Disabled, it stops and its actions go; the config file says so.
    call(&c, method::PLUGINS_DISABLE, json!({ "name": "luaplug" })).unwrap();
    let list = until_plugins(&c, "disabled", |l| {
        plugin(l, "luaplug")["state"] == "disabled"
    });
    assert!(
        plugin(&list, "luaplug")["actions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let config = std::fs::read_to_string(env.dir.join("config/config.toml")).unwrap();
    assert!(config.contains("disabled = [\"luaplug\"]"), "{config}");
    assert!(!action_names(&c).contains(&"luaplug.hello".to_owned()));
    call(&c, method::PLUGINS_ENABLE, json!({ "name": "luaplug" })).unwrap();
    until_plugins(&c, "enabled", |l| {
        plugin(l, "luaplug")["state"] == "running"
    });

    // A plugin removed from the directory goes on reload.
    std::fs::remove_file(plugins.join("noisy.lua")).unwrap();
    call(&c, method::PLUGINS_RELOAD, Value::Null).unwrap();
    until_plugins(&c, "removed", |l| {
        !l.as_array().unwrap().iter().any(|p| p["name"] == "noisy")
    });
}

/// A script whose client leaves stops, and so does a program it waits on.
#[test]
fn a_scripts_program_stops_when_its_client_leaves() {
    let env = Env::new("ext-script-leaves");
    let _d = env.start();
    let pidfile = env.dir.join("prog.pid");
    let stream = std::os::unix::net::UnixStream::connect(&env.socket).unwrap();
    let closer = stream.try_clone().unwrap();
    let c = Client::from_stream(stream, "e2e", false, &[CAP_EXT], |_| {}, || {}).unwrap();
    let source = format!(
        "thurm.exec({{ 'sh', '-c', 'echo $$ > {}; exec sleep 30' }})",
        pidfile.display()
    );
    let waiting = std::thread::spawn({
        let c = c.clone();
        move || call(&c, method::SCRIPT_RUN, json!({ "source": source }))
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let pid: i32 = loop {
        if let Ok(t) = std::fs::read_to_string(&pidfile)
            && let Ok(p) = t.trim().parse()
        {
            break p;
        }
        assert!(Instant::now() < deadline, "the program did not start");
        std::thread::sleep(Duration::from_millis(50));
    };
    // The CLI goes (Ctrl-C).
    closer.shutdown(std::net::Shutdown::Both).unwrap();
    let at = Instant::now();
    let _ = waiting.join();
    let running = || {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        !state.is_empty() && !state.starts_with('Z')
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while running() {
        assert!(
            Instant::now() < deadline,
            "program {pid} outlived its script"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(at.elapsed() < Duration::from_secs(15), "{:?}", at.elapsed());
}

/// Scripts waiting on an action nobody answers end when their clients go, and so do the
/// calls they were waiting on.
#[test]
fn a_scripts_waiting_calls_end_when_its_client_leaves() {
    let env = Env::new("ext-script-calls");
    let d = env.start();
    // Provides an action and never answers it.
    let (provider, _prx) = ext_client(&env, "provider");
    call(
        &provider,
        method::ACTIONS_REGISTER,
        json!({ "actions": [{ "name": "never.answers" }] }),
    )
    .unwrap();
    // A pane that runs on: waiting for its exit waits as long as it does.
    let pane = match provider
        .request(Request::CreatePane(CreatePane {
            command: Some(vec!["sleep".into(), "60".into()]),
            cwd: Some(env.dir.display().to_string()),
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    std::thread::sleep(Duration::from_millis(300));
    let before = thread_count(d.child.id());
    let mut closers = Vec::new();
    let mut waiting = Vec::new();
    for i in 0..8 {
        let source = if i % 2 == 0 {
            "thurm.run('never.answers')".to_owned()
        } else {
            format!("thurm.wait({pane}, {{ exit = true }})")
        };
        let stream = std::os::unix::net::UnixStream::connect(&env.socket).unwrap();
        closers.push(stream.try_clone().unwrap());
        let c = Client::from_stream(stream, "e2e", false, &[CAP_EXT], |_| {}, || {}).unwrap();
        waiting.push(std::thread::spawn(move || {
            call(&c, method::SCRIPT_RUN, json!({ "source": source }))
        }));
    }
    std::thread::sleep(Duration::from_millis(1000));
    assert!(
        thread_count(d.child.id()) > before + 8,
        "the scripts are not waiting"
    );
    for c in &closers {
        c.shutdown(std::net::Shutdown::Both).unwrap();
    }
    for w in waiting {
        let _ = w.join();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = thread_count(d.child.id());
        if now <= before + 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{before} threads before, {now} now"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A desktop app on this machine: it is asked what plugins ask.
fn app_client(env: &Env) -> (Arc<Client>, Receiver<(String, Value)>) {
    let (tx, rx) = unbounded();
    let c = Client::connect_with(
        ConnectOptions {
            socket: env.socket.clone(),
            spawn_daemon: None,
            client_name: "app",
            ui: true,
        },
        &[CAP_EXT, CAP_EXT_UI],
        move |ev| {
            if let Event::Ext { name, payload } = ev {
                let _ = tx.send((name, serde_json::from_str(&payload).unwrap()));
            }
        },
        || {},
    )
    .unwrap();
    (c, rx)
}

/// A program plugin that ignores SIGTERM and writes after it was disabled has no say: what
/// it asks then is not done.
#[test]
fn a_stopped_program_plugin_is_not_heard() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("ext-zombie");
    let plugins = env.dir.join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let script = plugins.join("zombie.sh");
    std::fs::write(
        &script,
        r#"#!/bin/sh
trap '' TERM
echo '{"jsonrpc":"2.0","id":1,"method":"actions.register","params":{"actions":[{"name":"zombie.alive"}]}}'
cat > /dev/null
touch "$THURM_PLUGIN_DIR/late-written"
echo '{"jsonrpc":"2.0","id":2,"method":"actions.register","params":{"actions":[{"name":"zombie.late"}]}}'
sleep 10
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _d = env.start();
    let (c, _rx) = ext_client(&env, "e2e");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !action_names(&c).contains(&"zombie.alive".to_owned()) {
        assert!(Instant::now() < deadline, "the plugin did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    call(&c, method::PLUGINS_DISABLE, json!({ "name": "zombie" })).unwrap();
    // It writes its late request (after this mark) once its input closed.
    let mark = plugins.join("late-written");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !mark.exists() {
        assert!(
            Instant::now() < deadline,
            "the plugin did not write after its stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(500));
    let names = action_names(&c);
    assert!(!names.iter().any(|n| n.starts_with("zombie.")), "{names:?}");
}

/// Reloading one plugin reads its manifest again.
#[test]
fn reloading_a_plugin_by_name_reads_its_files_again() {
    let env = Env::new("ext-reload-one");
    let dir = env.dir.join("config/plugins/tool");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "version = \"1.0.0\"\nmain = \"a.lua\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("a.lua"),
        "thurm.action{ name = 'tool.a', run = function() end }",
    )
    .unwrap();
    std::fs::write(
        dir.join("b.lua"),
        "thurm.action{ name = 'tool.b', run = function() end }",
    )
    .unwrap();
    // Another Lua plugin, which a reload of `tool` leaves running: it notes each start.
    let starts = env.dir.join("other-starts");
    std::fs::write(
        env.dir.join("config/plugins/other.lua"),
        format!(
            "local f = assert(io.open({:?}, 'a')) f:write('start\\n') f:close()",
            starts.display().to_string()
        ),
    )
    .unwrap();
    let _d = env.start();
    let (c, _rx) = ext_client(&env, "e2e");
    until_plugins(&c, "started", |l| {
        plugin(l, "tool")["actions"] == json!(["tool.a"])
    });
    std::fs::write(
        dir.join("plugin.toml"),
        "version = \"2.0.0\"\nmain = \"b.lua\"\n",
    )
    .unwrap();
    call(&c, method::PLUGINS_RELOAD, json!({ "name": "tool" })).unwrap();
    let list = until_plugins(&c, "reloaded", |l| {
        plugin(l, "tool")["actions"] == json!(["tool.b"])
    });
    assert_eq!(plugin(&list, "tool")["version"], "2.0.0");
    // Given the time a restart would take, `other` started once.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(std::fs::read_to_string(&starts).unwrap(), "start\n");
}

/// A program plugin's leader may exit on SIGTERM while something it started ignores it:
/// that one is stopped too when the plugin is disabled.
#[test]
fn a_disabled_plugin_leaves_nothing_running() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("ext-group");
    let plugins = env.dir.join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let pidfile = env.dir.join("child.pid");
    let script = plugins.join("parent.sh");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
sh -c 'trap "" TERM; while :; do sleep 1; done' &
echo $! > {pidfile:?}
echo '{{"jsonrpc":"2.0","id":1,"method":"actions.register","params":{{"actions":[{{"name":"parent.up"}}]}}}}'
trap 'exit 0' TERM
while :; do sleep 1; done
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _d = env.start();
    let (c, _rx) = ext_client(&env, "e2e");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !action_names(&c).contains(&"parent.up".to_owned()) {
        assert!(Instant::now() < deadline, "the plugin did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let child: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    call(&c, method::PLUGINS_DISABLE, json!({ "name": "parent" })).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    // Gone, or a zombie nobody reaps (a container's init may not).
    let running = || {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", &child.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        !state.is_empty() && !state.starts_with('Z')
    };
    while running() {
        assert!(
            Instant::now() < deadline,
            "process {child} survived the plugin"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A reload keeps a Lua plugin whose new code doesn't compile running as it was, and starts
/// again a plugin that failed to start once it can.
#[test]
fn reloads_keep_what_works_and_retry_what_failed() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("ext-reload-retry");
    let plugins = env.dir.join("config/plugins");
    std::fs::create_dir_all(plugins.join("later")).unwrap();
    std::fs::write(
        plugins.join("good.lua"),
        "thurm.action{ name = 'good.one', run = function() end }",
    )
    .unwrap();
    std::fs::write(plugins.join("later/plugin.toml"), "exec = [\"./run.sh\"]\n").unwrap();
    let _d = env.start();
    let (c, _rx) = ext_client(&env, "e2e");
    let list = until_plugins(&c, "started", |l| {
        plugin(l, "good")["actions"] == json!(["good.one"])
            && plugin(l, "later")["state"] == "broken"
    });
    // A program that isn't there is listed as broken, saying why, and is not started.
    assert!(
        plugin(&list, "later")["error"]
            .as_str()
            .unwrap_or_default()
            .contains("run.sh is missing"),
        "{list}"
    );

    std::fs::write(plugins.join("good.lua"), "thurm.action{ name = ").unwrap();
    let run = plugins.join("later/run.sh");
    std::fs::write(
        &run,
        "#!/bin/sh\necho '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"actions.register\",\"params\":{\"actions\":[{\"name\":\"later.up\"}]}}'\ncat > /dev/null\n",
    )
    .unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    c.request(Request::ReloadConfig).unwrap();
    let list = until_plugins(&c, "reloaded", |l| {
        plugin(l, "later")["actions"] == json!(["later.up"])
    });
    assert_eq!(plugin(&list, "good")["actions"], json!(["good.one"]));
    assert!(
        plugin(&list, "good")["error"]
            .as_str()
            .unwrap_or_default()
            .contains("stays")
    );
}

#[test]
fn a_plugin_places_the_floating_pane_it_opens() {
    let env = Env::new("ext-float-place");
    let _d = env.start();
    let (app, app_rx) = env.connect();
    let (plugin, _rx) = ext_client(&env, "plugin");
    let target = create(&app, &env.dir);
    let opened = call(
        &plugin,
        method::PANES_OPEN,
        json!({
            "command": ["sleep", "30"],
            "target": target,
            "cells": { "col": 2, "row": -1, "cols": 40, "rows": 10 },
            "background": true,
        }),
    )
    .unwrap();
    assert_eq!(opened["shown"], json!(true));
    let pane = opened["pane"].as_u64().unwrap();
    let want = UiCommand::OpenFloatingAt {
        pane,
        target: Some(target),
        place: FloatPlace::Cells {
            anchor: target,
            col: 2,
            row: -1,
            cols: 40,
            rows: 10,
        },
        background: true,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = false;
    while Instant::now() < deadline && !got {
        got = matches!(app_rx.recv_timeout(Duration::from_millis(200)), Ok(Event::Ui(ref c)) if *c == want);
    }
    assert!(got, "no {want:?}");

    let err = call(
        &plugin,
        method::PANES_OPEN,
        json!({ "frame": [0.0, 0.0, 1.5, 1.0] }),
    )
    .unwrap_err();
    assert!(err.contains("frame"), "{err}");
    let err = call(
        &plugin,
        method::PANES_OPEN,
        json!({
            "command": ["sleep", "30"],
            "target": 999_999,
            "cells": { "cols": 40, "rows": 10 },
        }),
    )
    .unwrap_err();
    assert!(err.contains("no such pane"), "{err}");
    let panes = match app.request(Request::ListPanes).unwrap() {
        Response::Panes(p) => p.len(),
        other => panic!("{other:?}"),
    };
    assert_eq!(panes, 2, "a refused place opens no pane");
}

#[test]
fn an_app_from_before_placing_floats_the_pane_where_new_ones_go() {
    let env = Env::new("ext-float-old");
    let _d = env.start();
    let mut old = super::ui_layout::ui_with(
        &env,
        &[
            CAP_ACTIVE_SIZE,
            CAP_UI_LAYOUT,
            CAP_AGENT_WALL,
            CAP_FLOATING,
            CAP_FLOATING_NEW,
        ],
    );
    let (plugin, _rx) = ext_client(&env, "plugin");
    let target = create(&plugin, &env.dir);
    let opened = call(
        &plugin,
        method::PANES_OPEN,
        json!({
            "command": ["sleep", "30"],
            "target": target,
            "cells": { "cols": 40, "rows": 10 },
        }),
    )
    .unwrap();
    assert_eq!(opened["shown"], json!(true));
    let want = UiCommand::OpenFloating {
        pane: opened["pane"].as_u64().unwrap(),
        target: Some(target),
        background: false,
    };
    old.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    loop {
        let frame = codec::read_frame(&mut old).unwrap().expect("daemon closed");
        match codec::decode(&frame).unwrap() {
            ServerMessage::Event(Event::Ui(c)) => {
                assert_eq!(c, want);
                break;
            }
            _ => continue,
        }
    }
}

#[test]
fn an_action_hears_the_selection_the_file_and_the_remote_host() {
    let env = Env::new("ext-context");
    let _d = env.start();
    let (provider, provider_rx) = ext_client(&env, "provider");
    let (user, _rx) = ext_client(&env, "user");
    call(
        &provider,
        method::ACTIONS_REGISTER,
        json!({ "actions": [{ "name": "ctx.echo" }] }),
    )
    .unwrap();
    let answering = std::thread::spawn({
        let provider = provider.clone();
        move || {
            for _ in 0..3 {
                let invoke = next(&provider_rx, notice::ACTION_INVOKE);
                call(
                    &provider,
                    method::ACTIONS_RESULT,
                    json!({ "id": invoke["id"], "result": invoke["ctx"] }),
                )
                .unwrap();
            }
        }
    });
    let pane = create(&user, &env.dir);
    let ctx = call(
        &user,
        method::ACTIONS_RUN,
        json!({ "name": "ctx.echo", "pane": pane, "selection": "a b", "file": "src/x.rs" }),
    )
    .unwrap();
    assert_eq!(ctx["pane"], json!(pane));
    assert_eq!(ctx["selection"], json!("a b"));
    assert_eq!(ctx["file"], json!("src/x.rs"));
    assert!(ctx.get("remote").is_none(), "{ctx}");

    let ctx = call(
        &user,
        method::ACTIONS_RUN,
        json!({ "name": "ctx.echo", "no_pane": true, "remote": "devbox" }),
    )
    .unwrap();
    assert_eq!(ctx["pane"], Value::Null);
    assert_eq!(ctx["remote"], json!("devbox"));

    // A pane of this daemon's is not another host's.
    let ctx = call(
        &user,
        method::ACTIONS_RUN,
        json!({ "name": "ctx.echo", "pane": pane, "remote": "devbox" }),
    )
    .unwrap();
    assert!(ctx.get("remote").is_none(), "{ctx}");
    answering.join().unwrap();
}

/// The listener is a python3 program: required, so the ports are always tested.
fn require_python() {
    assert!(
        std::process::Command::new("python3")
            .args(["-c", ""])
            .status()
            .is_ok_and(|s| s.success()),
        "this test needs python3 (for a listener in a pane)"
    );
}

/// A pane whose program listens on a free loopback port, and prints `port=<n>`.
fn listener_pane(c: &Client, env: &Env) -> PaneId {
    let r = c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec![
                "python3".into(),
                "-c".into(),
                "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); print('port=%d' % s.getsockname()[1], flush=True); time.sleep(600)".into(),
            ]),
            cwd: Some(env.dir.display().to_string()),
            ..Default::default()
        }))
        .unwrap();
    match r {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    }
}

/// Asserts `c` hears no more `ports_changed` than it has: an event of its own, sent now, comes
/// next (what was sent before it would come first).
fn no_more_ports(c: &Client, rx: &Receiver<(String, Value)>) {
    call(
        c,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["test.sentinel"] }),
    )
    .unwrap();
    call(c, method::EVENTS_EMIT, json!({ "name": "test.sentinel" })).unwrap();
    loop {
        let ev = next(rx, notice::EVENT);
        assert_ne!(ev["event"], "ports_changed", "heard more: {ev}");
        if ev["event"] == "test.sentinel" {
            return;
        }
    }
}

/// The next `event` notice called `event`, skipping others.
fn next_event(rx: &Receiver<(String, Value)>, event: &str) -> Value {
    loop {
        let ev = next(rx, notice::EVENT);
        if ev["event"] == event {
            return ev;
        }
    }
}

/// A subscriber to `ports_changed` gets the daemon to watch its panes' ports, hears a pane's
/// list, and can ask for it; files sent to a pane are heard once their paths are typed.
#[test]
fn a_subscriber_hears_ports_and_uploaded_files() {
    use std::io::Write;
    require_python();
    let env = Env::new("ext-ports");
    let _d = env.start_with(&[("THURM_PORTS_INTERVAL_MS", Path::new("200"))]);
    let (c, rx) = ext_client(&env, "listener");
    call(
        &c,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed", "files_uploaded"] }),
    )
    .unwrap();

    {
        let pane = listener_pane(&c, &env);
        wait_match(&c, pane, r"port=\d+");
        let text = capture(&c, pane);
        let port: u64 = text[text.rfind("port=").unwrap() + 5..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap();
        let ev = loop {
            let ev = next_event(&rx, "ports_changed");
            if ev["id"] == pane {
                break ev;
            }
        };
        assert_eq!(ev["pane"]["id"], pane);
        assert!(ev.get("host").is_none(), "{ev}");
        assert_eq!(ev["ports"][0]["port"], port, "{ev}");
        assert_eq!(ev["complete"], true);
        let listed = call(&c, method::PORTS_LIST, json!({ "pane": pane })).unwrap();
        assert_eq!(listed[0]["pane"], pane);
        assert_eq!(listed[0]["ports"][0]["port"], port, "{listed}");
        assert!(call(&c, method::PORTS_LIST, json!({ "pane": 4242 })).is_err());
        // A pane that is not one is refused, not taken for every pane.
        for bad in [json!(-1), json!("3"), json!(1.5), json!(true)] {
            let e = call(&c, method::PORTS_LIST, json!({ "pane": bad })).unwrap_err();
            assert!(e.contains("not an id"), "{bad}: {e}");
        }
        assert!(call(&c, method::PORTS_LIST, json!({ "pane": null })).is_ok());
    }

    let pane = match c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "cat > /dev/null".into(),
            ]),
            cwd: Some(env.dir.display().to_string()),
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    let ticket = c.upload_begin(pane, "notes.txt", 5, None).unwrap();
    let (mut r, mut w) = thurm_client::stream::open(&env.socket, &ticket.ticket).unwrap();
    w.write_all(b"hello").unwrap();
    w.finish().unwrap();
    let mut rest = Vec::new();
    let _ = std::io::Read::read_to_end(&mut r, &mut rest);
    let ev = next_event(&rx, "files_uploaded");
    assert_eq!(ev["id"], pane);
    assert_eq!(ev["pane"]["id"], pane);
    let path = ev["paths"][0].as_str().unwrap();
    assert!(path.ends_with("/notes.txt"), "{ev}");
    assert_eq!(std::fs::read(path).unwrap(), b"hello");
}

/// An app that forwards, as the Mac app does.
fn forwarding_app(env: &Env, caps: &[&str]) -> (Arc<Client>, Receiver<(String, Value)>) {
    let (tx, rx) = unbounded();
    let c = Client::connect_with(
        ConnectOptions {
            socket: env.socket.clone(),
            spawn_daemon: None,
            client_name: "app",
            ui: true,
        },
        caps,
        move |ev| {
            if let Event::Ext { name, payload } = ev {
                let _ = tx.send((name, serde_json::from_str(&payload).unwrap()));
            }
        },
        || {},
    )
    .unwrap();
    (c, rx)
}

/// Forwards live in the app: a plugin's open and close go to it, what it reports is heard
/// as events, and its forwards close with it.
#[test]
fn forwards_go_through_the_app() {
    let env = Env::new("ext-forwards");
    let _d = env.start();
    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["forward_opened", "forward_closed", "ports_changed"] }),
    )
    .unwrap();

    // Without an app that forwards, a clear error; an app without the capability is not one.
    let (_old_app, old_rx) = forwarding_app(&env, &[CAP_EXT, CAP_EXT_UI]);
    let e = call(
        &plugin,
        method::FORWARDS_OPEN,
        json!({ "host": "devbox", "port": 5173 }),
    )
    .unwrap_err();
    assert!(e.contains("forwards ports"), "{e}");
    assert!(call(&plugin, method::FORWARDS_LIST, Value::Null).is_err());
    assert!(
        old_rx.try_iter().all(|(n, _)| n != notice::UI_REQUEST),
        "an app without ext-forwards was asked"
    );
    // Only the app reports.
    let e = call(&plugin, method::FORWARDS_REPORT, json!({ "forwards": [] })).unwrap_err();
    assert!(e.contains("only for the Thurm app"), "{e}");

    let (app, app_rx) = forwarding_app(&env, &[CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS]);
    assert_eq!(
        call(&plugin, method::FORWARDS_LIST, Value::Null).unwrap(),
        json!([])
    );
    let opening = std::thread::spawn({
        let plugin = plugin.clone();
        move || {
            call(
                &plugin,
                method::FORWARDS_OPEN,
                json!({ "host": "devbox", "port": 5173, "browse": true }),
            )
        }
    });
    let req = next(&app_rx, notice::UI_REQUEST);
    assert_eq!(req["kind"], "forward_open");
    assert_eq!(req["host"], "devbox");
    assert_eq!(req["port"], 5173);
    assert_eq!(req["browse"], true);
    let forward = json!({
        "id": 7, "host": "devbox", "remote_port": 5173, "local_port": 5174,
        "url": "http://localhost:5174/",
    });
    let mut answer = forward.clone();
    answer["new"] = json!(true);
    call(
        &app,
        method::UI_REPLY,
        json!({ "id": req["id"], "result": answer }),
    )
    .unwrap();
    assert_eq!(opening.join().unwrap().unwrap(), answer);

    call(
        &app,
        method::FORWARDS_REPORT,
        json!({ "forwards": [forward], "change": { "change": "opened", "forward": forward } }),
    )
    .unwrap();
    let opened = next_event(&rx, "forward_opened");
    assert_eq!(opened["id"], 7);
    assert_eq!(opened["url"], "http://localhost:5174/");
    assert_eq!(
        call(&plugin, method::FORWARDS_LIST, Value::Null).unwrap()[0]["local_port"],
        5174
    );

    // Closing goes to the app; what it answers comes back.
    let closing = std::thread::spawn({
        let plugin = plugin.clone();
        move || call(&plugin, method::FORWARDS_CLOSE, json!({ "id": 7 }))
    });
    let req = next(&app_rx, notice::UI_REQUEST);
    assert_eq!(req["kind"], "forward_close");
    assert_eq!(req["forward"], 7);
    call(
        &app,
        method::UI_REPLY,
        json!({ "id": req["id"], "error": "no forward 7" }),
    )
    .unwrap();
    assert!(
        closing
            .join()
            .unwrap()
            .unwrap_err()
            .contains("no forward 7")
    );

    // The port went away on the host: closed, and why.
    call(
        &app,
        method::FORWARDS_REPORT,
        json!({ "forwards": [], "change": { "change": "closed", "forward": forward, "why": "gone" } }),
    )
    .unwrap();
    let closed = next_event(&rx, "forward_closed");
    assert_eq!(closed["id"], 7);
    assert_eq!(closed["why"], "gone");

    // A remote host's ports, as the app hears them.
    call(
        &app,
        method::PORTS_REPORT,
        json!({ "host": "devbox", "pane": 3, "complete": true,
                "ports": [{ "port": 5173, "address": "127.0.0.1", "pid": 9, "command": "vite" }] }),
    )
    .unwrap();
    let ports = next_event(&rx, "ports_changed");
    assert_eq!(ports["host"], "devbox");
    assert_eq!(ports["id"], 3);
    assert_eq!(ports["ports"][0]["command"], "vite");
    assert!(ports.get("pane").is_none(), "{ports}");
    // Values that are not what they name are refused, not taken for a default.
    for bad in [
        json!({ "host": "devbox", "pane": -3, "ports": [] }),
        json!({ "host": "devbox", "pane": 3, "ports": [], "complete": "false" }),
        json!({ "host": "devbox", "pane": 3, "ports": [], "initial": 1 }),
        json!({ "host": "devbox", "gone": "yes" }),
    ] {
        assert!(
            call(&app, method::PORTS_REPORT, bad.clone()).is_err(),
            "{bad} was taken"
        );
    }
    let e = call(&plugin, method::FORWARDS_CLOSE, json!({ "id": -7 })).unwrap_err();
    assert!(e.contains("not an id"), "{e}");

    // The app quits: its forwards close with it.
    call(
        &app,
        method::FORWARDS_REPORT,
        json!({ "forwards": [forward] }),
    )
    .unwrap();
    assert_eq!(next_event(&rx, "forward_opened")["id"], 7);
    drop(app);
    let closed = next_event(&rx, "forward_closed");
    assert_eq!(closed["why"], "app");
}

/// With two apps that forward, each numbering its own forwards, plugins see only the first
/// app's: what is listed is what a close reaches. When it quits, the second's take over.
#[test]
fn the_first_app_holds_the_forwards() {
    let env = Env::new("ext-forwards-two");
    let _d = env.start();
    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["forward_opened", "forward_closed", "ports_changed"] }),
    )
    .unwrap();
    let caps = [CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS];
    let (first, first_rx) = forwarding_app(&env, &caps);
    let (second, second_rx) = forwarding_app(&env, &caps);
    // Both have a forward numbered 1.
    let forward = |port: u16| {
        json!({ "id": 1, "host": "devbox", "remote_port": port, "local_port": port,
                "url": format!("http://localhost:{port}/") })
    };
    call(
        &second,
        method::FORWARDS_REPORT,
        json!({ "forwards": [forward(8080)] }),
    )
    .unwrap();
    call(
        &first,
        method::FORWARDS_REPORT,
        json!({ "forwards": [forward(5173)] }),
    )
    .unwrap();
    let opened = next_event(&rx, "forward_opened");
    assert_eq!(opened["remote_port"], 5173, "{opened}");
    let list = call(&plugin, method::FORWARDS_LIST, Value::Null).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["remote_port"], 5173);

    // A close of what was listed goes to the app it came from, whichever is focused.
    let closing = std::thread::spawn({
        let plugin = plugin.clone();
        move || call(&plugin, method::FORWARDS_CLOSE, json!({ "id": 1 }))
    });
    let req = next(&first_rx, notice::UI_REQUEST);
    assert_eq!(req["kind"], "forward_close");
    call(
        &first,
        method::UI_REPLY,
        json!({ "id": req["id"], "result": forward(5173) }),
    )
    .unwrap();
    assert_eq!(closing.join().unwrap().unwrap()["remote_port"], 5173);
    assert!(
        second_rx.try_iter().all(|(n, _)| n != notice::UI_REQUEST),
        "the second app was asked"
    );

    // The first quits: its forward closes, the second's are the plugins' now.
    drop(first);
    let closed = next_event(&rx, "forward_closed");
    assert_eq!(
        (closed["remote_port"].clone(), closed["why"].clone()),
        (json!(5173), json!("app"))
    );
    let opened = next_event(&rx, "forward_opened");
    assert_eq!(opened["remote_port"], 8080, "{opened}");
    let list = call(&plugin, method::FORWARDS_LIST, Value::Null).unwrap();
    assert_eq!(list[0]["remote_port"], 8080, "{list}");

    // A host's lists as the app connected to it say so.
    let vite =
        |port: u16| json!([{ "port": port, "address": "127.0.0.1", "pid": 9, "command": "vite" }]);
    call(
        &second,
        method::PORTS_REPORT,
        json!({ "host": "devbox", "pane": 1, "ports": vite(5173), "complete": true, "initial": true }),
    )
    .unwrap();
    assert_eq!(next_event(&rx, "ports_changed")["initial"], true);
    call(
        &second,
        method::PORTS_REPORT,
        json!({ "host": "devbox", "pane": 1, "ports": vite(5174), "complete": true }),
    )
    .unwrap();
    assert!(next_event(&rx, "ports_changed").get("initial").is_none());
}

/// A plugin that subscribes after the app relayed a host's lists hears them first, as running
/// already; a host the app lost is not told; only the app that holds plugins' forwards is
/// heard, and when it quits, the next app's lists come as running already.
#[test]
fn a_late_subscriber_hears_the_remote_lists_as_initial() {
    let env = Env::new("ext-remote-ports");
    let _d = env.start();
    let caps = [CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS];
    let (first, _first_rx) = forwarding_app(&env, &caps);
    let (second, _second_rx) = forwarding_app(&env, &caps);
    let vite =
        |port: u16| json!({ "port": port, "address": "127.0.0.1", "pid": 9, "command": "vite" });
    let report = |app: &Client, host: &str, pane: u64, ports: Value| {
        call(
            app,
            method::PORTS_REPORT,
            json!({ "host": host, "pane": pane, "ports": ports, "complete": true }),
        )
        .unwrap();
    };
    // The host was connected and its server running before any plugin listened.
    report(&first, "devbox", 1, json!([vite(5173)]));
    report(&first, "pi", 2, json!([vite(3000)]));
    report(&first, "devbox", 3, json!([vite(8080)]));
    report(&first, "devbox", 3, json!([]));
    report(&second, "devbox", 1, json!([vite(9999)]));
    call(
        &first,
        method::PORTS_REPORT,
        json!({ "host": "pi", "gone": true }),
    )
    .unwrap();

    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    let first_list = next_event(&rx, "ports_changed");
    assert_eq!(first_list["host"], "devbox", "{first_list}");
    assert_eq!(first_list["id"], 1);
    assert_eq!(first_list["initial"], true);
    assert_eq!(first_list["ports"][0]["port"], 5173);

    // From now on, what changes is news; the second app is not heard.
    report(&second, "devbox", 1, json!([vite(9999), vite(9998)]));
    report(&first, "devbox", 1, json!([vite(5173), vite(5174)]));
    let change = next_event(&rx, "ports_changed");
    assert!(change.get("initial").is_none(), "{change}");
    assert_eq!(change["ports"][1]["port"], 5174, "{change}");

    // The first app quits: the second's lists are the plugins' now. Its list of pane 1 came as
    // changes (reported without `initial`), and goes to plugins as one.
    drop(first);
    let taken_over = next_event(&rx, "ports_changed");
    assert!(taken_over.get("initial").is_none(), "{taken_over}");
    assert_eq!(taken_over["ports"][1]["port"], 9998, "{taken_over}");
    report(&second, "devbox", 1, json!([]));
    let gone = next_event(&rx, "ports_changed");
    assert!(
        gone.get("initial").is_none() && gone["ports"] == json!([]),
        "{gone}"
    );
}

/// A plugin that subscribes to `ports_changed` while others listen gets every pane's list
/// first, as running already, and only it: the others hear nothing again. Subscribing again
/// changes nothing.
#[test]
fn a_new_ports_subscriber_alone_gets_every_list_first() {
    require_python();
    let env = Env::new("ext-ports-join");
    let _d = env.start_with(&[("THURM_PORTS_INTERVAL_MS", Path::new("200"))]);
    let (early, early_rx) = ext_client(&env, "early");
    call(
        &early,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    let pane = listener_pane(&early, &env);
    let heard = loop {
        let ev = next_event(&early_rx, "ports_changed");
        if ev["id"] == pane {
            break ev;
        }
    };
    assert!(
        heard.get("initial").is_none(),
        "a change, not a list resent: {heard}"
    );

    let (late, late_rx) = ext_client(&env, "late");
    call(
        &late,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    let first = loop {
        let ev = next_event(&late_rx, "ports_changed");
        if ev["id"] == pane {
            break ev;
        }
    };
    assert_eq!(first["initial"], true, "{first}");
    assert_eq!(first["pane"]["id"], pane);
    assert_eq!(first["ports"], heard["ports"]);

    // Subscribing again is not joining again.
    call(
        &early,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    // Several looks later, neither heard the pane's list again.
    std::thread::sleep(Duration::from_millis(1500));
    let again = |rx: &Receiver<(String, Value)>| {
        rx.try_iter()
            .filter(|(n, p)| n == notice::EVENT && p["event"] == "ports_changed" && p["id"] == pane)
            .count()
    };
    assert_eq!(
        again(&early_rx),
        0,
        "the early subscriber heard the list again"
    );
    assert_eq!(
        again(&late_rx),
        0,
        "the late subscriber heard the list twice"
    );
}

/// A host the app lost, or an app that quit, leaves no lists standing: each of their panes a
/// plugin heard of gets an empty list, not complete. Panes the next app knows are its to tell.
#[test]
fn a_lost_host_or_app_ends_its_remote_lists() {
    let env = Env::new("ext-remote-ports-end");
    let _d = env.start();
    let caps = [CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS];
    let (first, _first_rx) = forwarding_app(&env, &caps);
    let (second, _second_rx) = forwarding_app(&env, &caps);
    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    let vite =
        |port: u16| json!([{ "port": port, "address": "127.0.0.1", "pid": 9, "command": "vite" }]);
    let report = |app: &Client, host: &str, pane: u64, ports: Value| {
        call(
            app,
            method::PORTS_REPORT,
            json!({ "host": host, "pane": pane, "ports": ports, "complete": true }),
        )
        .unwrap();
    };
    report(&first, "devbox", 1, vite(5173));
    report(&first, "devbox", 2, vite(5174));
    report(&first, "pi", 3, vite(3000));
    report(&first, "nas", 4, vite(8080));
    report(&second, "nas", 4, vite(8080));
    for _ in 0..4 {
        next_event(&rx, "ports_changed");
    }

    // The app lost devbox: both its panes end, pi stands.
    call(
        &first,
        method::PORTS_REPORT,
        json!({ "host": "devbox", "gone": true }),
    )
    .map(|answer| assert_eq!(answer, json!({ "heard": true })))
    .unwrap();
    let mut ended: Vec<u64> = (0..2)
        .map(|_| {
            let ev = next_event(&rx, "ports_changed");
            assert_eq!(ev["host"], "devbox", "{ev}");
            assert_eq!(
                (ev["ports"].clone(), ev["complete"].clone()),
                (json!([]), json!(false))
            );
            assert_eq!(ev["gone"], true, "{ev}");
            ev["id"].as_u64().unwrap()
        })
        .collect();
    ended.sort_unstable();
    assert_eq!(ended, [1, 2]);

    // A host the second app lost is not the plugins' news: it does not hold them.
    let answer = call(
        &second,
        method::PORTS_REPORT,
        json!({ "host": "nas", "gone": true }),
    )
    .unwrap();
    assert_eq!(answer, json!({ "heard": false }));
    // The first quits: pi, which the second doesn't know, ends. nas, which the second has
    // forgotten too, ends; nothing else is said.
    drop(first);
    let mut hosts: Vec<String> = (0..2)
        .map(|_| {
            let ev = next_event(&rx, "ports_changed");
            assert_eq!(ev["ports"], json!([]), "{ev}");
            assert_eq!(ev["complete"], false, "{ev}");
            assert_eq!(ev["gone"], true, "{ev}");
            ev["host"].as_str().unwrap().to_owned()
        })
        .collect();
    hosts.sort();
    assert_eq!(hosts, ["nas", "pi"]);
    no_more_ports(&plugin, &rx);
}

/// A plugin that subscribes while the app's report of a host's list is under way hears that
/// list first as running already (its replay), never as a change ahead of it.
#[test]
fn a_report_under_way_does_not_overtake_a_new_subscribers_lists() {
    let env = Env::new("ext-report-race");
    let _d = env.start_with(&[("THURM_EXT_REPORT_DELAY_MS", Path::new("800"))]);
    let (app, _app_rx) = forwarding_app(&env, &[CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS]);
    let reporting = std::thread::spawn({
        let app = app.clone();
        move || {
            call(
                &app,
                method::PORTS_REPORT,
                json!({ "host": "devbox", "pane": 1, "complete": true,
                        "ports": [{ "port": 5173, "address": "127.0.0.1", "pid": 9, "command": "vite" }] }),
            )
        }
    });
    // The report holds the lists now; the plugin subscribes meanwhile.
    std::thread::sleep(Duration::from_millis(200));
    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    reporting.join().unwrap().unwrap();
    let first = next_event(&rx, "ports_changed");
    assert_eq!(
        first["initial"], true,
        "a change came before the lists: {first}"
    );
    assert_eq!(first["ports"][0]["port"], 5173);
    no_more_ports(&plugin, &rx);
}

/// A plugin that subscribes while a look of the watch is under way hears none of that look's
/// changes before its own first lists.
#[test]
fn a_look_under_way_does_not_reach_a_new_subscriber_first() {
    require_python();
    let env = Env::new("ext-look-race");
    let _d = env.start_with(&[
        ("THURM_PORTS_INTERVAL_MS", Path::new("200")),
        ("THURM_PORTS_LOOK_DELAY_MS", Path::new("1500")),
    ]);
    let (early, early_rx) = ext_client(&env, "early");
    call(
        &early,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    // The early subscriber's own first look is done.
    std::thread::sleep(Duration::from_millis(2000));
    // A new pane starts a look (it pokes the watch), whose scan comes once its listener runs;
    // the late plugin subscribes while that look is under way.
    let pane = listener_pane(&early, &env);
    std::thread::sleep(Duration::from_millis(100));
    let (late, late_rx) = ext_client(&env, "late");
    call(
        &late,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    let change = loop {
        let ev = next_event(&early_rx, "ports_changed");
        if ev["id"] == pane {
            break ev;
        }
    };
    assert!(change.get("initial").is_none(), "{change}");
    let first = loop {
        let ev = next_event(&late_rx, "ports_changed");
        if ev["id"] == pane {
            break ev;
        }
    };
    assert_eq!(
        first["initial"], true,
        "a change came before the lists: {first}"
    );
}

/// A remote pane whose read missed some processes and found nothing is kept: a plugin that
/// subscribes later hears it, as running already and not complete, before the next list.
#[test]
fn a_late_subscriber_hears_an_incomplete_empty_remote_list() {
    let env = Env::new("ext-remote-incomplete");
    let _d = env.start();
    let (app, _app_rx) = forwarding_app(&env, &[CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS]);
    call(
        &app,
        method::PORTS_REPORT,
        json!({ "host": "devbox", "pane": 1, "complete": false, "ports": [] }),
    )
    .unwrap();
    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    let first = next_event(&rx, "ports_changed");
    assert_eq!(first["initial"], true, "{first}");
    assert_eq!(first["complete"], false, "{first}");
    assert_eq!(first["ports"], json!([]), "{first}");
    // A complete empty list forgets it.
    call(
        &app,
        method::PORTS_REPORT,
        json!({ "host": "devbox", "pane": 1, "complete": true, "ports": [] }),
    )
    .unwrap();
    next_event(&rx, "ports_changed");
    let (later, later_rx) = ext_client(&env, "later");
    call(
        &later,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    no_more_ports(&later, &later_rx);
}

/// When the app that holds plugins' lists quits, the next one's lists go out as they came to it:
/// running already only when its host said so, a change otherwise, and nothing for a pane it
/// lists as plugins last heard it.
#[test]
fn the_next_apps_lists_keep_what_they_were() {
    let env = Env::new("ext-remote-takeover");
    let _d = env.start();
    let caps = [CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS];
    let (first, _first_rx) = forwarding_app(&env, &caps);
    let (second, _second_rx) = forwarding_app(&env, &caps);
    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    let vite =
        |port: u16| json!([{ "port": port, "address": "127.0.0.1", "pid": 9, "command": "vite" }]);
    let report = |app: &Client, pane: u64, ports: Value, initial: bool| {
        call(
            app,
            method::PORTS_REPORT,
            json!({ "host": "devbox", "pane": pane, "ports": ports, "complete": true, "initial": initial }),
        )
        .unwrap()
    };
    // Plugins hear the first app's, and the answer says so.
    assert_eq!(
        report(&first, 3, vite(9000), true),
        json!({ "heard": true })
    );
    assert_eq!(
        report(&first, 4, vite(7000), false),
        json!({ "heard": true })
    );
    next_event(&rx, "ports_changed");
    next_event(&rx, "ports_changed");
    // The second's: a server it heard start in pane 1, pane 2 running as it connected, pane 3 as
    // the first had it, pane 4 known to listen on nothing. Plugins hear none of it yet, and the
    // answer says so (the app sends it again as it came, should it connect to the daemon again).
    assert_eq!(
        report(&second, 1, vite(5173), false),
        json!({ "heard": false })
    );
    report(&second, 2, vite(8080), true);
    report(&second, 3, vite(9000), true);
    report(&second, 4, json!([]), false);
    // Pane 5: known to listen on nothing, never heard of by plugins: no news.
    report(&second, 5, json!([]), false);
    drop(first);
    let mut heard: Vec<(u64, bool, Value, Value)> = (0..3)
        .map(|_| {
            let ev = next_event(&rx, "ports_changed");
            (
                ev["id"].as_u64().unwrap(),
                ev["initial"] == true,
                ev["complete"].clone(),
                ev["gone"].clone(),
            )
        })
        .collect();
    heard.sort_unstable_by_key(|h| h.0);
    let plain = |id: u64, initial: bool| (id, initial, json!(true), Value::Null);
    // Pane 4: an empty list the next app knows, not `gone`.
    assert_eq!(heard, [plain(1, false), plain(2, true), plain(4, false)]);
    no_more_ports(&plugin, &rx);
}

/// A pane asked for has a list even when its processes were not looked at: one whose program
/// ended listens on nothing, and says so as complete.
#[test]
fn ports_of_a_pane_not_looked_at_are_its_own() {
    let env = Env::new("ext-ports-dead");
    let _d = env.start();
    let (c, _rx) = ext_client(&env, "plugin");
    let pane = match c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "exit 0".into()]),
            cwd: Some(env.dir.display().to_string()),
            hold: true,
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while matches!(
        c.request(Request::PaneInfo { pane }),
        Ok(Response::PaneInfo(ref info)) if info.alive
    ) {
        assert!(Instant::now() < deadline, "the pane's program did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    let list = call(&c, method::PORTS_LIST, json!({ "pane": pane })).unwrap();
    assert_eq!(
        list,
        json!([{ "pane": pane, "ports": [], "complete": true }]),
        "{list}"
    );
}

/// A plugin that subscribes to `ports_changed`, leaves and subscribes again before the watch's
/// next look gets its first lists once, not once per subscription.
#[test]
fn subscribing_again_before_the_look_gets_the_lists_once() {
    require_python();
    let env = Env::new("ext-ports-resubscribe");
    let _d = env.start_with(&[
        ("THURM_PORTS_INTERVAL_MS", Path::new("200")),
        ("THURM_PORTS_LOOK_DELAY_MS", Path::new("800")),
    ]);
    let (c, rx) = ext_client(&env, "plugin");
    let pane = listener_pane(&c, &env);
    wait_match(&c, pane, r"port=\d+");
    let sub = |names: Value| call(&c, method::EVENTS_SUBSCRIBE, json!({ "names": names })).unwrap();
    sub(json!(["ports_changed"]));
    call(
        &c,
        method::EVENTS_UNSUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    sub(json!(["ports_changed"]));
    let first = loop {
        let ev = next_event(&rx, "ports_changed");
        if ev["id"] == pane {
            break ev;
        }
    };
    assert_eq!(first["initial"], true, "{first}");
    // Two looks later, nothing more of that pane.
    std::thread::sleep(Duration::from_millis(2200));
    call(
        &c,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["test.sentinel"] }),
    )
    .unwrap();
    call(&c, method::EVENTS_EMIT, json!({ "name": "test.sentinel" })).unwrap();
    loop {
        let ev = next(&rx, notice::EVENT);
        assert!(
            !(ev["event"] == "ports_changed" && ev["id"] == pane),
            "the first lists came twice: {ev}"
        );
        if ev["event"] == "test.sentinel" {
            break;
        }
    }
}

/// A remote pane plugins never heard listen on anything, listing nothing, is no news (as many
/// come as the app connects to a host); one whose servers go is, once.
#[test]
fn an_empty_remote_list_is_news_only_after_ports() {
    let env = Env::new("ext-remote-empty");
    let _d = env.start();
    let (app, _app_rx) = forwarding_app(&env, &[CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS]);
    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    let report = |ports: Value, initial: bool| {
        call(
            &app,
            method::PORTS_REPORT,
            json!({ "host": "devbox", "pane": 7, "ports": ports, "complete": true, "initial": initial }),
        )
        .unwrap()
    };
    let vite = json!([{ "port": 5173, "address": "127.0.0.1", "pid": 9, "command": "vite" }]);
    // As the app connects: an empty pane, said to nobody.
    assert_eq!(report(json!([]), true), json!({ "heard": true }));
    no_more_ports(&plugin, &rx);
    // A server starts, then goes: both news.
    report(vite.clone(), false);
    assert_eq!(next_event(&rx, "ports_changed")["ports"][0]["port"], 5173);
    report(json!([]), false);
    assert_eq!(next_event(&rx, "ports_changed")["ports"], json!([]));
    // Empty again: nothing new.
    report(json!([]), false);
    no_more_ports(&plugin, &rx);
}

/// A plugin that unsubscribes while its first look sends the lists, and subscribes again
/// before that look is done, still gets its first lists: the look that left it out queues it
/// again.
#[test]
fn a_subscriber_back_before_its_skipped_look_is_done_gets_its_lists() {
    let env = Env::new("ext-resubscribe-race");
    let _d = env.start_with(&[
        ("THURM_PORTS_INTERVAL_MS", Path::new("200")),
        ("THURM_PORTS_LOOK_DELAY_MS", Path::new("800")),
        ("THURM_EXT_SERVED_DELAY_MS", Path::new("1500")),
    ]);
    let (plugin, rx) = ext_client(&env, "plugin");
    let pane = match plugin
        .request(Request::CreatePane(CreatePane {
            command: Some(vec!["/bin/sh".into(), "-c".into(), "sleep 600".into()]),
            cwd: Some(env.dir.display().to_string()),
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    let ports = json!({ "names": ["ports_changed"] });
    call(&plugin, method::EVENTS_SUBSCRIBE, ports.clone()).unwrap();
    // Out before the look sends its lists, back in after it sent them and before it is done.
    std::thread::sleep(Duration::from_millis(300));
    call(&plugin, method::EVENTS_UNSUBSCRIBE, ports.clone()).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    call(&plugin, method::EVENTS_SUBSCRIBE, ports).unwrap();
    let first = loop {
        let ev = next_event(&rx, "ports_changed");
        if ev["id"] == pane {
            break ev;
        }
    };
    assert_eq!(first["initial"], true, "{first}");
}

/// The first app quits while the next one reports a change: plugins hear the next app's
/// forwards as theirs before that change, so the last event matches what is listed.
#[test]
fn forward_events_keep_the_order_of_their_changes() {
    let env = Env::new("ext-forwards-order");
    let _d = env.start_with(&[("THURM_EXT_FORWARD_EVENT_DELAY_MS", Path::new("800"))]);
    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["forward_opened", "forward_closed"] }),
    )
    .unwrap();
    let caps = [CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS];
    let (first, _first_rx) = forwarding_app(&env, &caps);
    let (second, _second_rx) = forwarding_app(&env, &caps);
    let forward = json!({
        "id": 7, "host": "devbox", "remote_port": 5173, "local_port": 5174,
        "url": "http://localhost:5174/",
    });
    call(
        &second,
        method::FORWARDS_REPORT,
        json!({ "forwards": [forward] }),
    )
    .unwrap();
    // The first quits: telling of the second's forward takes a while; the second closes it
    // meanwhile.
    drop(first);
    std::thread::sleep(Duration::from_millis(300));
    call(&second, method::FORWARDS_REPORT, json!({ "forwards": [] })).unwrap();
    let opened = next(&rx, notice::EVENT);
    assert_eq!(opened["event"], "forward_opened", "{opened}");
    let closed = next(&rx, notice::EVENT);
    assert_eq!(closed["event"], "forward_closed", "{closed}");
    assert_eq!(
        call(&plugin, method::FORWARDS_LIST, Value::Null).unwrap(),
        json!([])
    );
}

/// A report of the next app while the first app's handover is under way is heard once: as the
/// holder's, after the handover, which does not tell it again.
#[test]
fn a_report_during_a_handover_is_heard_once() {
    let env = Env::new("ext-handover-report");
    let _d = env.start_with(&[("THURM_EXT_HANDOVER_DELAY_MS", Path::new("800"))]);
    let caps = [CAP_EXT, CAP_EXT_UI, CAP_EXT_FORWARDS];
    let (first, _first_rx) = forwarding_app(&env, &caps);
    let (second, _second_rx) = forwarding_app(&env, &caps);
    let (plugin, rx) = ext_client(&env, "plugin");
    call(
        &plugin,
        method::EVENTS_SUBSCRIBE,
        json!({ "names": ["ports_changed"] }),
    )
    .unwrap();
    // The first quits: its handover takes a while; a server starts in the second's pane meanwhile.
    drop(first);
    std::thread::sleep(Duration::from_millis(300));
    let vite = json!([{ "port": 5173, "address": "127.0.0.1", "pid": 9, "command": "vite" }]);
    assert_eq!(
        call(
            &second,
            method::PORTS_REPORT,
            json!({ "host": "devbox", "pane": 1, "ports": vite, "complete": true, "initial": false }),
        )
        .unwrap(),
        json!({ "heard": true })
    );
    let ev = next_event(&rx, "ports_changed");
    assert_eq!(ev["id"], 1, "{ev}");
    assert_ne!(ev["initial"], true, "{ev}");
    // The handover is done by now, and told nothing more.
    std::thread::sleep(Duration::from_millis(800));
    no_more_ports(&plugin, &rx);
}
