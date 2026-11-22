//! Session titles and interrupts from agent transcripts and metadata files.
//!
//! - Claude Code appends `{"type":"ai-title","aiTitle":…}` and
//!   `{"type":"custom-title","customTitle":…}` lines to the session's JSONL transcript, and
//!   records user interrupts as a user entry with `interruptedMessageId`.
//! - Pi appends `{"type":"session_info","name":…}` to its JSONL transcript when named.
//! - Antigravity (`agy`) writes generated and renamed titles as `title:"…"` in
//!   `<app_data_dir>/annotations/<conversationId>.pbtxt`.
//!
//! Hooks hand us the transcript path (and session id); we follow JSONL files incrementally and
//! re-read `.pbtxt` files when their metadata changes.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

/// Longest title we keep, in characters.
const MAX_TITLE: usize = 60;
/// Maximum bytes read from a `.pbtxt` annotation file.
const MAX_PBTXT_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone)]
pub struct TitleReader {
    path: PathBuf,
    /// Bytes consumed so far (JSONL: at a line boundary; `.pbtxt`: last read size).
    offset: u64,
    /// Last observed mtime for rewritten files (`.pbtxt`).
    modified: Option<SystemTime>,
    /// Whether a `.pbtxt` file has been read at least once.
    pbtxt_read: bool,
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
    #[serde(rename = "session_info")]
    SessionInfo { name: Option<String> },
    #[serde(rename = "user", rename_all = "camelCase")]
    User {
        interrupted_message_id: Option<serde::de::IgnoredAny>,
        #[serde(default)]
        is_sidechain: bool,
    },
}

/// Path to Antigravity's per-conversation annotation file (`annotations/<conversationId>.pbtxt`),
/// derived from the hook's `transcriptPath` (parent of `brain/`) or `~/.gemini/antigravity-cli`.
pub fn agy_annotation_path(
    transcript_path: Option<&str>,
    session_id: Option<&str>,
) -> Option<PathBuf> {
    let id = session_id?.trim();
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
        return None;
    }
    let app_dir = transcript_path
        .and_then(|p| {
            Path::new(p)
                .ancestors()
                .find(|a| a.file_name().and_then(|n| n.to_str()) == Some("brain"))
                .and_then(Path::parent)
                .map(Path::to_path_buf)
        })
        .or_else(default_agy_app_dir)?;
    Some(app_dir.join("annotations").join(format!("{id}.pbtxt")))
}

fn default_agy_app_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let cli = home.join(".gemini/antigravity-cli");
    if cli.is_dir() {
        return Some(cli);
    }
    let app = home.join(".gemini/antigravity");
    if app.is_dir() {
        return Some(app);
    }
    Some(cli)
}

impl TitleReader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            modified: None,
            pbtxt_read: false,
            custom: None,
            ai: None,
            interrupted: false,
        }
    }

    pub fn is(&self, path: &Path) -> bool {
        self.path == path
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn is_pbtxt(&self) -> bool {
        self.path.extension().and_then(|e| e.to_str()) == Some("pbtxt")
    }

    /// The session's title: the user's `/rename` name, else the generated one.
    pub fn title(&self) -> Option<&str> {
        self.custom.as_deref().or(self.ai.as_deref())
    }

    /// Whether the user interrupted a turn since the last call.
    pub fn take_interrupted(&mut self) -> bool {
        std::mem::take(&mut self.interrupted)
    }

    /// Read what changed since the last call. A partial last JSONL line is left for next time.
    pub fn poll(&mut self) {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return;
        };
        let len = meta.len();
        if self.is_pbtxt() {
            let modified = meta.modified().ok();
            // Skip re-reading a settled `.pbtxt` whose size and mtime haven't moved; re-read
            // files touched within the last 2 s so same-length renames inside one mtime tick
            // are still picked up.
            if self.pbtxt_read
                && self.offset == len
                && self.modified == modified
                && modified.is_some_and(|m| m.elapsed().is_ok_and(|e| e > Duration::from_secs(2)))
            {
                return;
            }
            let Ok(f) = File::open(&self.path) else {
                return;
            };
            let mut text = String::new();
            if f.take(MAX_PBTXT_BYTES).read_to_string(&mut text).is_err() {
                return;
            }
            self.offset = len;
            self.modified = modified;
            self.pbtxt_read = true;
            self.ai = parse_pbtxt_title(&text);
            return;
        }
        if len == self.offset {
            return;
        }
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
        if !has(b"-title\"") && !has(b"\"session_info\"") && !has(b"\"interruptedMessageId\"") {
            return;
        }
        match serde_json::from_slice::<Entry>(line) {
            Ok(Entry::AiTitle { ai_title }) => self.ai = clean(&ai_title).or(self.ai.take()),
            Ok(Entry::CustomTitle { custom_title }) => self.custom = clean(&custom_title),
            Ok(Entry::SessionInfo { name }) => {
                self.custom = name.as_deref().and_then(clean);
            }
            // A subagent's interrupt does not end the main turn.
            Ok(Entry::User {
                interrupted_message_id: Some(_),
                is_sidechain: false,
            }) => self.interrupted = true,
            _ => {}
        }
    }
}

/// Extract `title:"…"` from a protobuf text-format annotation file.
fn parse_pbtxt_title(text: &str) -> Option<String> {
    for line in text.lines() {
        let rest = line.trim_start();
        let Some(rest) = rest.strip_prefix("title") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(raw) = unescape_pbtxt_string(rest) else {
            continue;
        };
        return clean(&raw);
    }
    None
}

/// Parse one double-quoted protobuf text-format string literal (with standard C/octal escapes).
fn unescape_pbtxt_string(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return String::from_utf8(out).ok(),
            b'\\' => {
                i += 1;
                let &esc = bytes.get(i)?;
                match esc {
                    b'"' | b'\'' | b'\\' => {
                        out.push(esc);
                        i += 1;
                    }
                    b'n' => {
                        out.push(b'\n');
                        i += 1;
                    }
                    b'r' => {
                        out.push(b'\r');
                        i += 1;
                    }
                    b't' => {
                        out.push(b'\t');
                        i += 1;
                    }
                    b'0'..=b'7' => {
                        let mut val = 0u16;
                        for _ in 0..3 {
                            match bytes.get(i) {
                                Some(&d @ b'0'..=b'7') => {
                                    val = (val << 3) | u16::from(d - b'0');
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        out.push(val as u8);
                    }
                    b'x' | b'X' => {
                        i += 1;
                        let mut val = 0u8;
                        let mut digits = 0;
                        while digits < 2 {
                            match bytes.get(i).and_then(|&b| (b as char).to_digit(16)) {
                                Some(d) => {
                                    val = (val << 4) | (d as u8);
                                    i += 1;
                                    digits += 1;
                                }
                                None => break,
                            }
                        }
                        if digits == 0 {
                            return None;
                        }
                        out.push(val);
                    }
                    other => {
                        out.push(other);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    None
}

/// One line, trimmed, control characters dropped, capped at [`MAX_TITLE`].
pub(crate) fn clean(title: &str) -> Option<String> {
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

        // Pi's session_info updates or clears the custom title.
        append(
            &path,
            "{\"type\":\"session_info\",\"id\":\"a\",\"name\":\"Refactor auth\"}\n",
        );
        r.poll();
        assert_eq!(r.title(), Some("Refactor auth"));
        append(
            &path,
            "{\"type\":\"session_info\",\"id\":\"b\",\"name\":\"\"}\n",
        );
        r.poll();
        assert_eq!(r.title(), Some("Other"));

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
    fn reads_and_updates_pbtxt_annotations() {
        let path =
            std::env::temp_dir().join(format!("thurm-annotation-{}.pbtxt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut r = TitleReader::new(&path);
        r.poll();
        assert_eq!(r.title(), None);

        std::fs::write(&path, "title:\"Fix \\\"quoted\\\" \\303\\251 title\"\n").unwrap();
        r.poll();
        assert_eq!(r.title(), Some("Fix \"quoted\" é title"));

        // Same-length in-place rename is picked up immediately.
        std::fs::write(&path, "title:\"Rename A\"\n").unwrap();
        r.poll();
        assert_eq!(r.title(), Some("Rename A"));
        std::fs::write(&path, "title:\"Rename B\"\n").unwrap();
        r.poll();
        assert_eq!(r.title(), Some("Rename B"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn derives_agy_annotation_path_from_transcript() {
        let p = agy_annotation_path(
            Some("/tmp/antigravity-cli/brain/abc-123/.system_generated/logs/transcript.jsonl"),
            Some("abc-123"),
        );
        assert_eq!(
            p,
            Some(PathBuf::from(
                "/tmp/antigravity-cli/annotations/abc-123.pbtxt"
            ))
        );
        assert_eq!(agy_annotation_path(None, Some("../escape")), None);
        assert_eq!(agy_annotation_path(None, Some("")), None);
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
