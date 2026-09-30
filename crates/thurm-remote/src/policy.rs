//! What programs in remote panes may do on this Mac. Enforced by the app on top of whatever
//! the remote daemon's own config allows, so a remote config can only be stricter.

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

/// OSC 52 read from a pane of `host` (`None`: this Mac's daemon, whose own config already
/// decided, as before remote workspaces).
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

/// OSC 52 write from a pane of `host`: follows this Mac's `terminal.osc52`.
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
}

/// Cmd-click on `url` in a pane of `host`. The URL comes from program output (OSC 8 or text),
/// so only web and mail links open directly, even in this Mac's panes: other schemes can start
/// apps (`smb:`, custom handlers) and a `file:` URL can name an app or a `.command` script.
pub fn link(host: Option<&str>, url: &str) -> LinkAction {
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
    fn links_from_remote_panes() {
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
