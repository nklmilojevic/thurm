# Agent notes

This is `thurm`.

## Three components

Thurm is three repositories, checked out side by side (`~/dev/personal/`). A change in one often
needs a change, or at least a check, in the others: before calling something done, look at what
it touches across all three.

| Repository | What it is |
|---|---|
| `thurm` (public) | The daemon (`thurmd`), CLI, the macOS and Linux apps, and the Rust core the iOS app compiles. |
| `thurm-ios` (private) | The iPhone and iPad app. Builds the core from `../thurm` (`build-core.sh`) and compiles the Mac renderer sources. |
| `thurm-push-relay` (private) | The Go service between `thurmd` and APNs: device registration, agent notices and Live Activity updates. |

What they share, and where a change on one side needs the other:

- **Daemon protocol** (`thurm-proto`). The iOS app speaks it through `thurm-ffi`'s `mobile`
  feature; a new or changed `Request`, `Event` or capability (`CAP_MOBILE`,
  `CAP_ACTIVE_SIZE`) needs the app's side too. Mind the protocol window: an installed app and a
  newer daemon (or the other way round) must keep working.
- **Rust core for iOS.** `thurm-ffi` (`mobile`) and `thurm-ssh` build for `aarch64-apple-ios`:
  no process spawning or Linux-only code there (CI clippies that target). Changes to the C header
  or the Mac renderer sources (Renderer, GlyphAtlas, FontShaper, BoxDrawing, Models) rebuild
  into the app.
- **Push relay API.** `thurmd` (`crates/thurm-daemon/src/push.rs`) posts to `POST /v1/push`,
  `POST /v1/activity` and `POST /v1/read` (a pane read elsewhere: the relay sends the app a silent
  push with `read: {host, pane}`, and the app takes that pane's notices down), signed with the
  device secret (`X-Thurm-Timestamp`, `X-Thurm-Signature`: HMAC-SHA256 of `<timestamp>.<body>`).
  The app registers with `POST /v1/devices` and leaves with `DELETE /v1/devices/{device}`;
  `thurmd` forgets a device only on a 404 with `X-Thurm-Device: unknown` (any other 404, from a
  proxy or an older relay, keeps it). Body fields, headers, signatures and status codes must
  agree on both sides.
- **Pairing.** The Mac's Pair iPhone sheet (`macos/Sources/Thurm/PairPhone.swift`) makes the
  `thurm://pair?d=` payload the app parses (`PairingPayload`), and keeps the app's device keys in
  `authorized_keys` under the comment `thurm-ios:<device name>`; revoking goes through
  `RevokePhone`.
- **Releases.** An app in TestFlight or on the App Store meets daemons of older and newer
  versions, and the deployed relay serves both: ship changes so each side tolerates the other
  being behind.

Keep business details out of `thurm`: it is public.
