//! Per-pane git status (repository, branch, diff size) for tab sidebars, probed with the
//! `git` CLI off the monitor thread.
//!
//! The probe runs in whatever directory a pane is in (which program output can also set, with
//! OSC 7), so it must not run code a repository's own config names: fsmonitor hooks, external
//! diff and textconv commands, and filter drivers are all kept out.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A git command that takes longer (a huge or hung repository) is killed.
const TIMEOUT: Duration = Duration::from_secs(3);

use thurm_proto::GitInfo;

/// Repository status of `cwd`, or `None` when it isn't inside a work tree (or git is missing).
pub fn probe(cwd: &str) -> Option<GitInfo> {
    if !Path::new(cwd).is_dir() {
        return None;
    }
    let head = git(
        cwd,
        &["rev-parse", "--show-toplevel", "--abbrev-ref", "HEAD"],
    )?;
    let mut lines = head.lines();
    let root = lines.next()?.trim().to_owned();
    let mut branch = lines.next().unwrap_or("").trim().to_owned();
    if branch == "HEAD" {
        // Detached: show the short commit instead.
        branch = git(cwd, &["rev-parse", "--short", "HEAD"])
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
    }
    let (mut added, mut removed) = (0u32, 0u32);
    // Diffing the work tree runs the clean filter of files whose attributes name one; a
    // repository that defines its own filter drivers gets no counts. Reading config runs
    // nothing.
    // Anything but a clear "none" (exit status 1) counts as having them.
    let own_filters = !matches!(
        run_git(
            &root,
            &[
                "config",
                "--local",
                "--get-regexp",
                r"^filter\..*\.(clean|smudge|process)$",
            ],
        ),
        Some((Some(1), _))
    );
    // `HEAD` fails in a repository without commits; then everything is untracked anyway.
    let numstat = (!own_filters)
        .then(|| {
            git(
                &root,
                &[
                    "diff",
                    "--numstat",
                    "--no-ext-diff",
                    "--no-textconv",
                    "HEAD",
                ],
            )
        })
        .flatten();
    if let Some(numstat) = numstat {
        for line in numstat.lines() {
            let mut f = line.split('\t');
            // Binary files show "-".
            added += f.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
            removed += f.next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
        }
    }
    Some(GitInfo {
        root,
        branch,
        added,
        removed,
    })
}

/// Output of a successful git command.
fn git(dir: &str, args: &[&str]) -> Option<String> {
    match run_git(dir, args)? {
        (Some(0), out) => Some(out),
        _ => None,
    }
}

/// Exit status and output of a git command; `None` when it didn't start or timed out.
fn run_git(dir: &str, args: &[&str]) -> Option<(Option<i32>, String)> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        // A repository's config must not make the probe run programs.
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .args(args)
        // Never take the index lock (a probe must not get in the way of the user's git).
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read while it runs (a large diff fills the pipe), and give up after TIMEOUT.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let out = reader.join().ok()?;
    Some((status.code(), String::from_utf8_lossy(&out).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn branch_and_diff_counts() {
        let dir = std::env::temp_dir().join(format!("thurm-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "1\n2\n3\n").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("a.txt"), "1\nchanged\n3\n4\n5\n").unwrap();

        let g = probe(dir.join("sub").to_str().unwrap()).expect("inside a repo");
        assert_eq!(
            Path::new(&g.root).canonicalize().unwrap(),
            dir.canonicalize().unwrap()
        );
        assert_eq!(g.branch, "main");
        assert_eq!((g.added, g.removed), (3, 1));
        assert!(probe("/").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repository_config_runs_no_programs() {
        let dir = std::env::temp_dir().join(format!("thurm-git-evil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "1\n").unwrap();
        run(&dir, &["add", "."]);
        run(&dir, &["commit", "-q", "-m", "init"]);
        // What an extracted archive's .git/config could hold.
        let marker = dir.join("ran");
        let hook = format!("touch {}", marker.display());
        run(&dir, &["config", "core.fsmonitor", &hook]);
        run(&dir, &["config", "diff.external", &hook]);
        run(&dir, &["config", "filter.evil.clean", &hook]);
        std::fs::write(dir.join(".gitattributes"), "* filter=evil diff=evil\n").unwrap();
        run(&dir, &["config", "diff.evil.textconv", &hook]);
        std::fs::write(dir.join("a.txt"), "1\n2\n").unwrap();

        let g = probe(dir.to_str().unwrap()).expect("inside a repo");
        assert_eq!(g.branch, "main");
        // The repository's own filter driver means no counts, rather than running it.
        assert_eq!((g.added, g.removed), (0, 0));
        assert!(
            !marker.exists(),
            "the probe ran a program from the repository's config"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
