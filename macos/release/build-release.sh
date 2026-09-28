#!/usr/bin/env bash
# Build a signed, notarized Thurm release for one update channel into macos/build/dist/:
#
#   Thurm-<version>.dmg / Thurm-tip.dmg            disk image for people (notarized, stapled)
#   Thurm-<version>.zip / Thurm-tip-<build>.zip    the app for Sparkle (stapled, EdDSA-signed)
#   sparkle.json                                   the appcast entry (see appcast.py)
#
#   macos/release/build-release.sh release|tip
#
# Environment:
#   THURM_VERSION             marketing version (release: the tag without "v")
#   THURM_BUILD_NUMBER        CFBundleVersion: must grow with every build on any channel
#   THURM_FEED_URL            appcast URL compiled into the app
#   THURM_SPARKLE_PUBLIC_KEY  EdDSA public key (generate_keys) compiled into the app
#   SPARKLE_KEY_FILE          the matching private key file (generate_keys -x)
#   THURM_DOWNLOAD_BASE       URL the archives are published under (a GitHub release)
#   THURM_SIGN_IDENTITY       "Developer ID Application: …"
#   NOTARY_*                  Apple credentials, see notarize.sh
#   THURM_NOTES_FILE          optional Markdown release notes for the appcast
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MACOS="$(cd "$HERE/.." && pwd)"
ROOT="$(cd "$MACOS/.." && pwd)"
die() { echo "error: $*" >&2; exit 1; }
log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

CHANNEL="${1:-}"
[[ "$CHANNEL" == release || "$CHANNEL" == tip ]] || die "usage: $0 release|tip"
for v in THURM_VERSION THURM_BUILD_NUMBER THURM_FEED_URL THURM_SPARKLE_PUBLIC_KEY SPARKLE_KEY_FILE \
    THURM_DOWNLOAD_BASE THURM_SIGN_IDENTITY; do
    [[ -n "${!v:-}" ]] || die "$v is not set"
done
[[ -f "$SPARKLE_KEY_FILE" ]] || die "SPARKLE_KEY_FILE $SPARKLE_KEY_FILE not found"
[[ "$THURM_BUILD_NUMBER" =~ ^[0-9]+$ ]] || die "THURM_BUILD_NUMBER must be a number"

SHA="$(git -C "$ROOT" rev-parse --short=7 HEAD)"
if [[ "$CHANNEL" == release ]]; then
    SHORT="$THURM_VERSION"
    ZIP_NAME="Thurm-$THURM_VERSION.zip"
    DMG_NAME="Thurm-$THURM_VERSION.dmg"
else
    SHORT="$THURM_VERSION-tip.$SHA"
    ZIP_NAME="Thurm-tip-$THURM_BUILD_NUMBER.zip"
    DMG_NAME="Thurm-tip.dmg"
fi
export THURM_CHANNEL="$CHANNEL"
export THURM_BUILD="$SHORT+$THURM_BUILD_NUMBER"
export THURM_VERSION="$SHORT"

DIST="$MACOS/build/dist"
rm -rf "$DIST"
mkdir -p "$DIST"

log "Building Thurm $SHORT ($THURM_BUILD_NUMBER) for the $CHANNEL channel"
"$MACOS/build.sh" --distribution
APP="$MACOS/build/Thurm.app"

log "Notarizing the app"
"$HERE/notarize.sh" "$APP"
spctl --assess --type execute --verbose=2 "$APP"

log "Sparkle archive"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$DIST/$ZIP_NAME"

log "Disk image"
"$MACOS/package-dmg.sh" --notarize --output "$DIST/$DMG_NAME"

SIGN_UPDATE="$(find "$MACOS/.build/artifacts" -path '*/bin/sign_update' -type f | head -n1)"
[[ -x "$SIGN_UPDATE" ]] || die "sign_update not found under macos/.build/artifacts"
SIGNATURE="$("$SIGN_UPDATE" --ed-key-file "$SPARKLE_KEY_FILE" -p "$DIST/$ZIP_NAME")"
[[ -n "$SIGNATURE" ]] || die "sign_update produced no signature"
LENGTH="$(stat -f %z "$DIST/$ZIP_NAME")"
MIN_OS="$(/usr/libexec/PlistBuddy -c 'Print LSMinimumSystemVersion' "$APP/Contents/Info.plist")"

META_CHANNEL="$CHANNEL" META_VERSION="$THURM_BUILD_NUMBER" META_SHORT="$SHORT" \
META_COMMIT="$(git -C "$ROOT" rev-parse HEAD)" META_URL="$THURM_DOWNLOAD_BASE/$ZIP_NAME" \
META_LENGTH="$LENGTH" META_SIGNATURE="$SIGNATURE" META_MIN_OS="$MIN_OS" \
META_NOTES_FILE="${THURM_NOTES_FILE:-}" \
python3 - "$DIST/sparkle.json" <<'EOF'
import json, os, sys, time
e = os.environ
notes_file = e.get("META_NOTES_FILE", "")
notes = open(notes_file, encoding="utf-8").read() if notes_file and os.path.isfile(notes_file) else ""
with open(sys.argv[1], "w", encoding="utf-8") as f:
    json.dump({
        "channel": e["META_CHANNEL"],
        "version": e["META_VERSION"],
        "short_version": e["META_SHORT"],
        "commit": e["META_COMMIT"],
        "url": e["META_URL"],
        "length": int(e["META_LENGTH"]),
        "ed_signature": e["META_SIGNATURE"],
        "minimum_system_version": e["META_MIN_OS"],
        "published": time.strftime("%a, %d %b %Y %H:%M:%S +0000", time.gmtime()),
        "notes": notes,
    }, f, indent=2)
EOF
log "Done: $(ls "$DIST" | tr '\n' ' ')"
