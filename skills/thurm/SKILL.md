---
name: thurm
description: Drive the Thurm terminal from inside a pane — open tabs and splits, run long commands in a separate pane, read other panes' output, wait for commands or other agents to finish, and notify the user. Use when running inside Thurm ($THURM_PANE_ID is set) and you need a second terminal, a dev server, a watcher, or to coordinate with another agent.
---

# Driving Thurm with the `thurm` CLI

Thurm keeps every shell in a background daemon, so panes survive the app closing and are
addressable by numeric id. `$THURM_PANE_ID` is the pane you are running in; every command
defaults to it. Add `--json` to any command for machine-readable output.

## Run something in its own pane

```sh
# Split the current pane to the right and start a dev server there; prints the new pane id.
id=$(thurm split --dir right -- npm run dev)

# Wait until it's ready (regex on the visible screen), 60 s timeout. Exit code 124 = timeout.
thurm wait --pane "$id" --match 'ready|listening on' --timeout 60

# Read its output (visible screen, or the last N lines incl. scrollback).
thurm capture --pane "$id" -n 50
```

Other ways to open panes: `thurm new-tab [--cwd DIR] -- CMD...`, `thurm split --dir down`,
`--hold` keeps the pane open after the command exits.

## Type into a pane

```sh
thurm send --pane "$id" "cargo test"          # appends Enter
thurm send --pane "$id" --no-enter "partial"
thurm send --pane "$id" --paste - < script.sh  # bracketed paste from stdin
thurm send-keys --pane "$id" C-c              # tmux-style keys: Enter Tab Escape Up C-d M-x ...
```

## Wait for things

| Command | Returns when |
|---|---|
| `thurm wait --pane ID --prompt` | the shell is back at a prompt (the command finished) |
| `thurm wait --pane ID --idle 2000` | no output for 2 s |
| `thurm wait --pane ID --match REGEX` | the regex matches the screen |
| `thurm wait --pane ID --exit` | the pane's process exited |
| `thurm wait --pane ID --agent-free` | the agent in that pane stopped working (idle or asking for input) |
| `thurm wait --pane ID --agent-done` | the agent in that pane finished its turn (needs hooks, see below) |
| `thurm wait --pane ID --agent-waiting` | the agent in that pane is asking for input |

Always pass `--timeout SECS` so you never hang.

## Other agents

```sh
thurm agents            # panes running Claude Code, Codex, Gemini, Aider, ... with status
thurm presets           # configured agent launch presets
thurm launch claude --split right   # start a preset in a split
```

Status is `Working`, `Idle`, `NeedsInput` (a permission prompt or question) or `Done` (finished a
turn nobody has looked at yet). Without hooks it is inferred from the screen; with hooks
(`thurm hooks install`: Claude Code and Codex) the agent reports it directly, which is exact and also gives
`Done` and the session id. `thurm hooks status` shows whether they are installed; don't install
them without asking the user, it edits their `~/.claude/settings.json`.

`thurm fork --split right` starts a copy of the current agent session next to it (needs hooks).

Delegating to another agent: `thurm launch claude --split right`, type the task with
`thurm send --pane ID --paste "..."`, then `thurm wait --pane ID --agent-done --timeout 1800` and
read the result with `thurm capture --pane ID`.

## Misc

```sh
thurm list                       # all panes (id, pid, size, title, agent, cwd)
thurm info                       # details for the current pane
thurm title "backend"            # tab title
thurm focus ID                   # bring a pane to the front
thurm notify "Done" "tests pass" # desktop notification
thurm close ID                   # kill a pane
```

Guidelines: prefer a separate pane for long-running processes instead of backgrounding them in
your own shell; clean up panes you created with `thurm close` when finished; never type into a
pane the user is working in without being asked.
