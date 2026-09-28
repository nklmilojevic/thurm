# Thurm

[Documentation](https://nklmilojevic.github.io/thurm/) includes installation, user guides, configuration, and CLI commands.

Thurm is a macOS terminal with tabs, split panes, and support for coding agents.
It uses libghostty-vt for terminal state, AppKit for the interface, and Metal and
CoreText to draw text and images.

The `thurmd` daemon runs the shells. By default, they keep running when you quit
the app. Open Thurm again to return to your session.

Thurm includes:

- Native tabs, a tab sidebar grouped by repository, and workspaces.
- Split panes with keyboard controls for focus, size, and zoom.
- Font ligatures, themes, and support for the kitty keyboard and graphics protocols.
- Shell integration for zsh, bash, and fish, with prompt marks and working directory tracking.
- Coding agent status, launch presets, and notifications when an agent needs input.
- A command-line tool to open panes, send input, capture output, and wait for results.
- Secure Keyboard Entry, with automatic activation when a password prompt is detected.
- Optional on-device Apple Intelligence features, off by default. See `[ai]` below.

To build the app, use an Apple silicon Mac with macOS 14 or later and Xcode 26 or
later. [flake.nix](flake.nix) provides everything else (Rust, Zig 0.16, Python, gh). The
build downloads the pinned Ghostty source and builds libghostty-vt, so the first build
needs network access.

Run these commands from the repository root (or `direnv allow` once, and skip
`nix develop`):

```sh
nix develop
./macos/build.sh
open macos/build/Thurm.app
```

Without Nix, put Rust stable (with the `aarch64-apple-darwin` target) and Zig 0.16.0 on
`PATH`. Thurm runs on Apple silicon only.

The script signs the app with an ad hoc signature by default. Releases, update channels and
signing are described in [macos/README.md](macos/README.md#releases-and-updates).

To build, install, and open the app:

```sh
./macos/build.sh --install
```

This replaces Thurm in `/Applications`, or in `~/Applications` if `/Applications`
is not writable. It also links the `thurm` command into `~/.local/bin` if that
directory exists and the destination is not a regular file. You can also select
**Thurm > Integrations > Install Command-Line Tool**. Make sure the command's
directory is on your `PATH`.

Use these shortcuts to start:

| Action | Shortcut |
| --- | --- |
| New tab | Cmd+T |
| New window | Cmd+N |
| Split right | Cmd+D |
| Split down | Cmd+Shift+D |
| Zoom split | Cmd+Shift+Return |
| Close pane | Cmd+W |
| Command palette | Cmd+Shift+P |
| Switch workspace | Cmd+Shift+O |
| Switch to an agent | Cmd+Shift+A |
| Open settings | Cmd+, |
| Reload configuration | Cmd+Shift+, |

Thurm reads `~/.config/thurm/config.toml` by default. Use `thurm config-path` to
find the active file. All settings are optional. The
[example configuration](config.example.toml) includes every option, its default,
and examples of optional settings. Copy it to your active configuration path,
or copy only the settings you need.

For example:

```toml
[font]
size = 14.0
ligatures = true

[window]
tab_style = "sidebar"

[colors]
theme = "light:catppuccin-latte,dark:catppuccin-mocha"

[session]
quit = "detach"
```

To use Apple Intelligence's on-device model, set `enabled = true` under `[ai]`. This
requires macOS 26 or later, Apple silicon, and Apple Intelligence turned on in System
Settings. Screen text never leaves the Mac, and Thurm skips the model while a password
prompt is active. When enabled, Thurm names agent tabs that have no session title, detects
agents that ask a question no pattern matches, describes agent requests and finished turns
in notifications, and explains the last command with **Edit > Explain Last Command** or
`thurm explain`. You can turn each feature off separately.

Run `thurm reload` after you edit the file. You can also change settings from the CLI:

```sh
thurm set font.size 14.0
thurm theme nord
```

The daemon saves the layout, working directories, and scrollback under
`~/Library/Application Support/Thurm`. By default, it saves every 15 seconds and
keeps up to 4,000 scrollback lines per pane. Run `thurm save` to save a snapshot now.

After a reboot or daemon restart, Thurm restores the saved state and starts new
processes. It cannot restore the memory of a process that stopped. It can run
configured agent resume commands when `session.resume_agents` is enabled, which
is the default. Closing a pane stops its process; quitting the app leaves shells
running unless `session.quit` is set to `"terminate"`.

Use the CLI from a Thurm pane to control your session. This example opens a shell
in a split, sends a command, waits for its output, and reads the last 20 lines:

```sh
pane=$(thurm split --dir right)
thurm send --pane "$pane" 'echo THURM_READY'
thurm wait --pane "$pane" --match '^THURM_READY$' --timeout 10
thurm capture --pane "$pane" -n 20
```

`thurm send` adds Enter unless you pass `--no-enter`. Commands with an optional
`--pane` use the current pane by default through `THURM_PANE_ID`. Use `thurm list`
to find pane IDs and `--json` for JSON output. A wait timeout returns exit code 124.

For coding agents, start with:

```sh
thurm presets
thurm agents
thurm hooks status
```

Thurm detects supported agent processes. For Claude Code and Codex, hooks provide
turn completion events and session IDs. Run `thurm hooks install` to add hooks to
agent configuration files. Use `--agent claude` or `--agent codex` to select one.
Run `thurm hooks uninstall` to remove them. Installed agent tools are required for
launch presets.

Use `thurm --help` or `thurm <command> --help` for all options. The
[Thurm agent skill](skills/thurm/SKILL.md) describes how agents can use the CLI.

The source is split into these parts:

| Path | Purpose |
| --- | --- |
| `macos/` | Swift app, rendering, and app build script. |
| `crates/thurm-daemon/` | Shell processes, panes, agent tracking, and saved sessions. |
| `crates/thurm-term/` | Terminal state, graphics, input encoding, and scrollback. |
| `crates/thurm-config/` | Settings, themes, and agent definitions. |
| `crates/thurm-proto/` | Messages and layout data shared by clients and the daemon. |
| `crates/thurm-client/` | Rust client library. |
| `crates/thurm-ffi/` | C interface used by the Swift app. |
| `crates/thurm-cli/` | The `thurm` command. |
| `shell-integration/` | Shell scripts included in the daemon. |
| `vendor/libghostty-vt-sys/` | Rust bindings and build code for the pinned Ghostty source. |

CI runs the following Rust checks on macOS. Zig 0.16.0 must be available
on `PATH`. The macOS app is built separately with `./macos/build.sh`.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
