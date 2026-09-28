# Use Thurm

## Tabs and split panes

Each pane has its own process and terminal state. A tab can contain several
panes. Use a new window when you need another group of tabs.

| Action | Default shortcut |
| --- | --- |
| New tab / window | Cmd+T / Cmd+N |
| Close pane / tab | Cmd+W / Cmd+Shift+W |
| Select tab 1 through 8 / last tab | Cmd+1 through Cmd+8 / Cmd+9 |
| Previous / next tab | Cmd+Shift+[ / Cmd+Shift+] |
| Split right / down | Cmd+D / Cmd+Shift+D |
| Move focus between panes | Cmd+Option+Arrow |
| Move a divider | Cmd+Control+Arrow |
| Zoom the current pane | Cmd+Shift+Return |
| Equalize split sizes | Cmd+Control+= |
| Command palette | Cmd+Shift+P |
| Workspace switcher | Cmd+Shift+O |
| Agent switcher | Cmd+Shift+A |

Closing a pane stops its process. Quitting the app keeps processes running by
default. See [Sessions](sessions.md).

## Workspaces and sidebar

A workspace is a named set of tabs. Use the workspace switcher to change the
visible workspace. Shells in hidden workspaces continue to run. If another
window already shows the selected workspace, Thurm brings that window forward.

To show tabs in a sidebar grouped by repository:

```sh
thurm set window.tab_style sidebar
```

Use `thurm set window.tab_style native` to return to native tabs.

## Text, search, and navigation

| Action | Default shortcut |
| --- | --- |
| Copy / paste / select all | Cmd+C / Cmd+V / Cmd+A |
| Find text in scrollback | Cmd+F |
| Find next / previous | Cmd+G / Cmd+Shift+G |
| Clear screen / scrollback | Cmd+K / Cmd+Option+K |
| Increase / decrease / reset font size | Cmd++ / Cmd+- / Cmd+0 |
| Open a URL | Cmd+click |
| Open / reload configuration | Cmd+, / Cmd+Shift+, |
| Secure Keyboard Entry | Cmd+Option+I |

Shell integration adds prompt marks and working directory tracking for zsh,
bash, and fish. It also supports completion choices at a shell prompt. Use
`thurm scroll prev-prompt` or `thurm scroll next-prompt` to move between marks.

## Input and notifications

The Option key is not Alt by default. Set `window.option_as_alt` to `left`,
`right`, or `both` if your shell tools need Alt key combinations.

Thurm can enable Secure Keyboard Entry when it detects a password prompt.
It releases this protection when the app is not in front. It can also ask before
a paste with multiple lines when the program does not use bracketed paste mode.

Allow notifications in macOS System Settings to receive agent and command
notifications. By default, command notifications apply to commands that take
at least 15 seconds and finish in a pane without focus.
