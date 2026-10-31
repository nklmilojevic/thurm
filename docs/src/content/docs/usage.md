---
title: Use Thurm
---

## Tabs and split panes

Each pane has its own process and terminal state. A tab can contain several
panes. Thurm has one window; use a new workspace when you need another group
of tabs.

| Action | Shortcut |
| --- | --- |
| New tab / workspace | Cmd+T / Cmd+N |
| Close pane / tab | Cmd+W / Cmd+Shift+W |
| Select tab 1 through 8 / last tab | Cmd+1 through Cmd+8 / Cmd+9 |
| Previous / next tab | Cmd+Shift+[ / Cmd+Shift+] |
| Split right / down | Cmd+D / Cmd+Shift+D |
| Move focus between panes | Cmd+Option+Arrow |
| Move a divider | Cmd+Control+Arrow |
| Zoom the current pane | Cmd+Shift+Return |
| Show / hide floating panes | Cmd+Option+Return |
| Equalize split sizes | Cmd+Control+= |
| Command palette | Cmd+Shift+P |
| Workspace switcher | Cmd+Shift+O |
| Agent switcher | Cmd+Shift+A |

Menu items named here (such as **Thurm › Remotes…**) are in the menu bar and in the
command palette.

A floating pane is a card above the tab's splits. Cmd+Option+Return opens a new
shell as one when the tab has none, and otherwise hides the tab's floating panes
or shows them again; hidden, their programs keep running. **View › New Floating
Pane** adds another, and **View › Float Pane** lifts the current pane out of the
splits (the last pane of the splits stays). Drag a card's header to move it, drag
its edges to resize it, and double-click the header to zoom it. To put it in the
splits, use **View › Dock Pane**, or hold Cmd while dragging its header over a pane
and drop it on the side where it should go (Esc cancels the drag).

Closing a pane stops its process. Quitting the app keeps processes running by
default. See [Sessions](/sessions/).

## Workspaces and sidebar

A workspace is a named set of tabs. The window shows one workspace at a time;
use the workspace switcher to change it. Shells in hidden workspaces continue
to run. A tab opened for another workspace, such as `thurm launch` on a remote
host while the window shows a local workspace, goes to that workspace in the
background, and the window says where. **Move Tab to New Workspace** (or
dragging a tab out of the tab bar) moves a tab into a new workspace.

By default, tabs show in a sidebar grouped by repository. To use native tabs in the
titlebar:

```sh
thurm set window.tab_style native
```

Use `thurm set window.tab_style sidebar` to return to the sidebar.

The View menu changes what the sidebar lists, as do the settings below:

- **Group Tabs by Repository** (`window.sidebar_group_by_repo`, on by default). Off,
  the tabs form one list in tab order.
- **Tabs from All Workspaces** (`window.sidebar_all_workspaces`). Every workspace's
  tabs show under a header for the workspace. Click a tab of another workspace to
  switch to it.
- **Only Active Tabs** (`window.sidebar_active_only`, or the filter button next to
  the sidebar button in the titlebar). The sidebar lists only the tabs with an agent that isn't
  idle or a program reporting progress, plus the selected tab.

When the sidebar is hidden, the filter button beside its toggle becomes **+** (New Tab).
Showing the sidebar restores the filter button, with its setting preserved.

## Text, search, and navigation

| Action | Shortcut |
| --- | --- |
| Copy / paste / select all | Cmd+C / Cmd+V / Cmd+A |
| Find text in scrollback | Cmd+F |
| Find next / previous | Cmd+G / Cmd+Shift+G |
| Clear screen / scrollback | Cmd+K / Cmd+Option+K |
| Delete to line start / go to line start / end | Cmd+Backspace / Cmd+Left / Cmd+Right |
| Increase / decrease / reset font size | Cmd++ / Cmd+- / Cmd+0 |
| Open a URL | Cmd+click |
| Open / reload configuration | Cmd+, / Cmd+Shift+, |
| Secure Keyboard Entry | Cmd+Option+I |
| Full screen | Cmd+Control+F |
| Quit | Cmd+Q |

Change any shortcut in the `[keybindings]` table of the configuration: a key
combination maps to an action name, or to `none` to remove it. See
[Key bindings](/configuration/#key-bindings) for the action names.

Shell integration adds prompt marks and working directory tracking for zsh,
bash, and fish. It also supports completion choices at a shell prompt. Use
`thurm scroll prev-prompt` or `thurm scroll next-prompt` to move between marks.

Completion choices for a command come from a spec named `<command>.json`. Thurm
reads it from `~/.config/thurm/completions` first, then from the specs bundled with
the app. A spec can list subcommands, options and values, or tell Thurm to ask the
program itself at each Tab, so the choices match the installed version:

```json
{ "dynamic": { "protocol": "cobra" } }
```

Use `cobra` for Go programs built with Cobra (they answer `__complete`). Use
`{ "protocol": "clap", "env": "PROG_COMPLETE" }` for Rust programs that support
clap's dynamic completion through that environment variable. If the program can't
answer, Thurm uses the rest of the spec.

## Input and notifications

The Option key is not Alt by default. Set `window.option_as_alt` to `left`,
`right`, or `both` if your shell tools need Alt key combinations.

Thurm can enable Secure Keyboard Entry when it detects a password prompt.
It releases this protection when the app is not in front. It can also ask before
a paste with multiple lines when the program does not use bracketed paste mode.

Allow notifications in macOS System Settings to receive agent and command
notifications. By default, command notifications apply to commands that take at
least 15 seconds and finish in a pane without focus.
