# Thurm

Thurm is a terminal for Apple silicon Macs. It has native tabs, split panes,
workspaces, and controls for coding agents. AppKit provides the interface.
Metal and CoreText draw text and images. The libghostty-vt library maintains
terminal state.

The `thurmd` daemon runs your shells. By default, they continue to run when you
quit the app. Open Thurm again to return to them.

- [Install Thurm](install.md) and open your first terminal.
- [Use tabs, splits, and workspaces](usage.md) to organize your work.
- [Change settings](configuration.md), fonts, and themes.
- [Control panes from the command line](cli.md).
- [Set up coding agents](agents.md) and receive turn notifications.
- [Understand saved sessions](sessions.md) before you stop the daemon.

Thurm requires macOS 14 or later and Apple silicon. Optional on-device AI
features require macOS 26 or later and Apple Intelligence. They are off by default.

These pages describe the `main` branch. An older installed build can have fewer
commands or settings. Use `thurm --version` to check your CLI version.
