//! Serving the daemon over Tailscale.
//!
//! The `thurm-tailnet` helper holds the Tailscale side: an embedded node (tsnet) with its own
//! name on the tailnet, or this host's own Tailscale. It admits a caller only after asking
//! Tailscale who it is, then connects to the daemon's socket for it, starting with an
//! [ingress preamble](write_ingress) that says who the caller is. The types here are shared by
//! the daemon, the helper (as JSON) and the apps.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// Opens an ingress preamble. As a frame length it would be over [`crate::codec::MAX_MESSAGE`],
/// so a connection that starts with it can't be a client speaking frames.
pub const INGRESS_MAGIC: u32 = u32::MAX;

/// The longest ingress preamble the daemon reads.
pub const MAX_INGRESS: usize = 64 * 1024;

/// Path of the WebSocket the helper serves the daemon's protocol on.
pub const CONNECT_PATH: &str = "/v1/connect";

/// Who is on the other end of a connection.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct Principal {
    pub kind: PrincipalKind,
    /// A readable name for logs and lists, never an identifier: compare `uid` for a peer and
    /// `user_id` for a tailnet caller.
    pub display: String,
    /// Local peers only.
    pub uid: Option<u32>,
    /// Tailnet callers only: the Tailscale user, its login (usually an email address) and the
    /// calling device. A non-empty `tags` means a tagged machine rather than a person.
    pub user_id: Option<i64>,
    pub login_name: Option<String>,
    pub node: Option<String>,
    pub tags: Vec<String>,
    pub remote_addr: Option<String>,
    /// Which listener admitted it: `node` (the embedded node), `host` (this host's Tailscale)
    /// or `serve` (behind `tailscale serve`).
    pub via: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A process of this machine on the daemon's socket.
    #[default]
    Peer,
    /// A tailnet caller Tailscale identified.
    Tailscale,
}

/// The state of serving over Tailscale, as `TailscaleStatus` answers and
/// `TailscaleStatusChanged` reports.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TailscaleStatus {
    pub state: TailscaleState,
    /// Why the state is `Error`, or a warning worth showing next to it.
    pub error: Option<String>,
    /// The node's requested name.
    pub hostname: String,
    /// Its full name on the tailnet (`thurm.tail1234.ts.net`), once it has one.
    pub dns_name: Option<String>,
    /// The sign-in link while `NeedsLogin`. An empty link right after asking for one is not an
    /// error: it arrives later in a status event.
    pub auth_url: Option<String>,
    /// The account that enrolled the node.
    pub account: Option<TailscaleAccount>,
    /// The admin-defined tailnet name (the tailnet domain when none is set).
    pub tailnet_name: Option<String>,
    /// Whether any tailnet member may connect; otherwise only `account`.
    pub allow_tailnet_members: bool,
    /// ACL tags the node runs with.
    pub tags: Vec<String>,
    pub https: TailscaleHttps,
    /// Where a client connects: `wss://<dns_name>/v1/connect` once HTTPS is ready.
    pub url: Option<String>,
    /// Endpoints served through this host's own Tailscale (`ws://100.x.y.z:7297/v1/connect`).
    pub host_urls: Vec<String>,
    /// The Unix socket served for `tailscale serve`, when on.
    pub serve_socket: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TailscaleState {
    /// Not serving over Tailscale.
    #[default]
    Off,
    Starting,
    /// Waiting for someone to open `auth_url` and sign in.
    NeedsLogin,
    /// Signed in; a tailnet administrator must approve the device.
    NeedsMachineAuth,
    Running,
    /// The helper or node stopped; it is restarted.
    Stopped,
    Error,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TailscaleAccount {
    /// Usually an email address; for display.
    pub login_name: String,
    pub display_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct TailscaleHttps {
    pub state: HttpsState,
    pub error: Option<String>,
}

#[derive(Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpsState {
    /// The node isn't running.
    #[default]
    Unavailable,
    /// MagicDNS is off for the tailnet, so the node has no name to hold a certificate.
    MagicDnsOff,
    /// HTTPS certificates are off for the tailnet.
    CertsOff,
    Provisioning,
    Ready,
}

/// [`HttpsState`]'s wire form, which frames (postcard) keep.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum HttpsStateRepr {
    Unavailable,
    MagicDnsOff,
    CertsOff,
    Provisioning,
    Ready,
}

impl From<HttpsStateRepr> for HttpsState {
    fn from(r: HttpsStateRepr) -> Self {
        match r {
            HttpsStateRepr::Unavailable => Self::Unavailable,
            HttpsStateRepr::MagicDnsOff => Self::MagicDnsOff,
            HttpsStateRepr::CertsOff => Self::CertsOff,
            HttpsStateRepr::Provisioning => Self::Provisioning,
            HttpsStateRepr::Ready => Self::Ready,
        }
    }
}

/// In the helper's JSON an empty, null or unknown state reads as `Unavailable`, so it can't
/// cost the whole status.
impl<'de> Deserialize<'de> for HttpsState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if !d.is_human_readable() {
            return HttpsStateRepr::deserialize(d).map(Into::into);
        }
        let Some(s) = Option::<String>::deserialize(d)? else {
            return Ok(Self::Unavailable);
        };
        let de = serde::de::value::StrDeserializer::<serde::de::value::Error>::new(&s);
        Ok(HttpsStateRepr::deserialize(de).map_or(Self::Unavailable, Into::into))
    }
}

/// Starts a connection on the daemon's socket for a tailnet caller: [`INGRESS_MAGIC`], the
/// JSON length, then `principal` as JSON. The daemon reads it before the first frame.
pub fn write_ingress<W: Write>(w: &mut W, principal: &Principal) -> io::Result<()> {
    let json = serde_json::to_vec(principal)?;
    if json.len() > MAX_INGRESS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ingress preamble too large",
        ));
    }
    w.write_all(&INGRESS_MAGIC.to_le_bytes())?;
    w.write_all(&(json.len() as u32).to_le_bytes())?;
    w.write_all(&json)?;
    w.flush()
}

/// Reads the rest of an ingress preamble whose [`INGRESS_MAGIC`] was already read.
pub fn read_ingress<R: Read>(r: &mut R) -> io::Result<Principal> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_INGRESS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ingress preamble too large",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let mut principal: Principal = serde_json::from_slice(&buf)?;
    // Only tailnet callers come in this way; a preamble can't claim a local peer's uid.
    principal.kind = PrincipalKind::Tailscale;
    principal.uid = None;
    Ok(principal)
}

/// Whether `name` can be the node's name on the tailnet: one DNS label of 1 to 63 ASCII
/// letters, digits or hyphens, with no leading or trailing hyphen.
pub fn valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Whether `tag` is an ACL tag (`tag:name`).
pub fn valid_tag(tag: &str) -> bool {
    tag.strip_prefix("tag:").is_some_and(|name| {
        !name.is_empty()
            && name.starts_with(|c: char| c.is_ascii_alphabetic())
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingress_round_trips_and_is_not_a_frame() {
        let p = Principal {
            kind: PrincipalKind::Tailscale,
            display: "me@example.com".into(),
            user_id: Some(42),
            login_name: Some("me@example.com".into()),
            node: Some("phone".into()),
            via: Some("node".into()),
            ..Default::default()
        };
        let mut buf = Vec::new();
        write_ingress(&mut buf, &p).unwrap();
        let magic = u32::from_le_bytes(buf[..4].try_into().unwrap());
        assert_eq!(magic, INGRESS_MAGIC);
        assert!(magic as usize > crate::codec::MAX_MESSAGE);
        assert_eq!(read_ingress(&mut &buf[4..]).unwrap(), p);
    }

    #[test]
    fn ingress_cannot_claim_a_peer() {
        let json = br#"{"kind":"peer","uid":0,"display":"root"}"#;
        let mut buf = (json.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(json);
        let p = read_ingress(&mut &buf[..]).unwrap();
        assert_eq!(p.kind, PrincipalKind::Tailscale);
        assert_eq!(p.uid, None);
    }

    #[test]
    fn oversized_ingress_is_refused() {
        let buf = ((MAX_INGRESS + 1) as u32).to_le_bytes();
        assert!(read_ingress(&mut &buf[..]).is_err());
    }

    #[test]
    fn status_reads_the_helpers_json() {
        let s: TailscaleStatus = serde_json::from_str(
            r#"{"state":"needs_login","hostname":"thurm","auth_url":"https://login.tailscale.com/a/x",
                "https":{"state":"magic_dns_off"}}"#,
        )
        .unwrap();
        assert_eq!(s.state, TailscaleState::NeedsLogin);
        assert_eq!(s.https.state, HttpsState::MagicDnsOff);
        assert!(s.tags.is_empty());
    }

    #[test]
    fn status_survives_an_unknown_https_state() {
        for https in [r#""""#, r#""quantum""#, "null"] {
            let s: TailscaleStatus = serde_json::from_str(&format!(
                r#"{{"state":"running","hostname":"thurm","https":{{"state":{https}}},
                    "host_urls":["ws://100.64.0.1:7297/v1/connect"]}}"#
            ))
            .unwrap();
            assert_eq!(s.state, TailscaleState::Running);
            assert_eq!(s.https.state, HttpsState::Unavailable);
            assert_eq!(s.host_urls.len(), 1);
        }
    }

    #[test]
    fn https_state_round_trips_in_frames() {
        for state in [
            HttpsState::Unavailable,
            HttpsState::MagicDnsOff,
            HttpsState::CertsOff,
            HttpsState::Provisioning,
            HttpsState::Ready,
        ] {
            let bytes = postcard::to_stdvec(&state).unwrap();
            assert_eq!(postcard::from_bytes::<HttpsState>(&bytes).unwrap(), state);
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<HttpsState>(&json).unwrap(), state);
        }
    }

    #[test]
    fn hostnames_and_tags() {
        assert!(valid_hostname("thurm"));
        assert!(valid_hostname("my-mac-2"));
        assert!(!valid_hostname(""));
        assert!(!valid_hostname("-mac"));
        assert!(!valid_hostname("mac-"));
        assert!(!valid_hostname("my.mac"));
        assert!(!valid_hostname(&"a".repeat(64)));
        assert!(valid_tag("tag:thurm"));
        assert!(!valid_tag("thurm"));
        assert!(!valid_tag("tag:"));
        assert!(!valid_tag("tag:1x"));
    }
}
