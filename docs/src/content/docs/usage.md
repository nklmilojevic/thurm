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
| Back / forward through the tabs visited | Cmd+[ / Cmd+] |
| Split right / down | Cmd+D / Cmd+Shift+D |
| Move focus between panes | Cmd+Option+Arrow |
| Move a pane with the mouse | Cmd+Option+drag inside the pane |
| Move a divider | Cmd+Control+Arrow |
| Zoom the current pane | Cmd+Shift+Return |
| Show / hide floating panes | Cmd+Option+Return |
| Equalize split sizes | Cmd+Control+= |
| Command palette | Cmd+Shift+P |
| Insert path | Cmd+Option+P |
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

Hold Cmd+Option and drag inside a pane to move it, even when pane headers are
hidden. Drop it at an edge of the tab to give it that half of the tab, or over
another pane to split that pane on the side under the pointer. The drop preview
shows the new position. Esc cancels the move. A click without a drag keeps the
layout unchanged, and on a file's path opens it in Quick Look. A split pane can
move only if another split pane remains.

You can also drag a floating card's header to an edge to dock it there.
Normal titlebar dragging and double-clicking keep their window actions.
**View › Move Pane to Edge** moves the current pane to an edge from the keyboard.

The minimize button in a card's header (or **View › Minimize Floating Pane**) puts
the card away as a chip in the bottom right of the tab; its program keeps running,
and the chip shows its agent's status. Chips stay in one row; use the **+N** menu
to restore cards whose chips do not fit. Chips stay on screen while floating panes are
hidden. Click a chip to bring the card back where
it was. Focusing the pane another way (the agent wall, a notification, `thurm
focus`) brings it back too.

Drag a chip to put it anywhere in the tab; it stays there, also after a restart,
until you drop it back on the row of chips. Drop it on the sidebar, between two
rows, to make the pane a tab of its own there. Esc cancels the drag.
`thurm new-float --minimized -- CMD` starts a command in a card that is already
minimized, and `thurm float --minimize` (or `--restore`) minimizes an existing
pane from the command line.

Drag a tab's row from the sidebar onto the terminal area to float that tab as a
card in the tab you're looking at; its programs keep running and its own floating
panes come along. Only a tab with one pane in its splits can float, and only into
a tab of the same window and host. **View › Float Tab Here** (or **Float Tab
Here** in a sidebar row's menu) does the same. The other way, drag a card's header
onto the sidebar and drop it between two rows to make it a tab of its own there,
or use **View › Move Floating Pane to New Tab**. Esc cancels either drag.

**Window › Back** returns to the tab you were on before, as in a browser, and
**Window › Forward** goes forward again after a Back. The history spans every
workspace: going back to a tab of a hidden workspace switches the window to that
workspace. Picking a tab any other way after a Back drops the tabs ahead of it,
and closed tabs are skipped. The quick terminal and the agent wall are not part
of the history. The mouse's back and forward buttons do the same, also while a
program uses the mouse; set `window.mouse_navigation_buttons = false` to send them
to the program instead.

Closing a pane stops its process. Quitting the app keeps processes running by
default. See [Sessions](/sessions/).

## Insert a path

Choose **Edit > Insert Path…**, press **Cmd+Option+P**, or use **Insert Path** in
the command palette or terminal context menu. Search starts in the terminal's
working folder. An empty search lists that folder and recent selections.
Type a name to search its subfolders, or use a path such as `../`, `src/`,
`~/`, or `/tmp/`. Type a leading dot to include hidden names.

Use the arrow keys to select a path. **Return** inserts it as one shell word
with the required escapes and a space. It does not send Enter to the terminal.
**Tab** opens the selected folder in the picker. **Esc** closes the picker.
Recent selections are kept for each host and working folder.

Remote workspaces search on their own host. For a terminal running a separate
`ssh` or `mosh` command, use a Thurm remote workspace instead. Update Thurm on
an older host if the picker asks for it. Large searches have a limit; add a
folder or a more specific name when the picker shows that more paths exist.
Paths with control characters cannot be inserted.

## Workspaces and sidebar

A workspace is a named set of tabs. The window shows one workspace at a time;
use the workspace switcher to change it. Shells in hidden workspaces continue
to run. A tab opened for another workspace, such as `thurm launch` on a remote
host while the window shows a local workspace, goes to that workspace in the
background, and the window says where. **Move Tab to New Workspace** (or
dragging a tab out of the tab bar) moves a tab into a new workspace.
**Move Tab to Workspace** (in the Window menu, the command palette, and a
sidebar tab's context menu) moves it into an existing workspace on the same
host. Its splits and programs move with it. Moving a workspace's last tab
shows the workspace it moved to, and the emptied workspace closes.

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
  switch to it. Drag a tab onto another workspace to move it there.
- **Only Active Tabs** (`window.sidebar_active_only`, or the filter button next to
  the sidebar button in the titlebar). The sidebar lists what is going on, by status:
  **Needs input**, **Failed**, **Done** and **Working**. Each agent that isn't idle gets its
  own row, in any window or workspace, so an agent in another split or workspace is one click
  away. Each row shows what the agent is doing and when it last reported. An agent keeps its
  place within its status. Under **Tabs** come the tabs with a program reporting progress,
  and the selected tab when its focused pane has no agent row. Dragging rows does not reorder tabs in this list.

To keep a tab in sight whatever workspace the sidebar shows, right-click it and choose
**Pin Tab**, or use **Window > Pin Tab**. Pinned tabs show under **Pinned** at the top of the
sidebar, in every workspace, and not again in their own group. Click one from another workspace
to switch to it. **Only Active Tabs** never hides a pinned tab. Pins are kept across restarts.

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
| Preview a file | Cmd+Option+click its path, or right-click it and select Quick Look |
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

On Mac, **Quick Look** (or a Cmd+Option+click on a path) opens a native preview
panel for images, PDFs, and other files that macOS can preview. Press Escape to
close the panel. If macOS has no preview provider for the file type, the panel
shows basic file information.

Use an absolute path, a `~/` path, a relative path such as `./report.pdf`, or a
`file://` link. Filenames with an extension, such as `report.pdf`, also work.
Put paths with spaces in quotes or use shell escapes. A path must fit on one
terminal row. OSC 8 file links can span rows. A `:line` or `:line:column` suffix
is removed only if the full filename does not exist. This rule also applies to
validated local file URLs. References such as `server:report.pdf` are remote
paths, even if a local file has that name. To preview that local file, use
`./server:report.pdf`, an absolute path, or a local file URL. Numeric suffixes
such as `report.pdf:42` keep the full filename check.

Relative paths use the clicked pane's current reported directory when the menu
opens. Old output does not retain its original directory. Use an absolute path
if the pane has since changed directory or its directory is unknown.
In a remote pane, Quick Look fetches the file from the host first (see
[Remote hosts](/remote/#files)). Hosts running an older Thurm, and file links to
other hosts from this Mac's panes, cannot use Quick Look: copy the file to this
Mac first. Missing files, unreadable files, folders, and special files
show a message. In programs that use the mouse, hold Shift when you right-click
to open the Thurm context menu; Cmd+Option+click works there as it is.

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

Drop files on a pane to insert their paths, quoted for the shell and separated
by spaces. A pasted or dropped image is saved to a file first. In a remote pane,
dropped files, and files copied in Finder and sent with **Send Copied Files** in the pane's
context menu, go to the host once you confirm them, and their
paths there are inserted (see [Remote hosts](/remote/#files)).

Allow notifications in macOS System Settings to receive agent and command
notifications. By default, command notifications apply to commands that take at
least 15 seconds and finish in a pane without focus.
