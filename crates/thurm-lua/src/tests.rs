use std::sync::Mutex;

use super::*;

/// A daemon stand-in: answers `exec` after a pause, records the rest.
#[derive(Default)]
struct FakeHost {
    calls: Mutex<Vec<(String, Value)>>,
    /// Forward opens to the hosts `flaky` and `flaky2` so far: the first of each fails.
    flaky: std::sync::atomic::AtomicUsize,
    flaky2: std::sync::atomic::AtomicUsize,
    flaky3: std::sync::atomic::AtomicUsize,
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
            // A link no program on this Mac opens.
            "exec" if params["argv"] == json!(["open", "http://localhost:4999/"]) => Ok(json!({
                "ok": false, "code": 1, "stdout": "", "stderr": "no application can open it\n",
            })),
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
            "ports.list" if params["pane"] == 3 => Ok(json!([{
                "pane": 3,
                "ports": [{ "port": 5173, "address": "127.0.0.1", "pid": 9, "command": "vite" }],
                "complete": false,
            }])),
            "ports.list" if params["pane"].is_u64() => Ok(json!([])),
            "ports.list" => Ok(json!([
                {
                    "pane": 3,
                    "ports": [{ "port": 4000, "address": "127.0.0.1", "pid": 9, "command": "node_modules/.bin/vite" }],
                    "complete": true,
                },
                // Read in part: a server it left out still runs.
                { "pane": 6, "ports": [], "complete": false },
            ])),
            // A host whose first forward fails, as when its tunnel is down a moment.
            "forwards.open"
                if params["host"] == "flaky"
                    && self.flaky.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 =>
            {
                Err("devbox is not connected".into())
            }
            "forwards.open"
                if params["host"] == "flaky3"
                    && self
                        .flaky3
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        == 0 =>
            {
                Err("devbox is not connected".into())
            }
            // Another whose first forward fails.
            "forwards.open"
                if params["host"] == "flaky2"
                    && self
                        .flaky2
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        == 0 =>
            {
                Err("devbox is not connected".into())
            }
            "forwards.open" => Ok(json!({
                "id": 4, "host": params["host"], "remote_port": params["port"],
                "local_port": 5174, "url": "http://localhost:5174/", "new": params["browse"],
            })),
            "forwards.close" => Ok(json!({ "id": params["id"] })),
            "forwards.list" => Ok(json!([{ "id": 4, "host": "devbox" }])),
            // Answered on the runtime's own thread: it is held up meanwhile.
            "ui.toast" if params["text"] == "stall" => {
                std::thread::sleep(Duration::from_millis(1500));
                Ok(Value::Null)
            }
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
fn thurm_fail_says_only_what_it_was_given() {
    let f = fixture();
    let e = run(&f, "\n\nthurm.fail('no such file: x.png')").unwrap_err();
    assert_eq!(e, "no such file: x.png");
    // Caught and raised again, it is still the message alone.
    let e = run(&f, "local ok, e = pcall(thurm.fail, 'nope')\nerror(e)").unwrap_err();
    assert_eq!(e, "nope");
    let e = run(&f, "thurm.fail(42)").unwrap_err();
    assert_eq!(e, "42");

    // From an action, after it waited on something.
    let done = f.rt.load(spec(
        1,
        r#"
        local function check(path)
            thurm.exec({ "slowprog" })
            thurm.fail("no such file: " .. path)
        end
        thurm.action{ name = "test.missing", run = function(ctx, args) check(args.path) end }
        "#,
        false,
    ));
    assert_eq!(
        done.recv_timeout(Duration::from_secs(20)).unwrap(),
        Ok(Value::Null)
    );
    f.rt.notice(
        1,
        notice::ACTION_INVOKE,
        json!({ "id": 20, "name": "test.missing", "args": { "path": "x.png" }, "ctx": {} }),
    );
    assert_eq!(
        next(&f, "actions.result"),
        json!({ "id": 20, "error": "no such file: x.png" })
    );
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

/// While the runtime is busy, notices past the limit wait in the sender rather than pile up.
#[test]
fn notices_back_up_in_the_sender() {
    let f = fixture();
    let busy = f.rt.load(spec(5, "thurm.ui.toast('stall')", true));
    std::thread::sleep(Duration::from_millis(100));
    std::thread::scope(|s| {
        let sender = s.spawn(|| {
            for _ in 0..MAX_NOTICES + 10 {
                f.rt.notice(77, notice::EVENT, json!({}));
            }
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(!sender.is_finished());
        assert_eq!(f.rt.notices.load(Ordering::Acquire), MAX_NOTICES);
        sender.join().unwrap();
    });
    busy.recv_timeout(Duration::from_secs(20)).unwrap().unwrap();
}

/// A FIFO (or device) is not opened: that could hold up every plugin. Regular files are.
#[test]
fn only_regular_files_are_opened() {
    let dir = std::env::temp_dir().join(format!("thurm-lua-fifo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("pipe.lua");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(dir.join("plain.lua"), "return 42").unwrap();
    std::fs::write(dir.join("run.lua"), "#!/usr/bin/env lua\nreturn 'ran'").unwrap();
    let f = fixture();
    let mut s = spec(
        8,
        &format!(
            r#"
            local dir = {dir:?}
            local f, why = io.open(dir .. "/pipe.lua")
            local lf, lwhy = loadfile(dir .. "/pipe.lua")
            local req_ok, req_err = pcall(require, "pipe")
            local plain = io.open(dir .. "/plain.lua"):read("a")
            local new = assert(io.open(dir .. "/new.txt", "w")) new:write("x") new:close()
            local more = assert(io.open(dir .. "/new.txt", "a")) more:write("y") more:close()
            local written = io.open(dir .. "/new.txt"):read("a")
            local count = 0
            for _ in io.lines(dir .. "/plain.lua") do count = count + 1 end
            local missing, mwhy = io.open(dir .. "/nope.txt")
            return {{ f = f == nil, why = why, lf = lf == nil, lwhy = lwhy,
                      req = not req_ok and tostring(req_err):find("not a regular file") ~= nil,
                      plain = plain, mod = require("plain"),
                      stdin = not pcall(io.read), lines = not pcall(io.lines),
                      written = written, count = count, missing = missing == nil,
                      mwhy = mwhy:find("No such file") ~= nil, shebang = dofile(dir .. "/run.lua") }}
            "#,
            dir = dir.display().to_string()
        ),
        true,
    );
    s.dir = Some(dir.clone());
    let r =
        f.rt.load(s)
            .recv_timeout(Duration::from_secs(20))
            .expect("script did not end")
            .unwrap();
    let not_regular = format!("{}: not a regular file", fifo.display());
    assert_eq!(
        r,
        json!({ "f": true, "why": not_regular, "lf": true, "lwhy": not_regular, "req": true,
                "plain": "return 42", "mod": 42, "stdin": true, "lines": true,
                "written": "xy", "count": 1, "missing": true, "mwhy": true, "shebang": "ran" })
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ports_and_forwards() {
    let f = fixture();
    let r = run(
        &f,
        r#"
        local ports, complete = thurm.ports(3)
        local none, none_complete = thurm.ports(8)
        local fw = thurm.forward.open("devbox", 5173, { browse = true })
        local plain = thurm.forward.open("devbox", 8080)
        return {
            port = ports[1].port, command = ports[1].command, complete = complete,
            none = #none, none_complete = none_complete, all = thurm.ports()[1].pane,
            url = fw.url, new = fw.new, plain_new = plain.new,
            closed = thurm.forward.close(fw.id).id, listed = thurm.forward.list()[1].host,
        }
        "#,
    )
    .unwrap();
    assert_eq!(
        r,
        json!({
            "port": 5173, "command": "vite", "complete": false, "none": 0, "none_complete": false,
            "all": 3, "url": "http://localhost:5174/", "new": true, "plain_new": false,
            "closed": 4, "listed": "devbox",
        })
    );
    assert_eq!(
        next(&f, "forwards.open"),
        json!({ "host": "devbox", "port": 5173, "browse": true })
    );
    let e = run(&f, "thurm.forward.open('devbox', '5173')").unwrap_err();
    assert!(e.contains("usage: thurm.forward.open"), "{e}");
    let e = run(&f, "thurm.forward.open('devbox', 5173, true)").unwrap_err();
    assert!(e.contains("usage: thurm.forward.open"), "{e}");
    // A pane that is not one is refused, not taken for every pane.
    for bad in ["-1", "'3'", "1.5"] {
        let e = run(&f, &format!("return thurm.ports({bad})")).unwrap_err();
        assert!(e.contains("usage: thurm.ports"), "{bad}: {e}");
    }
}

/// The example dev-server plugin opens servers that start, never ones that were running: this
/// Mac's when it loads, nor a remote host's listed as `initial` when the app connects to it,
/// however late that is.
#[test]
fn the_dev_server_example_opens_only_new_servers() {
    let f = fixture();
    let mut s = spec(
        5,
        include_str!("../../../examples/plugins/dev-server/init.lua"),
        false,
    );
    s.settings = json!({ "retry_seconds": 0.2 });
    let done = f.rt.load(s);
    assert_eq!(
        done.recv_timeout(Duration::from_secs(20)).unwrap(),
        Ok(Value::Null)
    );
    let vite = |port: u16| json!({ "port": port, "address": "127.0.0.1", "pid": 9, "command": "node_modules/.bin/vite" });
    let event = |payload: Value| f.rt.notice(5, notice::EVENT, payload);
    let ports = |host: Option<&str>, id: u64, list: &[u16], complete: bool, initial: bool| {
        let mut ev = json!({ "event": "ports_changed", "id": id, "complete": complete,
                             "ports": list.iter().map(|&p| vite(p)).collect::<Vec<_>>() });
        if let Some(h) = host {
            ev["host"] = h.into();
        }
        if initial {
            ev["initial"] = true.into();
        }
        event(ev);
    };
    // The lists the subscription brings: the server this Mac had at load, and one that
    // started between that read and the subscription, which is news.
    ports(None, 3, &[4000], true, true);
    ports(None, 5, &[4002], true, true);
    // A host the app connects to after the plugin has long been running.
    std::thread::sleep(Duration::from_millis(3500));
    ports(Some("devbox"), 1, &[5173], true, true);
    // Then servers start: one more on the host, one more here, one on a host connected before.
    ports(Some("devbox"), 1, &[5173, 5174], true, false);
    ports(None, 3, &[4000, 4001], true, false);
    ports(Some("other"), 2, &[3000], true, false);
    // Not a dev server.
    event(json!({ "event": "ports_changed", "id": 4, "complete": true,
                  "ports": [{ "port": 22, "address": "0.0.0.0", "pid": 1, "command": "sshd" }] }));
    // A read that missed some processes leaves out 4000, which still runs: it does not open
    // again when a complete list has it.
    ports(None, 3, &[4001], false, false);
    ports(None, 3, &[4000, 4001], true, false);
    // The pane read in part at load stays incomplete (a process this user cannot read): a
    // server that shows up meanwhile listens for sure, and opens.
    ports(None, 6, &[4300], false, false);
    // The pane read in part at load: its first complete list says what ran, opening nothing.
    ports(None, 6, &[4100], true, false);
    // The app lost devbox: its lists go; what it lists as the app connects again runs already.
    event(
        json!({ "event": "ports_changed", "host": "devbox", "id": 1, "ports": [],
                  "complete": false, "gone": true }),
    );
    ports(Some("devbox"), 1, &[5173, 5174], true, true);
    // A host the app connects to whose first read missed a server: its first complete list
    // has it, and it ran already.
    ports(Some("nas"), 7, &[], false, true);
    ports(Some("nas"), 7, &[8080], true, false);
    // A server listening on every address, as macOS says it: a dev server too.
    event(json!({ "event": "ports_changed", "id": 8, "complete": true,
                  "ports": [{ "port": 4200, "address": "*", "pid": 9, "command": "node_modules/.bin/vite" }] }));
    // A forward that fails once (the host's tunnel down a moment) is tried again while listed.
    ports(Some("flaky"), 9, &[7000], true, false);
    // A forward that fails, then its server stops and starts again before the try again: the
    // restart opens it, and the earlier try does not open it a second time.
    ports(Some("flaky2"), 10, &[7100], true, false);
    ports(Some("flaky2"), 10, &[], true, false);
    ports(Some("flaky2"), 10, &[7100], true, false);
    // A forward that fails, then the app loses the host and connects again, the server running
    // already: the earlier try does not open it.
    ports(Some("flaky3"), 11, &[7200], true, false);
    event(
        json!({ "event": "ports_changed", "host": "flaky3", "id": 11, "ports": [],
                  "complete": false, "gone": true }),
    );
    ports(Some("flaky3"), 11, &[7200], true, true);
    // 4000 really stops (a complete list) and starts again: news.
    ports(None, 3, &[4001], true, false);
    ports(None, 3, &[4000, 4001], true, false);
    event(json!({ "event": "bell", "id": 0 }));
    let opened = |f: &Fixture| -> Vec<Value> {
        f.host
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(m, p)| match m.as_str() {
                "forwards.open" => Some(json!([p["host"], p["port"], p["browse"]])),
                "exec" => Some(p["argv"].clone()),
                _ => None,
            })
            .collect()
    };
    // Each event's handler runs on its own and waits on its calls: opens of different servers
    // may reach the host in either order. What counts is which open, and how many times.
    let sorted = |mut v: Vec<Value>| {
        v.sort_by_key(|x| x.to_string());
        v
    };
    let want = vec![
        json!(["open", "http://localhost:4002/"]),
        json!(["devbox", 5174, true]),
        json!(["open", "http://localhost:4001/"]),
        json!(["other", 3000, true]),
        json!(["open", "http://localhost:4300/"]),
        json!(["open", "http://localhost:4200/"]),
        json!(["flaky", 7000, true]),
        json!(["flaky2", 7100, true]),
        json!(["flaky2", 7100, true]),
        json!(["flaky3", 7200, true]),
        json!(["open", "http://localhost:4000/"]),
        json!(["flaky", 7000, true]),
    ];
    let deadline = Instant::now() + Duration::from_secs(20);
    while opened(&f).len() < want.len() {
        assert!(Instant::now() < deadline, "{:?}", opened(&f));
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(sorted(opened(&f)), sorted(want));
}

/// A server the dev-server example cannot open in the browser is warned of, not taken as opened.
#[test]
fn the_dev_server_example_warns_of_a_server_it_cannot_open() {
    let f = fixture();
    assert_eq!(
        f.rt.load(spec(
            7,
            include_str!("../../../examples/plugins/dev-server/init.lua"),
            false,
        ))
        .recv_timeout(Duration::from_secs(20))
        .unwrap(),
        Ok(Value::Null)
    );
    f.rt.notice(
        7,
        notice::EVENT,
        json!({ "event": "ports_changed", "id": 9, "complete": true,
            "ports": [{ "port": 4999, "address": "127.0.0.1", "pid": 9, "command": "vite" }] }),
    );
    let warned = next(&f, "log");
    assert_eq!(warned["level"], "warn", "{warned}");
    let message = warned["message"].as_str().unwrap();
    assert!(
        message.contains("http://localhost:4999/") && message.contains("no application"),
        "{warned}"
    );
}

/// The dev-server example takes `commands` as a list, or one program as a string, and warns of
/// anything else rather than match nothing.
#[test]
fn the_dev_server_example_takes_one_command_as_a_string() {
    let f = fixture();
    let event = json!({ "event": "ports_changed", "id": 9, "complete": true,
        "ports": [{ "port": 4400, "address": "127.0.0.1", "pid": 9, "command": "astro dev" }] });
    let mut s = spec(
        7,
        include_str!("../../../examples/plugins/dev-server/init.lua"),
        false,
    );
    s.settings = json!({ "commands": "astro" });
    assert_eq!(
        f.rt.load(s).recv_timeout(Duration::from_secs(20)).unwrap(),
        Ok(Value::Null)
    );
    f.rt.notice(7, notice::EVENT, event.clone());
    assert_eq!(
        next(&f, "exec")["argv"],
        json!(["open", "http://localhost:4400/"])
    );

    let mut s = spec(
        8,
        include_str!("../../../examples/plugins/dev-server/init.lua"),
        false,
    );
    s.settings = json!({ "commands": 5 });
    assert_eq!(
        f.rt.load(s).recv_timeout(Duration::from_secs(20)).unwrap(),
        Ok(Value::Null)
    );
    let warned = next(&f, "log");
    assert_eq!(warned["level"], "warn", "{warned}");
    assert!(
        warned["message"].as_str().unwrap().contains("commands"),
        "{warned}"
    );
}
