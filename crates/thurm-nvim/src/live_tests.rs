//! `windows.lua` against a real Neovim (skipped when `nvim` isn't on PATH).

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use rmpv::Value;

use crate::grid::{Model, WinKind};
use crate::nvim::{self, Message, Nvim};

const WINDOWS_LUA: &str = include_str!("windows.lua");

struct Live {
    child: Child,
    nvim: Nvim,
    rx: Receiver<Message>,
    model: Model,
    notes: Vec<(String, Vec<Value>)>,
    _dir: TempDir,
}

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Live {
    fn start() -> Option<Live> {
        let dir = std::env::temp_dir().join(format!(
            "tn-live-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("n.sock");
        let child = Command::new("nvim")
            .args(["--clean", "--headless", "--listen"])
            .arg(&sock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            match UnixStream::connect(&sock) {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(e) => panic!("nvim socket: {e}"),
            }
        };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let (tx, rx) = unbounded();
        std::thread::spawn(move || {
            while let Ok(Some(m)) = nvim::read_message(&mut reader) {
                if tx.send(m).is_err() {
                    return;
                }
            }
        });
        Some(Live {
            child,
            nvim: Nvim::new(stream),
            rx,
            model: Model::default(),
            notes: Vec::new(),
            _dir: TempDir(dir),
        })
    }

    /// Sends a request and pumps messages until its answer.
    fn call(&mut self, method: &str, params: Vec<Value>) -> Result<Value, String> {
        let id = self.nvim.request(method, params).unwrap();
        self.nvim.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Message::Response { id: got, result }) if got == id => {
                    return result.map_err(|e| nvim::error_text(&e));
                }
                Ok(m) => self.take(m),
                Err(_) => {}
            }
        }
        panic!("no answer to {method}")
    }

    fn take(&mut self, m: Message) {
        match m {
            Message::Notification { method, params } if method == "redraw" => {
                self.model.apply_redraw(&params);
            }
            Message::Notification { method, params } => self.notes.push((method, params)),
            _ => {}
        }
    }

    /// Pumps messages for a while (Neovim's scheduled callbacks and redraws).
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            if let Ok(m) = self.rx.recv_timeout(Duration::from_millis(50)) {
                self.take(m);
            }
        }
    }

    fn lua(&mut self, code: &str, args: Vec<Value>) -> Result<Value, String> {
        self.call("nvim_exec_lua", vec![code.into(), Value::Array(args)])
    }

    fn kind(&self, win: i64) -> Option<WinKind> {
        self.model
            .wins
            .values()
            .find(|w| w.win == win)
            .map(|w| w.kind.clone())
    }

    fn note(&self, name: &str) -> Option<&Vec<Value>> {
        self.notes
            .iter()
            .rev()
            .find(|(m, _)| m == name)
            .map(|(_, p)| p)
    }
}

fn setup(live: &mut Live) -> (i64, i64) {
    let opts = Value::Map(
        ["ext_linegrid", "ext_multigrid", "rgb"]
            .iter()
            .map(|k| (Value::from(*k), Value::from(true)))
            .collect(),
    );
    live.call("nvim_ui_attach", vec![40.into(), 12.into(), opts])
        .unwrap();
    let info = live.call("nvim_get_api_info", vec![]).unwrap();
    let chan = info.as_array().unwrap()[0].as_i64().unwrap();
    let r = live
        .lua(WINDOWS_LUA, vec![chan.into(), 40.into(), 10.into()])
        .unwrap();
    let a = r.as_array().unwrap();
    let first = nvim::handle(&a[0]).unwrap();
    let anchor = nvim::handle(&a[1]).unwrap();
    live.settle();
    (first, anchor)
}

#[test]
fn windows_become_external_and_report_splits() {
    let Some(mut live) = Live::start() else {
        eprintln!("no nvim on PATH: skipped");
        return;
    };
    let (first, anchor) = setup(&mut live);
    assert_ne!(first, anchor);
    assert_eq!(live.kind(first), Some(WinKind::External));
    let grid = live.model.grid_of_win(first).unwrap();
    assert_eq!(
        (live.model.grids[&grid].cols, live.model.grids[&grid].rows),
        (40, 10)
    );
    assert_eq!(
        live.note("thurm_win").and_then(|p| nvim::handle(&p[0])),
        Some(first)
    );

    // :vsplit: a split note (side by side), and an ordinary window to adopt.
    live.call("nvim_command", vec!["vsplit".into()]).unwrap();
    live.settle();
    let split = live.note("thurm_split").expect("a split note").clone();
    let new = nvim::handle(&split[0]).unwrap();
    assert_eq!(split[1].as_bool(), Some(true));
    assert_eq!(nvim::handle(&split[3]), Some(first));
    assert_eq!(live.kind(new), Some(WinKind::Ordinary));
    live.lua(
        "thurm_nvim.adopt(...)",
        vec![new.into(), 20.into(), 10.into()],
    )
    .unwrap();
    live.settle();
    assert_eq!(live.kind(new), Some(WinKind::External));

    // Sizing.
    live.lua(
        "thurm_nvim.size(...)",
        vec![new.into(), 30.into(), 7.into()],
    )
    .unwrap();
    live.settle();
    let g = live.model.grid_of_win(new).unwrap();
    assert_eq!(
        (live.model.grids[&g].cols, live.model.grids[&g].rows),
        (30, 7)
    );

    // Statuslines: both windows, as cells.
    let st = live
        .lua(
            "return thurm_nvim.statuslines(...)",
            vec![
                Value::Array(vec![first.into(), new.into()]),
                Value::Array(vec![40.into(), 30.into()]),
            ],
        )
        .unwrap();
    let st = st.as_array().unwrap();
    let text: String = st[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_array().unwrap()[0].as_str().unwrap().to_owned())
        .collect();
    assert!(text.contains("0,0-1"), "{text:?}");
    assert_eq!(st[1].as_array().unwrap().len(), 30);

    // Closing one: its grid goes.
    live.lua("thurm_nvim.close(...)", vec![new.into()]).unwrap();
    live.settle();
    assert_eq!(live.kind(new), None);
}

#[test]
fn close_of_the_last_window_reopens_it_and_quit_quits() {
    let Some(mut live) = Live::start() else {
        eprintln!("no nvim on PATH: skipped");
        return;
    };
    let (first, _) = setup(&mut live);
    live.call("nvim_command", vec!["close".into()]).unwrap();
    live.settle();
    assert_eq!(
        live.note("thurm_reopen").and_then(|p| nvim::handle(&p[0])),
        Some(first)
    );
    let reopened: Vec<i64> = live
        .model
        .wins
        .values()
        .filter(|w| w.kind == WinKind::External)
        .map(|w| w.win)
        .collect();
    assert_eq!(reopened.len(), 1);
    assert_ne!(reopened[0], first);

    // :q in the last window quits Neovim.
    // (Input, not a command: Neovim exits rather than answer.)
    live.call("nvim_input", vec![":q<CR>".into()]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = live.child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("nvim didn't quit");
}
