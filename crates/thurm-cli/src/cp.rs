//! `thurm cp`: files between this machine and `[[remote]]` hosts, through the tunnels the app
//! keeps open to them (as `--remote` goes).
//!
//! A host's file is named `NAME:PATH`, as scp names it: `~` and relative paths are under the
//! home of the host's user. Only files are copied, one stream each; a file is whole at its
//! destination or not there at all.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use thurm_client::Client;
use thurm_remote::transfer::{self, Cancel, Host, Progress, TransferError};

type R = Result<ExitCode, Box<dyn std::error::Error>>;

/// One end of a copy.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Place {
    Local(PathBuf),
    /// `path` on the `[[remote]]` host `host`.
    Remote {
        host: String,
        path: String,
    },
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Place::Local(p) => write!(f, "{}", p.display()),
            Place::Remote { host, path } => write!(f, "{host}:{path}"),
        }
    }
}

/// What `arg` names: `NAME:PATH` is a host's file when `NAME` is one of `remotes`; a name with
/// a `/` before its first colon (`./a:b`) is always a file here, and so is one that exists
/// here. Anything else before a colon is taken for a host that is not configured.
fn place(arg: &str, remotes: &[String], exists: impl Fn(&Path) -> bool) -> Result<Place, String> {
    let local = || Ok(Place::Local(PathBuf::from(arg)));
    let Some((name, path)) = arg.split_once(':') else {
        return local();
    };
    if name.is_empty() || name.contains('/') {
        return local();
    }
    if remotes.iter().any(|r| r == name) {
        return Ok(Place::Remote {
            host: name.to_owned(),
            // `devbox:` is the home directory there.
            path: if path.is_empty() { "~" } else { path }.to_owned(),
        });
    }
    if exists(Path::new(arg)) {
        return local();
    }
    Err(if remotes.is_empty() {
        format!("no remote named {name:?} (for a file here, write ./{arg})")
    } else {
        format!(
            "no remote named {name:?} (configured: {}; for a file here, write ./{arg})",
            remotes.join(", ")
        )
    })
}

/// A copy to carry out: one file from `from` to `to`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Copy {
    from: Place,
    to: Place,
}

/// The copies `args` (sources, then the destination) ask for. One side of each is a host;
/// several sources go into a folder.
fn plan(
    args: &[String],
    remotes: &[String],
    exists: impl Fn(&Path) -> bool,
    is_dir: impl Fn(&Path) -> bool,
) -> Result<Vec<Copy>, String> {
    let Some((dest, sources)) = args.split_last().filter(|(_, s)| !s.is_empty()) else {
        return Err("a file to copy and where to copy it".into());
    };
    let dest = place(dest, remotes, &exists)?;
    let sources = sources
        .iter()
        .map(|s| place(s, remotes, &exists))
        .collect::<Result<Vec<_>, _>>()?;
    let several = sources.len() > 1;
    let dest = match dest {
        Place::Local(dir) if several && !is_dir(&dir) => {
            return Err(format!(
                "{} is not a folder (several files go into one)",
                dir.display()
            ));
        }
        // Said to be a folder, so the host refuses it when it is not, rather than write every
        // file over the last.
        Place::Remote { host, path } if several && !path.ends_with('/') => Place::Remote {
            host,
            path: format!("{path}/"),
        },
        dest => dest,
    };
    sources
        .into_iter()
        .map(|from| match (&from, &dest) {
            (Place::Local(_), Place::Local(_)) => Err(format!(
                "{from} and {dest} are both on this machine: name a host's file as NAME:PATH"
            )),
            (Place::Remote { .. }, Place::Remote { .. }) => Err(format!(
                "{from} and {dest} are both on a host: copy the file to this machine first"
            )),
            _ => Ok(Copy {
                from,
                to: dest.clone(),
            }),
        })
        .collect()
}

/// The hosts connected to so far: one connection each, however many files.
#[derive(Default)]
struct Hosts(HashMap<String, (Arc<Client>, PathBuf)>);

impl Hosts {
    fn get(&mut self, name: &str) -> Result<(Arc<Client>, PathBuf), Box<dyn std::error::Error>> {
        if let Some(known) = self.0.get(name) {
            return Ok(known.clone());
        }
        let client = crate::remote::connect_remote(name)?;
        if !client.has_capability(thurm_proto::CAP_COPY) {
            return Err(format!(
                "Thurm on {name} is too old to copy files; `thurm remote install {name}`"
            )
            .into());
        }
        let known = (client, thurm_config::remote_socket_path(name));
        self.0.insert(name.to_owned(), known.clone());
        Ok(known)
    }
}

/// Shows how far a file got on standard error, when that is a terminal: one line, rewritten.
struct Meter {
    shown: bool,
    label: String,
    last: Option<u64>,
}

impl Meter {
    fn new(label: String, quiet: bool) -> Meter {
        Meter {
            shown: !quiet && std::io::stderr().is_terminal(),
            label,
            last: None,
        }
    }

    fn at(&mut self, p: Progress) {
        if !self.shown {
            return;
        }
        // An empty file is all there once it is begun.
        let percent = p
            .done
            .saturating_mul(100)
            .checked_div(p.total)
            .unwrap_or(100);
        if self.last == Some(percent) {
            return;
        }
        self.last = Some(percent);
        eprint!("\r\x1b[K{} {percent}%", self.label);
        let _ = std::io::stderr().flush();
    }

    fn done(&mut self) {
        if self.shown && self.last.take().is_some() {
            eprint!("\r\x1b[K");
            let _ = std::io::stderr().flush();
        }
    }
}

/// `thurm cp [--force] SOURCE... DEST`.
pub fn run(args: &[String], force: bool, json: bool) -> R {
    let remotes: Vec<String> = thurm_config::Config::load()
        .map_err(|e| e.to_string())?
        .remote
        .iter()
        .map(|r| r.name.clone())
        .collect();
    let copies = plan(args, &remotes, |p| p.exists(), |p| p.is_dir())?;

    // Ctrl-C ends the file on its way and leaves no part of it, here or on the host.
    let cancel = Cancel::default();
    let interrupted = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(sig, interrupted.clone())?;
    }
    let finished = Arc::new(AtomicBool::new(false));
    {
        let (interrupted, finished, cancel) =
            (interrupted.clone(), finished.clone(), cancel.clone());
        std::thread::Builder::new()
            .name("cp-interrupt".into())
            .spawn(move || {
                while !finished.load(Ordering::Relaxed) {
                    if interrupted.load(Ordering::Relaxed) {
                        cancel.cancel();
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            })?;
    }

    let mut hosts = Hosts::default();
    let mut copied = Vec::new();
    let mut failed = None;
    for copy in &copies {
        let mut meter = Meter::new(copy.from.to_string(), json);
        let result = one(&mut hosts, copy, force, &cancel, |p| meter.at(p));
        meter.done();
        match result {
            Ok((to, bytes)) => copied.push(serde_json::json!({
                "from": copy.from.to_string(), "to": to, "bytes": bytes,
            })),
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    finished.store(true, Ordering::Relaxed);
    if json {
        println!("{}", serde_json::Value::Array(copied));
    }
    match failed {
        None => Ok(ExitCode::SUCCESS),
        Some(e) => Err(e),
    }
}

/// Carries out one copy: where the file is now, as it is named to the user, and its size.
fn one(
    hosts: &mut Hosts,
    copy: &Copy,
    force: bool,
    cancel: &Cancel,
    progress: impl FnMut(Progress),
) -> Result<(String, u64), Box<dyn std::error::Error>> {
    match (&copy.from, &copy.to) {
        (Place::Local(src), Place::Remote { host, path }) => {
            let (client, socket) = hosts.get(host)?;
            let at = Host {
                client: &client,
                socket: &socket,
            };
            // Where it landed, not the folder that was named.
            let (landed, bytes) = transfer::copy_to(at, src, path, force, cancel, progress)
                .map_err(|e| said(host, e))?;
            Ok((format!("{host}:{landed}"), bytes))
        }
        (Place::Remote { host, path }, Place::Local(dest)) => {
            let (client, socket) = hosts.get(host)?;
            let at = Host {
                client: &client,
                socket: &socket,
            };
            let (to, bytes) = transfer::copy_from(at, path, dest, force, cancel, progress)
                .map_err(|e| said(host, e))?;
            Ok((to.display().to_string(), bytes))
        }
        _ => unreachable!("planned with one side on a host"),
    }
}

/// A transfer's error with the host named where the host said it.
fn said(host: &str, e: TransferError) -> String {
    match e {
        TransferError::Remote(why) | TransferError::NotFound(why) => format!("{host}: {why}"),
        TransferError::Unsupported => {
            format!("Thurm on {host} is too old to copy files; `thurm remote install {host}`")
        }
        e => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remotes() -> Vec<String> {
        vec!["devbox".into(), "pi".into()]
    }

    fn local(p: &str) -> Place {
        Place::Local(p.into())
    }

    fn remote(host: &str, path: &str) -> Place {
        Place::Remote {
            host: host.into(),
            path: path.into(),
        }
    }

    #[test]
    fn a_configured_name_before_a_colon_is_a_host() {
        let none = |_: &Path| false;
        let r = remotes();
        assert_eq!(
            place("devbox:a.txt", &r, none),
            Ok(remote("devbox", "a.txt"))
        );
        assert_eq!(
            place("devbox:/var/log/x:y", &r, none),
            Ok(remote("devbox", "/var/log/x:y"))
        );
        // No path: the home directory there.
        assert_eq!(place("pi:", &r, none), Ok(remote("pi", "~")));
        assert_eq!(place("a.txt", &r, none), Ok(local("a.txt")));
        // A slash before the colon, or nothing before it: a file here.
        assert_eq!(place("./devbox:a", &r, none), Ok(local("./devbox:a")));
        assert_eq!(place("dir/a:b", &r, none), Ok(local("dir/a:b")));
        assert_eq!(place(":a", &r, none), Ok(local(":a")));
    }

    #[test]
    fn an_unknown_name_before_a_colon_is_a_file_only_when_it_is_there() {
        let r = remotes();
        assert_eq!(
            place("notes:old", &r, |p| p == Path::new("notes:old")),
            Ok(local("notes:old"))
        );
        let e = place("laptop:a", &r, |_| false).unwrap_err();
        assert!(e.contains("no remote named \"laptop\""), "{e}");
        assert!(e.contains("devbox, pi") && e.contains("./laptop:a"), "{e}");
        // A configured host wins over a file of that name.
        assert_eq!(place("pi:a", &r, |_| true), Ok(remote("pi", "a")));
    }

    #[test]
    fn copies_have_one_side_on_a_host() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let r = remotes();
        let none = |_: &Path| false;
        let plan_of = |a: &[&str], dir: bool| plan(&args(a), &r, none, |_| dir);

        assert_eq!(
            plan_of(&["a.txt", "devbox:work/"], false),
            Ok(vec![Copy {
                from: local("a.txt"),
                to: remote("devbox", "work/"),
            }])
        );
        // Several files: the host's destination is said to be a folder.
        let several = plan_of(&["a", "b", "devbox:work"], false).unwrap();
        assert_eq!(several.len(), 2);
        assert!(several.iter().all(|c| c.to == remote("devbox", "work/")));
        // From two hosts into a folder here.
        let down = plan_of(&["devbox:a", "pi:b", "out"], true).unwrap();
        assert_eq!(down[1].from, remote("pi", "b"));
        assert_eq!(down[1].to, local("out"));

        let e = plan_of(&["devbox:a", "pi:b", "out"], false).unwrap_err();
        assert!(e.contains("out is not a folder"), "{e}");
        let e = plan_of(&["a", "b"], false).unwrap_err();
        assert!(e.contains("both on this machine"), "{e}");
        let e = plan_of(&["devbox:a", "pi:a"], false).unwrap_err();
        assert!(e.contains("both on a host"), "{e}");
        assert!(plan_of(&["a"], false).is_err());
        assert!(plan_of(&[], false).is_err());
    }
}
