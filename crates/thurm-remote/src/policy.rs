//! What programs in remote panes may do on this Mac. Enforced by the app on top of whatever
//! the remote daemon's own config allows, so a remote config can only be stricter.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thurm_config::{ClipboardRead, Config, Osc52Mode};

/// Answer to an OSC 52 clipboard read (or write).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    /// Prompt: "devbox wants to read your clipboard — Allow once / Always for devbox / Deny".
    Ask,
}

/// OSC 52 read, or a clipboard tool's paste or type listing, from a pane of `host` (`None`: this
/// Mac's daemon, whose own config already decided, as before remote workspaces).
pub fn clipboard_read(cfg: &Config, host: Option<&str>) -> Decision {
    let Some(host) = host else {
        return Decision::Allow;
    };
    if !matches!(cfg.terminal.osc52, Osc52Mode::Paste | Osc52Mode::CopyPaste) {
        return Decision::Deny;
    }
    match cfg.remote(host).map(|r| r.clipboard_read) {
        Some(ClipboardRead::Always) => Decision::Allow,
        Some(ClipboardRead::Never) => Decision::Deny,
        Some(ClipboardRead::Ask) => Decision::Ask,
        // A host the config no longer lists gets nothing.
        None => Decision::Deny,
    }
}

/// OSC 52 write, or a clipboard tool's copy or clear, from a pane of `host`: follows this Mac's
/// `terminal.osc52`.
pub fn clipboard_write(cfg: &Config, host: Option<&str>) -> Decision {
    if host.is_none() {
        return Decision::Allow;
    }
    if matches!(cfg.terminal.osc52, Osc52Mode::Copy | Osc52Mode::CopyPaste) {
        Decision::Allow
    } else {
        Decision::Deny
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum LinkAction {
    Open,
    /// Confirm before opening (a scheme that may start an app).
    Ask,
    /// A path on the remote host: never opened here; offer to copy it.
    CopyPath {
        path: String,
    },
    /// A file on this Mac: shown in Finder, never opened (it could be an app or a script).
    Reveal {
        path: String,
    },
    /// Not a URL at all.
    Ignore,
    /// A web link to the remote host's own loopback (`localhost`, `127.0.0.1`, `[::1]`,
    /// `0.0.0.0`): open it through a forward of `port` to this Mac.
    Forward {
        port: u16,
    },
}

/// An `http`/`https` URL to a loopback host, split for rewriting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalUrl<'a> {
    /// Lowercase.
    pub scheme: String,
    /// `user:pass@`, or empty.
    pub userinfo: &'a str,
    /// As written (`localhost`, `127.0.0.1`, `[::1]`, `0.0.0.0`).
    pub host: &'a str,
    /// The port written, or the scheme's.
    pub port: u16,
    /// Path, query and fragment, as written.
    pub rest: &'a str,
}

impl<'a> LocalUrl<'a> {
    /// `url` if it is a web link to a loopback host.
    pub fn parse(url: &'a str) -> Option<LocalUrl<'a>> {
        let (scheme, after) = url.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let default = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return None,
        };
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        let (authority, rest) = after.split_at(end);
        let (userinfo, hostport) = match authority.rfind('@') {
            Some(i) => authority.split_at(i + 1),
            None => ("", authority),
        };
        let (host, port) = if hostport.starts_with('[') {
            let close = hostport.find(']')?;
            let (h, p) = hostport.split_at(close + 1);
            (h, p.strip_prefix(':'))
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (hostport, None),
            }
        };
        let port = match port {
            None | Some("") => default,
            Some(p) => p.parse::<u16>().ok().filter(|&p| p != 0)?,
        };
        let loopback = host.eq_ignore_ascii_case("localhost")
            || matches!(host, "127.0.0.1" | "[::1]" | "0.0.0.0");
        loopback.then_some(LocalUrl {
            scheme,
            userinfo,
            host,
            port,
            rest,
        })
    }
}

/// [`link`] when the config can't be read (being rewritten): a remote pane's link to its host's
/// loopback goes through a forward, as by default, never to this Mac's own port of that number,
/// a different program. Anything else as with no config.
pub fn link_without_config(host: Option<&str>, url: &str) -> LinkAction {
    if host.is_some()
        && let Some(local) = LocalUrl::parse(url)
    {
        return LinkAction::Forward { port: local.port };
    }
    link(&Config::default(), host, url)
}

/// [`link`] against the config as last read: with the file unreadable (being rewritten), the
/// last one that loaded still decides, so a host's `forward_links = false` holds meanwhile.
#[derive(Default)]
pub struct Links {
    last: Mutex<Option<Config>>,
}

impl Links {
    pub const fn new() -> Links {
        Links {
            last: Mutex::new(None),
        }
    }

    /// Cmd-click on `url` in a pane of `host`, with `loaded` the config as read now (`None`:
    /// unreadable). With none ever read, [`link_without_config`].
    pub fn link(&self, loaded: Option<Config>, host: Option<&str>, url: &str) -> LinkAction {
        let mut last = self.last.lock();
        if let Some(cfg) = loaded {
            *last = Some(cfg);
        }
        match last.as_ref() {
            Some(cfg) => link(cfg, host, url),
            None => link_without_config(host, url),
        }
    }
}

/// Cmd-click on `url` in a pane of `host`. The URL comes from program output (OSC 8 or text),
/// so only web and mail links open directly, even in this Mac's panes: other schemes can start
/// apps (`smb:`, custom handlers) and a `file:` URL can name an app or a `.command` script. A
/// web link to a remote host's own loopback opens through a forward, unless that host has
/// `forward_links = false`.
pub fn link(cfg: &Config, host: Option<&str>, url: &str) -> LinkAction {
    if let Some(h) = host
        && cfg.remote(h).is_some_and(|r| r.forward_links)
        && let Some(local) = LocalUrl::parse(url)
    {
        return LinkAction::Forward { port: local.port };
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return LinkAction::Ignore;
    };
    let scheme = scheme.to_ascii_lowercase();
    if !scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        || scheme.is_empty()
    {
        return LinkAction::Ignore;
    }
    match scheme.as_str() {
        "http" | "https" => LinkAction::Open,
        "mailto" if host.is_none() => LinkAction::Open,
        "file" => {
            // file://host/path or file:///path: the path part, percent-decoded.
            let rest = rest.strip_prefix("//").unwrap_or(rest);
            let path = match rest.find('/') {
                Some(i) => &rest[i..],
                None => "/",
            };
            let path = percent_decode(path);
            if host.is_none() {
                LinkAction::Reveal { path }
            } else {
                LinkAction::CopyPath { path }
            }
        }
        _ => LinkAction::Ask,
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(osc52: &str, remotes: &str) -> Config {
        Config::parse(&format!("[terminal]\nosc52 = \"{osc52}\"\n{remotes}")).unwrap()
    }

    const HOSTS: &str = r#"
[[remote]]
name = "devbox"
host = "devbox"
[[remote]]
name = "trusted"
host = "t"
clipboard_read = "always"
[[remote]]
name = "locked"
host = "l"
clipboard_read = "never"
"#;

    #[test]
    fn clipboard_reads_prompt_per_host() {
        let c = cfg("copy-paste", HOSTS);
        assert_eq!(clipboard_read(&c, None), Decision::Allow, "local unchanged");
        assert_eq!(clipboard_read(&c, Some("devbox")), Decision::Ask);
        assert_eq!(clipboard_read(&c, Some("trusted")), Decision::Allow);
        assert_eq!(clipboard_read(&c, Some("locked")), Decision::Deny);
        assert_eq!(clipboard_read(&c, Some("gone")), Decision::Deny);
        // The Mac's osc52 setting caps every host.
        let c = cfg("copy", HOSTS);
        assert_eq!(clipboard_read(&c, Some("trusted")), Decision::Deny);
        assert_eq!(clipboard_read(&c, Some("devbox")), Decision::Deny);
    }

    #[test]
    fn clipboard_writes_follow_the_mac() {
        assert_eq!(
            clipboard_write(&cfg("copy", HOSTS), Some("devbox")),
            Decision::Allow
        );
        assert_eq!(
            clipboard_write(&cfg("paste", HOSTS), Some("devbox")),
            Decision::Deny
        );
        assert_eq!(
            clipboard_write(&cfg("disabled", HOSTS), Some("devbox")),
            Decision::Deny
        );
        assert_eq!(
            clipboard_write(&cfg("disabled", HOSTS), None),
            Decision::Allow
        );
    }

    #[test]
    fn loopback_links_from_remote_panes_forward() {
        let c = cfg("copy", HOSTS);
        let r = Some("devbox");
        let fwd = |port| LinkAction::Forward { port };
        assert_eq!(link(&c, r, "http://localhost:5173/x?y#z"), fwd(5173));
        assert_eq!(link(&c, r, "HTTPS://LOCALHOST:8443"), fwd(8443));
        assert_eq!(link(&c, r, "http://127.0.0.1:3000"), fwd(3000));
        assert_eq!(link(&c, r, "http://[::1]:4000/"), fwd(4000));
        assert_eq!(link(&c, r, "http://0.0.0.0:8000"), fwd(8000));
        assert_eq!(link(&c, r, "http://localhost/"), fwd(80));
        assert_eq!(link(&c, r, "https://localhost"), fwd(443));
        assert_eq!(link(&c, r, "http://me:pw@localhost:9000/a"), fwd(9000));
        // Not loopback, or not a web link.
        assert_eq!(
            link(&c, r, "http://localhost.example.com:80"),
            LinkAction::Open
        );
        assert_eq!(link(&c, r, "http://127.0.0.2:80"), LinkAction::Open);
        assert_eq!(link(&c, r, "ftp://localhost:21"), LinkAction::Ask);
        assert_eq!(link(&c, r, "http://localhost:99999"), LinkAction::Open);
        // This Mac's panes, a host that said no, a host the config no longer lists.
        assert_eq!(link(&c, None, "http://localhost:5173"), LinkAction::Open);
        let off = cfg(
            "copy",
            "[[remote]]\nname = \"devbox\"\nhost = \"devbox\"\nforward_links = false\n",
        );
        assert_eq!(link(&off, r, "http://localhost:5173"), LinkAction::Open);
        assert_eq!(
            link(&c, Some("gone"), "http://localhost:5173"),
            LinkAction::Open
        );
    }

    #[test]
    fn with_the_config_unreadable_a_remote_loopback_link_still_forwards() {
        let r = Some("devbox");
        assert_eq!(
            link_without_config(r, "http://localhost:5173/x"),
            LinkAction::Forward { port: 5173 }
        );
        assert_eq!(
            link_without_config(r, "https://example.com"),
            LinkAction::Open
        );
        assert_eq!(
            link_without_config(None, "http://localhost:5173"),
            LinkAction::Open
        );
        assert_eq!(link_without_config(r, "vscode://x"), LinkAction::Ask);
    }

    #[test]
    fn with_the_config_unreadable_the_last_one_read_decides() {
        let r = Some("devbox");
        let links = Links::new();
        // Never read: as by default.
        assert_eq!(
            links.link(None, r, "http://localhost:5173"),
            LinkAction::Forward { port: 5173 }
        );
        let off = cfg(
            "copy",
            "[[remote]]\nname = \"devbox\"\nhost = \"devbox\"\nforward_links = false\n",
        );
        assert_eq!(
            links.link(Some(off), r, "http://localhost:5173"),
            LinkAction::Open
        );
        // Being rewritten: the host still said no.
        assert_eq!(
            links.link(None, r, "http://localhost:5173"),
            LinkAction::Open
        );
        // Read again with the setting back on.
        assert_eq!(
            links.link(Some(cfg("copy", HOSTS)), r, "http://localhost:5173"),
            LinkAction::Forward { port: 5173 }
        );
    }

    #[test]
    fn links_from_remote_panes() {
        let c = cfg("copy", HOSTS);
        let link = |h: Option<&str>, u: &str| link(&c, h, u);
        let r = Some("devbox");
        assert_eq!(link(r, "https://example.com/x"), LinkAction::Open);
        assert_eq!(link(r, "HTTP://example.com"), LinkAction::Open);
        assert_eq!(link(r, "vscode://file/x"), LinkAction::Ask);
        assert_eq!(link(r, "mailto:me@x.org"), LinkAction::Ask);
        assert_eq!(
            link(r, "file://devbox/home/me/My%20File.txt"),
            LinkAction::CopyPath {
                path: "/home/me/My File.txt".into()
            }
        );
        assert_eq!(
            link(r, "file:///etc/hosts"),
            LinkAction::CopyPath {
                path: "/etc/hosts".into()
            }
        );
        assert_eq!(link(r, "not a url"), LinkAction::Ignore);
        // This Mac's panes: files are revealed, other schemes ask.
        assert_eq!(
            link(None, "file:///Users/me/x%20y.command"),
            LinkAction::Reveal {
                path: "/Users/me/x y.command".into()
            }
        );
        assert_eq!(link(None, "https://example.com"), LinkAction::Open);
        assert_eq!(link(None, "mailto:me@x.org"), LinkAction::Open);
        assert_eq!(link(None, "vscode://x"), LinkAction::Ask);
        assert_eq!(link(None, "smb://attacker/share"), LinkAction::Ask);
    }
}
