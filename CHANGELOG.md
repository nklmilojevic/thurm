# Changelog

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
