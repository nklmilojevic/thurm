---
title: Attach from a terminal
description: Control a persistent pane from a terminal or SSH session.
---

Use a terminal to control a pane without the desktop app:

```sh
thurm list
thurm attach --pane 7
```

For a pane on another host, run the command through SSH with a terminal:

```sh
ssh -t devbox thurm attach --pane 7
```

If the desktop app already has a tunnel to a configured host, you can also use
`thurm --remote devbox attach --pane 7`.

Press **Ctrl-]**, then **d**, to detach. The pane and its process continue to run.
Press **Ctrl-]** twice to send one Ctrl-] to the pane. Ctrl-C goes to the pane.

The command shows the existing screen and subsequent output, including alternate
screen applications. The pane follows the size of the attached terminal. Thurm
restores the previous pane size when you detach or the connection closes.

Only one terminal can control a pane at a time. During attachment, the desktop
app can show output, but its input and resize requests fail. Other commands that
send input also fail. Detach to give control back to the desktop app or scripts.

The command requires an ANSI terminal on both stdin and stdout. It rejects JSON
output, a pane that has exited, and attachment of a pane to itself. The daemon
must already be running. Local terminal settings are restored on normal detach,
connection errors, and handled termination signals. A forced kill cannot run this
cleanup.

`SIGTSTP` restores the local terminal and releases control of the pane before it
suspends the attachment process. After `SIGCONT`, the command connects again and
loads the current pane screen and terminal size. If another terminal controls the
pane, the connection fails and the local terminal stays restored.

This view shows text, colors, and cursor position. Images, hyperlinks, mouse
reporting, and extended keyboard protocols are not supported. Clipboard requests
are not sent to the local terminal. Use the desktop app for these functions.
