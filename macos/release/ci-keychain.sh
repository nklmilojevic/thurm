#!/usr/bin/env bash
# CI only: import the Developer ID certificate into a throwaway keychain and print the signing
# identity. Reads MACOS_CERTIFICATE_P12 (base64 of the .p12) and MACOS_CERTIFICATE_PASSWORD from
# the environment; the keychain is locked with a random value that never leaves this runner.
#
#   macos/release/ci-keychain.sh create KEYCHAIN_PATH   prints the identity
#   macos/release/ci-keychain.sh delete KEYCHAIN_PATH
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
[[ $# -eq 2 ]] || die "usage: $0 create|delete KEYCHAIN_PATH"
KEYCHAIN="$2"

case "$1" in
    delete)
        security delete-keychain "$KEYCHAIN" 2>/dev/null || true
        exit 0 ;;
    create) ;;
    *) die "usage: $0 create|delete KEYCHAIN_PATH" ;;
esac

[[ -n "${MACOS_CERTIFICATE_P12:-}" ]] || die "MACOS_CERTIFICATE_P12 is not set"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
umask 077
openssl rand -base64 32 > "$WORK/lock"
printf '%s' "$MACOS_CERTIFICATE_P12" | base64 --decode > "$WORK/certificate.p12"

security create-keychain -p "$(cat "$WORK/lock")" "$KEYCHAIN" >&2
security set-keychain-settings -lut 21600 "$KEYCHAIN" >&2
security unlock-keychain -p "$(cat "$WORK/lock")" "$KEYCHAIN" >&2
security import "$WORK/certificate.p12" -P "${MACOS_CERTIFICATE_PASSWORD:-}" -A -t cert -f pkcs12 \
    -k "$KEYCHAIN" >&2
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$(cat "$WORK/lock")" \
    "$KEYCHAIN" >/dev/null
# Searched first, before the login keychain.
# shellcheck disable=SC2046
security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | tr -d '"')

security find-identity -v -p codesigning "$KEYCHAIN" \
    | grep -o '"Developer ID Application: [^"]*"' | head -n1 | tr -d '"'
