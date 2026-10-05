#!/usr/bin/env bash
# Prepares an Ubuntu 24.04+ guest to build and run the Linux app (crates/thurm-gtk):
# GTK 4 headers, the Rust toolchain from rust-toolchain.toml, and Zig 0.16 for libghostty-vt.
# Idempotent. Run as the guest user (it uses sudo for apt).
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
rust_version="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo/rust-toolchain.toml")"
zig_version=0.16.0

case "$(uname -m)" in
    aarch64 | arm64) zig_arch=aarch64 ;;
    x86_64) zig_arch=x86_64 ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -q
sudo apt-get install -y -q \
    build-essential pkg-config git curl xz-utils ca-certificates \
    libgtk-4-dev libadwaita-1-dev fonts-dejavu-core fonts-noto-color-emoji fonts-jetbrains-mono \
    xvfb xdotool imagemagick dbus-x11 musl-tools fish zsh

if ! command -v rustup >/dev/null && [ ! -x "$HOME/.cargo/bin/rustup" ]; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
fi
"$HOME/.cargo/bin/rustup" toolchain install "$rust_version" --profile minimal \
    --target "$(uname -m | sed 's/arm64/aarch64/')-unknown-linux-musl"

zig_dir="$HOME/.local/zig-$zig_version"
if [ ! -x "$zig_dir/zig" ]; then
    mkdir -p "$zig_dir"
    curl -sSfL "https://ziglang.org/download/$zig_version/zig-$zig_arch-linux-$zig_version.tar.xz" |
        tar -xJ -C "$zig_dir" --strip-components=1
fi
mkdir -p "$HOME/.local/bin"
ln -sf "$zig_dir/zig" "$HOME/.local/bin/zig"

echo "provisioned: rust $rust_version, zig $("$HOME/.local/bin/zig" version), gtk $(pkg-config --modversion gtk4)"
