#!/usr/bin/env bash
# One-time setup of the release pipeline (.github/workflows/release.yml) for this repository.
# Stores the signing material as GitHub Actions secrets and turns on GitHub Pages for the
# appcast. Nothing is written to the repository or printed.
#
#   macos/release/setup-github.sh \
#       --certificate  op://Private/Thurm Developer ID/certificate.p12 \
#       --certificate-password op://Private/Thurm Developer ID/password \
#       --notary-key   op://Private/Thurm Notary API Key/AuthKey.p8 \
#       --notary-key-id op://Private/Thurm Notary API Key/key id \
#       --notary-issuer op://Private/Thurm Notary API Key/issuer id
#
# Each value is a 1Password reference (read with `op read`) or a local file path.
#   certificate          Developer ID Application certificate with its private key, as .p12
#                        (Keychain Access › export). Your own Apple Developer account's.
#   notary key           App Store Connect API key (.p8, "Developer" access), its key id and
#                        issuer id (App Store Connect › Users and Access › Integrations)
# The Sparkle EdDSA key pair is created in your login keychain by Sparkle's generate_keys (or
# reused when it exists); back up the private key: generate_keys --account thurm -x FILE.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MACOS="$(cd "$HERE/.." && pwd)"
die() { echo "error: $*" >&2; exit 1; }
log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

CERT_REF="" CERT_PASS_REF="" NOTARY_REF="" NOTARY_ID_REF="" NOTARY_ISSUER_REF=""
while [[ $# -gt 0 ]]; do
    [[ $# -gt 1 ]] || die "$1 needs a value"
    case "$1" in
        --certificate) CERT_REF="$2" ;;
        --certificate-password) CERT_PASS_REF="$2" ;;
        --notary-key) NOTARY_REF="$2" ;;
        --notary-key-id) NOTARY_ID_REF="$2" ;;
        --notary-issuer) NOTARY_ISSUER_REF="$2" ;;
        *) die "unknown argument $1 (see the header of $0)" ;;
    esac
    shift 2
done
[[ -n "$CERT_REF" && -n "$CERT_PASS_REF" && -n "$NOTARY_REF" && -n "$NOTARY_ID_REF" && -n "$NOTARY_ISSUER_REF" ]] \
    || die "--certificate, --certificate-password, --notary-key, --notary-key-id and --notary-issuer are required"
command -v gh >/dev/null || die "gh not found"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
chmod 700 "$WORK"

# A 1Password reference or a local file, copied byte for byte to $2 (op read's stdout is
# text, which can mangle a binary file such as the certificate).
fetch_file() {
    if [[ "$1" == op://* ]]; then
        op read --force --out-file "$2" "$1" >/dev/null
    else
        [[ -f "$1" ]] || die "$1 is not a file or an op:// reference"
        cp "$1" "$2"
    fi
}

# A short text value (a password, an id): from 1Password, or the first line of a file.
fetch_text() {
    if [[ "$1" == op://* ]]; then
        op read --no-newline "$1"
    else
        [[ -f "$1" ]] || die "$1 is not a file or an op:// reference"
        head -n1 "$1" | tr -d '\r\n'
    fi
}

REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
log "Configuring $REPO"

log "Developer ID certificate"
CERT="$WORK/certificate"
fetch_file "$CERT_REF" "$CERT"
fetch_text "$CERT_PASS_REF" > "$CERT.pass"
# Import it exactly as CI will, into a throwaway keychain, before storing anything.
if ! IDENTITY="$(MACOS_CERTIFICATE_P12="$(base64 < "$CERT")" \
    MACOS_CERTIFICATE_PASSWORD="$(cat "$CERT.pass")" \
    "$HERE/ci-keychain.sh" create "$WORK/check.keychain-db")"; then
    "$HERE/ci-keychain.sh" delete "$WORK/check.keychain-db"
    die "the certificate does not open with that password (re-export it, or check the password item)"
fi
"$HERE/ci-keychain.sh" delete "$WORK/check.keychain-db"
[[ -n "$IDENTITY" ]] || die "the certificate holds no Developer ID Application identity with its private key"
log "Certificate OK: $IDENTITY"
base64 < "$CERT" | gh secret set MACOS_CERTIFICATE_P12 --repo "$REPO"
gh secret set MACOS_CERTIFICATE_PASSWORD --repo "$REPO" < "$CERT.pass"

log "Notarization API key"
NOTARY="$WORK/notary"
fetch_file "$NOTARY_REF" "$NOTARY"
grep -q "BEGIN PRIVATE KEY" "$NOTARY" || die "the notary key is not an App Store Connect .p8 key"
base64 < "$NOTARY" | gh secret set NOTARY_API_KEY_P8 --repo "$REPO"
fetch_text "$NOTARY_ID_REF" | gh secret set NOTARY_API_KEY_ID --repo "$REPO"
fetch_text "$NOTARY_ISSUER_REF" | gh secret set NOTARY_API_ISSUER_ID --repo "$REPO"

log "Sparkle update signing key"
(cd "$MACOS" && swift package resolve >/dev/null)
GENERATE_KEYS="$(find "$MACOS/.build/artifacts" -path '*/bin/generate_keys' -type f | head -n1)"
[[ -x "$GENERATE_KEYS" ]] || die "generate_keys not found under macos/.build/artifacts"
"$GENERATE_KEYS" --account thurm >/dev/null
PUBLIC="$("$GENERATE_KEYS" --account thurm -p)"
[[ -n "$PUBLIC" ]] || die "generate_keys printed no public key"
gh variable set SPARKLE_PUBLIC_KEY --repo "$REPO" --body "$PUBLIC"
"$GENERATE_KEYS" --account thurm -x "$WORK/sparkle" >/dev/null
gh secret set SPARKLE_PRIVATE_KEY --repo "$REPO" < "$WORK/sparkle"

log "GitHub Pages (docs and appcast), deployed by pages.yml"
if ! gh api -X POST "repos/$REPO/pages" -f build_type=workflow >/dev/null 2>&1; then
    gh api -X PUT "repos/$REPO/pages" -f build_type=workflow >/dev/null
fi
# pages.yml deploys from main; v* tags are allowed too, for a deploy started from a release.
gh api -X PUT "repos/$REPO/environments/github-pages" \
    -F 'deployment_branch_policy[protected_branches]=false' \
    -F 'deployment_branch_policy[custom_branch_policies]=true' >/dev/null
for policy in "main branch" "v* tag"; do
    set -- $policy
    gh api -X POST "repos/$REPO/environments/github-pages/deployment-branch-policies" \
        -f name="$1" -f type="$2" >/dev/null 2>&1 || true # already there
done
URL="$(gh api "repos/$REPO/pages" -q .html_url)"
log "Done. Appcast: ${URL}appcast.xml (published once the first Release run finishes)"
echo "Apps built before a repository rename keep the old feed URL; set the SPARKLE_FEED_URL"
echo "repository variable to pin it."
