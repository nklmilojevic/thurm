---
title: Coding agents
---

Thurm detects supported agent processes and shows their status. Launch presets
start tools that are already installed. Thurm does not install those tools or
configure their accounts.

```sh
thurm presets
thurm launch Codex
thurm agents
```

Use the preset name shown by `thurm presets`. Add `--split right` to launch in
a split or `--cwd PATH` to select a working directory. Cmd+Shift+A (Linux: Ctrl+Alt+A) opens the
agent switcher.

Use [Agent prompts](/agent-prompts/) to submit text and wait for the same turn.
Use [Agent reporting and diagnostics](/agent-integration/) to inspect detection
or add support for another agent.

## Install hooks

Claude Code and Codex hooks provide turn completion events and session IDs.
Check and install them with:

```sh
thurm hooks status
thurm hooks install --agent claude
thurm hooks install --agent codex
```

These commands change the agent configuration files. Without `--agent`, Thurm
selects supported agents whose configuration directories exist. Use
`thurm hooks uninstall --agent codex` or `--agent claude` to remove Thurm hooks.
Run agents inside Thurm so their processes inherit `THURM_PANE_ID`.

You rarely need to run these yourself. When Thurm launches `claude` or `codex`
and its hooks are missing, the daemon adds them first, keeping a backup of the
file (`settings.json.thurm-backup`). This includes agents installed after
Thurm, on this Mac or on a remote host. Set `agents.install_hooks = false` to
turn it off.

Hooks support `thurm wait --agent-done --timeout 120`, session titles, and
notifications when turns finish. Without hooks, detection uses process and
screen information, which cannot provide the same completion signal.

## Resume and fork

With hooks and a known session ID, use `thurm fork --split right` from a Claude
Code or Codex pane to create a separate branch of the agent session.
Use `--pane ID` to select another source pane. Without `--split`, the fork
opens in a new tab. The installed agent must support its configured fork command.

Saved sessions can use agent resume commands after a daemon restart.
See [Sessions](/sessions/) for the limits of restoration.

## Custom presets and detection

To replace the default preset list:

```toml
[agents]
presets = [{ name = "My Bot", command = ["mybot"] }]
```

An empty preset list uses installed built-in agents. To define another agent,
add an `[[agents.define]]` block. `processes` matches executable names; `argv`
matches parts of process arguments. `working` and `attention` match screen text.
`working_title` matches the window title's spinner; once the agent shows it, the
title decides between working and idle instead of output activity.
Optional launch, resume, and fork commands control how Thurm starts that agent.
See the [configuration reference](/configuration-reference/) for a full example.

Agent notifications apply to panes without focus. Check macOS notification
permissions and `[notifications]` if they do not appear. Optional AI summaries
are separate and require `ai.enabled = true`.

With Claude Code hooks installed, a permission prompt's notification has
**Approve** and **Deny** buttons. Approve picks the prompt's default choice
(allow once) and needs the Mac unlocked. Deny declines, as Esc would. Either
answers only the prompt the notification is about: once you answer in the
terminal, press a key in the pane, or the agent moves on, the notification is
withdrawn, and a button pressed in the meantime opens the pane instead.

For agent authors, the repository includes a
[Thurm skill](https://github.com/nklmilojevic/thurm/blob/main/skills/thurm/SKILL.md)
with CLI procedures.
