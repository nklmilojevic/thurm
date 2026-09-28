---
title: Configuration
---

Thurm uses TOML. All settings are optional. Unset settings use their defaults.
Find the active file with:

```sh
thurm config-path
```

The default is `~/.config/thurm/config.toml`. `THURM_CONFIG_DIR` overrides the
configuration directory. Otherwise, `XDG_CONFIG_HOME` changes the base directory
to `$XDG_CONFIG_HOME/thurm`. Set these variables before you start the app and CLI.

Create the parent directory if needed. Edit the file, then run `thurm reload`
or press Cmd+Shift+,. Cmd+, opens the configuration file.

## Example

```toml
[font]
size = 14.0
ligatures = true

[window]
tab_style = "sidebar"
option_as_alt = "left"

[colors]
theme = "light:catppuccin-latte,dark:catppuccin-mocha"

[session]
quit = "detach"
```

Use the [complete reference](/thurm/configuration-reference/) for defaults, value
ranges, and optional settings. Copy only the sections you need. Do not repeat
a TOML section header when you add more settings to that section.

## Change settings from the CLI

```sh
thurm set font.size 14.0
thurm set font.ligatures true
thurm set window.tab_style sidebar
thurm theme
thurm theme nord
```

`set` writes the setting and reloads the configuration. Strings can be bare.
Other values use TOML syntax. Quote arrays to protect them from your shell:

```sh
thurm set font.features '["ss01", "zero"]'
```

`thurm theme` lists available themes. A theme can also be a file path.
The `light:NAME,dark:NAME` form follows the system appearance. Explicit color
settings override theme colors.

## Settings groups

| Section | Controls |
| --- | --- |
| `font` | Family, size, ligatures, spacing, fallback fonts, and symbols |
| `window` | Padding, opacity, blur, initial size, tabs, and pane appearance |
| `cursor` | Shape, blink, and thickness |
| `colors` | Theme and individual colors |
| `terminal` | Shell, scrollback, protocols, selection, and shell integration |
| `terminal.env` | Environment variables for shell processes |
| `session` | Saved sessions, quit behavior, and agent resume |
| `security` | Secure input and paste confirmation |
| `notifications` | Agent, command, program, and bell notifications |
| `agents` | Detection, launch presets, and custom definitions |
| `ai` | Optional on-device features |
| `updates` | Update channel, background checks, and automatic downloads |
| `keybindings` | Key combinations mapped to action names |

Shell and environment changes apply to new processes. They do not change the
environment of shells that are already running.

## On-device AI

Set `ai.enabled = true` to enable the optional Apple Intelligence model.
This needs macOS 26 or later, Apple silicon, and Apple Intelligence enabled in
System Settings. Model processing stays on the Mac. Thurm skips model use while
a password prompt is active.

The `titles`, `status`, `notifications`, and `explain` settings let you disable
individual features. Use **Edit > Explain Last Command** or `thurm explain` to
explain the last finished command. Agent status inference applies to agents
without hooks.

## Updates

Release builds use these defaults:

```toml
[updates]
channel = "auto"
check_automatically = true
download_automatically = false
```

`auto` follows the installed build: a Tip build receives Tip updates, and a
Release build receives tagged releases. Set `channel` to `release` or `tip`
to keep a fixed channel. Tip also accepts tagged releases.

**Thurm > Update Channel** writes the same setting as the CLI:

```sh
thurm set updates.channel tip
thurm set updates.check_automatically true
thurm set updates.download_automatically true
```

Automatic downloads install updates when Thurm quits. Use
`thurm set updates.channel auto` to follow the installed build again.
These settings require a build with an update feed and public signing key.
They do not enable updates for a local build that has neither.
