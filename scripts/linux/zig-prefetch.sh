#!/usr/bin/env bash
# Puts Ghostty's Zig packages into Zig's global cache before the libghostty-vt build, from
# downloads made with curl and git. Zig's own HTTP client stalls or fails its TLS setup in some
# VMs (TlsInitializationFailed); `zig fetch` of a local file or directory needs no network.
# The list is the one the Nix build uses (nix/ghostty-zig-deps.nix, for the pinned Ghostty
# commit). Packages already in the cache are skipped.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
cache="${ZIG_GLOBAL_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/zig}"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/root"
touch "$scratch/root/build.zig"

fetch_local() { (cd "$scratch/root" && zig fetch --global-cache-dir "$cache" "$1"); }

# Each package: `"<package hash>" = fetchzip|fetchurl|fetchZigGit {`, then its `url` (and `rev`
# for git), up to `};`.
awk '/^    "[^"]+" = fetch[A-Za-z]+ \{$/ { split($0, a, "\""); hash = a[2]; kind = $3; url = rev = ""; next }
     hash && /^      url = "/ { split($0, a, "\""); url = a[2] }
     hash && /^      rev = "/ { split($0, a, "\""); rev = a[2] }
     hash && /^    };$/ { print hash, kind, url, rev; hash = "" }' \
    "$repo/nix/ghostty-zig-deps.nix" |
    while read -r hash kind url rev; do
        [ -d "$cache/p/$hash" ] && continue
        src="$scratch/src"
        rm -rf "$src"
        case "$kind" in
        fetchZigGit)
            git init -q "$src"
            git -C "$src" fetch -q --depth 1 "$url" "$rev"
            git -C "$src" checkout -q FETCH_HEAD
            rm -rf "$src/.git"
            ;;
        *)
            name="$(basename "${url%%\?*}")"
            mkdir -p "$src"
            curl -sSfL --retry 3 -o "$src/$name" "$url"
            src="$src/$name"
            ;;
        esac
        got="$(fetch_local "$src")"
        if [ "$got" != "$hash" ]; then
            echo "error: $url unpacked as $got, expected $hash" >&2
            exit 1
        fi
    done
echo "zig packages ready in $cache/p ($(ls "$cache/p" | wc -l))"
