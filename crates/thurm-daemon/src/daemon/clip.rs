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

    /// The desktop UI to ask about `pane`: the one that focused it last, else the first one.
    fn clipboard_ui(&self, pane: PaneId) -> Option<Arc<Client>> {
        let last = self.focus.lock().last_client(pane);
        let clients = self.clients.lock();
        let can = |c: &&Arc<Client>| {
            c.ui.load(Ordering::Relaxed)
                && !c.mobile.load(Ordering::Relaxed)
                && c.clip_query.load(Ordering::Relaxed)
                && !c.gone.load(Ordering::Relaxed)
        };
        last.and_then(|id| clients.get(&id))
            .filter(can)
            .or_else(|| clients.values().filter(can).min_by_key(|c| c.id))
            .cloned()
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
