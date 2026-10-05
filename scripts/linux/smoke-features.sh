#!/usr/bin/env bash
# Headless check of the app's panels: agent status (simulated with `thurm agent-hook`),
# workspaces and the switcher, the theme browser's live preview, Processes & Ports, tab
# completion and link hover. Screenshots go to OUT_DIR.
#   scripts/linux/smoke-features.sh BIN_DIR OUT_DIR
set -euo pipefail
bin="${1:?bin dir}"
out="${2:?out dir}"
mkdir -p "$out"
rm -f "$out"/*.png
sandbox="$(mktemp -d)"
export XDG_RUNTIME_DIR="$sandbox/run" XDG_CONFIG_HOME="$sandbox/config" XDG_STATE_HOME="$sandbox/state"
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
export DISPLAY=:98 GDK_BACKEND=x11 NO_AT_BRIDGE=1 GTK_A11Y=none SHELL=/bin/bash PATH="$bin:$PATH"
Xvfb :98 -screen 0 1400x900x24 >/dev/null 2>&1 &
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
dbus-run-session -- bash -c 'echo "$DBUS_SESSION_BUS_ADDRESS" >"$0"; exec "$1"' "$sandbox/bus" "$bin/thurm-gtk" >>"$out/thurm-gtk.log" 2>&1 &
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
win="$(xdotool search --onlyvisible --class rs.thurm.Thurm | head -1)"
sleep 1.5
n=0
shot() {
    n=$((n + 1))
    sleep "${2:-1}"
    import -window root "$out/$(printf %02d "$n")-$1.png"
}
typeln() {
    xdotool type --delay 12 "$1"
    xdotool key Return
}

# An "agent" in the first pane: a foreground process named claude (how the daemon recognizes
# agents) sending a prompt (working), then a permission request (needs input), then waiting.
mkdir -p "$sandbox/bin"
cat >"$sandbox/bin/claude" <<'AGENT'
#!/bin/bash
echo '{"session_id":"s1","prompt":"Fix the login bug"}' | thurm agent-hook claude UserPromptSubmit
sleep 1
echo '{"session_id":"s1","message":"Claude needs your permission to use Bash"}' | thurm agent-hook claude Notification
echo "fake agent waiting"
sleep 600
AGENT
chmod +x "$sandbox/bin/claude"
typeln "clear; $sandbox/bin/claude"
sleep 2
xdotool key ctrl+shift+t
sleep 1
typeln 'echo see https://example.com/docs for more'
shot agent-status
# Ctrl over the link underlines it.
eval "$(xdotool getwindowgeometry --shell "$win")"
# The URL on the output line (row 2, columns 4-26): ~15 px cells, 8 px padding, 245 px sidebar.
xdotool mousemove $((X + 360)) $((Y + 90))
xdotool keydown ctrl
xdotool mousemove $((X + 365)) $((Y + 91))
shot link-hover 0.5
xdotool keyup ctrl
# Tab completion: one match completes, several open the list.
typeln 'cd /tmp && mkdir -p thurm-cmp/alpha-one thurm-cmp/alpha-two thurm-cmp/beta && cd thurm-cmp'
xdotool type --delay 20 'ls be'
xdotool key Tab
sleep 0.5
xdotool key ctrl+u
xdotool type --delay 20 'ls al'
xdotool key Tab
shot completion
xdotool key Escape
xdotool key ctrl+u
# Workspaces.
xdotool key ctrl+shift+n
shot new-workspace 1.5
xdotool key ctrl+alt+o
shot switcher
xdotool key Escape
# Theme browser: moving the selection previews the theme.
xdotool key ctrl+shift+p
sleep 0.4
xdotool type --delay 20 'browse'
xdotool key Return
sleep 0.6
xdotool key Down Down Down
shot theme-preview
xdotool key Escape
# Processes & Ports.
xdotool key ctrl+shift+p
sleep 0.4
xdotool type --delay 20 'processes'
xdotool key Return
shot processes 2.5
# The quick terminal (toggled like a global hotkey would).
DBUS_SESSION_BUS_ADDRESS="$(cat "$sandbox/bus")" "$bin/thurm-gtk" --quick-terminal >>"$out/thurm-gtk.log" 2>&1 &
shot quick-terminal 2.5
echo "screenshots in $out"
