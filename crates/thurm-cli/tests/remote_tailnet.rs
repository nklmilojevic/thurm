//! `thurm remote add --tailnet` and `remote list` against stand-ins for a tailnet host
//! (`thurm-remote`'s `tests/support/tailnet_fakes.rs`), with every path in a scratch directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "../../thurm-remote/tests/support/tailnet_fakes.rs"]
mod fakes;

struct World {
    base: PathBuf,
}

impl World {
    fn new(name: &str) -> World {
        // Short: Unix socket paths are limited to about 104 bytes.
        let base = std::env::temp_dir().join(format!("thrt{}{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for d in ["config", "state", "remote"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        // The dial socket sits beside the daemon's socket (nothing listens on that one).
        fakes::dial_socket(&base.join(thurm_config::TAILNET_DIAL_SOCKET));
        World { base }
    }

    fn thurm(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_thurm"))
            .args(args)
            .env("THURM_SOCKET", self.base.join("thurmd.sock"))
            .env("THURM_CONFIG_DIR", self.base.join("config"))
            .env("THURM_STATE_DIR", self.base.join("state"))
            .env("THURM_REMOTE_DIR", self.base.join("remote"))
            .output()
            .unwrap()
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.base.join("config/config.toml")).unwrap_or_default()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.base.join(name)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stand_in_host(w: &World) -> u16 {
    let daemon: &Path = &w.path("d.sock");
    fakes::daemon(daemon);
    let (port, stop) = fakes::gateway(daemon.to_owned(), None);
    std::mem::forget(stop);
    port
}

#[test]
fn add_over_the_tailnet_writes_host_and_address() {
    let w = World::new("a");
    let port = stand_in_host(&w);
    let out = w.thurm(&[
        "remote",
        "add",
        "bastion",
        "bastion",
        "--tailnet",
        &format!("ws://127.0.0.1:{port}"),
    ]);
    assert!(out.status.success(), "{}", text(&out));
    let cfg = thurm_config::Config::parse(&w.config()).unwrap();
    let r = cfg.remote("bastion").unwrap();
    assert_eq!(r.host, "bastion");
    assert_eq!(
        r.tailnet.as_deref(),
        Some(format!("ws://127.0.0.1:{port}/v1/connect").as_str())
    );
    assert!(
        text(&out).contains("via Thurm's Tailscale node"),
        "{}",
        text(&out)
    );

    let list = w.thurm(&["remote", "list"]);
    assert!(list.status.success());
    let line = text(&list);
    assert!(
        line.contains("bastion") && line.contains("tailnet"),
        "{line}"
    );
}

#[test]
fn an_unreachable_peer_is_not_added() {
    let w = World::new("b");
    let out = w.thurm(&["remote", "add", "nobody", "nobody", "--tailnet"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("nobody"), "{}", text(&out));
    assert!(!w.config().contains("[[remote]]"), "{}", w.config());
}

#[test]
fn peers_are_listed_through_the_dial_socket() {
    let w = World::new("c");
    let out = w.thurm(&["tailscale", "peers"]);
    assert!(out.status.success(), "{}", text(&out));
    let table = text(&out);
    assert!(
        table.contains("bastion.tail1234.ts.net") && table.contains("100.64.0.1"),
        "{table}"
    );
    let out = w.thurm(&["--json", "tailscale", "peers"]);
    assert!(out.status.success(), "{}", text(&out));
    let peers: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(peers[0]["dns_name"], "bastion.tail1234.ts.net");
    assert_eq!(peers[0]["online"], true);
}

#[test]
fn without_a_dial_socket_peers_says_how_to_get_one() {
    let w = World::new("d");
    std::fs::remove_file(w.path(thurm_config::TAILNET_DIAL_SOCKET)).unwrap();
    let out = w.thurm(&["tailscale", "peers"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("thurm tailscale on"), "{}", text(&out));
}

#[test]
fn the_dial_socket_commands_refuse_remote() {
    let w = World::new("e");
    for args in [
        &["--remote", "devbox", "tailscale", "peers"][..],
        &["--remote", "devbox", "tailscale", "nc", "bastion", "22"][..],
    ] {
        let out = w.thurm(args);
        assert!(!out.status.success(), "{args:?}");
        assert!(text(&out).contains("drop --remote"), "{}", text(&out));
    }
}

#[test]
fn a_tailnet_address_adds_a_host_with_no_ssh_target() {
    let w = World::new("f");
    let port = stand_in_host(&w);
    let address = format!("ws://127.0.0.1:{port}");
    let out = w.thurm(&["remote", "add", "bastion", &address]);
    assert!(out.status.success(), "{}", text(&out));
    let cfg = thurm_config::Config::parse(&w.config()).unwrap();
    let r = cfg.remote("bastion").unwrap();
    assert_eq!(r.ssh_target(), None, "{}", w.config());
    let config = w.config();
    let entry: Vec<&str> = config
        .rsplit("\n[[remote]]\n")
        .next()
        .unwrap()
        .lines()
        .take_while(|l| !l.starts_with('#'))
        .collect();
    let entry = entry.join("\n");
    assert!(!entry.contains("host"), "{entry}");
    assert_eq!(
        r.tailnet.as_deref(),
        Some(format!("{address}/v1/connect").as_str())
    );
    assert_eq!(r.daemon_id.as_deref(), Some(fakes::FAKE_DAEMON_ID));
    // The app opens a workspace on it once it connects; no app runs here, and it says so.
    assert!(w.path("remote/bastion.added").exists());
    assert!(
        text(&out).contains("when it next opens, and opens a workspace on it"),
        "{}",
        text(&out)
    );

    // What needs ssh says the host has none, and how to give it one.
    for args in [
        &["remote", "install", "bastion", "--yes"][..],
        &["remote", "doctor", "bastion"][..],
        &["handoff", "--remote", "bastion"][..],
    ] {
        let out = w.thurm(args);
        assert!(!out.status.success(), "{args:?}: {}", text(&out));
        let said = text(&out);
        assert!(
            said.contains("bastion has no ssh target")
                && said.contains("thurm remote edit bastion --ssh"),
            "{args:?}: {said}"
        );
    }
    // Among every host, the doctor skips it.
    let all = w.thurm(&["remote", "doctor"]);
    assert!(text(&all).contains("has no ssh target"), "{}", text(&all));
    let list = text(&w.thurm(&["remote", "list"]));
    assert!(
        list.contains("bastion") && list.contains("tailnet"),
        "{list}"
    );

    // Given an ssh target, it keeps its tailnet address.
    let out = w.thurm(&["remote", "edit", "bastion", "--ssh", "me@bastion"]);
    assert!(out.status.success(), "{}", text(&out));
    let cfg = thurm_config::Config::parse(&w.config()).unwrap();
    let r = cfg.remote("bastion").unwrap();
    assert_eq!(r.ssh_target(), Some("me@bastion"));
    assert!(r.tailnet.is_some());
    let bad = w.thurm(&["remote", "edit", "bastion", "--ssh", "-oProxyCommand=x"]);
    assert!(!bad.status.success());

    // Removing it forgets that it was just added.
    let out = w.thurm(&["remote", "remove", "bastion"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!w.path("remote/bastion.added").exists());
}

#[test]
fn the_same_daemon_is_not_added_under_a_second_name() {
    let w = World::new("g");
    let port = stand_in_host(&w);
    let out = w.thurm(&["remote", "add", "one", &format!("ws://127.0.0.1:{port}")]);
    assert!(out.status.success(), "{}", text(&out));
    let before = w.config();
    // Another spelling of the address: told apart by the daemon's identifier.
    let out = w.thurm(&["remote", "add", "two", &format!("ws://bastion:{port}")]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("two is the same host as one; use one."),
        "{}",
        text(&out)
    );
    // With an ssh target as well (--tailnet), it is still the same daemon.
    let out = w.thurm(&[
        "remote",
        "add",
        "three",
        "me@elsewhere",
        "--tailnet",
        &format!("ws://127.0.0.1:{port}"),
    ]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("three is the same host as one"),
        "{}",
        text(&out)
    );
    assert_eq!(w.config(), before);
}

#[test]
fn unreachable_and_ambiguous_addresses_are_not_added() {
    let w = World::new("h");
    // The stand-in tailnet has no such machine.
    let out = w.thurm(&["remote", "add", "nobody", "nobody.tail1234.ts.net"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("There is no machine named nobody.tail1234.ts.net on your tailnet."),
        "{}",
        text(&out)
    );
    // Nothing listens there.
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = w.thurm(&["remote", "add", "gone", &format!("ws://127.0.0.1:{free}")]);
    assert!(!out.status.success(), "{}", text(&out));
    let out = w.thurm(&["remote", "add", "port", "devbox:2222"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("ssh://devbox:2222"), "{}", text(&out));
    assert!(!w.config().contains("[[remote]]"), "{}", w.config());
}

#[test]
fn remote_info_says_the_daemon_identifier() {
    let w = World::new("i");
    let info = |w: &World| -> serde_json::Value {
        let out = w.thurm(&["remote-info"]);
        assert!(out.status.success(), "{}", text(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let first = info(&w);
    let id = first["daemon_id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 32);
    assert_eq!(info(&w)["daemon_id"], id.as_str());
    assert_eq!(
        std::fs::read_to_string(w.path("state/daemon-id"))
            .unwrap()
            .trim(),
        id
    );
}
