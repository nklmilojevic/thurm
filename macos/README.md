# Thurm for macOS

The native macOS front end of Thurm. It is a thin client: every PTY and all terminal state live
in the session daemon `thurmd`. The app connects through the Rust core (`libthurm_ffi.a`, C API
in `crates/thurm-ffi/include/thurm.h`), sends input, gets screen grids back and renders them
with Metal and CoreText. Quitting the app leaves your shells running (unless
`session.quit = "terminate"`), and the next launch restores windows, tabs and splits.

## Requirements

- An Apple silicon Mac with macOS 14 (Sonoma) or newer. Thurm is built for arm64 only.
- Xcode 26 or newer
- Rust (edition 2024, 1.88+) and Zig 0.16: `nix develop` (or `direnv allow`) provides both,
  from the repository's `flake.nix`

## Build

```sh
./macos/build.sh
open macos/build/Thurm.app
```

`build.sh`:

1. runs `cargo build --release -p thurm-ffi -p thurm-daemon -p thurm-cli --target
   aarch64-apple-darwin` (`libthurm_ffi.a`, `thurmd`, `thurm`). `SKIP_RUST=1` reuses existing
   artifacts;
2. copies `thurm.h` into `Sources/CThurm/include/` (a copy, not a symlink, so SwiftPM sees it);
3. runs `swift build -c release --arch arm64` (SwiftPM fetches Sparkle);
4. assembles `macos/build/Thurm.app`: `Contents/MacOS/Thurm`, `Contents/Helpers/{thurmd,thurm}`,
   `Contents/Frameworks/Sparkle.framework`, resources, and `Info.plist` with the version,
   build number and (release builds only) the update feed;
5. signs the bundle: ad hoc, or with `THURM_SIGN_IDENTITY` (inside out, hardened runtime).

To work on the Swift code, build once with the script, then open `macos/Package.swift` in
Xcode or run `swift build` in `macos/` (set `THURM_LIB_DIR` if the Rust library lives somewhere
other than `../target/aarch64-apple-darwin/release`). A bare `swift run` works for rendering and input,
but desktop notifications need the bundled app, and `thurmd` is only spawned when it sits
next to the executable (or is named by `$THURM_DAEMON`).

## Releases and updates

`.github/workflows/release.yml` builds, signs (Developer ID), notarizes and publishes Thurm.
Users get updates through [Sparkle](https://sparkle-project.org) from one appcast on GitHub
Pages, in two channels:

| Channel | Gets | Published by |
| --- | --- | --- |
| **Release** (default) | tagged releases | `gh release create vX.Y.Z` (the tag must match `version` in `Cargo.toml`) |
| **Tip** | every commit to `main` | each push to `main` once CI passes; rolling `tip` prerelease |

Users switch with *Thurm › Update Channel*. Tip items carry `<sparkle:channel>tip`, so a
release-channel app never sees them. `CFBundleVersion` is the commit count, so it grows across
both channels. A tip build and a release of the same commit have the same number, so neither
replaces the other.

The appcast is regenerated on every publish from the `sparkle.json` asset of each release
(`macos/release/appcast.py`), so it holds no state of its own.

**The daemon across updates.** Sparkle replaces the bundle and relaunches the app. The new app
sees that `thurmd` reports another build (`Hello.build`) and upgrades it in place. It writes
the new binary's path to `<socket>.upgrade` and sends SIGUSR2. The daemon asks the new binary
whether it reads its hand-off format (`--handoff-check`), then saves the session and execs the
new binary in the same process. It passes the PTY masters and the listening socket through
exec, and the terminal state (both screens, modes, kitty flags) in a hand-off file. Shells and
running programs never notice. A daemon from before this mechanism (protocol < 13) still
needs the old restart, which restores layout and scrollback but not processes.
`thurm daemon upgrade` does the same from the command line.

**One-time setup** (your own Apple Developer account):

1. A *Developer ID Application* certificate exported as `.p12`, and an App Store Connect API
   key (`.p8`, Developer access) for notarization, stored in 1Password.
2. Run, with your 1Password references:

   ```sh
   macos/release/setup-github.sh \
       --certificate 'op://Private/Thurm Developer ID/certificate.p12' \
       --certificate-password 'op://Private/Thurm Developer ID/password' \
       --notary-key 'op://Private/Thurm Notary/AuthKey.p8' \
       --notary-key-id 'op://Private/Thurm Notary/key id' \
       --notary-issuer 'op://Private/Thurm Notary/issuer id'
   ```

   This sets the repository secrets `MACOS_CERTIFICATE_P12`, `MACOS_CERTIFICATE_PASSWORD`,
   `NOTARY_API_KEY_P8`, `NOTARY_API_KEY_ID`, `NOTARY_API_ISSUER_ID` and `SPARKLE_PRIVATE_KEY`,
   and the variable `SPARKLE_PUBLIC_KEY`. It creates the Sparkle EdDSA key in your login
   keychain (back it up: `generate_keys --account thurm -x FILE`; losing it strands every
   installed copy). It also switches GitHub Pages to deploy from Actions.
3. Optional variables: `SPARKLE_FEED_URL` (defaults to
   `https://<owner>.github.io/<repo>/appcast.xml`; pin it before renaming the repository) and
   `MACOS_SIGN_IDENTITY`.

**A local release build** (same as CI, with your login keychain and a notarytool profile):

```sh
export THURM_SIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)"
export NOTARY_PROFILE=thurm-notary   # xcrun notarytool store-credentials thurm-notary
export THURM_VERSION=0.2.0 THURM_BUILD_NUMBER="$(git rev-list --count HEAD)"
export THURM_FEED_URL=https://<owner>.github.io/<repo>/appcast.xml
export THURM_SPARKLE_PUBLIC_KEY=… SPARKLE_KEY_FILE=… THURM_DOWNLOAD_BASE=…
macos/release/build-release.sh release
```

To check the DMG layout without a certificate: `THURM_SIGN_IDENTITY=- ./macos/build.sh` and
`./macos/package-dmg.sh --preview`.

## Structure

```
macos/
├── Package.swift            SwiftPM manifest (macOS 14, Swift tools 5.10)
├── build.sh                 Rust + Swift build, bundle assembly, signing
├── Resources/Info.plist     bundle id com.thurm.terminal
└── Sources/
    ├── CThurm/              C module: module.modulemap (+ include/thurm.h copied by build.sh)
    └── Thurm/
        ├── main.swift            NSApplication bootstrap
        ├── AppDelegate.swift     app lifecycle, app-level actions
        ├── MainMenu.swift        programmatic main menu and shortcuts
        ├── Core.swift            FFI wrapper: connect, requests, callbacks → main queue
        ├── Models.swift          JSON helpers, PaneInfo, Layout (serde shapes), constants
        ├── Config.swift          UI config + theme from thurm_config_json()
        ├── SessionManager.swift  windows/tabs, pane lifecycle, layout save/restore,
        │                         daemon events, reconnect, command palette entries
        ├── WindowController.swift one native tab = one NSWindow; agent status dot
        ├── SplitView.swift       split tree (mirrors LayoutNode) and TabContentView
        ├── TerminalView.swift    CAMetalLayer view, NSTextInputClient, mouse, scroll
        ├── KeyMapping.swift      virtual key codes → Thurm keys, Option-as-Alt
        ├── Renderer.swift        Metal pipeline (shader compiled at runtime), grid snapshot
        ├── FontShaper.swift      CoreText fonts, metrics, ligature-aware shaping
        ├── GlyphAtlas.swift      R8 / BGRA glyph atlases, CoreGraphics rasterization
        ├── BoxDrawing.swift      procedural box drawing and block elements
        ├── SecureInput.swift     balanced Enable/DisableSecureEventInput
        ├── CommandPalette.swift  Cmd+Shift+P palette
        ├── FindBar.swift         Cmd+F scrollback search
        └── Notifications.swift   UNUserNotificationCenter
```

### How it works

- **Connection.** At launch the app calls `thurm_connect` with the bundled `thurmd` (spawned
  when no daemon runs), sends `Hello`, then `GetLayout` and `ListPanes`, and rebuilds windows,
  tabs and splits. Panes that exist but are missing from the layout open as tabs of one extra
  window. After every layout change (debounced) and every 3 s when something changed, the
  layout is stored with `SetLayout`. If the daemon connection drops, the app reconnects with
  backoff (respawning the daemon), resubscribes and rebuilds from the layout if needed.
- **Rendering.** Each pane view owns a `CAMetalLayer`. A per-view `CADisplayLink` checks, once
  per display refresh, whether the Rust core reported a new frame (or the cursor blinks, the
  bell flashes...). Only then it locks the grid, copies the cells, unlocks, and draws
  instanced quads: backgrounds, glyphs from the atlases, decorations (underline styles and
  undercurl computed in the fragment shader), cursor, kitty images and overlays.
- **Shaping.** Every row is split into runs of equal style, shaped with CoreText (ligatures,
  OpenType features from `font.features`, automatic font fallback for emoji / CJK / symbols),
  and every glyph is snapped to the grid column of its cluster, so ligatures span cells and
  the grid never drifts. Shaped rows are cached by content.
- **Tabs** are native NSWindow tabs (`tabbingIdentifier = "Thurm"`); AppKit state restoration
  is disabled because the daemon's layout is the source of truth.
- **Secure Keyboard Entry** can be toggled globally (app menu, ⌥⌘I) and turns on
  automatically while the focused pane shows a password prompt
  (`security.auto_secure_input`). It is released whenever the app is not frontmost.

### Shortcuts

| Action | Shortcut |
| --- | --- |
| New tab (inherits cwd) / new window | ⌘T / ⌘N |
| Close pane / close tab | ⌘W / ⇧⌘W |
| Select tab 1–9, previous / next tab | ⌘1…⌘9, ⇧⌘[ / ⇧⌘] |
| Split right / down | ⌘D / ⇧⌘D |
| Move focus between splits | ⌥⌘ + arrows |
| Move divider | ⌃⌘ + arrows |
| Zoom split / equalize splits | ⇧⌘↩ / ⌃⌘= |
| Copy / paste / select all | ⌘C / ⌘V / ⌘A |
| Clear scrollback | ⌘K |
| Find / next / previous | ⌘F / ⌘G / ⇧⌘G (in the bar: ↩ older, ⇧↩ newer, esc closes) |
| Font size | ⌘+ / ⌘− / ⌘0 |
| Command palette | ⇧⌘P |
| Open / reload config | ⌘, / ⇧⌘, |
| Secure Keyboard Entry | ⌥⌘I |
| Open hyperlink / URL | ⌘-click |
