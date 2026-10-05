#!/usr/bin/env bash
# A Linux desktop VM on this Mac (Tart, Ubuntu + GNOME) for trying the Linux app.
#
#   scripts/linux/vm.sh create     clone the Ubuntu image, size it, install GNOME and the build tools
#   scripts/linux/vm.sh build      build thurm-gtk, thurmd and thurm inside the VM and install them
#   scripts/linux/vm.sh up         boot the VM with its desktop window (logs in automatically)
#   scripts/linux/vm.sh launch     open the app in the VM's desktop session
#   scripts/linux/vm.sh exec CMD   run a command in the VM (as the desktop user)
#   scripts/linux/vm.sh screenshot PATH   save a PNG of the VM's window
#   scripts/linux/vm.sh down       shut the VM down
#   scripts/linux/vm.sh delete     remove the VM
#
# The repository is shared into the guest at ~/thurm (virtiofs); builds go to ~/thurm-target
# in the guest so they never touch this Mac's target/.
set -euo pipefail

VM="${THURM_VM:-thurm-linux-desktop}"
IMAGE="${THURM_VM_IMAGE:-ghcr.io/cirruslabs/ubuntu:latest}"
repo="$(cd "$(dirname "$0")/../.." && pwd)"
log_dir="${TMPDIR:-/tmp}/thurm-vm"
mkdir -p "$log_dir"

running() { tart list | awk -v vm="$VM" '$1 == "local" && $2 == vm && $NF == "running" { f = 1 } END { exit !f }'; }

guest() { tart exec "$VM" sudo -u admin -i bash -lc "$*"; }

wait_agent() {
    for _ in $(seq 1 120); do
        if tart exec "$VM" true 2>/dev/null; then return 0; fi
        sleep 2
    done
    echo "the VM's guest agent did not come up" >&2
    exit 1
}

start() {
    local graphics="$1"
    if running; then return 0; fi
    local args=(--dir="$repo:tag=thurm")
    [ "$graphics" = yes ] || args+=(--no-graphics)
    nohup tart run "$VM" "${args[@]}" >"$log_dir/$VM.log" 2>&1 &
    wait_agent
    # The share, at ~/thurm (fstab makes later boots mount it too).
    tart exec "$VM" sudo bash -c '
        mkdir -p /home/admin/thurm
        grep -q "^thurm " /etc/fstab || echo "thurm /home/admin/thurm virtiofs rw,nofail 0 0" >> /etc/fstab
        mountpoint -q /home/admin/thurm || mount /home/admin/thurm'
}

stop() {
    if running; then
        tart exec "$VM" sudo systemctl poweroff >/dev/null 2>&1 || true
        for _ in $(seq 1 60); do running || return 0; sleep 1; done
        tart stop "$VM"
    fi
}

case "${1:-}" in
create)
    if ! tart list | awk '{print $2}' | grep -qx "$VM"; then
        tart clone "$IMAGE" "$VM"
    fi
    tart set "$VM" --cpu "${THURM_VM_CPU:-6}" --memory "${THURM_VM_MEMORY:-8192}" \
        --disk-size "${THURM_VM_DISK:-50}" --display 1600x1000
    start no
    # The disk was grown: let the root filesystem use it.
    tart exec "$VM" sudo bash -c 'growpart /dev/vda 2 >/dev/null 2>&1 || true; resize2fs "$(findmnt -no SOURCE /)" >/dev/null 2>&1 || true'
    guest "~/thurm/scripts/linux/provision.sh"
    # GNOME (Wayland) with automatic login for `admin`, so `up` lands on the desktop.
    tart exec "$VM" sudo bash -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get install -y -q ubuntu-desktop-minimal gnome-terminal gnome-screenshot
        mkdir -p /etc/gdm3
        cat > /etc/gdm3/custom.conf <<EOF
[daemon]
AutomaticLoginEnable=true
AutomaticLogin=admin
WaylandEnable=true
EOF
        systemctl set-default graphical.target'
    # No first-login wizard, screen lock or blanking in a test VM.
    guest "mkdir -p ~/.config && echo yes > ~/.config/gnome-initial-setup-done"
    guest "dbus-run-session -- bash -c 'gsettings set org.gnome.desktop.session idle-delay 0; gsettings set org.gnome.desktop.screensaver lock-enabled false'" || true
    stop
    echo "created $VM; next: scripts/linux/vm.sh build && scripts/linux/vm.sh up"
    ;;
build)
    start "${THURM_VM_GRAPHICS:-no}"
    guest "~/thurm/scripts/linux/install.sh"
    ;;
up)
    start yes
    echo "$VM is up; build with: scripts/linux/vm.sh build"
    ;;
exec)
    shift
    start no
    guest "$*"
    ;;
screenshot)
    # The VM's window on this Mac (needs `up`): no screenshot permission games inside GNOME.
    out="${2:?usage: vm.sh screenshot PATH}"
    wid="$(swift - <<'SWIFT'
import CoreGraphics
let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
let w = list.first { ($0[kCGWindowOwnerName as String] as? String)?.lowercased() == "tart"
    && ($0[kCGWindowLayer as String] as? Int) == 0 }
print(w?[kCGWindowNumber as String] as? Int ?? 0)
SWIFT
)"
    [ "$wid" != 0 ] || { echo "no Tart window on screen; run: scripts/linux/vm.sh up" >&2; exit 1; }
    screencapture -x -o -l "$wid" "$out"
    echo "$out"
    ;;
launch)
    # Start the app in the desktop session (as if opened from the app grid).
    tart exec "$VM" sudo -u admin env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 \
        DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
        bash -lc 'nohup gtk-launch rs.thurm.Thurm >/dev/null 2>&1 &'
    ;;
down) stop ;;
delete)
    stop
    tart delete "$VM"
    ;;
*)
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
