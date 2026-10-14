//! Agent notices for the iOS app's lock screen. The app registers its device here (relay URL,
//! device id, secret); each notice is posted to the relay, which holds the APNs key, signed
//! with the device's secret. A notice carries the pane's title and what it wants (permission
//! or attention), never terminal content.
//!
//! The app can also follow an agent in a Live Activity: the activity's push token is kept
//! with the pane, and the agent's state (status, pane title, since when) is posted for it on
//! every change, until the agent leaves or the pane closes.

use std::path::{Path, PathBuf};
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
    Notice(Notice),
    Activity(Box<ActivityJob>),
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

pub struct Pusher {
    /// When each pane last sent a notice (see `notify`).
    last_notice: Mutex<std::collections::HashMap<PaneId, std::time::Instant>>,
    devices: Mutex<Vec<Device>>,
    follows: Mutex<Vec<Follow>>,
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
    /// Devices are kept in `state_dir/push-devices.json`.
    pub fn new(state_dir: &Path) -> Pusher {
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
            last_notice: Mutex::new(std::collections::HashMap::new()),
            devices: Mutex::new(devices),
            follows: Mutex::new(follows),
            path,
            follows_path,
            tx,
        };
        let devices_path = pusher.path.clone();
        std::thread::Builder::new()
            .name("thurm-push".into())
            .spawn(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(10)))
                    .build()
                    .into();
                for job in rx {
                    match job {
                        Job::Notice(notice) => {
                            // Read the devices again: registrations change while this runs.
                            let devices: Vec<Device> = std::fs::read(&devices_path)
                                .ok()
                                .and_then(|b| serde_json::from_slice(&b).ok())
                                .unwrap_or_default();
                            for d in devices {
                                if let Err(e) = post(&agent, &d, "/v1/push", &body(&d, &notice)) {
                                    log::warn!("push to {} for {}: {e}", d.relay, d.name);
                                }
                            }
                        }
                        Job::Activity(job) => {
                            let ActivityJob {
                                device,
                                token,
                                pane,
                                state,
                                end,
                            } = *job;
                            let body = activity_body(&device, &token, pane, &state, end);
                            if let Err(e) = post(&agent, &device, "/v1/activity", &body) {
                                log::warn!(
                                    "live activity to {} for {}: {e}",
                                    device.relay,
                                    device.name
                                );
                            }
                        }
                    }
                }
            })
            .expect("push thread");
        pusher
    }

    pub fn register(&self, device: Device) -> Result<(), String> {
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
        let mut devices = self.devices.lock();
        devices.retain(|d| d.device != device);
        self.save(&devices)?;
        let mut follows = self.follows.lock();
        follows.retain(|f| f.device != device);
        save_json(&self.follows_path, &*follows)
    }

    /// Queues `notice` for every registered device (dropped when the queue is full: a stuck
    /// relay must not hold up the daemon).
    pub fn notify(&self, notice: Notice) {
        if self.devices.lock().is_empty() || !self.fresh(&notice) {
            return;
        }
        let _ = self.tx.try_send(Job::Notice(notice));
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
        self.agent_changed(pane, state, true);
        Ok(())
    }

    pub fn unfollow(&self, device: &str, pane: PaneId) -> Result<(), String> {
        let mut follows = self.follows.lock();
        follows.retain(|f| !(f.device == device && f.pane == pane));
        save_json(&self.follows_path, &*follows)
    }

    /// `pane`'s agent is now `state` (None: gone, which ends its activities). `send`: whether
    /// to post now (false while a phone has the app open and updates its activities itself;
    /// the change goes out with the next one).
    pub fn agent_changed(&self, pane: PaneId, state: Option<ActivityState>, send: bool) {
        let mut follows = self.follows.lock();
        if !follows.iter().any(|f| f.pane == pane) {
            return;
        }
        let devices = self.devices.lock().clone();
        let mut changed = false;
        follows.retain_mut(|f| {
            let unchanged = match (&f.last, &state) {
                (Some(last), Some(now)) => last.same_as(now),
                _ => false,
            };
            if unchanged || !send {
                return true;
            }
            let Some(device) = devices.iter().find(|d| d.device == f.device) else {
                changed = true;
                return false;
            };
            // An ending activity keeps showing its last state, as ended.
            let shown = match &state {
                Some(s) => Some((s.clone(), false)),
                None => f.last.clone().map(|mut s| {
                    s.status = "ended".into();
                    s.since = now_secs();
                    (s, true)
                }),
            };
            if let Some((shown, end)) = shown {
                let _ = self.tx.try_send(Job::Activity(Box::new(ActivityJob {
                    device: device.clone(),
                    token: f.token.clone(),
                    pane,
                    state: shown,
                    end,
                })));
            }
            changed = true;
            f.last = state.clone();
            state.is_some()
        });
        if changed {
            let _ = save_json(&self.follows_path, &*follows);
        }
    }

    fn save(&self, devices: &[Device]) -> Result<(), String> {
        save_json(&self.path, devices)
    }
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

/// HTTPS only, except to this machine (tests, a relay on the same host).
fn validate_relay(relay: &str) -> Result<(), String> {
    let local = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| relay.starts_with(p));
    if relay.starts_with("https://") || local {
        Ok(())
    } else {
        Err(format!("the push relay must use https: {relay}"))
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

fn post(agent: &ureq::Agent, device: &Device, path: &str, body: &str) -> Result<(), String> {
    let ts = now_secs();
    let url = format!("{}{path}", device.relay.trim_end_matches('/'));
    agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-Thurm-Timestamp", ts.to_string())
        .header("X-Thurm-Signature", signature(&device.secret, ts, body))
        .send(body)
        .map(|_| ())
        .map_err(|e| e.to_string())
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
        let p = Pusher::new(&dir);
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
        let p = Pusher::new(&dir);
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
        let again = Pusher::new(&dir);
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
        assert!(Pusher::new(&dir).devices.lock().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
