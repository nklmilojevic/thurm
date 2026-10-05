#!/usr/bin/env bash
# Builds the .deb and .rpm of the Linux app (linux/nfpm.yaml) from built binaries, for this
# machine's architecture. Needs nfpm on PATH.
#   scripts/linux/package.sh VERSION THURM_GTK MUSL_DIR OUT_DIR
# VERSION is the workspace version (0.3.0), or 0.3.0+tip.<commit count>.<sha> for a tip
# build: it sorts after that release, before the next, and by commit among tips (dpkg, rpm
# and pacman alike). THURM_GTK is the thurm-gtk binary, MUSL_DIR the
# directory with the static thurm and thurmd; all three built with the same THURM_BUILD.
# OUT_DIR gets the versioned packages, copies with fixed names (thurm-amd64.deb,
# thurm-x86_64.rpm, ...) for releases/latest/download links, and a .sha256 for each.
set -euo pipefail

version="${1:?version}"
gtk="$(realpath "${2:?thurm-gtk binary}")"
musl="$(realpath "${3:?musl bin dir}")"
out="$(mkdir -p "${4:?out dir}" && cd "$4" && pwd)"
repo="$(cd "$(dirname "$0")/../.." && pwd)"

case "$(uname -m)" in
    x86_64) deb_arch=amd64 rpm_arch=x86_64 ;;
    aarch64 | arm64) deb_arch=arm64 rpm_arch=aarch64 ;;
    *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

cd "$repo"
[ -f THIRD_PARTY_LICENSES ] || python3 scripts/third-party-licenses.py
export NFPM_ARCH="$deb_arch" THURM_PKG_VERSION="$version" THURM_GTK_BIN="$gtk" THURM_MUSL_DIR="$musl"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
for packager in deb rpm; do
    nfpm package --config linux/nfpm.yaml --packager "$packager" --target "$work/"
done

cd "$work"
# `+` stays in the package version but not in file names (release asset names).
shopt -s nullglob
for f in *+*; do
    mv -- "$f" "${f//+/-}"
done
deb=(*.deb) rpm=(*.rpm)
cp "${deb[0]}" "thurm-$deb_arch.deb"
cp "${rpm[0]}" "thurm-$rpm_arch.rpm"
for f in *.deb *.rpm; do
    sha256sum "$f" > "$f.sha256"
done
mv -- * "$out/"
ls -l "$out"
