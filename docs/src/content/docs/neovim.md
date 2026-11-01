---
title: Neovim in panes
description: Run Neovim with each of its windows in a Thurm pane.
---

`thurm-nvim` runs Neovim with each window in its own Thurm pane. One Neovim runs
behind all the panes, so buffers, registers, undo history and language servers are
shared.

```sh
thurm-nvim src/main.rs
```

Run it in a Thurm pane. It passes its arguments to `nvim`, and it uses your usual
Neovim configuration and plugins.

- `:split` and `:vsplit` open a Thurm split where Neovim places the new window.
  `<C-w>` moves between windows and moves the Thurm focus with them. Clicking a
  pane makes its window current.
- Closing a pane closes its window. Unsaved changes stay in the buffer.
- `:q` in the last window quits Neovim, and `:qa` closes every pane. The pane you
  started from goes back to your shell.
- Each pane draws its window's statusline. The command line and messages appear
  at the bottom of the current window's pane, or in your plugin's popup if you
  use one.

`thurm-nvim` ships with the app. **Thurm › Integrations › Install Command-Line
Tool** links it into `~/.local/bin` next to `thurm`. Without that link, run it by
its full path; inside a Thurm pane `$THURM_BIN_DIR` holds that directory:

```sh
"$THURM_BIN_DIR/thurm-nvim" src/main.rs
```

On Linux it is in the release archive next to `thurm` and `thurmd`. Set
`THURM_NVIM` to run a different `nvim` binary.

## Limitations

- `<C-w>` resize, move and swap commands have no effect. Use Thurm's pane
  controls instead.
- There is one tab page. Neovim tab pages don't map to Thurm tabs.
- A pane you open in Thurm with Cmd+D runs a shell, not a new Neovim window.
- Plugins that arrange windows (sidebars, layout managers) don't get the
  placement they expect.
- Long messages still show the "Press ENTER" prompt.
