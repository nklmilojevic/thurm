#!/usr/bin/env bash
# Build Thurm.app (Apple silicon): Rust core, Swift front end, app bundle, signature.
#
#   ./macos/build.sh            release build into macos/build/Thurm.app
#   ./macos/build.sh --install  build, then install into /Applications (or ~/Applications),
#                               link the `thurm` CLI into ~/.local/bin, and open it
#   SKIP_RUST=1 ./macos/build.sh  reuse the Rust artifacts already in target/aarch64-apple-darwin/release
#   THURM_SWIFT_BUILD_SYSTEM=native ./macos/build.sh
#                               use Swift's native build system
#   THURM_SIGN_IDENTITY="Apple Development: …" ./macos/build.sh
#                               sign with that identity (see `security find-identity -v -p
#                               codesigning`) instead of ad hoc. macOS then recognizes Thurm
#                               across rebuilds, so privacy grants (e.g. access to other
#                               apps' data, which 1Password's CLI needs) persist. Also read
#                               from macos/.sign-identity (not committed).
#   ./macos/build.sh --distribution
#                               Developer ID signature with hardened runtime and secure
#                               timestamps, ready for notarization (macos/release/).
#
# Release builds (macos/release/build-release.sh) also set:
#   THURM_VERSION               CFBundleShortVersionString (default: the workspace version)
#   THURM_BUILD_NUMBER          CFBundleVersion, increasing across all channels (default: the
#                               commit count)
#   THURM_BUILD                 build id compiled into the app and daemon; an app that finds
#                               the daemon on another build replaces it in place (default:
#                               version+commit)
#   THURM_CHANNEL               release | tip | dev (default dev)
#   THURM_DOWNLOAD_BASE         the release the build is published under; with
#   THURM_SHA256_X86_64_UNKNOWN_LINUX_MUSL, THURM_SHA256_AARCH64_UNKNOWN_LINUX_MUSL
#                               checksums of the Linux archives built alongside it, the app
#                               installs itself on Linux remote hosts (Thurm > Remotes…)
#   THURM_FEED_URL, THURM_SPARKLE_PUBLIC_KEY
#                               Sparkle appcast and EdDSA public key; without both, the app
#                               never checks for updates
set -euo pipefail

INSTALL=0
DISTRIBUTION=0
for arg in "$@"; do
    case "$arg" in
        --install) INSTALL=1 ;;
        --distribution) DISTRIBUTION=1 ;;
        *) printf 'usage: %s [--install] [--distribution]\n' "$0" >&2; exit 2 ;;
    esac
done

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
RUST_TARGET=aarch64-apple-darwin
RUST_OUT="$ROOT/target/$RUST_TARGET/release"
APP="$HERE/build/Thurm.app"
BINARIES=(thurmd thurm)
STATICLIB=libthurm_ffi.a

# Deliberately not exporting MACOSX_DEPLOYMENT_TARGET: cargo applies it to host proc-macro
# dylibs too, and rustc 1.97 on macOS 27 then fails to load them ("can't find crate for
# `thiserror_impl`"). rustc's default minimum (11.0) is below the app's 14.0 anyway.
unset MACOSX_DEPLOYMENT_TARGET

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

IDENTITY="${THURM_SIGN_IDENTITY:-}"
if [[ -z "$IDENTITY" && -f "$HERE/.sign-identity" ]]; then
    IDENTITY="$(head -n1 "$HERE/.sign-identity")"
fi
if [[ "$DISTRIBUTION" == 1 ]]; then
    [[ -n "$IDENTITY" && "$IDENTITY" != - ]] || die "Set THURM_SIGN_IDENTITY to a Developer ID Application certificate"
    security find-identity -v -p codesigning | grep -F "$IDENTITY" | grep -q '"Developer ID Application:' \
        || die "Distribution requires a valid Developer ID Application identity"
fi

[[ "$(uname -s)" == "Darwin" ]] || die "Thurm.app can only be built on macOS"
[[ "$(uname -m)" == "arm64" ]] || die "Thurm supports Apple silicon only; build on an arm64 Mac"
command -v cargo >/dev/null || die "cargo not found (use the dev shell: nix develop)"
command -v swift >/dev/null || die "swift not found (install Xcode 26 or later)"
# libghostty-vt is built from source with Zig 0.16 (in the flake.nix dev shell).
zig version 2>/dev/null | grep -q '^0\.16\.' || die "zig 0.16 not found (use the dev shell: nix develop, or direnv allow)"

# ---------------------------------------------------------------------------------------
# 0. Version: shared by Info.plist, the app and the daemon.
# ---------------------------------------------------------------------------------------
GIT_SHA="$(git -C "$ROOT" rev-parse --short=7 HEAD 2>/dev/null || echo unknown)"
VERSION="${THURM_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n1)}"
BUILD_NUMBER="${THURM_BUILD_NUMBER:-$(git -C "$ROOT" rev-list --count HEAD 2>/dev/null || echo 1)}"
CHANNEL="${THURM_CHANNEL:-dev}"
DIRTY=""
if [[ "$CHANNEL" == dev ]] && ! git -C "$ROOT" diff --quiet HEAD 2>/dev/null; then
    DIRTY=".dirty"
fi
export THURM_BUILD="${THURM_BUILD:-$VERSION+$BUILD_NUMBER.$GIT_SHA$DIRTY}"
# The commit, for installing this build on a Nix remote host (the flake at this revision).
THURM_COMMIT="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || true)"
export THURM_COMMIT
log "Thurm $VERSION ($BUILD_NUMBER), build $THURM_BUILD, channel $CHANNEL"

# ---------------------------------------------------------------------------------------
# 1. Rust (arm64).
# ---------------------------------------------------------------------------------------
if [[ -z "${SKIP_RUST:-}" ]]; then
    if command -v rustup >/dev/null && ! rustup target list --installed 2>/dev/null | grep -qx "$RUST_TARGET"; then
        die "Rust target $RUST_TARGET not installed (rustup target add $RUST_TARGET)"
    fi
    log "cargo build --release ($RUST_TARGET)"
    (cd "$ROOT" && cargo build --release -p thurm-ffi -p thurm-daemon -p thurm-cli --target "$RUST_TARGET")
else
    log "SKIP_RUST set; using existing artifacts in $RUST_OUT"
fi

for f in "$STATICLIB" "${BINARIES[@]}"; do
    [[ -f "$RUST_OUT/$f" ]] || die "missing $RUST_OUT/$f"
done

# ---------------------------------------------------------------------------------------
# 2. C header for the CThurm module (copied, not symlinked, so SwiftPM sees a real file).
# ---------------------------------------------------------------------------------------
mkdir -p "$HERE/Sources/CThurm/include"
cp "$ROOT/crates/thurm-ffi/include/thurm.h" "$HERE/Sources/CThurm/include/thurm.h"

# ---------------------------------------------------------------------------------------
# 3. Swift.
# ---------------------------------------------------------------------------------------
export THURM_LIB_DIR="$RUST_OUT"
THURM_SDK_VERSION="$(xcrun --sdk macosx --show-sdk-version)"
export THURM_SDK_VERSION
cd "$HERE"
SWIFT_BUILD_FLAGS=()
if [[ -n "${THURM_SWIFT_BUILD_SYSTEM:-}" ]]; then
    SWIFT_BUILD_FLAGS+=(--build-system "$THURM_SWIFT_BUILD_SYSTEM")
fi

swift_release() {
    swift build ${SWIFT_BUILD_FLAGS[@]+"${SWIFT_BUILD_FLAGS[@]}"} -c release --arch arm64 "$@"
}

log "swift build (arm64)"
BIN_DIR="$(swift_release --show-bin-path)"
# SwiftPM does not track changes to the external Rust archive. Force a new link.
rm -f "$BIN_DIR/Thurm"
swift_release
SWIFT_BIN="$BIN_DIR/Thurm"
[[ -x "$SWIFT_BIN" ]] || die "Swift build produced no binary at $SWIFT_BIN"
INTELLIGENCE_BIN="$(dirname "$SWIFT_BIN")/thurm-intelligence"
[[ -x "$INTELLIGENCE_BIN" ]] || die "Swift build produced no binary at $INTELLIGENCE_BIN"

# ---------------------------------------------------------------------------------------
# 4. App bundle.
# ---------------------------------------------------------------------------------------
log "Assembling $APP"
rm -rf "$APP"
# The Rust binaries go to Contents/Helpers: `thurm` and `Thurm` are the same file name on
# a case-insensitive volume, so the CLI must not share Contents/MacOS with the app.
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Helpers" "$APP/Contents/Resources" "$APP/Contents/Frameworks"
cp "$SWIFT_BIN" "$APP/Contents/MacOS/Thurm"
for b in "${BINARIES[@]}"; do
    cp "$RUST_OUT/$b" "$APP/Contents/Helpers/$b"
    chmod +x "$APP/Contents/Helpers/$b"
done
SPARKLE="$(find "$HERE/.build/artifacts" -path '*Sparkle.xcframework/macos-*/Sparkle.framework' -maxdepth 6 -type d | head -n1)"
[[ -d "$SPARKLE" ]] || die "Sparkle.framework not found under macos/.build/artifacts"
ditto "$SPARKLE" "$APP/Contents/Frameworks/Sparkle.framework"
# thurmd runs it for the `[ai]` features (Apple Intelligence's on-device model).
cp "$INTELLIGENCE_BIN" "$APP/Contents/Helpers/thurm-intelligence"
cp "$HERE/Resources/Info.plist" "$APP/Contents/Info.plist"
plist() { /usr/libexec/PlistBuddy -c "$1" "$APP/Contents/Info.plist"; }
plist "Set :CFBundleShortVersionString $VERSION"
plist "Set :CFBundleVersion $BUILD_NUMBER"
plist "Add :ThurmBuild string $THURM_BUILD"
plist "Add :ThurmChannel string $CHANNEL"
# A malformed feed (e.g. `https://owner.github.io//appcast.xml`) would strand every copy of
# this build on it: no update could ever fix it.
if [[ -n "${THURM_FEED_URL:-}" ]] \
    && ! [[ "$THURM_FEED_URL" =~ ^https://[^/]+/[^/] && "${THURM_FEED_URL#https://}" != *//* ]]; then
    die "THURM_FEED_URL is not a valid https URL: $THURM_FEED_URL"
fi
if [[ -n "${THURM_FEED_URL:-}" && -n "${THURM_SPARKLE_PUBLIC_KEY:-}" ]]; then
    plist "Add :SUFeedURL string $THURM_FEED_URL"
    plist "Add :SUPublicEDKey string $THURM_SPARKLE_PUBLIC_KEY"
elif [[ "$DISTRIBUTION" == 1 ]]; then
    die "Distribution builds need THURM_FEED_URL and THURM_SPARKLE_PUBLIC_KEY"
fi
printf 'APPL????' > "$APP/Contents/PkgInfo"
if [[ -d "$ROOT/shell-integration" ]]; then
    cp -R "$ROOT/shell-integration" "$APP/Contents/Resources/shell-integration"
else
    log "note: $ROOT/shell-integration not found; bundle ships without it"
fi
# Command specs for tab completion (read by thurmd from Contents/Resources/completions).
if [[ -d "$ROOT/completions" ]]; then
    cp -R "$ROOT/completions" "$APP/Contents/Resources/completions"
fi
# Bundled fonts (Symbols Nerd Font Mono for icon fallback), registered at launch.
if [[ -d "$HERE/Resources/fonts" ]]; then
    cp -R "$HERE/Resources/fonts" "$APP/Contents/Resources/fonts"
fi
# The Claude Code skill (Thurm > Integrations links ~/.claude/skills/thurm to it).
if [[ -d "$ROOT/skills" ]]; then
    cp -R "$ROOT/skills" "$APP/Contents/Resources/skills"
fi
if [[ -f "$HERE/Resources/AppIcon.icns" ]]; then
    cp "$HERE/Resources/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
fi
# Thurm's license and the notices of everything it includes.
cp "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_LICENSES" "$APP/Contents/Resources/"

# ---------------------------------------------------------------------------------------
# 5. Sign helpers first, then the app.
# ---------------------------------------------------------------------------------------
# Resource access the hardened runtime otherwise denies to programs in the shells.
ENTITLEMENTS="$HERE/Resources/Thurm.entitlements"
if [[ -n "$IDENTITY" ]]; then
    log "codesign ($IDENTITY)"
    SIGN_FLAGS=(--timestamp=none --options runtime)
    if [[ "$DISTRIBUTION" == 1 ]]; then
        SIGN_FLAGS=(--timestamp --options runtime)
    fi
    sign() { codesign --force "${SIGN_FLAGS[@]}" -s "$IDENTITY" "$@"; }
    # Inside out, without --deep (Sparkle's Downloader keeps its own entitlements).
    FW="$APP/Contents/Frameworks/Sparkle.framework/Versions/B"
    sign "$FW/XPCServices/Installer.xpc"
    sign --preserve-metadata=entitlements "$FW/XPCServices/Downloader.xpc"
    sign "$FW/Autoupdate"
    sign "$FW/Updater.app"
    sign "$APP/Contents/Frameworks/Sparkle.framework"
    # thurmd too: started at login it is the process macOS holds responsible for the shells.
    for b in "${BINARIES[@]}" thurm-intelligence; do
        if [[ "$b" == thurmd ]]; then
            sign --entitlements "$ENTITLEMENTS" "$APP/Contents/Helpers/$b"
        else
            sign "$APP/Contents/Helpers/$b"
        fi
    done
    sign --entitlements "$ENTITLEMENTS" "$APP"
else
    log "codesign (ad-hoc; set THURM_SIGN_IDENTITY for grants that survive rebuilds)"
    codesign --force --deep -s - "$APP"
    codesign --force -s - --entitlements "$ENTITLEMENTS" "$APP/Contents/Helpers/thurmd"
    codesign --force -s - --entitlements "$ENTITLEMENTS" "$APP"
fi
codesign --verify --deep --strict "$APP"

log "Done: $APP"

[[ "$INSTALL" == 1 ]] || exit 0

# ---------------------------------------------------------------------------------------
# 6. Install: replace the installed app (the daemon keeps running, so do the shells).
# ---------------------------------------------------------------------------------------
DEST_DIR=/Applications
[[ -w "$DEST_DIR" ]] || { DEST_DIR="$HOME/Applications"; mkdir -p "$DEST_DIR"; }
DEST="$DEST_DIR/Thurm.app"

if pgrep -xq Thurm; then
    log "Quitting Thurm (tabs stay open in the daemon)"
    osascript -e 'tell application id "com.thurm.terminal" to quit' >/dev/null 2>&1 \
        || osascript -e 'tell application "Thurm" to quit' >/dev/null 2>&1 || true
    for _ in $(seq 1 40); do pgrep -xq Thurm || break; sleep 0.25; done
    pgrep -xq Thurm && die "Thurm is still running; quit it and run again"
fi

log "Installing $DEST"
ditto "$APP" "$DEST.new"
rm -rf "$DEST"
mv "$DEST.new" "$DEST"

# The CLI on PATH. Only replaces a link, never a real file.
LINK="$HOME/.local/bin/thurm"
if [[ -d "$HOME/.local/bin" ]] && { [[ ! -e "$LINK" ]] || [[ -L "$LINK" ]]; }; then
    ln -sfn "$DEST/Contents/Helpers/thurm" "$LINK"
    log "Linked $LINK"
else
    log "note: link the CLI yourself, or use Thurm > Integrations > Install Command-Line Tool"
fi

# Links made from an earlier location now point here.
SKILL="$HOME/.claude/skills/thurm"
if [[ -L "$SKILL" ]] && [[ "$(readlink "$SKILL")" == *Thurm.app/* ]]; then
    ln -sfn "$DEST/Contents/Resources/skills/thurm" "$SKILL"
fi
PLIST="$HOME/Library/LaunchAgents/com.thurm.daemon.plist"
if [[ -f "$PLIST" ]] && ! grep -q "$DEST/Contents/Helpers/thurmd" "$PLIST"; then
    log "note: the login daemon runs another copy; redo Thurm > Integrations > Start Daemon at Login"
fi

# A clean environment, as from the Dock: the app, its daemon and every shell it starts would
# otherwise inherit this script's (and whatever launched it, e.g. a Claude Code session's).
env -i HOME="$HOME" USER="$USER" PATH=/usr/bin:/bin:/usr/sbin:/sbin /usr/bin/open "$DEST"
log "Installed: $DEST"
