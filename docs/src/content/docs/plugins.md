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
`[keybindings]` entry: bound to one of the app's actions, it moves that
action's key; bound to a plugin's action, it runs it. The binding made last
wins, and goes when its plugin stops. A key bound with arguments
(`thurm.bind(key, action, args)`) runs the action with them.

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
| `NAME.anything` | A plugin's own event, sent with `thurm.emit` |
| `*` | Every event above |

`[[on]]` hooks run on the first eight, as before.

## The Lua API

Calls that wait (a program, an action, the user, `sleep`) work in the main
chunk, in actions, in event handlers and in timers; Thurm runs each of these
in a coroutine and keeps going meanwhile, so a slow handler never holds up
your keys. Errors are raised: catch them with `pcall` where you expect them.

| Function | Does |
| --- | --- |
| `thurm.action{name, title, description, category, hidden, args, run}` | Adds an action; `run(ctx, args)` returns its result. `ctx` has `pane`, `origin` and `depth` |
| `thurm.run(name, args, pane)` | Runs any action and returns its result |
| `thurm.actions()` | Every action and binding |
| `thurm.bind(key, action or fn, args)`, `thurm.unbind(key)` | Binds a key |
| `thurm.on(event, fn)`, `thurm.emit(name, payload)` | Hears events; sends one of your own |
| `thurm.panes()`, `thurm.info(pane)`, `thurm.focused()` | Panes, one pane, the focused pane |
| `thurm.open{command, cwd, env, agent, split, float, window, target, percent, background, hold}` | Opens a pane and returns its id |
| `thurm.split(dir, opts)` | `thurm.open` with `split = dir` |
| `thurm.send(pane, text, {enter})`, `thurm.capture(pane, {lines, scrollback, ansi})` | Types into a pane; reads its screen |
| `thurm.wait(pane, {match, idle, exit, agent, timeout})` | Waits for the prompt, a regex, quiet, exit or a free agent; `true`, or `false` and why |
| `thurm.close(pane)`, `thurm.focus(pane)` | Closes or focuses a pane |
| `thurm.ui.toast(text, pane)` | A short message over a pane |
| `thurm.ui.pick{title, placeholder, items}` | The user picks an item (a string, or a table with `title` and `detail`); returns it and its index, or `nil` |
| `thurm.ui.input{title, placeholder, value}`, `thurm.ui.confirm{title, body, ok, cancel}` | A line of text, or yes or no |
| `thurm.exec(argv, {cwd, env, stdin, timeout})` | Runs a program to its end through your login shell; `ok`, `code`, `stdout`, `stderr`, `timed_out`. A string runs with `sh -c` |
| `thurm.after(s, fn)`, `thurm.every(s, fn)`, `thurm.sleep(s)` | Timers; the first two return a handle with `cancel()` |
| `thurm.call(method, params)`, `thurm.request(request)` | Any method of the API below, or any daemon request as JSON |
| `thurm.json.encode`, `thurm.json.decode`, `thurm.log(...)`, `thurm.warn(...)` | JSON, and the plugin's log (`print` logs too) |
| `thurm.plugin`, `thurm.dir`, `thurm.settings`, `thurm.args`, `thurm.pane` | The plugin's name, directory and settings; a script's arguments and pane |

`require` loads modules from the plugin's own directory. `io.popen`,
`os.execute` and `os.exit` are not available (use `thurm.exec`). Code that
runs for 5 seconds without waiting is stopped, and a plugin may use at most
256 MiB.

## Scripts

`thurm do` runs a Lua file in the daemon to its end, prints what it logs, and
then what it returns. It has the same API, with `thurm.args` and the pane it
was started from in `thurm.pane`:

```lua
-- review.lua: open every changed file's diff in a split.
local root = thurm.info(thurm.pane).cwd
local r = thurm.exec({ "git", "diff", "--name-only" }, { cwd = root })
for file in r.stdout:gmatch("[^\n]+") do
  thurm.split("down", { command = { "git", "diff", "--", file }, cwd = root, hold = true })
end
return "opened diffs"
```

```sh
thurm do review.lua
echo 'return #thurm.panes()' | thurm do -
thurm --remote build do deploy.lua staging
```

Stopping `thurm do` (Ctrl-C) stops the script. With `--remote`, the `.lua` files
beside the script (a few levels deep) go along, so `require` finds them on the
other host.

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
plugins, stops removed and disabled ones, and restarts those whose settings
changed and the Lua ones.

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
| `actions.run` | `{name, args, pane}` | The action's result |
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

A plugin's actions, keys and subscriptions go away when it stops.
