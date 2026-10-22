---
name: thurm
description: Drive the Thurm terminal from inside a pane — open tabs and splits, run long commands in a separate pane, read other panes' output, wait for commands or other agents to finish, and notify the user. Use when running inside Thurm ($THURM_PANE_ID is set) and you need a second terminal, a dev server, a watcher, or to coordinate with another agent.
metadata:
  thurm-stub: "1"
---
<!-- Installed by "thurm skill setup". Remove it with "thurm skill remove". -->

Run `thurm skill show` and follow the guide it prints. The guide comes from the installed
`thurm` CLI, so it describes the commands that version has. If `thurm` isn't on PATH, run
`"$THURM_BIN_DIR/thurm" skill show`.

If the command fails, the installed Thurm predates the guide: start with `thurm info`, then
`thurm --help` and `thurm COMMAND --help`.
