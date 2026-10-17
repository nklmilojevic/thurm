---
title: Configuration
---

Thurm uses TOML. All settings are optional. Unset settings use their defaults.
Find the active file with:

```sh
thurm config-path
```

The default is `~/.config/thurm/config.toml`. `THURM_CONFIG_DIR` overrides the
configuration directory. Otherwise, `XDG_CONFIG_HOME` changes the base directory
to `$XDG_CONFIG_HOME/thurm`. Set these variables before you start the app and CLI.

Create the parent directory if needed. Edit the file, then run `thurm reload`
or press Cmd+Shift+, (Linux: Ctrl+Shift+R). Cmd+, (Linux: Ctrl+,) opens the
configuration file.

## Example

```toml
[font]
size = 14.0
ligatures = true

[window]
tab_style = "native"
option_as_alt = "left"

[colors]
theme = "light:catppuccin-latte,dark:catppuccin-mocha"

[session]
quit = "detach"
```

Use the [complete reference](/configuration-reference/) for defaults, value
ranges, and optional settings. Copy only the sections you need. Do not repeat
a TOML section header when you add more settings to that section.

## Change settings from the CLI

```sh
thurm set font.size 14.0
thurm set font.ligatures true
thurm set window.tab_style native
thurm theme
thurm theme nord
```

`set` writes the setting and reloads the configuration. Strings can be bare.
Other values use TOML syntax. Quote arrays to protect them from your shell:

```sh
thurm set font.features '["ss01", "zero"]'
```

`thurm theme` lists available themes. A theme can also be a file path.
The `light:NAME,dark:NAME` form follows the system appearance. Explicit color
settings override theme colors.

## Settings groups

| Section | Controls |
| --- | --- |
| `font` | Family, size, ligatures, spacing, fallback fonts, and symbols |
| `window` | Padding, opacity, blur, initial size, tabs, and pane appearance |
| `cursor` | Shape, blink, and thickness |
| `colors` | Theme and individual colors |
| `terminal` | Shell, scrollback, protocols, selection, and shell integration |
| `terminal.env` | Environment variables for shell processes |
| `session` | Saved sessions, quit behavior, and agent resume |
| `security` | Secure input and paste confirmation |
| `notifications` | Agent, command, program, and bell notifications |
| `agents` | Detection, launch presets, and custom definitions |
| `ai` | Optional on-device features |
| `updates` | Update channel, background checks, and automatic downloads |
| `keybindings` | Key combinations mapped to action names |
| `commands` | Your own commands, in the command palette and on keys |
| `on` | Commands the daemon runs when something happens in a pane |

Shell and environment changes apply to new processes. They do not change the
environment of shells that are already running.

## Commands

A `[[commands]]` entry adds a command of your own to the command palette, and
optionally to a key. On macOS it is also in **Shell > Commands**.

```toml
[[commands]]
name = "Dev layout"
exec = ["~/bin/dev-layout", "--branch", "main"]
key = "cmd+shift+l"
```

The daemon runs `exec` through your login shell, so your `PATH` applies, in the
focused pane's directory. `THURM_PANE_ID` names that pane and `THURM_SOCKET` the
daemon, so the command can drive Thurm with the CLI:

```sh
#!/bin/sh
# ~/bin/dev-layout: an editor, a dev server beside it, and an agent below.
thurm split --dir right -- npm run dev
thurm split --dir down -- claude
```

`key` uses the same chord syntax as `[keybindings]`, and takes the combination
from any built-in action. A command that fails shows as a notification with the
end of its error output. Commands run on this machine's daemon; for a pane on a
remote host, they run without `THURM_PANE_ID`.

## Event hooks

An `[[on]]` entry runs a command when something happens in a pane, even while
no Thurm window is open:

```toml
[[on]]
event = "agent_finished"
exec = ["~/bin/agent-done"]

[[on]]
event = "command_finished"
exec = ["~/bin/log-command"]
min_secs = 60
```

| Event | When |
|---|---|
| `agent_working`, `agent_waiting`, `agent_blocked`, `agent_finished`, `agent_failed`, `agent_idle` | An agent enters that state, as in `[notifications]` |
| `command_finished` | A command at the shell prompt finishes (needs shell integration). `min_secs` skips shorter ones |
| `pane_exited` | A pane's process exits |

The command gets the event as one line of JSON on standard input: `event`, the
`pane` as `thurm info --json` shows it, and for `command_finished` the
`exit_code`, `duration_secs` and `command`, for `pane_exited` the `exit_code`.
`THURM_EVENT`, `THURM_PANE_ID` and `THURM_SOCKET` are set too.

```sh
#!/bin/sh
# ~/bin/agent-done: tell a webhook which repository's agent finished.
jq -c '{text: "\(.pane.agent.name) finished in \(.pane.cwd)"}' |
  curl -s -H 'Content-Type: application/json' -d @- "$WEBHOOK_URL"
```

An entry with an unknown event or no `exec` is skipped with a warning, and the
rest of the file still loads. Failures are written to the daemon's log.

## On-device AI

Set `ai.enabled = true` to enable the optional Apple Intelligence model.
This needs macOS 26 or later, Apple silicon, and Apple Intelligence enabled in
System Settings. Model processing stays on the Mac. Thurm skips model use while
a password prompt is active.

The `titles`, `status`, `notifications`, and `explain` settings let you disable
individual features. Use **Edit > Explain Last Command** or `thurm explain` to
explain the last finished command. Agent status inference applies to agents
without hooks.

## Updates

The `[updates]` settings are for the macOS app. On Linux, install a newer package
(`.deb`, `.rpm` or the Arch package) to update.

Release builds use these defaults:

```toml
[updates]
channel = "auto"
check_automatically = true
download_automatically = false
```

`auto` follows the installed build: a Tip build receives Tip updates, and a
Release build receives tagged releases. Set `channel` to `release` or `tip`
to keep a fixed channel. Tip receives only Tip builds; each release reaches it
as the next Tip build.

**Thurm > Update Channel** writes the same setting as the CLI:

```sh
thurm set updates.channel tip
thurm set updates.check_automatically true
thurm set updates.download_automatically true
```

Thurm checks for updates about once an hour. A background check does not
open a window. When it finds an update, an **Update available** badge appears
in the bottom-right corner of each window. Click the badge to read the release
notes and install. **Thurm > Check for Updates…** shows the result right away.

Automatic downloads install updates when Thurm quits. Use
`thurm set updates.channel auto` to follow the installed build again.
These settings require a build with an update feed and public signing key.
They do not enable updates for a local build that has neither.
