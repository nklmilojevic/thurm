use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;

use thurm_proto::tailnet::{TailscaleState, write_ingress};

use super::*;

fn send(s: &mut UnixStream, id: u64, request: Request) -> Result<Response, String> {
    s.write_all(&codec::encode(&Envelope { id, request }).unwrap())
        .unwrap();
    loop {
        let frame = codec::read_frame(s).unwrap().expect("daemon closed");
        if let ServerMessage::Response { id: got, result } = codec::decode(&frame).unwrap()
            && got == id
        {
            return result;
        }
    }
}

fn hello(s: &mut UnixStream) {
    let r = send(
        s,
        1,
        Request::Hello {
            client: "e2e-tailnet".into(),
            version: PROTOCOL_VERSION,
            ui: false,
            capabilities: vec![],
        },
    );
    assert!(matches!(r, Ok(Response::Hello { .. })), "{r:?}");
}

fn whoami(s: &mut UnixStream) -> Principal {
    match send(s, 2, Request::Whoami) {
        Ok(Response::Whoami(p)) => p,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_local_peer_is_its_uid() {
    let env = Env::new("tailnet-peer");
    let _daemon = env.start();
    let mut s = UnixStream::connect(&env.socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    hello(&mut s);
    let p = whoami(&mut s);
    assert_eq!(p.kind, PrincipalKind::Peer);
    assert_eq!(p.uid, Some(unsafe { libc::getuid() }));
}

#[test]
fn an_ingress_preamble_makes_a_tailnet_caller() {
    let env = Env::new("tailnet-ingress");
    let _daemon = env.start();
    let mut s = UnixStream::connect(&env.socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let caller = Principal {
        kind: PrincipalKind::Tailscale,
        display: "me@example.com".into(),
        user_id: Some(42),
        login_name: Some("me@example.com".into()),
        node: Some("phone".into()),
        via: Some("node".into()),
        ..Default::default()
    };
    write_ingress(&mut s, &caller).unwrap();
    hello(&mut s);
    assert_eq!(whoami(&mut s), caller);
}

#[test]
fn a_preamble_after_the_first_frame_ends_the_connection() {
    let env = Env::new("tailnet-late-ingress");
    let _daemon = env.start();
    let mut s = UnixStream::connect(&env.socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    hello(&mut s);
    write_ingress(&mut s, &Principal::default()).unwrap();
    // Read as a frame, the magic is too long: the daemon drops the connection.
    let mut buf = [0u8; 1];
    let n = s.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "connection still open");
}

/// A helper that answers every command as a running node.
fn stub_helper(env: &Env) -> PathBuf {
    let path = env.dir.join("thurm-tailnet");
    std::fs::write(
        &path,
        r#"#!/bin/sh
while IFS= read -r line <&3; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '{"id":%s,"error":null,"status":{"state":"running","hostname":"stub","dns_name":"stub.tail1234.ts.net","url":"wss://stub.tail1234.ts.net/v1/connect","tags":[],"host_urls":[],"https":{"state":"ready"}}}\n' "$id" >&3
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn enable_runs_the_helper_and_reports_its_status() {
    let env = Env::new("tailnet-enable");
    let helper = stub_helper(&env);
    let _daemon = env.start_with(&[("THURM_TAILNET", &helper)]);
    let (c, rx) = env.connect();
    let r = c
        .request_timeout(
            Request::TailscaleEnable {
                hostname: Some("stub".into()),
                allow_tailnet_members: false,
                advertise_tags: vec![],
                auth_key: None,
            },
            Some(Duration::from_secs(10)),
        )
        .unwrap();
    let Response::TailscaleStatus(status) = r else {
        panic!("{r:?}")
    };
    assert_eq!(status.state, TailscaleState::Running);
    assert_eq!(status.dns_name.as_deref(), Some("stub.tail1234.ts.net"));

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::TailscaleStatusChanged(s)) if s.state == TailscaleState::Running => break,
            Ok(_) => continue,
            Err(_) => panic!("no TailscaleStatusChanged event"),
        }
    }
    match c.request(Request::TailscaleStatus).unwrap() {
        Response::TailscaleStatus(s) => assert_eq!(s.state, TailscaleState::Running),
        other => panic!("{other:?}"),
    }
}

#[test]
fn enable_refuses_tags_without_members() {
    let env = Env::new("tailnet-tags");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let r = c.request(Request::TailscaleEnable {
        hostname: None,
        allow_tailnet_members: false,
        advertise_tags: vec!["tag:thurm".into()],
        auth_key: None,
    });
    let err = format!("{:?}", r.unwrap_err());
    assert!(err.contains("owner-only"), "{err}");
}

#[test]
fn enable_without_the_helper_reports_it_missing() {
    let env = Env::new("tailnet-missing");
    let missing = env.dir.join("no-such-helper");
    let _daemon = env.start_with(&[("THURM_TAILNET", &missing)]);
    let (c, _rx) = env.connect();
    let r = c.request(Request::TailscaleEnable {
        hostname: None,
        allow_tailnet_members: false,
        advertise_tags: vec![],
        auth_key: None,
    });
    assert!(r.is_err(), "{r:?}");
    match c.request(Request::TailscaleStatus).unwrap() {
        Response::TailscaleStatus(s) => {
            assert_eq!(s.state, TailscaleState::Error);
            assert!(s.error.is_some());
        }
        other => panic!("{other:?}"),
    }
}

/// A helper answering every command as a running node enrolled with a tag.
fn tagged_helper(env: &Env) -> PathBuf {
    let path = env.dir.join("thurm-tailnet");
    std::fs::write(
        &path,
        r#"#!/bin/sh
while IFS= read -r line <&3; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '{"id":%s,"error":null,"status":{"state":"running","hostname":"stub","tags":["tag:thurm"],"host_urls":[],"allow_tailnet_members":true,"https":{"state":"ready"}}}\n' "$id" >&3
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn enable(c: &Client, allow_tailnet_members: bool) -> Result<Response, String> {
    c.request_timeout(
        Request::TailscaleEnable {
            hostname: None,
            allow_tailnet_members,
            advertise_tags: vec![],
            auth_key: None,
        },
        Some(Duration::from_secs(10)),
    )
    .map_err(|e| format!("{e:?}"))
}

#[test]
fn a_tagged_node_is_not_narrowed_to_its_owner() {
    let env = Env::new("tailnet-tagged-again");
    let helper = tagged_helper(&env);
    let _daemon = env.start_with(&[("THURM_TAILNET", &helper)]);
    let (c, _rx) = env.connect();
    enable(&c, true).unwrap();
    // `thurm tailscale on` again without --allow-tailnet-members.
    let err = enable(&c, false).unwrap_err();
    assert!(err.contains("tagged"), "{err}");
    match c.request(Request::TailscaleStatus).unwrap() {
        Response::TailscaleStatus(s) => assert!(s.allow_tailnet_members, "{s:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn logout_without_the_helper_is_not_reported_done() {
    let env = Env::new("tailnet-logout-missing");
    let missing = env.dir.join("no-such-helper");
    let _daemon = env.start_with(&[("THURM_TAILNET", &missing)]);
    let (c, _rx) = env.connect();
    let r = c.request_timeout(Request::TailscaleLogout, Some(Duration::from_secs(10)));
    assert!(r.is_err(), "{r:?}");
}

#[test]
fn a_helper_that_exits_is_restarted_once_it_can_start_again() {
    let env = Env::new("tailnet-restart");
    let path = env.dir.join("thurm-tailnet");
    let runs = env.dir.join("runs");
    // The first run answers, then exits; later ones keep running.
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
echo run >> '{runs}'
while IFS= read -r line <&3; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  printf '{{"id":%s,"error":null,"status":{{"state":"running","hostname":"stub","tags":[],"host_urls":[],"https":{{"state":"ready"}}}}}}\n' "$id" >&3
  [ "$(wc -l < '{runs}')" -eq 1 ] && exit 0
done
"#,
            runs = runs.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let away = env.dir.join("thurm-tailnet.away");
    let _daemon = env.start_with(&[("THURM_TAILNET", &path)]);
    let (c, _rx) = env.connect();
    enable(&c, false).unwrap();
    // The helper exits; while it can't be started, restarting fails.
    std::fs::rename(&path, &away).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    std::fs::rename(&away, &path).unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let restarted = std::fs::read_to_string(&runs)
            .unwrap_or_default()
            .lines()
            .count()
            >= 2;
        let running = matches!(
            c.request(Request::TailscaleStatus).unwrap(),
            Response::TailscaleStatus(s) if s.state == TailscaleState::Running
        );
        if restarted && running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the helper was not started again"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
