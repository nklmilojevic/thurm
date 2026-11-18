//! Adding a host from one address (`thurm remote add NAME ADDRESS`, Add Host…): which transport
//! the address means, a sentence for each way reaching it can fail, and whether its daemon is a
//! host saved already under another name.

use thurm_config::{RemoteConfig, TailnetTarget};
use thurm_tailnet_client::ConnectError;

use crate::ssh::{SshError, validate_target};
use crate::tailnet::{CheckError, Checked, OpenError};

/// What an address reaches the host with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// An ssh target: `user@host`, a `Host` alias, or `ssh://user@host[:port]`.
    Ssh(String),
    /// A daemon serving on the tailnet: a `*.ts.net` name, or its `wss://` or
    /// `ws://ADDR:PORT` address.
    Tailnet(String),
}

/// Reads an address: `wss://` and `ws://` addresses and `*.ts.net` names are the tailnet, the
/// rest ssh. `user@` before a tailnet name makes it ssh. A `*.ts.net` name becomes its `wss://`
/// address, which this Mac's Tailscale app reaches as well as Thurm's own node.
pub fn parse_address(address: &str) -> Result<Address, String> {
    let a = address.trim();
    if a.is_empty() {
        return Err(
            "give the host's address: user@host, an ssh Host alias, ssh://user@host:port, or its \
             name on the tailnet (host.tailnet.ts.net)"
                .into(),
        );
    }
    let lower = a.to_ascii_lowercase();
    if lower.starts_with("wss://") || lower.starts_with("ws://") {
        thurm_config::parse_remote_tailnet(a)?;
        return Ok(Address::Tailnet(a.to_owned()));
    }
    if lower.starts_with("ssh://") || a.contains('@') {
        validate_target(a)?;
        return Ok(Address::Ssh(a.to_owned()));
    }
    if a.contains("://") {
        return Err(format!(
            "{a}: give ssh://, wss:// or ws://, or an address without a scheme"
        ));
    }
    // One colon is a port (more is an IPv6 address, for ssh).
    let port = match a.split_once(':') {
        Some((host, port)) if !port.contains(':') => Some((host, port)),
        _ => None,
    };
    let host = port.map_or(a, |(h, _)| h);
    let tailnet_name = host
        .trim_end_matches('.')
        .to_ascii_lowercase()
        .ends_with(".ts.net");
    if let Some((host, port)) = port {
        return Err(if tailnet_name {
            format!(
                "{a}: give ws://{a} for a daemon's listener on the tailnet, or \
                 ssh://{host}:{port} for ssh"
            )
        } else {
            format!("{a}: give ssh://{a} for ssh on that port, or ws://{a} for a daemon's listener")
        });
    }
    if tailnet_name {
        let url = format!("wss://{}", a.trim_end_matches('.'));
        thurm_config::parse_remote_tailnet(&url)?;
        return Ok(Address::Tailnet(url));
    }
    validate_target(a)?;
    Ok(Address::Ssh(a.to_owned()))
}

/// Why a host was not added: each a sentence of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddError {
    /// There is no `ssh` to run on this Mac.
    SshMissing(String),
    /// ssh could not resolve the host's name.
    HostNotFound {
        host: String,
    },
    /// ssh reached the host but could not log in.
    SshLogin {
        target: String,
        detail: String,
    },
    /// ssh doesn't trust the host's key (yet).
    HostKey {
        target: String,
    },
    /// Nothing answered in time.
    Timeout {
        address: String,
    },
    /// Nothing reachable there.
    Unreachable {
        address: String,
        detail: String,
    },
    /// Something answered that is not a Thurm daemon.
    NotThurm {
        address: String,
        detail: String,
    },
    /// The host answers over the tailnet, but its daemon is not running.
    DaemonDown {
        address: String,
    },
    /// The daemon there speaks another protocol than this Mac.
    Protocol {
        address: String,
        theirs: u32,
        ours: u32,
        /// The host's build is the older one (a protocol of ours from before what we need).
        host_older: bool,
    },
    /// Neither Thurm's Tailscale node nor this Mac's Tailscale is on the tailnet.
    NoTailscale {
        detail: String,
    },
    /// The tailnet has no machine of that name.
    NoSuchPeer {
        name: String,
    },
    /// The host's daemon didn't let this Mac's Tailscale account in.
    Refused {
        address: String,
        detail: String,
    },
    /// The daemon there is the one of a host saved already.
    SameHost {
        name: String,
        other: String,
    },
    Other(String),
}

impl AddError {
    /// What the failure said, beyond the sentence, for whoever needs to dig.
    pub fn detail(&self) -> Option<&str> {
        match self {
            AddError::SshMissing(d)
            | AddError::SshLogin { detail: d, .. }
            | AddError::Unreachable { detail: d, .. }
            | AddError::NotThurm { detail: d, .. }
            | AddError::NoTailscale { detail: d }
            | AddError::Refused { detail: d, .. } => Some(d.as_str()).filter(|d| !d.is_empty()),
            _ => None,
        }
    }

    /// A short name of the kind, for the app.
    pub fn kind(&self) -> &'static str {
        match self {
            AddError::SshMissing(_) => "ssh_missing",
            AddError::HostNotFound { .. } => "host_not_found",
            AddError::SshLogin { .. } => "ssh_login",
            AddError::HostKey { .. } => "host_key",
            AddError::Timeout { .. } => "timeout",
            AddError::Unreachable { .. } => "unreachable",
            AddError::NotThurm { .. } => "not_thurm",
            AddError::DaemonDown { .. } => "daemon_down",
            AddError::Protocol { .. } => "protocol",
            AddError::NoTailscale { .. } => "no_tailscale",
            AddError::NoSuchPeer { .. } => "no_such_peer",
            AddError::Refused { .. } => "refused",
            AddError::SameHost { .. } => "same_host",
            AddError::Other(_) => "other",
        }
    }

    /// The sentence, then the detail on a line of its own.
    pub fn with_detail(&self) -> String {
        match self.detail() {
            Some(d) => format!("{self}\n{}", d.trim()),
            None => self.to_string(),
        }
    }
}

impl std::fmt::Display for AddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddError::SshMissing(_) => write!(f, "ssh was not found on this Mac."),
            AddError::HostNotFound { host } => {
                write!(f, "{host} was not found: ssh could not resolve its name.")
            }
            AddError::SshLogin { target, .. } => write!(
                f,
                "ssh could not log in to {target}: Thurm logs in without prompting, so it needs \
                 a key in an agent (the 1Password SSH agent works) or one ssh uses on its own."
            ),
            AddError::HostKey { target } => write!(
                f,
                "ssh does not trust {target}'s host key yet: connect once with `ssh {target}` \
                 in a terminal to check and accept it."
            ),
            AddError::Timeout { address } => write!(f, "Nothing answered at {address} in time."),
            AddError::Unreachable { address, .. } => write!(f, "{address} could not be reached."),
            AddError::NotThurm { address, .. } => write!(
                f,
                "Something answered at {address}, but it is not a Thurm daemon."
            ),
            AddError::DaemonDown { address } => write!(
                f,
                "{address} answers over the tailnet, but its Thurm daemon is not running."
            ),
            AddError::Protocol {
                address,
                theirs,
                ours,
                host_older,
            } => {
                let side = if *host_older {
                    "on the host (this Mac's build is newer)"
                } else {
                    "on this Mac (the host's build is newer)"
                };
                if theirs == ours {
                    write!(
                        f,
                        "The daemon at {address} is an older build of protocol {ours} than this \
                         Mac can talk to: update Thurm {side}."
                    )
                } else {
                    write!(
                        f,
                        "The daemon at {address} speaks protocol {theirs} and this Mac {ours}: \
                         update Thurm {side}."
                    )
                }
            }
            AddError::NoTailscale { .. } => write!(
                f,
                "Tailscale is not connected on this Mac: turn on Thurm's Tailscale (Settings › \
                 Tailscale, or `thurm tailscale on`) or start the Tailscale app."
            ),
            AddError::NoSuchPeer { name } => {
                write!(f, "There is no machine named {name} on your tailnet.")
            }
            AddError::Refused { address, .. } => write!(
                f,
                "The daemon at {address} does not let this Mac's Tailscale account in."
            ),
            AddError::SameHost { name, other } => {
                write!(f, "{name} is the same host as {other}; use {other}.")
            }
            AddError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for AddError {}

fn timed_out(text: &str) -> bool {
    let l = text.to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "did not finish within",
        // thurm-tailnet-client's handshake deadline.
        "no answer within",
        "would block",
        "temporarily unavailable",
    ]
    .iter()
    .any(|w| l.contains(w))
}

/// What a failed ssh to `target` means for adding it.
pub fn classify_ssh(target: &str, e: &SshError) -> AddError {
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    match e {
        SshError::Attention(_) | SshError::Transient(_) => {
            if lower.starts_with("cannot run ssh") {
                AddError::SshMissing(text)
            } else if has(&["could not resolve hostname", "nodename nor servname"]) {
                AddError::HostNotFound {
                    host: crate::tailnet::peer_name(target).to_owned(),
                }
            } else if has(&[
                "host key verification failed",
                "remote host identification has changed",
                "no matching host key",
            ]) {
                AddError::HostKey {
                    target: target.to_owned(),
                }
            } else if has(&[
                "permission denied",
                "passphrase",
                "password",
                "too many authentication failures",
                "verification code",
                "no such identity",
                "tailscale ssh requires an additional check",
            ]) {
                AddError::SshLogin {
                    target: target.to_owned(),
                    detail: text,
                }
            } else if timed_out(&text) {
                AddError::Timeout {
                    address: target.to_owned(),
                }
            } else if has(&[
                "connection refused",
                "no route to host",
                "network is unreachable",
            ]) {
                AddError::Unreachable {
                    address: target.to_owned(),
                    detail: text,
                }
            } else {
                AddError::Other(text)
            }
        }
        SshError::Remote { .. } => AddError::Other(text),
    }
}

/// What a failed [`crate::tailnet::check`] of `address` means for adding it.
pub fn classify_tailnet(address: &str, e: CheckError) -> AddError {
    let address = address.to_owned();
    match e {
        CheckError::Config(m) => AddError::Other(m),
        CheckError::NotThurm { detail, .. } if timed_out(&detail) => AddError::Timeout { address },
        CheckError::NotThurm { detail, .. } => AddError::NotThurm { address, detail },
        CheckError::Open(OpenError::Config(m)) => AddError::Other(m),
        CheckError::Open(OpenError::NoTailnet(detail)) => AddError::NoTailscale { detail },
        CheckError::Open(OpenError::NoSuchPeer(_)) => AddError::NoSuchPeer {
            name: match thurm_config::parse_remote_tailnet(&address) {
                Ok(TailnetTarget::Url { host, .. }) => host,
                Ok(TailnetTarget::Name(n)) => n,
                Err(_) => address,
            },
        },
        CheckError::Open(OpenError::Connect(c)) => match c {
            ConnectError::Unreachable(d) if timed_out(&d) => AddError::Timeout { address },
            ConnectError::Unreachable(detail) => AddError::Unreachable { address, detail },
            ConnectError::Forbidden(detail) => AddError::Refused { address, detail },
            ConnectError::DaemonUnavailable => AddError::DaemonDown { address },
            ConnectError::Protocol(detail) | ConnectError::BadUrl(detail) => {
                AddError::NotThurm { address, detail }
            }
            ConnectError::Daemon(e) => AddError::Refused {
                address,
                detail: e.to_string(),
            },
        },
    }
}

/// A daemon this Mac can't talk to: its protocol differs, or it is a build of ours from before
/// what this Mac needs.
pub fn protocol_problem(address: &str, protocol: u32, build: &str) -> Option<AddError> {
    let ours = thurm_proto::PROTOCOL_VERSION;
    if protocol == ours && !build.is_empty() {
        return None;
    }
    Some(AddError::Protocol {
        address: address.to_owned(),
        theirs: protocol,
        ours,
        host_older: protocol <= ours,
    })
}

/// A tailnet daemon [`crate::tailnet::check`] found, as the host to add.
pub fn tailnet_candidate(c: &Checked) -> Candidate {
    Candidate {
        ssh_target: None,
        tailnet: Some(c.url.clone()),
        daemon_id: c.daemon_id.clone(),
    }
}

/// The host about to be added, as far as it is known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidate {
    pub ssh_target: Option<String>,
    /// Its tailnet address.
    pub tailnet: Option<String>,
    /// Its daemon's identifier, when it said one.
    pub daemon_id: Option<String>,
}

/// Where a tailnet address points, comparable across its spellings.
fn tailnet_key(value: &str) -> Option<(String, u16)> {
    match thurm_config::parse_remote_tailnet(value).ok()? {
        TailnetTarget::Url { host, port, .. } => Some((host.to_ascii_lowercase(), port)),
        TailnetTarget::Name(n) => Some((n.to_ascii_lowercase(), 443)),
    }
}

/// The saved host that `candidate` is, if any: the same daemon (by identifier; one that is
/// unknown on either side never matches), the same ssh target, or the same tailnet address.
/// `known_id` gives a saved host's identifier: the one recorded when it was added, else the
/// one its connection last reported.
pub fn same_host(
    candidate: &Candidate,
    saved: &[RemoteConfig],
    known_id: impl Fn(&RemoteConfig) -> Option<String>,
) -> Option<String> {
    let id = candidate.daemon_id.as_deref().filter(|i| !i.is_empty());
    let target = candidate.ssh_target.as_deref().map(str::trim);
    let tailnet = candidate.tailnet.as_deref().and_then(tailnet_key);
    saved
        .iter()
        .find(|r| {
            id.is_some_and(|id| known_id(r).as_deref() == Some(id))
                || target.is_some_and(|t| r.ssh_target() == Some(t))
                || tailnet
                    .as_ref()
                    .is_some_and(|k| r.tailnet.as_deref().and_then(tailnet_key).as_ref() == Some(k))
        })
        .map(|r| r.name.clone())
}

/// A saved host's daemon identifier: recorded when it was added, else what its connection in
/// the app last reported.
pub fn saved_daemon_id(r: &RemoteConfig) -> Option<String> {
    r.daemon_id
        .clone()
        .or_else(|| crate::tunnel::Status::read(&r.name).and_then(|s| s.daemon_id))
}

/// Refuses adding `name` when `candidate` is a host of `saved` already.
pub fn check_new(
    name: &str,
    candidate: &Candidate,
    saved: &[RemoteConfig],
) -> Result<(), AddError> {
    match same_host(candidate, saved, saved_daemon_id) {
        Some(other) => Err(AddError::SameHost {
            name: name.to_owned(),
            other,
        }),
        None => Ok(()),
    }
}

/// Refuses giving saved host `name` a destination (`candidate`, an ssh target or a tailnet
/// address) that another saved host has, as [`check_new`] refuses adding it.
pub fn check_edit(
    name: &str,
    candidate: &Candidate,
    saved: &[RemoteConfig],
) -> Result<(), AddError> {
    let others: Vec<RemoteConfig> = saved.iter().filter(|r| r.name != name).cloned().collect();
    check_new(name, candidate, &others)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_name_their_transport() {
        for (address, want) in [
            ("me@devbox", Address::Ssh("me@devbox".into())),
            ("devbox", Address::Ssh("devbox".into())),
            (" devbox ", Address::Ssh("devbox".into())),
            ("10.50.0.24", Address::Ssh("10.50.0.24".into())),
            (
                "ssh://me@devbox:2222",
                Address::Ssh("ssh://me@devbox:2222".into()),
            ),
            ("devbox.local", Address::Ssh("devbox.local".into())),
            ("fe80::1", Address::Ssh("fe80::1".into())),
            // A user makes a tailnet name an ssh target (Tailscale SSH, or sshd over it).
            (
                "me@bastion.tail2177a1.ts.net",
                Address::Ssh("me@bastion.tail2177a1.ts.net".into()),
            ),
            (
                "bastion.tail2177a1.ts.net",
                Address::Tailnet("wss://bastion.tail2177a1.ts.net".into()),
            ),
            (
                "Bastion.Tail2177a1.TS.NET.",
                Address::Tailnet("wss://Bastion.Tail2177a1.TS.NET".into()),
            ),
            (
                "wss://bastion.tail2177a1.ts.net",
                Address::Tailnet("wss://bastion.tail2177a1.ts.net".into()),
            ),
            (
                "wss://bastion.tail2177a1.ts.net/v1/connect",
                Address::Tailnet("wss://bastion.tail2177a1.ts.net/v1/connect".into()),
            ),
            (
                "ws://100.64.0.1:7297",
                Address::Tailnet("ws://100.64.0.1:7297".into()),
            ),
        ] {
            assert_eq!(parse_address(address), Ok(want), "{address}");
        }
    }

    #[test]
    fn ambiguous_and_bad_addresses_say_what_to_give() {
        // A port without a scheme: ssh takes none, a listener needs ws://.
        let e = parse_address("devbox:2222").unwrap_err();
        assert!(
            e.contains("ssh://devbox:2222") && e.contains("ws://devbox:2222"),
            "{e}"
        );
        let e = parse_address("bastion.tail1.ts.net:7297").unwrap_err();
        assert!(
            e.contains("ws://bastion.tail1.ts.net:7297") && e.contains("ssh://"),
            "{e}"
        );
        // A ws:// address needs its port.
        assert!(parse_address("ws://100.64.0.1").is_err());
        for bad in [
            "",
            "  ",
            "https://bastion.ts.net",
            "-oProxyCommand=x",
            "me@a b",
        ] {
            assert!(parse_address(bad).is_err(), "{bad:?}");
        }
    }

    fn saved(name: &str, host: &str, tailnet: Option<&str>, id: Option<&str>) -> RemoteConfig {
        RemoteConfig {
            name: name.into(),
            host: host.into(),
            socket: None,
            enabled: true,
            clipboard_read: Default::default(),
            tailnet: tailnet.map(Into::into),
            forward_links: true,
            daemon_id: id.map(Into::into),
        }
    }

    #[test]
    fn the_same_daemon_under_a_second_name_is_found() {
        let list = vec![
            saved("devbox", "me@devbox", None, Some("aaaa")),
            saved(
                "bastion",
                "",
                Some("wss://bastion.tail1.ts.net/v1/connect"),
                None,
            ),
            saved("mini", "me@mini", None, None),
        ];
        // Only the connection knows mini's identifier.
        let known = |r: &RemoteConfig| {
            r.daemon_id
                .clone()
                .or_else(|| (r.name == "mini").then(|| "cccc".to_owned()))
        };
        let by_id = |id: &str| Candidate {
            daemon_id: Some(id.into()),
            ..Default::default()
        };
        assert_eq!(
            same_host(&by_id("aaaa"), &list, known).as_deref(),
            Some("devbox")
        );
        assert_eq!(
            same_host(&by_id("cccc"), &list, known).as_deref(),
            Some("mini")
        );
        assert_eq!(same_host(&by_id("bbbb"), &list, known), None);
        // An unknown identifier matches nothing, not even another unknown one.
        assert_eq!(same_host(&Candidate::default(), &list, known), None);
        assert_eq!(same_host(&by_id(""), &list, known), None);
        // The same ssh target, or the same tailnet address however it is spelled.
        let ssh = Candidate {
            ssh_target: Some(" me@devbox ".into()),
            ..Default::default()
        };
        assert_eq!(same_host(&ssh, &list, known).as_deref(), Some("devbox"));
        let tailnet = Candidate {
            tailnet: Some("wss://Bastion.tail1.ts.net".into()),
            ..Default::default()
        };
        assert_eq!(
            same_host(&tailnet, &list, known).as_deref(),
            Some("bastion")
        );
        let other = Candidate {
            ssh_target: Some("you@devbox".into()),
            tailnet: Some("ws://100.64.0.9:7297".into()),
            daemon_id: Some("dddd".into()),
        };
        assert_eq!(same_host(&other, &list, known), None);
        let e = AddError::SameHost {
            name: "box2".into(),
            other: "devbox".into(),
        };
        assert_eq!(
            e.to_string(),
            "box2 is the same host as devbox; use devbox."
        );
    }

    #[test]
    fn an_edit_cannot_point_at_another_saved_host() {
        let list = vec![
            saved("devbox", "me@devbox", None, None),
            saved(
                "bastion",
                "",
                Some("wss://bastion.tail1.ts.net/v1/connect"),
                None,
            ),
        ];
        let ssh = |t: &str| Candidate {
            ssh_target: Some(t.into()),
            ..Default::default()
        };
        // bastion, added over the tailnet, can't take devbox's ssh target.
        assert!(matches!(
            check_edit("bastion", &ssh("me@devbox"), &list),
            Err(AddError::SameHost { name, other }) if name == "bastion" && other == "devbox"
        ));
        // Its own target, kept or set again, is no clash; nor is a new one.
        assert!(check_edit("devbox", &ssh("me@devbox"), &list).is_ok());
        assert!(check_edit("bastion", &ssh("me@bastion"), &list).is_ok());
        let tailnet = Candidate {
            tailnet: Some("bastion.tail1.ts.net".into()),
            ..Default::default()
        };
        assert!(matches!(
            check_edit("devbox", &tailnet, &list),
            Err(AddError::SameHost { other, .. }) if other == "bastion"
        ));
    }

    #[test]
    fn ssh_failures_each_have_their_sentence() {
        let kind = |e: SshError| classify_ssh("me@devbox", &e).kind();
        assert_eq!(
            kind(SshError::Attention(
                "cannot run ssh: No such file or directory".into()
            )),
            "ssh_missing"
        );
        let e = classify_ssh(
            "me@devbox",
            &SshError::Attention(
                "ssh: Could not resolve hostname devbox: nodename nor servname provided".into(),
            ),
        );
        assert_eq!(
            e.to_string(),
            "devbox was not found: ssh could not resolve its name."
        );
        assert_eq!(
            kind(SshError::Attention(
                "me@devbox: Permission denied (publickey).".into()
            )),
            "ssh_login"
        );
        assert_eq!(
            kind(SshError::Attention("Host key verification failed.".into())),
            "host_key"
        );
        assert_eq!(
            kind(SshError::Transient(
                "ssh: connect to host devbox port 22: Operation timed out".into()
            )),
            "timeout"
        );
        assert_eq!(
            kind(SshError::Transient("ssh did not finish within 30 s".into())),
            "timeout"
        );
        assert_eq!(
            kind(SshError::Transient(
                "ssh: connect to host devbox port 22: Connection refused".into()
            )),
            "unreachable"
        );
        let login = classify_ssh(
            "me@devbox",
            &SshError::Attention("Permission denied (publickey).".into()),
        );
        assert!(
            login
                .with_detail()
                .ends_with("\nPermission denied (publickey).")
        );
    }

    #[test]
    fn tailnet_failures_each_have_their_sentence() {
        let a = "bastion.tail1.ts.net";
        let kind = |e| classify_tailnet(a, e).kind();
        assert_eq!(
            kind(CheckError::Open(OpenError::NoTailnet("no node".into()))),
            "no_tailscale"
        );
        assert_eq!(
            kind(CheckError::Open(OpenError::NoSuchPeer("no peer".into()))),
            "no_such_peer"
        );
        assert_eq!(
            classify_tailnet(a, CheckError::Open(OpenError::NoSuchPeer("x".into()))).to_string(),
            "There is no machine named bastion.tail1.ts.net on your tailnet."
        );
        assert_eq!(
            kind(CheckError::NotThurm {
                url: a.into(),
                detail: "unexpected Ok".into()
            }),
            "not_thurm"
        );
        assert_eq!(
            kind(CheckError::NotThurm {
                url: a.into(),
                detail: "Resource temporarily unavailable (os error 35)".into()
            }),
            "timeout"
        );
        // thurm-tailnet-client's handshake deadline, as it words it.
        assert_eq!(
            kind(CheckError::Open(OpenError::Connect(
                ConnectError::Unreachable("no answer within 8 seconds".into())
            ))),
            "timeout"
        );
        assert_eq!(
            kind(CheckError::Open(OpenError::Connect(
                ConnectError::Forbidden("not the owner".into())
            ))),
            "refused"
        );
        assert_eq!(
            kind(CheckError::Open(OpenError::Connect(
                ConnectError::DaemonUnavailable
            ))),
            "daemon_down"
        );
    }

    #[test]
    fn a_protocol_mismatch_says_which_side_to_update() {
        let ours = thurm_proto::PROTOCOL_VERSION;
        assert_eq!(protocol_problem("x", ours, "0.3.0"), None);
        let older = protocol_problem("x", ours - 1, "").unwrap().to_string();
        assert!(older.contains("update Thurm on the host"), "{older}");
        let newer = protocol_problem("x", ours + 1, "0.9.0")
            .unwrap()
            .to_string();
        assert!(newer.contains("update Thurm on this Mac"), "{newer}");
        // Our protocol, from a build before what this Mac needs.
        let same = protocol_problem("x", ours, "").unwrap().to_string();
        assert!(
            same.contains("older build") && same.contains("on the host"),
            "{same}"
        );
    }
}
