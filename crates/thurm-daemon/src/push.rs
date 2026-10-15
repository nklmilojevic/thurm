//! Agent notices for the iOS app's lock screen. The app registers its device here (relay URL,
//! device id, secret); each notice is posted to the relay, which holds the APNs key, signed
//! with the device's secret. A notice carries the pane's title and what it wants (permission
//! or attention), never terminal content.
//!
//! The app can also follow an agent in a Live Activity: the activity's push token is kept
//! with the pane, and the agent's state (status, pane title, since when) is posted for it on
//! every change, until the agent leaves or the pane closes.
//!
//! When a pane is read (on the Mac, or on another phone), the devices it sent notices to hear
//! of it (`POST /v1/read`), so the notices leave their lock screens too.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Sender, bounded};
use hmac::{Hmac, KeyInit, Mac};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thurm_proto::PaneId;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Device {
    pub device: String,
    pub relay: String,
    pub secret: String,
    /// What the app calls this daemon.
    pub host: String,
    pub name: String,
}

/// A Live Activity following a pane's agent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Follow {
    pub device: String,
    pub pane: PaneId,
    pub token: String,
    /// The state last sent, to send only changes.
    #[serde(default)]
    pub last: Option<ActivityState>,
}

/// What a Live Activity shows: no terminal content.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ActivityState {
    /// The agent's name ("Claude Code").
    pub agent: String,
    /// working, needs_input, permission, done, error or idle.
    pub status: String,
    pub title: String,
    /// When the status began (Unix seconds).
    pub since: u64,
    pub permission: Option<u64>,
}

impl ActivityState {
    /// Nothing worth an update: same status, agent and prompt. Title changes ride along with
    /// the next of those (agents animate their titles, and iOS budgets activity updates).
    fn same_as(&self, other: &ActivityState) -> bool {
        self.status == other.status
            && self.agent == other.agent
            && self.permission == other.permission
    }
}

/// A title for the lock screen: without the spinners agents animate in it (braille, circles,
/// stars), so it doesn't flicker or change for nothing.
pub fn clean_title(title: &str) -> String {
    let spinner =
        |c: char| ('\u{2800}'..='\u{28FF}').contains(&c) || "◐◓◑◒◴◷◶◵✳✶✻✽✢✺·•⋯…".contains(c);
    let words: Vec<&str> = title
        .split_whitespace()
        .filter(|w| !w.chars().all(spinner))
        .collect();
    let joined = words.join(" ");
    joined
        .trim_matches(|c: char| c == '|' || c == '-' || c == ':' || c.is_whitespace())
        .chars()
        .take(120)
        .collect()
}

enum Job {
    /// A notice for these devices.
    Notice(Notice, Vec<Device>),
    /// Live Activity updates are waiting in `Pusher::activities`.
    Activities,
    /// The pane was read: its notices on these devices are stale.
    Read(PaneId, Vec<Device>),
}

struct ActivityJob {
    device: Device,
    token: String,
    pane: PaneId,
    state: ActivityState,
    end: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub pane: PaneId,
    pub title: String,
    /// The permission prompt to answer, when that is what the agent wants.
    pub permission: Option<u64>,
}

/// Locks: `devices` before `follows`, never the other way round.
pub struct Pusher {
    /// `[push] relay` is set: off, nothing is registered or sent. The push thread checks it
    /// before each post, so what was queued before push went off is dropped.
    enabled: Arc<AtomicBool>,
    /// When each pane last sent a notice (see `notify`).
    last_notice: Mutex<std::collections::HashMap<PaneId, std::time::Instant>>,
    /// The devices each pane sent notices to since it was last read (see `read`).
    notified: Mutex<std::collections::HashMap<PaneId, Vec<String>>>,
    /// Shared with the push thread, which forgets a device the relay no longer knows.
    devices: Arc<Mutex<Vec<Device>>>,
    follows: Arc<Mutex<Vec<Follow>>>,
    /// Live Activity updates not posted yet, the latest per activity. They wait here rather
    /// than in the bounded queue, so a busy relay delays them but never loses one (an end
    /// least of all).
    activities: Arc<Mutex<Vec<ActivityJob>>>,
    /// See `Store::sending`.
    sending: Arc<Mutex<()>>,
    path: PathBuf,
    follows_path: PathBuf,
    tx: Sender<Job>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Pusher {
    /// Devices are kept in `state_dir/push-devices.json`. `enabled`: see [`Pusher::set_enabled`].
    pub fn new(state_dir: &Path, enabled: bool) -> Pusher {
        let path = state_dir.join("push-devices.json");
        let follows_path = state_dir.join("push-activities.json");
        let devices = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let follows = std::fs::read(&follows_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let (tx, rx) = bounded::<Job>(64);
        let pusher = Pusher {
            enabled: Arc::new(AtomicBool::new(enabled)),
            last_notice: Mutex::new(std::collections::HashMap::new()),
            notified: Mutex::new(std::collections::HashMap::new()),
            devices: Arc::new(Mutex::new(devices)),
            follows: Arc::new(Mutex::new(follows)),
            activities: Arc::new(Mutex::new(Vec::new())),
            sending: Arc::new(Mutex::new(())),
            path,
            follows_path,
            tx,
        };
        let activities = Arc::clone(&pusher.activities);
        let on = Arc::clone(&pusher.enabled);
        let store = pusher.store();
        std::thread::Builder::new()
            .name("thurm-push".into())
            .spawn(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(10)))
                    // `post` reads the status and headers of an error itself.
                    .http_status_as_error(false)
                    .build()
                    .into();
                let on = || on.load(Ordering::Relaxed);
                for job in rx {
                    let send =
                        |path, what, devices: &[Device], body: &dyn Fn(&Device) -> String| {
                            for d in devices.iter().take_while(|_| on()) {
                                let _sending = store.sending.lock();
                                // Queued before it was revoked or registered again: not for it now.
                                if !store.registered(d) {
                                    continue;
                                }
                                let r = post(&agent, d, path, &body(d));
                                store.check(d, what, r);
                            }
                        };
                    match &job {
                        Job::Notice(notice, devices) => {
                            send("/v1/push", "push", devices, &|d| body(d, notice))
                        }
                        Job::Read(pane, devices) => {
                            send("/v1/read", "read", devices, &|d| read_body(d, *pane))
                        }
                        Job::Activities => {}
                    }
                    // After every job: a wake-up lost to a full queue is made up by the next.
                    let waiting = std::mem::take(&mut *activities.lock());
                    for job in waiting.into_iter().take_while(|_| on()) {
                        let _sending = store.sending.lock();
                        if !store.registered(&job.device) {
                            continue;
                        }
                        let body =
                            activity_body(&job.device, &job.token, job.pane, &job.state, job.end);
                        let r = post(&agent, &job.device, "/v1/activity", &body);
                        store.check(&job.device, "live activity", r);
                    }
                }
            })
            .expect("push thread");
        pusher
    }

    /// `[push] relay` is set. While it is empty, push is off: registrations are refused and
    /// devices registered earlier get nothing.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn register(&self, device: Device) -> Result<(), String> {
        if !self.enabled() {
            return Err("push notifications are off on this host ([push] relay is empty)".into());
        }
        validate_relay(&device.relay)?;
        if device.device.is_empty() || device.secret.len() < 32 {
            return Err("push registration needs a device id and a secret".into());
        }
        let mut devices = self.devices.lock();
        // The same phone registering again (a new relay, a new code) replaces its old
        // registration, so it is not notified twice.
        let replaced: Vec<String> = devices
            .iter()
            .filter(|d| {
                d.device == device.device || (d.host == device.host && d.name == device.name)
            })
            .map(|d| d.device.clone())
            .collect();
        devices.retain(|d| !replaced.contains(&d.device));
        devices.push(device);
        self.save(&devices)?;
        let mut follows = self.follows.lock();
        let before = follows.len();
        follows.retain(|f| !replaced.contains(&f.device));
        if follows.len() != before {
            save_json(&self.follows_path, &*follows)?;
        }
        Ok(())
    }

    pub fn unregister(&self, device: &str) -> Result<(), String> {
        self.store().forget(&[device.to_owned()])
    }

    fn store(&self) -> Store {
        Store {
            devices: Arc::clone(&self.devices),
            follows: Arc::clone(&self.follows),
            path: self.path.clone(),
            follows_path: self.follows_path.clone(),
            sending: Arc::clone(&self.sending),
        }
    }

    /// Waits for the post in flight, if any: after `unregister` or `revoke_named`, nothing more
    /// reaches the devices they forgot. A post takes at most the relay timeout (10 s).
    pub fn settle(&self) {
        drop(self.sending.lock());
    }

    /// Forgets every device registered under the phone name `name` (spaces as dashes, as its
    /// device key's comment has it) and its activities. Returns their ids.
    pub fn revoke_named(&self, name: &str) -> Result<Vec<String>, String> {
        let gone: Vec<String> = self
            .devices
            .lock()
            .iter()
            .filter(|d| d.name.replace(' ', "-") == name)
            .map(|d| d.device.clone())
            .collect();
        if !gone.is_empty() {
            self.store().forget(&gone)?;
        }
        Ok(gone)
    }

    /// Queues `notice` for every registered device but those in `open` (phones with the app
    /// open, which hear it live). Dropped when the queue is full: a stuck relay must not hold up
    /// the daemon.
    pub fn notify(&self, notice: Notice, open: &[String]) {
        if !self.enabled() {
            return;
        }
        let devices: Vec<Device> = self
            .devices
            .lock()
            .iter()
            .filter(|d| !open.contains(&d.device))
            .cloned()
            .collect();
        if devices.is_empty() || !self.fresh(&notice) {
            return;
        }
        {
            let mut notified = self.notified.lock();
            let sent = notified.entry(notice.pane).or_default();
            for d in &devices {
                if !sent.contains(&d.device) {
                    sent.push(d.device.clone());
                }
            }
        }
        let _ = self.tx.try_send(Job::Notice(notice, devices));
    }

    /// `pane` was read (or closed): the devices it sent notices to since it was last read take
    /// them down, but those in `open` (the app open hears it live). The next notice buzzes
    /// again at once.
    pub fn read(&self, pane: PaneId, open: &[String]) {
        self.last_notice.lock().remove(&pane);
        let Some(notified) = self.notified.lock().remove(&pane) else {
            return;
        };
        if !self.enabled() {
            return;
        }
        let devices: Vec<Device> = self
            .devices
            .lock()
            .iter()
            .filter(|d| notified.contains(&d.device) && !open.contains(&d.device))
            .cloned()
            .collect();
        if !devices.is_empty() {
            let _ = self.tx.try_send(Job::Read(pane, devices));
        }
    }

    /// One buzz per pane per moment: a turn's end often raises two notices together (finished,
    /// then the agent's own "waiting for input"). Permission prompts always go: each has its
    /// own Allow / Deny.
    fn fresh(&self, notice: &Notice) -> bool {
        const QUIET: std::time::Duration = std::time::Duration::from_secs(30);
        let now = std::time::Instant::now();
        let mut last = self.last_notice.lock();
        let recent = last
            .get(&notice.pane)
            .is_some_and(|t| now.duration_since(*t) < QUIET);
        if recent && notice.permission.is_none() {
            return false;
        }
        last.insert(notice.pane, now);
        true
    }

    /// `device` follows `pane` in a Live Activity with push `token` (replacing an earlier one).
    /// The current state goes out at once.
    pub fn follow(
        &self,
        device: &str,
        pane: PaneId,
        token: String,
        state: Option<ActivityState>,
    ) -> Result<(), String> {
        if !self.enabled() {
            return Err("push notifications are off on this host ([push] relay is empty)".into());
        }
        if !self.devices.lock().iter().any(|d| d.device == device) {
            return Err("register the device for push first".into());
        }
        if token.is_empty() || token.len() > 400 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("bad activity token".into());
        }
        {
            let mut follows = self.follows.lock();
            follows.retain(|f| !(f.device == device && f.pane == pane));
            follows.push(Follow {
                device: device.to_owned(),
                pane,
                token,
                last: None,
            });
            save_json(&self.follows_path, &*follows)?;
        }
        self.agent_changed(pane, state, &[]);
        Ok(())
    }

    pub fn unfollow(&self, device: &str, pane: PaneId) -> Result<(), String> {
        let mut follows = self.follows.lock();
        follows.retain(|f| !(f.device == device && f.pane == pane));
        save_json(&self.follows_path, &*follows)
    }

    /// `pane`'s agent is now `state` (None: gone, which ends its activities). Devices in `open`
    /// have the app open and update their activities themselves: their change goes out with
    /// the next one, but an end always goes.
    pub fn agent_changed(&self, pane: PaneId, state: Option<ActivityState>, open: &[String]) {
        if !self.enabled() {
            return;
        }
        let devices = self.devices.lock().clone();
        let mut follows = self.follows.lock();
        if !follows.iter().any(|f| f.pane == pane) {
            return;
        }
        let mut changed = false;
        follows.retain_mut(|f| {
            if f.pane != pane {
                return true;
            }
            let unchanged = match (&f.last, &state) {
                (Some(last), Some(now)) => last.same_as(now),
                _ => false,
            };
            if unchanged {
                return true;
            }
            if let Some(now) = &state
                && open.contains(&f.device)
            {
                // The open app updates its activity itself: that is what it shows now, and
                // what an end goes on showing.
                f.last = Some(now.clone());
                changed = true;
                return true;
            }
            let Some(device) = devices.iter().find(|d| d.device == f.device) else {
                changed = true;
                return false;
            };
            // An ending activity keeps showing its last state, as ended; one that never had a
            // state (the agent was gone when it was followed) still ends.
            let (shown, end) = match &state {
                Some(s) => (s.clone(), false),
                None => {
                    let mut s = f.last.clone().unwrap_or(ActivityState {
                        agent: String::new(),
                        status: String::new(),
                        title: String::new(),
                        since: 0,
                        permission: None,
                    });
                    s.status = "ended".into();
                    s.since = now_secs();
                    (s, true)
                }
            };
            self.queue_activity(ActivityJob {
                device: device.clone(),
                token: f.token.clone(),
                pane,
                state: shown,
                end,
            });
            changed = true;
            f.last = state.clone();
            state.is_some()
        });
        if changed {
            let _ = save_json(&self.follows_path, &*follows);
        }
    }

    /// Hands `job` to the push thread, replacing an update of the same activity still waiting.
    fn queue_activity(&self, job: ActivityJob) {
        {
            let mut waiting = self.activities.lock();
            waiting.retain(|w| !(w.device.device == job.device.device && w.token == job.token));
            waiting.push(job);
        }
        // A full queue means the thread is busy: it posts what waits after its current job.
        let _ = self.tx.try_send(Job::Activities);
    }

    fn save(&self, devices: &[Device]) -> Result<(), String> {
        save_json(&self.path, devices)
    }
}

/// The registered devices and their activities, as saved.
struct Store {
    devices: Arc<Mutex<Vec<Device>>>,
    follows: Arc<Mutex<Vec<Follow>>>,
    path: PathBuf,
    follows_path: PathBuf,
    /// Held by the push thread from checking a post's device to the post's end: waiting for it
    /// (`Pusher::settle`) means no post decided before a revocation is still on its way.
    sending: Arc<Mutex<()>>,
}

impl Store {
    /// Forgets the devices `gone` and their activities.
    fn forget(&self, gone: &[String]) -> Result<(), String> {
        let mut devices = self.devices.lock();
        devices.retain(|d| !gone.contains(&d.device));
        save_json(&self.path, &*devices)?;
        let mut follows = self.follows.lock();
        follows.retain(|f| !gone.contains(&f.device));
        save_json(&self.follows_path, &*follows)
    }

    /// `device` is registered as it is (not revoked, not registered again since).
    fn registered(&self, device: &Device) -> bool {
        self.devices.lock().contains(device)
    }

    /// Forgets `device` and its activities if it is still registered as it is: a registration
    /// made since (another relay, a new secret) stays. Returns whether it was forgotten.
    fn forget_registration(&self, device: &Device) -> Result<bool, String> {
        let mut devices = self.devices.lock();
        let before = devices.len();
        devices.retain(|d| d != device);
        if devices.len() == before {
            return Ok(false);
        }
        save_json(&self.path, &*devices)?;
        let mut follows = self.follows.lock();
        follows.retain(|f| f.device != device.device);
        save_json(&self.follows_path, &*follows)?;
        Ok(true)
    }

    /// After a post for `device`: the relay no longer knowing it (the phone deleted it, turning
    /// notifications off) means it is gone for good, here too.
    fn check(&self, device: &Device, what: &str, result: Result<(), PostError>) {
        match result {
            Ok(()) => {}
            Err(PostError::Gone) => match self.forget_registration(device) {
                Ok(true) => log::info!(
                    "{what} to {}: the relay no longer knows {}; forgot it",
                    device.relay,
                    device.name
                ),
                Ok(false) => {}
                Err(e) => log::warn!("forget push device {}: {e}", device.name),
            },
            Err(PostError::Other(e)) => {
                log::warn!("{what} to {} for {}: {e}", device.relay, device.name);
            }
        }
    }
}

enum PostError {
    /// The relay has no such device: a 404 with `X-Thurm-Device: unknown`. Any other 404 (a
    /// proxy, a route an older relay lacks) is not that.
    Gone,
    Other(String),
}

/// Writes `value` to `path` as JSON, 0600, atomically.
fn save_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), String> {
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        f.write_all(&serde_json::to_vec(value).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }
}

/// The request body the relay gets for a Live Activity update or end.
pub fn activity_body(
    device: &Device,
    token: &str,
    pane: PaneId,
    state: &ActivityState,
    end: bool,
) -> String {
    serde_json::json!({
        "device": device.device,
        "host": device.host,
        "pane": pane,
        "token": token,
        "event": if end { "end" } else { "update" },
        "state": state,
    })
    .to_string()
}

/// A pane's agent as a Live Activity shows it, or None when the pane has no agent.
pub fn activity_state(info: &thurm_proto::PaneInfo, since: u64) -> Option<ActivityState> {
    use thurm_proto::AgentStatus;
    let agent = info.agent.as_ref()?;
    let status = if agent.permission.is_some() {
        "permission"
    } else {
        match agent.status {
            AgentStatus::Working => "working",
            AgentStatus::NeedsInput => "needs_input",
            AgentStatus::Done => "done",
            AgentStatus::Error => "error",
            AgentStatus::Idle => "idle",
        }
    };
    // The agent's own name for the session when it has one ("Fix the sidebar"), else the title.
    let title = agent
        .topic
        .as_deref()
        .map(clean_title)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| clean_title(&info.title));
    Some(ActivityState {
        agent: agent.name.clone(),
        status: status.to_owned(),
        title,
        since,
        permission: agent.permission,
    })
}

pub fn now() -> u64 {
    now_secs()
}

/// HTTPS only, except to this machine (tests, a relay on the same host). The host is parsed:
/// `http://localhost.example.net` is not this machine.
fn validate_relay(relay: &str) -> Result<(), String> {
    let bad = || format!("the push relay must be an https URL: {relay}");
    let uri: ureq::http::Uri = relay.parse().map_err(|_| bad())?;
    let host = uri.host().ok_or_else(bad)?;
    let local = matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1");
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http") if local => Ok(()),
        _ => Err(bad()),
    }
}

/// The request body the relay gets for `notice`, for `device`.
pub fn body(device: &Device, notice: &Notice) -> String {
    let text = if notice.permission.is_some() {
        "asks for permission"
    } else {
        "needs your attention"
    };
    serde_json::json!({
        "device": device.device,
        "host": device.host,
        "pane": notice.pane,
        "title": notice.title,
        "body": text,
        "permission": notice.permission,
    })
    .to_string()
}

/// The request body the relay gets when `pane` was read, for `device`.
pub fn read_body(device: &Device, pane: PaneId) -> String {
    serde_json::json!({
        "device": device.device,
        "host": device.host,
        "pane": pane,
    })
    .to_string()
}

/// `hex(HMAC-SHA256(secret, "<timestamp>.<body>"))`, the `X-Thurm-Signature` header.
pub fn signature(secret: &str, timestamp: u64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn post(agent: &ureq::Agent, device: &Device, path: &str, body: &str) -> Result<(), PostError> {
    let ts = now_secs();
    let url = format!("{}{path}", device.relay.trim_end_matches('/'));
    agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-Thurm-Timestamp", ts.to_string())
        .header("X-Thurm-Signature", signature(&device.secret, ts, body))
        .send(body)
        .map_err(|e| PostError::Other(e.to_string()))
        .and_then(|response| {
            let status = response.status();
            let unknown = response
                .headers()
                .get("X-Thurm-Device")
                .is_some_and(|v| v == "unknown");
            match status.as_u16() {
                200..=299 => Ok(()),
                404 if unknown => Err(PostError::Gone),
                _ => Err(PostError::Other(format!("http status: {status}"))),
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_hmac_sha256_of_timestamp_and_body() {
        // `printf '1.body' | openssl dgst -sha256 -hmac key`
        assert_eq!(
            signature("key", 1, "body"),
            "91b5374b153842ad05b2c4eab9349b8321b14703165bd3fb8b034dfb8be98ae5"
        );
        assert_ne!(signature("key", 1, "body"), signature("key", 2, "body"));
        assert_ne!(signature("key", 1, "body"), signature("other", 1, "body"));
    }

    #[test]
    fn a_turns_notices_buzz_once_but_prompts_always() {
        let dir = std::env::temp_dir().join(format!("thurm-push-fresh-{}", std::process::id()));
        let p = Pusher::new(&dir, true);
        let n = |pane, permission| Notice {
            pane,
            title: "Claude Code".into(),
            permission,
        };
        assert!(p.fresh(&n(1, None)));
        assert!(
            !p.fresh(&n(1, None)),
            "the second notice of the same moment"
        );
        assert!(p.fresh(&n(2, None)), "another pane");
        assert!(p.fresh(&n(1, Some(7))), "a permission prompt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles_lose_their_spinners() {
        assert_eq!(clean_title("⠴ ⠦ | nix-config"), "nix-config");
        assert_eq!(clean_title("thurm ⠙"), "thurm");
        assert_eq!(clean_title("◐ Fix the sidebar"), "Fix the sidebar");
        assert_eq!(clean_title("✳ Translate ⠓⠑⠇⠇⠕"), "Translate");
        assert_eq!(clean_title("Test | nix-config"), "Test | nix-config");
    }

    #[test]
    fn title_changes_alone_are_not_updates() {
        let a = ActivityState {
            agent: "Codex".into(),
            status: "working".into(),
            title: "⠴ nix-config".into(),
            since: 1,
            permission: None,
        };
        let spun = ActivityState {
            title: "⠦ nix-config".into(),
            since: 2,
            ..a.clone()
        };
        assert!(a.same_as(&spun));
        let done = ActivityState {
            status: "done".into(),
            ..a.clone()
        };
        assert!(!a.same_as(&done));
    }

    #[test]
    fn bodies_carry_no_terminal_text() {
        let d = Device {
            device: "d1".into(),
            relay: "https://r".into(),
            secret: "s".repeat(32),
            host: "studio".into(),
            name: "iPhone".into(),
        };
        let b: serde_json::Value = serde_json::from_str(&body(
            &d,
            &Notice {
                pane: 3,
                title: "Claude Code".into(),
                permission: Some(9),
            },
        ))
        .unwrap();
        assert_eq!(b["body"], "asks for permission");
        assert_eq!(b["host"], "studio");
        assert_eq!(b["permission"], 9);
    }

    #[test]
    fn registrations_persist_privately_and_need_https() {
        let dir = std::env::temp_dir().join(format!("thurm-push-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = Pusher::new(&dir, true);
        let mut d = Device {
            device: "d1".into(),
            relay: "http://example.net".into(),
            secret: "s".repeat(32),
            host: "studio".into(),
            name: "iPhone".into(),
        };
        assert!(p.register(d.clone()).is_err());
        d.relay = "https://push.example.net".into();
        p.register(d.clone()).unwrap();
        p.register(d.clone()).unwrap();
        let again = Pusher::new(&dir, true);
        assert_eq!(*again.devices.lock(), vec![d.clone()]);
        // The same phone through another relay replaces it; another phone does not.
        let moved = Device {
            device: "d2".into(),
            relay: "https://other.example.net".into(),
            ..d.clone()
        };
        p.register(moved.clone()).unwrap();
        let ipad = Device {
            device: "d3".into(),
            name: "iPad".into(),
            ..d.clone()
        };
        p.register(ipad.clone()).unwrap();
        assert_eq!(*p.devices.lock(), vec![moved, ipad]);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("push-devices.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        p.unregister("d2").unwrap();
        p.unregister("d3").unwrap();
        assert!(Pusher::new(&dir, true).devices.lock().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What was queued before push went off is not posted after.
    #[test]
    fn turning_push_off_drops_what_is_queued() {
        use std::io::{BufRead, BufReader, Write};
        let dir = std::env::temp_dir().join(format!("thurm-push-queued-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A relay that holds its first answer until told to go on.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (posted, posts) = crossbeam_channel::unbounded();
        let (go, wait) = crossbeam_channel::unbounded::<()>();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 2 {
                    line.clear();
                }
                let _ = posted.send(());
                let _ = wait.recv();
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let p = Pusher::new(&dir, true);
        p.register(Device {
            device: "d1".into(),
            relay,
            secret: "s".repeat(32),
            host: "studio".into(),
            name: "iPhone".into(),
        })
        .unwrap();
        let notice = |pane| Notice {
            pane,
            title: "Codex".into(),
            permission: None,
        };
        p.notify(notice(1), &[]);
        posts
            .recv_timeout(Duration::from_secs(10))
            .expect("the first notice");
        // Queued while the relay holds the first one.
        p.notify(notice(2), &[]);
        p.set_enabled(false);
        go.send(()).unwrap();
        go.send(()).unwrap();
        assert!(
            posts.recv_timeout(Duration::from_millis(500)).is_err(),
            "posted after push went off"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What was queued for a device before it was revoked is not posted after.
    #[test]
    fn revoking_drops_what_is_queued_for_the_device() {
        use std::io::{BufRead, BufReader, Write};
        let dir = std::env::temp_dir().join(format!("thurm-push-revoked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A relay that holds each answer until told to go on.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (posted, posts) = crossbeam_channel::unbounded();
        let (go, wait) = crossbeam_channel::unbounded::<()>();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 2 {
                    line.clear();
                }
                let _ = posted.send(());
                let _ = wait.recv();
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let p = Pusher::new(&dir, true);
        p.register(Device {
            device: "d1".into(),
            relay,
            secret: "s".repeat(32),
            host: "studio".into(),
            name: "iPhone".into(),
        })
        .unwrap();
        let working = ActivityState {
            agent: "Codex".into(),
            status: "working".into(),
            title: "nix-config".into(),
            since: 1,
            permission: None,
        };
        let notice = |pane| Notice {
            pane,
            title: "Codex".into(),
            permission: None,
        };
        p.notify(notice(1), &[]);
        posts
            .recv_timeout(Duration::from_secs(10))
            .expect("the first notice");
        // Queued while the relay holds the first one: a notice and a Live Activity update.
        p.notify(notice(2), &[]);
        p.follow("d1", 3, "aa".into(), Some(working)).unwrap();
        assert_eq!(p.revoke_named("iPhone").unwrap(), ["d1"]);
        // Settling waits for the post in flight, and only for it.
        let p = Arc::new(p);
        let (settled, done) = crossbeam_channel::unbounded();
        let settling = Arc::clone(&p);
        std::thread::spawn(move || {
            settling.settle();
            let _ = settled.send(());
        });
        assert!(
            done.recv_timeout(Duration::from_millis(300)).is_err(),
            "settled with a post in flight"
        );
        for _ in 0..3 {
            go.send(()).unwrap();
        }
        done.recv_timeout(Duration::from_secs(10))
            .expect("never settled");
        assert!(
            posts.recv_timeout(Duration::from_millis(500)).is_err(),
            "posted to a revoked phone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_phones_skip_updates_but_not_ends() {
        let dir = std::env::temp_dir().join(format!("thurm-push-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = Pusher::new(&dir, true);
        for (id, name) in [("d1", "iPhone"), ("d2", "iPad")] {
            p.register(Device {
                device: id.into(),
                relay: "https://push.example.net".into(),
                secret: "s".repeat(32),
                host: "studio".into(),
                name: name.into(),
            })
            .unwrap();
        }
        let working = ActivityState {
            agent: "Codex".into(),
            status: "working".into(),
            title: "nix-config".into(),
            since: 1,
            permission: None,
        };
        p.follow("d1", 4, "aa".into(), Some(working.clone()))
            .unwrap();
        p.follow("d2", 4, "bb".into(), Some(working.clone()))
            .unwrap();
        p.follow("d1", 5, "cc".into(), Some(working.clone()))
            .unwrap();
        let asking = ActivityState {
            status: "permission".into(),
            permission: Some(7),
            ..working.clone()
        };
        let open = ["d1".to_owned()];
        p.agent_changed(4, Some(asking.clone()), &open);
        let last = |device: &str, pane| {
            p.follows
                .lock()
                .iter()
                .find(|f| f.device == device && f.pane == pane)
                .map(|f| f.last.clone())
        };
        assert_eq!(
            last("d1", 4),
            Some(Some(asking.clone())),
            "the open phone updates itself: what it shows"
        );
        assert_eq!(
            last("d2", 4),
            Some(Some(asking)),
            "the other phone is sent the change"
        );
        assert_eq!(
            last("d1", 5),
            Some(Some(working)),
            "another pane's activity"
        );
        p.agent_changed(4, None, &open);
        assert_eq!(last("d1", 4), None, "the open phone's activity ends too");
        assert_eq!(last("d2", 4), None);
        assert!(last("d1", 5).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn revoking_a_phone_forgets_its_devices() {
        let dir = std::env::temp_dir().join(format!("thurm-push-revoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = Pusher::new(&dir, true);
        for (id, host, name) in [
            ("d1", "studio", "Nikola's iPhone"),
            ("d2", "laptop", "Nikola's iPhone"),
            ("d3", "studio", "iPad"),
        ] {
            p.register(Device {
                device: id.into(),
                relay: "https://push.example.net".into(),
                secret: "s".repeat(32),
                host: host.into(),
                name: name.into(),
            })
            .unwrap();
        }
        let working = ActivityState {
            agent: "Codex".into(),
            status: "working".into(),
            title: "nix-config".into(),
            since: 1,
            permission: None,
        };
        p.follow("d1", 4, "aa".into(), Some(working)).unwrap();
        assert_eq!(p.follows.lock().len(), 1);
        let mut gone = p.revoke_named("Nikola's-iPhone").unwrap();
        gone.sort();
        assert_eq!(gone, ["d1", "d2"]);
        let again = Pusher::new(&dir, true);
        let left: Vec<String> = again
            .devices
            .lock()
            .iter()
            .map(|d| d.device.clone())
            .collect();
        assert_eq!(left, ["d3"]);
        assert!(again.follows.lock().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The phone deleted its device at the relay (notifications off): the next post finds it
    /// gone, and the daemon forgets it too.
    #[test]
    fn a_device_the_relay_no_longer_knows_is_forgotten() {
        use std::io::{BufRead, BufReader, Write};
        let dir = std::env::temp_dir().join(format!("thurm-push-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 2 {
                    line.clear();
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nX-Thurm-Device: unknown\r\nContent-Length: 0\r\n\r\n",
                );
            }
        });
        let p = Pusher::new(&dir, true);
        for (id, relay) in [("d1", relay.as_str()), ("d2", "https://push.example.net")] {
            p.register(Device {
                device: id.into(),
                relay: relay.into(),
                secret: "s".repeat(32),
                host: "studio".into(),
                name: id.into(),
            })
            .unwrap();
        }
        p.follow("d1", 4, "aa".into(), None).unwrap();
        let start = std::time::Instant::now();
        p.notify(
            Notice {
                pane: 1,
                title: "Codex".into(),
                permission: Some(1),
            },
            &["d2".to_owned()],
        );
        while p.devices.lock().len() != 1 {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "still registered"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(p.devices.lock()[0].device, "d2", "only the gone device");
        let again = Pusher::new(&dir, true);
        assert_eq!(again.devices.lock().len(), 1, "forgotten on disk too");
        assert!(again.follows.lock().iter().all(|f| f.device != "d1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 404 that is not the relay saying it has no such device (a proxy, a route an older relay
    /// lacks) leaves the registration alone.
    #[test]
    fn a_plain_404_keeps_the_device() {
        use std::io::{BufRead, BufReader, Write};
        let dir = std::env::temp_dir().join(format!("thurm-push-404-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (answered, answers) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 2 {
                    line.clear();
                }
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                let _ = answered.send(());
            }
        });
        let p = Pusher::new(&dir, true);
        p.register(Device {
            device: "d1".into(),
            relay,
            secret: "s".repeat(32),
            host: "studio".into(),
            name: "iPhone".into(),
        })
        .unwrap();
        for pane in 1..=2 {
            p.notify(
                Notice {
                    pane,
                    title: "Codex".into(),
                    permission: Some(1),
                },
                &[],
            );
            answers
                .recv_timeout(Duration::from_secs(10))
                .expect("not posted");
        }
        // Both posts were answered, the second after the first was handled.
        assert_eq!(
            p.devices.lock().len(),
            1,
            "forgot the device on a plain 404"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 404 for a post made before the phone registered again (another relay) is about the old
    /// registration: the new one stays.
    #[test]
    fn a_stale_404_keeps_a_newer_registration() {
        let dir = std::env::temp_dir().join(format!("thurm-push-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = Pusher::new(&dir, true);
        let old = Device {
            device: "d1".into(),
            relay: "https://old.example.net".into(),
            secret: "s".repeat(32),
            host: "studio".into(),
            name: "iPhone".into(),
        };
        p.register(old.clone()).unwrap();
        let new = Device {
            relay: "https://new.example.net".into(),
            ..old.clone()
        };
        p.register(new.clone()).unwrap();
        p.store().check(&old, "push", Err(PostError::Gone));
        assert_eq!(*p.devices.lock(), vec![new.clone()]);
        p.store().check(&new, "push", Err(PostError::Gone));
        assert!(p.devices.lock().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relay answering 200 to every post, handing over each one's body.
    fn capturing_relay() -> (String, crossbeam_channel::Receiver<String>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) <= 2 {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                let _ = reader.read_exact(&mut body);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                let _ = tx.send(String::from_utf8_lossy(&body).into_owned());
            }
        });
        (url, rx)
    }

    /// Every activity ends with what the phone last showed: updates the open app made itself
    /// included, and one followed after its agent was gone too.
    #[test]
    fn every_activity_ends_with_what_it_showed() {
        let dir = std::env::temp_dir().join(format!("thurm-push-end-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (relay, posts) = capturing_relay();
        let p = Pusher::new(&dir, true);
        p.register(Device {
            device: "d1".into(),
            relay,
            secret: "s".repeat(32),
            host: "studio".into(),
            name: "iPhone".into(),
        })
        .unwrap();
        let next = || -> serde_json::Value {
            serde_json::from_str(&posts.recv_timeout(Duration::from_secs(10)).unwrap()).unwrap()
        };
        let working = ActivityState {
            agent: "Codex".into(),
            status: "working".into(),
            title: "nix-config".into(),
            since: 1,
            permission: None,
        };
        p.follow("d1", 4, "aa".into(), Some(working.clone()))
            .unwrap();
        assert_eq!(next()["state"]["status"], "working");
        // The app is open and shows the prompt itself.
        let asking = ActivityState {
            status: "permission".into(),
            title: "Fix the sidebar".into(),
            permission: Some(7),
            ..working
        };
        p.agent_changed(4, Some(asking), &["d1".to_owned()]);
        p.agent_changed(4, None, &["d1".to_owned()]);
        let end = next();
        assert_eq!(end["event"], "end");
        assert_eq!(end["state"]["status"], "ended");
        assert_eq!(end["state"]["title"], "Fix the sidebar");

        // Followed with no agent left: ends at once.
        p.follow("d1", 5, "bb".into(), None).unwrap();
        let end = next();
        assert_eq!(end["event"], "end");
        assert_eq!(end["token"], "bb");
        assert!(p.follows.lock().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_http_only_to_this_machine() {
        for ok in [
            "https://push.example.net",
            "http://127.0.0.1:8080",
            "http://localhost",
            "http://[::1]:9000/",
        ] {
            assert!(validate_relay(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://localhost.example.net",
            "http://127.0.0.1.example.net",
            "http://example.net",
            "ftp://localhost",
            "localhost",
            "",
        ] {
            assert!(validate_relay(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_empty_relay_turns_push_off() {
        let dir = std::env::temp_dir().join(format!("thurm-push-off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = Pusher::new(&dir, true);
        let d = Device {
            device: "d1".into(),
            relay: "https://push.example.net".into(),
            secret: "s".repeat(32),
            host: "studio".into(),
            name: "iPhone".into(),
        };
        p.register(d.clone()).unwrap();
        let working = ActivityState {
            agent: "Codex".into(),
            status: "working".into(),
            title: "nix-config".into(),
            since: 1,
            permission: None,
        };
        p.follow("d1", 4, "ab12".into(), Some(working.clone()))
            .unwrap();
        p.set_enabled(false);
        assert!(p.register(d).is_err(), "registered while off");
        assert!(
            p.follow("d1", 5, "bb".into(), None).is_err(),
            "followed while off"
        );
        p.agent_changed(4, None, &[]);
        let follows = p.follows.lock();
        assert_eq!(follows.len(), 1, "ended while off");
        assert_eq!(follows[0].last, Some(working));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
