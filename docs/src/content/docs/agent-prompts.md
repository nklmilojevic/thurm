---
title: Agent prompts
description: Submit a prompt and wait for the same agent turn.
---

Use `agent prompt` to submit text to the current agent in a pane:

```sh
thurm agent prompt --pane 12 "Review this change" --wait --timeout 300
printf 'Review this change.\nCheck the error paths.\n' | thurm agent prompt --pane 12 --wait -
```

Without `--pane`, the command uses `THURM_PANE_ID`. Without `--wait`, it returns after it queues the prompt. `--timeout` is in seconds. The default is 300 seconds. The allowed range is 1 to 86400 seconds.

The daemon checks the current foreground process and agent state before it sends the text. It rejects the prompt if the agent is working, needs input, has a permission request, or has another prompt pending. Text and Enter are sent together. A multiline prompt requires bracketed paste mode. Control characters other than newline and tab are rejected.

A wait applies to the same process and session. Other keyboard input stops the wait with an error. An old Idle or Done state cannot complete the wait. Hooks record the start of the new turn, including a turn that finishes between status checks. For agents without hooks, Thurm must observe a Working state before it accepts completion. A very short turn can time out if no Working state was observed. Completion means that the turn ended, not that the task succeeded.

If a submitted prompt does not start a turn, it stays pending after a timeout. This prevents a retry from sending duplicate input. Check the pane before you continue. The next turn-start event or a new agent session clears this state. `send` and `wait` keep their existing behavior.

| Exit code | Result |
| --- | --- |
| 0 | Prompt submitted, or the turn completed with `--wait` |
| 1 | Request rejected or agent identity changed |
| 2 | Pane exited |
| 3 | Agent needs input |
| 124 | Wait timed out |

Use `--json` for a result with `pane` and `outcome` fields.
