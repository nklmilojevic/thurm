---
title: Troubleshooting
---

## The shell cannot find `thurm`

Install the CLI from **Thurm > Integrations > Install Command-Line Tool**.
Check `command -v thurm` and make sure the installation directory is on `PATH`.
Open a new shell after you change its startup file.

## The CLI cannot connect or finds no current pane

Open the app, then run:

```sh
thurm daemon status
thurm list
```

Outside a Thurm pane, specify `--pane ID` for commands that need a target.
Use `thurm close ID` and `thurm focus ID` with a positional ID.
Check that the app and CLI use the same `THURM_SOCKET` and `XDG_RUNTIME_DIR`.
If the installed app and daemon have different versions, try
`thurm daemon upgrade`. See [Sessions](/sessions/) before stopping a daemon.

## A configuration change has no effect

Run `thurm config-path` and check that you edited that file. Run `thurm reload`
and read any error. Check TOML syntax, section names, and values against the
[reference](/configuration-reference/). A shell or environment change needs
a new process. `THURM_CONFIG_DIR` takes priority over `XDG_CONFIG_HOME`.

## Agent status or notifications are missing

Run `thurm agents` and `thurm hooks status`. Start the agent inside Thurm.
Install its hooks when you need reliable turn completion events. Check that
`agents.detect`, notifications, and macOS notification permissions are enabled.
Focused panes do not receive the same background notifications.

## A wait command times out

Exit code 124 means that the condition was not met before the timeout.
`--match` searches the visible screen. `--prompt` needs shell integration.
`--agent-done` needs hooks. Capture the pane to check its current state:

```sh
thurm capture --pane 1 -n 100
```

Replace `1` with the target ID from `thurm list`.

## Build fails

Check `xcodebuild -version`, `rustc --version`, and `zig version` against the
[requirements](/install/). Use `nix develop` for the repository toolchain.
The initial build needs network access. Run `./macos/build.sh` from the repository
root to build the Rust libraries, Swift app, and full app bundle together.

## Report a problem

Open an [issue](https://github.com/nklmilojevic/thurm/issues) with the macOS
version, Thurm version or source commit, reproduction steps, expected result,
and actual result. Include relevant command output or a small configuration
example. Remove passwords, tokens, and private terminal text before posting.
