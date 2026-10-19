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
    // The stand-in answers a dial, not a listing: an error that names the socket's answer.
    let out = w.thurm(&["tailscale", "peers"]);
    assert!(!out.status.success());
    // Without a dial socket at all, the hint says how to get one.
    std::fs::remove_file(w.path(thurm_config::TAILNET_DIAL_SOCKET)).unwrap();
    let out = w.thurm(&["tailscale", "peers"]);
    assert!(text(&out).contains("thurm tailscale on"), "{}", text(&out));
}
