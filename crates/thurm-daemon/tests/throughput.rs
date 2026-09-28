//! End-to-end throughput through a real daemon and PTY, with a subscriber that parses the
//! forwarded output like the app does.
//!
//!   cargo test --release -p thurm-daemon --test throughput -- --ignored --nocapture
//!
//! A zsh script in the pane `cat`s an 11 MB file (`THURM_BENCH_FILE`, default: generated
//! prose), then asks for the cursor position: the daemon answers only after parsing every
//! byte before the query, so "done" is when the daemon has caught up. Needs /bin/zsh.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use thurm_client::{Client, ConnectOptions};
use thurm_proto::{CreatePane, Event, PaneSize, Request, Response};
use thurm_term::{EngineConfig, Terminal};

const COLS: u16 = 79;
const ROWS: u16 = 33;

const SCRIPT: &str = r#"
zmodload zsh/datetime
out=$1; f=$2
stty -echo -icanon min 1 time 0
dsr() { printf '\e[6n'; IFS= read -rs -t 30 -d R _r }
sleep 0.5
lat=()
for i in {1..300}; do t0=$EPOCHREALTIME; dsr; t1=$EPOCHREALTIME; lat+=$(( (t1 - t0) * 1000000 )); sleep 0.002; done
lat=(${(on)lat})
printf 'latency_us %.0f %.0f\n' ${lat[150]} ${lat[297]} >> $out
for i in 1 2 3 4 5; do
  printf '\e[H\e[2J'; dsr
  t0=$EPOCHREALTIME; command cat $f; t1=$EPOCHREALTIME; dsr; t2=$EPOCHREALTIME
  printf 'cat %.1f done %.1f\n' $(( (t1 - t0) * 1000 )) $(( (t2 - t0) * 1000 )) >> $out
done
print -- end >> $out
sleep 30
"#;

fn corpus(dir: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("THURM_BENCH_FILE") {
        return p.into();
    }
    let words = [
        "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "to", "be", "or", "not",
        "that", "is", "question", "whether", "nobler", "mind", "suffer",
    ];
    let mut out = Vec::with_capacity(11_500_000);
    let mut i = 0usize;
    while out.len() < 11_000_000 {
        let len = 20 + (i * 7) % 60;
        let mut line = String::new();
        while line.len() < len {
            line.push_str(words[(i * 31 + line.len()) % words.len()]);
            line.push(' ');
        }
        out.extend_from_slice(line.trim_end().as_bytes());
        out.push(b'\n');
        i += 1;
    }
    let p = dir.join("corpus.txt");
    std::fs::write(&p, out).unwrap();
    p
}

struct Daemon(Child, PathBuf);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn run(subscribe: bool) -> (Vec<(f64, f64)>, String, String) {
    // Short path: unix socket paths are limited to ~104 bytes.
    let dir = PathBuf::from(format!("/tmp/thurm-tp-{}-{subscribe}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let script = dir.join("bench.zsh");
    std::fs::write(&script, SCRIPT).unwrap();
    let file = corpus(&dir);
    let out = dir.join("result.txt");
    std::fs::write(
        dir.join("config/config.toml"),
        format!(
            "[terminal]\nshell = [\"/bin/zsh\", {:?}, {:?}, {:?}]\nshell_integration = false\nscrollback = 10000\n",
            script.display().to_string(),
            out.display().to_string(),
            file.display().to_string(),
        ),
    )
    .unwrap();
    let socket = dir.join("d.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_thurmd"))
        .args(["--foreground", "--socket"])
        .arg(&socket)
        .env("THURM_CONFIG_DIR", dir.join("config"))
        .env("THURM_STATE_DIR", dir.join("state"))
        .spawn()
        .expect("spawn daemon");
    let daemon = Daemon(child, dir.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(Instant::now() < deadline, "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The subscriber: a local terminal fed with the forwarded output, like the app's copy.
    let (tx, rx) = unbounded::<Event>();
    let c: Arc<Client> = Client::connect(
        ConnectOptions {
            socket: socket.clone(),
            spawn_daemon: None,
            client_name: "throughput",
            ui: true,
        },
        move |ev| {
            let _ = tx.send(ev);
        },
        || {},
    )
    .expect("connect");
    let size = PaneSize {
        cols: COLS,
        rows: ROWS,
        cell_width: 8,
        cell_height: 16,
    };
    let consumer = std::thread::spawn(move || {
        let mut local = Terminal::new(size, EngineConfig::default());
        while let Ok(ev) = rx.recv() {
            match ev {
                Event::Output { data, .. } => local.advance(&data),
                Event::Attach { state, .. } => local.advance(&state),
                _ => {}
            }
        }
    });
    let pane = match c
        .request(Request::CreatePane(CreatePane {
            size,
            ..Default::default()
        }))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    if subscribe {
        c.request(Request::Subscribe { pane }).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    let text = loop {
        let t = std::fs::read_to_string(&out).unwrap_or_default();
        if t.contains("end") {
            break t;
        }
        assert!(Instant::now() < deadline, "benchmark did not finish: {t}");
        std::thread::sleep(Duration::from_millis(100));
    };
    // The daemon's memory with the pane's scrollback full, once the output has been handled.
    std::thread::sleep(Duration::from_millis(500));
    let memory = footprint(daemon.0.id());
    drop(c);
    let _ = consumer.join();
    let latency = text
        .lines()
        .find_map(|l| l.strip_prefix("latency_us "))
        .unwrap_or("?")
        .to_owned();
    let runs = text
        .lines()
        .filter(|l| l.starts_with("cat "))
        .filter_map(|l| {
            let v: Vec<f64> = l
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect();
            (v.len() == 2).then(|| (v[0], v[1]))
        })
        .collect();
    (runs, latency, memory)
}

/// Physical footprint as `vmmap` reports it ("39.8M"), or "?".
fn footprint(pid: u32) -> String {
    let out = Command::new("vmmap")
        .args(["--summary", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    out.lines()
        .find_map(|l| l.strip_prefix("Physical footprint:"))
        .map(|v| v.trim().to_owned())
        .unwrap_or_else(|| "?".into())
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[test]
#[ignore]
fn pty_throughput() {
    for subscribe in [false, true] {
        let (r, latency, memory) = run(subscribe);
        println!(
            "{}: cat {:.1} ms, done {:.1} ms, idle round trip p50/p99 {latency} µs, daemon {memory}  (runs: {:?})",
            if subscribe {
                "with subscriber"
            } else {
                "no subscriber  "
            },
            median(r.iter().map(|x| x.0).collect()),
            median(r.iter().map(|x| x.1).collect()),
            r.iter().map(|x| x.1.round()).collect::<Vec<_>>(),
        );
    }
}
