//! Program Resume Protocol (OSC 7577): verification, record lifecycle, trust evaluation,
//! and shell-safe execution on session restore.
//!
//! <https://www.superlogical.com/rex/docs/build/program-resume>

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::alphabet::STANDARD;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use thurm_config::{FlagArity, ResumeProgramsMode, ResumeRule};
use thurm_proto::ResumeRecordInfo;

use crate::persist::{self, SavedProgramResume};
use crate::procinfo;

/// Maximum full OSC 7577 sequence length accepted (including `ESC ]`, `7577;`, body, BEL/ST).
/// The spec requires supporting at least 2048 bytes.
pub const MAX_REPORT_BYTES: usize = 65_536;

/// Maximum number of `argv` elements accepted. The spec requires supporting at least 64.
pub const MAX_ARGV: usize = 128;

/// Maximum concurrent resume records kept per pane.
pub const MAX_RECORDS_PER_PANE: usize = 64;

/// Allowed clock skew between the report's `time` and the terminal's clock (spec SHOULD: 60 s).
pub const TIME_WINDOW_SECS: u64 = 60;

/// How long a record survives after its owning process exits, so logout/shutdown races where
/// the program exits just before the daemon do not drop it (spec SHOULD: a second or two).
pub const EXIT_GRACE_MS: u64 = 2_000;

/// Keys implemented by this terminal (used to evaluate `need=`).
const KNOWN_KEYS: &[&str] = &["pid", "time", "mac", "argv", "cwd", "clear", "need"];

/// Standard base64 decoders: padding is optional, but when present it MUST be canonical (per spec).
const BASE64_STD_CANONICAL: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical)
        .with_decode_allow_trailing_bits(false),
);
const BASE64_STD_NO_PAD: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::RequireNone)
        .with_decode_allow_trailing_bits(false),
);

/// Whether the daemon runs as root (real or effective UID 0).
/// Per spec: a terminal running as root SHOULD NOT accept reports or resume programs.
pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 || libc::getuid() == 0 }
}

/// Current wall-clock time in milliseconds since Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

/// Cryptographically random per-pane `OSC7577_TOKEN`: 32 random bytes from `/dev/urandom`
/// encoded as 43 URL-safe base64 characters (`[A-Za-z0-9_-]`, within the spec's 22..=64 range).
pub fn generate_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| anyhow::anyhow!("no random source for OSC7577_TOKEN: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Computes `SHA-256(prefix || ":" || token)`.
pub fn compute_mac(prefix: &[u8], token: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(prefix);
    h.update(b":");
    h.update(token.as_bytes());
    h.finalize().into()
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn decode_hex_32(hex: &[u8]) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let nibble = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        }
    };
    let mut out = [0u8; 32];
    for (i, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

fn is_pos_int(bytes: &[u8]) -> bool {
    !bytes.is_empty() && matches!(bytes[0], b'1'..=b'9') && bytes.iter().all(u8::is_ascii_digit)
}

fn parse_pos_u64(bytes: &[u8]) -> Option<u64> {
    if !is_pos_int(bytes) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn parse_usable_pid(bytes: &[u8]) -> Option<u32> {
    let n = parse_pos_u64(bytes)?;
    (n <= i32::MAX as u64).then_some(n as u32)
}

fn is_valid_key(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_lowercase())
}

fn is_valid_value(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
    })
}

fn has_control_char(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\u{0000}'..='\u{001F}' | '\u{007F}'..='\u{009F}'))
}

fn decode_b64_text(raw: &[u8]) -> Option<String> {
    let engine = if raw.contains(&b'=') {
        &BASE64_STD_CANONICAL
    } else {
        &BASE64_STD_NO_PAD
    };
    let bytes = engine.decode(raw).ok()?;
    let s = String::from_utf8(bytes).ok()?;
    if has_control_char(&s) {
        return None;
    }
    Some(s)
}

/// Validates `argv[0]`: must be either a bare command name (non-empty, no `/`, does not start
/// with `-`, not `.` or `..`) or an absolute path (starts with `/`, length > 1, no trailing `/`).
pub fn is_valid_program(arg0: &str) -> bool {
    if arg0.is_empty() || has_control_char(arg0) {
        return false;
    }
    if arg0.starts_with('/') {
        arg0.len() > 1 && !arg0.ends_with('/')
    } else {
        !arg0.contains('/') && !arg0.starts_with('-') && arg0 != "." && arg0 != ".."
    }
}

/// Validates a full `argv` vector per the OSC 7577 rules.
pub fn validate_argv(argv: &[String]) -> bool {
    if argv.is_empty() || argv.len() > MAX_ARGV {
        return false;
    }
    if !is_valid_program(&argv[0]) {
        return false;
    }
    argv.iter().skip(1).all(|a| !has_control_char(a))
}

/// Validates a `cwd` path per the OSC 7577 rules (non-empty absolute path, no control chars).
pub fn is_valid_cwd(cwd: &str) -> bool {
    cwd.starts_with('/') && !has_control_char(cwd)
}

/// Escapes control characters and invisible/bidirectional formatting characters before showing
/// a resume command or path to the user (per spec).
pub fn escape_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if has_control_char_char(c) || is_invisible_format(c) {
            out.push_str(&format!("\\u{{{:04X}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn has_control_char_char(c: char) -> bool {
    matches!(c, '\u{0000}'..='\u{001F}' | '\u{007F}'..='\u{009F}')
}

fn is_invisible_format(c: char) -> bool {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportKind {
    Set {
        argv: Vec<String>,
        cwd: Option<String>,
    },
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReport {
    pub pid: u32,
    pub time: u64,
    pub kind: ReportKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Failed `mac`, `time`, reserved `?`, or older timestamp than last accepted for `pid`.
    /// MUST have no effect on existing records.
    Ignore,
    /// Passed `mac` and `time`, but failed strict validation or limits.
    /// MUST remove all records for `pid` (if a usable `pid` was present).
    Reject {
        pid: Option<u32>,
        reason: &'static str,
    },
    /// Validated report.
    Accept(ParsedReport),
}

/// Parses and authenticates one OSC 7577 report body (everything after `7577;`).
///
/// Follows the spec's exact check order:
/// 1. `?` reserved query -> Ignore
/// 2. Trailing `:mac=<64 lowercase hex>` constant-time SHA-256 verification -> Ignore on fail
/// 3. `time` presence, format, and `±TIME_WINDOW_SECS` clock window -> Ignore on fail
/// 4. Monotonic `time` check against `last_accepted_time` for the same `pid` -> Ignore if older
/// 5. Strict syntax, limits, `need`, base64, UTF-8, control chars, `argv` XOR `clear` -> Reject
pub fn parse_report(
    body: &[u8],
    total_len: usize,
    token: &str,
    now_secs: u64,
    last_accepted_time: impl Fn(u32) -> Option<u64>,
) -> ParseOutcome {
    if body == b"?" {
        return ParseOutcome::Ignore;
    }

    // Step 1: trailing `:mac=<64 lowercase hex>` verified BEFORE anything else.
    const MAC_MARKER: &[u8] = b":mac=";
    let Some(mac_pos) = body.len().checked_sub(MAC_MARKER.len() + 64) else {
        return ParseOutcome::Ignore;
    };
    if mac_pos == 0 || &body[mac_pos..mac_pos + MAC_MARKER.len()] != MAC_MARKER {
        return ParseOutcome::Ignore;
    }
    let prefix = &body[..mac_pos];
    let mac_hex = &body[mac_pos + MAC_MARKER.len()..];
    let Some(expected_mac) = decode_hex_32(mac_hex) else {
        return ParseOutcome::Ignore;
    };
    let actual_mac = compute_mac(prefix, token);
    if !ct_eq(&actual_mac, &expected_mac) {
        return ParseOutcome::Ignore;
    }

    // Step 2: verify `time` BEFORE rejecting anything.
    let mut found_time: Option<u64> = None;
    let mut pid_count = 0usize;
    let mut usable_pid: Option<u32> = None;
    for pair in prefix.split(|&b| b == b':') {
        if let Some(eq) = pair.iter().position(|&b| b == b'=') {
            let k = &pair[..eq];
            let v = &pair[eq + 1..];
            if k == b"time" {
                let Some(t) = parse_pos_u64(v) else {
                    return ParseOutcome::Ignore;
                };
                if now_secs.abs_diff(t) > TIME_WINDOW_SECS {
                    return ParseOutcome::Ignore;
                }
                found_time = Some(t);
            } else if k == b"pid" {
                pid_count += 1;
                if pid_count == 1 {
                    usable_pid = parse_usable_pid(v);
                } else {
                    usable_pid = None;
                }
            }
        }
    }
    let Some(report_time) = found_time else {
        return ParseOutcome::Ignore;
    };

    // Step 3: monotonic time per pid (strictly earlier is ignored, equal is allowed).
    if let Some(pid) = usable_pid
        && let Some(prev) = last_accepted_time(pid)
        && report_time < prev
    {
        return ParseOutcome::Ignore;
    }

    // From here on, `mac` and `time` passed: any violation is a Reject (clearing `usable_pid`).
    let reject = |reason: &'static str| ParseOutcome::Reject {
        pid: usable_pid,
        reason,
    };

    if total_len > MAX_REPORT_BYTES {
        return reject("report exceeds length limit");
    }

    let mut seen: HashSet<&[u8]> = HashSet::new();
    let mut raw_argv: Option<&[u8]> = None;
    let mut raw_cwd: Option<&[u8]> = None;
    let mut raw_clear: Option<&[u8]> = None;
    let mut raw_need: Option<&[u8]> = None;
    let mut has_pid = false;

    for pair in prefix.split(|&b| b == b':') {
        let Some(eq) = pair.iter().position(|&b| b == b'=') else {
            return reject("malformed pair");
        };
        let k = &pair[..eq];
        let v = &pair[eq + 1..];
        if !is_valid_key(k) || !is_valid_value(v) {
            return reject("invalid key or value character");
        }
        if k == b"mac" || !seen.insert(k) {
            return reject("duplicate key");
        }
        match k {
            b"pid" => {
                if parse_usable_pid(v).is_none() {
                    return reject("invalid pid");
                }
                has_pid = true;
            }
            b"time" => {} // Already checked above.
            b"argv" => raw_argv = Some(v),
            b"cwd" => raw_cwd = Some(v),
            b"clear" => raw_clear = Some(v),
            b"need" => raw_need = Some(v),
            _ => {} // Unknown keys are ignored unless listed in `need`.
        }
    }

    if !has_pid {
        return reject("missing pid");
    }
    let Some(pid) = usable_pid else {
        return reject("invalid pid");
    };

    if let Some(need) = raw_need {
        if need.is_empty() {
            return reject("empty need");
        }
        for req in need.split(|&b| b == b',') {
            if !is_valid_key(req) {
                return reject("invalid key in need");
            }
            let Ok(name) = std::str::from_utf8(req) else {
                return reject("invalid key in need");
            };
            if !KNOWN_KEYS.contains(&name) {
                return reject("unsupported key in need");
            }
        }
    }

    // Validate `cwd` if present (even alongside `clear`).
    let cwd = match raw_cwd {
        Some(raw) => {
            let Some(decoded) = decode_b64_text(raw) else {
                return reject("invalid base64 or utf-8 in cwd");
            };
            if !is_valid_cwd(&decoded) {
                return reject("cwd must be an absolute path");
            }
            Some(decoded)
        }
        None => None,
    };

    match (raw_argv, raw_clear) {
        (Some(_), Some(_)) | (None, None) => reject("must specify exactly one of argv or clear"),
        (None, Some(c)) => {
            if c != b"1" {
                return reject("clear must be 1");
            }
            ParseOutcome::Accept(ParsedReport {
                pid,
                time: report_time,
                kind: ReportKind::Clear,
            })
        }
        (Some(raw), None) => {
            if raw.is_empty() {
                return reject("empty argv");
            }
            let mut argv = Vec::new();
            for (idx, elem) in raw.split(|&b| b == b',').enumerate() {
                if argv.len() >= MAX_ARGV {
                    return reject("too many argv elements");
                }
                if idx == 0 && elem.is_empty() {
                    return reject("empty program name");
                }
                let Some(decoded) = decode_b64_text(elem) else {
                    return reject("invalid base64 or utf-8 in argv");
                };
                if idx == 0 && !is_valid_program(&decoded) {
                    return reject("invalid program name in argv[0]");
                }
                argv.push(decoded);
            }
            ParseOutcome::Accept(ParsedReport {
                pid,
                time: report_time,
                kind: ReportKind::Set { argv, cwd },
            })
        }
    }
}

/// Live OS metadata for a validated owning process.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerSnapshot {
    pub pid: u32,
    pub birth: u64,
    pub process_name: String,
    pub process_exe: String,
    pub process_argv: Vec<String>,
    pub process_cwd: Option<String>,
    pub ancestors: Vec<u32>,
    pub first_process: bool,
    pub wrapped: bool,
    pub uid: u32,
}

/// Validates that `pid` is alive, owned by the daemon's user, belongs to this pane, and shares
/// the same root directory, returning a snapshot of its process metadata.
pub fn inspect_owner(
    pid: u32,
    pane_pid: u32,
    pane_tty_rdev: Option<u64>,
    pane_has_explicit_command: bool,
) -> Result<OwnerSnapshot, &'static str> {
    let my_uid = unsafe { libc::getuid() };
    let my_euid = unsafe { libc::geteuid() };
    if my_uid == 0 || my_euid == 0 {
        return Err("running as root");
    }
    let birth = procinfo::process_birth(pid).ok_or("owner process not alive")?;
    let (ruid, euid) = procinfo::process_uids(pid).ok_or("cannot read owner uid")?;
    if ruid != my_uid || euid != my_euid {
        return Err("owner uid differs");
    }
    let (ancestors, is_descendant) = procinfo::ancestors(pid, pane_pid);
    let on_pane_tty = pane_tty_rdev.is_some() && procinfo::controlling_tty(pid) == pane_tty_rdev;
    if !is_descendant && !on_pane_tty {
        return Err("owner does not belong to pane");
    }
    if !procinfo::same_root(pid, pane_pid) {
        return Err("owner root directory differs");
    }
    let info = procinfo::process_info(pid);
    let process_name = info.as_ref().map(|p| p.name.clone()).unwrap_or_default();
    let process_argv = info.map(|p| p.argv).unwrap_or_default();
    let process_exe = procinfo::exe_path(pid)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| process_name.clone());
    let process_cwd = procinfo::cwd(pid);

    let (first_process, wrapped) = classify_lineage(
        pid,
        pane_pid,
        &ancestors,
        is_descendant,
        pane_has_explicit_command,
        &process_name,
        |anc| procinfo::process_info(anc).map(|p| p.name),
    );

    Ok(OwnerSnapshot {
        pid,
        birth,
        process_name,
        process_exe,
        process_argv,
        process_cwd,
        ancestors,
        first_process,
        wrapped,
        uid: my_euid,
    })
}

fn classify_lineage(
    pid: u32,
    pane_pid: u32,
    ancestors: &[u32],
    is_descendant: bool,
    pane_has_explicit_command: bool,
    owner_name: &str,
    mut name_of: impl FnMut(u32) -> Option<String>,
) -> (bool, bool) {
    if pid == pane_pid {
        return (true, false);
    }
    if !is_descendant {
        // Detached from pane's process tree (reparented to PID 1, matched via tty): treat as wrapped.
        return (false, true);
    }
    // Check macOS `login(1) -> exec program` (the user ran `exec prog` replacing their shell).
    if !pane_has_explicit_command
        && ancestors == [pane_pid]
        && !procinfo::is_shell(owner_name)
        && name_of(pane_pid).as_deref() == Some("login")
    {
        return (true, false);
    }
    // Any ancestor between `pid` and `pane_pid` (inclusive) that is not a shell, login(1), or a
    // known PTY proxy means `pid` runs under a wrapper (sandbox, sudo, nix develop, agent...).
    let wrapped = ancestors.iter().any(|&anc| {
        if anc == pane_pid && !pane_has_explicit_command {
            // The pane root is our own shell or macOS login(1) (which may be unreadable as root).
            return false;
        }
        match name_of(anc) {
            Some(n) => {
                let exe = thurm_config::executable_name(&n);
                !procinfo::is_shell(exe) && !procinfo::is_pty_proxy(exe)
            }
            None => true,
        }
    });
    (false, wrapped)
}

/// One accepted OSC 7577 record held in memory for a pane.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResumeRecord {
    pub pid: u32,
    pub birth: u64,
    pub reported_at: u64,
    pub argv: Vec<String>,
    /// Resolved working directory (report's `cwd` key if given, else OS cwd of owner at accept).
    pub cwd: Option<String>,
    /// True when `cwd` came from the report's `cwd` key (must exist on restore).
    pub cwd_from_report: bool,
    pub first_process: bool,
    pub uid: u32,
    pub process_name: String,
    pub process_exe: String,
    pub process_argv: Vec<String>,
    pub ancestors: Vec<u32>,
    pub wrapped: bool,
    /// Wall-clock ms when the daemon first observed the owning process dead; `None` while alive.
    #[serde(default)]
    pub exited_at_ms: Option<u64>,
    /// Monotonic counter so ties prefer the most recently reported record.
    pub order: u64,
}

impl ResumeRecord {
    pub fn to_saved(&self) -> SavedProgramResume {
        SavedProgramResume {
            argv: self.argv.clone(),
            cwd: self.cwd.clone(),
            cwd_from_report: self.cwd_from_report,
            first_process: self.first_process,
            uid: self.uid,
            reported_at: self.reported_at,
            process_name: self.process_name.clone(),
            process_exe: self.process_exe.clone(),
            process_argv: self.process_argv.clone(),
            wrapped: self.wrapped,
            attempted: false,
        }
    }
}

/// Serializable state for an in-place daemon upgrade (`upgrade.rs`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResumeHandoff {
    pub records: Vec<ResumeRecord>,
    /// `(pid, birth, last_accepted_time_secs)`.
    pub last_times: Vec<(u32, u64, u64)>,
    pub next_order: u64,
}

/// Per-pane OSC 7577 state held by the daemon.
#[derive(Clone, Debug, Default)]
pub struct ProgramResumeState {
    /// Per-pane secret token (`None` when running as root).
    token: Option<String>,
    records: Vec<ResumeRecord>,
    /// `pid -> (birth, last_accepted_time_secs)` to block out-of-order replays within the window.
    last_times: HashMap<u32, (u64, u64)>,
    next_order: u64,
}

impl ProgramResumeState {
    pub fn with_token(token: Option<String>) -> Self {
        Self {
            token,
            records: Vec::new(),
            last_times: HashMap::new(),
            next_order: 1,
        }
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn handoff(&self) -> ResumeHandoff {
        ResumeHandoff {
            records: self.records.clone(),
            last_times: self
                .last_times
                .iter()
                .map(|(&pid, &(birth, t))| (pid, birth, t))
                .collect(),
            next_order: self.next_order,
        }
    }

    pub fn restore_handoff(&mut self, h: ResumeHandoff) {
        self.records = h.records;
        self.last_times = h
            .last_times
            .into_iter()
            .map(|(pid, birth, t)| (pid, (birth, t)))
            .collect();
        self.next_order = h.next_order.max(1);
    }

    /// Processes a raw OSC 7577 report for this pane. Returns `true` if the stored records
    /// changed (so the daemon can persist immediately).
    pub fn handle_report(
        &mut self,
        body: &[u8],
        total_len: usize,
        pane_pid: u32,
        pane_tty_rdev: Option<u64>,
        pane_has_explicit_command: bool,
    ) -> bool {
        if is_root() {
            return false;
        }
        let Some(token) = self.token.as_deref() else {
            return false;
        };
        let now = persist::now_secs();
        let outcome = parse_report(body, total_len, token, now, |pid| {
            let &(stored_birth, last_t) = self.last_times.get(&pid)?;
            // If the PID was reused by a new process, its birth differs: do not carry over.
            let current_birth = procinfo::process_birth(pid);
            (current_birth == Some(stored_birth)).then_some(last_t)
        });
        match outcome {
            ParseOutcome::Ignore => false,
            ParseOutcome::Reject { pid, reason } => {
                log::debug!("OSC 7577 rejected (pid={pid:?}): {reason}");
                pid.is_some_and(|p| self.remove_owner(p))
            }
            ParseOutcome::Accept(parsed) => match parsed.kind {
                ReportKind::Clear => {
                    // Record timestamp for alive owner so replaying an earlier Set is ignored.
                    if let Some(birth) = procinfo::process_birth(parsed.pid) {
                        self.record_time(parsed.pid, birth, parsed.time, now);
                    }
                    self.remove_owner(parsed.pid)
                }
                ReportKind::Set { argv, cwd } => {
                    let owner = match inspect_owner(
                        parsed.pid,
                        pane_pid,
                        pane_tty_rdev,
                        pane_has_explicit_command,
                    ) {
                        Ok(o) => o,
                        Err(reason) => {
                            log::debug!("OSC 7577 rejected pid {}: {reason}", parsed.pid);
                            return self.remove_owner(parsed.pid);
                        }
                    };
                    self.record_time(owner.pid, owner.birth, parsed.time, now);
                    let (resolved_cwd, cwd_from_report) = match cwd {
                        Some(dir) => (Some(dir), true),
                        None => (owner.process_cwd.clone(), false),
                    };
                    let order = self.next_order;
                    self.next_order = self.next_order.saturating_add(1);
                    let rec = ResumeRecord {
                        pid: owner.pid,
                        birth: owner.birth,
                        reported_at: parsed.time,
                        argv,
                        cwd: resolved_cwd,
                        cwd_from_report,
                        first_process: owner.first_process,
                        uid: owner.uid,
                        process_name: owner.process_name,
                        process_exe: owner.process_exe,
                        process_argv: owner.process_argv,
                        ancestors: owner.ancestors,
                        wrapped: owner.wrapped,
                        exited_at_ms: None,
                        order,
                    };
                    self.upsert(rec);
                    true
                }
            },
        }
    }

    fn record_time(&mut self, pid: u32, birth: u64, report_time: u64, now_secs: u64) {
        self.last_times
            .retain(|_, &mut (_, t)| now_secs.abs_diff(t) <= TIME_WINDOW_SECS + 10);
        self.last_times.insert(pid, (birth, report_time));
    }

    fn remove_owner(&mut self, pid: u32) -> bool {
        let before = self.records.len();
        self.records.retain(|r| r.pid != pid);
        self.records.len() != before
    }

    fn upsert(&mut self, rec: ResumeRecord) {
        self.records.retain(|r| r.pid != rec.pid);
        if self.records.len() >= MAX_RECORDS_PER_PANE {
            self.records.remove(0);
        }
        self.records.push(rec);
    }

    /// Checks owning process liveness, expiring records whose owners have been gone longer than
    /// `EXIT_GRACE_MS`. Returns `true` if any record was removed.
    pub fn poll_liveness(&mut self, now_ms: u64) -> bool {
        self.poll_liveness_with(now_ms, procinfo::process_birth)
    }

    fn poll_liveness_with(
        &mut self,
        now_ms: u64,
        mut birth_of: impl FnMut(u32) -> Option<u64>,
    ) -> bool {
        let before = self.records.len();
        self.records.retain_mut(|r| {
            let alive = birth_of(r.pid) == Some(r.birth);
            if alive {
                r.exited_at_ms = None;
                true
            } else {
                let first_gone = *r.exited_at_ms.get_or_insert(now_ms);
                now_ms.saturating_sub(first_gone) < EXIT_GRACE_MS
            }
        });
        self.records.len() != before
    }

    /// Selects the winning record for this pane:
    /// - Ancestors beat their descendants (spec SHOULD).
    /// - Among top-level records, one in the current foreground process group wins if present;
    ///   otherwise the most recently reported one wins.
    pub fn chosen(&self, foreground_pgrp: Option<u32>) -> Option<&ResumeRecord> {
        if self.records.is_empty() {
            return None;
        }
        let pids: HashSet<u32> = self.records.iter().map(|r| r.pid).collect();
        // Exclude any record that has an ancestor also holding a record.
        let top: Vec<&ResumeRecord> = self
            .records
            .iter()
            .filter(|r| !r.ancestors.iter().any(|a| pids.contains(a)))
            .collect();
        let candidates = if top.is_empty() {
            self.records.iter().collect()
        } else {
            top
        };
        if let Some(fg) = foreground_pgrp
            && let Some(&r) = candidates
                .iter()
                .filter(|r| r.pid == fg)
                .max_by_key(|r| r.order)
        {
            return Some(r);
        }
        candidates.into_iter().max_by_key(|r| r.order)
    }

    /// Builds diagnostic info for `Request::ResumeRecords`.
    pub fn inspect(
        &self,
        foreground_pgrp: Option<u32>,
        mode: ResumeProgramsMode,
        rules: &[ResumeRule],
    ) -> Vec<ResumeRecordInfo> {
        let chosen_pid = self.chosen(foreground_pgrp).map(|r| r.pid);
        self.records
            .iter()
            .map(|r| {
                let trust = evaluate_trust(
                    &TrustSubject {
                        argv: &r.argv,
                        cwd: r.cwd.as_deref(),
                        process_name: &r.process_name,
                        process_exe: &r.process_exe,
                        process_argv: &r.process_argv,
                        wrapped: r.wrapped,
                    },
                    mode,
                    rules,
                    false,
                );
                let (trusted, rule, untrusted_reason) = match trust {
                    TrustVerdict::Trusted { rule } => (true, rule, None),
                    TrustVerdict::Untrusted(reason) => (false, None, Some(reason.to_owned())),
                };
                ResumeRecordInfo {
                    pid: r.pid,
                    process_name: r.process_name.clone(),
                    argv: r.argv.clone(),
                    cwd: r.cwd.clone(),
                    cwd_from_report: r.cwd_from_report,
                    first_process: r.first_process,
                    reported_at: r.reported_at,
                    chosen: chosen_pid == Some(r.pid),
                    trusted,
                    rule,
                    untrusted_reason,
                }
            })
            .collect()
    }

    /// Returns the chosen record as a [`SavedProgramResume`] if currently trusted for persistence.
    pub fn chosen_for_save(
        &self,
        foreground_pgrp: Option<u32>,
        mode: ResumeProgramsMode,
        rules: &[ResumeRule],
    ) -> Option<SavedProgramResume> {
        if mode == ResumeProgramsMode::Off || is_root() {
            return None;
        }
        let chosen = self.chosen(foreground_pgrp)?;
        let trust = evaluate_trust(
            &TrustSubject {
                argv: &chosen.argv,
                cwd: chosen.cwd.as_deref(),
                process_name: &chosen.process_name,
                process_exe: &chosen.process_exe,
                process_argv: &chosen.process_argv,
                wrapped: chosen.wrapped,
            },
            mode,
            rules,
            false,
        );
        match trust {
            TrustVerdict::Trusted { .. } => Some(chosen.to_saved()),
            TrustVerdict::Untrusted(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Trust evaluation
// ---------------------------------------------------------------------------------------------

pub struct TrustSubject<'a> {
    pub argv: &'a [String],
    pub cwd: Option<&'a str>,
    pub process_name: &'a str,
    pub process_exe: &'a str,
    pub process_argv: &'a [String],
    pub wrapped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustVerdict {
    Trusted { rule: Option<String> },
    Untrusted(&'static str),
}

/// Evaluates whether a resume record is trusted under `mode` and `rules`.
/// When `verify_fs` is `true` (right before restore), `<under:DIR>` and `dirs` also canonicalize
/// existing filesystem paths to block symlink escapes.
pub fn evaluate_trust(
    subject: &TrustSubject<'_>,
    mode: ResumeProgramsMode,
    rules: &[ResumeRule],
    verify_fs: bool,
) -> TrustVerdict {
    evaluate_trust_with_root(subject, mode, rules, verify_fs, is_root())
}

fn evaluate_trust_with_root(
    subject: &TrustSubject<'_>,
    mode: ResumeProgramsMode,
    rules: &[ResumeRule],
    verify_fs: bool,
    running_as_root: bool,
) -> TrustVerdict {
    if running_as_root {
        return TrustVerdict::Untrusted("running_as_root");
    }
    if !validate_argv(subject.argv) {
        return TrustVerdict::Untrusted("invalid_argv");
    }
    match mode {
        ResumeProgramsMode::Off => TrustVerdict::Untrusted("program_resume_off"),
        ResumeProgramsMode::All => {
            if subject.wrapped {
                TrustVerdict::Untrusted("wrapped")
            } else {
                TrustVerdict::Trusted { rule: None }
            }
        }
        ResumeProgramsMode::Rules => {
            let mut best_reason: Option<&'static str> = None;
            for rule in rules {
                if !rule.enabled || rule.command.is_empty() {
                    continue;
                }
                if rule.command[0] != subject.argv[0] {
                    continue;
                }
                match evaluate_rule(subject, rule, verify_fs) {
                    Ok(()) => {
                        return TrustVerdict::Trusted {
                            rule: (!rule.name.is_empty()).then(|| rule.name.clone()),
                        };
                    }
                    Err(reason) => {
                        best_reason = Some(pick_more_specific(best_reason, reason));
                    }
                }
            }
            TrustVerdict::Untrusted(best_reason.unwrap_or("no_rule"))
        }
    }
}

fn pick_more_specific(prev: Option<&'static str>, next: &'static str) -> &'static str {
    let rank = |r: &str| match r {
        "no_rule" => 0,
        "wrapped" => 1,
        "program_differs" => 2,
        "launch_unreadable" => 3,
        "flag_not_listed" => 4,
        "flag_value" => 5,
        "flags_differ" => 6,
        "dir_not_allowed" => 7,
        _ => 1,
    };
    match prev {
        Some(p) if rank(p) >= rank(next) => p,
        _ => next,
    }
}

fn evaluate_rule(
    subject: &TrustSubject<'_>,
    rule: &ResumeRule,
    verify_fs: bool,
) -> Result<(), &'static str> {
    if subject.wrapped {
        return Err("wrapped");
    }
    if rule.match_process
        && !process_matches_command(
            &rule.command[0],
            subject.process_name,
            subject.process_exe,
            subject.process_argv,
        )
    {
        return Err("program_differs");
    }
    if subject.argv.len() < rule.command.len() {
        return Err("no_rule");
    }
    for (pat, actual) in rule.command.iter().zip(subject.argv.iter()) {
        if !match_element(pat, actual, verify_fs) {
            return Err("no_rule");
        }
    }
    let extra = &subject.argv[rule.command.len()..];
    if rule.flags.is_empty() {
        if !extra.is_empty() {
            return Err("flag_not_listed");
        }
    } else if !extra.is_empty() {
        let reported_flags = parse_reported_flags(extra, &rule.flags)?;
        if subject.process_argv.is_empty() {
            return Err("launch_unreadable");
        }
        let launch_flags = extract_launch_flags(subject.process_argv, &rule.flags);
        if !flags_subset_of(&reported_flags, &launch_flags) {
            return Err("flags_differ");
        }
    }
    if !rule.dirs.is_empty() {
        let Some(cwd) = subject.cwd else {
            return Err("dir_not_allowed");
        };
        if !rule
            .dirs
            .iter()
            .any(|allowed| path_within_dir(cwd, allowed, true, verify_fs))
        {
            return Err("dir_not_allowed");
        }
    }
    Ok(())
}

/// Checks whether a command element matches `pat`:
/// - `"<uuid>"`: canonical 8-4-4-4-12 hex UUID
/// - `"<id>"`: 1..=128 ASCII chars `[A-Za-z0-9][A-Za-z0-9_-]*`
/// - `"<under:DIR>"`: absolute path strictly under `DIR` (`~` expanded)
/// - `"<oneof:a|b>"`: any of the `|`-separated exact strings
/// - anything else: exact literal match
fn match_element(pat: &str, value: &str, verify_fs: bool) -> bool {
    if pat == "<uuid>" {
        return is_uuid(value);
    }
    if pat == "<id>" {
        return is_safe_id(value);
    }
    if let Some(dir) = pat
        .strip_prefix("<under:")
        .and_then(|s| s.strip_suffix('>'))
    {
        return path_within_dir(value, dir, false, verify_fs);
    }
    if let Some(alts) = pat
        .strip_prefix("<oneof:")
        .and_then(|s| s.strip_suffix('>'))
    {
        return alts.split('|').any(|a| a == value);
    }
    pat == value
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if c != b'-' {
                return false;
            }
        } else if !c.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn is_safe_id(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Checks whether `target` is inside `allowed_dir` (`~` expanded).
/// When `allow_equal` is `false` (`<under:DIR>`), `target` must be strictly below `allowed_dir`.
/// Lexically rejects relative paths and `..`/`.` components; when `verify_fs` is `true` (or for
/// `dirs` when paths exist), also verifies canonical paths to stop symlink escapes.
fn path_within_dir(target: &str, allowed_dir: &str, allow_equal: bool, verify_fs: bool) -> bool {
    if !target.starts_with('/') {
        return false;
    }
    let target_path = Path::new(target);
    if target_path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let base = thurm_config::expand_home(allowed_dir);
    if !base.is_absolute()
        || base
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let within = |t: &Path, b: &Path| {
        if allow_equal {
            t.starts_with(b)
        } else {
            t != b && t.starts_with(b)
        }
    };
    let canon_base = std::fs::canonicalize(&base).ok();
    let lexical_ok = within(target_path, &base)
        || canon_base
            .as_deref()
            .is_some_and(|cb| within(target_path, cb));
    if !lexical_ok {
        return false;
    }
    if verify_fs {
        let Some(canon_base) = canon_base else {
            return false;
        };
        let canon_target = std::fs::canonicalize(target_path).or_else(|_| {
            // If the file itself does not exist yet, canonicalize its nearest existing ancestor
            // and append the remaining lexical components.
            let mut cur = target_path.to_path_buf();
            let mut tail = Vec::new();
            while !cur.exists() {
                let name = cur
                    .file_name()
                    .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?
                    .to_os_string();
                tail.push(name);
                if !cur.pop() {
                    return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
                }
            }
            let mut resolved = std::fs::canonicalize(&cur)?;
            for seg in tail.into_iter().rev() {
                resolved.push(seg);
            }
            Ok(resolved)
        });
        let Ok(canon_target) = canon_target else {
            return false;
        };
        return within(&canon_target, &canon_base);
    }
    true
}

/// Checks whether the owning OS process matches the rule's `command[0]`.
fn process_matches_command(
    want: &str,
    process_name: &str,
    process_exe: &str,
    process_argv: &[String],
) -> bool {
    if want.starts_with('/') {
        if process_exe == want {
            return true;
        }
        if let (Ok(a), Ok(b)) = (
            std::fs::canonicalize(process_exe),
            std::fs::canonicalize(want),
        ) && a == b
        {
            return true;
        }
        return false;
    }
    let exe_base = thurm_config::executable_name(process_exe);
    let name_base = thurm_config::executable_name(process_name);
    if exe_base == want || name_base == want {
        return true;
    }
    // Versioned native binaries symlinked under a bare name (e.g. `~/.local/bin/claude` ->
    // `~/.local/share/claude/versions/2.1.332`): match when `argv[0]` names `want` and the
    // resolved exe lives under a directory segment named `want` (or matches `argv[0]` on disk).
    if let Some(arg0) = process_argv.first()
        && thurm_config::executable_name(arg0) == want
    {
        if Path::new(process_exe)
            .components()
            .any(|c| c.as_os_str() == want)
        {
            return true;
        }
        if arg0.starts_with('/')
            && let (Ok(a), Ok(b)) = (
                std::fs::canonicalize(arg0),
                std::fs::canonicalize(process_exe),
            )
            && a == b
        {
            return true;
        }
    }
    // Interpreters (`node /opt/homebrew/bin/claude`, `python3 ...`): check script argument.
    if is_interpreter(exe_base) || is_interpreter(name_base) {
        for arg in process_argv.iter().skip(1) {
            if arg == "--" {
                break;
            }
            if arg.starts_with('-') {
                continue;
            }
            if thurm_config::executable_name(arg) == want {
                return true;
            }
            if known_interpreter_marker(want, arg) {
                return true;
            }
            break;
        }
    }
    false
}

fn is_interpreter(exe: &str) -> bool {
    matches!(
        exe,
        "node" | "bun" | "deno" | "ruby" | "perl" | "uv" | "npx"
    ) || exe.starts_with("python")
}

fn known_interpreter_marker(want: &str, script: &str) -> bool {
    match want {
        "claude" => {
            script.contains("@anthropic-ai/claude-code") || script.contains("claude-code/cli")
        }
        "codex" => script.contains("@openai/codex"),
        "opencode" => script.contains("opencode-ai"),
        "pi" => script.contains("@earendil-works/pi-coding-agent"),
        "agy" => script.contains("antigravity-cli"),
        _ => false,
    }
}

/// Strictly parses extra reported flags against `allowed`. Rejects positional args, `--`,
/// unlisted flags, or wrong value counts.
fn parse_reported_flags(
    extra: &[String],
    allowed: &std::collections::BTreeMap<String, FlagArity>,
) -> Result<Vec<(String, Vec<String>)>, &'static str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < extra.len() {
        let tok = &extra[i];
        if tok == "--" || !tok.starts_with('-') || tok == "-" {
            return Err("flag_not_listed");
        }
        let (flag, inline_val) = split_inline_flag(tok);
        let Some(&arity) = allowed.get(flag) else {
            return Err("flag_not_listed");
        };
        i += 1;
        match (arity, inline_val) {
            (FlagArity::None, Some(_)) => return Err("flag_value"),
            (FlagArity::None, None) => out.push((flag.to_owned(), Vec::new())),
            (FlagArity::One, Some(v)) => out.push((flag.to_owned(), vec![v.to_owned()])),
            (FlagArity::One, None) => {
                let Some(v) = extra.get(i) else {
                    return Err("flag_value");
                };
                i += 1;
                out.push((flag.to_owned(), vec![v.clone()]));
            }
            (FlagArity::Optional, Some(v)) => out.push((flag.to_owned(), vec![v.to_owned()])),
            (FlagArity::Optional, None) => {
                if let Some(v) = extra.get(i)
                    && !v.starts_with('-')
                {
                    i += 1;
                    out.push((flag.to_owned(), vec![v.clone()]));
                } else {
                    out.push((flag.to_owned(), Vec::new()));
                }
            }
            (FlagArity::Many, Some(v)) => out.push((flag.to_owned(), vec![v.to_owned()])),
            (FlagArity::Many, None) => {
                let mut vals = Vec::new();
                while let Some(v) = extra.get(i) {
                    if v.starts_with('-') {
                        break;
                    }
                    vals.push(v.clone());
                    i += 1;
                }
                if vals.is_empty() {
                    return Err("flag_value");
                }
                out.push((flag.to_owned(), vals));
            }
        }
    }
    Ok(out)
}

/// Extracts occurrences of listed flags from the owning process's launch `argv`.
fn extract_launch_flags(
    process_argv: &[String],
    allowed: &std::collections::BTreeMap<String, FlagArity>,
) -> HashMap<String, Vec<Option<String>>> {
    let mut map: HashMap<String, Vec<Option<String>>> = HashMap::new();
    let mut i = 1;
    // If launched via an interpreter (`node /path/to/cli ...`), advance past the script arg.
    if let Some(arg0) = process_argv.first()
        && is_interpreter(thurm_config::executable_name(arg0))
    {
        while let Some(tok) = process_argv.get(i) {
            i += 1;
            if tok == "--" || !tok.starts_with('-') {
                break;
            }
        }
    }
    while i < process_argv.len() {
        let tok = &process_argv[i];
        if tok == "--" {
            break;
        }
        i += 1;
        if !tok.starts_with('-') || tok == "-" {
            continue;
        }
        let (flag, inline_val) = split_inline_flag(tok);
        let Some(&arity) = allowed.get(flag) else {
            continue;
        };
        let entry = map.entry(flag.to_owned()).or_default();
        match (arity, inline_val) {
            (FlagArity::None, None) => entry.push(None),
            (FlagArity::None, Some(_)) => {}
            (FlagArity::One, Some(v)) => entry.push(Some(v.to_owned())),
            (FlagArity::One, None) => {
                if let Some(v) = process_argv.get(i) {
                    i += 1;
                    entry.push(Some(v.clone()));
                }
            }
            (FlagArity::Optional, Some(v)) => entry.push(Some(v.to_owned())),
            (FlagArity::Optional, None) => {
                if let Some(v) = process_argv.get(i)
                    && !v.starts_with('-')
                {
                    i += 1;
                    entry.push(Some(v.clone()));
                } else {
                    entry.push(None);
                }
            }
            (FlagArity::Many, Some(v)) => entry.push(Some(v.to_owned())),
            (FlagArity::Many, None) => {
                while let Some(v) = process_argv.get(i) {
                    if v.starts_with('-') {
                        break;
                    }
                    entry.push(Some(v.clone()));
                    i += 1;
                }
            }
        }
    }
    map
}

fn split_inline_flag(tok: &str) -> (&str, Option<&str>) {
    if tok.starts_with("--")
        && let Some((k, v)) = tok.split_once('=')
    {
        return (k, Some(v));
    }
    (tok, None)
}

fn flags_subset_of(
    reported: &[(String, Vec<String>)],
    launch: &HashMap<String, Vec<Option<String>>>,
) -> bool {
    for (flag, vals) in reported {
        let Some(allowed_vals) = launch.get(flag) else {
            return false;
        };
        if vals.is_empty() {
            if !allowed_vals.iter().any(Option::is_none) {
                return false;
            }
        } else {
            for v in vals {
                if !allowed_vals
                    .iter()
                    .any(|lv| lv.as_deref() == Some(v.as_str()))
                {
                    return false;
                }
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------------------------
// One-shot resume file & shell-safe executor (`thurmd --exec-resume <file>`)
// ---------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ResumeExecSpec {
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub require_cwd: bool,
}

fn resume_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("resume")
}

/// Removes leftover one-shot resume files from previous runs.
pub fn prune_resume_files(state_dir: &Path) {
    let dir = resume_dir(state_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(300))
        .unwrap_or(UNIX_EPOCH);
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff);
        if stale {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Writes a private (`0600`) one-shot resume specification file under `<state_dir>/resume/`.
pub fn write_resume_file(state_dir: &Path, spec: &ResumeExecSpec) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

    let dir = resume_dir(state_dir);
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
    }
    let uid = unsafe { libc::geteuid() };
    let dm = std::fs::metadata(&dir)?;
    if !dm.is_dir() || dm.uid() != uid || (dm.mode() & 0o077) != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "resume directory is not private to current user",
        ));
    }
    let mut rnd = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut rnd)?;
    let hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("resume-{hex}.json"));
    let json = serde_json::to_vec(spec).map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(&json)?;
    f.sync_all()?;
    Ok(path)
}

/// Reads and immediately unlinks a one-shot resume file, verifying ownership and permissions.
pub fn consume_resume_file(path: &Path) -> Result<ResumeExecSpec, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("cannot open resume file: {e}"))?;
    let meta = f
        .metadata()
        .map_err(|e| format!("cannot stat resume file: {e}"))?;
    let uid = unsafe { libc::geteuid() };
    if !meta.is_file() || meta.uid() != uid || (meta.mode() & 0o077) != 0 {
        return Err("resume file is not a private regular file owned by current user".into());
    }
    let mut raw = Vec::new();
    f.by_ref()
        .take((MAX_REPORT_BYTES * 2) as u64)
        .read_to_end(&mut raw)
        .map_err(|e| format!("cannot read resume file: {e}"))?;
    // Strictly one-shot: delete immediately before exec.
    let _ = std::fs::remove_file(path);
    let spec: ResumeExecSpec =
        serde_json::from_slice(&raw).map_err(|e| format!("invalid resume file: {e}"))?;
    if !validate_argv(&spec.argv) {
        return Err("invalid resume argv".into());
    }
    if let Some(dir) = &spec.cwd
        && !is_valid_cwd(dir)
    {
        return Err("invalid resume cwd".into());
    }
    Ok(spec)
}

/// Executes the command in a one-shot resume file via `execvp` (never returns on success).
///
/// Zero shell parsing: `argv` elements are passed verbatim as `CString` pointers directly to
/// `execvp`, bypassing shell aliases, functions, and builtins, while inheriting the shell's
/// full environment (`PATH`, `direnv`, etc.).
pub fn exec_resume_file(path: &Path) -> ! {
    let spec = match consume_resume_file(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("thurm: resume failed: {e}");
            std::process::exit(1);
        }
    };
    if let Some(dir) = &spec.cwd
        && let Err(e) = std::env::set_current_dir(dir)
        && spec.require_cwd
    {
        eprintln!(
            "thurm: cannot resume {}: directory {} unavailable: {e}",
            escape_display(&spec.argv[0]),
            escape_display(dir)
        );
        std::process::exit(1);
    }
    let mut cstrings = Vec::with_capacity(spec.argv.len());
    for arg in &spec.argv {
        match CString::new(arg.as_bytes()) {
            Ok(c) => cstrings.push(c),
            Err(_) => {
                eprintln!("thurm: resume argument contains NUL byte");
                std::process::exit(1);
            }
        }
    }
    let mut ptrs: Vec<*const libc::c_char> = cstrings.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { libc::execvp(cstrings[0].as_ptr(), ptrs.as_ptr()) };
    let err = std::io::Error::last_os_error();
    let code = if err.raw_os_error() == Some(libc::EACCES) {
        126
    } else {
        127
    };
    eprintln!(
        "thurm: cannot resume {}: {err}",
        escape_display(&spec.argv[0])
    );
    std::process::exit(code);
}

/// Whether `shell` (path or basename) can run a restored foreground job inside an interactive shell.
pub fn shell_supports_job_resume(shell: &str) -> bool {
    let name = shell.rsplit('/').next().unwrap_or(shell);
    matches!(
        name,
        "zsh"
            | "bash"
            | "fish"
            | "sh"
            | "ash"
            | "dash"
            | "ksh"
            | "mksh"
            | "oksh"
            | "pdksh"
            | "yash"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_TOKEN: &str = "dummy-token-replace-me";
    const SPEC_TIME: u64 = 1_795_543_200;

    fn sign(prefix: &str, token: &str) -> Vec<u8> {
        let mac = compute_mac(prefix.as_bytes(), token);
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        format!("{prefix}:mac={hex}").into_bytes()
    }

    fn parse_default(body: &[u8], now: u64) -> ParseOutcome {
        let total = 2 + b"7577;".len() + body.len() + 2;
        parse_report(body, total, SPEC_TOKEN, now, |_| None)
    }

    /// All 4 valid examples from the Program Resume Protocol v0.5 specification, using exact MACs.
    #[test]
    fn spec_valid_examples_match_verbatim() {
        // 1. Neovim session
        let nvim = b"pid=74877:time=1795543200:argv=bnZpbQ==,LVM=,L2hvbWUvdXNlci8ubG9jYWwvc3RhdGUvbnZpbS9zZXNzaW9ucy84ZjJjMWQudmlt:mac=9dbb99bfb856749a87563839d7ce9062818d78cf25909db93f309e0cd3b1909b";
        assert_eq!(
            parse_default(nvim, SPEC_TIME),
            ParseOutcome::Accept(ParsedReport {
                pid: 74877,
                time: SPEC_TIME,
                kind: ReportKind::Set {
                    argv: vec![
                        "nvim".into(),
                        "-S".into(),
                        "/home/user/.local/state/nvim/sessions/8f2c1d.vim".into(),
                    ],
                    cwd: None,
                },
            })
        );

        // 2. Neovim clear
        let clear = b"pid=74877:time=1795543200:clear=1:mac=c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93";
        assert_eq!(
            parse_default(clear, SPEC_TIME),
            ParseOutcome::Accept(ParsedReport {
                pid: 74877,
                time: SPEC_TIME,
                kind: ReportKind::Clear,
            })
        );

        // 3. lazygit with cwd
        let lazygit = b"pid=74877:time=1795543200:argv=bGF6eWdpdA==:cwd=L2hvbWUvdXNlci9zcmMvcHJvamVjdA==:mac=35cb909805c380336efd3689a125acc774a9dec8422a1d21492ebec06ca758e1";
        assert_eq!(
            parse_default(lazygit, SPEC_TIME),
            ParseOutcome::Accept(ParsedReport {
                pid: 74877,
                time: SPEC_TIME,
                kind: ReportKind::Set {
                    argv: vec!["lazygit".into()],
                    cwd: Some("/home/user/src/project".into()),
                },
            })
        );

        // 4. resume-wrap ssh devbox
        let wrap = b"pid=90412:time=1795543200:argv=cmVzdW1lLXdyYXA=,c3No,ZGV2Ym94:mac=e76149d0ba54a83570ffab0583867dacdfa23928ba9d18604f0badea88b62b65";
        assert_eq!(
            parse_default(wrap, SPEC_TIME),
            ParseOutcome::Accept(ParsedReport {
                pid: 90412,
                time: SPEC_TIME,
                kind: ReportKind::Set {
                    argv: vec!["resume-wrap".into(), "ssh".into(), "devbox".into()],
                    cwd: None,
                },
            })
        );
    }

    /// All 9 invalid examples from the spec's "Invalid reports" table (Ignore vs Reject).
    #[test]
    fn spec_invalid_table_ignore_vs_reject() {
        // Ignored: wrong MAC
        assert_eq!(
            parse_default(
                b"pid=74877:time=1795543200:argv=bnZpbQ==:mac=0000000000000000000000000000000000000000000000000000000000000000",
                SPEC_TIME
            ),
            ParseOutcome::Ignore
        );
        // Ignored: no MAC
        assert_eq!(
            parse_default(b"pid=74877:time=1795543200:argv=bnZpbQ==", SPEC_TIME),
            ParseOutcome::Ignore
        );
        // Ignored: MAC is not last
        assert_eq!(
            parse_default(
                b"mac=0000000000000000000000000000000000000000000000000000000000000000:pid=74877:time=1795543200:argv=bnZpbQ==",
                SPEC_TIME
            ),
            ParseOutcome::Ignore
        );
        // Ignored: time too far from terminal clock
        let stale = sign("pid=74877:time=1000000000:argv=bnZpbQ==", SPEC_TOKEN);
        assert_eq!(parse_default(&stale, SPEC_TIME), ParseOutcome::Ignore);
        // Ignored: no time key
        let no_time = sign("pid=74877:argv=bnZpbQ==", SPEC_TOKEN);
        assert_eq!(parse_default(&no_time, SPEC_TIME), ParseOutcome::Ignore);
        // Ignored: reserved `?`
        assert_eq!(parse_default(b"?", SPEC_TIME), ParseOutcome::Ignore);

        // Rejected: relative path in argv[0] (`./run.sh`)
        let rel_argv = sign("pid=74877:time=1795543200:argv=Li9ydW4uc2g=", SPEC_TOKEN);
        assert!(matches!(
            parse_default(&rel_argv, SPEC_TIME),
            ParseOutcome::Reject {
                pid: Some(74877),
                ..
            }
        ));
        // Rejected: both argv and clear
        let both = sign(
            "pid=74877:time=1795543200:argv=bnZpbQ==:clear=1",
            SPEC_TOKEN,
        );
        assert!(matches!(
            parse_default(&both, SPEC_TIME),
            ParseOutcome::Reject {
                pid: Some(74877),
                ..
            }
        ));
        // Rejected: relative path in cwd (`project`)
        let rel_cwd = sign(
            "pid=74877:time=1795543200:argv=bnZpbQ==:cwd=cHJvamVjdA==",
            SPEC_TOKEN,
        );
        assert!(matches!(
            parse_default(&rel_cwd, SPEC_TIME),
            ParseOutcome::Reject {
                pid: Some(74877),
                ..
            }
        ));
        // Rejected: unsupported key in `need`
        let need_later = sign(
            "pid=74877:time=1795543200:argv=bnZpbQ==:need=later",
            SPEC_TOKEN,
        );
        assert!(matches!(
            parse_default(&need_later, SPEC_TIME),
            ParseOutcome::Reject {
                pid: Some(74877),
                ..
            }
        ));
    }

    #[test]
    fn parser_edge_cases_padding_controls_order_monotonic_and_limits() {
        // Optional padding accepted, empty middle/trailing argument accepted, keys in any order,
        // unknown key ignored when not in `need`.
        let ok = sign(
            "argv=bnZpbQ,,LVM:future=xyz:need=cwd,argv:cwd=L3RtcA:time=1795543200:pid=42",
            SPEC_TOKEN,
        );
        assert_eq!(
            parse_default(&ok, SPEC_TIME),
            ParseOutcome::Accept(ParsedReport {
                pid: 42,
                time: SPEC_TIME,
                kind: ReportKind::Set {
                    argv: vec!["nvim".into(), "".into(), "-S".into()],
                    cwd: Some("/tmp".into()),
                },
            })
        );

        // Wrong base64 padding is rejected (`bnZpbQ=` needs `==` or none).
        let bad_pad = sign("pid=42:time=1795543200:argv=bnZpbQ=", SPEC_TOKEN);
        assert!(matches!(
            parse_default(&bad_pad, SPEC_TIME),
            ParseOutcome::Reject { pid: Some(42), .. }
        ));

        // Control characters (LF, NUL, DEL, C1 U+0085) rejected.
        for bad_arg in ["a\nb", "a\0b", "a\x7fb", "a\u{0085}b"] {
            let b64 = base64::engine::general_purpose::STANDARD.encode(bad_arg);
            let body = sign(
                &format!("pid=42:time=1795543200:argv=bnZpbQ==,{b64}"),
                SPEC_TOKEN,
            );
            assert!(
                matches!(
                    parse_default(&body, SPEC_TIME),
                    ParseOutcome::Reject { pid: Some(42), .. }
                ),
                "control char {bad_arg:?} should be rejected"
            );
        }

        // Bare command starting with `-` is rejected (`-nvim` -> `LW52aW0=`).
        let dash = sign("pid=42:time=1795543200:argv=LW52aW0=", SPEC_TOKEN);
        assert!(matches!(
            parse_default(&dash, SPEC_TIME),
            ParseOutcome::Reject { pid: Some(42), .. }
        ));

        // Empty `argv=` and leading empty element `argv=,LVM=` rejected.
        for bad in [
            "pid=42:time=1795543200:argv=",
            "pid=42:time=1795543200:argv=,LVM=",
        ] {
            let body = sign(bad, SPEC_TOKEN);
            assert!(matches!(
                parse_default(&body, SPEC_TIME),
                ParseOutcome::Reject { pid: Some(42), .. }
            ));
        }

        // Duplicate key rejected (including duplicate `mac` inside prefix).
        let dup = sign(
            "pid=42:time=1795543200:argv=bnZpbQ==:argv=bnZpbQ==",
            SPEC_TOKEN,
        );
        assert!(matches!(
            parse_default(&dup, SPEC_TIME),
            ParseOutcome::Reject { pid: Some(42), .. }
        ));

        // Uppercase hex in MAC is ignored.
        let mut upper = sign("pid=42:time=1795543200:clear=1", SPEC_TOKEN);
        let len = upper.len();
        for b in &mut upper[len - 64..] {
            *b = b.to_ascii_uppercase();
        }
        assert_eq!(parse_default(&upper, SPEC_TIME), ParseOutcome::Ignore);

        // Monotonic time per pid: strictly older ignored, equal accepted.
        let body_older = sign("pid=42:time=1795543199:clear=1", SPEC_TOKEN);
        let body_same = sign("pid=42:time=1795543200:clear=1", SPEC_TOKEN);
        assert_eq!(
            parse_report(&body_older, 200, SPEC_TOKEN, SPEC_TIME, |p| (p == 42)
                .then_some(SPEC_TIME)),
            ParseOutcome::Ignore
        );
        assert!(matches!(
            parse_report(&body_same, 200, SPEC_TOKEN, SPEC_TIME, |p| (p == 42)
                .then_some(SPEC_TIME)),
            ParseOutcome::Accept(_)
        ));

        // Clock skew boundary: ±60s accepted, ±61s ignored.
        let at_edge = sign("pid=42:time=1795543260:clear=1", SPEC_TOKEN);
        let past_edge = sign("pid=42:time=1795543261:clear=1", SPEC_TOKEN);
        assert!(matches!(
            parse_default(&at_edge, SPEC_TIME),
            ParseOutcome::Accept(_)
        ));
        assert_eq!(parse_default(&past_edge, SPEC_TIME), ParseOutcome::Ignore);

        // Total length limit enforced AFTER mac+time -> Reject (clears pid).
        let ok_body = sign("pid=42:time=1795543200:argv=bnZpbQ==", SPEC_TOKEN);
        assert!(matches!(
            parse_report(
                &ok_body,
                MAX_REPORT_BYTES + 1,
                SPEC_TOKEN,
                SPEC_TIME,
                |_| None
            ),
            ParseOutcome::Reject { pid: Some(42), .. }
        ));

        // Too many argv elements (> MAX_ARGV) -> Reject.
        let many = std::iter::repeat_n("bnZpbQ==", MAX_ARGV + 1)
            .collect::<Vec<_>>()
            .join(",");
        let many_body = sign(&format!("pid=42:time=1795543200:argv={many}"), SPEC_TOKEN);
        assert!(matches!(
            parse_default(&many_body, SPEC_TIME),
            ParseOutcome::Reject { pid: Some(42), .. }
        ));

        // Unusable pid (overflowing i32::MAX) -> Reject with pid: None.
        let big_pid = sign("pid=99999999999:time=1795543200:clear=1", SPEC_TOKEN);
        assert!(matches!(
            parse_default(&big_pid, SPEC_TIME),
            ParseOutcome::Reject { pid: None, .. }
        ));
    }

    #[test]
    fn token_meets_spec_length_and_charset() {
        let t1 = generate_token().unwrap();
        let t2 = generate_token().unwrap();
        assert_ne!(t1, t2);
        assert!((22..=64).contains(&t1.len()), "len={}", t1.len());
        assert!(
            t1.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        );
    }

    #[test]
    fn record_hierarchy_and_exit_grace() {
        let mut state = ProgramResumeState::with_token(Some(SPEC_TOKEN.into()));
        let mk = |pid: u32, ancestors: Vec<u32>, order: u64| ResumeRecord {
            pid,
            birth: pid as u64 * 10,
            reported_at: SPEC_TIME,
            argv: vec![format!("prog{pid}")],
            cwd: None,
            cwd_from_report: false,
            first_process: false,
            uid: 1000,
            process_name: format!("prog{pid}"),
            process_exe: format!("/usr/bin/prog{pid}"),
            process_argv: vec![format!("prog{pid}")],
            ancestors,
            wrapped: false,
            exited_at_ms: None,
            order,
        };
        // Parent (100) and child (200, whose ancestors include 100): ancestor wins even if child
        // reported later and is in the foreground.
        state.upsert(mk(100, vec![10], 1));
        state.upsert(mk(200, vec![100, 10], 2));
        assert_eq!(state.chosen(Some(200)).map(|r| r.pid), Some(100));

        // Sibling (300, not a descendant of 100): foreground wins; otherwise most recent wins.
        state.upsert(mk(300, vec![10], 3));
        assert_eq!(state.chosen(Some(100)).map(|r| r.pid), Some(100));
        assert_eq!(state.chosen(Some(300)).map(|r| r.pid), Some(300));
        assert_eq!(state.chosen(None).map(|r| r.pid), Some(300));

        // Exit grace: owner 300 dies at t=1000 -> kept at t=2500 (<2s), removed at t=3000 (>=2s).
        let birth = |pid: u32| (pid != 300).then_some(pid as u64 * 10);
        assert!(!state.poll_liveness_with(1000, birth));
        assert_eq!(state.records.len(), 3);
        assert!(!state.poll_liveness_with(2500, birth));
        assert_eq!(state.records.len(), 3);
        assert!(state.poll_liveness_with(3000, birth));
        assert_eq!(state.records.len(), 2);
        assert!(!state.records.iter().any(|r| r.pid == 300));
    }

    #[test]
    fn trust_rules_flags_dirs_process_and_symlinks() {
        let rules = thurm_config::builtin_resume_rules();
        let claude_argv = vec![
            "claude".into(),
            "--resume".into(),
            "4394937c-b07a-4bd5-b6d0-034e7091ff1f".into(),
            "--model".into(),
            "opus".into(),
        ];
        // Valid claude launched with --model=opus
        let ok = TrustSubject {
            argv: &claude_argv,
            cwd: Some("/tmp"),
            process_name: "2.1.332",
            process_exe: "/Users/me/.local/share/claude/versions/2.1.332",
            process_argv: &["claude".into(), "--model=opus".into()],
            wrapped: false,
        };
        assert_eq!(
            evaluate_trust_with_root(&ok, ResumeProgramsMode::Rules, &rules, false, false),
            TrustVerdict::Trusted {
                rule: Some("claude".into())
            }
        );
        // Running as root is always refused even in `all` mode.
        assert_eq!(
            evaluate_trust_with_root(&ok, ResumeProgramsMode::All, &rules, false, true),
            TrustVerdict::Untrusted("running_as_root")
        );

        // Escalated flag not present at launch -> rejected (`flags_differ`)
        let escalated = TrustSubject {
            process_argv: &["claude".into()],
            ..ok
        };
        assert_eq!(
            evaluate_trust_with_root(&escalated, ResumeProgramsMode::Rules, &rules, false, false),
            TrustVerdict::Untrusted("flags_differ")
        );

        // Wrong process trying to claim claude -> `program_differs`
        let spoofed_proc = TrustSubject {
            process_name: "bash",
            process_exe: "/bin/bash",
            process_argv: &["bash".into()],
            ..ok
        };
        assert_eq!(
            evaluate_trust_with_root(
                &spoofed_proc,
                ResumeProgramsMode::Rules,
                &rules,
                false,
                false
            ),
            TrustVerdict::Untrusted("program_differs")
        );

        // Wrapped under sandbox/agent -> `wrapped` in both Rules and All modes
        let wrapped = TrustSubject {
            wrapped: true,
            ..ok
        };
        assert_eq!(
            evaluate_trust_with_root(&wrapped, ResumeProgramsMode::Rules, &rules, false, false),
            TrustVerdict::Untrusted("wrapped")
        );
        assert_eq!(
            evaluate_trust_with_root(&wrapped, ResumeProgramsMode::All, &rules, false, false),
            TrustVerdict::Untrusted("wrapped")
        );

        // Symlink escape under `<under:DIR>` blocked when `verify_fs = true`
        let root = std::env::temp_dir().join(format!("thurm-resume-sym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let allowed = root.join("sessions");
        let outside = root.join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let real_outside = outside.join("evil.vim");
        std::fs::write(&real_outside, "").unwrap();
        let link = allowed.join("work.vim");
        std::os::unix::fs::symlink(&real_outside, &link).unwrap();
        let good = allowed.join("ok.vim");
        std::fs::write(&good, "").unwrap();

        let nvim_rule = vec![ResumeRule {
            name: "nvim".into(),
            enabled: true,
            command: vec![
                "nvim".into(),
                "-S".into(),
                format!("<under:{}>", allowed.display()),
            ],
            dirs: Vec::new(),
            flags: Default::default(),
            match_process: true,
        }];
        let link_argv = vec!["nvim".into(), "-S".into(), link.display().to_string()];
        let link_subj = TrustSubject {
            argv: &link_argv,
            cwd: Some("/tmp"),
            process_name: "nvim",
            process_exe: "/usr/bin/nvim",
            process_argv: &["nvim".into()],
            wrapped: false,
        };
        // Lexical passes, canonical symlink check rejects!
        assert_eq!(
            evaluate_trust_with_root(
                &link_subj,
                ResumeProgramsMode::Rules,
                &nvim_rule,
                false,
                false
            ),
            TrustVerdict::Trusted {
                rule: Some("nvim".into())
            }
        );
        assert_eq!(
            evaluate_trust_with_root(
                &link_subj,
                ResumeProgramsMode::Rules,
                &nvim_rule,
                true,
                false
            ),
            TrustVerdict::Untrusted("no_rule")
        );
        let good_argv = vec!["nvim".into(), "-S".into(), good.display().to_string()];
        let good_subj = TrustSubject {
            argv: &good_argv,
            ..link_subj
        };
        assert_eq!(
            evaluate_trust_with_root(
                &good_subj,
                ResumeProgramsMode::Rules,
                &nvim_rule,
                true,
                false
            ),
            TrustVerdict::Trusted {
                rule: Some("nvim".into())
            }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn display_escaping_neutralizes_bidi_and_control_chars() {
        let evil = "nvim \u{202E}txt.exe\n\x1b[31m";
        let escaped = escape_display(evil);
        assert!(!escaped.contains('\u{202E}'));
        assert!(!escaped.contains('\n'));
        assert!(!escaped.contains('\x1b'));
        assert!(escaped.contains("\\u{202E}"));
    }

    #[test]
    fn resume_file_roundtrip_is_one_shot_and_private() {
        let dir = std::env::temp_dir().join(format!("thurm-resume-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let spec = ResumeExecSpec {
            argv: vec!["echo".into(), "$(touch /tmp/nope); a; b".into(), "".into()],
            cwd: Some("/tmp".into()),
            require_cwd: true,
        };
        let path = write_resume_file(&dir, &spec).unwrap();
        let got = consume_resume_file(&path).unwrap();
        assert_eq!(got, spec);
        // Unlinked immediately on read.
        assert!(!path.exists());
        assert!(consume_resume_file(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
