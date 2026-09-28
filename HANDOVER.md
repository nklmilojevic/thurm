# Thurm — handover

This document picks up where the first (Linux, cloud) session left off, for continuing on a
Mac. Read this first, then `README.md` for the user-facing overview.

## Where we started

The repository was empty (a one-line README). The goal:

> A modern terminal based on Alacritty, with native macOS tabs, splits, secure input like
> Ghostty, session persistence like [tty7](https://github.com/l0ng-ai/tty7) but without GPUI,
> kitty terminal support, agent support and ligatures.

Decisions made in the interview:

| Question | Answer |
|---|---|
| App shell | **Swift/AppKit front end + Rust core** (Ghostty-style split), C ABI between them |
| Renderer | **Swift: Metal + CoreText** (Rust sends a resolved cell grid, Swift shapes and draws) |
| Kitty | **Keyboard protocol, graphics protocol, and extras** (OSC 99, OSC 52, OSC 8, undercurl) |
| Agents | **Detect + status, control CLI, launch presets** |
| Persistence | **Daemon + snapshot**: a daemon owns the PTYs; layout/cwd/scrollback saved to disk |
| Name | **Thurm** (binaries `thurmd`, `thurm`; bundle id `com.thurm.terminal`; `~/.config/thurm`) |

The first session ran on Linux with no Swift toolchain, which shapes everything below: **all
Rust code is built and tested; none of the Swift code has ever been compiled.**

## Architecture in one picture

```
Thurm.app (Swift)  ──C ABI──▶ libthurm_ffi.a ──unix socket (postcard)──▶ thurmd (Rust)
  windows, tabs, splits,        grid cache,                               PTYs, libghostty-vt,
  Metal/CoreText renderer,      JSON bridge                               kitty graphics, OSC, keys,
  secure input, palette                                                   agents, persistence
                                              thurm CLI ──same socket──▶
```

* The daemon is the source of truth for every terminal. The app is a thin client: it sends
  keys/mouse/resizes, receives incremental **frames** (changed rows only, colors already
  resolved), and renders them.
* Key and mouse *encoding* happens in the daemon (it knows the exact terminal mode), so the
  app only reports what the user pressed.
* The layout (windows → tabs → split tree → pane ids) is owned by the app and stored in the
  daemon (`SetLayout` / `GetLayout`), which also writes it to disk.

## What is done

### Rust — built, tested (78 tests), clippy `-D warnings` clean, type-checks for `aarch64-apple-darwin`

| Crate | Contents | Confidence |
|---|---|---|
| `thurm-proto` | Requests/responses/events, frames, key/mouse types, layout model (JSON, internally tagged), length-prefixed postcard codec | Tested |
| `thurm-config` | TOML config with defaults + commented default file, 9 themes, 14 agent definitions (process/argv match, "working"/"attention" screen patterns, launch + resume commands), socket/state/config paths | Tested |
| `thurm-term` | Engine around libghostty-vt (via `libghostty-vt-sys`): stream filter that pulls APC (kitty graphics) and OSC 7/9/99/133/633/777/1337 out of the PTY stream; OSC parsing (cwd, notifications incl. multi-part kitty OSC 99, progress, prompt marks); color resolution; per-client frame diffing via row hashes; selection; regex search with highlight; prompt jumping; capture; ANSI scrollback serialization + replay; kitty graphics placements anchored to cells (they scroll with text and die when erased); key/mouse/wheel/focus encoding (legacy xterm + all kitty keyboard flags) | Tested, incl. kitty image place/scroll/delete |
| `thurm-daemon` (`thurmd`) | PTY spawning (login(1) on macOS, like Terminal.app), shell integration injection (zsh ZDOTDIR, bash --rcfile, fish XDG_DATA_DIRS), per-pane reader/writer threads, 120 Hz frame pump with sync-update (mode 2026) handling and slow-client backpressure, monitor loop (foreground process, agent status, password prompt via termios ECHO/ICANON, cwd, titles, notifications, autosave), snapshot/restore incl. agent resume, `wait` conditions, same-uid socket check | End-to-end tested on Linux (real shells, restart + restore) |
| `thurm-client` | Blocking client used by CLI and FFI, spawns the daemon if absent | Tested |
| `thurm-ffi` | Static lib implementing `include/thurm.h`: JSON request bridge, grid cache with dirty-row mask, image cache, key code mapping | Unit tested; never linked into Swift |
| `thurm-cli` (`thurm`) | list, info, new-tab, split, launch, presets, send, send-keys, capture, wait (exit 124 on timeout), agents, focus, close, title, scroll, clear, notify, layout, reload, save, daemon status/start/stop | Tested + manual smoke test |

Also: `shell-integration/` (zsh, bash, fish; embedded into the daemon at build time),
`skills/thurm/SKILL.md` (teaches coding agents to use the CLI), `.github/workflows/ci.yml`
(Rust tests + clippy on Linux and macOS).

macOS-specific Rust paths that have only been **type-checked, never run**: `procinfo.rs`
(proc_pidpath, KERN_PROCARGS2, PROC_PIDVNODEPATHINFO), `getpeereid` in `server.rs`, the
`/usr/bin/login` wrapper in `shell.rs`, `openpty` in `pty.rs`.

### Swift — written (~6,300 lines), reviewed twice by reading, **never compiled**

`macos/Package.swift` (SwiftPM, tools 5.10, macOS 14), `macos/build.sh` (builds Rust for both
Apple targets, lipo, `swift build`, assembles and ad-hoc signs `macos/build/Thurm.app`),
`macos/Resources/Info.plist`, and `macos/Sources/Thurm/`:

| File | Role |
|---|---|
| `Core.swift` | FFI wrapper, C callbacks → main queue, per-connection token |
| `Models.swift`, `Config.swift` | JSON models, mirrored C constants, UI config |
| `SessionManager.swift` | Launch/restore from layout, layout saving, daemon events, CLI `Ui` commands, reconnect |
| `WindowController.swift` | Native tabs (`tabbingMode = .preferred`), agent status dot as tab accessory |
| `SplitView.swift` | Split tree, dividers, zoom, spatial focus, resize, equalize |
| `TerminalView.swift` | CAMetalLayer view, display link, NSTextInputClient/IME, keys, mouse, wheel, links, paste confirm, lock badge |
| `KeyMapping.swift` | macOS virtual key codes → Thurm keys, Option-as-Alt |
| `Renderer.swift`, `FontShaper.swift`, `GlyphAtlas.swift`, `BoxDrawing.swift` | Metal renderer (runtime-compiled shaders), CoreText shaping with ligatures mapped to cells, glyph atlases (gray + color emoji), procedural box drawing |
| `SecureInput.swift` | Ghostty-style secure input: manual toggle + automatic on password prompts |
| `CommandPalette.swift`, `FindBar.swift`, `Notifications.swift`, `MainMenu.swift`, `AppDelegate.swift` | Palette (actions + agent presets), find, UNUserNotifications, menus |

## What needs to be done next (in order)

1. **Get it compiling on macOS.**
   ```sh
   rustup target add aarch64-apple-darwin x86_64-apple-darwin
   cargo test --workspace            # should be green on macOS too; fix anything platform-specific
   ./macos/build.sh                  # expect a handful of Swift compile errors
   ```
   Likely trouble spots flagged during review: `override` on `selectAll(_:)` /
   `newWindowForTab(_:)`, Int/UInt/CGFloat conversions, CoreText CF bridging, Metal optionals,
   shifted-symbol menu shortcuts (⇧⌘[ ⇧⌘] ⇧⌘,). For quick iteration after the first full
   build: `SKIP_RUST=1 ./macos/build.sh`.
2. **First run.** `open macos/build/Thurm.app`. The app spawns `thurmd` from its bundle.
   Logs: `~/Library/Application Support/Thurm/thurmd.log` (`THURM_LOG=debug` for more).
   Useful while debugging: run the daemon in a terminal with
   `cargo run -p thurm-daemon -- --foreground` and poke it with `cargo run -p thurm-cli -- list`.
3. **Verify each feature by hand:**
   * Tabs: ⌘T, drag a tab out and back, ⌘1…9, tab titles and the agent dot.
   * Splits: ⌘D, ⌘⇧D, ⌘⌥arrows, divider drag, ⌘⇧↩ zoom, closing the last pane of a tab.
   * Rendering: ligatures (JetBrains Mono / Fira Code: `=> != -> ===`), emoji and ZWJ
     sequences, CJK, box drawing (`htop`, `tmux`), undercurl (`printf '\e[4:3mcurl\e[0m'`),
     Retina, resizing, font size ⌘+/⌘−.
   * Input: IME (Japanese/Chinese), dead keys, Option-as-Alt, `kitty +kitten show_key -m kitty`
     or `nvim` with the kitty keyboard protocol on.
   * Kitty graphics: `kitty +kitten icat image.png`, `yazi` previews, `timg -pk`.
   * Secure input: `sudo -k; sudo true` should show the lock badge; the menu toggle.
   * Persistence: quit the app, relaunch (same shells, still running); `thurm daemon stop`, then
     relaunch (layout + scrollback + cwd restored, fresh shells); reboot.
   * Agents: run `claude` in a pane; tab dot goes Working → Idle; a permission prompt should
     flip it to NeedsInput and post a notification when the pane isn't focused.
     `thurm agents`, `thurm split -- htop`, `thurm wait --agent-free`.
4. **Known gaps to implement:**
   * Kitty graphics: animation (`a=f/a/c`), Unicode-placeholder / virtual placements (`U=1`),
     relative placements (`P/Q/H/V` are parsed but placed at the cursor), shared-memory
     transmission (`t=s`).
   * Renderer: `font.thicken`, `window.blur`, rounded box corners, dashed box lines,
     double-line junctions (approximated), emoji wider than two cells, gamma-correct blending,
     atlas upload synchronization with in-flight frames.
   * Layout: tab reorder is detected by a 3 s poll; left/up splits restored from JSON don't
     swap children; closing the last window doesn't quit (decide on the desired behavior).
   * App icon, code signing/notarization, Sparkle-style updates, a launchd agent option for
     the daemon.
   * Performance: CoreText row cache clears wholesale at 8,192 entries; clusters are fetched
     with one FFI call per non-blank cell in dirty rows — batch this if profiling shows it.
5. **Nice to have (tty7 parity ideas):** SSH/remote panes, process/port panel, per-repo branch
   and diff info on agent tabs, agent forking.

## Conventions and pointers

* Protocol changes: edit `crates/thurm-proto`, bump `PROTOCOL_VERSION` when incompatible. JSON
  shapes the Swift side depends on are pinned by `crates/thurm-ffi/src/lib.rs` tests
  (`request_json_shapes`, `key_codes_match_header`) — keep `thurm.h` and those tests in sync.
* Config: every field has a default and unknown keys are rejected
  (`crates/thurm-config/src/lib.rs`); document new options in `default_config.toml`.
* Environment overrides for testing: `THURM_SOCKET`, `THURM_STATE_DIR`, `THURM_CONFIG_DIR`,
  `THURM_LOG`. Panes get `THURM_PANE_ID` and `THURM_SOCKET`.
* Engine tests: `crates/thurm-term/src/terminal/tests.rs`; daemon end-to-end:
  `crates/thurm-daemon/tests/e2e.rs`.
* State on disk: `~/Library/Application Support/Thurm/` (`session.json`, `scrollback/*.ansi`
  written 0600, `shell-integration/`, `thurmd.log`); socket in `$TMPDIR`-independent
  `/tmp/thurm-<uid>/thurmd.sock` (dir 0700).
