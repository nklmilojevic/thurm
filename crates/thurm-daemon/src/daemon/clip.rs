//! Clipboard tools in a pane (`thurm clip`, and `pbcopy`, `xclip`... run as it) reach the
//! clipboard of the app showing the pane: a copy goes out like an OSC 52 one, a paste asks the
//! desktop UI that last focused the pane and waits for its answer.

use super::*;

/// How long a paste waits for the app: long enough for the user to answer its question.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// At most this many pastes wait at once.
const MAX_PENDING: usize = 64;

pub(super) struct PendingClip {
    ui: u64,
    reply: Sender<Result<Vec<u8>, String>>,
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
    pub(super) fn clipboard_set(&self, pane: PaneId, text: String) -> Result<Response, String> {
        if text.len() > thurm_proto::CLIP_MAX {
            return Err("the text is larger than 32 MiB".into());
        }
        self.pane(pane).ok_or("no such pane")?;
        self.broadcast(Event::ClipboardStore { pane, text }, true);
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

    pub(super) fn clipboard_get(
        &self,
        caller: u64,
        pane: PaneId,
        mime: String,
    ) -> Result<Response, String> {
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
                Ok(answer) => break answer.map(|data| Response::ClipboardData { data }),
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
            Some(d) => (Ok(d), Ok(Response::Ok)),
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
    use super::pick_ui;

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
