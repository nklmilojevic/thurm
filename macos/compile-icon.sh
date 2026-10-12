#!/usr/bin/env bash
# Compile the Icon Composer icon into a bundle's Resources: Assets.car for macOS 26 and later,
# AppIcon.icns for older systems.
#
#   macos/compile-icon.sh RESOURCES_DIR
set -euo pipefail

[[ $# == 1 ]] || { echo "usage: $0 RESOURCES_DIR" >&2; exit 2; }
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$1"
mkdir -p "$OUT"

# actool insists on writing the partial Info.plist; Info.plist already has its keys.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

xcrun actool "$HERE/Resources/AppIcon.icon" --compile "$OUT" \
    --platform macosx --minimum-deployment-target 14.0 --app-icon AppIcon --include-all-app-icons \
    --output-partial-info-plist "$WORK/AppIcon.plist" --errors --warnings >/dev/null

for f in Assets.car AppIcon.icns; do
    [[ -s "$OUT/$f" ]] || { echo "error: actool did not produce $f" >&2; exit 1; }
done
