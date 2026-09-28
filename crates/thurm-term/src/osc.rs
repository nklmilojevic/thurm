//! Interpretation of the OSC sequences intercepted by [`crate::filter`].

use std::collections::HashMap;

use base64::Engine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// Working directory reported by the shell (OSC 633;P;Cwd; the others are
    /// libghostty-vt's).
    Cwd(String),
    /// The shell's `$PATH` (OSC 633;P;ThurmPath=…, from Thurm's shell integration), for
    /// completing command names and running completion generators like the shell would.
    ShellPath(String),
    Notify {
        title: String,
        body: String,
    },
    /// OSC 133;A — a prompt is about to be drawn. `redraw`: its `redraw=` option (kitty's),
    /// whether the shell redraws the prompt itself on resize.
    PromptStart {
        redraw: Option<bool>,
    },
    /// OSC 133;B — prompt drawn, user input starts.
    InputStart,
    /// OSC 133;C — the command line was submitted and is running.
    CommandStart,
    /// OSC 133;D[;exit] — the command finished.
    CommandFinished(Option<i32>),
    /// OSC 633;E — the command line being executed.
    CommandLine(String),
}

/// State for multi-part kitty (OSC 99) notifications.
#[derive(Default, Debug)]
pub struct OscState {
    kitty_pending: HashMap<String, (String, String)>,
}

impl OscState {
    pub fn parse(&mut self, body: &[u8]) -> Option<OscEvent> {
        let text = String::from_utf8_lossy(body);
        let (num, rest) = match text.find(';') {
            Some(i) => (&text[..i], &text[i + 1..]),
            None => (&text[..], ""),
        };
        match num {
            "99" => self.parse_kitty_notification(rest),
            "133" | "633" => parse_prompt_mark(num, rest),
            _ => None,
        }
    }

    /// `OSC 99 ; metadata ; payload` — metadata is `key=value` pairs separated by `:`.
    fn parse_kitty_notification(&mut self, rest: &str) -> Option<OscEvent> {
        let (meta, payload) = rest.split_once(';').unwrap_or((rest, ""));
        let mut id = String::new();
        let mut done = true;
        let mut kind = "title";
        let mut base64 = false;
        for kv in meta.split(':').filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            match k {
                "i" => id = v.to_owned(),
                "d" => done = v != "0",
                "p" => {
                    kind = if v == "body" {
                        "body"
                    } else if v == "title" {
                        "title"
                    } else {
                        "?"
                    }
                }
                "e" => base64 = v == "1",
                _ => {}
            }
        }
        if kind == "?" {
            // Queries / icons / buttons: not supported, ignore.
            return None;
        }
        let payload = if base64 {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(payload.trim())
                .ok()?;
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            payload.to_owned()
        };
        let entry = self.kitty_pending.entry(id.clone()).or_default();
        if kind == "title" {
            entry.0.push_str(&payload);
        } else {
            entry.1.push_str(&payload);
        }
        if !done {
            return None;
        }
        let (title, body) = self.kitty_pending.remove(&id).unwrap_or_default();
        if title.is_empty() && body.is_empty() {
            return None;
        }
        Some(OscEvent::Notify { title, body })
    }
}

/// Option on prompt marks Thurm writes when replaying history: they recreate libghostty-vt's
/// prompt marks in a copy of the terminal, and are no events.
pub const REPLAY_MARK: &str = "thurm=replay";

fn parse_prompt_mark(num: &str, rest: &str) -> Option<OscEvent> {
    if rest.split(';').any(|kv| kv == REPLAY_MARK) {
        return None;
    }
    let mut parts = rest.split(';');
    match parts.next()? {
        "A" => Some(OscEvent::PromptStart {
            redraw: parts
                .find_map(|kv| kv.strip_prefix("redraw="))
                .map(|v| v == "1"),
        }),
        "B" => Some(OscEvent::InputStart),
        "C" => Some(OscEvent::CommandStart),
        "D" => Some(OscEvent::CommandFinished(
            parts.next().and_then(|s| s.parse().ok()),
        )),
        "E" if num == "633" => Some(OscEvent::CommandLine(unescape_633(
            parts.next().unwrap_or(""),
        ))),
        "P" => {
            let prop = parts.next()?;
            if let Some(p) = prop.strip_prefix("ThurmPath=") {
                return Some(OscEvent::ShellPath(unescape_633(p)));
            }
            prop.strip_prefix("Cwd=")
                .map(|d| OscEvent::Cwd(unescape_633(d)))
        }
        _ => None,
    }
}

/// VS Code escapes `;` and non-printables as `\xAB` and backslash as `\\`.
fn unescape_633(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(v) = u8::from_str_radix(&hex, 16) {
                    out.push(v as char);
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// `file://hostname/path/with%20escapes` → `/path/with escapes`.
pub fn parse_file_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("file://")
        .or_else(|| url.strip_prefix("kitty-shell-cwd://"))?;
    let path = &rest[rest.find('/')?..];
    Some(percent_decode(path))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<OscEvent> {
        OscState::default().parse(s.as_bytes())
    }

    #[test]
    fn cwd() {
        assert_eq!(
            parse_file_url("file://mac.local/Users/me/My%20Code").as_deref(),
            Some("/Users/me/My Code")
        );
        assert_eq!(p("633;P;Cwd=/a\\x3bb"), Some(OscEvent::Cwd("/a;b".into())));
        assert_eq!(parse_file_url("file://host"), None);
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("a%2"), "a%2");
    }

    #[test]
    fn kitty_multipart() {
        let mut s = OscState::default();
        assert_eq!(s.parse(b"99;i=1:d=0;Hello"), None);
        assert!(
            s.parse(b"99;i=1:p=body;V29ybGQ=").is_some(),
            "d defaults to 1"
        );
        let mut s = OscState::default();
        s.parse(b"99;i=2:d=0;Title");
        assert_eq!(
            s.parse(b"99;i=2:p=body:e=1;V29ybGQ="),
            Some(OscEvent::Notify {
                title: "Title".into(),
                body: "World".into()
            })
        );
        assert_eq!(
            s.parse(b"99;;Simple"),
            Some(OscEvent::Notify {
                title: "Simple".into(),
                body: "".into()
            })
        );
    }

    #[test]
    fn prompt_marks() {
        assert_eq!(p("133;A"), Some(OscEvent::PromptStart { redraw: None }));
        assert_eq!(
            p("133;A;click_events=1;redraw=1"),
            Some(OscEvent::PromptStart { redraw: Some(true) })
        );
        assert_eq!(
            p("133;A;redraw=0"),
            Some(OscEvent::PromptStart {
                redraw: Some(false)
            })
        );
        assert_eq!(p("133;B"), Some(OscEvent::InputStart));
        assert_eq!(
            p("633;P;ThurmPath=/nix/bin:/usr/bin"),
            Some(OscEvent::ShellPath("/nix/bin:/usr/bin".into()))
        );
        assert_eq!(p("133;C"), Some(OscEvent::CommandStart));
        assert_eq!(p("133;D;1"), Some(OscEvent::CommandFinished(Some(1))));
        assert_eq!(p("133;D"), Some(OscEvent::CommandFinished(None)));
        assert_eq!(
            p("633;E;ls -la"),
            Some(OscEvent::CommandLine("ls -la".into()))
        );
    }
}
