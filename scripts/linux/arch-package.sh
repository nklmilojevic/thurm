#!/usr/bin/env bash
# Builds the Arch Linux package (linux/arch/PKGBUILD) from the working tree, uncommitted
# changes included, instead of the release tarball the PKGBUILD downloads. Run on Arch (or
# CachyOS) with base-devel installed; the package lands in OUT_DIR (default target/arch).
#   scripts/linux/arch-package.sh [OUT_DIR]
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
out="$(mkdir -p "${1:-$repo/target/arch}" && cd "${1:-$repo/target/arch}" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pkgver="$(sed -n 's/^pkgver=//p' "$repo/linux/arch/PKGBUILD")"
cd "$repo"
# Tracked and untracked files, not the ignored ones (target/, node_modules, …), laid out like
# GitHub's tag archive. makepkg uses a source already in SRCDEST instead of downloading it.
git ls-files --cached --others --exclude-standard -z |
    tar --null -T - --transform "s,^,thurm-$pkgver/," -czf "$work/thurm-$pkgver.tar.gz"
sed "s/^sha256sums=.*/sha256sums=('SKIP')/" linux/arch/PKGBUILD > "$work/PKGBUILD"
cd "$work"
PKGDEST="$out" SRCDEST="$work" makepkg --force --cleanbuild --noconfirm
ls -l "$out"
