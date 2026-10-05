---
title: Install Thurm
---

## Requirements

On macOS, Thurm runs on Apple silicon with macOS 14 or later. Intel Macs are not
supported. For Linux, see [Linux](#linux).

## Download

1. Download [Thurm.dmg](https://github.com/nklmilojevic/thurm/releases/latest/download/Thurm.dmg).
2. Open it and drag **Thurm** to **Applications**.
3. Eject the disk image, then open Thurm from Applications.

Release builds are signed with a Developer ID and notarized by Apple. On the first
launch, macOS asks you to confirm that you want to open an app downloaded from
the internet. Select **Open**.

Don't run Thurm from the disk image. macOS runs apps opened there from a
temporary, read-only location. The daemon, the command-line tool and the login
item would point at a path that disappears when the image is ejected, and
updates can't replace the app.

To get nightly builds of `main` instead, download
[Thurm-tip.dmg](https://github.com/nklmilojevic/thurm/releases/download/tip/Thurm-tip.dmg),
or select **Thurm > Update Channel > Tip** in an installed Thurm.

## Linux

The Linux app needs GTK 4.14 and libadwaita 1.5 or later: Ubuntu 24.04, Debian 13,
Fedora 40, Arch Linux, or newer. Packages are built for x86_64 and arm64.

On Ubuntu and Debian, download
[thurm-amd64.deb](https://github.com/nklmilojevic/thurm/releases/latest/download/thurm-amd64.deb)
(or [thurm-arm64.deb](https://github.com/nklmilojevic/thurm/releases/latest/download/thurm-arm64.deb))
and install it:

```sh
sudo apt install ./thurm-amd64.deb
```

On Fedora, download
[thurm-x86_64.rpm](https://github.com/nklmilojevic/thurm/releases/latest/download/thurm-x86_64.rpm)
(or [thurm-aarch64.rpm](https://github.com/nklmilojevic/thurm/releases/latest/download/thurm-aarch64.rpm))
and install it:

```sh
sudo dnf install ./thurm-x86_64.rpm
```

On Arch Linux and CachyOS (x86_64), install the package from the latest release:

```sh
sudo pacman -U https://github.com/nklmilojevic/thurm/releases/latest/download/thurm-x86_64.pkg.tar.zst
```

Each package installs the app (`thurm-gtk`, listed as **Thurm** in the desktop's app list),
the daemon `thurmd`, and the `thurm` command-line tool. Updates come with new packages:
install the new `.deb`, `.rpm` or `.pkg.tar.zst` the same way. Nightly builds of `main`
are on the [tip release](https://github.com/nklmilojevic/thurm/releases/tag/tip), with the
same file names.

Shortcuts that use Cmd on macOS use Ctrl+Shift on Linux. For example, Ctrl+Shift+T opens a
tab and Ctrl+Shift+D splits the pane. To start the daemon at login, before the app is
opened, run `thurm daemon install-systemd`.

## Build from source

To build from source, install Xcode 26 or later, Rust stable with the
`aarch64-apple-darwin` target, and Zig 0.16.0.

Clone the repository and enter its directory:

```sh
git clone https://github.com/nklmilojevic/thurm.git
cd thurm
```

With Nix installed, the development shell provides Rust, Zig, Python, and `gh`:

```sh
nix develop
./macos/build.sh
open macos/build/Thurm.app
```

Without Nix, put Rust and Zig on `PATH`, then run the last two commands.
The first build needs network access to download the pinned Ghostty source and
Swift dependencies. The script signs local builds with an ad hoc signature.

To build, install, and open the app:

```sh
./macos/build.sh --install
```

This replaces the app in `/Applications`, or in `~/Applications` if the first
path is not writable.

## Install the command-line tool

In the app, select **Thurm > Integrations > Install Command-Line Tool**.
The build script also links `thurm` into `~/.local/bin` when that directory
exists and the destination is not a regular file. Add that directory to your
shell's `PATH` if needed:

```sh
export PATH="$HOME/.local/bin:$PATH"
thurm --version
thurm --help
```

Add the `export` line to your shell configuration to keep it for new shells.
Open Thurm, then run `thurm list` to see your panes.

## Updates

All builds are listed on [GitHub Releases](https://github.com/nklmilojevic/thurm/releases).
Signed release builds use Sparkle for updates. Select **Thurm > Update Channel**
to choose Release or Tip. By default, `updates.channel = "auto"` follows the
installed build. Release receives tagged versions. Tip receives a nightly
build of `main`, made from the newest commit that passed CI. Local builds do not have an
update feed and public signing key unless you configure them during the build.
See [update settings](/configuration/#updates) for background checks and downloads.

Continue with [Use Thurm](/usage/) and [Configuration](/configuration/).
