//! Tab completion for the shell's command line, after tty7: commands from `$PATH`, flags and
//! subcommands (with descriptions) from command specs converted from Fig's autocomplete corpus
//! (MIT), values from the specs' generator scripts, and paths.
//!
//! Specs are JSON files named `<command>.json`, looked up in `$THURM_COMPLETIONS_DIR`,
//! `~/.config/thurm/completions` and the app bundle's `Resources/completions`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use thurm_proto::{CompletionItem, CompletionKind, Completions};

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
struct Spec {
    description: Option<String>,
    options: Vec<Opt>,
    subcommands: Vec<Sub>,
    args: Vec<Arg>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
struct Sub {
    names: Vec<String>,
    description: Option<String>,
    hidden: bool,
    options: Vec<Opt>,
    subcommands: Vec<Sub>,
    args: Vec<Arg>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
struct Opt {
    names: Vec<String>,
    description: Option<String>,
    args: Vec<Arg>,
    hidden: bool,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
struct Arg {
    template: Vec<String>,
    suggestions: Vec<Suggestion>,
    generators: Vec<Generator>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
struct Suggestion {
    names: Vec<String>,
    description: Option<String>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
struct Generator {
    script: Vec<String>,
}

/// A spec node while walking the command line (the root spec or a subcommand).
struct Node<'a> {
    options: &'a [Opt],
    subcommands: &'a [Sub],
    args: &'a [Arg],
}

const MAX_ITEMS: usize = 300;

/// Completions for `line` (the text before the cursor) in `cwd`. `path` is the shell's `$PATH`
/// (the daemon's own is launchd's minimal one).
pub fn complete(line: &str, cwd: &Path, path: Option<&str>) -> Completions {
    let path = path
        .map(str::to_owned)
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let segment = last_segment(line);
    let (words, current) = split_words(segment);
    let mut items = if words.is_empty() {
        commands(&current, &path)
    } else {
        let command = basename(&words[0]);
        match load_spec(command) {
            Some(spec) => from_spec(&spec, &words[1..], &current, cwd, &path),
            None => paths(&current, cwd, dirs_only(command)),
        }
    };
    items.sort_by(|a, b| a.text.cmp(&b.text));
    items.dedup_by(|a, b| a.text == b.text);
    items.truncate(MAX_ITEMS);
    Completions {
        word: current,
        items,
    }
}

fn dirs_only(command: &str) -> bool {
    matches!(command, "cd" | "pushd" | "rmdir" | "z" | "zoxide")
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// The part of the line after the last `|`, `&&`, `||` or `;` outside quotes.
fn last_segment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let (mut quote, mut start, mut i) = (None::<u8>, 0, 0);
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'\'' | b'"' => quote = Some(b),
                b'\\' => i += 1,
                b'|' | b';' | b'&' => start = i + 1,
                _ => {}
            },
        }
        i += 1;
    }
    &line[start.min(line.len())..]
}

/// Completed words, and the word under the cursor ("" after a space). Quotes are removed.
fn split_words(s: &str) -> (Vec<String>, String) {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote = None::<char>;
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                    in_word = true;
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                }
                c => {
                    cur.push(c);
                    in_word = true;
                }
            },
        }
    }
    (words, cur)
}

fn item(
    text: impl Into<String>,
    kind: CompletionKind,
    description: Option<String>,
) -> CompletionItem {
    CompletionItem {
        text: text.into(),
        kind,
        description: description.filter(|d| !d.is_empty()),
    }
}

/// Executables on `$PATH` (and a few shell builtins) starting with `prefix`.
fn commands(prefix: &str, path: &str) -> Vec<CompletionItem> {
    if prefix.contains('/') {
        return paths(prefix, Path::new("."), false);
    }
    const BUILTINS: &[&str] = &[
        "alias", "builtin", "cd", "command", "echo", "eval", "exec", "exit", "export", "history",
        "jobs", "popd", "pushd", "set", "source", "type", "unset",
    ];
    let mut out: Vec<CompletionItem> = BUILTINS
        .iter()
        .filter(|b| b.starts_with(prefix))
        .map(|b| item(*b, CompletionKind::Command, None))
        .collect();
    if prefix.is_empty() {
        return out;
    }
    for dir in std::env::split_paths(path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(prefix) || name.starts_with('.') {
                continue;
            }
            use std::os::unix::fs::PermissionsExt;
            // Follow symlinks (nix profiles and Homebrew are all links).
            if std::fs::metadata(e.path())
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            {
                out.push(item(name, CompletionKind::Command, None));
            }
        }
    }
    out
}

/// Entries of the directory part of `prefix`, relative to `cwd` (`~` expanded). Directories get
/// a trailing `/`; dotfiles only when the name part starts with `.`.
fn paths(prefix: &str, cwd: &Path, dirs_only: bool) -> Vec<CompletionItem> {
    let (dir_part, name_part) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("", prefix),
    };
    let dir = if dir_part.is_empty() {
        cwd.to_path_buf()
    } else if let Some(rest) = dir_part.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
    } else if dir_part.starts_with('/') {
        PathBuf::from(dir_part)
    } else {
        cwd.join(dir_part)
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with(name_part) || (name.starts_with('.') && !name_part.starts_with('.')) {
            continue;
        }
        let is_dir = e.path().is_dir();
        if dirs_only && !is_dir {
            continue;
        }
        let text = format!("{dir_part}{name}{}", if is_dir { "/" } else { "" });
        out.push(item(
            text,
            if is_dir {
                CompletionKind::Directory
            } else {
                CompletionKind::File
            },
            None,
        ));
    }
    out
}

fn from_spec(
    spec: &Spec,
    args: &[String],
    current: &str,
    cwd: &Path,
    path: &str,
) -> Vec<CompletionItem> {
    // Walk subcommands; skip options (and the values of options that take one).
    let mut node = Node {
        options: &spec.options,
        subcommands: &spec.subcommands,
        args: &spec.args,
    };
    let mut pending_value: Option<&Opt> = None;
    let mut positional = 0usize;
    for w in args {
        if pending_value.take().is_some() {
            continue;
        }
        if w.starts_with('-') {
            let name = w.split('=').next().unwrap_or(w);
            if let Some(o) = node
                .options
                .iter()
                .find(|o| o.names.iter().any(|n| n == name))
                && !o.args.is_empty()
                && !w.contains('=')
            {
                pending_value = Some(o);
            }
            continue;
        }
        if let Some(sub) = node
            .subcommands
            .iter()
            .find(|s| s.names.iter().any(|n| n == w))
        {
            node = Node {
                options: &sub.options,
                subcommands: &sub.subcommands,
                args: &sub.args,
            };
            positional = 0;
            continue;
        }
        positional += 1;
    }
    // The value of an option like `git -C <path>`.
    if let Some(opt) = pending_value {
        return arg_values(&opt.args[0], current, cwd, path);
    }
    let mut out = Vec::new();
    if current.starts_with('-') {
        for o in node.options.iter().filter(|o| !o.hidden) {
            for n in o.names.iter().filter(|n| n.starts_with(current)) {
                out.push(item(n.clone(), CompletionKind::Flag, o.description.clone()));
            }
        }
        return out;
    }
    for s in node.subcommands.iter().filter(|s| !s.hidden) {
        if let Some(n) = s.names.iter().find(|n| n.starts_with(current)) {
            out.push(item(
                n.clone(),
                CompletionKind::Subcommand,
                s.description.clone(),
            ));
        }
    }
    let arg = node.args.get(positional).or_else(|| node.args.last());
    if let Some(arg) = arg {
        out.extend(arg_values(arg, current, cwd, path));
    } else if node.subcommands.is_empty() {
        out.extend(paths(current, cwd, false));
    }
    out
}

/// Suggestions, generator output and path templates of one argument.
fn arg_values(arg: &Arg, current: &str, cwd: &Path, path: &str) -> Vec<CompletionItem> {
    let mut out = Vec::new();
    for s in &arg.suggestions {
        for n in s.names.iter().filter(|n| n.starts_with(current)) {
            out.push(item(
                n.clone(),
                CompletionKind::Value,
                s.description.clone(),
            ));
        }
    }
    for g in &arg.generators {
        for line in run_generator(&g.script, cwd, path) {
            // `git branch` marks the current branch with "* ".
            let v = line.trim().trim_start_matches("* ").trim();
            if v.is_empty() || v.contains(" -> ") {
                continue;
            }
            let remote = v.starts_with("remotes/");
            let v = v.strip_prefix("remotes/").unwrap_or(v);
            // A remote branch is also offered by its short name (git's checkout/switch DWIM).
            let short = v.split_once('/').map(|(_, b)| b).filter(|_| remote);
            for c in [Some(v), short].into_iter().flatten() {
                if c.starts_with(current) {
                    out.push(item(c.to_owned(), CompletionKind::Value, None));
                }
            }
        }
    }
    if arg.template.iter().any(|t| t == "filepaths") {
        out.extend(paths(current, cwd, false));
    } else if arg.template.iter().any(|t| t == "folders") {
        out.extend(paths(current, cwd, true));
    }
    out
}

/// Output lines of a generator script, run in `cwd` with a 600 ms timeout, cached for 5 s.
fn run_generator(script: &[String], cwd: &Path, path: &str) -> Vec<String> {
    type Cache = Mutex<HashMap<(Vec<String>, PathBuf), (Instant, Vec<String>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let Some((program, args)) = script.split_first() else {
        return Vec::new();
    };
    let key = (script.to_vec(), cwd.to_path_buf());
    let cache = CACHE.get_or_init(Default::default);
    if let Some((at, lines)) = cache.lock().unwrap().get(&key)
        && at.elapsed() < Duration::from_secs(5)
    {
        return lines.clone();
    }
    // Command::new looks the program up in *our* PATH; use the shell's.
    let program = std::env::split_paths(path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(program));
    let Ok(mut child) = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("PATH", path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let deadline = Instant::now() + Duration::from_millis(600);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let mut out = String::new();
    if let Some(s) = child.stdout.take() {
        use std::io::Read;
        let _ = s.take(4 << 20).read_to_string(&mut out);
    }
    let lines: Vec<String> = out.lines().map(str::to_owned).collect();
    cache
        .lock()
        .unwrap()
        .insert(key, (Instant::now(), lines.clone()));
    lines
}

/// Directories searched for `<command>.json`.
fn spec_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("THURM_COMPLETIONS_DIR") {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(thurm_config::config_dir().join("completions"));
    if let Ok(exe) = std::env::current_exe()
        && let Some(contents) = exe.parent().and_then(Path::parent)
    {
        // Thurm.app/Contents/Helpers/thurmd → Thurm.app/Contents/Resources/completions
        dirs.push(contents.join("Resources/completions"));
    }
    dirs
}

fn load_spec(command: &str) -> Option<Spec> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Spec>>>> = OnceLock::new();
    if command.is_empty() || command.contains(['/', '.']) {
        return None;
    }
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(command) {
        return hit.clone();
    }
    let spec = spec_dirs().into_iter().find_map(|d| {
        let text = std::fs::read_to_string(d.join(format!("{command}.json"))).ok()?;
        serde_json::from_str::<Spec>(&text).ok()
    });
    let _ = spec.as_ref().map(|s| &s.description);
    cache
        .lock()
        .unwrap()
        .insert(command.to_owned(), spec.clone());
    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_segments() {
        assert_eq!(last_segment("ls | grep 'a|b' && git ch"), " git ch");
        let (w, c) = split_words(" git commit -m \"a b\" --am");
        assert_eq!(w, vec!["git", "commit", "-m", "a b"]);
        assert_eq!(c, "--am");
        let (w, c) = split_words("cd ");
        assert_eq!((w, c.as_str()), (vec!["cd".to_owned()], ""));
    }

    #[test]
    fn paths_dirs_and_dotfiles() {
        let dir = std::env::temp_dir().join(format!("thurm-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join(".env"), "").unwrap();
        let texts = |v: Vec<CompletionItem>| v.into_iter().map(|i| i.text).collect::<Vec<_>>();
        let mut got = texts(paths("src/", &dir, false));
        got.sort();
        assert_eq!(got, vec!["src/main.rs", "src/nested/"]);
        assert_eq!(texts(paths("src/", &dir, true)), vec!["src/nested/"]);
        assert!(!texts(paths("", &dir, false)).contains(&".env".to_owned()));
        assert!(texts(paths(".", &dir, false)).contains(&".env".to_owned()));
        let c = complete("cd sr", &dir, None);
        assert_eq!(c.word, "sr");
        assert_eq!(texts(c.items), vec!["src/"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spec_walk_flags_and_subcommands() {
        let spec: Spec = serde_json::from_str(
            r#"{"options":[{"names":["-C"],"args":[{"template":["folders"]}]},{"names":["--version"],"description":"Output version"}],
                "subcommands":[{"names":["commit"],"description":"Record changes","options":[{"names":["--amend"],"description":"Amend"}]},
                               {"names":["checkout"],"args":[{"suggestions":[{"names":["-"],"description":"last branch"}]}]}]}"#,
        )
        .unwrap();
        let cwd = std::env::temp_dir();
        let texts = |v: Vec<CompletionItem>| v.into_iter().map(|i| i.text).collect::<Vec<_>>();
        assert_eq!(
            texts(from_spec(&spec, &[], "c", &cwd, "")),
            vec!["commit", "checkout"]
        );
        let flags = from_spec(&spec, &["commit".into()], "--a", &cwd, "");
        assert_eq!(texts(flags.clone()), vec!["--amend"]);
        assert_eq!(flags[0].description.as_deref(), Some("Amend"));
        assert_eq!(
            texts(from_spec(&spec, &["checkout".into()], "", &cwd, "")),
            vec!["-"]
        );
        // `-C <path>` takes a folder, then subcommands again.
        assert!(
            from_spec(&spec, &["-C".into()], "", &cwd, "")
                .iter()
                .all(|i| i.kind == CompletionKind::Directory)
        );
        assert_eq!(
            texts(from_spec(
                &spec,
                &["-C".into(), "/tmp".into()],
                "comm",
                &cwd,
                ""
            )),
            vec!["commit"]
        );
    }

    #[test]
    fn bundled_specs_parse() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../completions");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            if e.path().extension().is_some_and(|x| x == "json") {
                let text = std::fs::read_to_string(e.path()).unwrap();
                serde_json::from_str::<Spec>(&text)
                    .unwrap_or_else(|err| panic!("{:?}: {err}", e.path()));
                n += 1;
            }
        }
        assert!(n >= 90, "only {n} specs");
        // git's subcommands and their flags come through.
        let git: Spec =
            serde_json::from_str(&std::fs::read_to_string(dir.join("git.json")).unwrap()).unwrap();
        let cwd = std::env::temp_dir();
        let subs: Vec<String> = from_spec(&git, &[], "comm", &cwd, "")
            .into_iter()
            .map(|i| i.text)
            .collect();
        assert!(subs.contains(&"commit".to_owned()), "{subs:?}");
        let flags = from_spec(&git, &["commit".into()], "--am", &cwd, "");
        assert!(
            flags
                .iter()
                .any(|i| i.text == "--amend" && i.description.is_some())
        );
    }

    #[test]
    fn commands_follow_symlinks() {
        let dir = std::env::temp_dir().join(format!("thurm-cmds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/bin/ls", dir.join("thurm-fake-cmd")).unwrap();
        let names: Vec<String> = commands("thurm-fake", dir.to_str().unwrap())
            .into_iter()
            .map(|i| i.text)
            .collect();
        assert_eq!(names, vec!["thurm-fake-cmd"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generator_timeout() {
        let t = Instant::now();
        assert!(
            run_generator(
                &["sleep".into(), "5".into()],
                Path::new("/"),
                "/bin:/usr/bin"
            )
            .is_empty()
        );
        assert!(t.elapsed() < Duration::from_secs(2));
        assert_eq!(
            run_generator(
                &["printf".into(), "a\\nb\\n".into()],
                Path::new("/"),
                "/bin:/usr/bin"
            ),
            vec!["a", "b"]
        );
    }
}
