#!/usr/bin/env bash
# Headless check of remote workspaces against a real ssh host: the Remotes window before
# Thurm is installed there, install, a workspace on the host, its offline notice.
#   scripts/linux/smoke-remote.sh BIN_DIR OUT_DIR SSH_TARGET
# SSH_TARGET must accept this user's key without a prompt; its Thurm install is replaced.
set -euo pipefail
bin="${1:?bin dir}"
out="${2:?out dir}"
target="${3:?ssh target}"
mkdir -p "$out"
rm -f "$out"/*.png
sandbox="$(mktemp -d)"
export XDG_RUNTIME_DIR="$sandbox/run" XDG_CONFIG_HOME="$sandbox/config" XDG_STATE_HOME="$sandbox/state"
export XDG_CACHE_HOME="$sandbox/cache"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/thurm" && chmod 700 "$XDG_RUNTIME_DIR"
export DISPLAY=:93 GDK_BACKEND=x11 NO_AT_BRIDGE=1 GTK_A11Y=none SHELL=/bin/bash PATH="$bin:$PATH"
cat >"$XDG_CONFIG_HOME/thurm/config.toml" <<EOF
[[remote]]
name = "devbox"
host = "$target"
EOF
# A clean host: no Thurm yet.
ssh -o BatchMode=yes "$target" 'pkill -x thurmd; sleep 0.5; rm -rf ~/.local/share/thurm ~/.local/state/thurm ~/.local/bin/thurm ~/.local/bin/thurmd' || true
Xvfb :93 -screen 0 1400x900x24 >/dev/null 2>&1 &
xvfb=$!
app=
cleanup() {
    [ -n "$app" ] && kill "$app" 2>/dev/null || true
    "$bin/thurm" daemon stop >/dev/null 2>&1 || true
    kill "$xvfb" 2>/dev/null || true
    rm -rf "$sandbox"
}
trap cleanup EXIT
sleep 1
dbus-run-session -- "$bin/thurm-gtk" >>"$out/thurm-gtk.log" 2>&1 &
app=$!
for _ in $(seq 1 50); do
    xdotool search --onlyvisible --class rs.thurm.Thurm >/dev/null 2>&1 && break
    sleep 0.2
done
if ! xdotool search --onlyvisible --class rs.thurm.Thurm >/dev/null 2>&1; then
    echo "thurm-gtk showed no window:" >&2
    tail -n 40 "$out/thurm-gtk.log" >&2 || true
    exit 1
fi
sleep 4
n=0
shot() {
    n=$((n + 1))
    sleep "${2:-1}"
    import -window root "$out/$(printf %02d "$n")-$1.png"
}
palette() {
    xdotool key ctrl+shift+p
    sleep 0.4
    xdotool type --delay 20 "$1"
    xdotool key Return
}

palette 'remotes'
shot remotes-not-installed 4
# No window manager here: GTK does not take keys in a second window, so the Remotes window
# stays up; the main window gets the focus back.
main="$(xdotool search --onlyvisible --class rs.thurm.Thurm | head -1)"
sleep 0.5
xdotool windowfocus --sync "$main"
"$bin/thurm" remote install -y devbox >"$out/install.txt" 2>&1 || true
"$bin/thurm" remote status devbox >"$out/status.txt" 2>&1 || true
# The tunnel comes up on its next retry.
for _ in $(seq 1 40); do
    "$bin/thurm" remote status devbox 2>/dev/null | grep -q connected && break
    sleep 0.5
done
# Hand a local repository off to the host: a tab there, in a worktree of it.
xdotool type --delay 12 'mkdir -p /tmp/handoff-repo && cd /tmp/handoff-repo && git init -q -b main && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m init && echo ready'
xdotool key Return
sleep 2
palette 'hand off'
sleep 1
xdotool type --delay 20 'shell'
xdotool key Return
shot handoff 8
xdotool key alt+1
sleep 1
"$bin/thurm" --remote devbox list >>"$out/remote-list.txt" 2>&1 || true
sleep 1
xdotool key ctrl+alt+o
sleep 0.5
xdotool type --delay 20 'on devbox'
xdotool key Return
sleep 2
xdotool type --delay 15 'echo "on $(hostname)"'
xdotool key Return
shot remote-workspace 2
"$bin/thurm" --remote devbox list >"$out/remote-list.txt" 2>&1 || true
# Cut the connection: the panes show the offline notice.
ssh -o BatchMode=yes "$target" 'pkill -x thurmd' || true
shot offline 4
echo "screenshots in $out"
