#!/usr/bin/env bash
# Notarize a signed .app, .dmg or .zip with Apple, then staple the ticket (an .app is sent as a
# zip and stapled in place; a .zip can't be stapled, so pass the .app instead).
#
#   macos/release/notarize.sh PATH
#
# Credentials, one of:
#   NOTARY_KEY_PATH, NOTARY_KEY_ID, NOTARY_ISSUER_ID   App Store Connect API key (.p8), as in CI
#   NOTARY_PROFILE                                    a `xcrun notarytool store-credentials` profile
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
[[ $# -eq 1 ]] || die "usage: $0 PATH"
TARGET="$1"
[[ -e "$TARGET" ]] || die "$TARGET not found"

AUTH=()
if [[ -n "${NOTARY_KEY_PATH:-}" ]]; then
    [[ -n "${NOTARY_KEY_ID:-}" && -n "${NOTARY_ISSUER_ID:-}" ]] || die "NOTARY_KEY_ID and NOTARY_ISSUER_ID are required with NOTARY_KEY_PATH"
    AUTH=(--key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID")
elif [[ -n "${NOTARY_PROFILE:-}" ]]; then
    AUTH=(--keychain-profile "$NOTARY_PROFILE")
else
    die "set NOTARY_KEY_PATH/NOTARY_KEY_ID/NOTARY_ISSUER_ID or NOTARY_PROFILE"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

SUBMIT="$TARGET"
if [[ "$TARGET" == *.app ]]; then
    SUBMIT="$WORK/$(basename "$TARGET" .app).zip"
    ditto -c -k --sequesterRsrc --keepParent "$TARGET" "$SUBMIT"
fi

# A key of the last notarytool result, empty when it has none (or the result is no plist).
result() { plutil -extract "$1" raw -o - "$WORK/result.plist" 2>/dev/null || true; }

echo "==> notarytool submit $(basename "$TARGET")"
xcrun notarytool submit "$SUBMIT" "${AUTH[@]}" --wait --timeout 30m \
    --output-format plist > "$WORK/result.plist" || true
STATUS="$(result status)"
ID="$(result id)"
[[ "$ID" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$ ]] || ID=""
# The wait timed out while Apple was still processing: keep waiting for the same submission.
if [[ -z "$STATUS" && -n "$ID" ]]; then
    echo "==> still in progress after 30m, waiting for $ID"
    xcrun notarytool wait "$ID" "${AUTH[@]}" --timeout 45m \
        --output-format plist > "$WORK/result.plist" || true
    STATUS="$(result status)"
fi
STATUS="${STATUS:-unknown}"
if [[ "$STATUS" != Accepted ]]; then
    cat "$WORK/result.plist" >&2 || true
    if [[ -n "$ID" ]]; then
        xcrun notarytool log "$ID" "${AUTH[@]}" >&2 || true
    fi
    die "notarization of $(basename "$TARGET") returned: $STATUS"
fi

if [[ "$TARGET" != *.zip ]]; then
    xcrun stapler staple "$TARGET"
    xcrun stapler validate "$TARGET"
fi
echo "==> notarized $(basename "$TARGET") ($ID)"
