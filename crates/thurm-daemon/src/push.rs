//! Agent notices for the iOS app's lock screen. The app registers its device here (relay URL,
//! device id, secret); each notice is posted to the relay, which holds the APNs key, signed
//! with the device's secret. A notice carries the pane's title and what it wants (permission
//! or attention), never terminal content.

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

#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub pane: PaneId,
    pub title: String,
    /// The permission prompt to answer, when that is what the agent wants.
    pub permission: Option<u64>,
}

pub struct Pusher {
    devices: Mutex<Vec<Device>>,
    path: PathBuf,
    tx: Sender<Notice>,
}

impl Pusher {
    /// Devices are kept in `state_dir/push-devices.json`.
    pub fn new(state_dir: &Path) -> Pusher {
        let path = state_dir.join("push-devices.json");
        let devices = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let (tx, rx) = bounded::<Notice>(64);
        let pusher = Pusher {
            devices: Mutex::new(devices),
            path,
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
                for notice in rx {
                    // Read the devices again: registrations change while this runs.
                    let devices: Vec<Device> = std::fs::read(&devices_path)
                        .ok()
                        .and_then(|b| serde_json::from_slice(&b).ok())
                        .unwrap_or_default();
                    for d in devices {
                        if let Err(e) = post(&agent, &d, &notice) {
                            log::warn!("push to {} for {}: {e}", d.relay, d.name);
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
        devices.retain(|d| d.device != device.device);
        devices.push(device);
        self.save(&devices)
    }

    pub fn unregister(&self, device: &str) -> Result<(), String> {
        let mut devices = self.devices.lock();
        devices.retain(|d| d.device != device);
        self.save(&devices)
    }

    /// Queues `notice` for every registered device (dropped when the queue is full: a stuck
    /// relay must not hold up the daemon).
    pub fn notify(&self, notice: Notice) {
        if !self.devices.lock().is_empty() {
            let _ = self.tx.try_send(notice);
        }
    }

    fn save(&self, devices: &[Device]) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        f.write_all(&serde_json::to_vec(devices).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }
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

fn post(agent: &ureq::Agent, device: &Device, notice: &Notice) -> Result<(), String> {
    let body = body(device, notice);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let url = format!("{}/v1/push", device.relay.trim_end_matches('/'));
    agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-Thurm-Timestamp", ts.to_string())
        .header("X-Thurm-Signature", signature(&device.secret, ts, &body))
        .send(&body)
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
        assert_eq!(*again.devices.lock(), vec![d]);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("push-devices.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        again.unregister("d1").unwrap();
        assert!(Pusher::new(&dir).devices.lock().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
