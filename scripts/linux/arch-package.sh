#!/usr/bin/env bash
# Builds the Arch Linux package (linux/arch/PKGBUILD) from the working tree, uncommitted
# changes included. Run on Arch (or CachyOS) with base-devel and rustup installed; the
# package lands in OUT_DIR (default target/arch).
#   scripts/linux/arch-package.sh [OUT_DIR]
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="$(mkdir -p "${1:-$repo/target/arch}" && cd "${1:-$repo/target/arch}" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cd "$repo"
# Tracked and untracked files, not the ignored ones (target/, node_modules, …).
git ls-files --cached --others --exclude-standard -z |
    tar --null -T - --transform 's,^,thurm/,' -czf "$work/thurm-src.tar.gz"
cp linux/arch/PKGBUILD "$work/"
cd "$work"
PKGDEST="$out" SRCDEST="$work" makepkg --force --cleanbuild --noconfirm
ls -l "$out"
