//! One app, many daemons (remote workspaces, phase 0): two daemons on their own sockets,
//! driven through the FFI the app uses. Pane ids collide across daemons; events, frames and
//! input stay with their connection, and the layout the local daemon stores keeps the other
//! daemon's panes.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use thurm_ffi::*;

struct Daemon {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}

impl Daemon {
    fn start(name: &str) -> Daemon {
        let dir = std::env::temp_dir().join(format!("thmd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join("config/config.toml"),
            "[terminal]\nshell = [\"/bin/sh\"]\n[session]\npersist = false\n",
        )
        .unwrap();
        let socket = dir.join("d.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_thurmd"))
            .args(["--foreground", "--socket"])
            .arg(&socket)
            .env("THURM_CONFIG_DIR", dir.join("config"))
            .env("THURM_STATE_DIR", dir.join("state"))
            .env("SHELL", "/bin/sh")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::os::unix::net::UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Daemon { child, dir, socket }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// What the callbacks saw: (host, event json) and (host, pane) frames.
static EVENTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static FRAMES: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

unsafe extern "C" fn on_event(ctx: *mut c_void, json: *const c_char) {
    let host = unsafe { CStr::from_ptr(ctx as *const c_char) }
        .to_string_lossy()
        .into_owned();
    let json = unsafe { CStr::from_ptr(json) }
        .to_string_lossy()
        .into_owned();
    EVENTS.lock().unwrap().push((host, json));
}

unsafe extern "C" fn on_frame(ctx: *mut c_void, pane: u64) {
    let host = unsafe { CStr::from_ptr(ctx as *const c_char) }
        .to_string_lossy()
        .into_owned();
    FRAMES.lock().unwrap().push((host, pane));
}

fn connect(d: &Daemon, host: &'static CStr) -> *mut thurm_client {
    let sock = CString::new(d.socket.display().to_string()).unwrap();
    let mut err: *mut c_char = std::ptr::null_mut();
    let c = unsafe {
        thurm_connect_socket(
            sock.as_ptr(),
            std::ptr::null(),
            c"multi-daemon-test".as_ptr(),
            Some(on_event),
            Some(on_frame),
            host.as_ptr() as *mut c_void,
            &mut err,
        )
    };
    assert!(!c.is_null(), "connect failed");
    c
}

fn request(c: *mut thurm_client, json: &str) -> serde_json::Value {
    let j = CString::new(json).unwrap();
    let raw = unsafe { thurm_request(c, j.as_ptr()) };
    let text = unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    unsafe { thurm_string_free(raw) };
    serde_json::from_str(&text).unwrap()
}

fn create(c: *mut thurm_client) -> u64 {
    let v = request(
        c,
        r#"{"CreatePane":{"command":null,"cwd":"/tmp","env":[],"size":{"cols":40,"rows":10,"cell_width":8,"cell_height":16},"agent_preset":null,"inherit_cwd_from":null,"hold":false}}"#,
    );
    v["PaneCreated"]["pane"]
        .as_u64()
        .unwrap_or_else(|| panic!("{v}"))
}

/// The text on screen in the connection's copy of `pane`.
fn screen(c: *mut thurm_client, pane: u64) -> String {
    let mut info = thurm_grid_info::default();
    let mut cells: *const thurm_cell = std::ptr::null();
    if !unsafe { thurm_grid_lock(c, pane, &mut info, &mut cells) } {
        return String::new();
    }
    let n = info.cols as usize * info.rows as usize;
    let text: String = unsafe { std::slice::from_raw_parts(cells, n) }
        .iter()
        .map(|cell| {
            char::from_u32(cell.ch)
                .filter(|c| *c != '\0')
                .unwrap_or(' ')
        })
        .collect();
    unsafe { thurm_grid_unlock(c, pane) };
    text
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn two_daemons_one_app() {
    let local = Daemon::start("local");
    let devbox = Daemon::start("devbox");
    let a = connect(&local, c"local");
    let b = connect(&devbox, c"devbox");

    // Pane ids collide: each daemon numbers its own.
    let pa = create(a);
    let pb = create(b);
    assert_eq!(pa, pb, "both daemons start at the same id");

    unsafe {
        thurm_subscribe(a, pa);
        thurm_subscribe(b, pb);
    }
    wait_for("frames from both", || {
        let f = FRAMES.lock().unwrap();
        f.contains(&("local".into(), pa)) && f.contains(&("devbox".into(), pb))
    });

    // Input goes to the connection it was sent on, output comes back on it.
    let line = b"echo from-$((40+2))-devbox\r";
    unsafe { thurm_input(b, pb, line.as_ptr(), line.len()) };
    wait_for("devbox output", || screen(b, pb).contains("from-42-devbox"));
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !screen(a, pa).contains("from-42-devbox"),
        "the local pane with the same id saw nothing"
    );

    // Events carry their connection: closing the devbox pane is not a local event.
    EVENTS.lock().unwrap().clear();
    request(b, &format!(r#"{{"ClosePane":{{"pane":{pb}}}}}"#));
    wait_for("PaneClosed from devbox", || {
        EVENTS
            .lock()
            .unwrap()
            .iter()
            .any(|(h, e)| h == "devbox" && e.contains("PaneClosed"))
    });
    assert!(
        !EVENTS
            .lock()
            .unwrap()
            .iter()
            .any(|(h, e)| h == "local" && e.contains("PaneClosed")),
        "the local pane {pa} is still open"
    );
    let panes = request(a, r#""ListPanes""#);
    assert_eq!(panes["Panes"].as_array().unwrap().len(), 1);

    // The layout lives in the local daemon, remote leaves included (they survive its
    // pruning of panes it doesn't have); an unknown local pane is dropped as before.
    let remote_pane = 99;
    let layout = serde_json::json!({
        "windows": [{
            "frame": null, "selected_tab": 0, "fullscreen": false, "workspace": 1,
            "tabs": [
                {"title": null, "root": {"type": "pane", "id": pa}, "focused": pa},
                {"title": null, "root": {"type": "split", "dir": "right", "ratio": 0.5,
                    "first": {"type": "pane", "id": remote_pane, "host": "devbox"},
                    "second": {"type": "pane", "id": pa, "host": "devbox"}},
                 "focused": remote_pane, "handoff": "devbox/repo-1/quiet-otter"},
                {"title": null, "root": {"type": "pane", "id": 12345}, "focused": 12345}
            ]
        }],
        "workspaces": [{"id": 1, "name": "devbox"}]
    });
    let set = serde_json::json!({"SetLayout": {"json": layout.to_string()}});
    assert_eq!(request(a, &set.to_string()), serde_json::json!("Ok"));
    let got = request(a, r#""GetLayout""#);
    let restored: serde_json::Value =
        serde_json::from_str(got["Layout"].as_str().unwrap()).unwrap();
    let tabs = restored["windows"][0]["tabs"].as_array().unwrap();
    assert_eq!(
        tabs.len(),
        2,
        "the tab of a vanished local pane is gone: {restored}"
    );
    assert_eq!(
        tabs[0]["root"],
        serde_json::json!({"type": "pane", "id": pa})
    );
    assert_eq!(tabs[1]["root"]["first"]["host"], "devbox");
    assert_eq!(tabs[1]["root"]["second"]["id"], pa);
    assert_eq!(tabs[1]["handoff"], "devbox/repo-1/quiet-otter");

    unsafe {
        thurm_disconnect(a);
        thurm_disconnect(b);
    }
}
