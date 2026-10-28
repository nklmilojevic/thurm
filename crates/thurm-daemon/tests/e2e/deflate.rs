//! Remote clients that say [`CAP_DEFLATE`] get their big events deflated; the others never do.

use std::io::Write as _;
use std::os::unix::net::UnixStream;

use super::*;

/// A raw connection, so the frames are seen as the daemon sent them, before a client unwraps
/// them.
struct Raw {
    stream: UnixStream,
    next_id: u64,
    /// Events that came before the answer to a request.
    events: std::collections::VecDeque<(bool, Event)>,
}

/// What a connection was sent: the output bytes, and how many of its output events were deflated
/// and how many were small plain ones.
#[derive(Default)]
struct Seen {
    output: usize,
    deflated: usize,
    small_plain: usize,
}

impl Raw {
    fn hello(env: &Env, capabilities: &[&str]) -> Raw {
        let stream = UnixStream::connect(&env.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut raw = Raw {
            stream,
            next_id: 1,
            events: Default::default(),
        };
        let answer = raw.request(Request::Hello {
            client: "e2e-deflate".into(),
            version: PROTOCOL_VERSION,
            ui: false,
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
        });
        assert!(matches!(answer, Ok(Response::Hello { .. })), "{answer:?}");
        raw
    }

    fn request(&mut self, request: Request) -> Result<Response, String> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = codec::encode(&Envelope { id, request }).unwrap();
        self.stream.write_all(&frame).unwrap();
        loop {
            let frame = codec::read_frame(&mut self.stream)
                .unwrap()
                .expect("daemon closed");
            match codec::decode_message(&frame).unwrap() {
                ServerMessage::Response { id: got, result } if got == id => return result,
                ServerMessage::Response { .. } => {}
                ServerMessage::Event(ev) => {
                    let deflated = is_compressed(&frame);
                    self.events.push_back((deflated, ev));
                }
            }
        }
    }

    /// Reads the connection's events until its output reaches `bytes`.
    fn output_until(&mut self, bytes: usize) -> Seen {
        let mut seen = Seen::default();
        while seen.output < bytes {
            let (deflated, ev) = match self.events.pop_front() {
                Some(queued) => queued,
                None => {
                    let frame = codec::read_frame(&mut self.stream)
                        .unwrap()
                        .expect("daemon closed");
                    let ServerMessage::Event(ev) = codec::decode_message(&frame).unwrap() else {
                        continue;
                    };
                    (is_compressed(&frame), ev)
                }
            };
            let Event::Output { data, .. } = ev else {
                continue;
            };
            seen.output += data.len();
            if deflated {
                seen.deflated += 1;
            } else if data.len() < codec::DEFLATE_MIN {
                seen.small_plain += 1;
            }
        }
        seen
    }
}

/// Whether a frame is a deflated event, before it is unwrapped.
fn is_compressed(frame: &[u8]) -> bool {
    matches!(
        codec::decode::<ServerMessage>(frame),
        Ok(ServerMessage::Event(Event::Compressed { .. }))
    )
}

#[test]
fn only_clients_that_ask_get_deflated_events() {
    let env = Env::new("deflate");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let pane = create(&c, &env.dir);
    wait_match(&c, pane, r"\$");

    let mut asks = Raw::hello(&env, &[CAP_DEFLATE]);
    let mut plain = Raw::hello(&env, &[]);
    for raw in [&mut asks, &mut plain] {
        raw.request(Request::Subscribe { pane }).unwrap();
    }

    // Small output first: plain for both.
    c.request(Request::Input {
        pane,
        data: b"echo small\n".to_vec(),
    })
    .unwrap();
    let small = asks.output_until(1);
    assert!(small.small_plain >= 1, "{}", small.output);
    assert_eq!(small.deflated, 0);

    // Then a big one, repetitive so it deflates.
    let big = 64 * 1024;
    c.request(Request::Input {
        pane,
        data: format!("yes 'line of terminal output' | head -c {big}\n").into_bytes(),
    })
    .unwrap();
    let asked = asks.output_until(big);
    assert!(
        asked.deflated >= 1,
        "no deflated output for a client that asked"
    );
    let not_asked = plain.output_until(big);
    assert_eq!(
        not_asked.deflated, 0,
        "a client that did not ask got deflated output"
    );
}

#[test]
fn a_later_hello_keeps_the_capability() {
    let env = Env::new("deflate-hello");
    let _daemon = env.start();
    let (c, _rx) = env.connect();
    let pane = create(&c, &env.dir);
    wait_match(&c, pane, r"\$");
    // A second Hello on the connection, without the capability (a client asking for the daemon's
    // version), must not drop the first one's.
    let mut raw = Raw::hello(&env, &[CAP_DEFLATE]);
    raw.request(Request::Hello {
        client: "e2e-deflate".into(),
        version: PROTOCOL_VERSION,
        ui: false,
        capabilities: vec![],
    })
    .unwrap();
    raw.request(Request::Subscribe { pane }).unwrap();
    c.request(Request::Input {
        pane,
        data: format!("yes 'line of terminal output' | head -c {}\n", 64 * 1024).into_bytes(),
    })
    .unwrap();
    assert!(raw.output_until(64 * 1024).deflated >= 1);
}
