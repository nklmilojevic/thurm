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

# `name = "<package hash>"` followed by its `url = "..."`.
awk '/name = "/ && !hash { split($0, a, "\""); hash = a[2]; next }
     /url = "/ && hash { split($0, a, "\""); print hash, a[2]; hash = "" }' \
    "$repo/nix/ghostty-zig-deps.nix" |
    while read -r hash url; do
        [ -d "$cache/p/$hash" ] && continue
        src="$scratch/src"
        rm -rf "$src"
        case "$url" in
        git+*)
            # git+https://host/repo[.git]#<commit>
            remote="${url#git+}"
            rev="${remote##*#}"
            remote="${remote%%#*}"
            remote="${remote%%\?*}"
            git init -q "$src"
            git -C "$src" fetch -q --depth 1 "$remote" "$rev"
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
        [ "$got" = "$hash" ] || echo "warning: $url unpacked as $got, expected $hash" >&2
    done
echo "zig packages ready in $cache/p ($(ls "$cache/p" | wc -l))"
