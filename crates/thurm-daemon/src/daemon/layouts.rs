use super::*;

pub(super) struct PendingLayout {
    ui: u64,
    reply: Sender<Result<(), String>>,
}

impl Daemon {
    pub(super) fn apply_layout(
        &self,
        caller: u64,
        json: String,
        timeout_ms: u64,
    ) -> Result<(), String> {
        if !(1..=30_000).contains(&timeout_ms) {
            return Err("layout timeout must be between 1 and 30000 ms".into());
        }
        let tab: thurm_proto::TabLayout =
            serde_json::from_str(&json).map_err(|e| format!("invalid tab layout: {e}"))?;
        let ids = thurm_proto::template::validate_tab(&tab)?;
        if ids.iter().any(|id| self.pane(*id).is_none()) {
            return Err("a layout pane no longer exists".into());
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut tried = Vec::new();
        loop {
            let ui = self
                .clients
                .lock()
                .values()
                .filter(|c| c.ui.load(Ordering::Relaxed) && !tried.contains(&c.id))
                .min_by_key(|c| c.id)
                .cloned()
                .ok_or(thurm_proto::LAYOUT_NO_WINDOW)?;
            tried.push(ui.id);
            let result = self.confirm_layout(caller, &ui, json.clone(), deadline);
            // This reply guarantees that the desktop made no layout changes.
            // Other rejections must reach the caller so it can close the new panes.
            if result
                .as_ref()
                .err()
                .is_none_or(|error| error != thurm_proto::LAYOUT_NO_WINDOW)
            {
                return result;
            }
        }
    }

    fn confirm_layout(
        &self,
        caller: u64,
        ui: &Client,
        json: String,
        deadline: Instant,
    ) -> Result<(), String> {
        if Instant::now() >= deadline {
            return Err("desktop layout confirmation timed out".into());
        }
        let request_id = self.next_layout.fetch_add(1, Ordering::Relaxed);
        let (reply, receive) = crossbeam_channel::bounded(1);
        {
            let mut pending = self.pending_layouts.lock();
            if pending.len() >= 64 {
                return Err("too many layout requests are pending".into());
            }
            pending.insert(request_id, PendingLayout { ui: ui.id, reply });
        }
        ui.send(ServerMessage::Event(Event::Ui(
            thurm_proto::UiCommand::OpenLayout { json, request_id },
        )));
        let result = loop {
            let now = Instant::now();
            if now >= deadline {
                break Err("desktop layout confirmation timed out".into());
            }
            {
                let clients = self.clients.lock();
                if !clients.contains_key(&ui.id) || !clients.contains_key(&caller) {
                    break Err("a client disconnected before layout confirmation".into());
                }
            }
            match receive.recv_timeout((deadline - now).min(Duration::from_millis(25))) {
                Ok(result) => break result,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    break Err("layout confirmation channel closed".into());
                }
            }
        };
        self.pending_layouts.lock().remove(&request_id);
        result
    }

    pub(super) fn layout_applied(
        &self,
        client: u64,
        request_id: u64,
        error: Option<String>,
    ) -> Result<(), String> {
        let pending = self.pending_layouts.lock();
        let request = pending
            .get(&request_id)
            .ok_or("layout request is no longer pending")?;
        if request.ui != client {
            return Err("only the selected desktop client can confirm this layout".into());
        }
        request
            .reply
            .try_send(error.map_or(Ok(()), Err))
            .map_err(|_| "layout request already has a reply".into())
    }
}
