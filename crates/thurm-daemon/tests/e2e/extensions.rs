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
    // One that compiles but fails as it runs gives way to the one that worked.
    std::fs::write(
        &init,
        "thurm.action{ name = 'lua.half', run = function() end }\nerror('no')",
    )
    .unwrap();
    c.request(Request::ReloadConfig).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while action_names(&c) != ["lua.greet"] {
        assert!(Instant::now() < deadline, "{:?}", action_names(&c));
        std::thread::sleep(Duration::from_millis(50));
    }

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
