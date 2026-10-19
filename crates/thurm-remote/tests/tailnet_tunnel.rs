//! A `[[remote]]` with `tailnet`, end to end but for Tailscale (see `support/tailnet_fakes.rs`):
//! the supervisor relays its local socket over the stand-ins.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use thurm_config::RemoteConfig;
use thurm_remote::Ssh;
use thurm_remote::tailnet::Via;
use thurm_remote::tunnel::{Phase, Status, Supervisor, Transport, ping};

#[path = "support/tailnet_fakes.rs"]
mod fakes;
use fakes::{daemon, gateway};

/// The stand-in dial socket, once, where the environment points.
fn dial_socket() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let base = base();
        let path = thurm_config::tailnet_dial_socket_path();
        assert!(path.starts_with(base), "{}", path.display());
        fakes::dial_socket(&path);
    });
}

/// The test's directory; the environment points the dial socket and the tunnels into it once,
/// for every test in this file.
fn base() -> &'static Path {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        // Short: Unix socket paths are limited to about 104 bytes.
        let base = std::env::temp_dir().join(format!("thtn{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("remote")).unwrap();
        // SAFETY: set before any test thread reads them, under the OnceLock.
        unsafe {
            std::env::set_var("THURM_SOCKET", base.join("thurmd.sock"));
            std::env::set_var("THURM_REMOTE_DIR", base.join("remote"));
        }
        base
    })
}

fn remote(name: &str, tailnet: &str) -> RemoteConfig {
    RemoteConfig {
        name: name.into(),
        host: "bastion".into(),
        socket: None,
        enabled: true,
        clipboard_read: Default::default(),
        tailnet: Some(tailnet.into()),
    }
}

fn start(r: RemoteConfig) -> Supervisor {
    // Never the real tunnels' directory.
    let base = base();
    assert!(thurm_config::remote_dir().starts_with(base));
    let ssh = Ssh::new(&r.host).unwrap();
    Supervisor::start(r, ssh, true, |_| {})
}

fn wait_for(sup: &Supervisor, want: impl Fn(&Status) -> bool) -> Status {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let s = sup.status();
        if want(&s) {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "gave up waiting; last status {s:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stand_in_daemon(name: &str) -> PathBuf {
    let path = base().join(format!("{name}.d"));
    daemon(&path);
    path
}

#[test]
fn connects_through_the_dial_socket_and_relays() {
    let d = stand_in_daemon("ok");
    dial_socket();
    let (port, stop) = gateway(d, None);
    let sup = start(remote("ok", &format!("ws://127.0.0.1:{port}/v1/connect")));
    let s = wait_for(&sup, |s| s.phase == Phase::Connected);
    assert_eq!(s.transport, Transport::Tailnet);
    assert_eq!(s.via, Some(Via::Node));
    assert_eq!(s.tunnel_pid, None);
    assert_eq!(s.remote_build.as_deref(), Some(thurm_proto::BUILD));
    // The local socket is the tunnel, as with ssh: a client says Hello through it, twice.
    let local = thurm_config::remote_socket_path("ok");
    for _ in 0..2 {
        let (version, _) = ping(&local, Duration::from_secs(10)).unwrap();
        assert_eq!(version, thurm_proto::PROTOCOL_VERSION);
    }
    // `thurm --remote` reads the same state.
    let file = Status::read("ok").unwrap();
    assert_eq!(file.transport, Transport::Tailnet);

    // The host goes away: a restart notices, and the tunnel waits for it.
    stop.send(true).unwrap();
    sup.kick(true);
    wait_for(&sup, |s| {
        s.phase == Phase::Reconnecting && s.message.is_some()
    });
}

#[test]
fn a_refusing_host_needs_attention_with_its_reason() {
    let d = stand_in_daemon("no");
    dial_socket();
    let (port, _stop) = gateway(
        d,
        Some("forbidden: me@example.com is not this host's owner; turn on allow_tailnet_members"),
    );
    let sup = start(remote("no", &format!("ws://127.0.0.1:{port}")));
    let s = wait_for(&sup, |s| s.phase == Phase::NeedsAttention);
    let msg = s.message.unwrap();
    assert!(
        msg.contains("does not let this Mac in") && msg.contains("allow_tailnet_members"),
        "{msg}"
    );
}

#[test]
fn an_unknown_peer_needs_attention() {
    dial_socket();
    let sup = start(remote("who", "wss://nobody.tail1234.ts.net/v1/connect"));
    let s = wait_for(&sup, |s| s.phase == Phase::NeedsAttention);
    assert!(s.message.unwrap().contains("nobody"));
}

#[test]
fn a_bad_address_needs_attention() {
    let sup = start(remote("bad", "https://bastion"));
    let s = wait_for(&sup, |s| s.phase == Phase::NeedsAttention);
    assert!(s.message.unwrap().contains("tailnet address"));
}

#[test]
fn a_status_file_without_the_new_fields_still_reads() {
    let old = r#"{"name":"x","host":"x","phase":"connected","message":null,"socket":"/x","retry_at":null,"since":1,"remote_build":null,"remote_protocol":null,"upgrade_available":false,"os":null,"arch":null,"linger":null,"owner_pid":1}"#;
    let s: Status = serde_json::from_str(old).unwrap();
    assert_eq!(s.transport, Transport::Ssh);
    assert_eq!(s.via, None);
}

#[test]
fn check_says_how_the_host_answers() {
    let d = stand_in_daemon("chk");
    dial_socket();
    let (port, _stop) = gateway(d, None);
    let socket = thurm_config::tailnet_dial_socket_path();
    let c = thurm_remote::tailnet::check(
        &format!("ws://127.0.0.1:{port}"),
        &socket,
        Duration::from_secs(8),
    )
    .unwrap();
    assert_eq!(c.url, format!("ws://127.0.0.1:{port}/v1/connect"));
    assert_eq!(c.via, Via::Node);
    assert_eq!(c.protocol, thurm_proto::PROTOCOL_VERSION);
    let e =
        thurm_remote::tailnet::check("ws://nobody.ts.net:7297", &socket, Duration::from_secs(8))
            .unwrap_err();
    assert!(e.contains("nobody"), "{e}");
}
