# Remote workspaces

Attach Thurm.app to `thurmd` running on other machines (a Linux devbox, another Mac), show
remote agents next to local ones, and hand a local repo to a remote agent through git.

Status: built (all of v1). Code: `crates/thurm-remote` (ssh, tunnel, install, handoff, policy),
`thurm remote` / `--remote` / `thurm handoff` in `crates/thurm-cli`, the FFI in
`crates/thurm-ffi/src/remote.rs`, and `macos/Sources/Thurm/Remote.swift`. User docs:
`docs/src/content/docs/remote.md`.

## Goals

- A remote host's panes appear as a workspace in the Mac app, with the same tabs, splits,
  agent sidebar, Cmd+Shift+A switcher and macOS notifications as local panes.
- Agents keep running on the remote host when the Mac sleeps, moves networks or quits.
- "Hand off" a local repo to a remote agent: the code goes over through git, the agent works
  in its own worktree, committed results come back as a local remote-tracking branch.
- Remote hosts cannot read the Mac's disk, clipboard or credentials without the user's
  explicit action.

## Non-goals (v1)

- Thurm's own network layer, relay, or mesh. The transport is the system `ssh`; Tailscale (or
  anything else) is only the network underneath.
- Pushing to GitHub or opening PRs from the remote host. Code comes back to the Mac and the
  user pushes from there.
- Copying gitignored files (`.env`, `node_modules`, local config) into handoff worktrees.
- Interactive SSH authentication (passphrases, unknown host keys, 2FA, Tailscale SSH checks).
- Shared/multi-user remote hosts.
- Injecting a task prompt or instructions into a handoff agent.
- Tailscale peer discovery (v2).
- A protocol compatibility window (v2; see Versions).

## Supported hosts

| Host | Artifact |
| --- | --- |
| macOS, Apple silicon | The app's own `thurm`/`thurmd` (copied over as-is) |
| Linux x86_64 | `x86_64-unknown-linux-musl`, static |
| Linux aarch64 | `aarch64-unknown-linux-musl`, static |
| NixOS / nix hosts | `thurm` flake package output |

The daemon already has `target_os = "linux"` branches (`procinfo.rs`, `peer_is_same_user`).
The Linux build links libghostty-vt through `thurm-term`, so the release job has to
cross-build it with Zig for both musl targets.

## Architecture

```
 Mac                                              remote host
┌────────────────────────────┐  ssh (Thurm-owned)  ┌─────────────────────────────────┐
│ Thurm.app                  │ -L <local.sock>:    │ thurmd  (PTYs, agent state,     │
│  Core: one client per host │   <remote.sock>     │          its own config.toml)   │
│   local  → thurmd.sock     │────────────────────▶│   ▲                             │
│   devbox → remote-devbox.sock                    │   │ AgentHook                   │
│  policy enforced here      │                     │ thurm agent-hook ◀─ claude/codex│
└────────────────────────────┘                     │   (THURM_SOCKET, THURM_PANE_ID) │
       ▲                                           └─────────────────────────────────┘
       │ thurm --remote devbox … (CLI via the app's tunnel)
```

- Agent hooks never cross the network. On the remote host, `thurm agent-hook` talks to the
  remote `thurmd` through `THURM_SOCKET`. The Mac only sees `PaneInfo` events.
- The remote daemon's `peer_is_same_user` check is unchanged: the peer is the user's own
  `sshd` session.
- The Mac never reads paths named by the remote side. Kitty file/shared-memory image
  transmissions are already read by the daemon (`MediaReader`) and forwarded inline.

### Phase 0: one app, many daemons (prerequisite)

Today the app assumes a single daemon:

- `Core.shared` is a singleton holding one `thurm_client` from `thurm_connect`.
- Pane IDs are bare `UInt64` throughout `Models.swift`, `LayoutNode`, `SessionManager`, the
  sidebar and notifications.
- The window layout is saved in the local daemon (`SetLayout`).

Changes:

- `Core` holds a map `HostId → client`. `HostId` is `local` or a `[[remote]]` name.
- Pane identity becomes `(HostId, PaneId)` everywhere in the app, including `LayoutNode`,
  notification user info and the agent switcher. Remote pane IDs are never passed to the
  local daemon, and the reverse.
- The saved layout (still stored by the local daemon) tags each pane leaf with its host, so
  remote tabs restore after an app restart (see Restore).
- The FFI already returns a handle per connection. `handle_event` / frame callbacks carry
  the host so Swift routes them to the right model.

This phase ships with no user-visible change and is tested on local panes only.

## Configuration

### Source of truth

```toml
[[remote]]
name = "devbox"          # HostId, shown in the sidebar
host = "devbox"          # ssh target: a Host alias, user@host, or ssh://user@host:port
# socket = "/run/user/1000/thurm-1000/thurmd.sock"   # optional; discovered otherwise
# enabled = true
```

- `thurm remote add <name> <ssh-target>` checks connectivity, offers the install/upgrade,
  and writes the `[[remote]]` entry. `thurm remote list | remove | status` round it out.
- Settings gets a read-only Remotes list: per host state (connected / reconnecting / needs
  attention), remote build, and an Install/Upgrade button.

### Which config applies to a remote pane

| Concern | Source |
| --- | --- |
| Font, theme, appearance, keybindings, notifications, rendering | Mac config, pushed as today (`SetTheme`, `SetAppearance`, `SetSetting`) |
| Shell, env, `term`, `shell_integration`, agent presets, `[[agents.define]]` | The remote host's own `config.toml` |
| OSC 52 reads, link opening, SSH forwarding options | Mac, enforced in the app; a remote config can only be stricter |

The Mac's shell and preset paths often don't exist on Linux, so the remote's own config
decides what runs there.

## Connection management

- Thurm spawns the system `ssh`, one ControlMaster-free process per host:

  ```
  ssh -N -F <generated> \
      -o BatchMode=yes -o ExitOnForwardFailure=yes -o StreamLocalBindUnlink=yes \
      -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
      -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes \
      -L <local.sock>:<remote.sock> <host>
  ```

  The generated config `Include`s `~/.ssh/config` first, so user Host blocks, `ProxyJump` and
  the 1Password `IdentityAgent` still apply. Explicit `-o` options win over them for the
  security-relevant settings. `ClearAllForwardings` does remove the `-L` given on the same
  command line (checked with OpenSSH 10.5), so the tunnel doesn't use it; instead `ssh -G`
  runs first and a host whose config adds `RemoteForward`/`DynamicForward` goes to needs
  attention. Command connections (probe, install, git) do use `ClearAllForwardings=yes`.
- `BatchMode=yes` always. The 1Password SSH agent's own biometric prompt still works,
  because it is not a TTY prompt. Anything that needs a TTY fails fast, and the host goes to
  **needs attention** with ssh's stderr shown ("add the host key with `ssh devbox` once",
  and so on).
- Local socket: `~/Library/Caches/Thurm/remote/<name>.sock` in a 0700 directory.
- Remote socket: `thurm socket-path` (new command) run over ssh, unless `socket` is set.
- Before forwarding: `ssh <host> thurmd --daemonize --socket <path>` (the client's existing
  spawn path) if nothing is listening.
- Reconnect: automatic with exponential backoff, capped at 2 minutes. The backoff resets
  after 60 s of healthy connection. Triggers: ssh exit, `Disconnected` event, wake from
  sleep, network change (retry immediately).
- Tailscale SSH supports streamlocal forwarding from 1.98.0 (tailscale/tailscale#19006,
  April 2026); older versions fail with "unsupported channel type", which is reported with a
  hint to use a normal `sshd` over the tailnet.

### Remote host requirements (documented)

- `loginctl enable-linger $USER` on systemd hosts. `socket_path()` uses `$XDG_RUNTIME_DIR`,
  which logind removes when the last session ends.
- Claude Code / Codex hooks installed on the remote: `thurm hooks install --agent claude`.
  `thurm remote add` offers to run this over ssh.

## Install and upgrade

- On `thurm remote add`, or a protocol/build mismatch on connect, ask the user first, then:
  - Same platform (Mac to Mac): copy the app's `thurm` and `thurmd`.
  - Linux: download the matching musl artifact for the app's exact build from the release
    channel the app is on (tip/release). Verify its checksum.
  - Nix host (the remote has `nix`): offer `nix profile install` of the flake output at the
    matching revision instead.
- Install to `~/.local/share/thurm/bin` on the remote, and link `thurm` into `~/.local/bin`
  when that directory exists (same rule as the local install).
- Upgrade uses the existing hot-upgrade path (`upgrade_daemon`, SIGUSR2) run on the remote
  over ssh, so running panes survive. If the remote daemon is older than
  `HOT_UPGRADE_PROTOCOL`, say that panes will be stopped and ask.
- Background reconnects never install. They only report "upgrade needed".

## Versions

- Exact `PROTOCOL_VERSION` match stays. A Sparkle update of the app that bumps the protocol
  puts every remote in "upgrade needed" until the user confirms the hot-upgrade.
- To keep a later compatibility window possible, `Hello` should carry a capability list now,
  even while exact match is enforced.

## Remote agents

- In scope: Claude Code and Codex with hooks (Working / Idle / NeedsInput / Done, session id,
  title). Other agents use screen detection as they do locally.
- Sidebar and switcher entries show the host: `devbox · repo · Needs input`.
- Notifications fire on the Mac with the same focus rules as local panes. A status that
  changed while disconnected shows on reconnect (NeedsInput badges immediately). Only the
  current state is replayed on reconnect; missed notifications are not queued.
- Launch presets in a remote workspace come from the remote's config (`ListAgentPresets`
  goes to that host's daemon).

## Handoff: local repo → remote agent

Entry: command palette "Hand off to…" from a local repo tab, or
`thurm handoff --remote devbox [--preset claude] [--branch agent/<slug>]`.

The handoff only prepares the worktree and opens a remote tab running the preset. No task
prompt or instructions are injected; the user types the task.

1. **Snapshot.** Committed HEAD. If the tree is dirty, `git stash create` plus untracked
   files are made into a WIP commit on top of HEAD without touching the local tree, index or
   stash list.
2. **Mirror.** A bare mirror per repo per host at
   `~/.local/share/thurm/mirrors/<repo-id>.git`. `repo-id` comes from the origin URL, or
   from the root commit hash when there is no origin.
3. **Push.** `git push` over the same ssh target to
   `refs/heads/agent/<slug>`. `slug` is auto-generated and unique per repo per host.
4. **Worktree.** `git -C <mirror> worktree add ~/.local/share/thurm/worktrees/<repo-id>/<slug> agent/<slug>`.
5. **Local remote.** Add (once) a git remote on the local repo named `thurm-<host>` pointing at
   the mirror over ssh.
6. **Launch.** Open a tab in the remote workspace with `cwd` set to the worktree and the chosen
   preset. The tab is grouped under the local repo in the sidebar and tagged as a handoff.

Several handoffs of one repo can run at once, each in its own worktree and branch.

### Return path

- Committed work only. The agent is responsible for committing; Thurm does not auto-commit
  on the remote.
- Auto-fetch `git fetch thurm-<host> agent/<slug>` whenever the handoff pane's agent reaches
  `Done` or `NeedsInput`. Also available as a "Fetch result" command.
- The local ref lives at `refs/remotes/thurm-<host>/agent/<slug>`. Thurm never merges, checks
  out or rebases locally.
- A failed fetch (offline, auth) is shown on the tab and retried on the next trigger.

### Cleanup

Closing a handoff tab offers to remove it:

1. Final fetch.
2. If the remote worktree has uncommitted changes, or commits not yet fetched, warn and
   require confirmation.
3. `git worktree remove` on the remote, and delete `agent/<slug>` from the mirror if it is
   merged into the local repo's default branch. Otherwise keep the branch and say so.
4. The local remote-tracking ref is left in place.

## Security policy for remote panes (enforced in the app)

- **OSC 52 reads:** every read from a remote pane prompts: "devbox wants to read your
  clipboard — Allow once / Always for devbox / Deny". "Always" is stored in the Mac config
  per host. This replaces today's unconditional reply in `SessionManager.swift`
  (`ClipboardRequest`) for remote hosts. Local behavior is unchanged.
- **OSC 52 writes:** follow the Mac's `osc52` setting.
- **Links:** Cmd-click on a remote pane opens http(s) links directly and asks for other
  schemes. `file://` links are remote paths, so they are never opened on the Mac; "Copy path"
  is offered instead.
- **SSH:** agent forwarding, X11 forwarding, reverse and dynamic forwards are always off. The
  remote agent has no access to the Mac's SSH keys, so it cannot push to GitHub (by design).
- **Image paste:** the Mac client writes the image to a remote temp file (0600, under the
  remote's runtime dir) through the daemon and pastes the path, like herdr. Only what the
  user pastes crosses over.
- **Documented, not enforced:** a Tailscale ACL that lets the Mac reach the devbox but not
  the reverse. Remote hosts are assumed single-user and owned by the user.

## Offline and restore

- An unreachable host keeps its tabs. They show the last frame with a "Disconnected —
  reconnecting…" overlay, input is blocked, and the retry countdown is visible.
- After an app restart, remote tabs come back from the saved layout (host-tagged leaves),
  start as reconnecting, and attach to the remote daemon's still-running panes.
- If the remote daemon restarted and a pane is gone, its tab closes with a notice. Panes the
  remote has that the layout doesn't know about open as new tabs in that workspace.

## CLI

- `thurm --remote <name> <command>` routes any existing command (`agents`, `panes`,
  `capture`, `send`, `wait --agent-done`, `launch`…) to the host's daemon through the
  socket the app keeps forwarded.
- If the app isn't connected to that host: "devbox is not connected in Thurm (state:
  reconnecting)". The exit code is non-zero, and the CLI never opens its own ssh.
- New: `thurm remote add|list|remove|status`, `thurm socket-path`, `thurm handoff`.

## Testing

- **Loopback SSH integration tests** (Rust, CI on macOS and Linux): start `sshd` on
  localhost with a throwaway key and config, run `thurmd` behind it, and cover tunnel setup,
  kill-and-reconnect with backoff, protocol mismatch reporting, hot-upgrade across the
  tunnel, and BatchMode failure turning into needs-attention.
- **Linux CI job:** build both musl targets and run the daemon and term test suites on
  x86_64 and aarch64 runners on every PR.
- **Handoff git tests:** temp repos with a local path as the "remote" (no ssh). Cover the
  dirty-tree snapshot leaving the local tree byte-identical, mirror push, worktree create,
  parallel handoffs, fetch on status change, and cleanup with and without unfetched or
  uncommitted work.
- **Phase 0:** multi-daemon model tests with two local daemons on different sockets
  (pane-ID collisions, routing, layout restore).
- **Policy:** unit tests for OSC 52 read prompts and link decisions per host.

## Delivery order

All of it ships in v1. Build order:

1. Phase 0: multi-daemon `Core`, `(HostId, PaneId)` identity, host-tagged layout.
2. Linux musl builds (x86_64, aarch64) in CI and release; flake package.
3. `thurm socket-path`, `[[remote]]` config, ssh tunnel manager, reconnect, offline overlay.
4. Remote install/upgrade with confirmation; `thurm remote add|list|remove|status`.
5. Remote agents in sidebar, switcher and notifications; restore across app restarts.
6. App-side policy: OSC 52 read prompts, link handling, image paste via remote temp file.
7. `thurm --remote` CLI routing.
8. Handoff: snapshot, mirror, worktree, auto-fetch, cleanup; `thurm handoff`.
9. Settings Remotes list; docs (install, linger, hooks, Tailscale ACL).

## Decisions and tradeoffs

- **System ssh, no own transport.** Reuses the user's ssh config, keys and 1Password agent.
  The cost is no interactive auth in v1.
- **Exact protocol match.** Simple and matches today's code. The cost: every protocol bump
  needs a remote hot-upgrade. The `Hello` capability list keeps the door open.
- **Git mirror on the remote instead of cloning from origin.** Works for private repos and
  unpushed commits without credentials on the devbox. The cost: the first push of a large
  repo goes over the ssh link.
- **Committed-only return.** Keeps Thurm out of the agent's working tree. The cost: work the
  agent didn't commit is only visible in the remote tab.
- **No push from remote.** Keys never leave the Mac. The cost: PRs are opened from the Mac.
- **Config split by concern.** Remote execution matches the remote machine; security stays
  under the Mac's control.

## Open questions

Answered while building:

- `ClearAllForwardings=yes` drops a command-line `-L` too: not used for the tunnel (see
  Connection management).
- Tailscale SSH forwards Unix sockets from 1.98.0.
- A handoff tab closed while its host is offline is queued (`pending_cleanup` in the
  registry) and cleaned up on reconnect, without forcing: work that would be lost is kept and
  reported. `thurm handoff --cleanup ID` handles the rest by hand.

Still open:

- Latency: is raw forwarding good enough over WAN links, or is batching or local echo needed?
  Measure on a real devbox.
- `repo-id` for repos with no origin and a rewritten root commit (rare; the fallback may need
  a stored UUID). Today it is `<dir>-root-<first 12 of the root commit>`.
