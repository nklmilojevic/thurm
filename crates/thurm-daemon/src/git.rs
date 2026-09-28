//! Per-pane git status (repository, branch, diff size) for tab sidebars, probed with the
//! `git` CLI off the monitor thread.

use std::path::Path;
use std::process::{Command, Stdio};

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
    // `HEAD` fails in a repository without commits; then everything is untracked anyway.
    if let Some(numstat) = git(&root, &["diff", "--numstat", "HEAD"]) {
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

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Never take the index lock (a probe must not get in the way of the user's git).
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
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
}
