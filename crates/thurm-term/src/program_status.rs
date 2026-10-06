//! Program Status Protocol (OSC 7501): programs report what they are doing (idle, working,
//! done, blocked on the user, failed) and why, as records the terminal keeps.
//!
//! <https://www.superlogical.com/rex/docs/build/program-status>

use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

use thurm_proto::{BlockedKind, ProgramRecord, ProgramState};

/// The OSC number.
pub const OSC: &[u8] = b"7501";
/// Answer to the feature detection query (`OSC 7501 ; ? ST`).
pub const QUERY_REPLY: &[u8] = b"\x1b]7501;?\x1b\\";

/// Longest whole sequence, OSC through ST.
const MAX_SEQUENCE: usize = 4096;
const MAX_KEY: usize = 16;
const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG: usize = 2048;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE: usize = 192;
const MAX_APP: usize = 32;
const MAX_ID: usize = 128;
const MAX_SEGMENT: usize = 32;
const MAX_DEPTH: usize = 8;
/// Records kept per terminal; the least recently updated one makes room.
pub const MAX_RECORDS: usize = 256;

/// Standard base64, padding optional.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// `OSC 7501 ; ? ST`: answer with [`QUERY_REPLY`].
    Query,
    /// Replace the record `id` (empty: the root record).
    Set(ProgramRecord),
    /// Remove the record `id` and every record beneath it; `None` removes all.
    Clear(Option<String>),
}

/// Parse the body after `7501;`. `None`: the report is discarded whole.
pub fn parse(body: &[u8]) -> Option<Parsed> {
    // ESC ] 7501 ; body BEL is the shortest form of the sequence.
    if 2 + OSC.len() + 1 + body.len() + 1 > MAX_SEQUENCE {
        return None;
    }
    if body.trim_ascii() == b"?" {
        return Some(Parsed::Query);
    }
    // Every pair is checked against the limits before anything is applied.
    let mut pairs: Vec<(&[u8], &[u8])> = Vec::new();
    for raw in body.split(|&b| b == b':') {
        let Some(eq) = raw.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = raw[..eq].trim_ascii();
        let value = raw[eq + 1..].trim_ascii();
        if key.len() > MAX_KEY {
            return None;
        }
        if key.is_empty()
            || !key.iter().all(u8::is_ascii_lowercase)
            || !value.iter().copied().all(value_byte)
        {
            continue;
        }
        let limit = match key {
            b"msg" => MAX_MSG_ENCODED,
            b"title" => MAX_TITLE_ENCODED,
            b"app" => MAX_APP,
            b"id" => MAX_ID,
            _ => usize::MAX,
        };
        if value.len() > limit {
            return None;
        }
        pairs.retain(|(k, _)| *k != key);
        pairs.push((key, value));
    }
    let get = |key: &[u8]| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);

    let id = match get(b"id") {
        Some(id) => Some(valid_id(id)?.to_owned()),
        None => None,
    };
    let state = match get(b"state")? {
        b"idle" => ProgramState::Idle,
        b"working" => ProgramState::Working,
        b"done" => ProgramState::Done,
        b"blocked" => ProgramState::Blocked,
        b"error" => ProgramState::Error,
        b"clear" => return Some(Parsed::Clear(id)),
        _ => return None,
    };
    let title = get(b"title").map(|v| text(v, MAX_TITLE)).transpose().ok()?;
    let message = get(b"msg").map(|v| text(v, MAX_MSG)).transpose().ok()?;
    let kind = get(b"kind")
        .filter(|_| state == ProgramState::Blocked)
        .and_then(|k| match k {
            b"permission" => Some(BlockedKind::Permission),
            b"question" => Some(BlockedKind::Question),
            b"auth" => Some(BlockedKind::Auth),
            _ => None,
        });
    let progress = get(b"progress")
        .filter(|_| matches!(state, ProgramState::Working | ProgramState::Blocked))
        .filter(|p| !p.is_empty() && p.len() <= 3 && p.iter().all(u8::is_ascii_digit))
        .and_then(|p| std::str::from_utf8(p).ok()?.parse::<u8>().ok())
        .filter(|&p| p <= 100);
    let app = get(b"app")
        .filter(|a| !a.is_empty() && a.iter().copied().all(segment_byte))
        .map(|a| String::from_utf8_lossy(a).into_owned());
    Some(Parsed::Set(ProgramRecord {
        id: id.unwrap_or_default(),
        state,
        kind,
        progress,
        app,
        title: title.flatten(),
        message: message.flatten(),
        seen: false,
    }))
}

/// `[A-Za-z0-9_.,+/=-]`
fn value_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b)
}

/// `[A-Za-z0-9_.+-]`: id segments and app names.
fn segment_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_.+-".contains(&b)
}

fn valid_id(id: &[u8]) -> Option<&str> {
    let segments = id.split(|&b| b == b'/');
    let mut depth = 0;
    for s in segments {
        depth += 1;
        if s.is_empty() || s.len() > MAX_SEGMENT || !s.iter().copied().all(segment_byte) {
            return None;
        }
    }
    if depth > MAX_DEPTH {
        return None;
    }
    std::str::from_utf8(id).ok()
}

/// Decode a free-text value. `Err`: it breaks the rules (the report is discarded);
/// `Ok(None)`: empty.
fn text(encoded: &[u8], max: usize) -> Result<Option<String>, ()> {
    let bytes = BASE64.decode(encoded).map_err(|_| ())?;
    if bytes.len() > max {
        return Err(());
    }
    let s = String::from_utf8(bytes).map_err(|_| ())?;
    if s.chars().any(char::is_control) {
        return Err(());
    }
    let s: String = s.chars().filter(|c| !invisible_format(*c)).collect();
    Ok((!s.is_empty()).then_some(s))
}

/// Text direction overrides and other invisible formatting characters: shown outside the
/// terminal grid, they could make a message read as something else.
fn invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
    )
}

/// Whether `id` is `parent` or beneath it.
fn within(id: &str, parent: &str) -> bool {
    parent.is_empty()
        || id == parent
        || id
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The records of one terminal, least recently updated first.
#[derive(Debug, Default, Clone)]
pub struct ProgramStatus {
    records: Vec<ProgramRecord>,
}

impl ProgramStatus {
    /// Apply a report. Returns whether the records changed.
    pub fn apply(&mut self, parsed: Parsed) -> bool {
        match parsed {
            Parsed::Query => false,
            Parsed::Clear(None) => self.clear(),
            Parsed::Clear(Some(id)) => self.remove(|r| within(&r.id, &id)),
            Parsed::Set(record) => {
                match self.records.iter().position(|r| r.id == record.id) {
                    Some(i) => {
                        self.records.remove(i);
                    }
                    None if self.records.len() >= MAX_RECORDS => {
                        self.records.remove(0);
                    }
                    None => {}
                }
                self.records.push(record);
                true
            }
        }
    }

    /// A shell prompt began (OSC 133;A): whatever was running is over. Done and error records
    /// stay for the user to find.
    pub fn prompt_started(&mut self) -> bool {
        self.remove(|r| !matches!(r.state, ProgramState::Done | ProgramState::Error))
    }

    /// The process attached to the terminal exited.
    pub fn process_exited(&mut self) -> bool {
        self.prompt_started()
    }

    /// Full reset (RIS).
    pub fn clear(&mut self) -> bool {
        let changed = !self.records.is_empty();
        self.records.clear();
        changed
    }

    /// The user came back to the terminal: finished work is no longer news.
    pub fn acknowledge(&mut self) -> bool {
        let mut changed = false;
        for r in &mut self.records {
            if matches!(r.state, ProgramState::Done | ProgramState::Error) && !r.seen {
                r.seen = true;
                changed = true;
            }
        }
        changed
    }

    /// The records as reported (`app` not inherited), to hand over to another terminal.
    pub fn stored(&self) -> &[ProgramRecord] {
        &self.records
    }

    /// Take over records from [`ProgramStatus::stored`].
    pub fn restore(&mut self, mut records: Vec<ProgramRecord>) {
        let excess = records.len().saturating_sub(MAX_RECORDS);
        records.drain(..excess);
        self.records = records;
    }

    fn remove(&mut self, f: impl Fn(&ProgramRecord) -> bool) -> bool {
        let before = self.records.len();
        self.records.retain(|r| !f(r));
        self.records.len() != before
    }

    /// The records, each with the `app` of its nearest ancestor when it has none.
    pub fn records(&self) -> Vec<ProgramRecord> {
        self.records
            .iter()
            .map(|r| {
                let mut r = r.clone();
                if r.app.is_none() {
                    r.app = self.inherited_app(&r.id);
                }
                r
            })
            .collect()
    }

    fn inherited_app(&self, id: &str) -> Option<String> {
        let mut id = id;
        while !id.is_empty() {
            id = id.rfind('/').map_or("", |i| &id[..i]);
            if let Some(app) = self
                .records
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| r.app.clone())
            {
                return Some(app);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    fn set(body: &str) -> ProgramRecord {
        match parse(body.as_bytes()) {
            Some(Parsed::Set(r)) => r,
            other => panic!("{body}: {other:?}"),
        }
    }

    fn table(reports: &[&str]) -> ProgramStatus {
        let mut t = ProgramStatus::default();
        for r in reports {
            if let Some(p) = parse(r.as_bytes()) {
                t.apply(p);
            }
        }
        t
    }

    #[test]
    fn spec_example() {
        let r = set(
            "state=blocked:kind=permission:app=terraform:msg=QXBwbHkgMyB0byBhZGQsIDEgdG8gY2hhbmdlLCAwIHRvIGRlc3Ryb3k/",
        );
        assert_eq!(r.id, "");
        assert_eq!(r.state, ProgramState::Blocked);
        assert_eq!(r.kind, Some(BlockedKind::Permission));
        assert_eq!(r.app.as_deref(), Some("terraform"));
        assert_eq!(
            r.message.as_deref(),
            Some("Apply 3 to add, 1 to change, 0 to destroy?")
        );
        assert_eq!(parse(b"?"), Some(Parsed::Query));
    }

    #[test]
    fn pairs() {
        // Whitespace is trimmed, malformed pairs are skipped, the last value wins, unknown keys
        // are ignored.
        let r = set(" state = idle :junk:=x:Bad=1:app=a b:state=working:future=1:app=cargo");
        assert_eq!(r.state, ProgramState::Working);
        assert_eq!(r.app.as_deref(), Some("cargo"));
        // A value outside the character set is a malformed pair, not a broken report.
        assert_eq!(set("state=working:msg=!!!").message, None);
        // A malformed repeat does not replace a good value.
        assert_eq!(set("state=done:state=wo rking").state, ProgramState::Done);
        // Padding is optional.
        assert_eq!(
            set(&format!(
                "state=done:msg={}",
                b64("ab").trim_end_matches('=')
            ))
            .message,
            Some("ab".into())
        );
    }

    #[test]
    fn ignored_reports() {
        for body in [
            "",
            "app=x",
            "state=paused",
            "state=working:id=a//b",
            "state=working:id=/a",
            "state=working:id=",
            "state=working:id=a/b/c/d/e/f/g/h/i",
            "state=working:msg=A",
            "state=working:msg=a,b",
            // Control characters in decoded text discard the report.
            &format!("state=working:msg={}", b64("a\nb")),
            &format!("state=working:title={}", b64("\u{85}")),
            // Limits.
            &format!("state=working:{}=1", "k".repeat(17)),
            &format!("state=working:app={}", "a".repeat(33)),
            &format!("state=working:title={}", b64(&"t".repeat(193))),
            &format!("state=working:msg={}", b64(&"m".repeat(2049))),
            &format!("state=working:id={}", "a".repeat(33)),
            &format!("state=working:x={}", "a".repeat(4096)),
        ] {
            assert!(
                !matches!(parse(body.as_bytes()), Some(Parsed::Set(_))),
                "{body:.60}"
            );
        }
        assert!(
            parse(format!("state=working:msg={}", b64(&"m".repeat(2048))).as_bytes()).is_some()
        );
        assert!(parse(format!("state=working:id={}", ["a"; 8].join("/")).as_bytes()).is_some());
    }

    #[test]
    fn keys_that_only_apply_to_some_states() {
        let r = set("state=working:kind=permission:progress=40");
        assert_eq!((r.kind, r.progress), (None, Some(40)));
        let r = set("state=blocked:kind=auth:progress=100");
        assert_eq!((r.kind, r.progress), (Some(BlockedKind::Auth), Some(100)));
        assert_eq!(set("state=blocked:kind=other").kind, None);
        assert_eq!(set("state=done:progress=40").progress, None);
        for p in ["101", "-1", "4.5", "", "1000"] {
            assert_eq!(set(&format!("state=working:progress={p}")).progress, None);
        }
        assert_eq!(set("state=working:app=a/b").app, None);
    }

    #[test]
    fn invisible_formatting_is_disarmed() {
        let r = set(&format!("state=done:msg={}", b64("ok\u{202E}txt.exe")));
        assert_eq!(r.message.as_deref(), Some("oktxt.exe"));
    }

    #[test]
    fn reports_replace_records() {
        let t = table(&[
            "state=working:app=brew:msg=SW5zdGFsbGluZyB1cGRhdGVz",
            "state=done",
        ]);
        let r = &t.records()[0];
        assert_eq!(
            (r.state, r.app.as_ref(), r.message.as_ref()),
            (ProgramState::Done, None, None)
        );
    }

    #[test]
    fn several_records() {
        let mut t = table(&[
            "state=working:app=deploy",
            &format!(
                "state=working:id=us-east:title={}:progress=40",
                b64("US East")
            ),
            &format!(
                "state=blocked:kind=permission:id=eu-west/db:title={}",
                b64("EU West")
            ),
        ]);
        let recs = t.records();
        assert_eq!(recs.len(), 3);
        // Children take app from their nearest ancestor, which needn't exist.
        assert!(recs.iter().all(|r| r.app.as_deref() == Some("deploy")));
        t.apply(parse(b"state=clear:id=eu-west").unwrap());
        assert_eq!(t.records().len(), 2);
        // `us` is no parent of `us-east`.
        t.apply(parse(b"state=clear:id=us").unwrap());
        assert_eq!(t.records().len(), 2);
        t.apply(parse(b"state=clear").unwrap());
        assert!(t.records().is_empty());
    }

    #[test]
    fn lifetimes() {
        let mut t = table(&[
            "state=working:id=a",
            "state=blocked:id=b",
            "state=idle:id=c",
            "state=done:id=d",
            "state=error:id=e",
        ]);
        assert!(t.prompt_started());
        let ids: Vec<_> = t.records().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["d", "e"]);
        assert!(!t.process_exited());
        assert!(t.acknowledge());
        assert!(t.records().iter().all(|r| r.seen));
        assert!(!t.acknowledge());
        // A new report is news again.
        t.apply(parse(b"state=done:id=d").unwrap());
        assert!(!t.records().last().unwrap().seen);
    }

    #[test]
    fn least_recently_updated_record_makes_room() {
        let mut t = ProgramStatus::default();
        for i in 0..MAX_RECORDS {
            t.apply(parse(format!("state=working:id=r{i}").as_bytes()).unwrap());
        }
        // r0 is updated, so r1 is the oldest when r-new arrives.
        t.apply(parse(b"state=done:id=r0").unwrap());
        t.apply(parse(b"state=working:id=new").unwrap());
        let recs = t.records();
        assert_eq!(recs.len(), MAX_RECORDS);
        assert!(!recs.iter().any(|r| r.id == "r1"));
        assert!(recs.iter().any(|r| r.id == "r0"));
    }
}
