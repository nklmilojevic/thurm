---
title: Remote workspaces
description: Attach Thurm to thurmd on a Linux devbox or another Mac, and hand repositories to remote agents.
sidebar:
  label: Remote workspaces
---

Thurm can attach to `thurmd` running on other machines. A remote host's panes
show as a workspace next to your local ones, with the same tabs, splits, agent
sidebar, agent switcher (⌘⇧A) and notifications. Agents keep running there when
your computer sleeps, changes networks, or quits.

See [Use Thurm](/usage/) for the full list of shortcuts.

The connection is the system `ssh`, one tunnel per host. Thurm has no relay or
account of its own; Tailscale or any other network works underneath. Your phone
and other devices can also reach a daemon over Tailscale without ssh: see
[Serve over Tailscale](#serve-over-tailscale). So can this Mac, for a host that
serves there: see [Reach a host over Tailscale](#reach-a-host-over-tailscale).

## Add a host

```sh
thurm remote add devbox me@devbox.example.org
```

`thurm remote add NAME SSH-TARGET` connects once, shows the host's platform,
offers to install this build of Thurm there, and writes the entry to your
config. Then it checks the rest the host needs (see
[Check a host](#check-a-host)) and offers each fix. In the app, **Thurm ›
Remotes… › Add Host…** does the same. Run without a terminal (from a script),
it needs `--yes` or `--no`, and it says so instead of assuming "no".

The entry it writes:

```toml
[[remote]]
name = "devbox"          # shown in the sidebar; `thurm --remote devbox ...`
host = "me@devbox.example.org"   # a Host alias, user@host, or ssh://user@host:port
# socket = "/run/user/1000/thurm-1000/thurmd.sock"   # discovered when unset
# enabled = true
# clipboard_read = "ask"   # "ask", "always" or "never"
# forward_links = true     # ⌘-click on a localhost link forwards that port here
# tailnet = "wss://devbox.YOUR-TAILNET.ts.net/v1/connect"   # over Tailscale instead of ssh
```

The app connects to every enabled `[[remote]]` in the background. Other
commands:

| Command | Purpose |
| --- | --- |
| `thurm remote list` | Hosts and their connection state |
| `thurm remote status [NAME]` | State, remote build, last error, retry countdown |
| `thurm remote install NAME` | Install this build there, and upgrade its daemon in place |
| `thurm remote doctor [NAME]` | Check what the host needs; `--fix` offers each fix, `--yes` runs them |
| `thurm remote remove NAME` | Forget a host; its daemon and panes keep running there |
| `thurm remote ports NAME` | The ports programs in the host's panes listen on |
| `thurm remote forward NAME PORT\|URL` | Open one of the host's ports on this Mac (see [Ports](#ports)) |

**Thurm › Remotes…** lists the same hosts with their state and build, and the
selected host's checklist with a **Fix** button for each problem.

## Check a host

`thurm remote doctor devbox`, and the checklist in **Thurm › Remotes…**, check:

| Check | Fix |
| --- | --- |
| Thurm | Installs this build (see [Supported hosts](#supported-hosts)) |
| Daemon | Replaces a daemon of another build in place |
| Lingering | `loginctl enable-linger`, or in a tab on the host with `sudo` when that needs your password |
| Command-line tool | Links `thurm` into `~/.local/bin`, so `ssh devbox thurm …` finds it |
| Claude Code | Runs the official installer (`https://claude.ai/install.sh`), after asking |
| Claude Code sign-in | Opens a tab running `claude` on the host, to sign in (Linux hosts) |
| Claude Code / Codex hooks | Installs them for exact agent status |
| Clipboard tools | Links `pbcopy`, `pbpaste`, `xclip`, `xsel`, `wl-copy` and `wl-paste` to `thurm` in `~/.local/share/thurm/shims`, after asking (Linux hosts whose daemon runs this build); other programs already there are left alone and reported |

A fix that needs you at the host (a password, a sign-in) runs over `ssh -t`:
in a tab of this Mac from the app, in your terminal from `thurm remote doctor
--fix`.

## Supported hosts

| Host | What gets installed |
| --- | --- |
| macOS, Apple silicon | The app's own `thurm` and `thurmd`, copied over |
| Linux x86_64 | The static `x86_64-unknown-linux-musl` build of the app's exact version |
| Linux aarch64 | The static `aarch64-unknown-linux-musl` build of the app's exact version |
| Hosts with Nix | `nix profile install` of the flake's `thurm` at the app's commit |

Linux archives are downloaded from the release the app came from and checked
against the SHA-256 built into the app. Files go to
`~/.local/share/thurm/bin`, and `thurm` is linked into `~/.local/bin` when that
directory exists. NixOS users can instead add the flake's `packages.thurm` to
their configuration.

Thurm never installs anything in the background. A host without Thurm, or with
another version, shows **not installed** or **upgrade needed** until you
confirm the install.

## Host requirements

- **An ssh key that works without prompting.** Thurm runs ssh with
  `BatchMode=yes`: a key in an agent (the 1Password SSH agent works, with its
  own approval prompt), no passphrase prompts, no password or 2FA prompts.
  Connect once with `ssh devbox` in a terminal to accept the host key.
- **Unix socket forwarding** in the host's `sshd` (`AllowStreamLocalForwarding
  yes`, the OpenSSH default).
- **Lingering on systemd hosts.** The daemon's socket lives in
  `$XDG_RUNTIME_DIR`, which logind deletes when your last session ends. The
  doctor's **Lingering** fix turns it on (`loginctl enable-linger $USER`).
- **Agent hooks on the host**, for exact agent status (Working, Needs input,
  Done). The host's daemon adds them when it launches an agent without them,
  and the doctor installs them for agents that are already there.

Your `~/.ssh/config` applies as usual: Host aliases, `ProxyJump`, and the
1Password `IdentityAgent`. Thurm refuses a host whose ssh config adds
`RemoteForward` or `DynamicForward` (see [Security](#security)).

## Tailscale

Tailscale is only the network. Either works:

- **Tailscale SSH**, version 1.98 or later. Earlier versions cannot forward a
  Unix socket, and the host shows **needs attention** with "unsupported channel
  type". Tailscale SSH opens the forward as root; an older Thurm daemon accepts
  only your own user and closes it, so the host stays **reconnecting** until you
  upgrade the daemon. Tailscale SSH's `check` action signs you in through a browser, which a
  background connection cannot do: use an `accept` rule for your own devices.
- **A regular `sshd`** reached over the tailnet.

To let the Mac reach the devbox but not the reverse, use a grant like this in
your tailnet policy (tags are examples):

```json
{
  "grants": [
    { "src": ["tag:mac"], "dst": ["tag:devbox"], "ip": ["tcp:22"] }
  ]
}
```

With no grant from `tag:devbox` to `tag:mac`, nothing on the devbox can open a
connection to your Mac.

## Serve over Tailscale

`thurmd` can also be a device on your tailnet itself, so your iPhone and other
devices reach it without Remote Login, `sshd` or `authorized_keys`:

```sh
thurm tailscale on
```

The first time, it prints a sign-in link (also under **Settings › Remotes ›
Tailscale**, and as a notification). Sign in with your Tailscale account and the
daemon joins your tailnet as `thurm` (`--hostname` changes the name), serving
`wss://thurm.YOUR-TAILNET.ts.net/v1/connect` with the tailnet's HTTPS
certificate. **Pair iPhone** then puts that address in the pairing code; the app
connects there first, with ssh as the fallback. The phone needs the Tailscale
app, signed in to the same tailnet.

A remote host that serves over Tailscale goes into the code with its own
address too: the phone connects to its daemon directly, and over ssh only when
this Mac knows its host key. Such a host needs no host key, and no device key
on it, when the phone reaches it over Tailscale alone. The Mac must be connected
to the host when you open **Pair iPhone**, to ask it for the address.

Without a Mac, pair the phone on the host itself: `thurm pair` prints a QR
code to scan in the app (and the same link, to paste instead). The code
carries only this host's Tailscale address, and the push relay when the host's
config has one; the phone connects over Tailscale alone, with no ssh fallback.
When HTTPS is not ready, `thurm pair` carries the plain addresses of
[Headscale](#headscale) instead, and fails only when there are neither.

`thurm tailscale on` lasts until the daemon restarts. To keep it on, turn on
**Serve over Tailscale** in Settings, or:

```sh
thurm set tailscale.enabled true
```

Who may connect:

- **Only your account** (the default): the account that signed the device in.
  Tailscale tells the daemon who each connection is from; anyone else is
  refused, and so are tagged devices, which have no owner.
- **Every tailnet member**: `allow_tailnet_members = true` (or
  `--allow-tailnet-members`). Needed to enroll with ACL tags
  (`advertise_tags`), since a tagged device has no owner.

HTTPS needs MagicDNS and HTTPS certificates turned on for your tailnet (the
admin console's DNS page); `thurm tailscale status` says when one is off.

### Headscale

To sign the node in to a [headscale](https://headscale.net) server instead of
Tailscale's, set its address:

```toml
[tailscale]
enabled = true
control_url = "https://headscale.example.com"
plain_listen = ":7297"
```

`control_url` (or `thurm set tailscale.control_url`) is the coordination server
the node signs in to; empty is Tailscale's. Changing it restarts the node, which
asks for a sign-in again on the new server. The admin pages in the Mac app and
in the status then say "your coordination server" and link to nothing.

A headscale tailnet without MagicDNS and HTTPS certificates has no `wss://`
address. There, `plain_listen` (`thurm set tailscale.plain_listen :7297`) has the
node serve the same gateway without TLS, at `ws://100.x.y.z:7297/v1/connect` for
each of its tailnet addresses and at `ws://<name>:7297/v1/connect` once the name
resolves. The tailnet's WireGuard encrypts that hop, and callers are admitted by
their tailnet identity as they are over HTTPS. A browser always sends an
`Origin` header, so browsers are refused there.

The phone needs the same tailnet: a headscale client, or the Tailscale app
signed in to the same server. **Pair iPhone** and `thurm pair` then put the
plain addresses in the code, as `tailnet_plain`, and the app connects to them
over the tailnet.

To serve through a machine's own Tailscale instead, which is already on the
headscale tailnet, use `host_listen` (`:7297`, see above). It needs no
`control_url` of its own.

### In a browser

The same address can serve a web client: open
`https://thurm.YOUR-TAILNET.ts.net/` in a browser on any device of your tailnet
and you get your workspaces and tabs, the panes as they are on the Mac and the
phone, typing, scrollback and agent notices with Approve and Deny. It is off by
default:

```sh
thurm set tailscale.web true
```

`thurm tailscale status` then prints the page's address. Release builds
include the web client; a Thurm built from source needs `just web` before
building, or the page says the client was not built in. There is no password
or token: the page is admitted exactly like the phone, by who Tailscale says
the device belongs to, so a browser that can open it has a shell on the host.
Only that page itself may talk to the daemon; a page from any other site,
including another `ts.net` address, is refused. Layouts, settings and images
stay on the desktop app.

Browsers keep some shortcuts for themselves (Ctrl+W, Ctrl+T, Ctrl+N): the page
asks before closing while it is connected, and in fullscreen Chromium hands
them to the terminal. Copy is Cmd+C on a Mac and Ctrl+Shift+C elsewhere.

| Command | What it does |
| --- | --- |
| `thurm tailscale status` | State, address, account, and the sign-in link while one is needed |
| `thurm tailscale off` | Stop serving; tailnet connections close, panes keep running, the device stays signed in |
| `thurm tailscale login` | Ask for a new sign-in link |
| `thurm tailscale logout` | Sign the device out and forget it |
| `thurm tailscale rename NAME` | Rename the device without signing in again |
| `thurm whoami` | Who the daemon sees on this connection |

To enroll without a browser, put an auth key in a file and set
`auth_key_file` (or pass `--auth-key-file`). The device's keys are kept in the
daemon's state directory (`tailscale/`), never in the config.

If your Mac already runs Tailscale, the daemon can listen through it instead of
being a device of its own: `host_listen = ":7297"` serves on this Mac's tailnet
addresses, and `serve_socket = true` gives `tailscale serve` a socket to proxy
to (`thurm tailscale status` prints the command). Both admit only this Mac's
owner unless `host_allow_identified = true`.

The Tailscale part is a separate program, `thurm-tailnet`, next to `thurmd` in
the app, the Linux archives, and the Nix package; the daemon starts
it only while serving over Tailscale. Installing Thurm on a remote host copies it
too, from builds that have it.

## Reach a host over Tailscale

A host that serves over Tailscale (`thurm tailscale on` there) can be reached
from this Mac the same way, without ssh:

```sh
thurm remote add bastion bastion --tailnet
```

`--tailnet` takes the host's name on the tailnet (default: the ssh target's host), or
its address: `wss://bastion.YOUR-TAILNET.ts.net/v1/connect` for a device of its
own, `ws://100.x.y.z:7297` for one that listens through its machine's Tailscale
(`host_listen`). Thurm checks that the daemon answers, then writes `tailnet` with
the full address next to `host`. In the app, turn on **Over Tailscale** in
**Add Host…**.

The connection goes through this Mac's own Thurm device when it runs
(`thurm tailscale on` here), so it works with the Tailscale app stopped. When
that device is off, a full `wss://…/v1/connect` address goes through the
Tailscale app instead; `thurm remote add --tailnet` saves that form. A bare peer
name written into `tailnet` by hand, and a `ws://` address, work through Thurm's
device only. `thurm remote list` shows which way a host is reached:
`tailnet (node)` or `tailnet (app)`.

The host decides who gets in, as for your phone: by default only its own
account. If you sign this Mac's Thurm device in with the same account, it is let
in. A host that is tagged, or signed in with another account, needs
`allow_tailnet_members = true` (or `host_allow_identified = true` for
`host_listen`), and then admits everyone your tailnet policy lets reach it:
keep that policy tight. Your policy must also let this Mac reach the host's
port 443 (or the `host_listen` port).

ssh is still what installs, checks and hands off: `thurm remote install`,
`thurm remote doctor` and `thurm handoff` use `host`, as do starting a daemon
that isn't running and the ssh config's `LocalForward`s. With the Tailscale app
stopped, ssh can go through Thurm's device too:

```
# ~/.ssh/config
Host bastion
  ProxyCommand thurm tailscale nc %h %p
```

`thurm tailscale peers` lists the devices Thurm's device sees.

## What runs where

| Setting | Comes from |
| --- | --- |
| Font, theme, appearance, key bindings, notifications, rendering | This Mac's config |
| Shell, `env`, `term`, `shell_integration`, agent presets, `[[agents.define]]` | The host's own `~/.config/thurm/config.toml` |
| OSC 52 reads, link opening, ssh forwarding | This Mac, enforced in the app; the host's config can only be stricter |

Launch presets in a remote workspace (⌘⇧P, ⌘⇧A) are the host's.

## Using remote panes

- **Workspaces.** A host's panes open in a workspace named after it. ⌘⇧O
  lists **New Workspace on devbox**. New tabs and splits in a remote workspace
  run on that host. A pane started on the host from elsewhere (`thurm --remote
  devbox launch`) goes to its workspace in the background; the agent sidebar
  and ⌘⇧A switch to it.
- **Agents.** Sidebar and switcher rows show the host: `devbox · repo · Needs
  input`. Notifications follow the same focus rules as local panes. A status
  that changed while the host was unreachable shows when it is back; missed
  notifications are not replayed.
- **Offline.** An unreachable host keeps its tabs. They show the last frame
  under **Disconnected — reconnecting…** with the retry countdown, and input is
  blocked. Thurm retries with backoff up to two minutes, and right away after
  sleep or a network change.
- **Restore.** After the app restarts, remote tabs come back from the saved
  layout and attach to the host's still-running panes. Tabs of panes that ended
  meanwhile close with a notice; panes the host has that the layout doesn't know
  open as new tabs.
- **Files.** See [Files](#files).
- **Clipboard tools.** Programs on a Linux host that call `pbcopy`, `pbpaste`,
  `xclip`, `xsel`, `wl-copy` or `wl-paste` reach this Mac's clipboard once the
  doctor's **Clipboard tools** fix has linked them. New panes whose
  environment has no `DISPLAY` or `WAYLAND_DISPLAY` (the daemon's, with the
  host's `[terminal.env]` and what the client passes on top) have them first
  on `PATH`, ahead of a `PATH` set in `[terminal.env]`. After the shell's
  startup files they are put first again, or taken off `PATH` when those files
  set a display, whose own tools then work: by the shell integration in bash,
  zsh and fish, and for a pane that runs a command in any shell. In an
  interactive shell without the integration (`terminal.shell_integration =
  false`, or another shell), they stay first only if its startup files keep
  the `PATH` they are given. Only the clipboard selection is this Mac's
  clipboard: PRIMARY and SECONDARY are refused, and with them xclip's and
  xsel's default (use `xclip -selection clipboard`, `xsel -b`). Text is UTF-8:
  another charset, or X11's `STRING` (`xclip -noutf8`), is refused. With no text on
  the clipboard, `pbpaste` and `xsel` print nothing and succeed, and `xclip` and
  `wl-paste` fail, as each does with a display. What is checked, and where:

  | | On the host (its daemon) | On this Mac (the app) |
  | --- | --- | --- |
  | Copy, clear | the host's `terminal.osc52` is `copy` or `copy-paste` | this Mac's `terminal.osc52` is `copy` or `copy-paste` |
  | Paste | the host's `terminal.osc52` is `paste` or `copy-paste` | this Mac's `terminal.osc52` is `paste` or `copy-paste`, then the host's `clipboard_read`: asks, always, or never |
  | List the types | as a paste | as a paste, but never asks |

  These are the checks an OSC 52 copy or read gets, and both sides must allow
  it. A copy, clear or paste goes to the Thurm app that last showed the pane
  (the only one connected, if none has; with several and none of them, the
  tool asks you to show the pane in one; a phone or an app without clipboard
  tools showing it since changes nothing), which answers it, so the tool hears
  whether it was done: anything refused,
  on the host or on this Mac, fails the tool. Listing the types
  (`xclip -selection clipboard -t TARGETS -o`, `wl-paste --list-types`) fails where reads from that
  host are refused (`clipboard_read = "never"`, or pastes off in either
  `terminal.osc52`). Pastes carry text or a PNG image (an agent's image paste
  works; a screenshot, copied as TIFF, is converted when it is at most 256 MiB
  and 64 megapixels), at most 32 MiB each, and at most 128 MiB at once on its way through
  the host's daemon or queued there for a program or app that has not read it
  yet (a paste or copy over that is refused, to try again).
  A clear (`xsel -b --clear`, `wl-copy --clear`) empties
  the clipboard, leaving no type. Options the tools cannot honour here are
  refused rather than ignored: a copy meant to go away (`xclip -loops`,
  `-wait`, `-sensitive`, `xsel --selectionTimeout`, `wl-copy --paste-once`),
  `xsel --append`, `--follow` and `--exchange`, another X server
  (`xclip -display`, `xsel --display`), and a pasteboard or format
  other than the general one and text (`pbcopy -pboard find`,
  `pbpaste -Prefer rtf`). `thurm clip copy`, `thurm clip paste [--type image/png]` and
  `thurm clip clear` do the same on any host; on this Mac's own panes only
  this Mac's `terminal.osc52` applies.

## Files

Drop files on a remote pane, or copy them in Finder and pick **Send Copied Files to
devbox…** in the pane's context menu, and they go to the host. A pasted screenshot or an image dragged out of a browser goes as
`image-….png`. A toast shows the progress, with **Cancel**. Files dropped while
others are on their way wait for them, so paths are typed in the order you
dropped them. Once every file has
arrived, the host's daemon types their paths into the pane, quoted and separated
by spaces, as a paste, so an agent at its prompt gets them as one paste. If a
file fails or you cancel before every file has arrived, nothing is typed and
nothing is kept. Dropped and copied files are sent once you confirm them in the
dialog that names them; a pasted screenshot or a dragged image, which is no file
of this Mac's, is sent without asking. Pasting pastes text whenever there is some, even when the app that
copied it put a file beside it: Finder's copies carry their names as text, so
pasting them pastes the names. Folders are
refused; drop the files in them instead.

Uploads are kept on the host, readable by you only, in Thurm's state directory
(`$XDG_STATE_HOME/thurm`, usually `~/.local/state/thurm`; `~/Library/Application
Support/Thurm` on a Mac): `drops/<pane>/<batch>/<name>`. Names keep their last path component without control
characters, at most 255 bytes, numbered (`name-2.txt`) when two in a batch are
the same. Limits:

| | |
|---|---|
| One file | 4 GiB |
| Files in one drop | 256 |
| Files on their way to one pane | 64 |
| Drops not complete yet | 8 per app, 32 in all, and 512 files in them, sent or not |
| Space left free on the host's disk | 1 GiB, and room for 10 000 more files: the oldest finished drops give way for the space, and an upload that still does not fit is refused |
| Kept | 24 hours from when a drop was complete (one never complete: from its last write), 8 GiB and 1024 drops in all, drops on their way included (every file and folder counts as 4 KiB at least, an empty one too): the oldest finished drops go first, and a file that still does not fit is refused |

The other way, ⌘⌥-click a path in a remote pane, or right-click it and pick
**Quick Look**, to preview the host's file here (the toast shows it coming, with
**Cancel**): relative paths use the pane's directory when you clicked, `~` is your
home on the host. A `file://` link offers **Quick Look**
and **Download** (to `~/Downloads`, with progress and **Cancel**) beside **Copy
Path**. Previewed files are kept in the app's caches folder and fetched again
only when they changed on the host. Kept previews and those downloading share
256 MiB: a preview that does not fit beside the others stops with a message. Only regular files are sent, at most
four at a time from one pane. A file that changes between the request and the
end of sending fails instead of arriving mixed; try again.

A host running an older Thurm keeps the earlier behavior: a pasted or dropped
image is written to a private file on the host and its path pasted; other files
insert their paths on this Mac.

## Ports

A dev server on the host prints `http://localhost:5173`. That is the host's
port 5173, not the Mac's. ⌘-click the link and Thurm opens the host's port on
this Mac first: it listens on port 5173 of this Mac's loopback, `127.0.0.1` and
`[::1]` both, so `localhost` reaches it whichever address the browser tries
(another free port when 5173 is taken on either, kept for next time), carries
each connection to the host's daemon, which connects to
port 5173 on the host's own loopback, and opens the link pointed at it. The
first time, a toast on that pane says where it went (`devbox:5173 →
localhost:5173`), with **Copy Link**; an alert does when the pane is gone by then,
or when there was none (Forward Port… with no window open). Links to `localhost`, `127.0.0.1`, `[::1]` and `0.0.0.0` are
forwarded. The host's daemon connects to `127.0.0.1`, then `[::1]`; a link to
`127.0.0.1` or `[::1]` reaches that address only, through a forward of its own,
so two programs on the same port, one per address, each open as linked (a
daemon older than this build tries both for those too). Set `forward_links = false` in the host's `[[remote]]` (or turn off
**Open localhost links through a forward** in its details) to open them on the
Mac instead.

Other ways to open one:

- **Shell › Forward Port…** (⌘⇧P, **Forward Port…**) lists the ports the
  focused host's panes listen on, and takes a port number or a link. With a
  pane of this Mac focused and more than one host connected, it asks which host
  first.
- **Shell › Processes & Ports…**: double-click a remote row with a port.
- `thurm remote forward devbox 5173` from a terminal, until Ctrl-C.
  `--local PORT` picks the port on the Mac (an error if the port is forwarded to
  another one already), `--no-open` only prints the URL, and `--json` prints
  `{host, remote_port, local_port, url}` without opening it.
  `thurm remote ports devbox` lists what listens there.
- A [plugin](/plugins/#plugins) you installed, with `thurm.forward.open` when it
  hears a port start listening: the
  [`dev-server`](https://github.com/nklmilojevic/thurm/tree/main/examples/plugins/dev-server)
  example opens a dev server's port as it starts.

Nothing is forwarded until you or a plugin you installed does one of these: a
port that merely opens on the host stays there, and a host that is turned off
gets no forwards. The host's
details in **Thurm › Remotes…** list its forwards, with **Open** (the link it
was opened for), **Copy Link** and **Close**; ⌘⇧P has **Copy Forward Link** and
**Close Forward** for each too. Each checks first that the forward is still open
on the same port: one that has closed meanwhile is neither opened nor copied (its
port may be another program's by then). Closing a forward also ends the connections it
carries. A forward also closes when the host is removed or turned off, and when
the port has been gone from every pane of the host for 30 seconds with no
connection open (while part of a host's processes can't be read, or the host is
disconnected, that clock doesn't run). A forward to a port no pane listed (a
container, a system service) stays until you close it. One forward carries at
most 64 connections at a time.

The host needs a Thurm daemon of this version or later; an older one is
reported when you open the link.

## The CLI

`--remote NAME` sends any pane command to the host's daemon through the tunnel
the app keeps open:

```sh
thurm --remote devbox agents
thurm --remote devbox capture --pane 3 -n 40
thurm --remote devbox wait --pane 3 --agent-done --timeout 600
thurm --remote devbox launch claude
```

Pane IDs are the host's, so pass `--pane` (the current pane is a pane of this
Mac). When the app is not connected to the host, the command fails with, for
example, `devbox is not connected in Thurm (state: reconnecting)`. The CLI
never opens an ssh connection of its own for this.

## Hand off a repository

Give a local repository to an agent on a remote host. Your code goes over
through git; nothing else from the Mac does.

From a tab in the repository, press ⌘⇧P and choose **Hand Off to devbox…**,
then an agent preset (or a shell). From a terminal:

```sh
thurm handoff --remote devbox --preset claude
```

Thurm then:

1. Takes a snapshot: `HEAD`, or with uncommitted changes (untracked files
   included, ignored files not), a WIP commit on top of it. Your working tree,
   index, and stash stay as they are.
2. Pushes it over ssh to a bare mirror of the repository on the host
   (`~/.local/share/thurm/mirrors`), as `agent/<name>`.
3. Checks it out in its own worktree (`~/.local/share/thurm/worktrees`).
4. Adds a git remote `thurm-devbox` to your local repository.
5. Opens a tab on the host in that worktree, running the preset. The tab is
   listed under your local repository in the sidebar. Type the task yourself.

Several handoffs of one repository can run at once, each on its own branch.

**Getting the work back.** The agent commits. When it reaches Done or Needs
input, Thurm fetches `agent/<name>` into
`refs/remotes/thurm-devbox/agent/<name>`. **Fetch Handoff Result** (⌘⇧P) and
`thurm handoff --fetch ID` fetch on demand. Thurm never merges, checks out, or
rebases anything in your repository; review and merge the branch yourself, and
push from the Mac. The remote host cannot push anywhere: it has no access to
your keys.

Gitignored files (`.env`, `node_modules`, local config) are not copied. Set up
what the agent needs on the host.

**Cleaning up.** Closing a handoff tab offers to remove its worktree. Thurm
fetches first; if the worktree has uncommitted changes or commits not fetched
yet, it asks before deleting them. The branch on the host is deleted when your
default branch already contains it, and kept otherwise. Your local
remote-tracking ref stays. A tab closed while the host is offline is cleaned up
when it is back. `thurm handoff --list` and `thurm handoff --cleanup ID` do the
same from a terminal.

## Security

Remote hosts are assumed to be yours and single-user. Even so, they cannot
reach the Mac's disk, clipboard, or credentials without you doing something:

- **Clipboard reads (OSC 52 and clipboard tools).** A remote pane's read
  follows the host's `clipboard_read`: with `ask` (the default) every OSC 52
  read and every paste asks "devbox wants to read your clipboard — Allow Once /
  Always for devbox / Deny" (one question per host at a time: another read
  meanwhile is refused, and a question left unanswered closes after 25
  seconds as Deny: an answer given later, as to one another window kept
  covered, is a Deny too, and saves nothing, and from then on that host may
  ask again); with `always` they go ahead without asking
  ("Always" writes that to the host's `[[remote]]`); with `never` they are
  refused. Listing the clipboard's types (`xclip -selection clipboard -t TARGETS -o`,
  `wl-paste --list-types`) never asks: it says whether there is text or an
  image, not what, and is refused wherever reads are. Your `terminal.osc52`
  setting caps every host, and the host's own `terminal.osc52` can only be
  stricter.
- **Clipboard tools' requests** come only from programs on the host itself
  (a phone or tailnet caller sending them is refused), and go to the Thurm app
  that last showed the pane; this Mac's own daemon asks only the Thurm app.
- **Clipboard writes** (a copy, or a clipboard tool's clear) follow your
  `terminal.osc52` setting, and the host's.
- **Links.** ⌘-click opens `http` and `https` links from remote panes (a link
  to the host's `localhost` through a forward, see [Ports](#ports)), asks for
  other schemes, and never opens `file://` links (they are paths on the host):
  it offers to copy the path, preview the file, or download it instead.
- **ssh.** Agent forwarding, X11 forwarding, and remote and dynamic forwards
  are always off. The only forward is the local socket for the daemon.
- **Ports.** The Mac only ever connects out to the host: a forward is a
  connection through the daemon's socket, and the host never reaches a port of
  the Mac. Forwards open only when you ask for one, and closing one ends its
  connections. A forward listens on loopback only (`127.0.0.1` and `[::1]`), so
  other machines can't use it, but loopback has no notion of users: any program on this Mac, run by
  any account logged in to it, can connect to an open forward and reach the
  host's port as you, the same as with `ssh -L`. On a Mac other people use, keep
  forwards to services that trust their loopback (no password of their own)
  closed when you don't need them.
- **Forwarded sites in the browser.** A forwarded link opens at `localhost` or
  `127.0.0.1` on this Mac, so the browser treats the site as one of this Mac's
  loopback sites. Browsers don't keep cookies apart by port: cookies set for
  `localhost` (by a local dev server, or another host's forward) are sent to the
  forwarded site, which runs on the host, and cookies it sets reach those
  sites. This is how any port forward behaves; a host you would not give those
  cookies to should keep `forward_links = false`, and its links then open on the
  Mac as they are.
- **Files.** Only files you paste, drop, or send with **Send Copied Files** go to the
  host, and only the files you preview or download come back. A download into
  `~/Downloads` is written in a private folder of its own there, then moved in
  under a free name (`report 2.pdf` beside an existing `report.pdf`) by a rename
  that never replaces a file: nothing already in `~/Downloads` is written into,
  moved or deleted.
- **Over Tailscale.** The host admits this Mac by its Tailscale identity. On
  this Mac, the tunnel's socket and Thurm's dial socket (next to the daemon's)
  are yours alone, and the dial socket reaches tailnet devices only.

## Troubleshooting

| State | What to do |
| --- | --- |
| needs attention | ssh needs you: read the message (host key, authentication, forwarding). Fix it, then **Retry**. Over Tailscale: the host refused this Mac (the message names the setting), or neither Thurm's device nor the Tailscale app is on |
| not installed | **Fix** next to Thurm in Thurm › Remotes…, or `thurm remote install NAME` |
| upgrade needed | The host runs another protocol version. Install this build; its daemon is replaced in place and its panes keep running (daemons too old for that are restarted after you confirm) |
| reconnecting | The host is unreachable; Thurm retries on its own |

The tunnel's local end is `~/Library/Caches/Thurm/remote/NAME.sock`.
