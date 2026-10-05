#!/usr/bin/env bash
# Headless check of the Linux app: runs thurm-gtk on a virtual X display with its own daemon
# socket, config and state, drives it with the keyboard and the CLI, and saves a screenshot
# after each step to OUT_DIR (01-…png, 02-…png, …).
#   scripts/linux/smoke.sh BIN_DIR OUT_DIR
set -euo pipefail
bin="${1:?bin dir}"
out="${2:?out dir}"
mkdir -p "$out"
rm -f "$out"/*.png
sandbox="$(mktemp -d)"
export XDG_RUNTIME_DIR="$sandbox/run" XDG_CONFIG_HOME="$sandbox/config" XDG_STATE_HOME="$sandbox/state"
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
export DISPLAY=:97 GDK_BACKEND=x11 NO_AT_BRIDGE=1 GTK_A11Y=none SHELL=/bin/bash
Xvfb :97 -screen 0 1400x900x24 >/dev/null 2>&1 &
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

launch() {
    dbus-run-session -- "$bin/thurm-gtk" >>"$out/thurm-gtk.log" 2>&1 &
    app=$!
    for _ in $(seq 1 50); do
        xdotool search --onlyvisible --class rs.thurm.Thurm >/dev/null 2>&1 && break
        sleep 0.2
    done
    sleep 1.5
}
n=0
shot() {
    n=$((n + 1))
    sleep "${2:-1}"
    import -window root "$out/$(printf %02d "$n")-$1.png"
}
typeln() {
    xdotool type --delay 15 "$1"
    xdotool key Return
}

launch
typeln 'printf "\e[1mbold\e[0m \e[3mitalic\e[0m \e[4:3mcurly\e[0m \e[9mstrike\e[0m \e[31mred\e[0m \e[44m blue \e[0m ünïcødé 日本語 ✓ -> != ===\n"'
typeln 'printf "╭────╮ ┏━━┓ ╔══╗ █▓▒░ ▀▄▌▐\n│ ab │ ┃  ┃ ║  ║ ┄┄┄┄ ╌╌╌╌\n╰────╯ ┗━━┛ ╚══╝   \n"'
# A kitty graphics image: 32x16 RGBA, transmitted and displayed.
typeln "python3 -c \"import base64,sys;d=bytes([255,80,40,255]*(32*16));sys.stdout.write('\\x1b_Ga=T,f=32,s=32,v=16;'+base64.b64encode(d).decode()+'\\x1b\\\\\\\\');print()\""
shot text-boxes-image
xdotool key ctrl+shift+d
sleep 1
typeln 'ls --color=always /'
xdotool key ctrl+shift+e
sleep 1
typeln 'echo third pane'
shot splits
xdotool key ctrl+shift+t
sleep 1
typeln 'echo second tab'
shot new-tab
xdotool key alt+1
sleep 0.5
xdotool key ctrl+shift+p
sleep 0.5
xdotool type --delay 30 'zoom'
shot palette 0.5
xdotool key Return
shot zoomed
xdotool key ctrl+shift+Return
xdotool key ctrl+shift+f
sleep 0.5
xdotool type --delay 30 'pane'
xdotool key Return
shot find
xdotool key Escape
"$bin/thurm" list >"$out/list.txt" 2>&1 || true

# Quit (the window's close button) and relaunch: the shells come back in the same layout.
xdotool key ctrl+shift+q
sleep 1.5
app=
launch
shot restored
echo "screenshots in $out"
