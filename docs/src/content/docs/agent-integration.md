---
title: Agent reporting and diagnostics
---

Use `thurm agent explain --pane ID` to inspect an agent. Add `--json` for structured output. The result contains the foreground process and arguments, the state source, matched detection rules, recent screen text, and the idle threshold. Screen text can contain private data. The command reads the current screen when requested.

The source is `report`, `hook`, `notification`, `ai`, `screen-and-activity`, or `none`. A foreground report or hook overrides screen rules. The listed rules also show matching screen patterns when a report overrides them.

## Report lifecycle state

Run reports on the host where the agent process runs. The process must belong to the target pane's foreground process group. The command uses `THURM_PANE_ID` unless you supply `--pane`.

```sh
thurm agent report --agent my-agent --owner-pid 1234 \
  --instance launch-unique-id --sequence 1 --status idle \
  --session-id session-42 \
  --resume-json '["my-agent","resume","session-42"]'

thurm agent report --agent my-agent --owner-pid 1234 \
  --instance launch-unique-id --sequence 2 --status working

thurm agent report --agent my-agent --owner-pid 1234 \
  --instance launch-unique-id --sequence 3 --status needs-input \
  --message 'Approve the file change.'

thurm agent report --agent my-agent --owner-pid 1234 \
  --instance launch-unique-id --sequence 4 --status done

thurm agent release --owner-pid 1234 \
  --instance launch-unique-id --sequence 5
```

Use the agent's PID for `--owner-pid`. Do not use the PID of the short-lived `thurm` command. Generate a unique instance ID when the agent process starts. Keep that ID and agent kind for the process lifetime. Sequence numbers start at one and must increase for every report and release. Allocate numbers before sending concurrent requests. The daemon rejects duplicate or older numbers without a state change.

The daemon binds the instance ID to the owner PID and its operating system start time. Another reporter cannot take the same active process group. A replaced or released instance cannot report again. Release before the agent process exits, while it remains in the foreground. Reports can no longer change the pane after their owner leaves the foreground. If the owner moves to another process group, the old instance is released. Public report ownership is tracked even when automatic detection is disabled. These checks prevent accidental stale reports; the local daemon socket remains a same-user control interface.

Supported states are `idle`, `working`, `needs-input`, and `done`. Report `working` when a new turn starts, and `done` when it finishes. Report `needs-input` when the agent needs a response. Public reports do not create one-click permission choices. Existing built-in `agent-hook` integrations still work. A public report has priority over legacy hooks in its process group.

Each new turn must start from `idle` or `done`. Repeated `working` reports update the
same turn. A transition from `needs-input` to `working` continues that turn.
`agent prompt` rejects a submission while the reported state is `working` or
`needs-input`. Sequence numbers order reports; they do not identify turns.

An omitted session ID keeps the existing ID. An omitted message clears the previous message. Resume arguments remain until the session ID changes, the reporting process is replaced, or it releases ownership. Report the full resume arguments again after a session ID change. A release ends reporting and returns the pane to automatic detection.

## Exact session restore

`--resume-json` is a JSON array containing an executable followed by its arguments. A session ID is required with this array. Arguments cannot contain control characters. There can be at most 128 arguments and 32768 bytes in total.

Thurm saves the session ID and resume arguments with the active pane. When `session.resume_agents` is enabled, restore opens a shell pane and starts the reported command below that shell. Each argument is protected with shell quoting. Thurm does not expand argument text or replace placeholders. Supply the actual session ID in the argument array. The process inherits the saved working directory. The shell and saved history remain available if the resume command fails or exits. Automatic resume supports sh, bash, zsh, dash, ksh, and fish. For another shell, Thurm keeps the pane open and shows that automatic resume was skipped. Without reported arguments, the existing configured resume rules apply.

After a cold restart, a restored agent process starts a new instance and reports again. An in-place daemon upgrade keeps reporting ownership, sequence numbers, and resume arguments while the agent process remains alive. Reports are limited to 4096 replaced instances per pane.

## Socket API

The public requests are `AgentReport`, `AgentRelease`, and `AgentExplain`. `AgentReport` takes a pane ID and an `AgentReport` object with the same fields as the CLI. `resume_argv` is the argument array. `AgentRelease` takes `pane`, `owner_pid`, `instance`, and `sequence`. `AgentExplain` takes `pane` and returns `AgentExplanation`.

`AgentReport` and `AgentRelease` return `Ok` on success and an error on invalid ownership, sequence, or arguments. They do not discard errors as the legacy hook command does. Use a CLI and daemon from the same build for these requests.
