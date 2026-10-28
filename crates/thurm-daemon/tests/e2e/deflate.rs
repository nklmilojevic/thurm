//! Remote clients that say [`CAP_DEFLATE`] get their big events deflated; the others never do.

use std::io::Write as _;
use std::os::unix::net::UnixStream;

use super::*;

/// A raw connection, so the frames are seen as the daemon sent them, before a client unwraps
/// them.
struct Raw {
    stream: UnixStream,
    next_id: u64,
    /// Events that came before the answer to a request, with whether they were deflated.
    events: std::collections::VecDeque<(bool, Event)>,
}

/// How a subscriber's replay came: deflated, small and plain, or big and plain.
#[derive(Default, Debug)]
struct Seen {
    deflated: usize,
    small_plain: usize,
    big_plain: usize,
}

/// What a subscriber received while a pane's output flowed: the output events and their bytes
/// on the wire, how many of them were deflated, and the output text.
#[derive(Default)]
struct Flood {
    events: usize,
    compressed: usize,
    wire: usize,
    text: Vec<u8>,
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
            let (frame_len, deflated, msg) = self.next_message();
            match msg {
                ServerMessage::Response { id: got, result } if got == id => return result,
                ServerMessage::Response { .. } => {}
                ServerMessage::Event(ev) => self.events.push_back((deflated, ev)),
            }
            let _ = frame_len;
        }
    }

    /// The next message from the daemon: its frame's size on the wire, whether it was deflated,
    /// and the message.
    fn next_message(&mut self) -> (usize, bool, ServerMessage) {
        let frame = codec::read_frame(&mut self.stream)
            .unwrap()
            .expect("daemon closed");
        let msg = codec::decode_message(&frame).unwrap();
        (frame.len() + 4, is_compressed(&frame), msg)
    }

    /// Subscribes to `pane` and says how the replay (`Attach`) it was sent came: the one big
    /// event a screen gives.
    fn subscribe(&mut self, pane: PaneId) -> Seen {
        self.request(Request::Subscribe { pane }).unwrap();
        loop {
            let (deflated, ev) = match self.events.pop_front() {
                Some(queued) => queued,
                None => {
                    let (_, deflated, msg) = self.next_message();
                    match msg {
                        ServerMessage::Event(ev) => (deflated, ev),
                        ServerMessage::Response { .. } => continue,
                    }
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

    /// Reads `pane`'s output after a subscription until `marker` shows in it.
    fn flood(&mut self, marker: &[u8]) -> Flood {
        let mut flood = Flood::default();
        loop {
            let (wire, deflated, ev) = match self.events.pop_front() {
                Some((deflated, ev)) => (0, deflated, ev),
                None => {
                    let (wire, deflated, msg) = self.next_message();
                    match msg {
                        ServerMessage::Event(ev) => (wire, deflated, ev),
                        ServerMessage::Response { .. } => continue,
                    }
                }
            };
            let Event::Output { data, .. } = ev else {
                continue;
            };
            flood.events += 1;
            flood.compressed += usize::from(deflated);
            flood.wire += wire;
            flood.text.extend_from_slice(&data);
            if flood.text.windows(marker.len()).any(|w| w == marker) {
                return flood;
            }
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

/// `seq 1 lines` as the terminal prints it, and the marker the flood ends at.
fn seq_flood(c: &Client, pane: PaneId, lines: usize) {
    c.request(Request::Input {
        pane,
        data: format!("seq 1 {lines}; echo done-$((40+2))\n").into_bytes(),
    })
    .unwrap();
}

/// Reads the same flood on a deflate client and a plain one, concurrently, until `marker`.
fn both_floods(asks: &mut Raw, plain: &mut Raw, marker: &[u8]) -> (Flood, Flood) {
    std::thread::scope(|s| {
        let a = s.spawn(|| asks.flood(marker));
        let p = s.spawn(|| plain.flood(marker));
        (a.join().unwrap(), p.join().unwrap())
    })
}

#[test]
fn a_flood_reads_the_same_merged_as_plain() {
    let env = Env::new("deflate-flood");
    let _daemon = env.start();
    let (c, pane) = filled_pane(&env);
    let mut asks = Raw::hello(&env, &[CAP_DEFLATE]);
    let mut plain = Raw::hello(&env, &[]);
    asks.subscribe(pane);
    plain.subscribe(pane);
    seq_flood(&c, pane, 20_000);
    let (asks_flood, plain_flood) = both_floods(&mut asks, &mut plain, b"done-42");
    // The same bytes in the same order; the deflate client may have fewer, bigger events.
    let (short, long) = if asks_flood.text.len() < plain_flood.text.len() {
        (&asks_flood.text, &plain_flood.text)
    } else {
        (&plain_flood.text, &asks_flood.text)
    };
    assert!(long.starts_with(short), "the two floods differ");
    assert!(asks_flood.compressed >= 1, "{}", asks_flood.events);
    assert_eq!(plain_flood.compressed, 0);
}

/// Not a check: what a flood costs on the wire, with and without deflate. Run with
/// `cargo test -p thurm-daemon --test e2e flood_measure -- --ignored --nocapture`.
#[test]
#[ignore]
fn flood_measure() {
    let env = Env::new("deflate-measure");
    let _daemon = env.start();
    let (c, pane) = filled_pane(&env);
    let mut asks = Raw::hello(&env, &[CAP_DEFLATE]);
    let mut plain = Raw::hello(&env, &[]);
    asks.subscribe(pane);
    plain.subscribe(pane);
    seq_flood(&c, pane, 200_000);
    let (asks_flood, plain_flood) = both_floods(&mut asks, &mut plain, b"done-42");
    eprintln!(
        "FLOOD asks: events {} compressed {} wire {} text {}",
        asks_flood.events,
        asks_flood.compressed,
        asks_flood.wire,
        asks_flood.text.len()
    );
    eprintln!(
        "FLOOD plain: events {} compressed {} wire {} text {}",
        plain_flood.events,
        plain_flood.compressed,
        plain_flood.wire,
        plain_flood.text.len()
    );
}
