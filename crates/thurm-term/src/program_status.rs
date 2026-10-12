//! Program Status Protocol (OSC 7501): programs report what they are doing (idle, working,
//! done, blocked on the user, failed) and why, as records the terminal keeps. libghostty-vt
//! parses the reports; Thurm keeps the records and applies their lifetime rules.
//!
//! <https://www.superlogical.com/rex/docs/build/program-status>

use thurm_proto::{ProgramRecord, ProgramState};

/// Records kept per terminal; the least recently updated one makes room.
pub const MAX_RECORDS: usize = 256;

/// A report, as libghostty-vt parsed and checked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// Replace the record `id` (empty: the root record).
    Set(ProgramRecord),
    /// Remove the record `id` and every record beneath it; `None` removes all.
    Clear(Option<String>),
}

/// Text direction overrides and other invisible formatting characters: shown outside the
/// terminal grid, they could make a message read as something else.
pub(crate) fn invisible_format(c: char) -> bool {
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
    pub fn apply(&mut self, report: Report) -> bool {
        match report {
            Report::Clear(None) => self.clear(),
            Report::Clear(Some(id)) => self.remove(|r| within(&r.id, &id)),
            Report::Set(record) => {
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

    fn set(id: &str, state: ProgramState) -> Report {
        Report::Set(ProgramRecord {
            id: id.into(),
            state,
            kind: None,
            progress: None,
            app: None,
            title: None,
            message: None,
            seen: false,
        })
    }

    fn with_app(id: &str, app: &str) -> Report {
        let Report::Set(mut r) = set(id, ProgramState::Working) else {
            unreachable!()
        };
        r.app = Some(app.into());
        Report::Set(r)
    }

    fn table(reports: impl IntoIterator<Item = Report>) -> ProgramStatus {
        let mut t = ProgramStatus::default();
        for r in reports {
            t.apply(r);
        }
        t
    }

    #[test]
    fn reports_replace_records() {
        let t = table([with_app("", "brew"), set("", ProgramState::Done)]);
        let r = &t.records()[0];
        assert_eq!((r.state, r.app.as_ref()), (ProgramState::Done, None));
    }

    #[test]
    fn several_records() {
        let mut t = table([
            with_app("", "deploy"),
            set("us-east", ProgramState::Working),
            set("eu-west/db", ProgramState::Blocked),
        ]);
        let recs = t.records();
        assert_eq!(recs.len(), 3);
        // Children take app from their nearest ancestor, which needn't exist.
        assert!(recs.iter().all(|r| r.app.as_deref() == Some("deploy")));
        t.apply(Report::Clear(Some("eu-west".into())));
        assert_eq!(t.records().len(), 2);
        // `us` is no parent of `us-east`.
        t.apply(Report::Clear(Some("us".into())));
        assert_eq!(t.records().len(), 2);
        t.apply(Report::Clear(None));
        assert!(t.records().is_empty());
    }

    #[test]
    fn lifetimes() {
        let mut t = table([
            set("a", ProgramState::Working),
            set("b", ProgramState::Blocked),
            set("c", ProgramState::Idle),
            set("d", ProgramState::Done),
            set("e", ProgramState::Error),
        ]);
        assert!(t.prompt_started());
        let ids: Vec<_> = t.records().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["d", "e"]);
        assert!(!t.process_exited());
        assert!(t.acknowledge());
        assert!(t.records().iter().all(|r| r.seen));
        assert!(!t.acknowledge());
        // A new report is news again.
        t.apply(set("d", ProgramState::Done));
        assert!(!t.records().last().unwrap().seen);
    }

    #[test]
    fn least_recently_updated_record_makes_room() {
        let mut t = table((0..MAX_RECORDS).map(|i| set(&format!("r{i}"), ProgramState::Working)));
        // r0 is updated, so r1 is the oldest when r-new arrives.
        t.apply(set("r0", ProgramState::Done));
        t.apply(set("new", ProgramState::Working));
        let recs = t.records();
        assert_eq!(recs.len(), MAX_RECORDS);
        assert!(!recs.iter().any(|r| r.id == "r1"));
        assert!(recs.iter().any(|r| r.id == "r0"));
    }
}
