---
title: Command line
---

Install the CLI as described in [Install](/thurm/install/). Keep the app open for
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
| `info --pane ID` | Show pane details |
| `new-tab [COMMAND...]` | Open a tab; `--window`, `--cwd PATH`, `--hold` |
| `split [COMMAND...]` | Split a pane; `--dir right\|down\|left\|up`, `--pane ID`, `--cwd PATH`, `--hold` |
| `focus ID` | Focus a pane in the app |
| `close [ID]` | Stop and close a pane; ID is positional |
| `title [TEXT...]` | Set the tab title; `--pane ID`; empty text resets it |
| `send [TEXT...]` | Send text; `--pane ID`, `--paste`, `--no-enter`; `-` reads stdin |
| `send-keys [KEYS...]` | Send keys such as `Enter`, `Escape`, `C-c`; `--pane ID` |
| `capture` | Read visible text; `--pane ID`, `-n N`, `--scrollback`, `--ansi` |
| `wait` | Wait for a condition; see below |
| `scroll up\|down\|top\|bottom\|prev-prompt\|next-prompt` | Move the viewport; `--pane ID` |
| `clear` | Clear scrollback; `--pane ID` |
| `complete` | Show completion choices for prompt input; `--pane ID` |
| `procs` | List processes and ports; `--pane ID`, `--ports` |
| `notify TITLE [BODY...]` | Send a desktop notification through the current terminal |
| `presets` | List agent launch presets |
| `launch PRESET` | Launch a preset; `--split DIRECTION`, `--cwd PATH` |
| `agents` | List agent panes and status |
| `fork` | Fork an agent session; `--pane ID`, `--split DIRECTION` |
| `hooks install\|uninstall\|status` | Manage hooks; `--agent claude\|codex` |
| `explain` | Explain the last finished command; `--pane ID`; needs enabled AI |
| `config-path` | Print the configuration path |
| `reload` | Reload configuration |
| `set KEY VALUE` | Write a setting and reload |
| `theme [SPEC]` | List themes, or select a theme |
| `save` | Write a session snapshot |
| `layout` | Print layout JSON |
| `daemon ACTION` | Control the daemon; see [Sessions](/thurm/sessions/) |
| `socket-path` | Print the daemon socket path |
| `remote add\|list\|remove\|status\|install` | Manage remote hosts; see [Remote workspaces](/thurm/remote/) |
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
not all saved scrollback.

| Exit code | Meaning |
| --- | --- |
| `0` | Command succeeded or wait condition was met |
| `1` | Operation failed |
| `2` | Invalid CLI arguments |
| `124` | Wait timed out |

`--remote NAME` is global: it sends the command to that host's daemon through
the tunnel the app keeps open, and fails when the app is not connected to it.
Pass `--pane` with it. See [Remote workspaces](/thurm/remote/#the-cli).

`--json` is global. Use it for structured results where the command returns
data. A command that only performs an action can return no output.
