---
title: Linux server
---

A Linux host runs `thurmd` in the background, so its sessions outlive your
connections. You reach it from the Thurm app on your iPhone or iPad, or from
the Mac app's remote workspaces. Nothing here needs a desktop session or a
display.

## Install

The install script downloads the latest release for `x86_64` or `aarch64`,
checks its SHA-256 sum, and installs `thurm`, `thurmd` and `thurm-tailnet` into
`~/.local/share/thurm/bin`, with `~/.local/bin/thurm` linked to it:

```sh
curl -fsSL https://docs.thurm.rs/install.sh | sh
```

To install by hand, unpack the release tarball for your architecture into a
directory on your `PATH`:

```sh
curl -fsSL -O https://github.com/nklmilojevic/thurm/releases/latest/download/thurm-x86_64-unknown-linux-musl.tar.gz
tar -xzf thurm-x86_64-unknown-linux-musl.tar.gz
mkdir -p ~/.local/bin
install -m 755 thurm thurmd thurm-tailnet ~/.local/bin/
```

Use `thurm-aarch64-unknown-linux-musl.tar.gz` on ARM. The binaries are static,
so they run on any distribution.

With Nix, `nix profile install github:nklmilojevic/thurm` installs the same
programs. For a NixOS or Home Manager setup, see the module in the repository's
`nix` directory.

## Start the daemon at login

```sh
thurm daemon install-systemd
```

This writes `~/.config/systemd/user/thurmd.service`, enables it for your next
login, and starts it now if no daemon answers. The unit is not restarted when
it stops: `thurm daemon stop` stays stopped until the next login. Remove it with
`thurm daemon uninstall-systemd`, which leaves the running daemon's sessions
alone.

## Keep it running after you log out

By default, systemd ends your user services when your last session on the host
ends, which takes the daemon and its panes with it. Enable lingering so the
daemon outlives your logins:

```sh
sudo loginctl enable-linger "$USER"
```

`install-systemd` warns when lingering is off. `thurm remote doctor` on your Mac
checks the same for a host you have added.

## Push notifications

Agent notices reach your phone through a push relay. Set its address and
registration code on the host, then pair the phone again so the code carries
them:

```sh
thurm set push.relay https://push.example.net
thurm set push.code YOUR-CODE
```

`thurm set` writes the daemon's config file and reloads it. Leave both empty to
turn notifications off.

## Pair the iPhone

There are two ways, and the phone can use either or both.

**Over Tailscale, from the host.** When the host is on your tailnet and serves
over Tailscale (see [Remote](/remote/#serve-over-tailscale)), run:

```sh
thurm pair
```

It prints a QR code and the same link as text. Scan the code in the Thurm app
with **+**. The phone connects over Tailscale alone, with no ssh fallback. A
self-hosted Tailscale-compatible control server works too; it is covered on the
[Remote](/remote/) page.

**Over ssh, through a Mac.** A host without Thurm's tools can still be paired from
the Mac: in **Thurm › Remotes… › Pair iPhone…**, the device key the phone shows is
added to the host's `authorized_keys` with **Add Device Key**. A key added this
way carries the `restrict` option, which blocks terminals, so the phone can
reach the host but cannot open a shell there. For a host the phone should open
shells on, add the key yourself without `restrict`, or use the Tailscale route.

## Troubleshooting

- **The phone says the host did not accept the device's key.** Add the key on
  the Mac with **Add Device Key**. The app keeps retrying while it is open.
- **The Mac reports that `thurm` is not installed on the host.** Thurm looks in
  `~/.local/bin`, `~/.local/share/thurm/bin` and the usual Nix profile paths.
  Run `thurm --version` over ssh to see what the host's login environment finds.
- **The daemon stops when you log out.** Enable lingering, as above.
