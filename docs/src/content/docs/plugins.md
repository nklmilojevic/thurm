---
title: Plugins and scripting
---

Thurm's daemon keeps a list of **actions**: things with a name that the command
palette lists, a key can run, and the CLI and scripts can call. Your
`[[commands]]` are actions, the app's menu items are actions (`split_down`,
`new_tab`, ...), and plugins add more. Plugins can also bind keys, react to
events, open panes, run programs and ask you things.

There are three ways to extend Thurm:

| | What it is | Where |
| --- | --- | --- |
| `init.lua` | Lua that runs in the daemon whenever it starts or reloads | `~/.config/thurm/init.lua` |
| A plugin | A Lua plugin, or a program in any language, that the daemon runs and restarts | `~/.config/thurm/plugins/NAME/` |
| A script | Lua that runs once, to its end | `thurm do FILE` |

Everything here runs with your user's rights. Install only plugins you trust.

## A first init.lua

```lua
-- ~/.config/thurm/init.lua

-- An action: in the palette and Shell > Commands, and `thurm action run git.status`.
thurm.action{
  name = "git.status",
  title = "Git status",
  category = "Git",
  run = function(ctx)
    local info = thurm.info(ctx.pane)
    local r = thurm.exec({ "git", "status", "--short" }, { cwd = info.cwd })
    if not r.ok then error("git status failed: " .. r.stderr) end
    thurm.ui.toast(r.stdout ~= "" and r.stdout or "clean", ctx.pane)
  end,
}

-- A key for it, and a key for a function of your own.
thurm.bind("cmd+shift+g", "git.status")
thurm.bind("cmd+shift+y", function(ctx)
  thurm.split("right", { command = { "lazygit" }, target = ctx.pane })
end)

-- Something to do when an agent finishes.
thurm.on("agent_finished", function(ev)
  if not (ev.pane.cwd or ""):find("/work/") then return end
  thurm.exec({ "say", "agent done" })
end)
```

Thurm runs it when the daemon starts, and again on **Reload Configuration**
(`thurm reload`). An error shows as a message over the focused pane with the
file and line, and `thurm plugin logs init` keeps it. Run
`thurm plugin types` once to get completion and checks for the `thurm` API in
editors that use lua-language-server.

Settings stay in `config.toml`; Lua is for behavior.

## Actions and keys

```sh
thurm action list                      # every action, who provides it, its key
thurm action run git.status            # for this pane
thurm action run git.checkout '{"branch":"main"}'
thurm action run split_down --pane 4   # the app's own actions too
```

The command palette and **Shell > Commands** show the actions of
`[[commands]]` and plugins; an action's `category` makes a submenu. A key
bound with `thurm.bind` (or by a plugin program) works like a
`[keybindings]` entry: bound to one of the app's actions, it runs that
action too, and the menu shows it (the action's own key keeps working, as with
`[keybindings]`); bound to a plugin's action, it runs it. The binding made last
wins, and goes when its plugin stops. A key bound with arguments
(`thurm.bind(key, action, args)`) runs a plugin's action with them.
`[[commands]]` entries and the app's own actions take no arguments: a
`[[commands]]` entry runs as it is, and the app ignores such a key for its
actions.

## Events

`thurm.on(name, fn)` runs `fn` with a table that has `event` (the name) and,
for an event about a pane, `pane` (as `thurm info --json` shows it).

| Event | When |
| --- | --- |
| `agent_working`, `agent_waiting`, `agent_blocked`, `agent_finished`, `agent_failed`, `agent_idle` | An agent enters that state |
| `command_finished` | A command at the prompt finished; `exit_code`, `duration_secs` (the command line itself goes only to `[[on]]` hooks: it may hold a secret) |
| `pane_exited` | A pane's process exited; `exit_code` |
| `pane_created`, `pane_closed` | A pane was created or closed (`id` for a closed one) |
| `pane_changed` | A pane's title, directory, program or agent changed |
| `pane_focused` | A pane got the keyboard focus in an app |
| `bell`, `notification` | A program rang the bell, or sent a notification (`title`, `body`); these and `pane_closed` carry the pane's `id`, not `pane` |
| `config_reloaded` | The configuration was reloaded |
| `ports_changed` | The TCP ports a pane's programs listen on changed: `ports` (each with `port`, `address`, `pid`, `command`), the whole list, empty once they all went, and `complete` (false when some of the pane's processes could not be read). It carries the pane's `id`, and `pane` while it is there; a remote host's pane has `host` instead of `pane`, and `initial` marks lists of servers that were running already: those a plugin is sent when it subscribes, and those a host's daemon sends the app as it connects; every other list is a change. When the app loses the host (or quits), each of its panes gets an empty list with `complete` false and `gone`: nothing is known of their ports then. Without `gone`, a list that is not `complete` may leave out servers that still run |
| `files_uploaded` | Files dropped or pasted on a pane landed and their paths were typed there: `paths`, with the pane's `id` |
| `forward_opened`, `forward_closed` | The app opened or closed a forward of a remote host's port: `id` (the forward's, as `thurm.forward.close` takes it, not a pane's), `host`, `remote_port`, `local_port`, `url`; a closed one says `why`: `closed`, `host` (removed or disabled), `gone` (the port went away) or `app` (the app quit) |
| `NAME.anything` | A plugin's own event, sent with `thurm.emit` |
| `*` | Every event above |

`[[on]]` hooks run on the first eight, as before.

The daemon looks at its panes' ports only while someone listens: subscribing
to `ports_changed` by name starts it (`*` alone does not). A plugin that
subscribes gets every pane's list first (an empty one too), on its own and marked `initial`, with
the remote hosts' lists the app passed on so far (a remote pane plugins
already heard listen on nothing is left out); the others hear nothing
again. A remote host's ports and the forwards come from the app, so
a plugin on this Mac hears them while Thurm is open.

## The Lua API

Calls that wait (a program, an action, the user, `sleep`) work in the main
chunk, in actions, in event handlers and in timers; Thurm runs each of these
in a coroutine and keeps going meanwhile, so a slow handler never holds up
your keys. Errors are raised: catch them with `pcall` where you expect them.
An error says the file and line it came from, which helps with a bug; to stop
with a message meant for the user (a file that isn't there), use
`thurm.fail(message)`, which shows only the message. An action that fails
either way shows it over the pane when the app ran it, and `thurm action run`
prints it and exits with 1.

| Function | Does |
| --- | --- |
| `thurm.action{name, title, description, category, hidden, args, run}` | Adds an action; `run(ctx, args)` returns its result. `ctx` has `pane`, `origin` and `depth`, and more when the app runs it (below) |
| `thurm.run(name, args, pane)` | Runs any action and returns its result |
| `thurm.actions()` | Every action and binding |
| `thurm.bind(key, action or fn, args)`, `thurm.unbind(key)` | Binds a key |
| `thurm.on(event, fn)`, `thurm.emit(name, payload)` | Hears events; sends one of your own |
| `thurm.panes()`, `thurm.info(pane)`, `thurm.focused()` | Panes, one pane, the focused pane |
| `thurm.open{command, cwd, env, agent, split, float, frame, cells, window, target, percent, background, hold}` | Opens a pane and returns its id; `frame` and `cells` place a floating one (below) |
| `thurm.split(dir, opts)` | `thurm.open` with `split = dir` |
| `thurm.send(pane, text, {enter})`, `thurm.capture(pane, {lines, scrollback, ansi})` | Types into a pane; reads its screen |
| `thurm.wait(pane, {match, idle, exit, agent, timeout})` | Waits for the prompt, a regex, quiet, exit or a free agent; `true`, or `false` and why; waiting for `exit`, the exit code is the third value (`true, nil, code`) |
| `thurm.close(pane)`, `thurm.focus(pane)` | Closes or focuses a pane |
| `thurm.ports(pane)` | The ports the pane's programs listen on (`port`, `address`, `pid`, `command`), and whether all its processes could be read; without a pane, every pane's as `{pane, ports, complete}`. This Mac's panes only: a remote host's pane is known by its `ports_changed` (its `id` is that host's, with `host`) |
| `thurm.forward.open(host, port, {browse})`, `thurm.forward.close(id)`, `thurm.forward.list()` | Opens a remote host's port on this Mac through the app (or finds the forward that has it), closes one, lists them; a forward has `id`, `host`, `remote_port`, `local_port` and `url` (the link it was last opened for, on its local port), and `browse` opens the URL too. A forward that closed (its port went away), or moved to another local port, before the app could use it is an error, and nothing is opened |
| `thurm.ui.toast(text, pane)` | A short message over a pane |
| `thurm.ui.pick{title, placeholder, items}` | The user picks an item (a string, or a table with `title` and `detail`); returns it and its index, or `nil` |
| `thurm.ui.input{title, placeholder, value}`, `thurm.ui.confirm{title, body, ok, cancel}` | A line of text, or yes or no |
| `thurm.exec(argv, {cwd, env, stdin, timeout})` | Runs a program to its end through your login shell; `ok`, `code`, `stdout`, `stderr`, `timed_out`. A string runs with `sh -c` |
| `thurm.after(s, fn)`, `thurm.every(s, fn)`, `thurm.sleep(s)` | Timers; the first two return a handle with `cancel()` (a plugin may have 4096 set at a time) |
| `thurm.fail(message)` | Stops with a message for the user, without the file and line an error adds |
| `thurm.call(method, params)`, `thurm.request(request)` | Any method of the API below, or any daemon request as JSON |
| `thurm.json.encode`, `thurm.json.decode`, `thurm.json.null`, `thurm.log(...)`, `thurm.warn(...)` | JSON (decode keeps nulls as `thurm.json.null`; in the API's results a null field is `nil` and a null in an array is `thurm.json.null`), and the plugin's log (`print` logs too) |
| `thurm.plugin`, `thurm.dir`, `thurm.settings`, `thurm.args`, `thurm.pane` | The plugin's name, directory and settings; a script's arguments and pane |

When the app runs an action (a key, the palette, the menu) in one of this
Mac's panes, `ctx` also says what the person is looking at: `selection`, the
text selected in the pane, and `file`, the file the mouse pointer is over, as
the context menu's **Quick Look** finds it (a path as written there, so
relative to the pane's directory unless it is absolute). In a remote host's
pane, the action gets neither: `pane` is `nil` (this daemon's pane ids mean
nothing there) and `remote` is the host's `[[remote]]` name.

A floating pane goes where new floating panes go, unless you place it: `frame
= {x, y, width, height}` in fractions of the tab's area, from its top left
corner, or `cells = {cols = 60, rows = 20, col = 2, row = 1}` for a terminal of
that many cells whose first cell is `col` columns right of and `row` rows below
`target`'s (either may be negative; the app keeps the card inside the tab).
Either one floats the pane. An app too old to place it floats it where new
floating panes go. Set `float = true` with them as well: a Thurm from before
`frame` and `cells` ignores both, and opens a tab where `float` is not set.

```lua
-- A card over the middle of the current pane, 60 by 20 cells.
local pane = thurm.focused()
local info = thurm.info(pane)
thurm.open{
  command = { "htop" },
  target = pane,
  float = true,
  cells = {
    cols = 60, rows = 20,
    col = (info.size.cols - 60) // 2, row = (info.size.rows - 20) // 2,
  },
}
```

`require` loads modules from the plugin's own directory. `io.popen`,
`os.execute` and `os.exit` are not available (use `thurm.exec`). `io` opens
regular files only (a FIFO or device could hold up every plugin), and there
is no stdin to read. Code that
runs for 5 seconds without waiting is stopped, and a plugin may use at most
256 MiB (a script, 64 MiB).

## Scripts

`thurm do` runs a Lua file in the daemon to its end, prints what it logs, and
then what it returns. It has the same API, with `thurm.args`, and in
`thurm.pane` the pane it was started from (`nil` with `--remote`, where this
machine's pane means nothing: use `thurm.focused()` or `thurm.panes()` there):

```lua
-- review.lua: open the diff of every changed tracked file, staged or not, in a split
-- (new files git doesn't track yet are not in `git diff`).
local cwd = thurm.info(thurm.pane or thurm.focused()).cwd
local top = thurm.exec({ "git", "rev-parse", "--show-toplevel" }, { cwd = cwd })
if not top.ok then error("not in a git repository: " .. top.stderr) end
local root = top.stdout:gsub("%s+$", "")
-- -z: names as they are, NUL after each (a name may hold a newline).
local r = thurm.exec({ "git", "diff", "HEAD", "--name-only", "-z" }, { cwd = root })
if not r.ok then error(r.stderr) end
for file in r.stdout:gmatch("[^\0]+") do
  thurm.split("down", { command = { "git", "diff", "HEAD", "--", file }, cwd = root, hold = true })
end
return "opened diffs"
```

```sh
thurm do review.lua
echo 'return #thurm.panes()' | thurm do -
thurm --remote build do deploy.lua staging
```

Stopping `thurm do` (Ctrl-C) stops the script. At most 8 scripts run at
once. With `--remote --with-modules`, the `.lua` files beside the script (a few
levels deep) go along, so `require` finds them on the other host; without it,
only the script goes. A host the app reaches over the tailnet, not ssh, refuses scripts and
actions: running code there is for that machine's own clients.

## Plugins

A plugin is one of these in `~/.config/thurm/plugins/`:

- a directory with a `plugin.toml`;
- a directory with an `init.lua` and no manifest;
- a single `NAME.lua`;
- a single executable file, run as a program.

Its name is the directory's or the file's name, without the extension.

```toml
# plugins/git-tools/plugin.toml
version = "1.0.0"
description = "Git actions"
main = "init.lua"          # a Lua plugin, or:
# exec = ["plugin.py"]     # a program; a first word naming a file here is relative to the plugin
```

```sh
thurm plugin new git-tools            # a Lua plugin to start from
thurm plugin new watcher --program    # a program plugin (a Python example)
thurm plugin install https://github.com/someone/thurm-git-tools
thurm plugin list
thurm plugin reload git-tools         # after editing it
thurm plugin logs git-tools
thurm plugin disable git-tools
thurm plugin update
thurm plugin remove git-tools
```

For a whole plugin to start from, see
[`examples/plugins/quicklook`](https://github.com/nklmilojevic/thurm/tree/main/examples/plugins/quicklook)
in Thurm's repository: an action and a key (⌘Y) that preview a file with
macOS's Quick Look in a floating pane, drawn with the kitty graphics protocol
by a small script that ships with it.
[`examples/plugins/dev-server`](https://github.com/nklmilojevic/thurm/tree/main/examples/plugins/dev-server)
hears `ports_changed` and opens a dev server in the browser when it starts
listening, forwarding a remote host's port to this Mac first.

`install` clones the repository into the plugins directory (the name drops a
`thurm-` prefix; `--name` picks another). `disable` and `enable` change
`plugins.disabled` in `config.toml`:

```toml
[plugins]
enabled = true            # false runs no plugin, init.lua included
disabled = ["watcher"]

[plugins.settings]
git-tools = { remote = "origin", fetch_every = 300 }
```

A plugin's settings reach it as `thurm.settings`, or as JSON in
`THURM_PLUGIN_SETTINGS` for a program. Reloading the configuration starts new
plugins and those that failed to start, stops removed and disabled ones, and
restarts those whose settings changed and the Lua ones. A Lua plugin (or
`init.lua`) whose new code doesn't compile keeps running as it was, and one
whose new code fails as it starts runs its code that worked again; the error
shows either way.

## Program plugins

A program plugin can be written in any language. Thurm starts it in its
directory, through your login shell, with `THURM_SOCKET`, `THURM_PLUGIN`,
`THURM_PLUGIN_DIR` and `THURM_PLUGIN_SETTINGS` set, and talks to it in
JSON-RPC 2.0, one message per line, on its standard input and output.
Standard error goes to its log.

- Its requests are the methods below; each gets an answer with its `id`.
- Events it subscribed to, and other notices, come as notifications:
  `{"jsonrpc":"2.0","method":"event","params":{"event":"bell","id":3}}`.
- To run one of its actions, Thurm sends a request it answers with the
  action's result (or an error):
  `{"jsonrpc":"2.0","id":7,"method":"action.invoke","params":{"name":"x.y","args":null,"ctx":{"pane":3}}}`.
- When its standard input closes it should exit; Thurm also sends SIGTERM,
  then SIGKILL after 2 seconds.

```text
→ {"jsonrpc":"2.0","id":1,"method":"actions.register","params":{"actions":[{"name":"watcher.scan","title":"Scan"}]}}
← {"jsonrpc":"2.0","id":1,"result":null}
→ {"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"names":["pane_created"]}}
← {"jsonrpc":"2.0","id":2,"result":null}
← {"jsonrpc":"2.0","id":7,"method":"action.invoke","params":{"name":"watcher.scan","args":null,"ctx":{"pane":3,"origin":"Thurm.app","depth":1}}}
→ {"jsonrpc":"2.0","id":7,"result":"scanned 12 files"}
```

A program that exits is started again: at once if it ran for a minute, else
after a pause that doubles with each quick exit, up to a minute. `thurm plugin
list` shows it as `failed`, with why.

## The extension API

Lua plugins (`thurm.call`), program plugins and other clients of the daemon's
socket (the `Ext` request) all use these methods. Parameters and results are
JSON.

| Method | Parameters | Result |
| --- | --- | --- |
| `request` | `{request}`: any daemon request, as `thurm --json` would send it | Its response |
| `actions.list` | | `{actions, bindings}` |
| `actions.register` | `{actions: [{name, title, description, category, hidden, args}]}` | |
| `actions.unregister` | `{names}` | |
| `actions.run` | `{name, args, pane, no_pane, remote, selection, file}`; without `pane` the focused one, unless `no_pane`; the rest reach the action's `ctx` | The action's result |
| `keys.bind`, `keys.unbind` | `{key, action, args}`, `{key}` | |
| `events.subscribe`, `events.unsubscribe` | `{names}` (`"*"` for all) | |
| `events.emit` | `{name, payload}`; the name has a dot | |
| `panes.open` | As `thurm.open` | `{pane, shown}` |
| `ui.toast` | `{text, pane}` | |
| `ui.pick` | `{title, placeholder, items: [{id, title, detail}]}` | `{index, id}` or null |
| `ui.input`, `ui.confirm` | `{title, placeholder, value}`, `{title, body, ok, cancel}` | The text or null; true or false |
| `exec` | `{argv, cwd, env, stdin, timeout_ms}` | `{ok, code, signal, stdout, stderr, timed_out}` |
| `log` | `{level, message}` | |
| `plugins.list` | | `[{name, kind, state, version, path, error, actions}]` |
| `plugins.reload`, `plugins.enable`, `plugins.disable`, `plugins.logs` | `{name}` (`lines` for logs) | |
| `script.run` | `{source, name, args, dir, pane}` | What the script returned |
| `ports.list` | `{pane}` (every pane without) | `[{pane, ports, complete}]` |
| `forwards.list` | | `[{id, host, remote_port, local_port, url}]` |
| `forwards.open` | `{host, port, browse}` | The forward, with `new` when it was opened now |
| `forwards.close` | `{id}` | The forward |
| `forwards.report`, `ports.report` | From the app only: its forwards, and a remote host's ports | |

The `forwards` methods need the Thurm app open (forwards live in it); without
it, or with an app from before them, they fail and say so. Each app numbers its
own forwards, so with two open (a development build beside the installed one),
plugins see only the forwards of the one that connected first, and open and
close through it; when it quits, its forwards are heard closed and the other's
opened.
A `forwards.open` or `forwards.close` whose caller stopped waiting (the plugin
stopped, or the call timed out) is not carried out if the app hears so before
it starts. A close starts as soon as the app has it. An open the app hears of
once it has started opens no browser, and the app closes again the forward it
opened for it, unless someone else got that forward meanwhile (a plugin that
waited, or you opening its link).

A plugin's actions, keys and subscriptions go away when it stops. The API is
for clients on the daemon's machine and its plugins: a caller over the tailnet
may only read `actions.list` and `plugins.list`. The app's Secure Keyboard
Entry switch is never offered as an action.
