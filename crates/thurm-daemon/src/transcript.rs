//! Session titles and interrupts from Claude Code transcripts.
//!
//! Claude Code names every session: it appends `{"type":"ai-title","aiTitle":…}` lines to the
//! session's JSONL transcript (and `{"type":"custom-title","customTitle":…}` for `/rename`).
//! Pressing Esc mid-turn fires no hook, but appends a user entry with an
//! `interruptedMessageId` ("[Request interrupted by user]").
//! Hooks hand us the transcript path; we follow the file incrementally so each poll only
//! reads what was appended since the last one.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use serde::Deserialize;

/// Longest title we keep, in characters.
const MAX_TITLE: usize = 60;

#[derive(Debug, Clone)]
pub struct TitleReader {
    path: PathBuf,
    /// Bytes consumed so far (always at a line boundary).
    offset: u64,
    custom: Option<String>,
    ai: Option<String>,
    /// The user interrupted a turn since the last [`TitleReader::take_interrupted`].
    interrupted: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Entry {
    #[serde(rename = "ai-title", rename_all = "camelCase")]
    AiTitle { ai_title: String },
    #[serde(rename = "custom-title", rename_all = "camelCase")]
    CustomTitle { custom_title: String },
    #[serde(rename = "user", rename_all = "camelCase")]
    User {
        interrupted_message_id: Option<serde::de::IgnoredAny>,
        #[serde(default)]
        is_sidechain: bool,
    },
}

impl TitleReader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            custom: None,
            ai: None,
            interrupted: false,
        }
    }

    pub fn is(&self, path: &str) -> bool {
        self.path.as_os_str() == path
    }

    /// The session's title: the user's `/rename` name, else the generated one.
    pub fn title(&self) -> Option<&str> {
        self.custom.as_deref().or(self.ai.as_deref())
    }

    /// Whether the user interrupted a turn since the last call.
    pub fn take_interrupted(&mut self) -> bool {
        std::mem::take(&mut self.interrupted)
    }

    /// Read what was appended since the last call. A partial last line is left for next time.
    pub fn poll(&mut self) {
        let Ok(mut f) = File::open(&self.path) else {
            return;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Truncated or replaced: start over.
            *self = Self::new(std::mem::take(&mut self.path));
        }
        if len == self.offset || f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut reader = BufReader::new(f);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(n) if line.last() == Some(&b'\n') => {
                    self.offset += n as u64;
                    self.consume(&line);
                }
                Ok(_) => break,
            }
        }
    }

    fn consume(&mut self, line: &[u8]) {
        // Most lines are large messages; only parse the ones that can be titles or interrupts.
        let has = |needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
        if !has(b"-title\"") && !has(b"\"interruptedMessageId\"") {
            return;
        }
        match serde_json::from_slice::<Entry>(line) {
            Ok(Entry::AiTitle { ai_title }) => self.ai = clean(&ai_title).or(self.ai.take()),
            Ok(Entry::CustomTitle { custom_title }) => self.custom = clean(&custom_title),
            // A subagent's interrupt does not end the main turn.
            Ok(Entry::User {
                interrupted_message_id: Some(_),
                is_sidechain: false,
            }) => self.interrupted = true,
            _ => {}
        }
    }
}

/// One line, trimmed, control characters dropped, capped at [`MAX_TITLE`].
fn clean(title: &str) -> Option<String> {
    let t: String = title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if t.is_empty() {
        return None;
    }
    if t.chars().count() <= MAX_TITLE {
        return Some(t);
    }
    let mut cut: String = t.chars().take(MAX_TITLE - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    Some(cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn append(path: &std::path::Path, s: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn follows_titles_incrementally() {
        let path =
            std::env::temp_dir().join(format!("thurm-transcript-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        append(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"fix the \\\"x-title\\\" header\"}}\n",
        );
        let mut r = TitleReader::new(&path);
        r.poll();
        assert_eq!(r.title(), None);

        append(
            &path,
            "{\"type\":\"ai-title\",\"aiTitle\":\"Fix login  bug\",\"sessionId\":\"a\"}\n",
        );
        r.poll();
        assert_eq!(r.title(), Some("Fix login bug"));

        // A half-written line waits for its newline.
        append(&path, "{\"type\":\"ai-title\",\"aiTi");
        r.poll();
        assert_eq!(r.title(), Some("Fix login bug"));
        append(&path, "tle\":\"Refactor auth\"}\n");
        r.poll();
        assert_eq!(r.title(), Some("Refactor auth"));

        // /rename wins over later generated titles.
        append(
            &path,
            "{\"type\":\"custom-title\",\"customTitle\":\"auth work\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Other\"}\n",
        );
        r.poll();
        assert_eq!(r.title(), Some("auth work"));

        // Truncation restarts from the top.
        std::fs::write(&path, "{\"type\":\"ai-title\",\"aiTitle\":\"New\"}\n").unwrap();
        r.poll();
        assert_eq!(r.title(), Some("New"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn notices_interrupts() {
        let path =
            std::env::temp_dir().join(format!("thurm-interrupt-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // Mentioning the marker in a message is not an interrupt.
        append(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"what is \\\"interruptedMessageId\\\"?\"}}\n",
        );
        let mut r = TitleReader::new(&path);
        r.poll();
        assert!(!r.take_interrupted());

        let interrupt = |sidechain: bool| {
            format!(
                "{{\"type\":\"user\",\"isSidechain\":{sidechain},\"interruptedMessageId\":\"msg_1\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"[Request interrupted by user]\"}}]}}}}\n"
            )
        };
        append(&path, &interrupt(true));
        r.poll();
        assert!(!r.take_interrupted(), "subagent interrupts are ignored");
        append(&path, &interrupt(false));
        r.poll();
        assert!(r.take_interrupted());
        assert!(!r.take_interrupted(), "reported once");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_and_long_titles() {
        let mut r = TitleReader::new("/nonexistent/thurm/s.jsonl");
        r.poll();
        assert_eq!(r.title(), None);
        let long = clean(&"word ".repeat(30)).unwrap();
        assert_eq!(long.chars().count(), MAX_TITLE);
        assert!(long.ends_with('…'));
        assert_eq!(clean(" \n\t "), None);
        assert_eq!(clean("a\u{1b}b"), Some("ab".into()));
    }
}
