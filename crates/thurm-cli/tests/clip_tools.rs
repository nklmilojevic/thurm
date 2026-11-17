//! The clipboard tools as installed: links named `xclip` and `pbpaste` to this build's `thurm`,
//! run in a pane of a real `thurmd`, with this test answering as the Thurm app.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thurm_client::{Client, ConnectOptions};
use thurm_proto::{
    CreatePane, Event, PaneId, PaneSize, Request, Response, WaitCondition, WaitOutcome,
};

/// This build's `thurm` and `thurmd` (built on demand when only the CLI was), as
/// `remote_loopback.rs` finds them.
fn binaries() -> PathBuf {
    let thurm = PathBuf::from(env!("CARGO_BIN_EXE_thurm"));
    let dir = thurm.parent().unwrap().to_path_buf();
    if !dir.join("thurmd").is_file() {
        let mut c = Command::new(env!("CARGO"));
        c.args(["build", "-p", "thurm-daemon"]);
        if dir.ends_with("release") {
            c.arg("--release");
        }
        let up = dir.parent().unwrap();
        if up.join("CACHEDIR.TAG").is_file() {
            c.arg("--target-dir").arg(up);
        } else if let Some(root) = up.parent().filter(|r| r.join("CACHEDIR.TAG").is_file()) {
            c.arg("--target-dir").arg(root);
            c.arg("--target").arg(up.file_name().unwrap());
        }
        assert!(c.status().unwrap().success(), "building thurmd");
    }
    dir
}

struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn wait_for(c: &Client, pane: PaneId, regex: &str) {
    let r = c
        .request(Request::Wait {
            pane,
            until: WaitCondition::Match {
                regex: regex.into(),
            },
            timeout_ms: Some(10_000),
        })
        .unwrap();
    let screen = c
        .request(Request::Capture {
            pane,
            opts: Default::default(),
        })
        .map(|r| format!("{r:?}"))
        .unwrap_or_default();
    assert!(
        matches!(r, Response::Wait(WaitOutcome::Satisfied)),
        "waiting for {regex:?}: {r:?}\n{screen}"
    );
}

#[test]
fn installed_tools_copy_and_paste_through_the_app() {
    let bins = binaries();
    let dir = std::env::temp_dir().join(format!("thurm-clip-tools-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tools = dir.join("shims");
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    for t in ["xclip", "pbpaste"] {
        std::os::unix::fs::symlink(bins.join("thurm"), tools.join(t)).unwrap();
    }
    std::fs::write(
        dir.join("config/config.toml"),
        "[terminal]\nshell = [\"/bin/sh\"]\nosc52 = \"copy-paste\"\n",
    )
    .unwrap();
    let socket = dir.join("d.sock");
    let child = Command::new(bins.join("thurmd"))
        .args(["--foreground", "--socket"])
        .arg(&socket)
        .env("THURM_CONFIG_DIR", dir.join("config"))
        .env("THURM_STATE_DIR", dir.join("state"))
        .env("PS1", "$ ")
        .spawn()
        .unwrap();
    let _daemon = Daemon {
        child,
        dir: dir.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The Thurm app: takes the copy, answers the paste.
    let copied: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let app: Arc<Mutex<Option<Arc<Client>>>> = Arc::default();
    let (seen, handle) = (copied.clone(), app.clone());
    let client = Client::connect(
        ConnectOptions {
            socket: socket.clone(),
            spawn_daemon: None,
            client_name: "e2e-app",
            ui: true,
        },
        move |ev| {
            let Some(app) = handle.lock().unwrap().clone() else {
                return;
            };
            match ev {
                Event::ClipboardWrite { id, text, .. } => {
                    seen.lock().unwrap().push(text);
                    let _ = app.send(Request::ClipboardAnswer {
                        id,
                        data: Some(Vec::new()),
                    });
                }
                Event::ClipboardQuery { id, .. } => {
                    let _ = app.send(Request::ClipboardAnswer {
                        id,
                        data: Some(b"from the mac".to_vec()),
                    });
                }
                _ => {}
            }
        },
        || {},
    )
    .unwrap();
    *app.lock().unwrap() = Some(client.clone());

    let path = format!("{}:/usr/bin:/bin", tools.display());
    let Response::PaneCreated { pane } = client
        .request(Request::CreatePane(CreatePane {
            cwd: Some(dir.display().to_string()),
            env: vec![("PATH".into(), path)],
            size: PaneSize {
                cols: 200,
                ..Default::default()
            },
            ..Default::default()
        }))
        .unwrap()
    else {
        panic!("no pane");
    };
    client
        .request(Request::Focus {
            pane,
            focused: true,
        })
        .unwrap();
    wait_for(&client, pane, r"\$");
    let run = |line: &str| {
        client
            .request(Request::Input {
                pane,
                data: format!("{line}\r").into_bytes(),
            })
            .unwrap();
    };

    run("printf 'copied text' | xclip -selection clipboard; echo COPY-RC=$?");
    wait_for(&client, pane, "(?m)^COPY-RC=0$");
    assert_eq!(
        copied.lock().unwrap().as_slice(),
        [Some("copied text".to_string())]
    );

    run("pbpaste; echo; echo PASTE-RC=$?");
    wait_for(&client, pane, "(?m)^from the mac$");
    wait_for(&client, pane, "(?m)^PASTE-RC=0$");

    // A refused option fails the tool, and nothing reaches the app.
    run("echo x | xclip -selection primary; echo BAD-RC=$?");
    wait_for(&client, pane, "(?m)^BAD-RC=1$");
    assert_eq!(copied.lock().unwrap().len(), 1);
    assert!(Path::new(&tools.join("xclip")).is_symlink());
}
