#!/usr/bin/env bash
# Prepares an Ubuntu 24.04+ guest to build and run the Linux app (crates/thurm-gtk):
# GTK 4 headers and the Rust toolchain from rust-toolchain.toml (libghostty-vt comes prebuilt).
# Idempotent. Run as the guest user (it uses sudo for apt).
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
rust_version="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo/rust-toolchain.toml")"

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

echo "provisioned: rust $rust_version, gtk $(pkg-config --modversion gtk4)"
