#!/bin/sh
# Installs Thurm's command-line tools on a Linux host: thurm, thurmd and thurm-tailnet, from the
# latest release. Usage: curl -fsSL https://docs.thurm.rs/install.sh | sh
#
# The files go to ~/.local/share/thurm/bin, the same place `thurm remote add` installs them, and
# ~/.local/bin/thurm links to them. THURM_INSTALL_URL replaces the release location (for testing).
set -eu

BASE="${THURM_INSTALL_URL:-https://github.com/nklmilojevic/thurm/releases/latest/download}"

die() {
  echo "install.sh: $*" >&2
  exit 1
}

case "$(uname -s)" in
  Linux) ;;
  Darwin) die "this is for Linux hosts; on a Mac, install Thurm.app from $BASE/Thurm.dmg" ;;
  *) die "unsupported system: $(uname -s)" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) TARGET=x86_64-unknown-linux-musl ;;
  aarch64 | arm64) TARGET=aarch64-unknown-linux-musl ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

command -v curl >/dev/null 2>&1 || die "curl is needed"
command -v tar >/dev/null 2>&1 || die "tar is needed"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

ARCHIVE="thurm-$TARGET.tar.gz"
echo "Downloading $ARCHIVE"
curl -fsSL "$BASE/$ARCHIVE" -o "$TMP/$ARCHIVE"
curl -fsSL "$BASE/$ARCHIVE.sha256" -o "$TMP/$ARCHIVE.sha256"

WANT="$(awk '{print $1}' "$TMP/$ARCHIVE.sha256")"
GOT="$(sha256 "$TMP/$ARCHIVE")"
[ -n "$WANT" ] && [ "$WANT" = "$GOT" ] || die "checksum mismatch for $ARCHIVE (want $WANT, got $GOT)"

mkdir "$TMP/unpacked"
tar -xzf "$TMP/$ARCHIVE" -C "$TMP/unpacked"
for b in thurm thurmd thurm-tailnet; do
  [ -f "$TMP/unpacked/$b" ] || die "$ARCHIVE has no $b"
done

BIN="$HOME/.local/share/thurm/bin"
mkdir -p "$BIN" "$HOME/.local/bin"
for b in thurm thurmd thurm-tailnet; do
  # Copied beside the old file and renamed over it: a running daemon keeps its binary.
  cp "$TMP/unpacked/$b" "$BIN/.$b.new"
  chmod 755 "$BIN/.$b.new"
  mv -f "$BIN/.$b.new" "$BIN/$b"
done
ln -sfn "$BIN/thurm" "$HOME/.local/bin/thurm"

echo "Installed Thurm to $BIN, with $HOME/.local/bin/thurm linked to it."
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) echo "Add $HOME/.local/bin to your PATH to run thurm." ;;
esac
cat <<EOF

Next steps:
  thurm daemon install-systemd    start thurmd now and at login
  sudo loginctl enable-linger "\$USER"   keep thurmd running after you log out
  thurm pair                      pair your iPhone (see the Linux server guide)
EOF
