#!/usr/bin/env bash
# Prepares the AUR package `thurm` for a release: copies linux/arch/PKGBUILD and its LICENSE
# into a clone of ssh://aur@aur.archlinux.org/thurm.git, sets pkgver and the release
# tarball's checksum, regenerates .SRCINFO and commits. Pushing is left to you (`git push` in
# AUR_DIR). Needs makepkg (run it on Arch).
#   scripts/linux/aur-update.sh VERSION AUR_DIR      e.g. 0.3.0 ~/dev/personal/aur-thurm
set -euo pipefail

version="${1:?version, e.g. 0.3.0}"
aur="$(cd "${2:?AUR clone}" && pwd)"
repo="$(cd "$(dirname "$0")/../.." && pwd)"
[ -d "$aur/.git" ] || { echo "$aur is not a git clone of the AUR package" >&2; exit 1; }

url="https://github.com/nklmilojevic/thurm/archive/refs/tags/v$version.tar.gz"
sum="$(curl -fsSL "$url" | sha256sum | cut -d' ' -f1)"

sed -e "s/^pkgver=.*/pkgver=$version/" -e "s/^pkgrel=.*/pkgrel=1/" \
    -e "s/^sha256sums=.*/sha256sums=('$sum')/" "$repo/linux/arch/PKGBUILD" > "$aur/PKGBUILD"
cp "$repo/linux/arch/LICENSE" "$aur/LICENSE"
cd "$aur"
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO LICENSE
if git diff --cached --quiet; then
    echo "nothing changed for $version"
    exit 0
fi
git commit -q -m "Update to $version"
git log --oneline -1
echo "Review it, then: git -C $aur push"
