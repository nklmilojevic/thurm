---
title: Sessions and the daemon
sidebar:
  label: Sessions
---

The app is a client of `thurmd`. The daemon owns shell processes, terminal state,
and the saved window, tab, and split layout.

## Quit, close, and restart

| Action | Result |
| --- | --- |
| Quit with `session.quit = "detach"` (default) | Shells continue to run in the daemon |
| Open the app again | The app connects to the existing session |
| Close a pane | Its process stops |
| Quit with `session.quit = "terminate"` | Shells stop |
| Reboot or restart the daemon | Saved layout and scrollback can return; new processes start |

A disk snapshot cannot restore the memory of a stopped process.

## Saved state

With `session.persist = true`, the daemon saves state under
`~/Library/Application Support/Thurm`. `THURM_STATE_DIR` overrides this directory.
The default snapshot interval is 15 seconds. Up to 4,000 scrollback lines per
pane are saved. This is separate from `terminal.scrollback`, which defaults to
10,000 lines in memory.

```sh
thurm save
thurm layout
thurm daemon status
```

`save` writes a snapshot now. `layout` prints the saved layout as JSON.
Set `session.persist = false` to disable persistence.

When `session.resume_agents = true`, restored panes can run configured agent
resume commands. This starts an agent process again; it does not recover its
previous process memory. Hooks can provide session IDs for a specific resume.

## Daemon controls

```sh
thurm daemon start
thurm daemon upgrade
thurm daemon install-launchd     # macOS
thurm daemon install-systemd     # Linux
```

`install-launchd` installs a per-user LaunchAgent to start the daemon at login.
Use `thurm daemon uninstall-launchd` to remove it. On Linux, `install-systemd`
writes and enables a systemd user unit (`~/.config/systemd/user/thurmd.service`)
instead; `thurm daemon uninstall-systemd` removes it.

`upgrade` replaces an older daemon with the installed binary. Compatible
versions transfer terminal state and open process handles so that programs
continue to run. Older daemons without this protocol need a restart, which
cannot preserve running processes.

`thurm daemon stop` stops the daemon and its running session. Save your work
before you use it. Do not use it as the first step to fix a display problem.

The default socket is `/tmp/thurm-UID/thurmd.sock`. `XDG_RUNTIME_DIR` replaces
`/tmp`; `THURM_SOCKET` overrides the whole socket path. The app and CLI must use
the same path to control the same daemon.
