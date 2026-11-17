//! `thurm clip`: copy to and paste from the clipboard of the app showing this pane. Run under
//! the name of a clipboard tool (`pbcopy`, `pbpaste`, `xclip`, `xsel`, `wl-copy`, `wl-paste`,
//! a symlink `thurm remote doctor` installs), `thurm` acts as that tool.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use clap::Subcommand;
use thurm_proto::{CLIP_MAX, CLIP_PNG, CLIP_TARGETS, CLIP_TEXT, PaneId, Request, Response};

/// The tools `thurm` stands in for, by the name it is run as.
pub const TOOLS: &[&str] = &["pbcopy", "pbpaste", "xclip", "xsel", "wl-copy", "wl-paste"];

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

/// Parses the arguments `tool` was run with (without its name).
pub fn parse(tool: &str, args: &[String], stdin_tty: bool) -> Result<Action, String> {
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
                    Some("target") => {
                        target = Some(it.next().ok_or("xclip: -target needs a type")?.clone())
                    }
                    // Every selection is this Mac's one clipboard.
                    Some("selection" | "loops" | "display" | "wait") => {
                        it.next();
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
            let (mut input, mut output, mut clear) = (false, false, false);
            while let Some(a) = it.next() {
                let flags: Vec<char> = match a.strip_prefix("--") {
                    Some(long) => vec![match long {
                        "input" => 'i',
                        "output" => 'o',
                        "clear" => 'c',
                        "clipboard" | "primary" | "secondary" | "keep" | "exchange"
                        | "nodetach" | "verbose" | "trim" | "zeroflush" => 'b',
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
                        'b' | 'p' | 's' | 'k' | 'x' | 'n' | 'v' | 'z' | 'a' | 'f' => {}
                        'l' | 't' | 'w' => {
                            it.next();
                        }
                        'h' => return Ok(Action::Help("xsel (thurm): -b [-i | -o | -c]")),
                        _ => return Err(format!("xsel: unknown option -{f}")),
                    }
                }
            }
            if clear {
                Ok(Action::Copy {
                    text: Some(String::new()),
                    trim: false,
                    echo: false,
                })
            } else if output || (!input && stdin_tty) {
                Ok(Action::Paste {
                    mime: CLIP_TEXT.into(),
                    newline: false,
                })
            } else {
                Ok(Action::Copy {
                    text: None,
                    trim: false,
                    echo: false,
                })
            }
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
                    "-o" | "--paste-once" | "-f" | "--foreground" | "-p" | "--primary"
                    | "--regular" => {}
                    "-t" | "--type" => {
                        let t = mime(it.next().ok_or("wl-copy: --type needs a type")?)?;
                        if t != CLIP_TEXT {
                            return Err("wl-copy: only text can be copied to the clipboard".into());
                        }
                    }
                    "-s" | "--seat" => {
                        it.next();
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
            Ok(Action::Copy {
                text: if clear {
                    Some(String::new())
                } else {
                    (!words.is_empty()).then(|| words.join(" "))
                },
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
                    "-t" | "--type" => {
                        target = Some(it.next().ok_or("wl-paste: --type needs a type")?.clone())
                    }
                    "-s" | "--seat" => {
                        it.next();
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

/// `thurm` run as one of [`TOOLS`]: `None` when `argv0` names none of them.
pub fn run_as_tool(argv0: &str, args: &[String]) -> Option<ExitCode> {
    let name = std::path::Path::new(argv0).file_name()?.to_str()?;
    if !TOOLS.contains(&name) {
        return None;
    }
    let done = parse(name, args, std::io::stdin().is_terminal())
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

fn perform(action: Action) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = std::io::stdout().lock();
    match action {
        Action::Help(usage) => writeln!(out, "{usage}")?,
        Action::Copy { text, trim, echo } => {
            let pane = pane()?;
            let mut text = match text {
                Some(t) => t,
                None => read_stdin()?,
            };
            if echo {
                out.write_all(text.as_bytes())?;
            }
            if trim && text.ends_with('\n') {
                text.pop();
            }
            let c = crate::connect(false)?;
            c.request(Request::ClipboardSet { pane, text })?;
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
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(tool: &str, args: &[&str]) -> Result<Action, String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        parse(tool, &args, false)
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
        assert_eq!(
            p("xsel", &["-bc"]),
            Ok(Action::Copy {
                text: Some(String::new()),
                trim: false,
                echo: false
            })
        );
        // Neither -i nor -o: input when stdin is piped, output from a terminal.
        assert_eq!(p("xsel", &["-b"]), Ok(copy()));
        assert_eq!(
            parse("xsel", &["-b".to_string()], true),
            Ok(paste(CLIP_TEXT))
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
        assert_eq!(
            p("wl-copy", &["--clear"]),
            Ok(Action::Copy {
                text: Some(String::new()),
                trim: false,
                echo: false
            })
        );
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
        for t in TOOLS {
            assert!(parse(t, &[], false).is_ok(), "{t}");
        }
        assert!(parse("thurm", &[], false).is_err());
    }
}
