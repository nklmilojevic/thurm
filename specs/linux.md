# Linux app

A desktop Thurm for Linux: the same daemon, session model, layout format and CLI as on macOS,
with a GTK 4 / libadwaita front end in place of the AppKit one.

Status: built, at feature parity with the macOS app except where the platform differs (see
"Differences"). Code: `crates/thurm-gtk`, `scripts/linux/` (VM, provisioning, install,
packaging, headless scenario tests), `linux/` (desktop entry, icon, Arch PKGBUILD).

## Architecture

`thurm-gtk` is a second client of the C ABI the Swift app uses (`thurm-ffi`, linked as a Rust
library): one connection per daemon (this machine's, and each `[[remote]]` host's through its
tunnel), panes keyed by (host, id).

| macOS (Swift) | Linux (`crates/thurm-gtk/src`) |
| --- | --- |
| `Core.swift` | `core.rs`: connections, requests (sync and async), grid, images, links, peek line |
| `SessionManager.swift`, `Workspaces.swift` | `app.rs`: connect/Hello/upgrade, restore, layout save, reconnect, workspaces, tabs, splits, events, actions |
| `SplitView.swift` (tree, divide, focus, resize, equalize) | `model.rs` (toolkit-free, unit-tested) and `tab.rs` / `splitbox.rs` (layout, dividers, zoom, dim, find bar) |
| `TerminalView.swift`, `KeyMapping.swift` | `view.rs`, `keys.rs`: keys and input methods, mouse, links, wheel and smooth scrolling, completion popup, drop and paste, overlays |
| `Renderer.swift`, `FontShaper.swift`, `GlyphAtlas.swift`, `BoxDrawing.swift` | `render.rs`, `boxdraw.rs`: cairo + pango, run shaping anchored to the grid, ligatures, Nerd Font fitting, procedural box drawing, kitty images |
| `MainMenu.swift` | `actions.rs` (one table for menus, key bindings, `[keybindings]` and the palette), `window.rs` (main menu) |
| `TerminalWindow.swift`, `WindowController.swift` | `window.rs`: header bar, libadwaita tab bar / tab view, theme-derived CSS |
| `TabSidebar.swift` | `sidebar.rs`: workspace menu, tabs grouped by repository, agents panel |
| `CommandPalette.swift`, `AgentPicker.swift` | `palette.rs` (picker), palette / switcher / agent picker / theme browser in `app.rs` |
| `ProcessPanel.swift` | `processes.rs` |
| `QuickTerminal.swift` | `quick.rs` |
| `Notifications.swift` | `notify.rs`: `gio::Notification` with Approve/Deny, waiting-agents count |
| `Remote.swift` | `remote.rs`: tunnels, offline overlay, clipboard and link policy, handoffs, Remotes window |
| `Integrations.swift` | `integrations.rs`: CLI link, agent hooks, Claude skill, daemon at login (systemd) |
| `TerminalAccessibility.swift` | `termarea.rs` (`GtkAccessibleText`) and announcements in `view.rs` |

## Differences from macOS

- **Shortcuts**: `cmd` becomes Ctrl+Shift (GNOME Terminal's convention); see `actions.rs`. In
  `[keybindings]`, `cmd+…` means Ctrl+Shift+…, `cmd+shift+…` Ctrl+Shift+Alt+…, and Linux
  combinations (`ctrl+alt+d`) work as written. Links open with Ctrl+click (macOS: Cmd).
- **Middle click** pastes the primary selection, and selections set it (Linux convention).
- **Quick terminal**: the hotkey goes through the XDG GlobalShortcuts portal where the desktop
  has one (KDE, GNOME 48+); `thurm-gtk --quick-terminal` toggles it from a desktop shortcut.
  Wayland does not let apps place windows, so it fades in where the compositor puts it instead
  of sliding from a screen edge. Its size follows `quick_terminal.screen`: `mouse` uses the
  monitor showing the main window (GTK cannot see the pointer outside its windows), `main` the
  first monitor. The hotkey hides it whenever it is shown (macOS: only when it has focus, else
  focuses it), because a Wayland compositor may not focus the window it shows.
- **Updates**: no Sparkle; packages are updated by the package manager.
- **Secure keyboard entry**: macOS-only. Linux shows the lock badge on password prompts (input
  is not observable by other Wayland clients anyway).
- **Blur**: compositor-specific (KDE only); `window.opacity` works, `window.blur` is ignored.
- **Dock badge**: the waiting-agents count goes to the Unity LauncherEntry API (Ubuntu Dock,
  Dash to Dock, KDE).
- **On-device AI** (`[ai]`): Apple Intelligence only; the Explain menu item stays hidden
  unless `ai.enabled`.
- **Daemon at login**: a systemd user unit (`thurm daemon install-systemd`), the counterpart of
  the LaunchAgent.
- **Remote installs**: the app copies its own `thurm`/`thurmd` to a Linux host of the same
  architecture when they are static (musl), which is how the packages build them.

## Building and packaging

- Releases: the `linux` job of `.github/workflows/release.yaml` builds `thurm-gtk` (glibc, on
  Ubuntu 24.04) with the static `thurm`/`thurmd` it already builds for remote hosts, all with
  one `THURM_BUILD`, and packages them with `scripts/linux/package.sh` (nfpm,
  `linux/nfpm.yaml`): `thurm_<ver>-1_<arch>.deb`, `thurm-<ver>-1.<arch>.rpm`, and copies with
  fixed names (`thurm-amd64.deb`, `thurm-x86_64.rpm`, …) for `releases/latest/download`
  links. Each is installed in an `ubuntu:24.04` / `fedora:43` container before publishing.
  Tip packages are versioned `<ver>+tip.<commit count>.<sha>`: after the release they follow,
  before the next, in dpkg, rpm and pacman alike.
- Arch / CachyOS: `linux/arch/PKGBUILD` is the AUR package `thurm` (built from the release's
  tag tarball, with Arch's `zig`). `scripts/linux/aur-update.sh VERSION AUR_DIR` updates a
  clone of the AUR repository for a release (version, checksum, `.SRCINFO`) and commits;
  pushing is manual. `scripts/linux/arch-package.sh` builds the same PKGBUILD from the
  working tree into `target/arch/`; the `Arch package` workflow runs it in an Arch container,
  and the release workflow's `arch` job publishes its result as `thurm-x86_64.pkg.tar.zst`
  (until the package is on the AUR, whose registration was closed in October 2026).
- From source on Ubuntu 24.04+ / Debian: `scripts/linux/provision.sh`, then
  `scripts/linux/install.sh` (installs to `~/.local`).
- Installed layout: `/usr/bin/{thurm-gtk,thurm,thurmd}`, the desktop entry (with New Tab, New
  Workspace and Quick Terminal actions), `/usr/share/thurm/fonts` (bundled JetBrains Mono and
  Symbols Nerd Font, registered with fontconfig at startup), `/usr/share/thurm/icons` (the
  Adwaita symbolic icons the app uses, a hicolor fallback for icon themes without them, such as
  KDE's), `/usr/share/thurm/skills`.

## Testing

- Unit tests: `cargo test -p thurm-gtk` (split tree, layout conversion, focus by direction,
  equalize, names, paths, fuzzy matching, URLs, key bindings, box drawing, accessibility).
- Headless scenarios (Xvfb, xdotool, screenshots):
  - `scripts/linux/smoke.sh BIN OUT`: rendering (styles, ligatures, CJK, box drawing, Nerd Font,
    kitty image), splits, tabs, palette, zoom, find, quit and restore.
  - `scripts/linux/smoke-features.sh BIN OUT`: agent status, link hover, tab completion,
    workspaces and switcher, theme preview, Processes & Ports, quick terminal.
  - `scripts/linux/smoke-remote.sh BIN OUT SSH_TARGET`: Remotes window, install, tunnel,
    handoff, remote workspace, offline notice.
- A GNOME desktop VM on a Mac (Tart): `scripts/linux/vm.sh create|build|up|launch`.
- CI: the `linux-app` job builds, tests, lints and runs `smoke.sh` on x86_64 and aarch64.

## Shared code to move out of Swift

Both front ends now implement the session layer (workspaces, restore, reconnect, remote flows)
separately. Moving it into a Rust crate both call would keep them from drifting.
