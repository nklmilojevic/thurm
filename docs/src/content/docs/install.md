---
title: Install Thurm
---

## Requirements

Thurm runs on Apple silicon with macOS 14 or later. Intel Macs are not supported.
To build from source, install Xcode 26 or later, Rust stable with the
`aarch64-apple-darwin` target, and Zig 0.16.0.

## Build from source

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

## Release builds and updates

Check [GitHub Releases](https://github.com/nklmilojevic/thurm/releases) for
published builds. If no build is listed, use the source instructions above.
For a published DMG, open it and copy Thurm to Applications.

Signed release builds use Sparkle for updates. Select **Thurm > Update Channel**
to choose Release or Tip. By default, `updates.channel = "auto"` follows the
installed build. Release receives tagged versions. Tip receives a nightly
build of `main`, made from the newest commit that passed CI. Local builds do not have an
update feed and public signing key unless you configure them during the build.
See [update settings](/thurm/configuration/#updates) for background checks and downloads.

Continue with [Use Thurm](/thurm/usage/) and [Configuration](/thurm/configuration/).
