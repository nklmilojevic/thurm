//! Clipboard tools in a pane (`thurm clip`, and `pbcopy`, `xclip`... run as it) reach the
//! clipboard of the app showing the pane: the daemon asks the desktop UI that last focused the
//! pane to paste ([`Event::ClipboardQuery`]) or to copy or clear ([`Event::ClipboardWrite`]),
//! and waits for its answer, so a tool hears whether it was done.

use super::*;

/// How long a tool waits for the app: long enough for the user to answer its question.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// At most this many tools wait at once.
const MAX_PENDING: usize = 64;

/// At most this many bytes of clipboard contents are held at once: answers and copies on their
/// way, and what is still queued for a client that has not read it yet (see
/// [`clip_weight`]): four of the largest. Without it, 64 pastes answered with 32 MiB images held
/// 2 GiB, and a client that does not read could have more queued for it without end.
const MAX_HELD: usize = 4 * thurm_proto::CLIP_MAX;

/// [`MAX_HELD`], or `THURM_CLIP_HELD_BYTES` (tests).
pub(super) fn max_held() -> usize {
    std::env::var("THURM_CLIP_HELD_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_HELD)
}

/// The clipboard tools' contents `msg` carries, for [`Client::clip_queued`]: counted while it
/// is queued for the client, until its writer has written or dropped it. Only the tools' own (a
/// copy handed to the app, a paste's answer): an OSC 52 copy goes to every app and phone, and a
/// phone that stops reading must not stop the tools in every pane.
pub fn clip_weight(msg: &ServerMessage) -> usize {
    match msg {
        ServerMessage::Event(Event::ClipboardWrite { text, .. }) => {
            text.as_ref().map_or(0, String::len)
        }
        ServerMessage::Response {
            result: Ok(Response::ClipboardData { data }),
            ..
        } => data.len(),
        _ => 0,
    }
}

/// A bounded amount in use: the tools waiting now (each on a thread of its own: a slot is taken
/// before the thread is made, so a client sending requests without waiting for answers cannot
/// make more threads), or the clipboard bytes held on their way.
#[derive(Default)]
pub(super) struct Slots(std::sync::atomic::AtomicUsize);

/// Some of [`Slots`], given back when dropped.
pub(super) struct Slot(Arc<Slots>, usize);

impl Slots {
    /// Takes `n` of at most `max`, or nothing when that much is not free.
    fn take(self: &Arc<Self>, n: usize, max: usize) -> Option<Slot> {
        self.take_beside(n, max, || 0)
    }

    /// Takes `n` of at most `max` less what `beside` holds besides. What is taken is read
    /// before `beside`, so a hold moving from here to there (taken, queued, given back) is
    /// counted twice at worst, never missed.
    fn take_beside(
        self: &Arc<Self>,
        n: usize,
        max: usize,
        beside: impl Fn() -> usize,
    ) -> Option<Slot> {
        // Nothing to hold (a clear, an empty answer): it adds nothing, whatever is held.
        if n == 0 {
            return Some(Slot(self.clone(), 0));
        }
        loop {
            let used = self.0.load(Ordering::SeqCst);
            let total = used.checked_add(beside())?.checked_add(n)?;
            if total > max {
                return None;
            }
            if self
                .0
                .compare_exchange(used, used + n, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(Slot(self.clone(), n));
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.0.fetch_sub(self.1, Ordering::SeqCst);
    }
}

/// What a tool asks of the app.
pub(super) enum Ask {
    /// The clipboard in this type ([`Request::ClipboardGet`]).
    Paste(String),
    /// Copy this text, or clear the clipboard ([`Request::ClipboardSet`],
    /// [`Request::ClipboardClear`]).
    Write(Option<String>),
}

impl Ask {
    /// The pane and what a clipboard tool's request asks; `None` for any other request.
    pub(super) fn of(req: Request) -> Option<(PaneId, Ask)> {
        match req {
            Request::ClipboardGet { pane, mime } => Some((pane, Ask::Paste(mime))),
            Request::ClipboardSet { pane, text } => Some((pane, Ask::Write(Some(text)))),
            Request::ClipboardClear { pane } => Some((pane, Ask::Write(None))),
            _ => None,
        }
    }
}

/// An answer's contents, with the share of [`MAX_HELD`] they take until queued.
type Held = (Vec<u8>, Slot);

pub(super) struct PendingClip {
    ui: u64,
    reply: Sender<Result<Held, String>>,
    /// What the tool hears when the app answers `None`.
    refused: &'static str,
}

/// Removes request `.1` from the pending ones when dropped.
struct PendingGuard<'a>(&'a Mutex<HashMap<u64, PendingClip>>, u64);

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.lock().remove(&self.1);
    }
}

/// [`ANSWER_TIMEOUT`], or `THURM_CLIP_TIMEOUT_MS` (tests).
fn answer_timeout() -> Duration {
    std::env::var("THURM_CLIP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(ANSWER_TIMEOUT)
}

impl Daemon {
    /// Checks a tool's request and takes a slot for it, on the reader's thread: what is refused
    /// here gets its answer without a thread being made. A copy's text takes its share of
    /// [`max_held`] here too, before a thread holds it: returned beside the slot, it is held
    /// until the text is queued for the app.
    pub(super) fn clipboard_admit(
        &self,
        caller: &Client,
        pane: PaneId,
        ask: &Ask,
    ) -> Result<(Slot, Option<Slot>), String> {
        // A clipboard tool runs in a pane, a process of this user on this machine: a phone or
        // a tailnet caller reading or writing the clipboard through a pane is not one.
        if !caller.is_local_peer() || caller.mobile.load(Ordering::Relaxed) {
            return Err("clipboard tools are for programs on this machine".into());
        }
        let osc52 = self.config.read().terminal.osc52;
        use thurm_config::Osc52Mode::{Copy, CopyPaste, Paste};
        match ask {
            // As for an OSC 52 read: this host's config can turn them off, the type list too.
            Ask::Paste(mime) => {
                if !matches!(osc52, Paste | CopyPaste) {
                    return Err(
                        "pastes from the clipboard are off on this host (terminal.osc52)".into(),
                    );
                }
                if ![
                    thurm_proto::CLIP_TEXT,
                    thurm_proto::CLIP_PNG,
                    thurm_proto::CLIP_TARGETS,
                ]
                .contains(&mime.as_str())
                {
                    return Err(format!("unsupported clipboard type {mime}"));
                }
            }
            // As for an OSC 52 copy, and a clear with it.
            Ask::Write(text) => {
                if !matches!(osc52, Copy | CopyPaste) {
                    return Err(
                        "copies to the clipboard are off on this host (terminal.osc52)".into(),
                    );
                }
                if text
                    .as_ref()
                    .is_some_and(|t| t.len() > thurm_proto::CLIP_MAX)
                {
                    return Err("the text is larger than 32 MiB".into());
                }
            }
        }
        self.pane(pane).ok_or("no such pane")?;
        let slot = self
            .clip_slots
            .take(1, MAX_PENDING)
            .ok_or("too many clipboard requests are waiting")?;
        let bytes = match ask {
            Ask::Write(Some(text)) => Some(self.clip_held_bytes(text.len())?),
            _ => None,
        };
        Ok((slot, bytes))
    }

    /// Clipboard bytes queued for clients and not written yet.
    fn clip_queued(&self) -> usize {
        self.clients
            .lock()
            .values()
            .map(|c| c.clip_queued.load(Ordering::SeqCst))
            .sum()
    }

    /// Takes `n` bytes of [`max_held`] for clipboard contents on their way, beside what is
    /// queued for clients already.
    fn clip_held_bytes(&self, n: usize) -> Result<Slot, String> {
        self.clip_bytes
            .take_beside(n, max_held(), || self.clip_queued())
            .ok_or_else(|| {
                "too much clipboard data is on its way already; try again in a moment".into()
            })
    }

    /// The desktop UI to ask about `pane` (see [`pick_ui`]), or why there is none.
    fn clipboard_ui(&self, pane: PaneId) -> Result<Arc<Client>, String> {
        let last = self.focus.lock().last_client(pane);
        let clients = self.clients.lock();
        let candidates: Vec<(u64, bool)> = clients
            .values()
            .filter(|c| shares_clipboard(c) && !c.gone.load(Ordering::Relaxed))
            .map(|c| (c.id, c.clip_verified.load(Ordering::Relaxed)))
            .collect();
        // A daemon that can tell its own app asks only that: another client saying it is a UI
        // is never asked, the app connected or not.
        let only_verified = ext::verifies_its_app();
        match pick_ui(last, &candidates, only_verified) {
            Some(id) => clients
                .get(&id)
                .cloned()
                .ok_or_else(|| "the Thurm app left".into()),
            None if candidates.len() > 1 && !only_verified => Err(
                "more than one Thurm app is connected and none has shown this pane since: \
                 show it in the one whose clipboard to use"
                    .into(),
            ),
            None => Err("no Thurm app that can share its clipboard is connected".into()),
        }
    }

    /// A request [`Daemon::clipboard_admit`] let through: asks the app and waits for it. The
    /// [`Slot`] with a paste's answer holds its bytes until the response is queued, where
    /// [`Client::clip_queued`] counts them on.
    pub(super) fn clipboard_ask(
        &self,
        caller: u64,
        pane: PaneId,
        ask: Ask,
        bytes: Option<Slot>,
    ) -> (Result<Response, String>, Option<Slot>) {
        let write = matches!(ask, Ask::Write(_));
        match self.clipboard_ask_held(caller, pane, ask, bytes) {
            Ok(_) if write => (Ok(Response::Ok), None),
            Ok((data, held)) => (Ok(Response::ClipboardData { data }), Some(held)),
            Err(e) => (Err(e), None),
        }
    }

    /// `bytes`: a copy's share of [`max_held`], from [`Daemon::clipboard_admit`].
    fn clipboard_ask_held(
        &self,
        caller: u64,
        pane: PaneId,
        ask: Ask,
        bytes: Option<Slot>,
    ) -> Result<Held, String> {
        self.pane(pane).ok_or("no such pane")?;
        let ui = self.clipboard_ui(pane)?;
        let id = self.next_clip.fetch_add(1, Ordering::Relaxed);
        let refused = match ask {
            // The app answers only yes or no: every reason for a no is named.
            Ask::Paste(_) => {
                "the clipboard was not shared: it has nothing of that type (or an image too large \
                 to send), or the Thurm app refused, or is still asking about an earlier read \
                 from this host"
            }
            Ask::Write(_) => {
                "the Thurm app did not take it: this Mac's terminal.osc52 does not allow copies \
                 from this host"
            }
        };
        let (reply, answer) = crossbeam_channel::bounded(1);
        // Taken out of the pending requests however this ends, a panic included: one left
        // behind would hold one of MAX_PENDING for good.
        let _pending = PendingGuard(&self.pending_clips, id);
        {
            // Bounded by the slots `clipboard_admit` hands out.
            let mut pending = self.pending_clips.lock();
            pending.insert(
                id,
                PendingClip {
                    ui: ui.id,
                    reply,
                    refused,
                },
            );
        }
        match ask {
            Ask::Paste(mime) => {
                ui.send(ServerMessage::Event(Event::ClipboardQuery {
                    pane,
                    id,
                    mime,
                }));
            }
            // Held until queued for the app, where its queue counts it.
            Ask::Write(text) => {
                ui.send(ServerMessage::Event(Event::ClipboardWrite {
                    pane,
                    id,
                    text,
                }));
                drop(bytes);
            }
        }
        self.wait_answer(caller, ui.id, answer)
    }

    fn wait_answer(
        &self,
        caller: u64,
        ui: u64,
        answer: crossbeam_channel::Receiver<Result<Held, String>>,
    ) -> Result<Held, String> {
        let deadline = Instant::now() + answer_timeout();
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err("the Thurm app did not answer in time".to_string());
            }
            {
                let clients = self.clients.lock();
                if !clients.contains_key(&ui) {
                    return Err("the Thurm app disconnected".into());
                }
                if !clients.contains_key(&caller) {
                    return Err("the caller disconnected".into());
                }
            }
            match answer.recv_timeout((deadline - now).min(Duration::from_millis(250))) {
                Ok(answer) => return answer,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    return Err("the clipboard request was dropped".into());
                }
            }
        }
    }

    pub(super) fn clipboard_answer(
        &self,
        client: u64,
        id: u64,
        data: Option<Vec<u8>>,
    ) -> Result<Response, String> {
        let request = {
            let mut pending = self.pending_clips.lock();
            let request = pending
                .get(&id)
                .ok_or("the clipboard request is no longer waiting")?;
            if request.ui != client {
                return Err("only the app that was asked can answer".into());
            }
            pending.remove(&id).unwrap()
        };
        let (answer, result) = match data {
            Some(d) if d.len() > thurm_proto::CLIP_MAX => {
                let e = "the clipboard is larger than 32 MiB".to_string();
                (Err(e.clone()), Err(e))
            }
            Some(d) => match self.clip_held_bytes(d.len()) {
                Ok(held) => (Ok((d, held)), Ok(Response::Ok)),
                Err(e) => (Err(e.clone()), Err(e)),
            },
            None => (Err(request.refused.to_string()), Ok(Response::Ok)),
        };
        let _ = request.reply.try_send(answer);
        result
    }
}

/// Of the desktop UIs that can answer (`candidates`: id, and whether it is verified as this
/// daemon's own app, see `ext::peer_is_our_app`), the one to ask: the one that focused the pane
/// `last`, else the only one there is: with several and none of them the last to show the pane,
/// none, rather than the clipboard of a Mac the user is not at. A verified app goes before every
/// other client, so another program saying it is a UI cannot take the app's questions; a daemon
/// that cannot verify its app (on a remote host, reached through ssh or the tailnet) verifies
/// none and goes by focus alone.
fn pick_ui(last: Option<u64>, candidates: &[(u64, bool)], only_verified: bool) -> Option<u64> {
    let any_verified = only_verified || candidates.iter().any(|&(_, v)| v);
    let eligible: Vec<u64> = candidates
        .iter()
        .filter(|&&(_, v)| v || !any_verified)
        .map(|&(id, _)| id)
        .collect();
    match (last.filter(|id| eligible.contains(id)), eligible.as_slice()) {
        (Some(id), _) => Some(id),
        (None, [only]) => Some(*only),
        (None, _) => None,
    }
}

/// Whether `c` is a desktop UI a clipboard tool's request may go to: an app with
/// `clip-query`, not a phone.
pub(super) fn shares_clipboard(c: &Client) -> bool {
    c.ui.load(Ordering::Relaxed)
        && !c.mobile.load(Ordering::Relaxed)
        && c.clip_query.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Ask, MAX_HELD, Slots, pick_ui};

    /// A copy's text takes its share of the held bytes when it is admitted, before a thread
    /// holds it, and gives it back when that is dropped: pipelined copies cannot hold more.
    #[test]
    fn a_copy_takes_its_bytes_when_admitted() {
        use std::sync::atomic::AtomicBool;
        let dir = std::env::temp_dir().join(format!("thurm-clip-admit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let daemon = super::Daemon::new(
            thurm_config::Config::default(),
            dir.join("s.sock"),
            dir.join("state"),
            Arc::new(AtomicBool::new(false)),
        );
        let pane = daemon
            .create_pane(
                thurm_proto::CreatePane {
                    command: Some(vec!["cat".into()]),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let (client, _rx) = daemon.add_client();
        let copy = Ask::Write(Some("x".repeat(thurm_proto::CLIP_MAX)));
        let admitted: Vec<_> = (0..MAX_HELD / thurm_proto::CLIP_MAX)
            .map(|_| daemon.clipboard_admit(&client, pane, &copy).unwrap())
            .collect();
        assert!(admitted.iter().all(|(_, bytes)| bytes.is_some()));
        let Err(e) = daemon.clipboard_admit(&client, pane, &copy) else {
            panic!("admitted past the held bytes");
        };
        assert!(e.contains("too much clipboard data"), "{e}");
        let (_, bytes) = daemon
            .clipboard_admit(&client, pane, &Ask::Write(None))
            .unwrap();
        assert!(bytes.is_none(), "a clear holds no text");
        drop(admitted);
        assert!(daemon.clipboard_admit(&client, pane, &copy).is_ok());
        daemon.close_pane(pane);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pending_request_goes_however_its_wait_ends() {
        let pending = parking_lot::Mutex::new(std::collections::HashMap::new());
        let (reply, _answer) = crossbeam_channel::bounded(1);
        pending.lock().insert(
            7,
            super::PendingClip {
                ui: 1,
                reply,
                refused: "",
            },
        );
        let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = super::PendingGuard(&pending, 7);
            panic!("the wait failed");
        }));
        assert!(waited.is_err());
        assert!(pending.lock().is_empty(), "not left holding a slot");
    }

    #[test]
    fn the_tools_requests_are_one_list() {
        use thurm_proto::Request;
        for req in [
            Request::ClipboardGet {
                pane: 1,
                mime: "text/plain".into(),
            },
            Request::ClipboardSet {
                pane: 1,
                text: String::new(),
            },
            Request::ClipboardClear { pane: 1 },
            Request::ClipboardAnswer { id: 1, data: None },
            Request::ListPanes,
        ] {
            assert_eq!(
                super::Ask::of(req.clone()).is_some(),
                req.is_clipboard_tool(),
                "{req:?}"
            );
            assert_eq!(
                req.carries_clipboard(),
                req.is_clipboard_tool() || matches!(req, Request::ClipboardAnswer { .. })
            );
        }
    }

    #[test]
    fn slots_are_bounded_and_given_back() {
        let slots = Arc::new(Slots::default());
        let held: Vec<_> = (0..3).map(|_| slots.take(1, 3).expect("free")).collect();
        assert!(slots.take(1, 3).is_none(), "all taken");
        drop(held);
        let again: Vec<_> = (0..3)
            .map(|_| slots.take(1, 3).expect("given back"))
            .collect();
        assert_eq!(again.len(), 3);
    }

    /// Four answers of the largest size fit; a fifth waits for one of them to be sent on.
    #[test]
    fn clipboard_bytes_held_at_once_are_bounded() {
        let bytes = Arc::new(Slots::default());
        let max = super::MAX_HELD;
        let full = thurm_proto::CLIP_MAX;
        let held: Vec<_> = (0..4)
            .map(|_| bytes.take(full, max).expect("fits"))
            .collect();
        assert!(
            bytes.take(full, max).is_none(),
            "64 of them no longer add up to 2 GiB"
        );
        assert!(bytes.take(1, max).is_none());
        drop(held);
        assert!(bytes.take(full, max).is_some(), "given back once sent on");
        assert!(bytes.take(usize::MAX, max).is_none(), "no overflow");
        // A clear holds nothing: it goes through however much is held.
        let full = bytes.take(max, max).expect("all of it");
        assert!(bytes.take(1, max).is_none());
        assert!(bytes.take(0, max).is_some(), "zero bytes always fit");
        assert!(bytes.take_beside(0, max, || usize::MAX).is_some());
        drop(full);
    }

    #[test]
    fn the_app_that_looked_last_is_asked() {
        assert_eq!(pick_ui(Some(3), &[(2, false), (3, false)], false), Some(3));
        assert_eq!(pick_ui(None, &[(2, false)], false), Some(2), "the only one");
        // Several and none showed the pane: not one picked at random, maybe another Mac's.
        assert_eq!(pick_ui(None, &[(3, false), (2, false)], false), None);
        assert_eq!(
            pick_ui(Some(9), &[(3, false), (2, false)], false),
            None,
            "gone"
        );
        assert_eq!(
            pick_ui(Some(9), &[(3, false)], false),
            Some(3),
            "gone: the first one"
        );
        assert_eq!(pick_ui(Some(1), &[], false), None);
    }

    /// A daemon that can verify its app (in an app bundle) asks only it: with the app not
    /// connected, nobody, not another client saying it is a UI.
    #[test]
    fn a_daemon_that_verifies_asks_only_its_app() {
        assert_eq!(pick_ui(Some(5), &[(5, false)], true), None);
        assert_eq!(pick_ui(None, &[(5, false), (6, false)], true), None);
        assert_eq!(pick_ui(Some(5), &[(2, true), (5, false)], true), Some(2));
    }

    #[test]
    fn a_verified_app_goes_before_any_other_client() {
        // Another program said it is a UI and focused the pane last.
        assert_eq!(pick_ui(Some(5), &[(2, true), (5, false)], false), Some(2));
        assert_eq!(
            pick_ui(Some(5), &[(2, true), (4, true), (5, false)], false),
            None,
            "two verified, neither the last"
        );
        assert_eq!(
            pick_ui(Some(4), &[(2, true), (4, true), (5, false)], false),
            Some(4)
        );
    }
}
