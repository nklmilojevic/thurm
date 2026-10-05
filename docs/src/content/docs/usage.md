---
title: Use Thurm
---

## Tabs and split panes

Each pane has its own process and terminal state. A tab can contain several
panes. Thurm has one window; use a new workspace when you need another group
of tabs.

| Action | macOS | Linux |
| --- | --- | --- |
| New tab / workspace | Cmd+T / Cmd+N | Ctrl+Shift+T / Ctrl+Shift+N |
| Close pane / tab | Cmd+W / Cmd+Shift+W | Ctrl+Shift+W / Ctrl+Shift+Alt+W |
| Select tab 1 through 8 / last tab | Cmd+1 through Cmd+8 / Cmd+9 | Alt+1 through Alt+8 / Alt+9 |
| Previous / next tab | Cmd+Shift+[ / Cmd+Shift+] | Ctrl+Page Up / Ctrl+Page Down |
| Split right / down | Cmd+D / Cmd+Shift+D | Ctrl+Shift+D or Ctrl+Shift+O / Ctrl+Shift+E |
| Move focus between panes | Cmd+Option+Arrow | Ctrl+Alt+Arrow |
| Move a divider | Cmd+Control+Arrow | Ctrl+Shift+Alt+Arrow |
| Zoom the current pane | Cmd+Shift+Return | Ctrl+Shift+Return |
| Equalize split sizes | Cmd+Control+= | Ctrl+Alt+= |
| Command palette | Cmd+Shift+P | Ctrl+Shift+P |
| Workspace switcher | Cmd+Shift+O | Ctrl+Alt+O |
| Agent switcher | Cmd+Shift+A | Ctrl+Alt+A |

Menu items named here (such as **Thurm › Remotes…**) are in the menu bar on macOS. On
Linux they are in the main menu, the ☰ button in the header bar, and in the command
palette.

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

## Text, search, and navigation

| Action | macOS | Linux |
| --- | --- | --- |
| Copy / paste / select all | Cmd+C / Cmd+V / Cmd+A | Ctrl+Shift+C / Ctrl+Shift+V / Ctrl+Shift+A |
| Find text in scrollback | Cmd+F | Ctrl+Shift+F |
| Find next / previous | Cmd+G / Cmd+Shift+G | Ctrl+Shift+G / Ctrl+Shift+H |
| Clear screen / scrollback | Cmd+K / Cmd+Option+K | Ctrl+Shift+K / Ctrl+Shift+Alt+K |
| Delete to line start / go to line start / end | Cmd+Backspace / Cmd+Left / Cmd+Right | Your shell's own keys (Ctrl+U, Ctrl+A, Ctrl+E) |
| Increase / decrease / reset font size | Cmd++ / Cmd+- / Cmd+0 | Ctrl++ (or Ctrl+=) / Ctrl+- / Ctrl+0 |
| Open a URL | Cmd+click | Ctrl+click |
| Open / reload configuration | Cmd+, / Cmd+Shift+, | Ctrl+, / Ctrl+Shift+R |
| Secure Keyboard Entry | Cmd+Option+I | Not available (macOS only) |
| Full screen | Cmd+Control+F | F11 |
| Quit | Cmd+Q | Ctrl+Shift+Q |

On Linux, Ctrl combinations such as Ctrl+C, Ctrl+R and Ctrl+D belong to the programs in
the terminal, so most of Thurm's own shortcuts add Shift (like GNOME Terminal's). Middle click
pastes the primary selection, and selecting text sets it.

Change any shortcut in the `[keybindings]` table of the configuration: a key
combination maps to an action name, or to `none` to remove it. `cmd+…` means Cmd on macOS
and Ctrl+Shift on Linux (`cmd+shift+…` is Ctrl+Shift+Alt). Linux combinations such
as `ctrl+alt+d` work as written there.

Shell integration adds prompt marks and working directory tracking for zsh,
bash, and fish. It also supports completion choices at a shell prompt. Use
`thurm scroll prev-prompt` or `thurm scroll next-prompt` to move between marks.

## Input and notifications

On macOS, the Option key is not Alt by default. Set `window.option_as_alt` to `left`,
`right`, or `both` if your shell tools need Alt key combinations. On Linux, Alt is
always Alt.

On macOS, Thurm can enable Secure Keyboard Entry when it detects a password prompt.
It releases this protection when the app is not in front. It can also ask before
a paste with multiple lines when the program does not use bracketed paste mode.

Allow notifications in macOS System Settings (on Linux, in your desktop's
notification settings) to receive agent and command notifications. By default, command notifications apply to commands that take
at least 15 seconds and finish in a pane without focus.
