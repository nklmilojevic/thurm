//! Handing a local repository to a remote agent through git.
//!
//! 1. Snapshot: HEAD, or, with local changes, a WIP commit on top of it made through a
//!    temporary index (the working tree, the index and the stash list stay untouched).
//! 2. A bare mirror per repository on the host (`~/.local/share/thurm/mirrors/<id>.git`).
//! 3. The snapshot is pushed to `refs/heads/agent/<slug>` there,
//! 4. and checked out in its own worktree (`~/.local/share/thurm/worktrees/<id>/<slug>`).
//! 5. The local repository gets a remote `thurm-<host>` for the mirror.
//!
//! The agent commits; Thurm fetches `agent/<slug>` into `refs/remotes/thurm-<host>/` and
//! never merges, checks out or rebases anything locally.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ssh::Ssh;

/// Where a handoff's commands run: the remote host over ssh, or (tests) this machine with a
/// local path as the "remote".
pub trait Host {
    /// The `[[remote]]` name (`thurm-<name>` is the local git remote).
    fn name(&self) -> &str;
    /// Runs a POSIX sh script there with `args` as `$1`...; stdout.
    fn run(&self, script: &str, args: &[&str]) -> Result<String, String>;
    /// A URL git on this machine can push to for `path` there.
    fn git_url(&self, path: &str) -> String;
    /// Environment for local git talking to the host.
    fn git_env(&self) -> Vec<(String, String)>;
}

/// A `[[remote]]` host reached with [`Ssh`].
pub struct SshHost {
    pub name: String,
    pub ssh: Ssh,
}

impl Host for SshHost {
    fn name(&self) -> &str {
        &self.name
    }

    fn run(&self, script: &str, args: &[&str]) -> Result<String, String> {
        let script = format!(
            "{}; {}; {script}",
            crate::ssh::PRELUDE,
            crate::ssh::REMOTE_PATH
        );
        self.ssh
            .run(&script, args, None, Duration::from_secs(120))
            .map_err(|e| e.to_string())
    }

    fn git_url(&self, path: &str) -> String {
        self.ssh.git_url(path)
    }

    fn git_env(&self) -> Vec<(String, String)> {
        vec![("GIT_SSH_COMMAND".into(), self.ssh.git_ssh_command())]
    }
}

/// This machine standing in for a host (tests): `home` replaces the home directory.
pub struct LocalHost {
    pub name: String,
    pub home: PathBuf,
}

impl Host for LocalHost {
    fn name(&self) -> &str {
        &self.name
    }

    fn run(&self, script: &str, args: &[&str]) -> Result<String, String> {
        let script = format!("{}; {script}", crate::ssh::PRELUDE);
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg("thurm")
            .args(args)
            .env("THURM_HOME", &self.home)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }

    fn git_url(&self, path: &str) -> String {
        path.to_owned()
    }

    fn git_env(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// One handoff, as the registry keeps it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Handoff {
    /// `<host>/<repo id>/<slug>`; tags the tab.
    pub id: String,
    pub host: String,
    /// The local repository (its top level).
    pub repo: String,
    pub repo_id: String,
    pub slug: String,
    /// `agent/<slug>`.
    pub branch: String,
    /// On the host.
    pub mirror: String,
    pub worktree: String,
    /// `thurm-<host>`.
    pub git_remote: String,
    /// The pushed snapshot.
    pub base: String,
    /// The snapshot carried uncommitted changes (a WIP commit).
    pub wip: bool,
    pub created: u64,
    /// Last successful fetch (unix seconds) and what it brought.
    #[serde(default)]
    pub fetched_at: Option<u64>,
    #[serde(default)]
    pub fetched: Option<String>,
    /// The last fetch failed with this.
    #[serde(default)]
    pub fetch_error: Option<String>,
    /// The remote pane running the agent.
    #[serde(default)]
    pub pane: Option<u64>,
    /// The tab was closed while the host was offline: clean up on reconnect.
    #[serde(default)]
    pub pending_cleanup: bool,
}

impl Handoff {
    /// `refs/remotes/thurm-<host>/agent/<slug>`.
    pub fn tracking_ref(&self) -> String {
        format!("refs/remotes/{}/{}", self.git_remote, self.branch)
    }
}

fn git(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(repo);
    // Never prompt (no terminal here), never let a pager or editor start.
    c.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_EDITOR", "true");
    c
}

fn run_git(mut cmd: Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

fn git_out(repo: &Path, args: &[&str]) -> Result<String, String> {
    let mut c = git(repo);
    c.args(args);
    run_git(c)
}

/// The top level of the repository containing `path`.
pub fn toplevel(path: &Path) -> Result<PathBuf, String> {
    git_out(path, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|e| format!("{} is not in a git repository: {e}", path.display()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub commit: String,
    pub wip: bool,
}

/// Untracked files a snapshot leaves out: they usually hold secrets (`.env.local`), and a
/// handoff pushes the snapshot to another machine.
const SECRET_PATHSPECS: &[&str] = &[
    ":(exclude,glob)**/.env",
    ":(exclude,glob)**/.env.*",
    ":(exclude,glob)**/*.pem",
    ":(exclude,glob)**/*.key",
];

/// HEAD, or a WIP commit of HEAD plus every change (untracked files included, ignored ones
/// and [`SECRET_PATHSPECS`] not), made through a temporary index so nothing local changes.
pub fn snapshot(repo: &Path) -> Result<Snapshot, String> {
    let head = git_out(repo, &["rev-parse", "--verify", "-q", "HEAD^{commit}"])
        .map_err(|_| "the repository has no commits yet; commit something first".to_owned())?;
    let git_dir = PathBuf::from(git_out(repo, &["rev-parse", "--absolute-git-dir"])?);
    // One index per snapshot: the app may snapshot the same repository concurrently.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let index = git_dir.join(format!(
        "thurm-handoff-index-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let with_index = |args: &[&str]| {
        let mut c = git(repo);
        c.env("GIT_INDEX_FILE", &index).args(args);
        run_git(c)
    };
    let result = (|| {
        with_index(&["read-tree", "HEAD"])?;
        let mut add = vec!["add", "-A", "--", "."];
        add.extend(SECRET_PATHSPECS);
        with_index(&add)?;
        let tree = with_index(&["write-tree"])?;
        let head_tree = git_out(repo, &["rev-parse", "HEAD^{tree}"])?;
        if tree == head_tree {
            return Ok(Snapshot {
                commit: head.clone(),
                wip: false,
            });
        }
        let mut c = git(repo);
        c.args([
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-m",
            "WIP: handed off by Thurm",
        ]);
        // Commits need an identity; don't fail on a machine without one configured.
        if git_out(repo, &["config", "user.email"]).is_err() {
            c.env("GIT_AUTHOR_NAME", "Thurm")
                .env("GIT_AUTHOR_EMAIL", "thurm@localhost")
                .env("GIT_COMMITTER_NAME", "Thurm")
                .env("GIT_COMMITTER_EMAIL", "thurm@localhost");
        }
        let commit = run_git(c)?;
        Ok(Snapshot { commit, wip: true })
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// Names the repository on every host: from its origin URL, else its root commit.
pub fn repo_id(repo: &Path) -> Result<String, String> {
    if let Ok(url) = git_out(repo, &["remote", "get-url", "origin"])
        && !url.is_empty()
    {
        let norm = normalize_url(&url);
        let base = norm.rsplit('/').next().unwrap_or("repo");
        let hash = hex(&Sha256::digest(norm.as_bytes()));
        return Ok(format!("{}-{}", sanitize(base), &hash[..8]));
    }
    let roots = git_out(repo, &["rev-list", "--max-parents=0", "HEAD"])?;
    let root = roots.lines().last().unwrap_or("").trim();
    if root.len() < 12 {
        return Err("cannot identify the repository (no origin, no commits)".into());
    }
    let name = repo
        .file_name()
        .and_then(|n| n.to_str())
        .map(sanitize)
        .unwrap_or_else(|| "repo".into());
    Ok(format!("{name}-root-{}", &root[..12]))
}

/// `git@github.com:me/x.git`, `https://github.com/me/x` and `ssh://git@github.com/me/x.git`
/// are one repository: `github.com/me/x`.
fn normalize_url(url: &str) -> String {
    let mut u = url.trim().trim_end_matches('/').to_owned();
    if let Some(i) = u.find("://") {
        u = u[i + 3..].to_owned();
    } else if let Some((host, path)) = u.split_once(':')
        && !host.contains('/')
    {
        u = format!("{host}/{path}");
    }
    if let Some((_, rest)) = u.split_once('@')
        && !rest.is_empty()
        && u.find('@') < u.find('/')
    {
        u = rest.to_owned();
    }
    // A port is not part of the identity.
    if let Some((host, path)) = u.split_once('/')
        && let Some((h, _port)) = host.split_once(':')
    {
        u = format!("{h}/{path}");
    }
    u.trim_end_matches(".git").to_ascii_lowercase()
}

fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let out = out.trim_matches(|c| c == '.' || c == '-').to_owned();
    if out.is_empty() { "repo".into() } else { out }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks a user supplied `agent/<slug>`.
pub fn validate_branch(branch: &str) -> Result<String, String> {
    let slug = branch
        .strip_prefix("agent/")
        .ok_or_else(|| format!("handoff branches are agent/<name>, not {branch:?}"))?;
    if slug.is_empty()
        || slug.len() > 64
        || slug.starts_with(['.', '-'])
        || slug.ends_with(".lock")
        || slug.contains("..")
        || !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        return Err(format!(
            "{branch:?}: use letters, digits, '-', '_' and '.' after agent/"
        ));
    }
    Ok(slug.to_owned())
}

const ADJECTIVES: &[&str] = &[
    "amber", "bold", "brisk", "calm", "clever", "cosmic", "crisp", "dapper", "eager", "fuzzy",
    "gentle", "golden", "happy", "hidden", "jolly", "keen", "lucky", "mellow", "misty", "nimble",
    "noble", "polar", "proud", "quiet", "rapid", "rusty", "shiny", "silent", "sly", "sunny",
    "swift", "tidy", "velvet", "vivid", "wild", "witty", "young", "zesty",
];
const NOUNS: &[&str] = &[
    "badger", "beacon", "comet", "cedar", "falcon", "fern", "fox", "harbor", "heron", "island",
    "koala", "lark", "lynx", "maple", "meadow", "moose", "nebula", "orca", "otter", "owl", "panda",
    "pebble", "pine", "puffin", "quartz", "raven", "river", "robin", "sparrow", "summit", "tiger",
    "tundra", "walrus", "willow", "wolf", "yak", "zebra",
];

/// A slug not in `taken` ("quiet-otter", then "quiet-otter-2"...).
pub fn pick_slug(taken: &[String], seed: u64) -> String {
    let mut x = seed | 1;
    let mut next = || {
        // xorshift64
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..64 {
        let s = format!(
            "{}-{}",
            ADJECTIVES[(next() % ADJECTIVES.len() as u64) as usize],
            NOUNS[(next() % NOUNS.len() as u64) as usize]
        );
        if !taken.contains(&s) {
            return s;
        }
    }
    let base = format!("{}-{}", ADJECTIVES[0], NOUNS[0]);
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|s| !taken.contains(s))
        .expect("unbounded")
}

fn seed() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    t ^ ((std::process::id() as u64) << 32)
}

const PREPARE: &str = concat!(
    "M=\"$TH/.local/share/thurm/mirrors/$1.git\"; W=\"$TH/.local/share/thurm/worktrees/$1\"; ",
    "command -v git >/dev/null 2>&1 || { echo \"git is not installed\" >&2; exit 3; }; ",
    "mkdir -p \"$TH/.local/share/thurm/mirrors\" \"$W\" || exit 1; ",
    "[ -d \"$M\" ] || git init -q --bare \"$M\" || exit 1; ",
    "echo \"home=$TH\"; ",
    "git -C \"$M\" for-each-ref --format=\"ref %(refname)\" refs/heads/agent/; ",
    "for d in \"$W\"/*; do [ -e \"$d\" ] && echo \"dir ${d##*/}\"; done; true"
);

const WORKTREE: &str = concat!(
    "M=\"$TH/.local/share/thurm/mirrors/$1.git\"; W=\"$TH/.local/share/thurm/worktrees/$1/$2\"; ",
    "git -C \"$M\" worktree add -q \"$W\" \"agent/$2\" >&2 && ",
    "{ [ -z \"$3\" ] || git -C \"$W\" config user.name \"$3\"; } && ",
    "{ [ -z \"$4\" ] || git -C \"$W\" config user.email \"$4\"; } && echo \"$W\""
);

const CHECK: &str = concat!(
    "M=\"$TH/.local/share/thurm/mirrors/$1.git\"; W=\"$TH/.local/share/thurm/worktrees/$1/$2\"; ",
    "if [ -d \"$W\" ]; then echo \"dirty=$(git -C \"$W\" status --porcelain | head -n 1 | wc -l)\"; ",
    "echo \"head=$(git -C \"$W\" rev-parse -q --verify HEAD)\"; fi; ",
    "echo \"branch=$(git -C \"$M\" rev-parse -q --verify \"refs/heads/agent/$2\")\"; true"
);

const REMOVE: &str = concat!(
    "M=\"$TH/.local/share/thurm/mirrors/$1.git\"; W=\"$TH/.local/share/thurm/worktrees/$1/$2\"; ",
    "if [ -d \"$W\" ]; then git -C \"$M\" worktree remove --force \"$W\" || exit 1; fi; ",
    "git -C \"$M\" worktree prune; ",
    "if [ \"$3\" = 1 ]; then git -C \"$M\" branch -D \"agent/$2\" >/dev/null || exit 1; fi; true"
);

/// Sets up a handoff of the repository at `path` on `host`. The caller opens the tab
/// (a pane in `worktree`) and records the pane with the returned handoff.
pub fn prepare(host: &dyn Host, path: &Path, branch: Option<&str>) -> Result<Handoff, String> {
    let repo = toplevel(path)?;
    let snap = snapshot(&repo)?;
    let id = repo_id(&repo)?;
    let out = host.run(PREPARE, &[&id])?;
    let mut home = None;
    let mut taken = Vec::new();
    for line in out.lines() {
        if let Some(h) = line.strip_prefix("home=") {
            home = Some(h.to_owned());
        } else if let Some(r) = line.strip_prefix("ref refs/heads/agent/") {
            taken.push(r.to_owned());
        } else if let Some(d) = line.strip_prefix("dir ") {
            taken.push(d.to_owned());
        }
    }
    let home = home.ok_or("the host did not report its home directory")?;
    let slug = match branch {
        Some(b) => {
            let s = validate_branch(b)?;
            if taken.contains(&s) {
                return Err(format!("agent/{s} exists on {} already", host.name()));
            }
            s
        }
        None => pick_slug(&taken, seed()),
    };
    let branch = format!("agent/{slug}");
    let mirror = format!("{home}/.local/share/thurm/mirrors/{id}.git");
    let worktree = format!("{home}/.local/share/thurm/worktrees/{id}/{slug}");
    let url = host.git_url(&mirror);

    let mut push = git(&repo);
    push.envs(host.git_env()).args([
        "push",
        "--quiet",
        "--no-verify",
        &url,
        &format!("{}:refs/heads/{branch}", snap.commit),
    ]);
    run_git(push).map_err(|e| format!("pushing to {}: {e}", host.name()))?;

    // The agent's commits carry the user's identity (when it passes through a shell).
    let ident = |k: &str| {
        git_out(&repo, &["config", k])
            .ok()
            .filter(|v| crate::ssh::quote(v).is_ok())
            .unwrap_or_default()
    };
    let (name, email) = (ident("user.name"), ident("user.email"));
    let wt = host
        .run(WORKTREE, &[&id, &slug, &name, &email])
        .map_err(|e| format!("creating the worktree on {}: {e}", host.name()))?;
    let worktree = wt.lines().last().map(str::to_owned).unwrap_or(worktree);

    let git_remote = format!("thurm-{}", host.name());
    match git_out(&repo, &["remote", "get-url", &git_remote]) {
        Ok(existing) if existing == url => {}
        Ok(_) => {
            git_out(&repo, &["remote", "set-url", &git_remote, &url])?;
        }
        Err(_) => {
            git_out(&repo, &["remote", "add", &git_remote, &url])?;
            // Only handoff branches, and only when asked (`git fetch thurm-<host>`).
            git_out(
                &repo,
                &[
                    "config",
                    &format!("remote.{git_remote}.fetch"),
                    &format!("+refs/heads/agent/*:refs/remotes/{git_remote}/agent/*"),
                ],
            )?;
        }
    }
    Ok(Handoff {
        id: format!("{}/{id}/{slug}", host.name()),
        host: host.name().to_owned(),
        repo: repo.display().to_string(),
        repo_id: id,
        slug,
        branch,
        mirror,
        worktree,
        git_remote,
        base: snap.commit,
        wip: snap.wip,
        created: crate::tunnel::now_secs(),
        fetched_at: None,
        fetched: None,
        fetch_error: None,
        pane: None,
        pending_cleanup: false,
    })
}

/// `git fetch thurm-<host> agent/<slug>` into the tracking ref. Updates `h` either way
/// (a failure is kept for the tab to show) and returns the fetched commit.
pub fn fetch(host: &dyn Host, h: &mut Handoff) -> Result<String, String> {
    let repo = PathBuf::from(&h.repo);
    let mut c = git(&repo);
    c.envs(host.git_env()).args([
        "fetch",
        "--quiet",
        "--no-tags",
        &h.git_remote,
        &format!("+refs/heads/{}:{}", h.branch, h.tracking_ref()),
    ]);
    let result = run_git(c).and_then(|_| git_out(&repo, &["rev-parse", &h.tracking_ref()]));
    match &result {
        Ok(sha) => {
            h.fetched = Some(sha.clone());
            h.fetched_at = Some(crate::tunnel::now_secs());
            h.fetch_error = None;
        }
        Err(e) => h.fetch_error = Some(e.clone()),
    }
    result
}

/// [`prepare`], then records the handoff in `reg`. When it cannot be recorded, the worktree
/// just made on the host is removed again: nothing would find it for a fetch or a cleanup.
pub fn prepare_registered(
    host: &dyn Host,
    path: &Path,
    branch: Option<&str>,
    reg: &Registry,
) -> Result<Handoff, String> {
    let mut h = prepare(host, path, branch)?;
    if let Err(e) = reg.put(&h) {
        let undone = match cleanup(host, &mut h, true) {
            Ok(_) => "removed the new worktree again".to_owned(),
            Err(c) => format!("{} is left on {} ({c})", h.worktree, host.name()),
        };
        return Err(format!(
            "cannot record the handoff in {}: {e}; {undone}",
            reg.path.display()
        ));
    }
    Ok(h)
}

/// What closing a handoff would lose.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct CleanupCheck {
    /// The worktree has changes the agent did not commit.
    pub uncommitted: bool,
    /// The host has commits the local tracking ref doesn't.
    pub unfetched: bool,
    pub remote_head: Option<String>,
    pub fetched: Option<String>,
}

impl CleanupCheck {
    pub fn safe(&self) -> bool {
        !self.uncommitted && !self.unfetched
    }
}

pub fn check_cleanup(host: &dyn Host, h: &Handoff) -> Result<CleanupCheck, String> {
    let out = host.run(CHECK, &[&h.repo_id, &h.slug])?;
    let mut c = CleanupCheck::default();
    let (mut head, mut branch) = (None, None);
    for line in out.lines() {
        match line.split_once('=') {
            Some(("dirty", v)) => c.uncommitted = v.trim() != "0",
            Some(("head", v)) if !v.trim().is_empty() => head = Some(v.trim().to_owned()),
            Some(("branch", v)) if !v.trim().is_empty() => branch = Some(v.trim().to_owned()),
            _ => {}
        }
    }
    let fetched = git_out(
        Path::new(&h.repo),
        &["rev-parse", "-q", "--verify", &h.tracking_ref()],
    )
    .ok();
    // A detached worktree HEAD with commits of its own counts too.
    c.unfetched = [&head, &branch]
        .into_iter()
        .flatten()
        .any(|sha| Some(sha) != fetched.as_ref());
    c.remote_head = branch.or(head);
    c.fetched = fetched;
    Ok(c)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CleanupOutcome {
    /// `agent/<slug>` was deleted on the host (it was merged into the default branch).
    pub branch_deleted: bool,
    pub message: String,
}

/// Final fetch, then removes the worktree on the host; deletes the branch there when the
/// local default branch contains it. Refuses to lose work unless `force`.
pub fn cleanup(host: &dyn Host, h: &mut Handoff, force: bool) -> Result<CleanupOutcome, String> {
    let fetch_error = fetch(host, h).err();
    let check = check_cleanup(host, h)?;
    if !check.safe() && !force {
        let mut what = Vec::new();
        if check.uncommitted {
            what.push("uncommitted changes");
        }
        if check.unfetched {
            what.push("commits not fetched yet");
        }
        return Err(format!(
            "the worktree on {} has {}{}",
            host.name(),
            what.join(" and "),
            fetch_error
                .map(|e| format!(" (fetch failed: {e})"))
                .unwrap_or_default()
        ));
    }
    let merged = match (&check.fetched, default_branch(Path::new(&h.repo))) {
        (Some(sha), Some(default)) => {
            let mut c = git(Path::new(&h.repo));
            c.args(["merge-base", "--is-ancestor", sha, &default]);
            c.output().map(|o| o.status.success()).unwrap_or(false)
                && check.remote_head.as_ref() == Some(sha)
        }
        _ => false,
    };
    host.run(
        REMOVE,
        &[&h.repo_id, &h.slug, if merged { "1" } else { "0" }],
    )
    .map_err(|e| format!("removing the worktree on {}: {e}", host.name()))?;
    Ok(CleanupOutcome {
        branch_deleted: merged,
        message: if merged {
            format!("Removed the worktree and {} (merged).", h.branch)
        } else {
            format!(
                "Removed the worktree; {} stays on {} (not merged into the default branch).",
                h.branch,
                host.name()
            )
        },
    })
}

/// `origin/HEAD`'s branch, else a local `main` or `master`.
fn default_branch(repo: &Path) -> Option<String> {
    if let Ok(b) = git_out(
        repo,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    ) {
        return Some(b);
    }
    ["main", "master"]
        .into_iter()
        .find(|b| {
            git_out(
                repo,
                &["rev-parse", "-q", "--verify", &format!("refs/heads/{b}")],
            )
            .is_ok()
        })
        .map(str::to_owned)
}

/// Every handoff, in `<state dir>/handoffs.json` (shared by the app and the CLI).
pub struct Registry {
    pub path: PathBuf,
}

#[derive(Serialize, Deserialize, Default)]
struct RegistryFile {
    #[serde(default)]
    handoffs: Vec<Handoff>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            path: thurm_config::state_dir().join("handoffs.json"),
        }
    }
}

impl Registry {
    pub fn list(&self) -> Vec<Handoff> {
        self.load().unwrap_or_default()
    }

    /// The stored handoffs; an error when the file exists but can't be read, so an edit
    /// doesn't replace every other handoff with its own.
    fn load(&self) -> Result<Vec<Handoff>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => serde_json::from_str::<RegistryFile>(&t)
                .map(|f| f.handoffs)
                .map_err(|e| {
                    format!(
                        "{} is unreadable ({e}); fix or remove it",
                        self.path.display()
                    )
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("{}: {e}", self.path.display())),
        }
    }

    pub fn get(&self, id: &str) -> Option<Handoff> {
        self.list().into_iter().find(|h| h.id == id)
    }

    /// Inserts or replaces (by id).
    pub fn put(&self, h: &Handoff) -> Result<(), String> {
        self.edit(|all| match all.iter_mut().find(|x| x.id == h.id) {
            Some(x) => *x = h.clone(),
            None => all.push(h.clone()),
        })
    }

    /// Changes the handoff `id` in place and answers it; `None` when it is gone (cleaned
    /// up meanwhile), which it stays.
    pub fn update(
        &self,
        id: &str,
        f: impl FnOnce(&mut Handoff),
    ) -> Result<Option<Handoff>, String> {
        let mut out = None;
        self.edit(|all| {
            if let Some(h) = all.iter_mut().find(|h| h.id == id) {
                f(h);
                out = Some(h.clone());
            }
        })?;
        Ok(out)
    }

    /// Keeps what a fetch (or a cleanup's final fetch) learned in `h`, leaving the rest of
    /// the stored handoff (its pane, a deferred cleanup) as it is now.
    pub fn record_fetch(&self, h: &Handoff) -> Result<Option<Handoff>, String> {
        self.update(&h.id, |x| {
            x.fetched = h.fetched.clone();
            x.fetched_at = h.fetched_at;
            x.fetch_error = h.fetch_error.clone();
        })
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        self.edit(|all| all.retain(|h| h.id != id))
    }

    /// Read, change, write under an exclusive lock on `<path>.lock`, which the app's
    /// threads and the CLI all take.
    fn edit(&self, f: impl FnOnce(&mut Vec<Handoff>)) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let _lock = FileLock::acquire(&self.path.with_extension("json.lock"))?;
        let mut all = self.load()?;
        f(&mut all);
        let json = serde_json::to_string_pretty(&RegistryFile { handoffs: all })
            .map_err(|e| e.to_string())?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let tmp = self.path.with_extension(format!(
            "json.tmp{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })
    }
}

/// `flock(LOCK_EX)` on a file, released on drop (and by the kernel if the process dies).
struct FileLock(std::fs::File);

impl FileLock {
    fn acquire(path: &Path) -> Result<FileLock, String> {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(FileLock(file));
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(format!("locking {}: {e}", path.display()));
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_normalized() {
        for u in [
            "git@github.com:Me/Thurm.git",
            "https://github.com/me/thurm",
            "ssh://git@github.com/me/thurm.git",
            "ssh://git@github.com:22/me/thurm.git/",
            "https://token@github.com/me/thurm.git",
        ] {
            assert_eq!(normalize_url(u), "github.com/me/thurm", "{u}");
        }
        assert_eq!(normalize_url("/srv/git/x.git"), "/srv/git/x");
    }

    #[test]
    fn branches_validated() {
        assert_eq!(validate_branch("agent/fix-login").unwrap(), "fix-login");
        for bad in [
            "fix",
            "agent/",
            "agent/a b",
            "agent/../x",
            "agent/-x",
            "agent/x.lock",
            "agent/a/b",
        ] {
            assert!(validate_branch(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn slugs_avoid_taken_names() {
        let a = pick_slug(&[], 42);
        assert!(a.contains('-'));
        let b = pick_slug(std::slice::from_ref(&a), 42);
        assert_ne!(a, b);
        // Everything taken: numbered.
        let all: Vec<String> = ADJECTIVES
            .iter()
            .flat_map(|x| NOUNS.iter().map(move |n| format!("{x}-{n}")))
            .collect();
        assert_eq!(pick_slug(&all, 7), "amber-badger-2");
    }

    #[test]
    fn scripts_are_safe_to_quote() {
        for s in [PREPARE, WORKTREE, CHECK, REMOVE] {
            crate::ssh::quote(s).unwrap();
        }
    }

    #[test]
    fn an_unreadable_registry_is_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("thurm-registry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("handoffs.json");
        std::fs::write(&path, "{ not json").unwrap();
        let r = Registry { path: path.clone() };
        assert!(r.remove("x").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshots_leave_out_secret_files() {
        let dir = std::env::temp_dir().join(format!("thurm-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("README"), "x").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        for f in ["app.txt", ".env", "sub/.env.local", "cert.pem"] {
            std::fs::write(dir.join(f), "secret?").unwrap();
        }
        let snap = snapshot(&dir).unwrap();
        assert!(snap.wip);
        let files = git(&["ls-tree", "-r", "--name-only", &snap.commit]);
        let files: Vec<&str> = files.lines().collect();
        assert!(files.contains(&"app.txt"), "{files:?}");
        for secret in [".env", "sub/.env.local", "cert.pem"] {
            assert!(
                !files.contains(&secret),
                "{secret} was handed off: {files:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
