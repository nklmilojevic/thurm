//! Remote workspaces over a real ssh: `sshd` on localhost with a throwaway key and config,
//! a remote "home" in a temporary directory, and this build's `thurm`/`thurmd` installed
//! there the way the app installs them.
//!
//! Skipped (with a note) when no `sshd` is found.

use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use thurm_client::{Client, ConnectOptions};
use thurm_config::RemoteConfig;
use thurm_proto::{CaptureOpts, CreatePane, Envelope, Request, Response, ServerMessage, codec};
use thurm_remote::Ssh;
use thurm_remote::doctor::{self, State};
use thurm_remote::install::{self, Method};
use thurm_remote::tunnel::{Phase, Status, Supervisor};

struct Sshd {
    child: Child,
    port: u16,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find_sshd() -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("sshd"))
            .find(|p| p.is_file())
    });
    from_path
        .or_else(|| Some(PathBuf::from("/usr/sbin/sshd")).filter(|p| p.is_file()))
        .and_then(|p| std::fs::canonicalize(p).ok())
}

fn keygen(path: &Path) {
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .expect("ssh-keygen")
        .success();
    assert!(ok);
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct World {
    base: PathBuf,
    _sshd: Sshd,
    ssh: Ssh,
    bad_ssh: Ssh,
    remote_socket: String,
    bins: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        // Stop the daemon the tunnel started "remotely".
        let _ = self.ssh.run(
            "PATH=\"$THURM_HOME/.local/share/thurm/bin:$PATH\"; thurm daemon stop",
            &[],
            None,
            Duration::from_secs(20),
        );
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn setup() -> Option<World> {
    let Some(sshd) = find_sshd() else {
        eprintln!("note: no sshd found; skipping the loopback ssh tests");
        return None;
    };
    // Short: Unix socket paths are limited to ~104 bytes.
    let base = std::env::temp_dir().join(format!("thl{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for d in [
        "keys", "rhome", "rrun", "rconfig", "rstate", "local", "lconfig",
    ] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    std::fs::set_permissions(base.join("rrun"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let keys = base.join("keys");
    keygen(&keys.join("host"));
    keygen(&keys.join("user"));
    keygen(&keys.join("stranger"));
    std::fs::copy(keys.join("user.pub"), keys.join("authorized_keys")).unwrap();
    std::fs::write(
        base.join("rconfig/config.toml"),
        "[terminal]\nshell = [\"/bin/sh\"]\n[session]\npersist = false\n",
    )
    .unwrap();
    let port = free_port();
    let env = format!(
        "THURM_HOME={h} XDG_RUNTIME_DIR={r} THURM_CONFIG_DIR={c} THURM_STATE_DIR={s} SHELL=/bin/sh THURM_BASE_PATH=/usr/bin:/bin:/usr/sbin:/sbin",
        h = base.join("rhome").display(),
        r = base.join("rrun").display(),
        c = base.join("rconfig").display(),
        s = base.join("rstate").display(),
    );
    let config = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {k}/host\nAuthorizedKeysFile {k}/authorized_keys\n\
         PasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\n\
         StrictModes no\nPidFile {b}/sshd.pid\nAllowStreamLocalForwarding yes\nStreamLocalBindUnlink yes\n\
         SetEnv {env}\n",
        k = keys.display(),
        b = base.display(),
    );
    std::fs::write(base.join("sshd_config"), config).unwrap();
    let child = Command::new(&sshd)
        .arg("-D")
        .arg("-f")
        .arg(base.join("sshd_config"))
        .arg("-E")
        .arg(base.join("sshd.log"))
        .stdin(Stdio::null())
        .spawn()
        .expect("start sshd");
    let host_pub = std::fs::read_to_string(keys.join("host.pub")).unwrap();
    let mut pub_parts = host_pub.split_whitespace();
    std::fs::write(
        keys.join("known_hosts"),
        format!(
            "[127.0.0.1]:{port} {} {}\n",
            pub_parts.next().unwrap(),
            pub_parts.next().unwrap()
        ),
    )
    .unwrap();
    std::fs::write(base.join("ssh_config"), "").unwrap();
    let ssh_with = |key: &str| Ssh {
        target: "127.0.0.1".into(),
        config: base.join("ssh_config"),
        program: "ssh".into(),
        extra: vec![
            "-p".into(),
            port.to_string(),
            "-i".into(),
            keys.join(key).display().to_string(),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            "IdentityAgent=none".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", keys.join("known_hosts").display()),
            "-o".into(),
            "StrictHostKeyChecking=yes".into(),
        ],
    };
    let ssh = ssh_with("user");
    // Wait for sshd.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match ssh.run("true", &[], None, Duration::from_secs(5)) {
            Ok(_) => break,
            Err(e) if Instant::now() > deadline => panic!("sshd did not come up: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    // The tests share one process: the app side's directories.
    unsafe {
        std::env::set_var("THURM_REMOTE_DIR", base.join("local"));
    }
    let uid = unsafe { libc_getuid() };
    let remote_socket = base
        .join(format!("rrun/thurm-{uid}/thurmd.sock"))
        .display()
        .to_string();
    let bins = binaries();
    Some(World {
        _sshd: Sshd { child, port },
        bad_ssh: ssh_with("stranger"),
        ssh,
        base,
        remote_socket,
        bins,
    })
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// This build's `thurm` and `thurmd` (built on demand when only the CLI was).
fn binaries() -> PathBuf {
    let thurm = PathBuf::from(env!("CARGO_BIN_EXE_thurm"));
    let dir = thurm.parent().unwrap().to_path_buf();
    if !dir.join("thurmd").is_file() {
        let profile_release = dir.ends_with("release");
        let mut c = Command::new(env!("CARGO"));
        c.args(["build", "-p", "thurm-daemon"]);
        if profile_release {
            c.arg("--release");
        }
        // `<target dir>/<profile>`, or `<target dir>/<triple>/<profile>` under --target: build
        // into the same place (cargo marks the target dir's root with CACHEDIR.TAG).
        let up = dir.parent().unwrap();
        if up.join("CACHEDIR.TAG").is_file() {
            c.arg("--target-dir").arg(up);
        } else if let Some(root) = up.parent().filter(|r| r.join("CACHEDIR.TAG").is_file()) {
            c.arg("--target-dir").arg(root);
            c.arg("--target").arg(up.file_name().unwrap());
        }
        assert!(c.status().unwrap().success(), "building thurmd");
    }
    assert!(
        dir.join("thurmd").is_file(),
        "no thurmd next to {}",
        thurm.display()
    );
    dir
}

fn remote(name: &str, socket: Option<String>) -> RemoteConfig {
    RemoteConfig {
        name: name.into(),
        host: "127.0.0.1".into(),
        socket,
        enabled: true,
        clipboard_read: Default::default(),
    }
}

struct Watch {
    seen: Arc<Mutex<Vec<Status>>>,
}

impl Watch {
    fn new() -> (Watch, impl Fn(&Status) + Send + Sync + 'static) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        (Watch { seen }, move |s: &Status| {
            sink.lock().push(s.clone())
        })
    }

    fn wait(&self, sup: &Supervisor, phase: Phase, secs: u64) -> Status {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let s = sup.status();
            if s.phase == phase {
                return s;
            }
            assert!(
                Instant::now() < deadline,
                "waited for {phase:?}; now {:?}: {:?}\nhistory: {:?}",
                s.phase,
                s.message,
                self.seen.lock().iter().map(|s| s.phase).collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn connect(socket: &str) -> Arc<Client> {
    Client::connect(
        ConnectOptions {
            socket: socket.into(),
            spawn_daemon: None,
            client_name: "loopback",
            ui: false,
        },
        |_| {},
        || {},
    )
    .expect("connect through the tunnel")
}

fn wait_screen(c: &Client, pane: u64, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Response::Text(t)) = c.request(Request::Capture {
            pane,
            opts: CaptureOpts::default(),
        }) && t.contains(needle)
        {
            return;
        }
        assert!(Instant::now() < deadline, "{needle:?} never showed up");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A daemon of another protocol: refuses every Hello.
fn fake_old_daemon(path: &Path) -> std::thread::JoinHandle<()> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        for s in l.incoming().flatten().take(50) {
            let mut r = std::io::BufReader::new(s.try_clone().unwrap());
            let mut w = s;
            if let Ok(Some(env)) = codec::read_message::<_, Envelope>(&mut r) {
                let _ = codec::write_message(
                    &mut w,
                    &ServerMessage::Response {
                        id: env.id,
                        result: Err::<Response, _>(format!(
                            "protocol mismatch: daemon 12, client {}",
                            thurm_proto::PROTOCOL_VERSION
                        )),
                    },
                );
            }
        }
    })
}

fn cli(w: &World, args: &[&str]) -> std::process::Output {
    std::fs::write(
        w.base.join("lconfig/config.toml"),
        "[[remote]]\nname = \"loop\"\nhost = \"127.0.0.1\"\n",
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_thurm"))
        .args(args)
        .env("THURM_REMOTE_DIR", w.base.join("local"))
        .env("THURM_CONFIG_DIR", w.base.join("lconfig"))
        .env("THURM_SOCKET", w.base.join("local/nothing.sock"))
        .env_remove("THURM_PANE_ID")
        .output()
        .unwrap()
}

#[test]
fn remote_workspace_over_loopback_ssh() {
    let Some(w) = setup() else { return };

    // 1. Nothing installed: reported, never installed in the background.
    let (watch, listener) = Watch::new();
    let sup = Supervisor::start(remote("loop", None), w.ssh.clone(), true, listener);
    let s = watch.wait(&sup, Phase::NotInstalled, 30);
    assert!(s.message.unwrap().contains("not installed"));
    assert!(!w.base.join("rhome/.local/share/thurm/bin/thurm").exists());

    // 2. Install this build the way the app does (copying on a Mac, the release archive
    //    elsewhere), then connect.
    let plan = install::plan(&w.ssh, None, Some(&w.bins)).unwrap();
    assert!(!plan.installed);
    let method = if plan.methods.contains(&Method::Copy) {
        Method::Copy
    } else {
        let target = plan.host.artifact_target().expect("supported platform");
        let dir = w.base.join("artifacts");
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join(install::artifact_name(thurm_proto::BUILD, target));
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&w.bins)
            .args(["thurm", "thurmd"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        unsafe { std::env::set_var("THURM_ARTIFACT_DIR", &dir) };
        Method::Download
    };
    let info = install::install(&w.ssh, &plan.host, method, Some(&w.bins)).unwrap();
    assert_eq!(info.build, thurm_proto::BUILD);
    assert_eq!(info.socket, w.remote_socket);
    sup.kick(false);
    let s = watch.wait(&sup, Phase::Connected, 30);
    assert!(!s.upgrade_available);
    assert_eq!(s.remote_build.as_deref(), Some(thurm_proto::BUILD));
    let local = s.socket.clone();

    // 2b. The doctor: what is left, and the fixes Thurm runs over ssh.
    let bins = Some(w.bins.as_path());
    let r = doctor::run(&w.ssh, "loop", None, bins).unwrap();
    let state = |r: &doctor::Report, id: &str| r.check(id).map(|c| c.state);
    assert_eq!(state(&r, "thurm"), Some(State::Ok), "{:#?}", r.checks);
    assert_eq!(state(&r, "daemon"), Some(State::Ok));
    // No ~/.local/bin on the "host": `thurm` is not on PATH in plain ssh sessions.
    assert_eq!(state(&r, "path"), Some(State::Warn));
    assert_eq!(state(&r, "agent.claude"), Some(State::Warn));
    assert!(r.check("hooks.claude").is_none());
    doctor::fix(&w.ssh, None, bins, "path", false).unwrap();
    assert!(w.base.join("rhome/.local/bin/thurm").is_symlink());
    // An agent shows up (installed after Thurm): its hooks are missing, then fixed.
    let claude = w.base.join("rhome/.local/bin/claude");
    std::fs::write(&claude, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let r = doctor::run(&w.ssh, "loop", None, bins).unwrap();
    assert_eq!(state(&r, "path"), Some(State::Ok));
    assert_eq!(state(&r, "agent.claude"), Some(State::Ok));
    assert_eq!(state(&r, "hooks.claude"), Some(State::Fail));
    let login = if r.plan.host.os == "Darwin" {
        State::Skip
    } else {
        State::Warn
    };
    assert_eq!(state(&r, "login.claude"), Some(login));
    let out = doctor::fix(&w.ssh, None, bins, "hooks.claude", false).unwrap();
    assert!(out.contains("hooks installed"), "{out}");
    let settings = std::fs::read_to_string(w.base.join("rhome/.claude/settings.json")).unwrap();
    assert!(settings.contains("agent-hook claude"));
    let r = doctor::run(&w.ssh, "loop", None, bins).unwrap();
    assert_eq!(state(&r, "hooks.claude"), Some(State::Ok));
    let argv = w.ssh.interactive_argv("exec claude").unwrap();
    assert!(argv.contains(&"-t".to_owned()) && argv.last().unwrap().starts_with("sh -c "));

    // 3. Panes through the tunnel.
    let c = connect(&local);
    let pane = match c
        .request(Request::CreatePane(CreatePane::default()))
        .unwrap()
    {
        Response::PaneCreated { pane } => pane,
        other => panic!("{other:?}"),
    };
    c.request(Request::Input {
        pane,
        data: b"echo remote-$((6*7))\r".to_vec(),
    })
    .unwrap();
    wait_screen(&c, pane, "remote-42");

    // 4. `thurm --remote` routes through the app's tunnel.
    let out = cli(&w, &["--remote", "loop", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(&pane.to_string()));
    let out = cli(
        &w,
        &["--remote", "loop", "capture", "--pane", &pane.to_string()],
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("remote-42"));
    let out = cli(&w, &["--remote", "loop", "capture"]);
    assert!(
        !out.status.success(),
        "no implicit local pane with --remote"
    );

    // 5. The tunnel dies: reconnect with backoff; the remote pane is still there.
    let pid = s.tunnel_pid.expect("tunnel pid");
    unsafe {
        kill(pid as i32, 9);
    }
    watch.wait(&sup, Phase::Reconnecting, 10);
    let s = watch.wait(&sup, Phase::Connected, 30);
    assert_ne!(s.tunnel_pid, Some(pid));
    let c = connect(&local);
    wait_screen(&c, pane, "remote-42");

    // 6. In-place daemon upgrade over ssh keeps the pane.
    let plan = install::plan(&w.ssh, None, Some(&w.bins)).unwrap();
    assert!(plan.installed);
    let d = plan.daemon.expect("daemon state");
    assert!(d.running && d.hot_upgrade);
    let out = install::upgrade_daemon(&w.ssh, None, Some(&d), false).unwrap();
    assert!(out.contains("panes kept"), "{out}");
    let c = connect(&local);
    wait_screen(&c, pane, "remote-42");
    drop(c);
    // The app's connection dropped with the old daemon; its restart check finds the same
    // tunnel healthy and says "connected" again, which is what makes the app reconnect.
    let tunnel = sup.status().tunnel_pid;
    let before = watch.seen.lock().len();
    sup.kick(true);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let again = watch.seen.lock()[before..]
            .iter()
            .any(|s| s.phase == Phase::Connected && s.tunnel_pid == tunnel);
        if again {
            break;
        }
        assert!(Instant::now() < deadline, "no status after a restart check");
        std::thread::sleep(Duration::from_millis(50));
    }

    // 6b. The entry now names another socket: the connected tunnel is replaced, not kept
    //     because the old one still answers.
    let moved = w.base.join("rrun/moved.sock");
    let _moved = fake_old_daemon(&moved);
    sup.update(remote("loop", Some(moved.display().to_string())))
        .unwrap();
    let s = watch.wait(&sup, Phase::UpgradeNeeded, 30);
    assert!(
        s.message.as_deref().unwrap_or("").contains("protocol 12"),
        "{s:?}"
    );

    // 7. The app goes away: the CLI says the host is not connected, and fails.
    drop(sup);
    let out = cli(&w, &["--remote", "loop", "agents"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("loop is not connected in Thurm (state: stopped)"),
        "{err}"
    );

    // 8. A daemon speaking another protocol: "upgrade needed", with its version.
    let old = w.base.join("rrun/old.sock");
    let _fake = fake_old_daemon(&old);
    let (watch, listener) = Watch::new();
    let sup = Supervisor::start(
        remote("old", Some(old.display().to_string())),
        w.ssh.clone(),
        false,
        listener,
    );
    let s = watch.wait(&sup, Phase::UpgradeNeeded, 30);
    assert!(
        s.message.as_deref().unwrap_or("").contains("protocol 12"),
        "{s:?}"
    );
    drop(sup);

    // 9. Authentication that needs a person (BatchMode): needs attention, with ssh's words.
    let (watch, listener) = Watch::new();
    let sup = Supervisor::start(remote("stranger", None), w.bad_ssh.clone(), false, listener);
    let s = watch.wait(&sup, Phase::NeedsAttention, 30);
    let m = s.message.unwrap_or_default();
    assert!(m.contains("Permission denied"), "{m}");
    drop(sup);
    assert_ne!(w._sshd.port, 0);
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
