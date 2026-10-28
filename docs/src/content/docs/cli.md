---
title: Command line
---

Install the CLI as described in [Install](/install/). Keep the app open for
commands that create or focus tabs and splits.

```sh
thurm --help
thurm split --help
thurm list
thurm --json list
```

Commands that target the current pane use `THURM_PANE_ID`, which Thurm sets in
pane processes. Outside Thurm, use an explicit pane ID from `thurm list`.
`procs` can list all panes; use `--pane` to restrict it.

## Open a pane and read its output

Run this from a Thurm pane:

```sh
pane=$(thurm split --dir right)
thurm send --pane "$pane" 'echo THURM_READY'
thurm wait --pane "$pane" --match '^THURM_READY$' --timeout 10
thurm capture --pane "$pane" -n 20
```

`send` adds Enter unless you pass `--no-enter`. It sends input to the process
in the pane, so first check that the intended shell or agent is active.
To send standard input as a bracketed paste:

```sh
printf '%s' 'Text for the active program' | thurm send --pane "$pane" --paste --no-enter -
```

## Commands

Use `thurm COMMAND --help` for the full argument list.

| Command | Purpose and main options |
| --- | --- |
| `list` (alias `ls`) | List panes |
| `pane-id` | Print the current pane ID |
| `info --pane ID` | Show pane details, including `alt_screen` (a full-screen program has the screen) and `focused` |
| `new-tab [COMMAND...]` | Open a tab; `--window` (in a new workspace, in the background), `--cwd PATH`, `--hold`, `--title NAME`, `--background` (keep the focus and the selected tab), `--wait` (exit with the command's status) |
| `split [COMMAND...]` | Split a pane; `--dir right\|down\|left\|up`, `--pane ID`, `--cwd PATH`, `--hold`, `--size PERCENT` (the new pane's share), `--background`, `--wait` |
| `focus ID` | Focus a pane in the app |
| `focused` | Print the pane with keyboard focus in the app, or the last one that had it; `--history` lists them, most recent first |
| `zoom` | Zoom a pane to fill its tab; `--pane ID`, `--on`, `--off` (default: toggle) |
| `wall` | Gather the agents of the window's tabs into one grid tab, or put them back in their tabs; `--pane ID`, `--on`, `--off` (default: toggle) |
| `float` | Float a pane above its tab's splits as a card you can move and resize, or dock it back; `--pane ID`, `--on`, `--off` (default: toggle) |
| `dock` | Dock a floating pane back into its tab's splits; `--pane ID` |
| `new-float [COMMAND...]` | Run a command (default: shell) in a new pane floating above a tab's splits; `--pane ID` (a pane of the tab), `--cwd PATH`, `--hold`, `--background`, `--wait` |
| `move --to PANE` | Move a pane next to another, in any tab of the window; `--pane ID`, `--dir right\|down\|left\|up`, `--size PERCENT` |
| `swap --with PANE` | Exchange the places of two panes, in one tab or across tabs; `--pane ID` |
| `close [ID]` | Stop and close a pane; ID is positional |
| `title [TEXT...]` | Set the tab title; `--pane ID`; empty text resets it |
| `workspace rename NAME...` | Rename the workspace a pane is in; `--pane ID` (`new-tab --window` prints the new pane's ID) |
| `send [TEXT...]` | Send text; `--pane ID`, `--paste`, `--no-enter`; `-` reads stdin |
| `send-keys [KEYS...]` | Send keys such as `Enter`, `Escape`, `C-c`; `--pane ID` |
| `capture` | Read visible text; `--pane ID`, `-n N`, `--scrollback`, `--ansi` |
| `wait` | Wait for a condition; see below |
| `scroll up\|down\|top\|bottom\|prev-prompt\|next-prompt` | Move the viewport; `--pane ID` |
| `clear` | Clear scrollback; `--pane ID` |
| `complete` | Show completion choices for prompt input; `--pane ID` |
| `completion SHELL` | Print the shell completion script for `thurm` itself: `bash`, `elvish`, `fish`, `powershell` or `zsh`, e.g. `thurm completion fish \| source`. The Nix package installs the bash, fish and zsh scripts; on macOS, the Home Manager module sets up fish for the app's CLI |
| `procs` | List processes and ports; `--pane ID`, `--ports` |
| `notify TITLE [BODY...]` | Send a desktop notification through the current terminal |
| `presets` | List agent launch presets |
| `launch PRESET` | Launch a preset; `--split DIRECTION`, `--cwd PATH` |
| `agents` | List agent panes and status |
| `agent prompt TEXT...` | Submit a prompt to the current agent; `--pane ID`, `--wait`, `--timeout SECONDS`; see [Agent prompts](/agent-prompts/) |
| `agent explain` | Show the process, status source, and detection evidence; `--pane ID` |
| `agent report` / `agent release` | Report agent state and session details; see [Agent reporting](/agent-integration/) |
| `attach --pane ID` | Open an interactive terminal connection; see [Terminal attachment](/attach/) |
| `events` | Stream state changes as JSON Lines; `--pane ID`, `--filter TYPE,...`; see [Layouts and events](/workflows/) |
| `fork` | Fork an agent session; `--pane ID`, `--split DIRECTION` |
| `hooks install\|uninstall\|status` | Manage hooks; `--agent claude\|codex` |
| `skill show\|setup\|remove\|status` | Print the agent guide, or install the skill that points agents at it; `--agent claude\|agents` |
| `explain` | Explain the last finished command; `--pane ID`; needs enabled AI |
| `config-path` | Print the configuration path |
| `reload` | Reload configuration |
| `set KEY VALUE` | Write a setting and reload |
| `theme [SPEC]` | List themes, or select a theme |
| `save` | Write a session snapshot |
| `layout [export\|apply]` | Print layout JSON, export a tab template, or apply a template; see [Layouts and events](/workflows/) |
| `daemon ACTION` | Control the daemon; see [Sessions](/sessions/) |
| `socket-path` | Print the daemon socket path |
| `tailscale status\|on\|off\|login\|logout\|rename` | Serve the daemon over Tailscale; see [Serve over Tailscale](/remote/#serve-over-tailscale) |
| `tailscale peers\|nc HOST PORT` | Tailnet devices, and a connection to one through Thurm's own device (an ssh `ProxyCommand`); see [Reach a host over Tailscale](/remote/#reach-a-host-over-tailscale) |
| `pair [--name NAME] [--link]` | A QR code and link that pair the phone with this host directly over Tailscale, or over the plain addresses of a tailnet without HTTPS (see [Headscale](/remote/#headscale)); see [Serve over Tailscale](/remote/#serve-over-tailscale) |
| `whoami` | Show who the daemon sees on this connection |
| `remote add\|list\|remove\|status\|install` | Manage remote hosts; see [Remote workspaces](/remote/) |
| `remote doctor [NAME]` | Check what a host needs and offer fixes; `--fix`, `--yes` |
| `handoff` | Hand the repository to a remote agent; `--remote NAME`, `--preset`, `--branch agent/NAME`, `--list`, `--fetch ID`, `--cleanup ID` |

`--hold` keeps a pane open after its command exits. Use `--` before a command
when you need to separate its flags from Thurm flags:

```sh
thurm new-tab --hold -- /bin/sh -c 'printf "Task complete\n"'
```

## Wait conditions and exit codes

Choose one condition and use `--timeout SECONDS` to bound a script's wait.

| Condition | Meaning |
| --- | --- |
| `--idle MS` | No output for this many milliseconds |
| `--prompt` | The shell is at a prompt; needs shell integration |
| `--exit` | The pane process exited |
| `--match REGEX` | A regular expression matches the visible screen |
| `--agent-free` | The agent is idle or waits for input; alias `--until-free` |
| `--agent-done` | The agent finished its turn; needs hooks |
| `--agent-waiting` | The agent waits for input |

Output silence does not prove that a task succeeded. Check the output and use
a condition that matches the task. A screen match checks the visible screen,
not all scrollback.

| Exit code | Meaning |
| --- | --- |
| `0` | Command succeeded or wait condition was met |
| `1` | Operation failed |
| `2` | Invalid CLI arguments |
| `124` | Wait timed out |

`--remote NAME` is global: it sends the command to that host's daemon through
the tunnel the app keeps open, and fails when the app is not connected to it.
Pass `--pane` with it. See [Remote workspaces](/remote/#the-cli).

`--json` is global. Use it for structured results where the command returns
data. A command that only performs an action can return no output.
