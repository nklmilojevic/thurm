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
            if n == 3 then t.cancel(); thurm.log("ticked", n) end
        end)
        local cancelled = thurm.after(0.05, function() thurm.log("never") end)
        cancelled.cancel()
        thurm.after(0.3, function() thurm.log("after", n) end)
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
