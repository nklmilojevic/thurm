//! Clipboard tools in a pane (`thurm clip`, and `pbcopy`, `xclip`... run as it) reach the
//! clipboard of the app showing the pane: a copy goes out like an OSC 52 one, a paste asks the
//! desktop UI that last focused the pane and waits for its answer.

use super::*;

/// How long a paste waits for the app: long enough for the user to answer its question.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// At most this many pastes wait at once.
const MAX_PENDING: usize = 64;

/// At most this many bytes of clipboard contents are held at once, on their way between the
/// app and the tools (answers) or from a tool to the apps (copies): four of the largest. Without
/// it, 64 pastes answered with 32 MiB images each held 2 GiB.
const MAX_HELD: usize = 4 * thurm_proto::CLIP_MAX;

/// [`MAX_HELD`], or `THURM_CLIP_HELD_BYTES` (tests).
pub(super) fn max_held() -> usize {
    std::env::var("THURM_CLIP_HELD_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_HELD)
}

/// A bounded amount in use: the pastes waiting now (each on a thread of its own: a slot is
/// taken before the thread is made, so a client sending pastes without waiting for answers
/// cannot make more threads), or the clipboard bytes held.
#[derive(Default)]
pub(super) struct Slots(std::sync::atomic::AtomicUsize);

/// Some of [`Slots`], given back when dropped.
pub(super) struct Slot(Arc<Slots>, usize);

impl Slots {
    /// Takes `n` of at most `max`, or nothing when that much is not free.
    fn take(self: &Arc<Self>, n: usize, max: usize) -> Option<Slot> {
        self.0
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_add(n).filter(|u| *u <= max)
            })
            .ok()
            .map(|_| Slot(self.clone(), n))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.0.fetch_sub(self.1, Ordering::SeqCst);
    }
}

/// An answer's contents, with the share of [`MAX_HELD`] they take until sent on.
type Held = (Vec<u8>, Slot);

pub(super) struct PendingClip {
    ui: u64,
    reply: Sender<Result<Held, String>>,
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
    /// As for an OSC 52 copy: this host's config can turn copies (and clears) off.
    fn clipboard_writes(&self) -> Result<(), String> {
        if matches!(
            self.config.read().terminal.osc52,
            thurm_config::Osc52Mode::Copy | thurm_config::Osc52Mode::CopyPaste
        ) {
            Ok(())
        } else {
            Err("copies to the clipboard are off on this host (terminal.osc52)".into())
        }
    }

    pub(super) fn clipboard_set(&self, pane: PaneId, text: String) -> Result<Response, String> {
        self.clipboard_writes()?;
        if text.len() > thurm_proto::CLIP_MAX {
            return Err("the text is larger than 32 MiB".into());
        }
        self.pane(pane).ok_or("no such pane")?;
        // Held while it is copied out to every app.
        let _held = self.clip_held_bytes(text.len())?;
        self.broadcast(Event::ClipboardStore { pane, text }, true);
        Ok(Response::Ok)
    }

    /// Empties the clipboard: [`Event::ClipboardClear`] to the UIs that take it, which an
    /// older app does not (it gets nothing, as a copy it could not store).
    pub(super) fn clipboard_clear(&self, pane: PaneId) -> Result<Response, String> {
        self.clipboard_writes()?;
        self.pane(pane).ok_or("no such pane")?;
        let uis: Vec<_> = self
            .clients
            .lock()
            .values()
            .filter(|c| c.ui.load(Ordering::Relaxed) && c.clip_query.load(Ordering::Relaxed))
            .cloned()
            .collect();
        if uis.is_empty() {
            return Err("no Thurm app that can clear its clipboard is connected".into());
        }
        for ui in uis {
            ui.send(ServerMessage::Event(Event::ClipboardClear { pane }));
        }
        Ok(Response::Ok)
    }

    /// The desktop UI to ask about `pane` (see [`pick_ui`]).
    fn clipboard_ui(&self, pane: PaneId) -> Option<Arc<Client>> {
        let last = self.focus.lock().last_client(pane);
        let clients = self.clients.lock();
        let candidates: Vec<(u64, bool)> = clients
            .values()
            .filter(|c| {
                c.ui.load(Ordering::Relaxed)
                    && !c.mobile.load(Ordering::Relaxed)
                    && c.clip_query.load(Ordering::Relaxed)
                    && !c.gone.load(Ordering::Relaxed)
            })
            .map(|c| (c.id, c.clip_verified.load(Ordering::Relaxed)))
            .collect();
        pick_ui(last, &candidates).and_then(|id| clients.get(&id).cloned())
    }

    /// Checks a paste and takes a slot for it, on the reader's thread: what is refused here
    /// gets its answer without a thread being made.
    pub(super) fn clipboard_admit(&self, pane: PaneId, mime: &str) -> Result<Slot, String> {
        // As for an OSC 52 read: this host's config can turn them off, the type list too.
        if !matches!(
            self.config.read().terminal.osc52,
            thurm_config::Osc52Mode::Paste | thurm_config::Osc52Mode::CopyPaste
        ) {
            return Err("pastes from the clipboard are off on this host (terminal.osc52)".into());
        }
        if ![
            thurm_proto::CLIP_TEXT,
            thurm_proto::CLIP_PNG,
            thurm_proto::CLIP_TARGETS,
        ]
        .contains(&mime)
        {
            return Err(format!("unsupported clipboard type {mime}"));
        }
        self.pane(pane).ok_or("no such pane")?;
        self.clip_slots
            .take(1, MAX_PENDING)
            .ok_or_else(|| "too many clipboard requests are waiting".into())
    }

    /// Takes `n` bytes of [`max_held`] for clipboard contents on their way.
    fn clip_held_bytes(&self, n: usize) -> Result<Slot, String> {
        self.clip_bytes.take(n, max_held()).ok_or_else(|| {
            "too much clipboard data is on its way already; try again in a moment".into()
        })
    }

    /// A paste [`Daemon::clipboard_admit`] let through: asks the app and waits for it. The
    /// [`Slot`] with an answer holds its bytes' share until the response is sent.
    pub(super) fn clipboard_get(
        &self,
        caller: u64,
        pane: PaneId,
        mime: String,
    ) -> (Result<Response, String>, Option<Slot>) {
        match self.clipboard_get_held(caller, pane, mime) {
            Ok((data, held)) => (Ok(Response::ClipboardData { data }), Some(held)),
            Err(e) => (Err(e), None),
        }
    }

    fn clipboard_get_held(&self, caller: u64, pane: PaneId, mime: String) -> Result<Held, String> {
        if ![
            thurm_proto::CLIP_TEXT,
            thurm_proto::CLIP_PNG,
            thurm_proto::CLIP_TARGETS,
        ]
        .contains(&mime.as_str())
        {
            return Err(format!("unsupported clipboard type {mime}"));
        }
        self.pane(pane).ok_or("no such pane")?;
        let ui = self
            .clipboard_ui(pane)
            .ok_or("no Thurm app that can share its clipboard is connected")?;
        let id = self.next_clip.fetch_add(1, Ordering::Relaxed);
        let (reply, answer) = crossbeam_channel::bounded(1);
        {
            let mut pending = self.pending_clips.lock();
            if pending.len() >= MAX_PENDING {
                return Err("too many clipboard requests are waiting".into());
            }
            pending.insert(id, PendingClip { ui: ui.id, reply });
        }
        ui.send(ServerMessage::Event(Event::ClipboardQuery {
            pane,
            id,
            mime,
        }));
        let deadline = Instant::now() + answer_timeout();
        let result = loop {
            let now = Instant::now();
            if now >= deadline {
                break Err("the Thurm app did not answer in time".to_string());
            }
            {
                let clients = self.clients.lock();
                if !clients.contains_key(&ui.id) {
                    break Err("the Thurm app disconnected".into());
                }
                if !clients.contains_key(&caller) {
                    break Err("the caller disconnected".into());
                }
            }
            match answer.recv_timeout((deadline - now).min(Duration::from_millis(50))) {
                Ok(answer) => break answer,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    break Err("the clipboard request was dropped".into());
                }
            }
        };
        self.pending_clips.lock().remove(&id);
        result
    }

    pub(super) fn clipboard_answer(
        &self,
        client: u64,
        id: u64,
        data: Option<Vec<u8>>,
    ) -> Result<Response, String> {
        let mut pending = self.pending_clips.lock();
        let request = pending
            .get(&id)
            .ok_or("the clipboard request is no longer waiting")?;
        if request.ui != client {
            return Err("only the app that was asked can answer".into());
        }
        let request = pending.remove(&id).unwrap();
        let (answer, result) = match data {
            Some(d) if d.len() > thurm_proto::CLIP_MAX => {
                let e = "the clipboard is larger than 32 MiB".to_string();
                (Err(e.clone()), Err(e))
            }
            Some(d) => match self.clip_held_bytes(d.len()) {
                Ok(held) => (Ok((d, held)), Ok(Response::Ok)),
                Err(e) => (Err(e.clone()), Err(e)),
            },
            None => (
                Err("the clipboard has nothing of that type, or was not shared".into()),
                Ok(Response::Ok),
            ),
        };
        let _ = request.reply.try_send(answer);
        result
    }
}

/// Of the desktop UIs that can answer (`candidates`: id, and whether it is verified as this
/// daemon's own app, see `ext::peer_is_our_app`), the one to ask: the one that focused the pane
/// `last`, else the first. A verified app goes before every other client, so another program
/// saying it is a UI cannot take the app's questions; a daemon that cannot verify its app (on a
/// remote host, reached through ssh or the tailnet) verifies none and goes by focus alone.
fn pick_ui(last: Option<u64>, candidates: &[(u64, bool)]) -> Option<u64> {
    let any_verified = candidates.iter().any(|&(_, v)| v);
    let eligible = |&&(_, v): &&(u64, bool)| v || !any_verified;
    last.filter(|id| candidates.iter().filter(eligible).any(|(c, _)| c == id))
        .or_else(|| candidates.iter().filter(eligible).map(|&(id, _)| id).min())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Slots, pick_ui};

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
    }

    #[test]
    fn the_app_that_looked_last_is_asked() {
        assert_eq!(pick_ui(Some(3), &[(2, false), (3, false)]), Some(3));
        assert_eq!(pick_ui(None, &[(3, false), (2, false)]), Some(2));
        assert_eq!(
            pick_ui(Some(9), &[(3, false)]),
            Some(3),
            "gone: the first one"
        );
        assert_eq!(pick_ui(Some(1), &[]), None);
    }

    #[test]
    fn a_verified_app_goes_before_any_other_client() {
        // Another program said it is a UI and focused the pane last.
        assert_eq!(pick_ui(Some(5), &[(2, true), (5, false)]), Some(2));
        assert_eq!(
            pick_ui(Some(5), &[(2, true), (4, true), (5, false)]),
            Some(2)
        );
        assert_eq!(
            pick_ui(Some(4), &[(2, true), (4, true), (5, false)]),
            Some(4)
        );
    }
}
