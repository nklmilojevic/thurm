//! `thurm clip`: copy to and paste from the clipboard of the app showing this pane. Run under
//! the name of a clipboard tool (`pbcopy`, `pbpaste`, `xclip`, `xsel`, `wl-copy`, `wl-paste`,
//! a symlink `thurm remote doctor` installs), `thurm` acts as that tool.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use clap::Subcommand;
use thurm_proto::{
    CLIP_MAX, CLIP_PNG, CLIP_TARGETS, CLIP_TEXT, CLIP_TOOLS, PaneId, Request, Response,
};

#[derive(Subcommand)]
pub enum ClipCmd {
    /// Copy standard input to the clipboard.
    Copy,
    /// Print the clipboard.
    Paste {
        /// text/plain, image/png, or TARGETS for the types it holds.
        #[arg(long = "type", value_name = "MIME", default_value = CLIP_TEXT)]
        mime: String,
    },
    /// Empty the clipboard.
    Clear,
}

/// What a tool's arguments ask for.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Copy {
        /// The text, when given as arguments (`wl-copy TEXT`) instead of on standard input.
        text: Option<String>,
        /// Drop one trailing newline.
        trim: bool,
        /// Echo the input to standard output too (`xclip -f`).
        echo: bool,
    },
    Paste {
        mime: String,
        /// Append a newline to text (`wl-paste` without `-n`).
        newline: bool,
    },
    /// Print the types the clipboard holds, in the tool's own terms.
    Types { xclip: bool },
    /// Print the clipboard's text, then copy standard input (`xsel` with both of its standard
    /// streams redirected and neither mode asked for), one trailing newline dropped with `trim`.
    PasteThenCopy { trim: bool },
    /// Empty the clipboard (`xsel --clear`, `wl-copy --clear`): no type left, not empty text.
    Clear,
    /// Print a usage line, do nothing else.
    Help(&'static str),
}

/// The clipboard type `t` names, in the tools' various spellings.
fn mime(t: &str) -> Result<String, String> {
    match t {
        "text" | "TEXT" | "STRING" | "UTF8_STRING" | "COMPOUND_TEXT" => Ok(CLIP_TEXT.into()),
        t if t.eq_ignore_ascii_case(CLIP_TEXT)
            || t.to_ascii_lowercase().starts_with("text/plain;") =>
        {
            Ok(CLIP_TEXT.into())
        }
        t if t.eq_ignore_ascii_case(CLIP_PNG) => Ok(CLIP_PNG.into()),
        CLIP_TARGETS => Ok(CLIP_TARGETS.into()),
        t => Err(format!(
            "unsupported clipboard type {t} (text/plain or image/png)"
        )),
    }
}

/// `xclip`'s options take any unambiguous prefix of at least their first letter.
fn xclip_opt(arg: &str) -> Option<&'static str> {
    let name = arg.strip_prefix('-')?;
    let name = name.strip_prefix('-').unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    const OPTS: &[&str] = &[
        "selection",
        "in",
        "out",
        "target",
        "filter",
        "rmlastnl",
        "loops",
        "display",
        "quiet",
        "silent",
        "verbose",
        "noutf8",
        "help",
        "version",
        "sensitive",
        "wait",
    ];
    let mut found = OPTS.iter().filter(|o| o.starts_with(name));
    let first = found.next()?;
    // `-s` alone: xclip reads it as -selection.
    Some(if name == "s" { "selection" } else { first })
}

/// The value of option `opt`: the next argument, unless there is none or it is another option
/// (`xclip -selection -o` must not take `-o` for the selection and then copy).
fn operand<'a>(
    it: &mut std::slice::Iter<'a, String>,
    tool: &str,
    opt: &str,
) -> Result<&'a String, String> {
    match it.as_slice().first() {
        Some(v) if !v.starts_with('-') => Ok(it.next().expect("peeked")),
        _ => Err(format!("{tool}: {opt} needs a value")),
    }
}

/// Parses the arguments `tool` was run with (without its name).
///
/// `stdin_tty` and `stdout_tty` say whether those are terminals: `xsel` without a mode picks
/// one by them.
pub fn parse(
    tool: &str,
    args: &[String],
    stdin_tty: bool,
    stdout_tty: bool,
) -> Result<Action, String> {
    let mut it = args.iter();
    match tool {
        "pbcopy" => Ok(Action::Copy {
            text: None,
            trim: false,
            echo: false,
        }),
        "pbpaste" => Ok(Action::Paste {
            mime: CLIP_TEXT.into(),
            newline: false,
        }),
        "xclip" => {
            let (mut out, mut target, mut echo, mut trim) = (false, None, false, false);
            while let Some(a) = it.next() {
                match xclip_opt(a) {
                    Some("in") => out = false,
                    Some("out") => out = true,
                    Some("filter") => echo = true,
                    Some("rmlastnl") => trim = true,
                    Some("target") => target = Some(operand(&mut it, "xclip", a)?.clone()),
                    // Every selection is this Mac's one clipboard.
                    Some("selection" | "loops" | "display" | "wait") => {
                        operand(&mut it, "xclip", a)?;
                    }
                    Some("quiet" | "silent" | "verbose" | "noutf8" | "sensitive") => {}
                    Some("help" | "version") => {
                        return Ok(Action::Help(
                            "xclip (thurm): -i | -o [-selection c] [-t TYPE] [-f] [-r]",
                        ));
                    }
                    _ if !a.starts_with('-') => {
                        return Err("xclip: reading files is not supported; use stdin".into());
                    }
                    _ => return Err(format!("xclip: unknown option {a}")),
                }
            }
            match (out, target.as_deref().map(mime).transpose()?) {
                (true, Some(t)) if t == CLIP_TARGETS => Ok(Action::Types { xclip: true }),
                (true, t) => Ok(Action::Paste {
                    mime: t.unwrap_or_else(|| CLIP_TEXT.into()),
                    newline: false,
                }),
                (false, Some(t)) if t != CLIP_TEXT => {
                    Err("xclip: only text can be copied to the clipboard".into())
                }
                (false, _) => Ok(Action::Copy {
                    text: None,
                    trim,
                    echo,
                }),
            }
        }
        "xsel" => {
            let (mut input, mut output, mut clear, mut trim) = (false, false, false, false);
            while let Some(a) = it.next() {
                let flags: Vec<char> = match a.strip_prefix("--") {
                    Some(long) => vec![match long {
                        "input" => 'i',
                        "output" => 'o',
                        "clear" => 'c',
                        "delete" => 'd',
                        "append" => 'a',
                        "follow" => 'f',
                        "exchange" => 'x',
                        "trim" => 'T',
                        "clipboard" | "primary" | "secondary" | "keep" | "nodetach" | "verbose"
                        | "zeroflush" => 'b',
                        "logfile" | "display" | "selectionTimeout" | "window-name" => 't',
                        "help" | "version" => 'h',
                        _ => return Err(format!("xsel: unknown option {a}")),
                    }],
                    None => match a.strip_prefix('-') {
                        Some(short) if !short.is_empty() => short.chars().collect(),
                        _ => return Err(format!("xsel: unexpected argument {a}")),
                    },
                };
                for f in flags {
                    match f {
                        'i' => input = true,
                        'o' => output = true,
                        'c' | 'd' => clear = true,
                        'T' => trim = true,
                        // Not done here: refused rather than run as a plain copy.
                        'a' => return Err("xsel: --append is not supported".into()),
                        'f' => return Err("xsel: --follow is not supported".into()),
                        'x' => return Err("xsel: --exchange is not supported".into()),
                        'b' | 'p' | 's' | 'k' | 'n' | 'v' | 'z' => {}
                        'l' | 't' | 'w' => {
                            operand(&mut it, "xsel", a)?;
                        }
                        'h' => return Ok(Action::Help("xsel (thurm): -b [-i | -o | -c]")),
                        _ => return Err(format!("xsel: unknown option -{f}")),
                    }
                }
            }
            if clear {
                return Ok(Action::Clear);
            }
            // As xsel does: the modes asked for; with none, output when standard output is not
            // a terminal and input when standard input is not (both when neither is), and output
            // from a terminal.
            let (output, input) = match (output, input) {
                (false, false) if stdin_tty && stdout_tty => (true, false),
                (false, false) => (!stdout_tty, !stdin_tty),
                asked => asked,
            };
            Ok(match (output, input) {
                (true, true) => Action::PasteThenCopy { trim },
                (true, false) => Action::Paste {
                    mime: CLIP_TEXT.into(),
                    newline: false,
                },
                _ => Action::Copy {
                    text: None,
                    trim,
                    echo: false,
                },
            })
        }
        "wl-copy" => {
            let (mut trim, mut clear, mut words) = (false, false, Vec::new());
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--" => {
                        words.extend(it.by_ref().cloned());
                        break;
                    }
                    "-n" | "--trim-newline" => trim = true,
                    "-c" | "--clear" => clear = true,
                    // The text would stay after its one paste: refused rather than ignored.
                    "-o" | "--paste-once" => {
                        return Err("wl-copy: --paste-once is not supported".into());
                    }
                    "-f" | "--foreground" | "-p" | "--primary" | "--regular" => {}
                    "-t" | "--type" => {
                        let t = mime(operand(&mut it, "wl-copy", a)?)?;
                        if t != CLIP_TEXT {
                            return Err("wl-copy: only text can be copied to the clipboard".into());
                        }
                    }
                    "-s" | "--seat" => {
                        operand(&mut it, "wl-copy", a)?;
                    }
                    "-h" | "--help" | "-v" | "--version" => {
                        return Ok(Action::Help("wl-copy (thurm): [-n] [TEXT...] or stdin"));
                    }
                    a if a.starts_with("--type=") => {
                        if mime(&a["--type=".len()..])? != CLIP_TEXT {
                            return Err("wl-copy: only text can be copied to the clipboard".into());
                        }
                    }
                    a if a.starts_with('-') && a.len() > 1 => {
                        return Err(format!("wl-copy: unknown option {a}"));
                    }
                    a => {
                        words.push(a.to_owned());
                        words.extend(it.by_ref().cloned());
                        break;
                    }
                }
            }
            if clear {
                return Ok(Action::Clear);
            }
            Ok(Action::Copy {
                text: (!words.is_empty()).then(|| words.join(" ")),
                trim,
                echo: false,
            })
        }
        "wl-paste" => {
            let (mut newline, mut list, mut target) = (true, false, None);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-n" | "--no-newline" => newline = false,
                    "-l" | "--list-types" => list = true,
                    "-p" | "--primary" => {}
                    "-t" | "--type" => target = Some(operand(&mut it, "wl-paste", a)?.clone()),
                    "-s" | "--seat" => {
                        operand(&mut it, "wl-paste", a)?;
                    }
                    "-w" | "--watch" => return Err("wl-paste: --watch is not supported".into()),
                    "-h" | "--help" | "-v" | "--version" => {
                        return Ok(Action::Help(
                            "wl-paste (thurm): [-n] [-l] [-t text/plain|image/png]",
                        ));
                    }
                    a if a.starts_with("--type=") => target = Some(a["--type=".len()..].into()),
                    a => return Err(format!("wl-paste: unexpected argument {a}")),
                }
            }
            if list {
                return Ok(Action::Types { xclip: false });
            }
            let mime = target.as_deref().map(mime).transpose()?;
            let mime = mime.unwrap_or_else(|| CLIP_TEXT.into());
            if mime == CLIP_TARGETS {
                return Ok(Action::Types { xclip: false });
            }
            Ok(Action::Paste {
                newline: newline && mime == CLIP_TEXT,
                mime,
            })
        }
        _ => Err(format!(
            "{tool} is not a clipboard tool thurm stands in for"
        )),
    }
}

/// The types the clipboard holds (the app's answer to [`CLIP_TARGETS`]), as the tool lists
/// them.
pub fn list_types(answer: &str, xclip: bool) -> String {
    let mut out = Vec::new();
    if xclip {
        out.push("TARGETS");
    }
    for t in answer.lines() {
        match t {
            CLIP_TEXT => {
                if xclip {
                    out.push("UTF8_STRING");
                }
                out.extend(["text/plain;charset=utf-8", CLIP_TEXT]);
            }
            CLIP_PNG => out.push(CLIP_PNG),
            _ => {}
        }
    }
    out.iter().map(|t| format!("{t}\n")).collect()
}

/// `thurm` run as one of [`CLIP_TOOLS`]: `None` when `argv0` names none of them.
pub fn run_as_tool(argv0: &str, args: &[String]) -> Option<ExitCode> {
    let name = std::path::Path::new(argv0).file_name()?.to_str()?;
    if !CLIP_TOOLS.contains(&name) {
        return None;
    }
    let done = parse(
        name,
        args,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    )
    .and_then(|action| perform(action).map_err(|e| e.to_string()));
    Some(match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{name}: {e}");
            ExitCode::FAILURE
        }
    })
}

/// `thurm clip ...`.
pub fn run(cmd: ClipCmd, remote: bool) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if remote {
        return Err("clip works on this pane's own clipboard; drop --remote".into());
    }
    perform(match cmd {
        ClipCmd::Copy => Action::Copy {
            text: None,
            trim: false,
            echo: false,
        },
        ClipCmd::Paste { mime: m } => match mime(&m)? {
            t if t == CLIP_TARGETS => Action::Types { xclip: false },
            t => Action::Paste {
                mime: t,
                newline: false,
            },
        },
        ClipCmd::Clear => Action::Clear,
    })?;
    Ok(ExitCode::SUCCESS)
}

fn pane() -> Result<PaneId, String> {
    std::env::var(thurm_proto::ENV_PANE_ID)
        .ok()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| {
            format!(
                "not running inside Thurm (no ${})",
                thurm_proto::ENV_PANE_ID
            )
        })
}

fn read_stdin() -> Result<String, Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    std::io::stdin()
        .take(CLIP_MAX as u64 + 1)
        .read_to_end(&mut buf)?;
    if buf.len() > CLIP_MAX {
        return Err("the text is larger than 32 MiB".into());
    }
    Ok(String::from_utf8(buf).map_err(|_| "only UTF-8 text can be copied")?)
}

fn get(pane: PaneId, mime: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let c = crate::connect(false)?;
    match c.request(Request::ClipboardGet {
        pane,
        mime: mime.into(),
    })? {
        Response::ClipboardData { data } => Ok(data),
        other => Err(format!("unexpected response {other:?}").into()),
    }
}

/// Copies `text` (one trailing newline dropped with `trim`) through `set`, then, with `echo`
/// (`xclip -f`), writes what was copied to `out`: nothing when the copy failed.
fn copy_then_echo(
    mut text: String,
    trim: bool,
    echo: bool,
    set: impl FnOnce(String) -> Result<(), Box<dyn std::error::Error>>,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    if trim && text.ends_with('\n') {
        text.pop();
    }
    let copied = echo.then(|| text.clone());
    set(text)?;
    if let Some(t) = copied {
        out.write_all(t.as_bytes())?;
    }
    Ok(())
}

fn perform(action: Action) -> Result<(), Box<dyn std::error::Error>> {
    if let Action::PasteThenCopy { trim } = action {
        // The copy goes ahead though the paste failed (a refused read), as xsel's does.
        let pasted = perform(Action::Paste {
            mime: CLIP_TEXT.into(),
            newline: false,
        });
        let copied = perform(Action::Copy {
            text: None,
            trim,
            echo: false,
        });
        return pasted.and(copied);
    }
    let mut out = std::io::stdout().lock();
    match action {
        Action::Help(usage) => writeln!(out, "{usage}")?,
        Action::Copy { text, trim, echo } => {
            let pane = pane()?;
            let text = match text {
                Some(t) => t,
                None => read_stdin()?,
            };
            copy_then_echo(
                text,
                trim,
                echo,
                |text| {
                    crate::connect(false)?.request(Request::ClipboardSet { pane, text })?;
                    Ok(())
                },
                &mut out,
            )?;
        }
        Action::Paste { mime, newline } => {
            let mut data = get(pane()?, &mime)?;
            if newline && !data.ends_with(b"\n") {
                data.push(b'\n');
            }
            out.write_all(&data)?;
        }
        Action::Types { xclip } => {
            let data = get(pane()?, CLIP_TARGETS)?;
            out.write_all(list_types(&String::from_utf8_lossy(&data), xclip).as_bytes())?;
        }
        Action::PasteThenCopy { .. } => unreachable!("handled above"),
        Action::Clear => {
            crate::connect(false)?.request(Request::ClipboardClear { pane: pane()? })?;
        }
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(tool: &str, args: &[&str]) -> Result<Action, String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        // Piped input, output to a terminal.
        parse(tool, &args, false, true)
    }

    fn copy() -> Action {
        Action::Copy {
            text: None,
            trim: false,
            echo: false,
        }
    }

    fn paste(mime: &str) -> Action {
        Action::Paste {
            mime: mime.into(),
            newline: false,
        }
    }

    #[test]
    fn pbcopy_and_pbpaste() {
        assert_eq!(p("pbcopy", &[]), Ok(copy()));
        assert_eq!(p("pbpaste", &[]), Ok(paste(CLIP_TEXT)));
        assert_eq!(p("pbpaste", &["-Prefer", "txt"]), Ok(paste(CLIP_TEXT)));
    }

    #[test]
    fn xclip() {
        assert_eq!(p("xclip", &[]), Ok(copy()));
        assert_eq!(p("xclip", &["-selection", "clipboard"]), Ok(copy()));
        assert_eq!(p("xclip", &["-sel", "c", "-i"]), Ok(copy()));
        assert_eq!(
            p("xclip", &["-quiet", "-i", "-selection", "clipboard"]),
            Ok(copy())
        );
        assert_eq!(
            p("xclip", &["-selection", "clipboard", "-o"]),
            Ok(paste(CLIP_TEXT))
        );
        assert_eq!(p("xclip", &["-out"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(
            p(
                "xclip",
                &["-selection", "clipboard", "-t", "image/png", "-o"]
            ),
            Ok(paste(CLIP_PNG))
        );
        assert_eq!(
            p("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]),
            Ok(Action::Types { xclip: true })
        );
        assert_eq!(
            p("xclip", &["-o", "-target", "UTF8_STRING"]),
            Ok(paste(CLIP_TEXT))
        );
        assert_eq!(
            p("xclip", &["-f", "-r"]),
            Ok(Action::Copy {
                text: None,
                trim: true,
                echo: true
            })
        );
        assert!(p("xclip", &["-t", "image/png", "-i"]).is_err());
        assert!(p("xclip", &["-t"]).is_err());
        assert!(p("xclip", &["file.txt"]).is_err());
        assert!(p("xclip", &["-bogus"]).is_err());
    }

    #[test]
    fn options_without_their_value_are_refused() {
        for (tool, args) in [
            ("xclip", &["-selection"][..]),
            ("xclip", &["-selection", "-o"]),
            ("xclip", &["-display"]),
            ("xclip", &["-loops", "-i"]),
            ("xclip", &["-o", "-t"]),
            ("xclip", &["-t", "-o"]),
            ("xsel", &["-l"]),
            ("xsel", &["--logfile", "-o"]),
            ("xsel", &["-bt", "-o"]),
            ("wl-copy", &["--type"]),
            ("wl-copy", &["-t", "-n"]),
            ("wl-copy", &["--seat", "-n"]),
            ("wl-paste", &["--type", "-n"]),
            ("wl-paste", &["-s"]),
        ] {
            let e = p(tool, args).expect_err(&format!("{tool} {args:?}"));
            assert!(e.contains("needs a value"), "{tool} {args:?}: {e}");
        }
        assert_eq!(p("xclip", &["-selection", "c", "-o"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(p("xsel", &["-l", "/tmp/log", "-o"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(p("wl-paste", &["-s", "seat0"]).map(|_| ()), Ok(()));
    }

    #[test]
    fn the_filter_echoes_only_what_was_copied() {
        let mut out = Vec::new();
        let failed = super::copy_then_echo(
            "hi\n".into(),
            true,
            true,
            |_| Err("refused".into()),
            &mut out,
        );
        assert!(failed.is_err());
        assert!(out.is_empty(), "a failed copy echoes nothing");

        let mut copied = None;
        super::copy_then_echo(
            "hi\n".into(),
            true,
            true,
            |t| {
                copied = Some(t);
                Ok(())
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(copied.as_deref(), Some("hi"));
        assert_eq!(out, b"hi", "the text that was copied, trimmed");

        let mut out = Vec::new();
        super::copy_then_echo("x".into(), false, false, |_| Ok(()), &mut out).unwrap();
        assert!(out.is_empty(), "no echo without -f");
    }

    #[test]
    fn xsel() {
        assert_eq!(p("xsel", &["-b", "-i"]), Ok(copy()));
        assert_eq!(p("xsel", &["-bi"]), Ok(copy()));
        assert_eq!(p("xsel", &["--clipboard", "--input"]), Ok(copy()));
        assert_eq!(p("xsel", &["-b", "-o"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(p("xsel", &["-ob"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(
            p("xsel", &["--clipboard", "--output"]),
            Ok(paste(CLIP_TEXT))
        );
        assert_eq!(p("xsel", &["-bc"]), Ok(Action::Clear));
        assert_eq!(p("xsel", &["--clipboard", "--delete"]), Ok(Action::Clear));
        // --trim drops the input's trailing newline, in either mode that copies.
        let trimmed = Action::Copy {
            text: None,
            trim: true,
            echo: false,
        };
        assert_eq!(
            p("xsel", &["--clipboard", "--input", "--trim"]),
            Ok(trimmed)
        );
        assert_eq!(
            parse("xsel", &["-b".into(), "--trim".into()], false, false),
            Ok(Action::PasteThenCopy { trim: true })
        );
        // What the shim cannot do is refused, not run as a plain copy.
        for opt in [
            "-a",
            "--append",
            "-f",
            "--follow",
            "-x",
            "--exchange",
            "-bia",
        ] {
            let e = p("xsel", &[opt]).expect_err(opt);
            assert!(e.contains("not supported"), "{opt}: {e}");
        }
        // Neither -i nor -o: input when stdin is piped, output from a terminal.
        assert_eq!(p("xsel", &["-b"]), Ok(copy()));
        assert_eq!(
            parse("xsel", &["-b".to_string()], true, true),
            Ok(paste(CLIP_TEXT))
        );
        // Without a mode, by which standard streams are terminals (stdin, stdout).
        let b = ["-b".to_string()];
        assert_eq!(
            parse("xsel", &b, true, false),
            Ok(paste(CLIP_TEXT)),
            "> file"
        );
        assert_eq!(parse("xsel", &b, false, true), Ok(copy()), "piped in");
        assert_eq!(
            parse("xsel", &b, false, false),
            Ok(Action::PasteThenCopy { trim: false }),
            "both redirected: the old selection out, then the new one in"
        );
        // A mode asked for is the only one, whatever the streams.
        assert_eq!(parse("xsel", &["-bi".into()], false, false), Ok(copy()));
        assert_eq!(
            parse("xsel", &["-bo".into()], false, false),
            Ok(paste(CLIP_TEXT))
        );
        assert_eq!(
            parse("xsel", &["-b".into(), "-i".into(), "-o".into()], true, true),
            Ok(Action::PasteThenCopy { trim: false })
        );
        assert!(p("xsel", &["-q"]).is_err());
    }

    #[test]
    fn wl_copy() {
        assert_eq!(p("wl-copy", &[]), Ok(copy()));
        assert_eq!(
            p("wl-copy", &["-n", "hello", "world"]),
            Ok(Action::Copy {
                text: Some("hello world".into()),
                trim: true,
                echo: false
            })
        );
        assert_eq!(
            p("wl-copy", &["--", "-n"]),
            Ok(Action::Copy {
                text: Some("-n".into()),
                trim: false,
                echo: false
            })
        );
        assert_eq!(p("wl-copy", &["--type", "text/plain"]), Ok(copy()));
        assert_eq!(
            p("wl-copy", &["--type=text/plain;charset=utf-8"]),
            Ok(copy())
        );
        assert_eq!(p("wl-copy", &["--clear"]), Ok(Action::Clear));
        assert_eq!(p("wl-copy", &["-c"]), Ok(Action::Clear));
        for opt in ["-o", "--paste-once"] {
            let e = p("wl-copy", &[opt]).expect_err(opt);
            assert!(e.contains("not supported"), "{opt}: {e}");
        }
        assert!(p("wl-copy", &["--type", "image/png"]).is_err());
        assert!(p("wl-copy", &["--bogus"]).is_err());
    }

    #[test]
    fn wl_paste() {
        assert_eq!(
            p("wl-paste", &[]),
            Ok(Action::Paste {
                mime: CLIP_TEXT.into(),
                newline: true
            })
        );
        assert_eq!(p("wl-paste", &["-n"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(p("wl-paste", &["--type", "image/png"]), Ok(paste(CLIP_PNG)));
        assert_eq!(p("wl-paste", &["-t", "text"]).map(|_| ()), Ok(()));
        assert_eq!(
            p("wl-paste", &["--list-types"]),
            Ok(Action::Types { xclip: false })
        );
        assert_eq!(p("wl-paste", &["-l"]), Ok(Action::Types { xclip: false }));
        assert!(p("wl-paste", &["--watch", "cat"]).is_err());
        assert!(p("wl-paste", &["--type", "image/jpeg"]).is_err());
    }

    #[test]
    fn types_in_each_tools_terms() {
        let answer = format!("{CLIP_TEXT}\n{CLIP_PNG}\n");
        assert_eq!(
            list_types(&answer, true),
            "TARGETS\nUTF8_STRING\ntext/plain;charset=utf-8\ntext/plain\nimage/png\n"
        );
        assert_eq!(
            list_types(&answer, false),
            "text/plain;charset=utf-8\ntext/plain\nimage/png\n"
        );
        assert_eq!(list_types(CLIP_PNG, false), "image/png\n");
        assert_eq!(list_types("", false), "");
    }

    #[test]
    fn only_tool_names_switch_modes() {
        for t in CLIP_TOOLS {
            assert!(parse(t, &[], false, true).is_ok(), "{t}");
        }
        assert!(parse("thurm", &[], false, true).is_err());
    }
}
