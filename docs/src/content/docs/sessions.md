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
| Upgrade the daemon (`thurm daemon upgrade`) | Shells and scrollback continue |
| Reboot or restart the daemon | The saved layout returns with empty panes; new processes start |

A disk snapshot cannot restore the memory of a stopped process.

## Saved state

With `session.persist = true`, the daemon saves state under
`~/Library/Application Support/Thurm`. `THURM_STATE_DIR` overrides this directory.
The default snapshot interval is 15 seconds. A snapshot holds the layout and each
pane's working directory, command and agent session, never its scrollback:
terminal output often holds tokens and keys, so it stays in the daemon's memory
(`terminal.scrollback`, 10,000 lines by default). Older versions saved scrollback
in a `scrollback` folder there; the daemon deletes it when it starts.

An in-place upgrade passes every terminal to the new daemon in a file sealed with
AES-256-GCM. Its key never touches the disk: the new daemon reads it from a pipe
it inherits from the old one.

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

## Program resume (OSC 7577)

Thurm implements the [Program Resume Protocol](https://www.superlogical.com/rex/docs/build/program-resume)
(`OSC 7577`), which lets a running program tell the terminal how to resume it after a
daemon restart or reboot.

Each pane gets a unique random `OSC7577_TOKEN` in its initial environment (omitted when
running as `root`). Programs prove they belong to the pane by signing reports with a
SHA-256 `mac` over the report body and token, together with their owning `pid` and Unix
timestamp (`time`, verified within a 60-second window).

On restore, Thurm runs the reported command itself, without shell parsing: a bare program
name is looked up on the shell's `PATH`, and a file that is neither a binary nor a script
with a `#!` line is refused rather than run by `/bin/sh`. The restored shell gets only a
single-use ticket, which Thurm's helper redeems with the daemon for the command, so the
command is never written to a file. When a rule trusted the program by its executable,
only a program found in that executable's directory runs (relative `PATH` entries are not
searched), and a directory the command must start in is entered only while it is still the
one Thurm checked. How the command starts depends on where it ran:

- A program that was the pane's first process (no shell) starts the same way again,
  through a login shell command, without an interactive shell's startup files.
- A job of zsh, bash or fish with shell integration runs from the shell's own hook at its
  first prompt, after its startup files ran, and the shell takes over when it ends.
- In other POSIX shells, and in zsh or fish started without startup files (`zsh -f`,
  `fish --no-config`), Thurm types a fixed helper command (never the program's arguments)
  a few seconds after the shell started, but only while that shell waits in the
  foreground and nothing was typed into the pane; otherwise it skips the resume.

If `config.toml` fails to load when the daemon starts, program resume stays off until it
loads again. Thurm records each attempt before it starts the command and never retries a resume that
failed or crashed; if it cannot record the attempt, it starts plain shells instead. A pane
whose record is not resumed for another reason (not trusted, its directory is gone, or the
shell cannot run it) restores as it would without one: its command, or `resume_agents`.

By default (`session.resume_programs = "rules"`), Thurm only resumes commands that match
a trust rule and whose owning process is not wrapped inside a sandbox or another
non-shell program. Built-in rules (`session.resume_default_rules = true`) cover Claude
Code, Codex, OpenCode, Pi, and Antigravity. Add your own rules in `config.toml`:

```toml
[[session.resume_rules]]
name = "nvim"
command = ["nvim", "-S", "<under:~/.local/state/nvim/sessions>"]
dirs = ["~/code"]
```

A rule's `flags` lists options the report may add after `command`; each must have been
given, with the same values, when the program was launched. `preserve_flags` lists
restricting options: when the program was launched with one, the report must carry every
occurrence over unchanged and in the same order, or the program is not resumed. The Claude
Code rule keeps `--disallowed-tools`, `--tools`, `--permission-mode`, `--restricted`,
`--mcp-config` and `--strict-mcp-config`; the Codex rule keeps its sandbox and approval
settings (`--sandbox`/`-s`, `--ask-for-approval`/`-a`, `--config`/`-c`, `--profile`/`-p`).
A flag listed in both takes its `preserve_flags` arity.

Only those launch options are saved with the session, not the rest of the program's command
line, and only as keyed digests of their values (HMAC-SHA256), never the values themselves.
The key lives outside the state directory: in `~/Library/Application Support/Thurm Keys` on
macOS and `~/.local/share/thurm/keys` elsewhere (`THURM_KEY_DIR` overrides it). Without it,
programs are not saved for resumption. A saved program whose rule now checks an option the
session did not record is not resumed.

To disable a built-in rule, add an entry with its `name` and `enabled = false`. Set
`session.resume_programs = "all"` to trust any verified report from an unwrapped process,
or `"off"` to disable OSC 7577 resumption. Inspect held records and their trust status
with `thurm resume` (or `thurm resume --all --json`).

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
instead, and starts it when no daemon is running; `thurm daemon uninstall-systemd`
removes it. The unit is not restarted when it stops, so `thurm daemon stop` holds
until the next login. Lingering (`loginctl enable-linger`) keeps the daemon up
after you log out; `install-systemd` warns when it is off. See
[Linux server](/linux-server/).

`upgrade` replaces an older daemon with the installed binary. Compatible
versions transfer terminal state and open process handles so that programs
continue to run. Older daemons without this protocol need a restart, which
cannot preserve running processes.

`thurm daemon stop` stops the daemon and its running session. Save your work
before you use it. Do not use it as the first step to fix a display problem.

The default socket is `/tmp/thurm-UID/thurmd.sock`. `XDG_RUNTIME_DIR` replaces
`/tmp`; `THURM_SOCKET` overrides the whole socket path. The app and CLI must use
the same path to control the same daemon.
