//! A connection to the daemon without its I/O: the page moves bytes between the WebSocket and
//! [`Session::receive`] / [`Session::take_outgoing`], and keeps time with [`Session::tick`].
//! Subscribed panes have a local copy, as in the apps (`thurm-ffi`): `Attach` and `Output`
//! feed it, it answers viewport and selection locally, and encodes keys and mice into bytes.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use thurm_proto::{
    CAP_MOBILE, Envelope, Event, KeyAction, MouseButton, MouseEvent, MouseKind, PROTOCOL_VERSION,
    PaneId, PaneSize, Request, Response, ScrollCmd, ServerMessage, codec, hello_compatible,
};
use thurm_term::{ClientView, EngineConfig, MouseOutcome, Terminal};
use web_time::Instant;

use crate::grid::{Draw, Grid};
use crate::keys;

/// The daemon drops a mobile client after 45 s without a message.
const PING_EVERY: Duration = Duration::from_secs(15);

/// The client name in `Hello` (shown in the daemon's log).
pub const CLIENT: &str = "thurm-web";

struct LocalPane {
    term: Terminal,
    view: ClientView,
    grid: Grid,
}

impl LocalPane {
    /// Everything the copy would do to the outside world is the daemon's copy's to do.
    fn drain(&mut self) {
        let _ = self.term.drain_events();
    }
}

pub struct Session {
    inbuf: Vec<u8>,
    out: Vec<u8>,
    events: Vec<serde_json::Value>,
    next_id: u64,
    hello_id: u64,
    panes: HashMap<PaneId, LocalPane>,
    engine: EngineConfig,
    last_sent: Instant,
}

/// What the page gets for a key: whether it was sent (and the browser's default is to be
/// prevented).
pub enum KeyResult {
    Sent,
    /// Not a key for the terminal (a dead key, IME): the browser's text input handles it.
    Ignored,
}

impl Session {
    /// `dark`: the page's color scheme, which picks the default theme's dark or light side.
    pub fn new(dark: bool) -> Session {
        let mut s = Session {
            inbuf: Vec::new(),
            out: Vec::new(),
            events: Vec::new(),
            next_id: 1,
            hello_id: 0,
            panes: HashMap::new(),
            engine: EngineConfig::from_config(&thurm_config::Config::default(), dark),
            last_sent: Instant::now(),
        };
        s.hello_id = s.request(Request::Hello {
            client: CLIENT.into(),
            version: PROTOCOL_VERSION,
            ui: false,
            // Notices and pane info like the phone, no desktop UI commands.
            capabilities: vec![CAP_MOBILE.into()],
        });
        s
    }

    /// Bytes from the WebSocket, in any chunks.
    pub fn receive(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.inbuf.extend_from_slice(bytes);
        let mut at = 0;
        while self.inbuf.len() - at >= 4 {
            let len = u32::from_le_bytes(self.inbuf[at..at + 4].try_into().unwrap()) as usize;
            if len > codec::MAX_MESSAGE {
                return Err("the daemon sent a message that is too large".into());
            }
            if self.inbuf.len() - at - 4 < len {
                break;
            }
            let frame = self.inbuf[at + 4..at + 4 + len].to_vec();
            at += 4 + len;
            self.message(&frame);
        }
        self.inbuf.drain(..at);
        Ok(())
    }

    /// Frames for the WebSocket.
    pub fn take_outgoing(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// For the page, oldest first: `{"kind": "hello" | "response" | "event" | "frame", …}`.
    pub fn take_events(&mut self) -> Vec<serde_json::Value> {
        std::mem::take(&mut self.events)
    }

    /// Queues a request; its answer comes as a `response` event with this id.
    pub fn request(&mut self, request: Request) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(id, request);
        id
    }

    /// Queues a request whose answer nobody waits for.
    pub fn notify(&mut self, request: Request) {
        self.send(0, request);
    }

    fn send(&mut self, id: u64, request: Request) {
        match codec::encode(&Envelope { id, request }) {
            Ok(bytes) => self.out.extend_from_slice(&bytes),
            Err(e) => self.error(format!("cannot encode a request: {e}")),
        }
        self.last_sent = Instant::now();
    }

    fn error(&mut self, message: String) {
        self.events
            .push(json!({"kind": "error", "message": message}));
    }

    fn message(&mut self, frame: &[u8]) {
        let msg = match codec::decode::<ServerMessage>(frame) {
            Ok(m) => m,
            // A newer daemon's response: fail the request waiting for it. A newer event: skip.
            Err(_) => {
                if let Some(id) = codec::response_id(frame) {
                    self.events.push(json!({"kind": "response", "id": id,
                        "error": "the daemon answered in a form this page does not know"}));
                }
                return;
            }
        };
        match msg {
            ServerMessage::Response { id, result } if id == self.hello_id => self.hello(result),
            ServerMessage::Response { id, result } => self.events.push(match result {
                Ok(r) => json!({"kind": "response", "id": id, "ok": r}),
                Err(e) => json!({"kind": "response", "id": id, "error": e}),
            }),
            ServerMessage::Event(ev) => self.event(ev),
        }
    }

    fn hello(&mut self, result: Result<Response, String>) {
        self.events.push(match result {
            Ok(Response::Hello {
                version,
                build,
                capabilities,
                ..
            }) => json!({
                "kind": "hello",
                "version": version,
                "build": build,
                "compatible": hello_compatible(version, &capabilities),
                "newer": version > PROTOCOL_VERSION,
            }),
            Ok(other) => json!({"kind": "hello", "error": format!("unexpected answer {other:?}")}),
            Err(e) => json!({"kind": "hello", "error": e}),
        });
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Attach { pane, size, state } => {
                let mut term = Terminal::new(size, self.engine.clone());
                // A browser reads no files; the daemon has replaced the pane's own media.
                term.set_reads_media(false);
                term.replay(&state);
                let _ = term.drain_events();
                self.panes.insert(
                    pane,
                    LocalPane {
                        term,
                        view: ClientView::new(),
                        grid: Grid::default(),
                    },
                );
                self.frame_event(pane);
            }
            // Inside a synchronized update, or at a prompt waiting for the shell's redraw after
            // a resize, the frame waits for its end or `tick` at the deadline.
            Event::Output { pane, data } => {
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.term.advance(&data);
                    p.drain();
                    if p.term.sync_deadline().is_none() {
                        self.frame_event(pane);
                    }
                }
            }
            Event::Resized { pane, size } => {
                if let Some(p) = self.panes.get_mut(&pane) {
                    p.term.resize(size);
                    if p.term.sync_deadline().is_none() {
                        self.frame_event(pane);
                    }
                }
            }
            other => {
                if let Event::PaneClosed { pane } = &other {
                    self.panes.remove(pane);
                }
                self.events.push(json!({"kind": "event", "event": other}));
            }
        }
    }

    /// A pane may have something new to draw: the page asks for [`Session::draw`] on its next
    /// animation frame.
    fn frame_event(&mut self, pane: PaneId) {
        self.events.push(json!({"kind": "frame", "pane": pane}));
    }

    pub fn subscribe(&mut self, pane: PaneId, size: PaneSize) {
        self.notify(Request::Subscribe { pane });
        self.notify(Request::Resize { pane, size });
    }

    pub fn unsubscribe(&mut self, pane: PaneId) {
        self.panes.remove(&pane);
        self.notify(Request::Unsubscribe { pane });
    }

    pub fn resize(&mut self, pane: PaneId, size: PaneSize) {
        self.notify(Request::Resize { pane, size });
    }

    pub fn focus(&mut self, pane: PaneId, focused: bool) {
        if let Some(p) = self.panes.get_mut(&pane) {
            // The daemon's copy reports focus to the program; this one only draws it.
            let _ = p.term.focus(focused);
            self.frame_event(pane);
        }
        self.notify(Request::Focus { pane, focused });
    }

    /// A DOM key. Encoded here when the pane has a local copy, as the apps do.
    pub fn key(
        &mut self,
        pane: PaneId,
        code: &str,
        key: &str,
        mods: u8,
        action: KeyAction,
    ) -> KeyResult {
        let Some(ev) = keys::key_event(code, key, mods, action) else {
            return KeyResult::Ignored;
        };
        match self.panes.get_mut(&pane) {
            Some(p) => {
                let bytes = p.term.key(&ev);
                if !bytes.is_empty() {
                    self.notify(Request::Input { pane, data: bytes });
                }
                self.frame_event(pane);
            }
            None => self.notify(Request::Key { pane, key: ev }),
        }
        KeyResult::Sent
    }

    /// Text the browser's input produced (IME commits, dead keys, a layout's composed
    /// characters).
    pub fn text(&mut self, pane: PaneId, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(p) = self.panes.get_mut(&pane) {
            p.term.scroll(ScrollCmd::Bottom);
            self.frame_event(pane);
        }
        self.notify(Request::Input {
            pane,
            data: text.as_bytes().to_vec(),
        });
    }

    pub fn paste(&mut self, pane: PaneId, text: &str) {
        if let Some(p) = self.panes.get_mut(&pane) {
            p.term.scroll(ScrollCmd::Bottom);
            self.frame_event(pane);
        }
        self.notify(Request::Paste {
            pane,
            text: text.to_owned(),
        });
    }

    /// A mouse press, release or move over a cell. Selects locally, or reports to a program
    /// that asked for the mouse.
    #[allow(clippy::too_many_arguments)]
    pub fn mouse(
        &mut self,
        pane: PaneId,
        kind: u8,
        button: u8,
        mods: u8,
        clicks: u8,
        col: u16,
        row: u16,
        right_half: bool,
        x: u32,
        y: u32,
    ) {
        let ev = MouseEvent {
            kind: match kind {
                0 => MouseKind::Press,
                1 => MouseKind::Release,
                _ => MouseKind::Move,
            },
            button: match button {
                0 => MouseButton::Left,
                1 => MouseButton::Middle,
                2 => MouseButton::Right,
                3 => MouseButton::Back,
                4 => MouseButton::Forward,
                _ => MouseButton::None,
            },
            col,
            row,
            right_half,
            mods,
            clicks: clicks.max(1),
            x,
            y,
        };
        let Some(p) = self.panes.get_mut(&pane) else {
            self.notify(Request::Mouse { pane, event: ev });
            return;
        };
        let (outcome, bytes) = p.term.mouse(&ev);
        if !matches!(outcome, MouseOutcome::None) {
            self.frame_event(pane);
        }
        if !bytes.is_empty() {
            self.notify(Request::Input { pane, data: bytes });
        }
    }

    /// Wheel lines (> 0: up) over a cell: the local viewport, or the program's.
    pub fn wheel(&mut self, pane: PaneId, lines: i32, col: u16, row: u16, mods: u8) {
        if lines == 0 {
            return;
        }
        let Some(p) = self.panes.get_mut(&pane) else {
            self.notify(Request::Wheel {
                pane,
                lines,
                col,
                row,
                mods,
            });
            return;
        };
        let bytes = p.term.wheel(lines, col, row, mods);
        if bytes.is_empty() {
            self.frame_event(pane);
        } else {
            self.notify(Request::Input { pane, data: bytes });
        }
    }

    /// The local copy's selection, for the clipboard.
    pub fn selection_text(&self, pane: PaneId) -> String {
        self.panes
            .get(&pane)
            .map(|p| p.term.selection_text())
            .unwrap_or_default()
    }

    /// What changed in `pane` since its last draw; `None` while it holds its frame (a
    /// synchronized update, a resize waiting for the shell's redraw).
    pub fn draw(&mut self, pane: PaneId) -> Option<Draw> {
        let p = self.panes.get_mut(&pane)?;
        if let Some(snap) = p.term.snapshot(pane, &mut p.view) {
            p.grid.apply(snap.frame);
        }
        let draw = p.grid.take_draw();
        (!draw.dirty.is_empty() || draw.meta.cols > 0).then_some(draw)
    }

    /// Repaint everything on the next draw (a new canvas or font).
    pub fn invalidate(&mut self, pane: PaneId) {
        if let Some(p) = self.panes.get_mut(&pane) {
            p.view.invalidate();
            p.grid.invalidate();
        }
    }

    /// Milliseconds until [`Session::tick`] has something to do.
    pub fn next_deadline_ms(&self) -> f64 {
        let now = Instant::now();
        let mut next = self.last_sent + PING_EVERY;
        for p in self.panes.values() {
            if let Some(d) = p.term.sync_deadline() {
                next = next.min(d);
            }
        }
        next.saturating_duration_since(now).as_secs_f64() * 1000.0
    }

    /// Keeps the connection alive and ends synchronized updates that timed out.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if now >= self.last_sent + PING_EVERY {
            self.notify(Request::Ping);
        }
        let mut flushed = Vec::new();
        for (&pane, p) in &mut self.panes {
            if p.term.sync_deadline().is_some_and(|d| d <= now) {
                p.term.flush_sync();
                p.drain();
                flushed.push(pane);
            }
        }
        for pane in flushed {
            self.frame_event(pane);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thurm_proto::{CAPABILITIES, Key, NamedKey};

    fn size() -> PaneSize {
        PaneSize {
            cols: 20,
            rows: 4,
            cell_width: 8,
            cell_height: 16,
        }
    }

    fn server(msg: &ServerMessage) -> Vec<u8> {
        codec::encode(msg).unwrap()
    }

    fn sent(s: &mut Session) -> Vec<Envelope> {
        let bytes = s.take_outgoing();
        let mut r = bytes.as_slice();
        let mut out = Vec::new();
        while let Some(env) = codec::read_message::<_, Envelope>(&mut r).unwrap() {
            out.push(env);
        }
        out
    }

    fn hello_ok(s: &mut Session) {
        let caps = CAPABILITIES.iter().map(|c| (*c).to_owned()).collect();
        s.receive(&server(&ServerMessage::Response {
            id: s.hello_id,
            result: Ok(Response::Hello {
                version: PROTOCOL_VERSION,
                daemon_pid: 1,
                restored: false,
                build: "test".into(),
                capabilities: caps,
            }),
        }))
        .unwrap();
    }

    fn attach(s: &mut Session, pane: PaneId, state: &[u8]) {
        s.receive(&server(&ServerMessage::Event(Event::Attach {
            pane,
            size: size(),
            state: state.to_vec(),
        })))
        .unwrap();
    }

    fn screen(s: &mut Session, pane: PaneId) -> Vec<String> {
        let p = s.panes.get_mut(&pane).unwrap();
        p.grid.invalidate();
        let d = s.draw(pane).unwrap();
        d.cells
            .chunks(20 * crate::grid::CELL_WORDS)
            .map(|row| {
                row.chunks(crate::grid::CELL_WORDS)
                    .map(|c| char::from_u32(c[0]).filter(|&c| c != '\0').unwrap_or(' '))
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn says_hello_as_a_mobile_client() {
        let mut s = Session::new(true);
        let out = sent(&mut s);
        assert_eq!(out.len(), 1);
        match &out[0].request {
            Request::Hello {
                client,
                version,
                ui,
                capabilities,
            } => {
                assert_eq!(
                    (client.as_str(), *version, *ui),
                    (CLIENT, PROTOCOL_VERSION, false)
                );
                assert_eq!(capabilities, &[CAP_MOBILE.to_owned()]);
            }
            other => panic!("{other:?}"),
        }
        hello_ok(&mut s);
        let ev = s.take_events();
        assert_eq!(ev[0]["kind"], "hello");
        assert_eq!(ev[0]["compatible"], true);
    }

    #[test]
    fn a_daemon_without_active_size_is_incompatible() {
        let mut s = Session::new(true);
        s.receive(&server(&ServerMessage::Response {
            id: s.hello_id,
            result: Ok(Response::Hello {
                version: PROTOCOL_VERSION,
                daemon_pid: 1,
                restored: false,
                build: "old".into(),
                capabilities: vec![],
            }),
        }))
        .unwrap();
        assert_eq!(s.take_events()[0]["compatible"], false);
    }

    #[test]
    fn frames_split_anywhere() {
        let mut stream = Vec::new();
        for pane in 1..=3 {
            stream.extend(server(&ServerMessage::Event(Event::Bell { pane })));
        }
        stream.extend(server(&ServerMessage::Response {
            id: 9,
            result: Ok(Response::Text("x".repeat(70_000))),
        }));
        for chunk in [1, 2, 3, 7, 64, 65_536, stream.len()] {
            let mut s = Session::new(true);
            for part in stream.chunks(chunk) {
                s.receive(part).unwrap();
            }
            let ev = s.take_events();
            assert_eq!(ev.len(), 4, "chunk {chunk}");
            assert_eq!(ev[2]["event"]["Bell"]["pane"], 3);
            assert_eq!(ev[3]["id"], 9);
            assert_eq!(ev[3]["ok"]["Text"].as_str().unwrap().len(), 70_000);
            assert!(s.inbuf.is_empty());
        }
    }

    #[test]
    fn unknown_messages_are_skipped_or_fail_their_request() {
        let mut s = Session::new(true);
        // An event variant from a newer daemon, then a response this build can't decode.
        let mut stream = Vec::new();
        for payload in [vec![1u8, 200, 1, 0], vec![0, 5, 0, 0xE7, 0x07]] {
            stream.extend((payload.len() as u32).to_le_bytes());
            stream.extend(payload);
        }
        stream.extend(server(&ServerMessage::Event(Event::Bell { pane: 2 })));
        s.receive(&stream).unwrap();
        let ev = s.take_events();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0]["kind"], "response");
        assert_eq!(ev[0]["id"], 5);
        assert!(ev[0]["error"].is_string());
        assert_eq!(ev[1]["event"]["Bell"]["pane"], 2);
    }

    #[test]
    fn requests_get_ids_and_answers() {
        let mut s = Session::new(true);
        sent(&mut s);
        let id = s.request(Request::ListPanes);
        assert!(
            matches!(sent(&mut s)[0], Envelope { id: i, request: Request::ListPanes } if i == id)
        );
        s.receive(&server(&ServerMessage::Response {
            id,
            result: Err("nope".into()),
        }))
        .unwrap();
        let ev = s.take_events();
        assert_eq!(
            (ev[0]["id"].as_u64(), ev[0]["error"].as_str()),
            (Some(id), Some("nope"))
        );
    }

    #[test]
    fn attach_replays_and_output_follows() {
        let mut s = Session::new(true);
        hello_ok(&mut s);
        attach(&mut s, 4, b"hello\r\n");
        s.receive(&server(&ServerMessage::Event(Event::Output {
            pane: 4,
            data: b"world".to_vec(),
        })))
        .unwrap();
        let kinds: Vec<_> = s
            .take_events()
            .into_iter()
            .map(|e| e["kind"].clone())
            .collect();
        assert_eq!(kinds, ["hello", "frame", "frame"]);
        assert_eq!(screen(&mut s, 4), ["hello", "world", "", ""]);
        // A second Attach (the daemon resynced a client that fell behind) starts over.
        attach(&mut s, 4, b"again");
        assert_eq!(screen(&mut s, 4), ["again", "", "", ""]);
    }

    #[test]
    fn keys_are_encoded_locally_once_attached() {
        let mut s = Session::new(true);
        sent(&mut s);
        // Not attached: the daemon encodes.
        assert!(matches!(
            s.key(1, "ArrowUp", "ArrowUp", 0, KeyAction::Press),
            KeyResult::Sent
        ));
        match &sent(&mut s)[0].request {
            Request::Key { pane: 1, key } => assert_eq!(key.key, Key::Named(NamedKey::Up)),
            other => panic!("{other:?}"),
        }
        attach(&mut s, 1, b"\x1b[?1h");
        s.key(1, "ArrowUp", "ArrowUp", 0, KeyAction::Press);
        match &sent(&mut s)[0].request {
            Request::Input { pane: 1, data } => assert_eq!(data, b"\x1bOA"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            s.key(1, "KeyE", "Dead", 0, KeyAction::Press),
            KeyResult::Ignored
        ));
        assert!(sent(&mut s).is_empty());
    }

    #[test]
    fn text_paste_and_focus() {
        let mut s = Session::new(true);
        sent(&mut s);
        s.text(1, "é");
        s.paste(1, "a\nb");
        s.focus(1, true);
        let reqs: Vec<_> = sent(&mut s).into_iter().map(|e| e.request).collect();
        assert!(matches!(&reqs[0], Request::Input { data, .. } if data == "é".as_bytes()));
        assert!(matches!(&reqs[1], Request::Paste { text, .. } if text == "a\nb"));
        assert!(matches!(
            reqs[2],
            Request::Focus {
                pane: 1,
                focused: true
            }
        ));
    }

    #[test]
    fn wheel_scrolls_the_local_copy() {
        let mut s = Session::new(true);
        hello_ok(&mut s);
        let lines: String = (0..10).map(|i| format!("line{i}\r\n")).collect();
        attach(&mut s, 1, lines.as_bytes());
        sent(&mut s);
        s.wheel(1, 3, 0, 0, 0);
        assert!(sent(&mut s).is_empty(), "scrolled locally");
        assert_eq!(screen(&mut s, 1)[0], "line4");
        // In the alternate screen the wheel is arrow keys for the program.
        s.receive(&server(&ServerMessage::Event(Event::Output {
            pane: 1,
            data: b"\x1b[?1049h".to_vec(),
        })))
        .unwrap();
        s.wheel(1, 1, 0, 0, 0);
        assert!(matches!(&sent(&mut s)[0].request, Request::Input { .. }));
    }

    #[test]
    fn selection_by_mouse() {
        let mut s = Session::new(true);
        attach(&mut s, 1, b"hello world");
        // Double-click on "world" selects the word.
        s.mouse(1, 0, 0, 0, 2, 7, 0, false, 60, 8);
        s.mouse(1, 1, 0, 0, 2, 7, 0, false, 60, 8);
        assert_eq!(s.selection_text(1), "world");
    }

    #[test]
    fn pings_when_quiet() {
        let mut s = Session::new(true);
        sent(&mut s);
        assert!(s.next_deadline_ms() > 14_000.0);
        s.last_sent -= PING_EVERY;
        assert!(s.next_deadline_ms() < 1.0);
        s.tick();
        assert!(matches!(sent(&mut s)[0].request, Request::Ping));
    }

    #[test]
    fn synchronized_updates_hold_the_frame_until_they_end_or_time_out() {
        let mut s = Session::new(true);
        attach(&mut s, 1, b"");
        s.receive(&server(&ServerMessage::Event(Event::Output {
            pane: 1,
            data: b"\x1b[?2026hpartial".to_vec(),
        })))
        .unwrap();
        assert!(s.next_deadline_ms() <= 150.0);
        let frames = |s: &mut Session| {
            s.take_events()
                .iter()
                .filter(|e| e["kind"] == "frame")
                .count()
        };
        assert_eq!(frames(&mut s), 1, "only the attach's");
        std::thread::sleep(Duration::from_millis(160));
        s.tick();
        assert_eq!(frames(&mut s), 1);
        assert_eq!(screen(&mut s, 1)[0], "partial");
        // Ended by the program: drawn at once.
        s.receive(&server(&ServerMessage::Event(Event::Output {
            pane: 1,
            data: b"\x1b[?2026h!\x1b[?2026l".to_vec(),
        })))
        .unwrap();
        assert_eq!(frames(&mut s), 1);
    }
}
