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

/// How a subscriber's replay came: deflated, small and plain, or big and plain.
#[derive(Default, Debug)]
struct Seen {
    deflated: usize,
    small_plain: usize,
    big_plain: usize,
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

    /// Subscribes to `pane` and says how the replay (`Attach`) it was sent came: the one big
    /// event a screen gives.
    fn subscribe(&mut self, pane: PaneId) -> Seen {
        self.request(Request::Subscribe { pane }).unwrap();
        loop {
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
            let Event::Attach { state, .. } = ev else {
                continue;
            };
            let mut seen = Seen::default();
            if deflated {
                seen.deflated += 1;
            } else if state.len() < codec::DEFLATE_MIN {
                seen.small_plain += 1;
            } else {
                seen.big_plain += 1;
            }
            return seen;
        }
    }
}

/// Whether a frame is a deflated event, before it is unwrapped.
fn is_compressed(frame: &[u8]) -> bool {
    matches!(
        codec::decode::<ServerMessage>(frame),
        Ok(ServerMessage::Event(Event::Compressed { .. }))
    )
}

/// A pane whose screen holds more than a kilobyte of output, so a subscriber's replay is big.
fn filled_pane(env: &Env) -> (Arc<Client>, PaneId) {
    let (c, _rx) = env.connect();
    let pane = create(&c, &env.dir);
    wait_match(&c, pane, r"\$");
    c.request(Request::Input {
        pane,
        data: b"yes 'line of terminal output' | head -c 8192; echo marker-$((40+2))\n".to_vec(),
    })
    .unwrap();
    wait_match(&c, pane, "marker-42");
    (c, pane)
}

#[test]
fn only_clients_that_ask_get_deflated_events() {
    let env = Env::new("deflate");
    let _daemon = env.start();
    let (c, pane) = filled_pane(&env);
    let asks = Raw::hello(&env, &[CAP_DEFLATE]).subscribe(pane);
    let plain = Raw::hello(&env, &[]).subscribe(pane);
    assert_eq!(asks.deflated, 1, "{asks:?}");
    assert_eq!(plain.deflated, 0, "{plain:?}");
    assert_eq!(plain.big_plain, 1, "{plain:?}");
    drop(c);
}

#[test]
fn a_later_hello_keeps_the_capability() {
    let env = Env::new("deflate-hello");
    let _daemon = env.start();
    let (_c, pane) = filled_pane(&env);
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
    assert_eq!(raw.subscribe(pane).deflated, 1);
}
