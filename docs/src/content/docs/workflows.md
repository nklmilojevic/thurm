---
title: Layout templates and events
---

## Reuse a tab layout

Export the tab that contains the current pane:

```sh
thurm layout export --output project.json
```

Outside a Thurm pane, add `--pane ID`. The template contains the split directions,
split sizes, tab title, and working directories. It does not contain pane IDs,
remote host names, terminal output, or live process arguments. Each exported pane
starts a shell when you apply the template.

You can add a `command` argument array to a pane. For example:

```json
{
  "version": 1,
  "title": "Project",
  "root": {
    "type": "split",
    "dir": "right",
    "ratio": 0.6,
    "first": { "type": "pane", "cwd": "/path/to/project" },
    "second": {
      "type": "pane",
      "cwd": "/path/to/project",
      "command": ["cargo", "test"],
      "hold": true
    }
  }
}
```

Apply it with:

```sh
thurm layout apply project.json
```

Keep the desktop app open. This command creates a new workspace with one tab.
It runs commands from the template and prints the new pane IDs as JSON. Check
the commands before you apply a template from another source. Commands use
literal arguments. Shell operators and variable expansion need an explicit
shell command, such as `["sh", "-c", "your command"]`.

The CLI checks the full template before it creates panes. It waits up to 10 seconds
for the selected desktop app to confirm that it installed the tab. If creation,
installation, or confirmation fails, it closes the panes that it created. A command
that already started can have effects that closing its pane does not undo.

Templates support up to 64 panes and 16 split levels. Split ratios must be from
0.05 to 0.95. `hold` keeps a pane open after its command ends.

Use `--remote NAME` to export or apply a layout on a connected remote host.
Directories and commands then refer to that host. The app must keep its remote
connection open. Plain `thurm layout` still prints the saved session layout.

## Read state changes

```sh
thurm events --json
thurm events --json --pane 42
thurm --remote devbox events --json
```

Each line is one JSON event. The stream contains `PaneInfo`, `PaneExited`,
`PaneClosed`, `ConfigReloaded`, and `LayoutChanged` events. `PaneInfo` includes
agent state when an agent is present. With `--pane`, only events for that pane
are included. The stream does not contain terminal output or notifications.
Output is always JSON Lines, with or without `--json`.

Example:

```json
{"PaneClosed":{"pane":42}}
```

Events report changes after the connection starts. They are not a saved history
or a complete initial snapshot. Use `thurm --json list` and `thurm layout` to
read current state. When an event arrives, read current state again if needed.

The command fails if the connection closes or its event buffer fills. Reconnect
and read current state again. It does not silently reconnect or replay events.
Press Ctrl+C to stop it.
