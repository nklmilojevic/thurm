#!/usr/bin/env bash
# Create a disk image with Thurm and an Applications shortcut.
#
#   macos/package-dmg.sh [--preview] [--notarize | --notary-profile NAME] [--output PATH]
#
# --notarize takes Apple credentials from the environment (see macos/release/notarize.sh);
# --notary-profile NAME uses a `xcrun notarytool store-credentials` profile.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP="$HERE/build/Thurm.app"
PREVIEW=0
NOTARIZE=0
OUTPUT_ARG=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --preview) PREVIEW=1; shift ;;
        --notarize) NOTARIZE=1; shift ;;
        --notary-profile)
            [[ $# -gt 1 && -n "$2" ]] || { echo "Missing Keychain profile name" >&2; exit 2; }
            export NOTARY_PROFILE="$2"; NOTARIZE=1; shift 2 ;;
        --output)
            [[ $# -gt 1 && -n "$2" ]] || { echo "Missing output path" >&2; exit 2; }
            OUTPUT_ARG="$2"; shift 2 ;;
        *) echo "usage: $0 [--preview] [--notarize | --notary-profile NAME] [--output PATH]" >&2; exit 2 ;;
    esac
done
die() { echo "error: $*" >&2; exit 1; }
[[ -d "$APP" ]] || die "Run macos/build.sh first"
[[ "$PREVIEW" == 0 || "$NOTARIZE" == 0 ]] || die "A preview cannot be notarized"

IDENTITY="${THURM_SIGN_IDENTITY:-}"
if [[ -z "$IDENTITY" && -f "$HERE/.sign-identity" ]]; then
    IDENTITY="$(head -n1 "$HERE/.sign-identity")"
fi
if [[ "$PREVIEW" == 0 ]]; then
    [[ -n "$IDENTITY" && "$IDENTITY" != - ]] || die "Set THURM_SIGN_IDENTITY to a Developer ID Application certificate"
    security find-identity -v -p codesigning | grep -F "$IDENTITY" | grep -q '"Developer ID Application:' \
        || die "A valid Developer ID Application identity is required"
    for binary in "$APP" "$APP/Contents/Helpers/thurm" "$APP/Contents/Helpers/thurmd"; do
        details="$(codesign -dv --verbose=4 "$binary" 2>&1)"
        grep -q '^Authority=Developer ID Application:' <<<"$details" || die "Build with --distribution first"
        grep -q 'flags=.*runtime' <<<"$details" || die "Hardened runtime is missing: $binary"
        grep -q '^Timestamp=' <<<"$details" || die "Secure timestamp is missing: $binary"
    done
fi
codesign --verify --deep --strict "$APP"
if [[ "$NOTARIZE" == 1 ]]; then
    xcrun --find notarytool >/dev/null || die "Select an Xcode installation with notarytool"
    xcrun --find stapler >/dev/null || die "Select an Xcode installation with stapler"
fi

VERSION="$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")"
for binary in "$APP/Contents/MacOS/Thurm" "$APP/Contents/Helpers/thurm" "$APP/Contents/Helpers/thurmd"; do
    lipo "$binary" -verify_arch arm64 || die "$binary is not built for arm64"
done
SUFFIX=""
[[ "$PREVIEW" == 0 ]] || SUFFIX=-preview
OUTPUT="${OUTPUT_ARG:-$HERE/build/Thurm-$VERSION-arm64$SUFFIX.dmg}"
WORK="$(mktemp -d "$HERE/build/dmg.XXXXXX")"
cleanup() {
    rm -rf "$WORK"
}
trap cleanup EXIT
mkdir -p "$WORK/stage"
ditto "$APP" "$WORK/stage/Thurm.app"
ln -s /Applications "$WORK/stage/Applications"
# Saved Finder layout: 96-point icons, Thurm on the left, Applications on the right.
cp "$HERE/Resources/dmg-layout.DS_Store" "$WORK/stage/.DS_Store"
hdiutil create -volname Thurm -srcfolder "$WORK/stage" -fs HFS+ -format UDZO \
    -imagekey zlib-level=9 "$WORK/Thurm.dmg"
if [[ "$PREVIEW" == 0 ]]; then
    codesign --force --timestamp --sign "$IDENTITY" "$WORK/Thurm.dmg"
    codesign --verify --strict "$WORK/Thurm.dmg"
fi
if [[ "$NOTARIZE" == 1 ]]; then
    "$HERE/release/notarize.sh" "$WORK/Thurm.dmg"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$WORK/Thurm.dmg"
fi
hdiutil verify "$WORK/Thurm.dmg"
mv -f "$WORK/Thurm.dmg" "$OUTPUT"
echo "Created: $OUTPUT"
if [[ "$NOTARIZE" == 0 ]]; then
    echo "This image is not notarized. It is not ready for normal Gatekeeper installation."
fi
