#!/usr/bin/env bash
# Builds the Linux app (thurm-gtk) with thurmd and thurm and installs them for the current
# user: ~/.local/bin, plus a desktop entry and icon so GNOME lists "Thurm".
# PROFILE=debug for a faster, unoptimized build.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
profile="${PROFILE:-release}"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/thurm-target}"

cd "$repo"
"$repo/scripts/linux/zig-prefetch.sh"
flags=()
[ "$profile" = release ] && flags+=(--release)
cargo build "${flags[@]}" -p thurm-gtk
# thurm and thurmd static (musl), like the release's Linux builds: the app copies them onto
# remote hosts (Thurm › Remotes… › Install).
musl="$(uname -m | sed 's/arm64/aarch64/')-unknown-linux-musl"
cargo build "${flags[@]}" --target "$musl" -p thurm-daemon -p thurm-cli

bin="$HOME/.local/bin"
mkdir -p "$bin" "$HOME/.local/share/applications" "$HOME/.local/share/icons/hicolor/256x256/apps"
for b in thurm-gtk "$musl/$profile/thurmd" "$musl/$profile/thurm"; do
    src="$CARGO_TARGET_DIR/$b"
    [ "$b" = thurm-gtk ] && src="$CARGO_TARGET_DIR/$profile/$b"
    name="$(basename "$b")"
    install -m 755 "$src" "$bin/$name.new"
    mv -f "$bin/$name.new" "$bin/$name"
done
sed "s|^Exec=thurm-gtk|Exec=$bin/thurm-gtk|" linux/rs.thurm.Thurm.desktop \
    >"$HOME/.local/share/applications/rs.thurm.Thurm.desktop"
install -m 644 linux/rs.thurm.Thurm.png "$HOME/.local/share/icons/hicolor/256x256/apps/"
# Shared files: the bundled fonts (JetBrains Mono, Symbols Nerd Font), icons and the Claude skill.
share="$HOME/.local/share/thurm"
mkdir -p "$share/fonts" "$share/skills"
install -m 644 macos/Resources/fonts/*.ttf macos/Resources/fonts/*LICENSE* macos/Resources/fonts/*OFL* "$share/fonts/"
rm -rf "$share/skills/thurm" && cp -R skills/thurm "$share/skills/"
rm -rf "$share/icons" && cp -R linux/icons "$share/icons"
update-desktop-database -q "$HOME/.local/share/applications" 2>/dev/null || true
gtk-update-icon-cache -q "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
echo "installed thurm-gtk, thurmd and thurm ($profile) in $bin"
