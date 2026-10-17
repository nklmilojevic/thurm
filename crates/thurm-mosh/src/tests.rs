use std::time::{Duration, Instant};

use ocb3::aead::{Aead, KeyInit};

use crate::bootstrap::{self, BootstrapError, Connect, Options};
use crate::crypto::{Crypto, NO_TIMESTAMP, Packet, TO_CLIENT, parse_key};
use crate::fragment::{Assembly, MAX_FRAGMENT, split};
use crate::ssp::{ACK_DELAY, Apply, Engine, HEARTBEAT, MIN_RTO};
use crate::wire::{self, HostEvent, Instruction, UserEvent};
use crate::{Error, MoshEvent, MoshSession, Status};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn ocb_matches_rfc_7253() {
    // RFC 7253 appendix A, the first two samples (AES-128, 96-bit nonce, 128-bit tag).
    let key: [u8; 16] = hex("000102030405060708090A0B0C0D0E0F").try_into().unwrap();
    let cipher = ocb3::Ocb3::<aes::Aes128>::new(&key.into());
    let nonce: [u8; 12] = hex("BBAA99887766554433221100").try_into().unwrap();
    let out = cipher.encrypt(&nonce.into(), b"".as_slice()).unwrap();
    assert_eq!(out, hex("785407BFFFC8AD9EDCC5520AC9111EE6"));
}

fn packet(seq: u64, payload: &[u8]) -> Packet {
    Packet {
        seq,
        timestamp: 1234,
        timestamp_reply: NO_TIMESTAMP,
        payload: payload.to_vec(),
    }
}

#[test]
fn datagrams_round_trip_and_check_their_direction() {
    let c = Crypto::new(&[9; 16]);
    let d = c.seal(TO_CLIENT, &packet(42, b"hello"));
    assert_eq!(&d[..8], &(TO_CLIENT | 42).to_be_bytes());
    assert_eq!(c.open(TO_CLIENT, &d).unwrap(), packet(42, b"hello"));
    assert_eq!(c.open(0, &d), Err(Error::Packet("wrong direction")));
    let mut tampered = d.clone();
    *tampered.last_mut().unwrap() ^= 1;
    assert_eq!(
        c.open(TO_CLIENT, &tampered),
        Err(Error::Packet("does not decrypt"))
    );
    assert!(Crypto::new(&[8; 16]).open(TO_CLIENT, &d).is_err());
    assert_eq!(c.open(0, &d[..10]), Err(Error::Packet("too short")));
}

#[test]
fn session_keys_are_unpadded_base64() {
    assert_eq!(parse_key("AAAAAAAAAAAAAAAAAAAAAA").unwrap(), [0; 16]);
    assert_eq!(parse_key("/////////////////////w").unwrap(), [0xff; 16]);
    assert!(parse_key("AAAA").is_err());
    assert!(parse_key("not base64 at all!!!!!").is_err());
}

#[test]
fn fragments_reassemble_in_any_order() {
    let data: Vec<u8> = (0..MAX_FRAGMENT * 2 + 7).map(|i| i as u8).collect();
    let frags = split(5, &data);
    assert_eq!(frags.len(), 3);
    assert_eq!(&frags[2][8..10], &(0x8002u16).to_be_bytes());
    let mut a = Assembly::default();
    assert_eq!(a.add(&frags[2]).unwrap(), None);
    assert_eq!(a.add(&frags[0]).unwrap(), None);
    assert_eq!(a.add(&frags[0]).unwrap(), None);
    assert_eq!(a.add(&frags[1]).unwrap(), Some(data));
    // A newer instruction drops a partial older one.
    let older = split(6, &[1; MAX_FRAGMENT + 1]);
    let newer = split(7, b"x");
    assert_eq!(a.add(&older[0]).unwrap(), None);
    assert_eq!(a.add(&newer[0]).unwrap(), Some(b"x".to_vec()));
    assert_eq!(split(8, b"")[0], [0, 0, 0, 0, 0, 0, 0, 8, 0x80, 0]);
}

#[test]
fn instructions_encode_as_protobuf() {
    let i = Instruction {
        protocol_version: 2,
        old_num: 1,
        new_num: 300,
        ack_num: 4,
        throwaway_num: 1,
        diff: b"hi".to_vec(),
    };
    let bytes = i.encode();
    assert_eq!(
        bytes,
        [
            0x08, 2, 0x10, 1, 0x18, 0xac, 0x02, 0x20, 4, 0x28, 1, 0x32, 2, b'h', b'i'
        ]
    );
    assert_eq!(Instruction::decode(&bytes).unwrap(), i);
    // Unknown fields (chaff, 7) are skipped.
    let mut with_chaff = bytes.clone();
    with_chaff.extend([0x3a, 3, 1, 2, 3]);
    assert_eq!(Instruction::decode(&with_chaff).unwrap(), i);
    assert!(Instruction::decode(&[0x32, 5, 1]).is_err());
}

#[test]
fn user_messages_carry_keys_and_resizes() {
    assert_eq!(
        wire::encode_user(&[UserEvent::Keys(b"a".to_vec())]),
        [0x0a, 5, 0x12, 3, 0x22, 1, b'a']
    );
    assert_eq!(
        wire::encode_user(&[UserEvent::Resize {
            width: 80,
            height: 24
        }]),
        [0x0a, 6, 0x1a, 4, 0x28, 80, 0x30, 24]
    );
}

#[test]
fn host_messages_carry_bytes_and_echo_acks() {
    let msg = [
        0x0a, 7, 0x12, 5, 0x22, 3, b'a', b'b', b'c', //
        0x0a, 4, 0x3a, 2, 0x40, 7, //
        0x0a, 6, 0x1a, 4, 0x28, 100, 0x30, 30,
    ];
    assert_eq!(
        wire::decode_host(&msg).unwrap(),
        [
            HostEvent::Bytes(b"abc".to_vec()),
            HostEvent::EchoAck(7),
            HostEvent::Resize {
                width: 100,
                height: 30
            }
        ]
    );
}

#[test]
fn instructions_are_zlib_compressed() {
    let data = vec![b'z'; 10_000];
    let packed = wire::compress(&data);
    assert!(packed.len() < 200);
    assert_eq!(packed[0], 0x78, "a zlib header");
    assert_eq!(wire::decompress(&packed).unwrap(), data);
    assert!(wire::decompress(b"not zlib").is_err());
}

fn host(old: u64, new: u64, ack: u64, throwaway: u64, diff: &[u8]) -> Instruction {
    Instruction {
        protocol_version: 2,
        old_num: old,
        new_num: new,
        ack_num: ack,
        throwaway_num: throwaway,
        diff: diff.to_vec(),
    }
}

fn keys(diff: &[u8]) -> Vec<UserEvent> {
    // Decode a user message back, for the assertions.
    let mut events = Vec::new();
    let mut rest = diff;
    while !rest.is_empty() {
        let len = rest[1] as usize;
        let inner = &rest[2..2 + len];
        match inner[0] {
            0x12 => events.push(UserEvent::Keys(inner[4..].to_vec())),
            0x1a => events.push(UserEvent::Resize {
                width: inner[3] as u16,
                height: inner[5] as u16,
            }),
            _ => panic!("{inner:?}"),
        }
        rest = &rest[2 + len..];
    }
    events
}

#[test]
fn sends_input_and_resends_it_until_acknowledged() {
    let t0 = Instant::now();
    let mut e = Engine::new(t0);
    e.push(UserEvent::Resize {
        width: 80,
        height: 24,
    });
    let first = e.poll(t0).unwrap();
    assert_eq!((first.old_num, first.new_num, first.ack_num), (0, 1, 0));
    assert_eq!(
        keys(&first.diff),
        [UserEvent::Resize {
            width: 80,
            height: 24
        }]
    );
    assert_eq!(e.poll(t0), None);
    assert_eq!(e.next_deadline(t0), t0 + MIN_RTO);
    // Not acknowledged: sent again, as a new state from the same base.
    let again = e.poll(t0 + MIN_RTO).unwrap();
    assert_eq!((again.old_num, again.new_num), (0, 2));
    assert_eq!(again.diff, first.diff);
    // Backs off.
    assert_eq!(e.poll(t0 + MIN_RTO * 2), None);
    // The host acknowledges state 2: new input diffs from there.
    e.receive(t0 + MIN_RTO * 2, host(0, 0, 2, 0, b"")).unwrap();
    e.push(UserEvent::Keys(b"l".to_vec()));
    e.push(UserEvent::Keys(b"s".to_vec()));
    let next = e.poll(t0 + MIN_RTO * 2).unwrap();
    assert_eq!((next.old_num, next.new_num, next.throwaway_num), (2, 3, 2));
    assert_eq!(keys(&next.diff), [UserEvent::Keys(b"ls".to_vec())]);
}

#[test]
fn heartbeats_are_new_empty_states() {
    let t0 = Instant::now();
    let mut e = Engine::new(t0);
    let hello = e.poll(t0).unwrap();
    assert_eq!((hello.old_num, hello.new_num, hello.diff.len()), (0, 1, 0));
    e.receive(t0, host(0, 0, 1, 0, b"")).unwrap();
    assert_eq!(e.poll(t0 + HEARTBEAT / 2), None);
    let beat = e.poll(t0 + HEARTBEAT).unwrap();
    assert_eq!((beat.old_num, beat.new_num, beat.diff.len()), (1, 2, 0));
}

#[test]
fn applies_host_diffs_and_acknowledges_them() {
    let t0 = Instant::now();
    let mut e = Engine::new(t0);
    e.poll(t0);
    let a = e.receive(t0, host(0, 1, 0, 0, b"screen")).unwrap();
    assert_eq!(
        a,
        Some(Apply {
            old_num: 0,
            new_num: 1,
            diff: b"screen".to_vec(),
            oldest: 0
        })
    );
    // A duplicate, and one based on a state never received, change nothing.
    assert_eq!(e.receive(t0, host(0, 1, 0, 0, b"screen")).unwrap(), None);
    assert_eq!(e.receive(t0, host(5, 6, 0, 0, b"x")).unwrap(), None);
    // The ack goes out shortly, carrying the newest state.
    assert_eq!(e.next_deadline(t0), t0 + ACK_DELAY);
    let ack = e.poll(t0 + ACK_DELAY).unwrap();
    assert_eq!(ack.ack_num, 1);
    assert_eq!(e.take_snapshot_due(), Some(1));
    assert_eq!(e.take_snapshot_due(), None);
    // The host may diff from an older state we acknowledged: applied, from its snapshot.
    e.receive(t0, host(1, 2, 0, 0, b"two")).unwrap();
    let b = e.receive(t0, host(1, 3, 0, 1, b"three")).unwrap().unwrap();
    assert_eq!((b.old_num, b.new_num, b.oldest), (1, 3, 1));
    // Not from one shown but never acknowledged (no snapshot of it).
    e.receive(t0, host(3, 4, 0, 1, b"four")).unwrap();
    assert_eq!(e.receive(t0, host(3, 5, 0, 1, b"five")).unwrap(), None);
    // An older state than shown is dropped.
    assert_eq!(e.receive(t0, host(1, 2, 0, 1, b"two")).unwrap(), None);
    assert!(
        e.receive(
            t0,
            Instruction {
                protocol_version: 3,
                ..host(3, 4, 0, 0, b"")
            }
        )
        .is_err()
    );
}

#[test]
fn bootstrap_reads_mosh_connect() {
    let exec = |line: &str| {
        assert!(
            line.starts_with(
                "\"$SHELL\" -lc \"env MOSH_SERVER_NETWORK_TMOUT=259200 mosh-server new -s -c 256 "
            ),
            "{line}"
        );
        assert!(line.ends_with(" -p 60001:60010\""), "{line}");
        Ok((
            Some(0),
            "\r\nMOSH CONNECT 60004 AAAAAAAAAAAAAAAAAAAAAA\r\n\nmosh-server (mosh 1.4.0)\n\
             [mosh-server detached, pid = 4242]\n"
                .into(),
        ))
    };
    let opts = Options {
        server: None,
        ports: Some("60001:60010".into()),
    };
    assert_eq!(
        bootstrap::start(&exec, &opts).unwrap(),
        Connect {
            port: 60004,
            key: [0; 16],
            pid: Some(4242)
        }
    );
}

#[test]
fn bootstrap_tries_the_usual_places_then_says_how_to_install() {
    let tried = std::sync::Mutex::new(Vec::new());
    let exec = |line: &str| {
        tried.lock().unwrap().push(line.to_owned());
        if line.starts_with("uname") {
            return Ok((
                Some(0),
                "Linux\nNAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n".into(),
            ));
        }
        Ok((Some(127), "sh: mosh-server: command not found".into()))
    };
    let err = bootstrap::start(&exec, &Options::default()).unwrap_err();
    assert_eq!(
        err,
        BootstrapError::NotFound {
            hint: Some("sudo apt install mosh".into())
        }
    );
    let tried = tried.into_inner().unwrap();
    assert!(
        tried[1].contains("/opt/homebrew/bin/mosh-server new"),
        "{tried:?}"
    );
    assert_eq!(tried.len(), 7, "{tried:?}");
}

#[test]
fn bootstrap_retries_with_another_locale() {
    let exec = |line: &str| {
        if line.contains("LANG=en_US.UTF-8") {
            Ok((Some(1), "mosh-server needs a UTF-8 native locale to run.\nUnfortunately, the local environment ([no charset variables]) specifies\nthe character set \"US-ASCII\",".into()))
        } else {
            Ok((Some(0), "MOSH CONNECT 60001 AAAAAAAAAAAAAAAAAAAAAA".into()))
        }
    };
    assert_eq!(
        bootstrap::start(&exec, &Options::default()).unwrap().port,
        60001
    );
}

#[test]
fn bootstrap_refuses_what_the_command_line_cant_carry() {
    let exec = |_: &str| -> bootstrap::ExecResult { panic!("ran") };
    for opts in [
        Options {
            server: Some("/opt/mosh server".into()),
            ports: None,
        },
        Options {
            server: Some("mosh-server; rm -rf ~".into()),
            ports: None,
        },
        Options {
            server: None,
            ports: Some("60000-61000 $(x)".into()),
        },
    ] {
        assert!(matches!(
            bootstrap::start(&exec, &opts),
            Err(BootstrapError::Invalid(_))
        ));
    }
}

#[test]
fn install_hints_follow_the_system() {
    assert_eq!(
        bootstrap::install_hint("Darwin", "").as_deref(),
        Some("brew install mosh")
    );
    assert_eq!(
        bootstrap::install_hint("Linux", "ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"").as_deref(),
        Some("sudo dnf install mosh")
    );
    assert_eq!(
        bootstrap::install_hint("Linux", "ID=arch").as_deref(),
        Some("sudo pacman -S mosh")
    );
    assert_eq!(bootstrap::install_hint("Plan9", ""), None);
}

/// Against the real `mosh-server` on this machine, when there is one.
#[test]
fn talks_to_a_real_mosh_server() {
    use std::process::Command;
    let Ok(out) = Command::new("mosh-server")
        .args([
            "new",
            "-i",
            "127.0.0.1",
            "-c",
            "256",
            "-l",
            "LANG=en_US.UTF-8",
            "--",
            "/bin/sh",
        ])
        .env("LANG", "en_US.UTF-8")
        .output()
    else {
        eprintln!("no mosh-server here; skipped");
        return;
    };
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let connect = bootstrap::parse_connect(&text)
        .unwrap_or_else(|| panic!("no MOSH CONNECT in: {text}"))
        .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let session = MoshSession::start(
        rt.handle(),
        format!("127.0.0.1:{}", connect.port).parse().unwrap(),
        connect.key,
        (80, 24),
        move |ev| {
            let _ = tx.send(ev);
        },
    )
    .unwrap();
    session.send_keys(b"echo thurm-$((6*7))\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut screen, mut connected, mut num) = (Vec::new(), false, 0);
    while !String::from_utf8_lossy(&screen).contains("thurm-42") {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(MoshEvent::Status(Status::Connected)) => connected = true,
            Ok(MoshEvent::Status(_) | MoshEvent::Snapshot { .. }) => {}
            Ok(MoshEvent::Screen {
                old_num,
                new_num,
                bytes,
                ..
            }) => {
                assert!(
                    new_num > num && old_num <= num,
                    "{old_num} -> {new_num} after {num}"
                );
                num = new_num;
                screen.extend(bytes);
            }
            Err(_) => panic!(
                "no thurm-42 on the screen: {:?}",
                String::from_utf8_lossy(&screen)
            ),
        }
    }
    assert!(connected);
    // Survives a move to another socket.
    session.wake();
    session.send_keys(b"echo moved-$((2*3))\r");
    let mut after = Vec::new();
    while !String::from_utf8_lossy(&after).contains("moved-6") {
        let left = deadline.saturating_duration_since(Instant::now()) + Duration::from_secs(5);
        if let MoshEvent::Screen { bytes, .. } =
            rx.recv_timeout(left).expect("nothing after the move")
        {
            after.extend(bytes);
        }
    }
    session.send_keys(b"exit\r");
    std::thread::sleep(Duration::from_millis(300));
}
