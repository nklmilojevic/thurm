use std::sync::Mutex;

use super::*;

/// A daemon stand-in: answers `exec` after a pause, records the rest.
#[derive(Default)]
struct FakeHost {
    calls: Mutex<Vec<(String, Value)>>,
    events: Mutex<Option<Sender<(String, Value)>>>,
}

impl Host for FakeHost {
    fn call(&self, _instance: u64, method: &str, params: Value) -> Result<Value, String> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_owned(), params.clone()));
        if let Some(tx) = &*self.events.lock().unwrap() {
            let _ = tx.send((method.to_owned(), params.clone()));
        }
        match method {
            "exec" => {
                std::thread::sleep(Duration::from_millis(200));
                Ok(
                    json!({ "ok": true, "stdout": format!("ran {}", params["argv"][0].as_str().unwrap_or_default()) }),
                )
            }
            "fail" => Err("it failed".into()),
            "actions.register" if params.to_string().contains("refuse") => Err("refused".into()),
            "events.subscribe" if params.to_string().contains("bad.") => {
                Err("unknown event".into())
            }
            "request" if params["request"] == "Nulls" => {
                Ok(json!({ "gone": null, "list": [1, null, { "gone": null, "deep": [null, 2] }] }))
            }
            "request" => Ok(json!({ "Panes": [{ "id": 7 }] })),
            _ => Ok(Value::Null),
        }
    }

    fn report(&self, _instance: u64, error: &str) {
        if let Some(tx) = &*self.events.lock().unwrap() {
            let _ = tx.send(("report".into(), Value::String(error.into())));
        }
    }
}

struct Fixture {
    host: Arc<FakeHost>,
    rt: Runtime,
    rx: Receiver<(String, Value)>,
}

fn fixture() -> Fixture {
    let host = Arc::new(FakeHost::default());
    let (tx, rx) = unbounded();
    *host.events.lock().unwrap() = Some(tx);
    let rt = Runtime::start(host.clone()).unwrap();
    Fixture { host, rt, rx }
}

fn spec(instance: u64, source: &str, script: bool) -> Spec {
    Spec {
        instance,
        name: "test".into(),
        source: source.into(),
        file: "/plugins/test/init.lua".into(),
        dir: None,
        modules: [(
            "lib/util.lua".to_owned(),
            "return { twice = function(x) return 2 * x end }".to_owned(),
        )]
        .into_iter()
        .collect(),
        settings: json!({ "greeting": "hi" }),
        args: vec!["a1".into()],
        pane: Some(3),
        log: None,
        script,
    }
}

fn run(f: &Fixture, source: &str) -> Result<Value, String> {
    f.rt.load(spec(99, source, true))
        .recv_timeout(Duration::from_secs(20))
        .expect("script did not end")
}

/// The next call of `method` the host saw, skipping others.
fn next(f: &Fixture, method: &str) -> Value {
    loop {
        let (m, p) =
            f.rx.recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|_| panic!("no {method}"));
        if m == method {
            return p;
        }
    }
}

#[test]
fn a_script_sees_its_args_settings_and_returns_a_value() {
    let f = fixture();
    let r = run(
        &f,
        r#"
        thurm.log("hello", 1)
        return { args = thurm.args, greeting = thurm.settings.greeting, pane = thurm.pane,
                 panes = thurm.panes()[1].id, json = thurm.json.decode('{"a":[1,2]}').a[2] }
        "#,
    )
    .unwrap();
    assert_eq!(
        r,
        json!({ "args": ["a1"], "greeting": "hi", "pane": 3, "panes": 7, "json": 2 })
    );
    let log = next(&f, "log");
    assert_eq!(log, json!({ "level": "info", "message": "hello 1" }));
}

#[test]
fn errors_say_where_and_calls_raise() {
    let f = fixture();
    let e = run(&f, "\n\nlocal x = nil\nreturn x.y").unwrap_err();
    assert!(e.contains("init.lua:4:"), "{e}");
    let e = run(&f, "thurm.call('fail')").unwrap_err();
    assert!(e.contains("it failed") && e.contains("init.lua:1:"), "{e}");
    let e = run(&f, "io.popen('ls')").unwrap_err();
    assert!(e.contains("thurm.exec"), "{e}");
    let e = run(&f, "return (").unwrap_err();
    assert!(e.contains("init.lua"), "{e}");
}

#[test]
fn a_runaway_loop_is_stopped() {
    let f = fixture();
    let at = Instant::now();
    let e = run(&f, "while true do end").unwrap_err();
    assert!(e.contains("endless loop"), "{e}");
    assert!(at.elapsed() < Duration::from_secs(15));
}

#[test]
fn actions_run_in_coroutines_that_wait_without_blocking_others() {
    let f = fixture();
    let done = f.rt.load(spec(
        1,
        r#"
        thurm.action{ name = "test.slow", title = "Slow", run = function(ctx, args)
            local r = thurm.exec({ "slowprog" })
            return { out = r.stdout, pane = ctx.pane, n = args.n }
        end }
        thurm.action{ name = "test.fast", run = function() return "fast" end }
        thurm.action{ name = "test.broken", run = function() error("broke") end }
        thurm.action{ name = "test.nested", run = function() return thurm.run("x.y") end }
        "#,
        false,
    ));
    assert_eq!(
        done.recv_timeout(Duration::from_secs(20)).unwrap(),
        Ok(Value::Null)
    );
    let reg = next(&f, "actions.register");
    assert_eq!(reg["actions"][0]["name"], "test.slow");
    assert_eq!(reg["actions"][0]["title"], "Slow");

    f.rt.notice(
        1,
        notice::ACTION_INVOKE,
        json!({ "id": 10, "name": "test.slow", "args": { "n": 5 }, "ctx": { "pane": 4, "depth": 1 } }),
    );
    f.rt.notice(
        1,
        notice::ACTION_INVOKE,
        json!({ "id": 11, "name": "test.fast", "args": null, "ctx": {} }),
    );
    // The fast one answers while the slow one waits on its program.
    assert_eq!(
        next(&f, "actions.result"),
        json!({ "id": 11, "result": "fast" })
    );
    assert_eq!(
        next(&f, "actions.result"),
        json!({ "id": 10, "result": { "out": "ran slowprog", "pane": 4, "n": 5 } })
    );
    f.rt.notice(
        1,
        notice::ACTION_INVOKE,
        json!({ "id": 12, "name": "test.broken", "ctx": {} }),
    );
    let broken = next(&f, "actions.result");
    assert!(
        broken["error"].as_str().unwrap().contains("init.lua:7:"),
        "{broken}"
    );
    // An action run from an action says which invocation it comes from, and how deep it is.
    f.rt.notice(
        1,
        notice::ACTION_INVOKE,
        json!({ "id": 13, "name": "test.nested", "ctx": { "depth": 2 } }),
    );
    let run = next(&f, "actions.run");
    assert_eq!((&run["invocation"], &run["depth"]), (&json!(13), &json!(2)));
}

#[test]
fn handlers_timers_and_sleep() {
    let f = fixture();
    let done = f.rt.load(spec(
        2,
        r#"
        thurm.on("bell", function(ev) thurm.log("bell in", ev.id) end)
        thurm.on("demo.ev", function(ev) error("bad handler") end)
        local n = 0
        local t
        t = thurm.every(0.05, function()
            n = n + 1
            if n == 3 then
                t.cancel()
                thurm.log("ticked", n)
                -- Cancelled, it ticks no more.
                thurm.after(0.2, function() thurm.log("after", n) end)
            end
        end)
        local cancelled = thurm.after(0.05, function() thurm.log("never") end)
        cancelled.cancel()
        local at = os.clock()
        thurm.sleep(0.1)
        thurm.log("slept")
        "#,
        false,
    ));
    let sub = next(&f, "events.subscribe");
    assert_eq!(sub, json!({ "names": ["bell"] }));
    assert_eq!(
        done.recv_timeout(Duration::from_secs(20)).unwrap(),
        Ok(Value::Null)
    );
    f.rt.notice(2, notice::EVENT, json!({ "event": "bell", "id": 5 }));
    f.rt.notice(2, notice::EVENT, json!({ "event": "demo.ev" }));
    let mut logs = Vec::new();
    let mut report = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while logs.len() < 4 || report.is_none() {
        assert!(Instant::now() < deadline, "{logs:?} {report:?}");
        let Ok((m, p)) = f.rx.recv_timeout(Duration::from_secs(1)) else {
            continue;
        };
        match m.as_str() {
            "log" => logs.push(p["message"].as_str().unwrap().to_owned()),
            "report" => report = Some(p.as_str().unwrap().to_owned()),
            _ => {}
        }
    }
    logs.sort();
    assert_eq!(logs, ["after 3", "bell in 5", "slept", "ticked 3"]);
    assert!(report.unwrap().contains("bad handler"));
}

#[test]
fn waiting_inside_an_own_coroutine_is_refused() {
    let f = fixture();
    let r = run(
        &f,
        r#"
        local co = coroutine.create(function() thurm.sleep(0.01) end)
        local ok, err = coroutine.resume(co)
        return err
        "#,
    )
    .unwrap();
    assert!(r.as_str().unwrap().contains("coroutine of your own"), "{r}");
}

#[test]
fn unloading_drops_a_waiting_plugin() {
    let f = fixture();
    let done = f.rt.load(spec(5, "thurm.sleep(30)", false));
    std::thread::sleep(Duration::from_millis(100));
    f.rt.unload(5);
    let r = done.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(r.unwrap_err().contains("stopped"));
    drop(f.host);
}

#[test]
fn modules_brought_along_are_required() {
    let f = fixture();
    assert_eq!(
        run(&f, "return require('lib.util').twice(21)").unwrap(),
        json!(42)
    );
    let e = run(&f, "require('nope')").unwrap_err();
    assert!(e.contains("nope"), "{e}");
}

/// A plugin that sleeps 0 or sets a timer for now, over and over, still lets the others run,
/// and can be stopped.
#[test]
fn timers_due_at_once_do_not_keep_others_out() {
    let f = fixture();
    let spinner =
        f.rt.load(spec(20, "while true do thurm.sleep(0) end", false));
    let rearm = f.rt.load(spec(
        21,
        "local function again() thurm.after(0, again) end again()",
        false,
    ));
    let done = f.rt.load(spec(
        22,
        "thurm.action{ name = 'test.ping', run = function() return 'pong' end }",
        false,
    ));
    assert_eq!(
        done.recv_timeout(Duration::from_secs(20)).unwrap(),
        Ok(Value::Null)
    );
    f.rt.notice(
        22,
        notice::ACTION_INVOKE,
        json!({ "id": 1, "name": "test.ping", "ctx": {} }),
    );
    assert_eq!(
        next(&f, "actions.result"),
        json!({ "id": 1, "result": "pong" })
    );
    f.rt.unload(20);
    let r = spinner.recv_timeout(Duration::from_secs(20)).unwrap();
    assert!(r.unwrap_err().contains("stopped"));
    f.rt.unload(21);
    drop(rearm);
}

#[test]
fn a_refused_subscription_is_tried_again() {
    let f = fixture();
    // The fake host refuses `events.subscribe` for "bad.*" events.
    run(
        &f,
        r#"
        local ok = pcall(thurm.on, "bad.event", function() end)
        assert(not ok)
        pcall(thurm.on, "bad.event", function() end)
        "#,
    )
    .unwrap();
    let subscribes = f
        .host
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "events.subscribe")
        .count();
    assert_eq!(subscribes, 2);
}

#[test]
fn cancelled_timers_leave_the_heap() {
    let f = fixture();
    run(
        &f,
        r#"
        for _ = 1, 2000 do thurm.after(3600, function() end).cancel() end
        thurm.sleep(0)
        "#,
    )
    .unwrap();
    // Asked from the runtime's own thread: how many entries its heap keeps.
    let (tx, rx) = bounded(1);
    f.rt.tx.send(Job::Inspect(tx)).unwrap();
    let (heap, live) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(live, 0);
    assert!(heap <= 64, "{heap} timers left in the heap");
}

/// Each waiting call takes a thread: a plugin may have only so many at once.
#[test]
fn waiting_calls_are_bounded() {
    let f = fixture();
    let r = run(
        &f,
        r#"
        local refused
        for i = 1, 100 do
            thurm.after(0, function()
                local ok, err = pcall(thurm.exec, { "slowprog" })
                if not ok then refused = refused or err end
            end)
        end
        thurm.sleep(1)
        return refused
        "#,
    )
    .unwrap();
    assert!(
        r.as_str().unwrap_or_default().contains("waiting already"),
        "{r}"
    );
}

#[test]
fn check_says_whether_code_compiles() {
    assert!(super::check("return 1", "/x/init.lua").is_ok());
    let e = super::check("return (", "/x/init.lua").unwrap_err();
    assert!(e.contains("init.lua"), "{e}");
}

#[test]
fn a_script_has_less_memory_than_a_plugin() {
    let f = fixture();
    let e = run(&f, "local t = {} for i = 1, 1e8 do t[i] = i end").unwrap_err();
    assert!(e.to_lowercase().contains("memory"), "{e}");
}

#[test]
fn json_nulls_round_trip() {
    let f = fixture();
    let r = run(
        &f,
        r#"
        local t = thurm.json.decode('[1,null,3]')
        return { n = #t, null = t[2] == thurm.json.null, back = thurm.json.encode(t) }
        "#,
    )
    .unwrap();
    assert_eq!(r, json!({ "n": 3, "null": true, "back": "[1,null,3]" }));
}

/// In the API's results a null field is missing, while nulls in arrays, at any depth, keep
/// their places.
#[test]
fn api_results_keep_array_nulls_in_place() {
    let f = fixture();
    let r = run(
        &f,
        r#"
        local r = thurm.request("Nulls")
        local inner = r.list[3]
        return { gone = r.gone == nil, n = #r.list, null = r.list[2] == thurm.json.null,
                 inner_gone = inner.gone == nil, deep_n = #inner.deep,
                 deep_null = inner.deep[1] == thurm.json.null, deep = inner.deep[2],
                 back = thurm.json.encode(r.list) }
        "#,
    )
    .unwrap();
    assert_eq!(
        r,
        json!({ "gone": true, "n": 3, "null": true, "inner_gone": true, "deep_n": 2,
                "deep_null": true, "deep": 2, "back": "[1,null,{\"deep\":[null,2]}]" })
    );
}

/// A plugin may have only so many timers set at once; cancelled and fired ones free their
/// places.
#[test]
fn timers_are_bounded() {
    let f = fixture();
    let r = run(
        &f,
        &format!(
            r#"
            local set = {{}}
            for i = 1, {MAX_TIMERS} do set[i] = thurm.after(3600, function() end) end
            local ok, err = pcall(thurm.after, 3600, function() end)
            set[1].cancel()
            local again = pcall(thurm.after, 3600, function() end)
            for _, t in ipairs(set) do t.cancel() end
            return {{ refused = not ok and err:find("too many timers") ~= nil, again = again }}
            "#
        ),
    )
    .unwrap();
    assert_eq!(r, json!({ "refused": true, "again": true }));
}

/// A name the daemon refuses leaves the action that was there with its handler.
#[test]
fn a_refused_action_keeps_the_handler_it_had() {
    let f = fixture();
    let done = f.rt.load(spec(
        30,
        r#"
        thurm.action{ name = "test.keep", run = function() return "old" end }
        local ok = pcall(thurm.call, "fail")
        -- The fake host refuses this registration.
        pcall(thurm.action, { name = "test.keep", title = "refuse", run = function() return "new" end })
        "#,
        false,
    ));
    assert_eq!(
        done.recv_timeout(Duration::from_secs(20)).unwrap(),
        Ok(Value::Null)
    );
    f.rt.notice(
        30,
        notice::ACTION_INVOKE,
        json!({ "id": 7, "name": "test.keep", "ctx": {} }),
    );
    assert_eq!(
        next(&f, "actions.result"),
        json!({ "id": 7, "result": "old" })
    );
}
