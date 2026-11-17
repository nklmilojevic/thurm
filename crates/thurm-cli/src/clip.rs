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

/// Whether `t` is `text/plain` in UTF-8, the text the app gives: no parameters, or a UTF-8
/// `charset` only (`text/plain;charset=utf-8`, any case, quoted or not).
fn utf8_text(t: &str) -> bool {
    let mut parts = t.split(';').map(str::trim);
    if !parts
        .next()
        .is_some_and(|m| m.eq_ignore_ascii_case(CLIP_TEXT))
    {
        return false;
    }
    parts.all(|p| match p.split_once('=') {
        Some((k, v)) if k.trim().eq_ignore_ascii_case("charset") => {
            let v = v.trim().trim_matches('"');
            v.eq_ignore_ascii_case("utf-8") || v.eq_ignore_ascii_case("utf8")
        }
        _ => false,
    })
}

/// The clipboard type `t` names, in the tools' various spellings. Text is UTF-8 only: X11's
/// STRING (Latin-1) and COMPOUND_TEXT, or another charset, would get bytes they cannot read.
fn mime(t: &str) -> Result<String, String> {
    match t {
        "text" | "TEXT" | "UTF8_STRING" => Ok(CLIP_TEXT.into()),
        t if utf8_text(t) => Ok(CLIP_TEXT.into()),
        t if t.eq_ignore_ascii_case(CLIP_PNG) => Ok(CLIP_PNG.into()),
        CLIP_TARGETS => Ok(CLIP_TARGETS.into()),
        t => Err(format!(
            "unsupported clipboard type {t} (text/plain in UTF-8, or image/png)"
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
        "pbcopy" | "pbpaste" => {
            let copy = tool == "pbcopy";
            while let Some(a) = it.next() {
                match a.as_str() {
                    // Only the general pasteboard is this Mac's clipboard: copying to the find
                    // or font one must not overwrite it.
                    "-pboard" => match operand(&mut it, tool, a)?.as_str() {
                        "general" => {}
                        other => {
                            return Err(format!(
                                "{tool}: only the general pasteboard is supported, not {other}"
                            ));
                        }
                    },
                    // Text only: a format asked for is not swapped for plain text.
                    "-Prefer" if !copy => match operand(&mut it, tool, a)?.as_str() {
                        "txt" => {}
                        other => {
                            return Err(format!(
                                "{tool}: only text (-Prefer txt) is supported, not {other}"
                            ));
                        }
                    },
                    "-help" => {
                        return Ok(Action::Help(if copy {
                            "pbcopy (thurm): [-pboard general] < text"
                        } else {
                            "pbpaste (thurm): [-pboard general] [-Prefer txt]"
                        }));
                    }
                    _ => return Err(format!("{tool}: unknown option {a}")),
                }
            }
            Ok(if copy {
                Action::Copy {
                    text: None,
                    trim: false,
                    echo: false,
                }
            } else {
                Action::Paste {
                    mime: CLIP_TEXT.into(),
                    newline: false,
                }
            })
        }
        "xclip" => {
            let (mut out, mut target, mut echo, mut trim) = (false, None, false, false);
            // xclip's own default is PRIMARY; the last -selection given is the one it uses.
            let mut selection: Option<String> = None;
            // A copy that should go away (after some pastes, a time or one paste): not done here.
            let mut expires = None;
            while let Some(a) = it.next() {
                match xclip_opt(a) {
                    Some("in") => out = false,
                    Some("out") => out = true,
                    Some("filter") => echo = true,
                    Some("rmlastnl") => trim = true,
                    Some("target") => target = Some(operand(&mut it, "xclip", a)?.clone()),
                    // Only the clipboard: PRIMARY or SECONDARY is not this Mac's clipboard, and
                    // must not be written over it or read from it.
                    Some("selection") => selection = Some(operand(&mut it, "xclip", a)?.clone()),
                    // Another X server's clipboard: not this Mac's, refused rather than served.
                    Some("display") => {
                        let display = operand(&mut it, "xclip", a)?;
                        return Err(format!("xclip: -display {display} is not supported"));
                    }
                    Some(opt @ ("loops" | "wait")) => {
                        if operand(&mut it, "xclip", a)? != "0" {
                            expires = Some(opt);
                        }
                    }
                    Some("sensitive") => expires = Some("sensitive"),
                    Some("quiet" | "silent" | "verbose") => {}
                    // xclip's STRING (Latin-1) target: the app has UTF-8 text only.
                    Some("noutf8") => {
                        return Err("xclip: -noutf8 is not supported: text is UTF-8".into());
                    }
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
            match selection.as_deref() {
                Some(sel) if !sel.is_empty() && "clipboard".starts_with(sel) => {}
                Some(sel) => {
                    return Err(format!(
                        "xclip: only -selection clipboard is supported, not {sel}"
                    ));
                }
                None => {
                    return Err(
                        "xclip: only -selection clipboard is supported (the default is PRIMARY)"
                            .into(),
                    );
                }
            }
            if let (false, Some(opt)) = (out, expires) {
                return Err(format!(
                    "xclip: -{opt} is not supported: the copy would stay"
                ));
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
            let mut timeout = false;
            let mut selection = 'p';
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
                        "clipboard" => 'b',
                        "primary" => 'p',
                        "secondary" => 's',
                        "keep" | "nodetach" | "verbose" | "zeroflush" => 'n',
                        "logfile" | "window-name" => 'l',
                        // Another X server's selections: not this Mac's clipboard.
                        "display" => return Err("xsel: --display is not supported".into()),
                        "selectionTimeout" => 't',
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
                        // The selection: xsel's own default is PRIMARY, the last one named wins.
                        'b' | 'p' | 's' => selection = f,
                        'k' | 'n' | 'v' | 'z' => {}
                        'l' | 'w' => {
                            operand(&mut it, "xsel", a)?;
                        }
                        // The selection would go after this many milliseconds: not done here.
                        't' => {
                            if operand(&mut it, "xsel", a)? != "0" {
                                timeout = true;
                            }
                        }
                        'h' => return Ok(Action::Help("xsel (thurm): -b [-i | -o | -c]")),
                        _ => return Err(format!("xsel: unknown option -{f}")),
                    }
                }
            }
            if selection != 'b' {
                let name = if selection == 's' {
                    "SECONDARY"
                } else {
                    "PRIMARY"
                };
                return Err(format!(
                    "xsel: only --clipboard (-b) is supported, not {name} (xsel's default)"
                ));
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
            if input && timeout {
                return Err(
                    "xsel: --selectionTimeout is not supported: the copy would stay".into(),
                );
            }
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
                    "-f" | "--foreground" | "--regular" => {}
                    // PRIMARY is not this Mac's clipboard: refused, not copied over it.
                    "-p" | "--primary" => {
                        return Err("wl-copy: --primary is not supported".into());
                    }
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
                    "-p" | "--primary" => {
                        return Err("wl-paste: --primary is not supported".into());
                    }
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

/// The tool's arguments as text, refused when one is not UTF-8, as text on standard input is:
/// `wl-copy` would copy a replacement character in place of its bytes.
fn utf8_args(args: Vec<std::ffi::OsString>) -> Result<Vec<String>, String> {
    args.into_iter()
        .map(|a| {
            a.into_string()
                .map_err(|_| "only UTF-8 arguments are supported".to_string())
        })
        .collect()
}

/// `thurm` run as one of [`CLIP_TOOLS`]: `None` when `argv0` names none of them.
pub fn run_as_tool(argv0: &std::ffi::OsStr, args: Vec<std::ffi::OsString>) -> Option<ExitCode> {
    let name = std::path::Path::new(argv0).file_name()?.to_str()?;
    if !CLIP_TOOLS.contains(&name) {
        return None;
    }
    let done = utf8_args(args)
        .and_then(|args| {
            parse(
                name,
                &args,
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
            )
        })
        .and_then(|action| perform(action, empty_is_nothing(name)).map_err(|e| e.to_string()));
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
    perform(
        match cmd {
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
        },
        false,
    )?;
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
/// (`xclip -f`), writes the input as it came, before any trim, to `out`: nothing when the copy
/// failed.
fn copy_then_echo(
    mut text: String,
    trim: bool,
    echo: bool,
    set: impl FnOnce(String) -> Result<(), Box<dyn std::error::Error>>,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let copied = echo.then(|| text.clone());
    if trim && text.ends_with('\n') {
        text.pop();
    }
    set(text)?;
    if let Some(t) = copied {
        out.write_all(t.as_bytes())?;
    }
    Ok(())
}

/// Whether `tool` fails on empty text, as xclip and wl-paste do ("nothing is copied"), where
/// pbpaste and xsel print nothing and succeed.
fn empty_is_nothing(tool: &str) -> bool {
    matches!(tool, "xclip" | "wl-paste")
}

/// What a text paste prints: `data`, or an error when it is empty and `tool` says so for that.
fn pasted_text(data: Vec<u8>, empty_fails: bool) -> Result<Vec<u8>, String> {
    if data.is_empty() && empty_fails {
        Err("nothing to paste: the clipboard has no text".into())
    } else {
        Ok(data)
    }
}

fn perform(action: Action, empty_fails: bool) -> Result<(), Box<dyn std::error::Error>> {
    if let Action::PasteThenCopy { trim } = action {
        // The copy goes ahead though the paste failed (a refused read), as xsel's does.
        let pasted = perform(
            Action::Paste {
                mime: CLIP_TEXT.into(),
                newline: false,
            },
            empty_fails,
        );
        let copied = perform(
            Action::Copy {
                text: None,
                trim,
                echo: false,
            },
            empty_fails,
        );
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
            if mime == CLIP_TEXT {
                data = pasted_text(data, empty_fails)?;
            }
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
        assert_eq!(p("pbcopy", &["-pboard", "general"]), Ok(copy()));
        assert_eq!(p("pbpaste", &[]), Ok(paste(CLIP_TEXT)));
        assert_eq!(p("pbpaste", &["-Prefer", "txt"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(
            p("pbpaste", &["-pboard", "general", "-Prefer", "txt"]),
            Ok(paste(CLIP_TEXT))
        );
        // Another pasteboard or format, or an option they do not have, is refused: pbcopy
        // -pboard find must not overwrite the clipboard, pbpaste -Prefer rtf not get text.
        for (tool, args) in [
            ("pbcopy", &["-pboard", "find"][..]),
            ("pbcopy", &["-pboard", "font"]),
            ("pbcopy", &["-Prefer", "txt"]),
            ("pbpaste", &["-pboard", "ruler"]),
            ("pbpaste", &["-Prefer", "rtf"]),
            ("pbpaste", &["-Prefer", "ps"]),
            ("pbpaste", &["-x"]),
            ("pbcopy", &["file.txt"]),
            ("pbcopy", &["-pboard"]),
        ] {
            assert!(p(tool, args).is_err(), "{tool} {args:?}");
        }
    }

    #[test]
    fn copies_that_would_expire_are_refused() {
        for args in [
            &["-loops", "1"][..],
            &["-i", "-wait", "500"],
            &["-sensitive"],
            &["-loops", "2", "-i"],
        ] {
            let e = xc(args).expect_err(&format!("{args:?}"));
            assert!(e.contains("not supported"), "{args:?}: {e}");
        }
        assert_eq!(xc(&["-loops", "0"]), Ok(copy()), "0: no limit");
        assert_eq!(
            xc(&["-o", "-loops", "1"]),
            Ok(paste(CLIP_TEXT)),
            "pastes have none"
        );
        let e = p("xsel", &["-bi", "-t", "1000"]).unwrap_err();
        assert!(e.contains("not supported"), "{e}");
        assert!(p("xsel", &["-bi", "--selectionTimeout", "5"]).is_err());
        assert_eq!(p("xsel", &["-bi", "-t", "0"]), Ok(copy()));
        assert_eq!(p("xsel", &["-bo", "-t", "1000"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(p("xsel", &["-bi", "--logfile", "/tmp/x"]), Ok(copy()));
    }

    /// xclip with `-selection clipboard` before `args`.
    fn xc(args: &[&str]) -> Result<Action, String> {
        let mut all = vec!["-selection", "clipboard"];
        all.extend(args);
        p("xclip", &all)
    }

    #[test]
    fn only_the_clipboard_selection() {
        // PRIMARY and SECONDARY are not this Mac's clipboard: refused, to write and to read,
        // and so is xclip's and xsel's default, PRIMARY.
        for args in [
            &[][..],
            &["-i"],
            &["-o"],
            &["-selection", "primary", "-i"],
            &["-selection", "primary", "-o"],
            &["-sel", "p", "-o"],
            &["-selection", "secondary", "-i"],
            &["-selection", "s", "-o"],
            &["-selection", "bogus"],
            &["-selection", "clipboard", "-selection", "primary", "-o"],
        ] {
            assert!(p("xclip", args).is_err(), "xclip {args:?}");
        }
        // The last -selection is the one xclip uses.
        assert_eq!(
            p(
                "xclip",
                &["-selection", "primary", "-selection", "clipboard", "-o"]
            ),
            Ok(paste(CLIP_TEXT))
        );
        for sel in ["c", "cl", "clip", "clipboard"] {
            assert_eq!(
                p("xclip", &["-selection", sel, "-o"]),
                Ok(paste(CLIP_TEXT)),
                "{sel}"
            );
        }
        for args in [
            &["-i"][..],
            &["-o"],
            &["-p", "-i"],
            &["--primary", "--output"],
            &["-s", "-o"],
            &["--secondary", "--input"],
            &["-bp", "-i"],
        ] {
            let e = p("xsel", args).expect_err(&format!("xsel {args:?}"));
            assert!(e.contains("--clipboard"), "xsel {args:?}: {e}");
        }
        assert_eq!(
            p("xsel", &["-pb", "-o"]),
            Ok(paste(CLIP_TEXT)),
            "the last one named"
        );
        for (tool, opt) in [
            ("wl-copy", "-p"),
            ("wl-copy", "--primary"),
            ("wl-paste", "-p"),
            ("wl-paste", "--primary"),
        ] {
            let e = p(tool, &[opt]).expect_err(opt);
            assert!(e.contains("not supported"), "{tool} {opt}: {e}");
        }
    }

    #[test]
    fn text_is_utf8_only() {
        for t in [
            "text/plain",
            "TEXT/PLAIN",
            "text/plain;charset=utf-8",
            "text/plain; charset=UTF-8",
            "text/plain;charset=\"utf8\"",
            "UTF8_STRING",
            "TEXT",
            "text",
        ] {
            assert_eq!(mime(t).as_deref(), Ok(CLIP_TEXT), "{t}");
        }
        // Another encoding would get UTF-8 bytes it reads wrong.
        for t in [
            "text/plain;charset=iso-8859-1",
            "text/plain;charset=utf-16",
            "text/plain;format=flowed",
            "text/plainx",
            "STRING",
            "COMPOUND_TEXT",
        ] {
            assert!(mime(t).is_err(), "{t}");
        }
        assert!(xc(&["-o", "-t", "text/plain;charset=iso-8859-1"]).is_err());
        for args in [&["-noutf8"][..], &["-o", "-noutf8"], &["-i", "-noutf8"]] {
            let e = xc(args).expect_err(&format!("{args:?}"));
            assert!(e.contains("-noutf8"), "{args:?}: {e}");
        }
        assert!(p("wl-paste", &["-t", "STRING"]).is_err());
        assert!(p("wl-copy", &["--type", "text/plain;charset=latin1"]).is_err());
    }

    #[test]
    fn xclip() {
        assert!(p("xclip", &[]).is_err(), "PRIMARY by default");
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
        assert_eq!(xc(&["-out"]), Ok(paste(CLIP_TEXT)));
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
        assert_eq!(xc(&["-o", "-target", "UTF8_STRING"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(
            xc(&["-f", "-r"]),
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
        assert_eq!(p("xsel", &["-l", "/tmp/log", "-bo"]), Ok(paste(CLIP_TEXT)));
        assert_eq!(p("wl-paste", &["-s", "seat0"]).map(|_| ()), Ok(()));
    }

    #[test]
    fn the_filter_echoes_the_input_once_it_is_copied() {
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
        assert_eq!(
            out, b"hi\n",
            "the input as it came, as xclip -f -r echoes it"
        );

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

    /// An empty clipboard: pbpaste and xsel print nothing and succeed, xclip and wl-paste say
    /// there is nothing, as each does.
    #[test]
    fn empty_text_per_tool() {
        for tool in ["pbpaste", "xsel"] {
            assert_eq!(
                pasted_text(Vec::new(), empty_is_nothing(tool)),
                Ok(Vec::new()),
                "{tool}"
            );
        }
        for tool in ["xclip", "wl-paste"] {
            assert!(
                pasted_text(Vec::new(), empty_is_nothing(tool)).is_err(),
                "{tool}"
            );
        }
        assert_eq!(pasted_text(b"x".to_vec(), true), Ok(b"x".to_vec()));
    }

    /// A tool told to use another X server is refused, not given this Mac's clipboard.
    #[test]
    fn another_display_is_refused() {
        for args in [&["-display", ":1", "-o"][..], &["-d", ":1", "-i"]] {
            let e = xc(args).expect_err(&format!("{args:?}"));
            assert!(e.contains("not supported"), "{args:?}: {e}");
        }
        let e = p("xsel", &["-b", "--display", ":1", "-o"]).unwrap_err();
        assert!(e.contains("not supported"), "{e}");
    }

    #[test]
    fn arguments_must_be_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let bad = std::ffi::OsString::from_vec(vec![0xff]);
        let e = utf8_args(vec!["--".into(), bad]).unwrap_err();
        assert!(e.contains("UTF-8"), "{e}");
        assert_eq!(
            utf8_args(vec!["-n".into(), "héllo".into()]),
            Ok(vec!["-n".to_string(), "héllo".to_string()])
        );
    }

    #[test]
    fn wl_copy() {
        assert_eq!(p("wl-copy", &[]), Ok(copy()));
        // An explicit empty argument copies empty text; a bare `--` reads standard input.
        let text = |t: &str| Action::Copy {
            text: Some(t.into()),
            trim: false,
            echo: false,
        };
        assert_eq!(p("wl-copy", &[""]), Ok(text("")));
        assert_eq!(p("wl-copy", &["--", ""]), Ok(text("")));
        assert_eq!(p("wl-copy", &["--"]), Ok(copy()));
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
            let args: Vec<String> = match *t {
                "xclip" => vec!["-selection".into(), "clipboard".into()],
                "xsel" => vec!["-b".into()],
                _ => vec![],
            };
            assert!(parse(t, &args, false, true).is_ok(), "{t}");
        }
        assert!(parse("thurm", &[], false, true).is_err());
    }
}
