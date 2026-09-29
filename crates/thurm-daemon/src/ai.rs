//! Apple Intelligence's on-device model (`[ai]` in the config), through the
//! `thurm-intelligence` helper next to `thurmd` (the framework is Swift-only).
//!
//! The helper speaks JSON lines on stdio (see `macos/Sources/ThurmIntelligence/main.swift`).
//! It starts on the first request and answers one request at a time; a model that is
//! unavailable (Apple Intelligence off, model still downloading) is asked again after a while.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Longest wait for one answer (the first one loads the model).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an unavailable model is left alone before the helper is tried again.
const RETRY_UNAVAILABLE: Duration = Duration::from_secs(300);
/// Most text sent with a prompt. The model's context is 4096 tokens, instructions and
/// answer included.
const MAX_CONTEXT_CHARS: usize = 6000;

/// One request to the model.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Ask {
    pub instructions: &'static str,
    pub prompt: String,
    /// Constrain the answer to one of these.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<&'static [&'static str]>,
    pub max_tokens: u32,
}

#[derive(Serialize)]
struct Envelope<'a> {
    id: u64,
    #[serde(flatten)]
    ask: &'a Ask,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Reply {
    ready: Option<bool>,
    id: Option<u64>,
    text: Option<String>,
    error: Option<String>,
}

struct Helper {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Inner {
    helper: Option<Helper>,
    next_id: u64,
}

pub struct Model {
    path: Option<PathBuf>,
    /// Held for a whole exchange.
    inner: Mutex<Inner>,
    /// Kept apart so that checking it never waits for an answer.
    unavailable: Mutex<Option<(Instant, String)>>,
}

impl Model {
    /// The helper next to this executable (`THURM_INTELLIGENCE` overrides it).
    pub fn new() -> Self {
        let path = std::env::var_os("THURM_INTELLIGENCE")
            .map(PathBuf::from)
            .or_else(|| {
                let exe = std::env::current_exe().ok()?;
                Some(exe.parent()?.join("thurm-intelligence"))
            });
        Self::with_helper(path)
    }

    pub fn with_helper(path: Option<PathBuf>) -> Self {
        Self {
            path,
            inner: Mutex::new(Inner::default()),
            unavailable: Mutex::new(None),
        }
    }

    /// Stops the helper (`[ai]` turned off); the next request starts it again.
    pub fn stop(&self) {
        *self.unavailable.lock() = None;
        self.inner.lock().helper = None;
    }

    /// Whether requests currently fail fast because the model is unavailable.
    pub fn unavailable(&self) -> Option<String> {
        self.unavailable
            .lock()
            .as_ref()
            .filter(|(at, _)| at.elapsed() < RETRY_UNAVAILABLE)
            .map(|(_, why)| why.clone())
    }

    /// Runs `ask` and returns the model's answer.
    pub fn ask(&self, ask: &Ask) -> Result<String, String> {
        if let Some(why) = self.unavailable() {
            return Err(why);
        }
        let mut inner = self.inner.lock();
        if inner.helper.is_none() {
            match self.start() {
                Ok(h) => inner.helper = Some(h),
                Err(why) => {
                    log::warn!("on-device model unavailable: {why}");
                    *self.unavailable.lock() = Some((Instant::now(), why.clone()));
                    return Err(why);
                }
            }
        }
        inner.next_id += 1;
        let id = inner.next_id;
        let result = Self::exchange(inner.helper.as_mut().expect("started"), id, ask);
        if result.as_ref().is_err_and(|e| e.fatal) {
            // Gone or hung: start over with the next request.
            inner.helper = None;
        }
        result.map_err(|e| e.message)
    }

    fn start(&self) -> Result<Helper, String> {
        let path = self
            .path
            .as_ref()
            .filter(|p| p.is_file())
            .ok_or("thurm-intelligence is not installed next to thurmd")?;
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", path.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, lines) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("ai-helper".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let helper = Helper {
            child,
            stdin,
            lines,
        };
        let first = helper
            .lines
            .recv_timeout(ANSWER_TIMEOUT)
            .map_err(|_| "thurm-intelligence did not start".to_owned())?;
        let reply: Reply = serde_json::from_str(&first).unwrap_or_default();
        match reply.ready {
            Some(true) => {
                log::info!("on-device model ready");
                Ok(helper)
            }
            _ => Err(reply
                .error
                .unwrap_or_else(|| "the on-device model is unavailable".into())),
        }
    }

    fn exchange(helper: &mut Helper, id: u64, ask: &Ask) -> Result<String, ExchangeError> {
        let mut line = serde_json::to_string(&Envelope { id, ask }).map_err(ExchangeError::soft)?;
        line.push('\n');
        helper
            .stdin
            .write_all(line.as_bytes())
            .and_then(|()| helper.stdin.flush())
            .map_err(ExchangeError::fatal)?;
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = helper.lines.recv_timeout(left).map_err(|e| match e {
                RecvTimeoutError::Timeout => ExchangeError::fatal("the model took too long"),
                RecvTimeoutError::Disconnected => ExchangeError::fatal("thurm-intelligence exited"),
            })?;
            let reply: Reply = serde_json::from_str(&line).unwrap_or_default();
            // A late answer to a request that timed out earlier.
            if reply.id != Some(id) {
                continue;
            }
            return match (reply.text, reply.error) {
                (Some(text), _) => Ok(text),
                (None, e) => Err(ExchangeError::soft(e.unwrap_or_else(|| "no answer".into()))),
            };
        }
    }
}

struct ExchangeError {
    message: String,
    fatal: bool,
}

impl ExchangeError {
    fn fatal(e: impl ToString) -> Self {
        Self {
            message: e.to_string(),
            fatal: true,
        }
    }

    fn soft(e: impl ToString) -> Self {
        Self {
            message: e.to_string(),
            fatal: false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------------------------

/// The status fallback's answers (plain yes/no: the model is much better at those than at
/// descriptive labels).
pub const WAITING: &str = "yes";
pub const IDLE: &str = "no";
const STATUS_CHOICES: &[&str] = &[WAITING, IDLE];

/// A short tab title for an agent session.
pub fn title(agent: &str, prompt: Option<&str>, screen: &str) -> Ask {
    let mut text = String::new();
    if let Some(p) = prompt {
        text.push_str(&format!("The user's request:\n{}\n\n", clip(p, 1500)));
    }
    text.push_str(&format!(
        "The terminal running {agent}:\n{}\n\nTitle:",
        clip(screen, MAX_CONTEXT_CHARS - text.len().min(1500))
    ));
    Ask {
        instructions: "You name terminal tabs. Given what a user asked a coding agent, reply \
            with a title of 2 to 5 words for the task, like \"Fix flaky login test\" or \
            \"Add CSV export\". Reply with the title only: no quotes, no punctuation at the \
            end. If no task is visible, reply NONE.",
        prompt: text,
        choices: None,
        max_tokens: 20,
    }
}

/// Is a quiet agent waiting for the user to answer something?
pub fn status(agent: &str, screen: &str) -> Ask {
    let screen = crate::agents::current_activity(agent, screen);
    Ask {
        instructions: "You classify terminal screens. Answer yes if the bottom of the screen \
            has a CURRENT, UNANSWERED question or asks the user to approve, confirm, or choose between \
            options (for example 'Allow?', '(y/n)', 'Which do you prefer?', or a numbered list \
            to pick from). Earlier requests followed by command output or a completion message \
            are already resolved: answer no. A completed command can contain a question in its \
            text; that is not a pending request. Answer no for results, a banner, an input \
            placeholder, or an empty input line. If no pending request is clear, answer no.",
        prompt: format!(
            "The screen of {agent}:\n{}\n\nDoes the bottom of this screen ask the user a \
             question, or to approve, confirm, or choose?",
            clip(screen, MAX_CONTEXT_CHARS)
        ),
        choices: Some(STATUS_CHOICES),
        max_tokens: 10,
    }
}

/// What the agent asks the user for, in one line.
pub fn attention(agent: &str, hint: Option<&str>, screen: &str) -> Ask {
    let screen = crate::agents::current_activity(agent, screen);
    let hint = hint
        .map(|h| format!("The agent's own notification: {h}\n"))
        .unwrap_or_default();
    Ask {
        instructions: "You write macOS notifications for a developer. A coding agent in a \
            terminal may be waiting for the user. Look only at the bottom of the screen: if it \
            shows a pending question, approval prompt or list of options, say in one short \
            sentence (at most 15 words) what the agent needs, naming the exact command, file \
            or choice shown there, e.g. \"Approve `rm -rf build`?\" or \"Asks \
            which database to use: Postgres or SQLite\". Earlier output and commands that \
            already ran don't count. If the bottom of the screen asks nothing, reply NONE. \
            Reply with the sentence or NONE only.",
        prompt: format!(
            "{hint}The screen of {agent}:\n{}\n\nNotification:",
            clip(screen, MAX_CONTEXT_CHARS)
        ),
        choices: None,
        max_tokens: 40,
    }
}

/// What a finished agent turn did, in one line.
pub fn summary(agent: &str, screen: &str) -> Ask {
    Ask {
        instructions: "You write macOS notifications for a developer. A coding agent in a \
            terminal just finished a turn. In one short sentence (at most 15 words), say what \
            it did or concluded, using the screen's facts (files, tests, errors), e.g. \
            \"Refactored auth middleware; 3 tests still fail\". Reply with the sentence only.",
        prompt: format!(
            "The screen of {agent}:\n{}\n\nNotification:",
            clip(screen, MAX_CONTEXT_CHARS)
        ),
        choices: None,
        max_tokens: 40,
    }
}

/// What happened in the last command.
pub fn explain(output: &str, exit: Option<i32>) -> Ask {
    let status = match exit {
        Some(0) => "It exited successfully.".to_owned(),
        Some(c) => format!("It failed with exit status {c}."),
        None => String::new(),
    };
    Ask {
        instructions: "You help a developer read terminal output. Explain what happened in \
            the command below in 2 to 4 short sentences: for a failure, the error that \
            matters and its most likely cause, quoting the relevant line. Only use what the \
            output shows; say so when it does not show the cause. Plain text, no markdown \
            headings.",
        prompt: format!(
            "The command, then its output. {status}\n\n{}",
            clip(output, MAX_CONTEXT_CHARS)
        ),
        choices: None,
        max_tokens: 200,
    }
}

/// The end of `text`, at most `max` bytes (the newest output matters most).
fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    // Start on a whole line.
    match text[start..].find('\n') {
        Some(nl) if nl + 1 < text.len() - start => &text[start + nl + 1..],
        _ => &text[start..],
    }
}

/// A usable tab title from the model's answer, or `None`.
pub fn clean_title(answer: &str) -> Option<String> {
    let line = clean_line(answer, 60)?;
    let line = line
        .strip_prefix("Title:")
        .map(str::trim)
        .unwrap_or(&line)
        .trim_end_matches(['.', '!'])
        .to_owned();
    let words = line.split_whitespace().count();
    if words == 0 || words > 7 || line.eq_ignore_ascii_case("none") {
        return None;
    }
    Some(line)
}

/// A notification line from the model's answer; `None` when it saw nothing to say (NONE).
pub fn clean_detail(answer: &str) -> Option<String> {
    clean_line(answer, 140).filter(|l| !l.trim_end_matches('.').eq_ignore_ascii_case("none"))
}

/// The first line of the model's answer without wrapping quotes, cut to `max` characters.
pub fn clean_line(answer: &str, max: usize) -> Option<String> {
    let line = answer.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line
        .trim_matches(|c| matches!(c, '"' | '“' | '”' | '\''))
        .trim();
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= max {
        return Some(line.to_owned());
    }
    let cut: String = line.chars().take(max - 1).collect();
    Some(format!("{}…", cut.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_cleaned() {
        assert_eq!(
            clean_title("\"Fix flaky login test.\"\n").as_deref(),
            Some("Fix flaky login test")
        );
        assert_eq!(
            clean_title("Title: Add CSV export").as_deref(),
            Some("Add CSV export")
        );
        assert_eq!(clean_title("NONE"), None);
        assert_eq!(clean_title("  \n"), None);
        assert_eq!(
            clean_title("This is far too long to be a tab title for anything"),
            None
        );
    }

    #[test]
    fn nothing_to_say() {
        assert_eq!(clean_detail("NONE"), None);
        assert_eq!(clean_detail("None."), None);
        assert_eq!(
            clean_detail("Asks which file to edit").as_deref(),
            Some("Asks which file to edit")
        );
    }

    #[test]
    fn lines_cleaned_and_cut() {
        assert_eq!(
            clean_line("\n  “Hello”  \nmore", 20).as_deref(),
            Some("Hello")
        );
        assert_eq!(clean_line("abcdefghij", 5).as_deref(), Some("abcd…"));
    }

    #[test]
    fn clip_keeps_the_end_on_a_line() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("line one\nline two\nline three", 14), "line three");
        // Multi-byte text is cut on a character boundary.
        assert_eq!(clip("ééééé", 3), "é");
    }

    #[test]
    fn status_and_attention_exclude_completed_codex_requests() {
        let screen = include_str!("../tests/fixtures/codex-completed-approval.txt");
        for ask in [status("Codex", screen), attention("Codex", None, screen)] {
            assert!(ask.prompt.contains("Pushed to main"));
            assert!(!ask.prompt.contains("Would you like to run"));
            assert!(!ask.prompt.contains("git commit"));
        }
        // Summaries still need the command results.
        assert!(summary("Codex", screen).prompt.contains("git commit"));
    }

    #[test]
    fn prompts_fit_the_context() {
        let screen = "x".repeat(20_000);
        for ask in [
            title("Codex", Some(&"p".repeat(5000)), &screen),
            status("Codex", &screen),
            attention("Codex", Some("hint"), &screen),
            summary("Codex", &screen),
            explain(&screen, Some(1)),
        ] {
            assert!(
                ask.prompt.len() < MAX_CONTEXT_CHARS + 400,
                "{}",
                ask.prompt.len()
            );
        }
        assert_eq!(status("Codex", "").choices, Some(STATUS_CHOICES));
    }

    /// Against the real model: `THURM_INTELLIGENCE=…/thurm-intelligence cargo test -p
    /// thurm-daemon real_model -- --ignored --nocapture` on a Mac with Apple Intelligence.
    #[test]
    #[ignore]
    fn real_model() {
        let model = Model::new();
        let ask = |a: Ask| {
            let t = model.ask(&a).unwrap();
            println!("{:?} -> {t:?}", &a.instructions[..30]);
            t
        };
        let permission = "• I'll remove the stale build output.\n\n  $ rm -rf build/\n\nAllow \
            Codex to run this command?\n  › 1. Yes, just this once\n    2. Yes, always\n    \
            3. No, tell Codex what to do differently";
        let empty = "╭──────────────────────────╮\n│ >_ OpenAI Codex (v0.50)  │\n│ model: gpt-5 \
            │\n╰──────────────────────────╯\n\n  To get started, describe a task\n\n› \n\n  \
            ⏎ send   ⌃J newline   ⌃T transcript   ⌃C quit";
        let turn = "› the login test is flaky, fix it\n\n• Ran cargo test -p auth login\n  └ test \
            login::retries ... FAILED (timeout)\n• Edited src/auth/login.rs (+12 -4)\n  Made \
            the retry wait on the mock clock instead of sleeping.\n• Ran cargo test -p auth\n  \
            └ test result: ok. 24 passed; 0 failed\n\n  The flaky test now passes \
            reliably.\n\n› ";
        assert_eq!(ask(status("Codex", permission)), WAITING);
        assert_eq!(ask(status("Codex", empty)), IDLE);
        assert_eq!(ask(status("Codex", turn)), IDLE);
        let completed = include_str!("../tests/fixtures/codex-completed-approval.txt");
        assert_eq!(ask(status("Codex", completed)), IDLE);
        assert!(clean_detail(&ask(attention("Codex", None, completed))).is_none());
        let question = "• Read the configuration.\n• Which database should I use?\n  1. Postgres\n  2. SQLite\n› ";
        assert_eq!(ask(status("Codex", question)), WAITING);
        assert!(clean_title(&ask(title("Codex", None, turn))).is_some());
        assert!(
            clean_title(&ask(title(
                "Codex",
                Some("add a CSV export to the reports page"),
                empty
            )))
            .is_some()
        );
        assert!(clean_line(&ask(attention("Codex", None, permission)), 140).is_some());
        assert!(clean_line(&ask(summary("Codex", turn)), 140).is_some());
        let output = "$ cargo build\n   Compiling app v0.1.0\nerror[E0425]: cannot find value \
            `cfg` in this scope\n  --> src/main.rs:12:5\n   |\n12 |     cfg.load();\n   |     \
            ^^^ not found in this scope\nerror: could not compile `app` due to 1 previous error";
        assert!(!ask(explain(output, Some(101))).is_empty());
    }

    #[cfg(unix)]
    fn fake_helper(name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("thurm-ai-{name}-{}", std::process::id()));
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn talks_to_the_helper() {
        let path = fake_helper(
            "ok",
            r#"echo '{"ready":true}'
while read -r line; do
  id=$(printf '%s' "$line" | sed -E 's/.*"id":([0-9]+).*/\1/')
  case "$line" in
    *choices*) echo "{\"id\":$id,\"text\":\"no\"}" ;;
    *fail*) echo "{\"id\":$id,\"error\":\"guardrail\"}" ;;
    *) echo "{\"id\":$id,\"text\":\"Fix login bug\"}" ;;
  esac
done
"#,
        );
        let model = Model::with_helper(Some(path.clone()));
        assert_eq!(
            model.ask(&title("Codex", None, "fix the login")).unwrap(),
            "Fix login bug"
        );
        assert_eq!(model.ask(&status("Codex", "> ")).unwrap(), IDLE);
        assert_eq!(
            model.ask(&summary("Codex", "fail")).unwrap_err(),
            "guardrail"
        );
        // A soft error keeps the helper: the next request still works.
        assert_eq!(model.ask(&status("Codex", "> ")).unwrap(), IDLE);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_model_fails_fast() {
        let path = fake_helper(
            "off",
            r#"echo '{"ready":false,"error":"Apple Intelligence is turned off"}'"#,
        );
        let model = Model::with_helper(Some(path.clone()));
        let ask = status("Codex", "> ");
        assert_eq!(
            model.ask(&ask).unwrap_err(),
            "Apple Intelligence is turned off"
        );
        assert_eq!(
            model.unavailable().as_deref(),
            Some("Apple Intelligence is turned off")
        );
        assert!(model.ask(&ask).is_err());
        model.stop();
        assert_eq!(model.unavailable(), None);
        let _ = std::fs::remove_file(path);

        let missing = Model::with_helper(Some("/nonexistent/thurm-intelligence".into()));
        assert!(missing.ask(&ask).is_err());
    }
}
