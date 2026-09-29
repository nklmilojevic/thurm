//! Handoffs against a local "host": temporary repositories, and a directory standing in for
//! the remote home (no ssh).

use std::path::{Path, PathBuf};
use std::process::Command;

use thurm_remote::handoff::{self, Host, LocalHost, Registry};

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    host: LocalHost,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.org")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.org")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("thurm-handoff-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Local User"]);
        git(&repo, &["config", "user.email", "local@example.org"]);
        std::fs::write(repo.join("README"), "hello\n").unwrap();
        std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let home = dir.join("remote-home");
        std::fs::create_dir_all(&home).unwrap();
        Fixture {
            repo,
            host: LocalHost {
                name: "devbox".into(),
                home,
            },
            dir,
        }
    }

    /// Everything that identifies the local state: files, index, stash, refs other than the
    /// handoff's own remote.
    fn local_state(&self) -> String {
        let mut files: Vec<String> = walk(&self.repo)
            .into_iter()
            .filter(|p| !p.starts_with(self.repo.join(".git")))
            .map(|p| format!("{}={:?}", p.display(), std::fs::read(&p).unwrap()))
            .collect();
        files.sort();
        let index = std::fs::read(self.repo.join(".git/index")).unwrap();
        let stash = git(&self.repo, &["stash", "list"]);
        let status = git(&self.repo, &["status", "--porcelain=v2", "--branch"]);
        let head = git(&self.repo, &["rev-parse", "HEAD"]);
        format!("{files:?}\n{index:?}\n{stash}\n{status}\n{head}")
    }

    /// The agent works in the worktree.
    fn agent_commit(&self, worktree: &str, file: &str, text: &str) -> String {
        let wt = Path::new(worktree);
        std::fs::write(wt.join(file), text).unwrap();
        git(wt, &["add", file]);
        git(wt, &["commit", "-q", "-m", &format!("agent: {file}")]);
        git(wt, &["rev-parse", "HEAD"])
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn dirty_tree_snapshot_leaves_local_state_identical() {
    let f = Fixture::new("dirty");
    std::fs::write(f.repo.join("README"), "hello, edited\n").unwrap();
    std::fs::write(f.repo.join("new.txt"), "untracked\n").unwrap();
    std::fs::write(f.repo.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(f.repo.join("staged.txt"), "staged\n").unwrap();
    git(&f.repo, &["add", "staged.txt"]);
    let before = f.local_state();

    let h = handoff::prepare(&f.host, &f.repo, None).unwrap();
    assert!(h.wip);
    assert_eq!(
        f.local_state(),
        before,
        "tree, index, stash and HEAD untouched"
    );

    // The worktree has every change, the ignored file excepted.
    let wt = Path::new(&h.worktree);
    assert!(wt.starts_with(&f.host.home));
    assert_eq!(
        std::fs::read_to_string(wt.join("README")).unwrap(),
        "hello, edited\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("new.txt")).unwrap(),
        "untracked\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("staged.txt")).unwrap(),
        "staged\n"
    );
    assert!(
        !wt.join(".env").exists(),
        "gitignored files stay on the Mac"
    );
    // The WIP commit sits on top of HEAD.
    assert_eq!(
        git(wt, &["rev-parse", "HEAD^"]),
        git(&f.repo, &["rev-parse", "HEAD"])
    );
    // The agent commits as the user.
    assert_eq!(git(wt, &["config", "user.email"]), "local@example.org");
}

#[test]
fn clean_tree_hands_off_head_and_sets_up_the_remote() {
    let f = Fixture::new("clean");
    let head = git(&f.repo, &["rev-parse", "HEAD"]);
    let h = handoff::prepare(&f.host, &f.repo, Some("agent/fix-login")).unwrap();
    assert!(!h.wip);
    assert_eq!(h.base, head);
    assert_eq!(h.branch, "agent/fix-login");
    assert_eq!(h.id, format!("devbox/{}/fix-login", h.repo_id));
    assert!(
        h.mirror
            .ends_with(&format!(".local/share/thurm/mirrors/{}.git", h.repo_id))
    );
    assert!(h.worktree.ends_with(&format!(
        ".local/share/thurm/worktrees/{}/fix-login",
        h.repo_id
    )));
    // Bare mirror with the branch; worktree on it.
    assert_eq!(
        git(Path::new(&h.mirror), &["rev-parse", "--is-bare-repository"]),
        "true"
    );
    assert_eq!(
        git(
            Path::new(&h.mirror),
            &["rev-parse", "refs/heads/agent/fix-login"]
        ),
        head
    );
    assert_eq!(
        git(Path::new(&h.worktree), &["branch", "--show-current"]),
        "agent/fix-login"
    );
    // The local repo knows the mirror as thurm-devbox.
    assert_eq!(
        git(&f.repo, &["remote", "get-url", "thurm-devbox"]),
        h.mirror
    );
    // The name is taken now.
    assert!(handoff::prepare(&f.host, &f.repo, Some("agent/fix-login")).is_err());
    // The repo id comes from the root commit without an origin, from origin with one.
    assert!(h.repo_id.starts_with("repo-root-"));
    git(
        &f.repo,
        &["remote", "add", "origin", "git@github.com:me/Thurm.git"],
    );
    assert!(handoff::repo_id(&f.repo).unwrap().starts_with("thurm-"));
}

#[test]
fn parallel_handoffs_get_their_own_branches_and_worktrees() {
    let f = Fixture::new("parallel");
    let a = handoff::prepare(&f.host, &f.repo, None).unwrap();
    let b = handoff::prepare(&f.host, &f.repo, None).unwrap();
    let c = handoff::prepare(&f.host, &f.repo, None).unwrap();
    let slugs = [&a.slug, &b.slug, &c.slug];
    assert!(slugs[0] != slugs[1] && slugs[1] != slugs[2] && slugs[0] != slugs[2]);
    assert_eq!(a.mirror, b.mirror, "one mirror per repository per host");
    for h in [&a, &b, &c] {
        assert!(Path::new(&h.worktree).is_dir());
    }
    let ca = f.agent_commit(&a.worktree, "a.txt", "from a\n");
    let cb = f.agent_commit(&b.worktree, "b.txt", "from b\n");
    let (mut a, mut b) = (a, b);
    assert_eq!(handoff::fetch(&f.host, &mut a).unwrap(), ca);
    assert_eq!(handoff::fetch(&f.host, &mut b).unwrap(), cb);
    assert_eq!(git(&f.repo, &["rev-parse", &a.tracking_ref()]), ca);
    assert_eq!(git(&f.repo, &["rev-parse", &b.tracking_ref()]), cb);
    let unfetched = Command::new("git")
        .arg("-C")
        .arg(&f.repo)
        .args(["rev-parse", "-q", "--verify", &c.tracking_ref()])
        .output()
        .unwrap();
    assert!(!unfetched.status.success(), "c was never fetched");
}

#[test]
fn fetch_brings_committed_work_back_without_touching_local_branches() {
    let f = Fixture::new("fetch");
    let mut h = handoff::prepare(&f.host, &f.repo, None).unwrap();
    let before = f.local_state();
    let first = handoff::fetch(&f.host, &mut h).unwrap();
    assert_eq!(first, h.base);
    let c1 = f.agent_commit(&h.worktree, "work.txt", "1\n");
    // Uncommitted work is not part of the return.
    std::fs::write(Path::new(&h.worktree).join("scratch.txt"), "wip\n").unwrap();
    assert_eq!(handoff::fetch(&f.host, &mut h).unwrap(), c1);
    assert_eq!(h.fetched.as_deref(), Some(c1.as_str()));
    assert!(h.fetch_error.is_none() && h.fetched_at.is_some());
    assert_eq!(git(&f.repo, &["rev-parse", &h.tracking_ref()]), c1);
    assert_eq!(f.local_state(), before, "fetch never merges or checks out");

    // A failed fetch is kept on the handoff and retried later.
    let mirror = PathBuf::from(&h.mirror);
    let moved = mirror.with_extension("moved");
    std::fs::rename(&mirror, &moved).unwrap();
    assert!(handoff::fetch(&f.host, &mut h).is_err());
    assert!(h.fetch_error.is_some());
    assert_eq!(
        h.fetched.as_deref(),
        Some(c1.as_str()),
        "last good fetch kept"
    );
    std::fs::rename(&moved, &mirror).unwrap();
    assert_eq!(handoff::fetch(&f.host, &mut h).unwrap(), c1);
    assert!(h.fetch_error.is_none());
}

#[test]
fn cleanup_refuses_to_lose_work_unless_forced() {
    let f = Fixture::new("cleanup");
    let mut h = handoff::prepare(&f.host, &f.repo, None).unwrap();

    // Uncommitted changes in the worktree.
    std::fs::write(Path::new(&h.worktree).join("dirty.txt"), "x\n").unwrap();
    let check = handoff::check_cleanup(&f.host, &h).unwrap();
    assert!(check.uncommitted && !check.safe());
    let err = handoff::cleanup(&f.host, &mut h, false).unwrap_err();
    assert!(err.contains("uncommitted"), "{err}");
    assert!(Path::new(&h.worktree).exists());
    std::fs::remove_file(Path::new(&h.worktree).join("dirty.txt")).unwrap();

    // Commits the Mac has not fetched: cleanup fetches them first, then it is safe.
    let c = f.agent_commit(&h.worktree, "done.txt", "done\n");
    let check = handoff::check_cleanup(&f.host, &h).unwrap();
    assert!(check.unfetched && !check.uncommitted);
    let out = handoff::cleanup(&f.host, &mut h, false).unwrap();
    assert!(!Path::new(&h.worktree).exists());
    // Not merged locally: the branch stays on the host, the tracking ref here.
    assert!(!out.branch_deleted, "{}", out.message);
    assert_eq!(
        git(
            Path::new(&h.mirror),
            &["rev-parse", &format!("refs/heads/{}", h.branch)]
        ),
        c
    );
    assert_eq!(git(&f.repo, &["rev-parse", &h.tracking_ref()]), c);
}

#[test]
fn cleanup_deletes_merged_branches_and_forces_through_dirty_worktrees() {
    let f = Fixture::new("merged");
    let mut h = handoff::prepare(&f.host, &f.repo, None).unwrap();
    let c = f.agent_commit(&h.worktree, "feature.txt", "feature\n");
    handoff::fetch(&f.host, &mut h).unwrap();
    // The user merges the result locally.
    git(&f.repo, &["merge", "-q", "--ff-only", &h.tracking_ref()]);
    assert_eq!(git(&f.repo, &["rev-parse", "main"]), c);
    let out = handoff::cleanup(&f.host, &mut h, false).unwrap();
    assert!(out.branch_deleted, "{}", out.message);
    let branches = git(Path::new(&h.mirror), &["branch", "--list", "agent/*"]);
    assert!(branches.is_empty(), "{branches}");
    assert_eq!(
        git(&f.repo, &["rev-parse", &h.tracking_ref()]),
        c,
        "tracking ref left"
    );

    // Forced: uncommitted work is dropped on request.
    let mut g = handoff::prepare(&f.host, &f.repo, None).unwrap();
    std::fs::write(Path::new(&g.worktree).join("junk.txt"), "x\n").unwrap();
    assert!(handoff::cleanup(&f.host, &mut g, false).is_err());
    handoff::cleanup(&f.host, &mut g, true).unwrap();
    assert!(!Path::new(&g.worktree).exists());
}

#[test]
fn registry_round_trip() {
    let f = Fixture::new("registry");
    let reg = Registry {
        path: f.dir.join("state/handoffs.json"),
    };
    assert!(reg.list().is_empty());
    let mut h = handoff::prepare(&f.host, &f.repo, None).unwrap();
    reg.put(&h).unwrap();
    h.pane = Some(7);
    reg.put(&h).unwrap();
    assert_eq!(reg.list().len(), 1);
    assert_eq!(reg.get(&h.id).unwrap().pane, Some(7));
    reg.remove(&h.id).unwrap();
    assert!(reg.get(&h.id).is_none());
    assert_eq!(f.host.name(), "devbox");
}

#[test]
fn concurrent_registry_writes_keep_every_handoff() {
    let f = Fixture::new("registry-race");
    let path = f.dir.join("state/handoffs.json");
    let template = handoff::prepare(&f.host, &f.repo, None).unwrap();
    std::thread::scope(|s| {
        for t in 0..8 {
            let (path, template) = (path.clone(), template.clone());
            s.spawn(move || {
                let reg = Registry { path };
                for i in 0..20 {
                    let mut h = template.clone();
                    h.id = format!("{t}-{i}");
                    reg.put(&h).unwrap();
                    reg.update(&h.id, |h| h.pane = Some(i)).unwrap();
                }
            });
        }
    });
    let all = Registry { path: path.clone() }.list();
    assert_eq!(all.len(), 160);
    assert!(all.iter().all(|h| h.pane.is_some()));
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn a_fetch_neither_revives_a_removed_handoff_nor_resets_its_pane() {
    let f = Fixture::new("registry-fetch");
    let reg = Registry {
        path: f.dir.join("state/handoffs.json"),
    };
    let mut h = handoff::prepare(&f.host, &f.repo, None).unwrap();
    reg.put(&h).unwrap();
    // The pane is recorded while a fetch that read the handoff earlier is still running.
    reg.update(&h.id, |x| x.pane = Some(9)).unwrap();
    handoff::fetch(&f.host, &mut h).unwrap();
    let stored = reg.record_fetch(&h).unwrap().unwrap();
    assert_eq!(stored.pane, Some(9));
    assert_eq!(stored.fetched, h.fetched);
    // Cleaned up meanwhile: it stays gone.
    reg.remove(&h.id).unwrap();
    assert!(reg.record_fetch(&h).unwrap().is_none());
    assert!(reg.update(&h.id, |x| x.pane = None).unwrap().is_none());
    assert!(reg.list().is_empty());
}

#[test]
fn concurrent_snapshots_of_one_repository() {
    let f = Fixture::new("snapshot-race");
    std::fs::write(f.repo.join("README"), "edited\n").unwrap();
    let snaps: Vec<_> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..6)
            .map(|_| s.spawn(|| handoff::snapshot(&f.repo)))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap().unwrap()).collect()
    });
    let tree = |c: &str| git(&f.repo, &["rev-parse", &format!("{c}^{{tree}}")]);
    let first = tree(&snaps[0].commit);
    assert!(snaps.iter().all(|s| s.wip && tree(&s.commit) == first));
}
