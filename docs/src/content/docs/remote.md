---
title: Remote workspaces
description: Attach Thurm to thurmd on a Linux devbox or another Mac, and hand repositories to remote agents.
sidebar:
  label: Remote workspaces
---

Thurm can attach to `thurmd` running on other machines. A remote host's panes
show as a workspace next to your local ones, with the same tabs, splits, agent
sidebar, agent switcher (⌘⇧A) and notifications. Agents keep running there when
the Mac sleeps, changes networks, or quits.

The connection is the system `ssh`, one tunnel per host. Thurm has no network
layer, relay, or account of its own; Tailscale or any other network works
underneath.

## Add a host

```sh
thurm remote add devbox me@devbox.example.org
```

`thurm remote add NAME SSH-TARGET` connects once, shows the host's platform,
offers to install this build of Thurm there, and writes the entry to your
config. Then it checks the rest the host needs (see
[Check a host](#check-a-host)) and offers each fix. In the app, **Thurm ›
Remotes… › Add Host…** does the same. Run without a terminal (from a script),
it needs `--yes` or `--no`, and it says so instead of assuming "no".

The entry it writes:

```toml
[[remote]]
name = "devbox"          # shown in the sidebar; `thurm --remote devbox ...`
host = "me@devbox.example.org"   # a Host alias, user@host, or ssh://user@host:port
# socket = "/run/user/1000/thurm-1000/thurmd.sock"   # discovered when unset
# enabled = true
# clipboard_read = "ask"   # "ask", "always" or "never"
```

The app connects to every enabled `[[remote]]` in the background. Other
commands:

| Command | Purpose |
| --- | --- |
| `thurm remote list` | Hosts and their connection state |
| `thurm remote status [NAME]` | State, remote build, last error, retry countdown |
| `thurm remote install NAME` | Install this build there, and upgrade its daemon in place |
| `thurm remote doctor [NAME]` | Check what the host needs; `--fix` offers each fix, `--yes` runs them |
| `thurm remote remove NAME` | Forget a host; its daemon and panes keep running there |

**Thurm › Remotes…** lists the same hosts with their state and build, and the
selected host's checklist with a **Fix** button for each problem.

## Check a host

`thurm remote doctor devbox`, and the checklist in **Thurm › Remotes…**, check:

| Check | Fix |
| --- | --- |
| Thurm | Installs this build (see [Supported hosts](#supported-hosts)) |
| Daemon | Replaces a daemon of another build in place |
| Lingering | `loginctl enable-linger`, or in a tab on the host with `sudo` when that needs your password |
| Command-line tool | Links `thurm` into `~/.local/bin`, so `ssh devbox thurm …` finds it |
| Claude Code | Runs the official installer (`https://claude.ai/install.sh`), after asking |
| Claude Code sign-in | Opens a tab running `claude` on the host, to sign in (Linux hosts) |
| Claude Code / Codex hooks | Installs them for exact agent status |

A fix that needs you at the host (a password, a sign-in) runs over `ssh -t`:
in a tab of this Mac from the app, in your terminal from `thurm remote doctor
--fix`.

## Supported hosts

| Host | What gets installed |
| --- | --- |
| macOS, Apple silicon | The app's own `thurm` and `thurmd`, copied over |
| Linux x86_64 | The static `x86_64-unknown-linux-musl` build of the app's exact version |
| Linux aarch64 | The static `aarch64-unknown-linux-musl` build of the app's exact version |
| Hosts with Nix | `nix profile install` of the flake's `thurm` at the app's commit |

Linux archives are downloaded from the release the app came from and checked
against the SHA-256 built into the app. Files go to
`~/.local/share/thurm/bin`, and `thurm` is linked into `~/.local/bin` when that
directory exists. NixOS users can instead add the flake's `packages.thurm` to
their configuration.

Thurm never installs anything in the background. A host without Thurm, or with
another version, shows **not installed** or **upgrade needed** until you
confirm the install.

## Host requirements

- **An ssh key that works without prompting.** Thurm runs ssh with
  `BatchMode=yes`: a key in an agent (the 1Password SSH agent works, with its
  own approval prompt), no passphrase prompts, no password or 2FA prompts.
  Connect once with `ssh devbox` in a terminal to accept the host key.
- **Unix socket forwarding** in the host's `sshd` (`AllowStreamLocalForwarding
  yes`, the OpenSSH default).
- **Lingering on systemd hosts.** The daemon's socket lives in
  `$XDG_RUNTIME_DIR`, which logind deletes when your last session ends. The
  doctor's **Lingering** fix turns it on (`loginctl enable-linger $USER`).
- **Agent hooks on the host**, for exact agent status (Working, Needs input,
  Done). The host's daemon adds them when it launches an agent without them,
  and the doctor installs them for agents that are already there.

Your `~/.ssh/config` applies as usual: Host aliases, `ProxyJump`, and the
1Password `IdentityAgent`. Thurm refuses a host whose ssh config adds
`RemoteForward` or `DynamicForward` (see [Security](#security)).

## Tailscale

Tailscale is only the network. Either works:

- **Tailscale SSH**, version 1.98 or later. Earlier versions cannot forward a
  Unix socket, and the host shows **needs attention** with "unsupported channel
  type". Tailscale SSH's `check` action signs you in through a browser, which a
  background connection cannot do: use an `accept` rule for your own devices.
- **A regular `sshd`** reached over the tailnet.

To let the Mac reach the devbox but not the reverse, use a grant like this in
your tailnet policy (tags are examples):

```json
{
  "grants": [
    { "src": ["tag:mac"], "dst": ["tag:devbox"], "ip": ["tcp:22"] }
  ]
}
```

With no grant from `tag:devbox` to `tag:mac`, nothing on the devbox can open a
connection to your Mac.

## What runs where

| Setting | Comes from |
| --- | --- |
| Font, theme, appearance, key bindings, notifications, rendering | This Mac's config |
| Shell, `env`, `term`, `shell_integration`, agent presets, `[[agents.define]]` | The host's own `~/.config/thurm/config.toml` |
| OSC 52 reads, link opening, ssh forwarding | This Mac, enforced in the app; the host's config can only be stricter |

Launch presets in a remote workspace (⌘⇧P, ⌘⇧A) are the host's.

## Using remote panes

- **Workspaces.** A host's panes open in a workspace named after it. ⌘⇧O
  lists **New Workspace on devbox**. New tabs and splits in a remote workspace
  run on that host. A pane started on the host from elsewhere (`thurm --remote
  devbox launch`) goes to its workspace in the background; the agent sidebar
  and ⌘⇧A switch to it.
- **Agents.** Sidebar and switcher rows show the host: `devbox · repo · Needs
  input`. Notifications follow the same focus rules as local panes. A status
  that changed while the host was unreachable shows when it is back; missed
  notifications are not replayed.
- **Offline.** An unreachable host keeps its tabs. They show the last frame
  under **Disconnected — reconnecting…** with the retry countdown, and input is
  blocked. Thurm retries with backoff up to two minutes, and right away after
  sleep or a network change.
- **Restore.** After the app restarts, remote tabs come back from the saved
  layout and attach to the host's still-running panes. Tabs of panes that ended
  meanwhile close with a notice; panes the host has that the layout doesn't know
  open as new tabs.
- **Pasting images.** An image pasted or dropped into a remote pane is written
  to a private file on the host (in the daemon's runtime directory), and its
  path is pasted.

## The CLI

`--remote NAME` sends any pane command to the host's daemon through the tunnel
the app keeps open:

```sh
thurm --remote devbox agents
thurm --remote devbox capture --pane 3 -n 40
thurm --remote devbox wait --pane 3 --agent-done --timeout 600
thurm --remote devbox launch claude
```

Pane IDs are the host's, so pass `--pane` (the current pane is a pane of this
Mac). When the app is not connected to the host, the command fails with, for
example, `devbox is not connected in Thurm (state: reconnecting)`. The CLI
never opens an ssh connection of its own for this.

## Hand off a repository

Give a local repository to an agent on a remote host. Your code goes over
through git; nothing else from the Mac does.

From a tab in the repository, press ⌘⇧P and choose **Hand Off to devbox…**,
then an agent preset (or a shell). From a terminal:

```sh
thurm handoff --remote devbox --preset claude
```

Thurm then:

1. Takes a snapshot: `HEAD`, or with uncommitted changes (untracked files
   included, ignored files not), a WIP commit on top of it. Your working tree,
   index, and stash stay as they are.
2. Pushes it over ssh to a bare mirror of the repository on the host
   (`~/.local/share/thurm/mirrors`), as `agent/<name>`.
3. Checks it out in its own worktree (`~/.local/share/thurm/worktrees`).
4. Adds a git remote `thurm-devbox` to your local repository.
5. Opens a tab on the host in that worktree, running the preset. The tab is
   listed under your local repository in the sidebar. Type the task yourself.

Several handoffs of one repository can run at once, each on its own branch.

**Getting the work back.** The agent commits. When it reaches Done or Needs
input, Thurm fetches `agent/<name>` into
`refs/remotes/thurm-devbox/agent/<name>`. **Fetch Handoff Result** (⌘⇧P) and
`thurm handoff --fetch ID` fetch on demand. Thurm never merges, checks out, or
rebases anything in your repository; review and merge the branch yourself, and
push from the Mac. The remote host cannot push anywhere: it has no access to
your keys.

Gitignored files (`.env`, `node_modules`, local config) are not copied. Set up
what the agent needs on the host.

**Cleaning up.** Closing a handoff tab offers to remove its worktree. Thurm
fetches first; if the worktree has uncommitted changes or commits not fetched
yet, it asks before deleting them. The branch on the host is deleted when your
default branch already contains it, and kept otherwise. Your local
remote-tracking ref stays. A tab closed while the host is offline is cleaned up
when it is back. `thurm handoff --list` and `thurm handoff --cleanup ID` do the
same from a terminal.

## Security

Remote hosts are assumed to be yours and single-user. Even so, they cannot
reach the Mac's disk, clipboard, or credentials without you doing something:

- **Clipboard reads (OSC 52).** Every read from a remote pane asks: "devbox
  wants to read your clipboard — Allow Once / Always for devbox / Deny".
  "Always" writes `clipboard_read = "always"` to the host's `[[remote]]`. Your
  `terminal.osc52` setting caps every host.
- **Clipboard writes** follow your `terminal.osc52` setting.
- **Links.** ⌘-click opens `http` and `https` links from remote panes, asks for
  other schemes, and never opens `file://` links (they are paths on the host):
  it offers to copy the path instead.
- **ssh.** Agent forwarding, X11 forwarding, and remote and dynamic forwards
  are always off. The only forward is the local socket for the daemon.
- **Files.** Only what you paste or drop goes to the host.

## Troubleshooting

| State | What to do |
| --- | --- |
| needs attention | ssh needs you: read the message (host key, authentication, forwarding). Fix it, then **Retry** |
| not installed | **Fix** next to Thurm in Thurm › Remotes…, or `thurm remote install NAME` |
| upgrade needed | The host runs another protocol version. Install this build; its daemon is replaced in place and its panes keep running (daemons too old for that are restarted after you confirm) |
| reconnecting | The host is unreachable; Thurm retries on its own |

The tunnel's local end is `~/Library/Caches/Thurm/remote/NAME.sock`.
