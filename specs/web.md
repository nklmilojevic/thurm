# Thurm in the browser

A web client that attaches to `thurmd` from any browser on the user's tailnet: the same panes the
Mac and the iPhone show, live, at `https://<node>.<tailnet>.ts.net`. Typing in one follows in the
others. Inspired by Tern's `tern web serve` (a wasm client on WebGPU attached to the same daemon
session as its desktop and iPhone apps), without its `?token=` URL: the tailnet already says who
is asking.

Status: built, in review: nklmilojevic/ghostty-vt-rs#2 (merged; the wasm32 target), #167 (the
core builds for wasm32), #169 (the client, serving, CI and release). Checked end to end in
Chromium with Playwright, against a real `thurmd` behind a test gateway; not yet on a real
tailnet node. What was checked while writing this is in [Verified](#verified).

Where the build differs from the design below:

- The worker is a clock only. The WebSocket and the wasm session stay on the main thread: a
  keydown has to know at once whether the terminal took it (to prevent the browser's default),
  and the worker's ticks still reach a hidden tab.
- Events and requests cross into the page as serde JSON; only cells are a `Uint32Array`, and
  only the rows that changed. A row shift repaints every row.
- The reload rule compares the page's own script URL with the one the node's `index.html` names
  now, rather than `Hello`'s `build`.
- ghostty has no kitty graphics on freestanding targets (it needs a clock), so `thurm-term`
  calls stand-ins there; images were a non-goal anyway.
- The Host check applies to browser requests only (pages, and upgrades with an `Origin`): apps
  keep connecting whatever name or port form they use.
- Size: the wasm is 392 KiB brotli (505 KiB gzip), the whole first load 474 KiB brotli. Most of
  the module is `thurm-term` and `thurm-config`'s embedded themes.
- Option-as-Alt on a Mac and Thurm's own box drawing are not in v1; the nix package serves the
  placeholder page (the bundle needs Zig and ghostty's sources in the sandbox).

## Goals

- Attach to the host's daemon from a browser: list workspaces and tabs, view panes, type,
  scroll back, select and copy.
- Agent notices: which panes need input, the notice list, Approve / Deny on permission prompts.
- The same session as the other clients: every pane subscriber sees the same terminal; the
  device that typed last sets the size (`active-size`).
- Served by the `thurm-tailnet` node that already serves `/v1/connect`, admitted by the same
  Tailscale identity check. No tokens, no passwords, no cookies.
- Current Safari, Chrome and Firefox on desktop. Phones and tablets load it, but are not tuned for.

## Non-goals (v1)

- Editing layouts: no new tabs, splits, closing or moving panes. The layout is shown as it is.
- Settings, themes and config editing. The one local preference is font family and size.
- File previews, kitty graphics and other images, `thurm handoff`.
- Several hosts in one page. A page talks to the daemon of the node that served it (the origin
  check below makes that a rule, not a limit of v1).
- Web Push and service-worker notifications when no tab is open. The phone covers that.
- Serving over `host_listen` (plain `ws://` on a tailnet IP: no TLS, so no secure context) or
  over `tailscale serve` (see Open questions).
- Offline use or installing as a PWA.

## Architecture

```
 browser (https://thurm.tail1234.ts.net)            host (Mac or Linux)
┌─────────────────────────────────────┐           ┌───────────────────────────────────┐
│ index.html + app.js (TypeScript)    │  GET /    │ thurm-tailnet (tsnet node, :443)  │
│  DOM: input, IME, clipboard, UI     │──────────▶│  gateway: Identify → admit.Policy │
│  Canvas2D renderer                  │           │   /           static bundle       │
│ thurm_web.wasm (Rust)               │  wss      │   /v1/connect WebSocket ─┐        │
│  thurm-proto (postcard frames)      │──────────▶│                          ▼        │
│  thurm-term + libghostty-vt         │ /v1/connect  thurmd.sock ─ thurmd (PTYs)     │
└─────────────────────────────────────┘           └───────────────────────────────────┘
```

- The browser is a thin client like the iPhone: `Hello` with `CAP_MOBILE`, `Subscribe` a pane,
  replay `Event::Attach` into a local libghostty terminal, then `Output` into it. The daemon's
  copy stays authoritative.
- The transport is the existing `/v1/connect` WebSocket. The gateway already relays it to
  `thurmd.sock` with the caller's `Principal` in the ingress preamble; nothing about it changes
  except the origin check.
- The bundle is embedded in the `thurm-tailnet` binary, so the page and the daemon that serves it
  come from the same build.

## What already exists

- **Gateway** (`tailnet/internal/gateway/gateway.go`). `ServeHTTP` answers `/v1/connect` only,
  404 elsewhere: `Identify(r)` (Tailscale `WhoIs` on the remote address) → `Policy().Check`
  (`internal/admit`) → dial the daemon → ingress preamble → `websocket.Accept(w, r, nil)` →
  binary messages piped to the socket with `io.Copy`. Binary messages are not aligned to frames:
  the Rust client writes up to 64 KiB per message.
- **Origin.** `Accept` with nil options uses coder/websocket's default check: no `Origin` header
  passes, an `Origin` whose host equals the request's `Host` passes, any other origin is refused.
  The comment there ("Clients are apps, not browsers… a browser page can't reach this") holds
  for cross-origin pages only. The check runs after the daemon dial, so a refused origin still
  costs one.
- **Admission** (`admit.Policy`): the node's owner, or every identified caller with
  `allow_tailnet_members`. Tagged callers are never the owner. Admission is the only gate: an
  admitted caller has the whole protocol, like a local peer. `specs/tailnet-remotes.md` covers
  the policy and its open questions (a per-login allow list); this spec adds no rule of its own.
- **Protocol** (`thurm-proto`): `u32` little-endian length + postcard payload per frame.
  postcard has no field tags, so new data goes in new variants at the end of an enum and a client
  skips a message it can't decode. `PROTOCOL_VERSION` 18, `MIN_COMPATIBLE_PROTOCOL` 18,
  `hello_compatible` also wants `active-size` from the daemon.
- **The thin-client path** (`thurm-ffi`, `mobile` feature, and `thurm-tailnet-client`):
  - `Hello { client, version, ui: false, capabilities: [CAP_MOBILE] }`. A mobile client gets
    `Notify`, `PaneInfo` and `PaneRead` but no `Ui` commands, does not count as a window, and is
    dropped after 45 s of silence (`MOBILE_SILENCE`), so it sends `Ping`.
  - `Subscribe { pane }` → `Event::Attach { size, state }`, then `Output` / `Resized`. `state` is
    `Terminal::serialize_state`: the whole scrollback and screen as a VT stream, both screens,
    modes, kitty keyboard flags, palette, title, prompt marks. Scrollback arrives here; there is
    no separate request. The daemon may send `Attach` again at any time (a client that fell
    behind is resynced); the client then starts its terminal over.
  - Keys: with a local copy of the pane, the ffi encodes with libghostty's key encoder
    (`thurm_term::Terminal::key`) and sends `Input`. Without one it sends `Key` and the daemon
    encodes. Mouse and wheel work the same way. `Paste { text }` is bracketed by the daemon.
  - `AnswerPermission { pane, prompt, allow }` answers an agent prompt. Focus, input and answers
    mark a pane read (`pane_read` → `Event::PaneRead`), which takes its notices down everywhere.
- **libghostty-vt** comes from `ghostty-vt-rs-sys` 0.1 (crates.io, the `ghostty-vt-rs`
  repository), pinned to ghostty `1e9e14af`, prebuilt for the Apple and Linux targets. Its
  `build.rs` knows no wasm target.

## Rendering: libghostty-vt in wasm

Ghostty upstream already builds libghostty-vt for the browser:

- `zig build -Demit-lib-vt -Dtarget=wasm32-freestanding -Doptimize=ReleaseSmall` emits
  `zig-out/bin/ghostty-vt.wasm` (`GhosttyLibVt.initWasm`: no entry point, exported function
  table for callbacks, 128 KiB stack, a patch step to make the table growable) **and**
  `zig-out/lib/libghostty-vt.a` for wasm32. `example/wasm-vt` and `example/wasm-key-encode` use
  the module from plain JavaScript; `include/ghostty/vt/wasm.h` adds allocation helpers for
  hosts.
- The module exports the full C API Thurm uses natively, render state, key and mouse encoders,
  selection, search, paste encoding and snapshots among it, and imports nothing.

The client links the **static archive** into the Rust module, so libghostty-vt and thurm-term
are one wasm module, called from Rust as on every other target. No JS glue for ghostty, no
second module, no copying cells between modules.

The renderer itself is TypeScript, drawing what the Rust side hands it:

- **Canvas2D first.** Per frame, Rust walks libghostty's render state (dirty rows only, as the
  Mac renderer does) and fills a cell buffer in linear memory: per cell the grapheme, foreground
  and background, attribute bits; per frame the cursor, selection and viewport offset. TS reads
  it as typed arrays (re-acquired after each call, since linear memory may grow), paints
  backgrounds as rects and text in runs of one style with `fillText`, and caches nothing else.
  Device-pixel-ratio aware, one `<canvas>` per visible pane.
- Box drawing and block elements come from the font in v1. The Mac's `BoxDrawing` is a later
  port.
- **WebGL2 next**, only if profiling shows Canvas2D missing frames on full-screen output (a
  `cat` of a large file, a TUI redrawing everything): instanced quads and a glyph atlas built
  with an `OffscreenCanvas`, the same cell buffer as input.
- **WebGPU later, if ever.** It is what Tern uses, but a terminal of a few thousand cells is not
  GPU-bound, browser and OS coverage still varies, and WebGL2 is everywhere. Revisit when the
  WebGL2 path exists and shows a limit.

Frames are driven by `requestAnimationFrame`, and only when `Output`, a resize, the cursor blink
or a scroll made something dirty. Synchronized output (mode 2026) is honoured as the ffi does:
hold the frame until the update ends or its deadline passes.

## Protocol: a Rust client in wasm

Decided: **Rust compiled to wasm**, not a TypeScript reimplementation.

- `thurm-proto` already compiles for `wasm32-unknown-unknown` (checked), so the client encodes
  and decodes with the daemon's own types. A TS copy would have to mirror every enum's variant
  order and every field's position by hand: postcard has no tags, and one drift is a silent
  misdecode, not an error.
- The local terminal is `thurm-term` either way (it is what replays `Attach`, filters OSC
  extensions and encodes keys and mice), and that is Rust over libghostty. Once Rust is in the
  page, the protocol in Rust costs a few KB.
- `thurm-client` itself does not port: blocking `UnixStream`, a reader thread, crossbeam
  channels, `Instant`, `libc` peer checks and process spawning. The browser client is a new,
  single-threaded, sans-IO session: TS owns the WebSocket and the clock, Rust owns frames,
  request ids, panes and terminals.

```rust
// crates/thurm-web, exported with wasm-bindgen
impl Session {
    pub fn new(client_name: &str) -> Session;          // queues Hello
    pub fn receive(&mut self, bytes: &[u8]);            // any chunking; reassembles frames
    pub fn take_outgoing(&mut self) -> Vec<u8>;         // frames to send, possibly several
    pub fn take_events(&mut self) -> JsValue;           // decoded, for the UI: panes, notices, errors
    pub fn subscribe(&mut self, pane: u64, cols: u16, rows: u16, cell_w: u16, cell_h: u16);
    pub fn key(&mut self, pane: u64, ev: KeyInput);     // encoded locally, sent as Input
    pub fn text(&mut self, pane: u64, text: &str);      // IME commits, dead keys
    pub fn paste(&mut self, pane: u64, text: &str);
    pub fn mouse(&mut self, pane: u64, ev: MouseInput);
    pub fn frame(&mut self, pane: u64, now_ms: f64) -> FrameInfo; // fills the cell buffer
    pub fn tick(&mut self, now_ms: f64);                // Ping, sync-update deadlines
}
```

Porting work this needs, none of it changing native behaviour:

- `ghostty-vt-rs-sys`: map `wasm32-unknown-unknown` to zig's `wasm32-freestanding`, link the
  static archive, and publish it as one more prebuilt target. A change in the `ghostty-vt-rs`
  repository, then a version bump here.
- `thurm-term`: gate the kitty file and shared-memory media paths (`std::os::unix`, `libc::shm_open`)
  on `cfg(unix)` (the web client calls `set_reads_media(false)` like the phone); take time from
  the caller or `web-time` instead of `std::time::Instant`, which panics on this target.
- `thurm-config`: `thurm-term` needs its types (`Config`, `Theme`, `CursorStyle`, `Osc52Mode`)
  and `EngineConfig::from_config`. Gate the unix-only parts (peer uid, file modes, dirs) on
  `cfg(unix)` rather than moving the types; the web client uses `Config::default()`.
- CI: `cargo clippy -p thurm-web --target wasm32-unknown-unknown`, next to the iOS clippy, so
  native changes don't break the web build.

## Hosting and admission

- `[tailscale] web = true` (default false while it is new) serves the bundle on the node's
  HTTPS listener, the one that answers `wss://<node>.<tailnet>.ts.net/v1/connect`. Off, the
  gateway behaves as today. Defaulting off means no config migration; turning it on by default
  later needs a `MIGRATIONS` entry.
- New Go package `tailnet/internal/web`: the bundle via `go:embed`, served with the headers
  below. `index.html` is `Cache-Control: no-cache`; everything else has a content hash in its
  name and is `immutable`. `.wasm` as `application/wasm` so streaming compilation works.
  Precompressed brotli and gzip copies are embedded and picked by `Accept-Encoding`.
- The gateway routes: `/v1/connect` unchanged, `/` and `/assets/*` to `web` when it is on, 404
  otherwise. **Static files go through the same `Identify` + `Policy().Check` as the WebSocket**:
  one rule for everything on the node, and nothing for an unadmitted peer to fingerprint.
- The page's URL is the node's: `TailscaleStatus.url` with `https://` and no path. The Mac's
  Settings > Tailscale shows it with a copy button when `web` is on; `thurm tailscale status`
  prints it. No change to `TailscaleStatus` on the wire: the URL is derived.
- No `?token=`, no login form, no cookies, no `localStorage` credentials. A browser on a tailnet
  device the owner signed in to is admitted; one that isn't, gets 403 with the gateway's message.

## Security

Tailscale identity is ambient: it comes from the device's tailnet address, not from anything
the page holds. Any page open in a browser on an admitted device could try to use it, so the
browser's same-origin rules are what keep other sites out.

- **Cross-site WebSocket hijacking.** Browsers always send `Origin` on a WebSocket upgrade, and a
  page cannot forge it. The gateway checks it explicitly, before dialing the daemon:
  - no `Origin`: an app (the Mac, the phone, `thurm-tailnet-client`), allowed as today;
  - `Origin` exactly `https://<node DNS name>` (scheme, host, no port other than 443): the web
    client, allowed;
  - anything else, `null` included: 403, logged, and the daemon is never dialed.

  It replaces the library's default check (`InsecureSkipVerify: true` once the gateway has
  checked, so there is one rule, in our code, with tests). Pages on other `*.ts.net` nodes,
  including other Thurm nodes, are other origins and are refused.
- **Host.** The request's `Host` must be the node's DNS name. TLS already ties the connection to
  the node's certificate, so a DNS-rebinding page can't complete a handshake; the `Host` check
  makes that explicit and catches a misrouted proxy.
- **CSRF.** No cookies and no state-changing HTTP endpoints: `GET` on static files has no effect,
  and the only thing with effects is the WebSocket, covered by the origin check.
- **Content Security Policy** on every response:

  ```
  default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self';
  img-src 'self' data: blob:; font-src 'self'; connect-src 'self' wss://<node DNS name>;
  base-uri 'none'; form-action 'none'; frame-ancestors 'none'
  ```

  plus `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
  `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin`.
  No inline scripts or styles, no third-party anything: fonts and code come from the node.
- **Terminal content never becomes markup.** Cells are drawn on a canvas. The window title
  (OSC 0/2) is set as `document.title` text. OSC 8 links and plain URLs open on Cmd-click
  (Ctrl-click elsewhere), as on the Mac, only for `http`, `https` and `mailto`, in a new tab
  with `noopener,noreferrer`.
- **Clipboard.** OSC 52 writes (`ClipboardStore`) follow the host's `osc52` setting, then
  `navigator.clipboard.writeText`; where the browser refuses without a user gesture, the page
  shows a one-click "copy from terminal" prompt instead. OSC 52 reads (`ClipboardRequest`) are
  answered empty in v1: a page never hands the clipboard to the terminal unasked.
- **What an admitted browser can do.** Everything a shell can: the protocol has no read-only
  mode. That is today's rule for `/v1/connect`, not a new one, but a browser makes it easy to
  forget a tab on a shared screen. The page shows which account and device it is connected as
  (`Whoami`) and a disconnect button. `allow_tailnet_members` gives every member a shell, web or
  not; `specs/tailnet-remotes.md` has the per-login allow list as an open question.

## Protocol window

- **Same build, mostly.** The bundle ships inside `thurm-tailnet`, which ships beside `thurmd`,
  so a freshly loaded page and the daemon behind it are one build. Two cases break that:
  - a tab left open across a daemon upgrade, which reconnects to the new daemon;
  - an in-place upgrade that adopts the running helper (`Tailnet::adopt`), which keeps serving
    the old bundle in front of the new daemon.
- The client decides compatibility with `thurm_proto::hello_compatible`, as every client does.
  Within the window it keeps working: unknown event variants are skipped by the shared decoder.
  Outside it the page says which side to update.
- On reconnect, when `Hello`'s `build` differs from the page's, the page fetches `index.html`
  (uncached) and reloads only if that names a different bundle. That picks up a new bundle when
  there is one and doesn't loop when the adopted helper still serves the old one.
- **No protocol change in v1.** The web client is a `CAP_MOBILE` client with `client:
  "thurm-web"`: everything it needs (Subscribe/Attach, Input, Paste, Resize, Focus, Scroll,
  AnswerPermission, Ping, Whoami, GetLayout, ListPanes) exists today. If the web needs to be
  told apart from a phone later (it is dropped by `RevokePhone`'s "drop every mobile client",
  harmlessly: it reconnects), that is a new `"web"` capability, which an old daemon ignores.
- **Old daemon, new page / new daemon, old page:** as above. **iOS app, desktop apps:**
  unaffected; they see one more subscriber, sized by `active-size`.
- **Push relay:** unaffected. The web client registers no push device, and its reads go
  through `pane_read` like any client's, which already sends `/v1/read` for the phones.

## Panes, size and lifecycle

- On connect: `Hello`, `Whoami`, `GetLayout`. The layout's workspaces and tabs become the
  sidebar; a tab's split tree is drawn as it is, read-only. With no saved layout (a Linux host
  never opened by an app) the page falls back to `ListPanes`, one pane per tab. `LayoutChanged`
  re-reads it.
- Visible panes are subscribed; hidden tabs are unsubscribed (and drop their terminals) after a
  short delay, so switching back is instant but memory stays bounded.
- Size: the page measures cell size from the font (`measureText`, `fontBoundingBoxAscent` /
  `Descent`, after `document.fonts.load`), computes cols and rows per pane, and sends `Resize`,
  debounced, on window resizes and font changes. With `active-size` the page's size applies
  once it types; otherwise the pane is letterboxed at the active size, as on the other clients.
- Focus: a pane gets `Focus { focused: true }` when it is the focused pane and the document is
  visible and focused. That marks it read, like on the Mac.
- Keepalive: `Ping` every 15 s, ticked by a dedicated worker. Main-thread timers in hidden tabs
  are throttled hard enough (once a minute after a while, in Chromium) to pass the daemon's 45 s
  limit; a worker's messages still arrive. (As built, the worker is only that clock; see Status.)
  Nothing guarantees it: a browser may freeze a background tab outright. So a drop has to be
  cheap rather than prevented: `Attach` restores everything, retries are ticked by the same
  worker clock (not only a page timer), and `visibilitychange` reconnects at once.
- Reconnect with backoff (1 s up to 30 s) on close or error; resubscribe the visible panes.
- Tested with the page's own timers stubbed out (the worker keeps it connected past the
  daemon's limit, and brings it back after a drop). A real hidden tab can't be produced headless
  (both of Chromium's headless modes keep background pages visible and unthrottled), so real
  browsers' hidden tabs are checked by hand.
- `Event::Attach` again on a live pane: drop and rebuild that pane's terminal, keep the scroll
  position if the pane was at the bottom.
- Scrollback: the local terminal keeps what `Attach` replays, capped by `terminal.scrollback`
  from the default config (10,000 lines). A long scrollback makes the first `Attach` large (it is the daemon's whole
  history for the pane); see Open questions.

## Keyboard, IME and clipboard

Input goes to a hidden, focused `<textarea>` over the cursor, never to the canvas. It gets
key events, IME composition and paste, and places the IME candidate window.

- **Keys.** `keydown` → `KeyInput { code, key, mods, repeat }`. Rust maps it to the protocol's
  `KeyEvent`: `Key::Named` from `code`/`key` for named keys, `Key::Char` otherwise; kitty mod bits
  from `shiftKey` / `altKey` / `ctrlKey` / `metaKey` (Super) and `CapsLock` / `NumLock` from
  `getModifierState`; `base_layout` from `code` on a US layout, which kitty's protocol wants;
  `Release` from `keyup` when the pane's kitty flags ask for it. Then `Terminal::key` encodes it
  with libghostty's key encoder against the pane's current modes, and the bytes go out as
  `Input`. Same encoder, same modes as the desktop, so `vim`, `fish` and kitty-protocol apps see
  the same bytes.
- **Text.** Printable keys without Ctrl/Alt/Super are left to the `input` event, so layouts,
  dead keys and Option-characters on macOS produce what the OS produces. `keydown` with
  `isComposing` or `keyCode` 229 is ignored.
- **IME.** `compositionstart` / `compositionupdate` show the preedit at the cursor (drawn on
  the canvas, underlined); `compositionend` sends the committed text. The textarea follows the
  cursor so the candidate window does too.
- **Browser shortcuts.** A page can't take every key: Chromium keeps Ctrl/Cmd+W, T, N and
  Ctrl+Tab for itself, so Ctrl+W (delete word) in a shell would close the tab. While connected,
  the page sets a `beforeunload` guard, and offers fullscreen with the Keyboard Lock API where
  it exists (Chromium), which hands those keys to the page. Cmd+C / Cmd+V (Ctrl+Shift+C / V
  elsewhere) are copy and paste, as on the desktop.
- **Paste.** The `paste` event's text goes out as `Paste { text }`, so the daemon brackets it by
  the pane's mode. No confirmation for multi-line pastes, matching the apps today
  (`thurm_term::keys::paste_is_safe` is there if that changes everywhere).
- **Copy.** Selection is local (libghostty's selection on the pane's copy): drag, double-click
  word, triple-click line. Copy writes the selection's text in the `copy` event, which needs no
  clipboard permission.
- **Mouse.** When the pane has mouse reporting on, events are encoded locally (libghostty's mouse
  encoder) and sent as `Input`; otherwise they select. The wheel scrolls the local viewport, or
  goes to the program when it reports the mouse or the alternate screen wants arrows, as on the
  desktop.

## Fonts

- The bundle carries one monospace face as the default: JetBrains Mono (OFL), regular, bold,
  italic, as woff2, Latin subset plus box drawing and powerline. The docs site already uses it.
- The user can name a font family and size, kept in the browser's `localStorage`. An installed
  font is used when the browser exposes it to pages; Safari restricts which installed fonts
  pages may use (fingerprinting protection), so the bundled face is always the fallback.
- Missing glyphs fall back through the browser's font stack (`font-family: <chosen>,
  "JetBrains Mono", monospace`), which covers CJK and emoji with the platform's fonts. Wide
  characters take two cells by libghostty's width, not the font's.
- No Local Font Access API: it is Chromium-only and prompts for permission, for something a
  family name already gets.

## Agent notices

- `PaneInfo.agent` drives the sidebar: each tab and pane shows its agent's status (working, needs
  input, done, error) as the Mac's sidebar does.
- `Notify` adds to a notice list (title, body, pane, time); clicking one focuses its pane. A
  notice with `permission: Some(id)` has Approve and Deny, sent as `AnswerPermission`. The answer
  failing (the prompt changed, another client holds `AttachTerminal`) shows the daemon's text.
- `PaneRead` removes that pane's notices, whichever device read it.
- With the tab hidden, an agent notice may also show as a browser `Notification` if the user
  allowed it (asked once, from a button, never on load). No sound, no service worker.

## Bundle size

Measured (see Verified) and estimated:

| Part | Size (compressed) |
|---|---|
| `ghostty-vt.wasm`, whole C API, ReleaseSmall | 288 KB gzip, 227 KB brotli (measured) |
| Rust + libghostty static archive, terminal API only, LTO, `opt-level = "s"` | 149 KB gzip (measured) |
| `thurm_web.wasm`: thurm-term, encoders, render state, thurm-proto, postcard | ~300 to 400 KB gzip (estimate) |
| `app.js` and CSS | under 50 KB gzip (budget) |
| JetBrains Mono, 3 faces, subset | ~150 to 250 KB (estimate) |

Budget: under 1 MB compressed on first load, then cached by hash. `wasm-opt -Oz` in the build;
`WebAssembly.instantiateStreaming` so compilation overlaps the download. CI fails when
`thurm_web.wasm` grows past the budget by more than 10%.

## Where the code lives

All in `thurm` (public). No change in `thurm-ios` or `thurm-push-relay`.

| Path | What |
|---|---|
| `crates/thurm-web` | Rust, `cdylib` for `wasm32-unknown-unknown`, `wasm-bindgen`: the session, panes, key and mouse mapping, the cell buffer. Builds natively too, for tests. |
| `web/` | TypeScript, built with `bun build` (no framework: one canvas, a sidebar and a notice list). The worker, the renderer, input, UI. |
| `tailnet/internal/web` | Go: the embedded bundle, headers, compressed variants. |
| `tailnet/internal/gateway` | the explicit origin and `Host` checks, routing to `web`. |
| `crates/thurm-config` | `[tailscale] web`; `cfg(unix)` gates. |
| `crates/thurm-term` | `cfg(unix)` gates, time from the caller. |
| `ghostty-vt-rs` (separate repo) | the wasm32 target and its prebuilt archive. |

Build: a `just web` recipe (`cargo build -p thurm-web --target wasm32-unknown-unknown
--release`, `wasm-bindgen --target web`, `wasm-opt -Oz`, `bun build`, compress) writes
`tailnet/internal/web/dist/`, which is gitignored except for a placeholder `index.html` ("built
without the web client"), so `go build` works without it. The release and nix builds run the
recipe before building `thurm-tailnet`; the flake's dev shell gains `wasm-bindgen-cli` (matching
the crate's version), `binaryen`, and the wasm32 target in `rust-toolchain.toml`.

## Tests

- **Rust, `thurm-web`, native `cargo test`:** frame reassembly across every split of a recorded
  stream (one byte at a time, 64 KiB chunks, frames spanning messages); `Hello` and
  `hello_compatible` outcomes; request ids matched to responses; an unknown event variant skipped;
  `Attach` → terminal, a second `Attach` rebuilding it; keydown → `KeyEvent` → bytes for a table
  of keys (legacy and kitty flags, Alt, dead keys, AltGr, F-keys); `Ping` timing from `tick`.
- **Rust, wasm:** the same suite under `wasm-bindgen-test` in headless Node in CI. A golden test
  feeds one recorded daemon stream to native `thurm-term` and to the wasm build and compares the
  formatter's plain text and the cursor.
- **Go, gateway and web:** static files 403 for an unadmitted caller; `Origin` absent / same
  origin / other `*.ts.net` / `null` / wrong port, with the daemon dial counted to prove refused
  origins never reach it; wrong `Host` refused; CSP and the other headers on `index.html` and
  assets; `web = false` gives 404; `/v1/connect` for apps unchanged.
- **End to end, Playwright (Chromium, WebKit, Firefox) with bun:** a test gateway with a fake
  `Identify` in front of a real `thurmd` on an isolated socket and state dir. Load the page,
  attach, type `echo hi`, see it (read through a test hook that returns the pane's formatted
  text, not pixels); scroll back; paste multi-line; IME through Chromium's
  `Input.imeSetComposition`; a second client (`thurm` CLI) types into the same pane and the page
  shows it; a fake agent notice appears, Approve answers it, `PaneRead` takes it down; the
  daemon restarted under the page, and the page reconnecting.
- **By hand:** a real node, a browser on another tailnet device, the Mac app and the phone on the
  same pane. Never against the live daemon from a dev build: isolated `THURM_*` dirs and
  sockets.

## Order of work

1. `ghostty-vt-rs`: the wasm32 target and prebuilt archive; bump here.
2. `cfg(unix)` gates in `thurm-config` and `thurm-term`; the wasm clippy job.
3. `crates/thurm-web`: session, panes, input mapping, cell buffer, with the native tests.
4. `web/`: worker, Canvas2D renderer, input, sidebar, notices.
5. `tailnet/internal/web`, the gateway's origin and `Host` checks, `[tailscale] web`, Go tests.
6. Build: `just web`, release and nix packaging, the size check.
7. Playwright suite; docs (`docs/src/content/docs/`, a page on the web client and its security).
8. WebGL2 renderer if profiling asks for it.

## Verified

Checked while writing, on ghostty `1e9e14af` (the commit Thurm pins) and Rust 1.99.1:

- `zig build -Demit-lib-vt -Dtarget=wasm32-freestanding -Doptimize=ReleaseSmall` (Zig 0.16 from
  nixpkgs) builds `ghostty-vt.wasm` (850 KB; 288 KB gzip, 227 KB brotli) with no imports, and
  `lib/libghostty-vt.a` for wasm32.
- A Rust `cdylib` for `wasm32-unknown-unknown` linking that archive statically: 509 KB, 149 KB
  gzip with LTO. Run under bun, it created an 80×24 terminal, wrote `ESC[31mhello\r\nab`, and read
  the cursor back at column 2, row 1.
- `cargo check -p thurm-proto --target wasm32-unknown-unknown` passes.
  `cargo check -p thurm-client` fails in `thurm-config` (`std::os::unix`, `libc::getuid`,
  `getpeereid`, file modes), as expected.
- The coder/websocket default origin check, from its source (v1.8.15, `accept.go`).

Assumed, not checked: `thurm-term` building for wasm once gated (its remaining dependencies,
`flate2`, `png`, `regex`, `base64`, are pure Rust); the estimated sizes above; browser behaviour
around hidden-tab timers in workers, Keyboard Lock and installed fonts, which the Playwright
suite and the hand test have to confirm in each browser.

## Open questions

- **`tailscale serve`.** It terminates TLS with the machine's own `ts.net` certificate and
  passes identity in headers, so a browser could use it too, with the machine's name instead of
  the node's. Worth it once the node path works? The origin check would need that name.
- **Large first attach.** `Attach` carries the pane's whole scrollback. A `Subscribe` variant
  with a byte or line limit (a new request at the end of the enum, gated on a capability) would
  make first paint fast on a slow link. Measure first.
- **Per-login allow list** for who may use the node: shared with `specs/tailnet-remotes.md`.
- **Read-only viewers** (show a pane, refuse input) would need a daemon-side notion of a
  principal's rights. Not in v1, and not web-specific.
