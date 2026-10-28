# Changelog

## v0.15.0 - 2026-10-28

This release adds floating panes, which show a pane above the splits of a tab. It also compresses remote output and fixes input and connection problems.

### New features

- Floating panes: a pane can float above the splits of a tab as a card. Use View › Float Pane, the pane header button, or `thurm float`. Dock Pane puts the card back into the splits.
- Cmd+Option+Return opens a shell as a card on any tab, or shows and hides the cards.
- The `thurm new-float` command opens a pane as a card.
- The focus shortcuts now move to floating cards too.
- Right-click a link to choose Open Link or Copy Link. Open Link uses the same policy as Cmd-click.
- Copy Pane ID in the Shell menu copies a pane ID for use with `thurm --pane`.
- Remote clients, such as the iOS app and the web client, now receive compressed terminal output. This sends less data over the network.

### Fixes

- A click that closes the command palette no longer reaches the window below it.
- A click in a pane closes the tab-completion popup.
- ⌘1–⌘9 now work on keyboard layouts that need Shift for digits, such as AZERTY. Custom `[keybindings]` still apply.
- A program stopped with Ctrl+C no longer shows as working in the sidebar, even without shell integration.
- Busy remote connections, such as phones, no longer close while output flows. Only connections that send and receive nothing are closed.

### Other changes

- Building from source now needs Zig 0.17.
- The terminal engine, libghostty-vt, now uses a newer Ghostty version.

## v0.14.0 - 2026-10-27

Thurm now moves panes with a smoother, faster motion and a new `window.animation_speed` setting. A phone keeps the size of the pane it shows.

### New features

- Pane moves now use a critically damped spring. The default motion is faster than before, and zoom uses the same curve.
- The `window.animation_speed` setting adds `normal`, `slow` and `slower`. Use `slow` or `slower` for a longer motion.
- A pane shown on the phone keeps the phone's size. A layout change on the Mac no longer resizes it. A key press on the Mac takes the size back.
- The focus ring uses the theme's cursor color when the theme sets a distinct one. Themes without one keep blue.

### Fixes

- Pane moves no longer drop frames when the Mac is busy. The render server now plays each move, so the app does less work.

## v0.13.0 - 2026-10-24

This release adds an Agent Wall and an active pane border. It also fixes a bug that quit the app when a workspace lost its last tab.

### New features

- Thurm now shows a notice when the session daemon is an older build, locally or on a remote host. The notice offers a restart or an in-place upgrade, so new features work.
- Agent Wall moves every agent in a window into one grid tab, without a restart. Agents keep running and return to their tabs when you turn the wall off.
- A new option, `window.active_pane_border`, outlines the focused pane in the accent color. The option is off by default. It shows which pane has focus in a split tab.

### Fixes

- When a workspace loses its last tab, Thurm shows the most recently used workspace. Before, the app quit while other workspaces still had tabs.
- The sidebar tab shortcut, `⌘N`, now appears on the branch row only while Command is held. Long branch names use the full row width when the shortcut is hidden.
- `thurm send` now sends Enter as a separate write after the text. Claude Code no longer reads the Enter as a new line, so messages submit.

## v0.12.1 - 2026-10-24

This release fixes a macOS problem where clicks in the tab hover card did nothing.

### Fixes
- The tab hover card now takes clicks on its map and labels. Before this release, a click on a pane did nothing. Now, a click on a pane picks that pane.

## v0.12.0 - 2026-10-24

This release lets you go to a pane from the tab hover card on macOS. This release also fixes the pane header buttons and the mouse reports.

### New features

- The tab hover card on macOS now takes mouse clicks. A click on a pane in the map or the list selects its tab and focuses that pane.

### Fixes

- The split, zoom and close buttons in a pane header have a larger click area. A click near a button no longer focuses the pane by mistake.
- Mouse reports keep the correct format after a re-attach. Programs such as Textual no longer get every click at the top-left cell.

## v0.11.0 - 2026-10-22

Thurm now gives agents and scripts more control over panes. It also adds shell completions and fixes stray text in attached copies.

### New features

- Agents get the guide from the `thurm` they run, and `thurm skill setup` installs it for Claude Code, Codex, OpenCode and Pi. The CLI also gains layout commands for background panes, split sizes and cross-tab moves.
- Thurm now prints shell completion scripts for bash, elvish, fish, powershell and zsh with `thurm completion <shell>`. Your shell then completes `thurm` commands and options.
- Tab asks supported programs for their own completions, so they stay current. Bundled specs now cover sofka, vesnik, enklava, sefko, thurm, codex, mailerlite and mailersend.

### Fixes

- Attached copies no longer show stray text, such as `1m`, after you switch back to a window.
- Prompt marks in replayed zsh output now appear at the correct column.

### Other changes

- Thurm now uses a newer lock library. Locks that many threads share perform better under contention.
- To build Thurm from source, you now need Rust 1.95 or later.

## v0.10.1 - 2026-10-21

This release fixes a security advisory in a certificate library and restores the CI build.

### Fixes

- The v0.10.0 release built no artifacts because CI failed. This fix updates `rustls-webpki` to 0.103.16 (RUSTSEC-2026-0382) and makes CI pass.

## v0.10.0 - 2026-10-20

This release lets you run Thurm with a headscale coordination server. It also lets you pair a phone when HTTPS is off on your tailnet.

### New features

- Set `tailscale.control_url` to sign in to headscale or another coordination server. Set `tailscale.plain_listen` to pair a phone when HTTPS is off. The Pair iPhone sheet now checks the command-line tool and shows how to install it. A new Linux server guide and `install.sh` script install Thurm on Linux.
