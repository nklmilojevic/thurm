---@meta thurm
-- Types of the `thurm` table that Thurm's Lua plugins, init.lua and `thurm do` scripts get,
-- for lua-language-server. Calls that wait (run a program, an action, ask the user, sleep)
-- work in the main chunk, actions, event handlers and timers. Errors are raised.

---@class thurm.Pane
---@field id integer
---@field title string
---@field cwd string?
---@field pid integer?
---@field alive boolean
---@field exit_code integer?
---@field at_prompt boolean
---@field agent table?
---@field size { cols: integer, rows: integer, cell_width: integer, cell_height: integer } Cells, and a cell's size in pixels.
---@field foreground { pid: integer, name: string, argv: string[] }? The program in the foreground.

---@class thurm.Context
---@field pane integer? The pane it runs for: the focused one, when a person asked.
---@field origin string Who asked: a client's name, `plugin:<name>`.
---@field depth integer How many actions deep it runs.
---@field remote string? The `[[remote]]` host whose pane the person is in; `pane` is then nil.
---@field selection string? The text selected in the pane, when an app runs it.
---@field file string? The file the mouse pointer is over in the pane, as written there (relative to the pane's directory unless absolute), when an app runs it.

---@class thurm.ActionSpec
---@field name string `group.verb`: letters, digits, `.`, `_`, `-`, `:` and spaces.
---@field title string? Shown in the command palette; the name when unset.
---@field description string?
---@field category string? Groups it in the menu, prefixes it in the palette.
---@field hidden boolean? Left out of the palette and the menu.
---@field args table? A JSON Schema of what it takes.
---@field run fun(ctx: thurm.Context, args: any): any

---@class thurm.OpenOptions
---@field command string[]? Program and arguments; the shell when unset.
---@field cwd string?
---@field env table<string, string>?
---@field agent string? An agent preset to start instead of `command`.
---@field split "right"|"down"|"left"|"up"? Split `target` instead of opening a tab.
---@field float boolean? Float above `target`'s tab.
---@field frame number[]? Float at `{x, y, width, height}`: fractions of the tab's area, from its top left corner. Set `float` too, for a Thurm from before `frame`.
---@field cells thurm.FloatCells? Float with a terminal of this many cells, placed from `target`'s first cell. Set `float` too, for a Thurm from before `cells`.
---@field window boolean? In a new workspace.
---@field target integer? The pane to split or float over; the focused one when unset.
---@field percent integer? The new pane's share of a split.
---@field background boolean? Keep the focus where it is.
---@field hold boolean? Keep the pane open after its program exits.
---@field detached boolean? Do not show it: place it later.

---@class thurm.FloatCells
---@field cols integer Columns, at least 2.
---@field rows integer
---@field col integer? Its first cell this many columns right of `target`'s first cell (0); may be negative.
---@field row integer? And this many rows below it (0); may be negative.

---@class thurm.ExecResult
---@field ok boolean Exited with 0.
---@field code integer?
---@field signal integer?
---@field stdout string
---@field stderr string
---@field timed_out boolean

---@class thurm.Port
---@field port integer
---@field address string Where it listens: `127.0.0.1`, `::1`, `0.0.0.0`, `::`, `*` (every address, on macOS); empty when the system did not say.
---@field pid integer
---@field command string The listening process's command line.

---@class thurm.PanePorts
---@field pane integer
---@field ports thurm.Port[]
---@field complete boolean False when part of the pane's processes could not be read.

---@class thurm.Forward
---@field id integer
---@field host string The `[[remote]]` host.
---@field remote_port integer
---@field local_port integer The port on this Mac's `127.0.0.1`.
---@field url string The link it was last opened for, on its local port (`http://localhost:<local_port>/app`); else `http://localhost:<local_port>/`.
---@field new boolean? From `thurm.forward.open`: it was opened now, rather than found open.

---@class thurm.Timer
---@field cancel fun()

---@class thurm.Event
---@field event string Its name.
---@field pane thurm.Pane? The pane it is about.
---@field id integer? The pane's id, where `pane` may be missing (pane_closed, bell, notification, ports_changed, files_uploaded); in forward_opened and forward_closed, the forward's (as `thurm.forward.close` takes it), not a pane's.
---@field host string? ports_changed of a remote host's pane (whose `id` is then that host's, not one `thurm.ports` takes), and forward events: the `[[remote]]` host.
---@field ports thurm.Port[]? ports_changed: every port the pane listens on now.
---@field complete boolean? ports_changed: false when part of the pane's processes could not be read (servers may be left out), or with `gone`.
---@field gone boolean? ports_changed of a remote host's pane: the app lost the host (or quit); nothing is known of its ports.
---@field initial boolean? ports_changed: listening already (sent when this plugin subscribed, or by a remote host's daemon as the app connected to it); any other list is a change.
---@field paths string[]? files_uploaded: where the files landed.
---@field remote_port integer? forward_opened, forward_closed.
---@field local_port integer? forward_opened, forward_closed.
---@field url string? forward_opened, forward_closed.
---@field why string? forward_closed: `closed`, `host`, `gone` (the port went away) or `app` (the app quit).

---@class thurm
---@field plugin string This plugin's name (`init` for init.lua, `script` for `thurm do`).
---@field dir string? Its directory, where `require` looks.
---@field settings table `[plugins.settings.<name>]` from config.toml.
---@field args string[] A script's arguments.
---@field pane integer? The pane a script was started from.
thurm = {}

--- Calls a method of the extension API.
---@param method string
---@param params table?
---@return any
function thurm.call(method, params) end

--- Sends any daemon request, as JSON: `thurm.request("ListPanes")`,
--- `thurm.request({ ClosePane = { pane = 3 } })`.
---@param request string|table
---@return any
function thurm.request(request) end

--- Adds an action: the command palette lists it, keys and `thurm action run` run it.
---@param spec thurm.ActionSpec
function thurm.action(spec) end

--- Runs an action (any plugin's, the app's, a `[[commands]]` entry) and returns its result.
---@param name string
---@param args any?
---@param pane integer?
---@return any
function thurm.run(name, args, pane) end

--- Every action and key binding.
---@return { actions: table[], bindings: table[] }
function thurm.actions() end

--- Binds a key (written like a `[keybindings]` chord: "cmd+shift+g") to an action or a function.
---@param key string
---@param action string|fun(ctx: thurm.Context): any
---@param args any?
function thurm.bind(key, action, args) end

---@param key string
function thurm.unbind(key) end

--- Runs `fn` on an event: agent_working, agent_waiting, agent_blocked, agent_finished,
--- agent_failed, agent_idle, command_finished, pane_exited, pane_created, pane_closed,
--- pane_changed, pane_focused, bell, notification, config_reloaded, ports_changed,
--- files_uploaded, forward_opened, forward_closed, a plugin's own (`git.changed`), or `*` for
--- all.
---@param event string
---@param fn fun(ev: thurm.Event)
function thurm.on(event, fn) end

--- Sends an event of this plugin's own; its name has a dot.
---@param name string
---@param payload table?
function thurm.emit(name, payload) end

---@return thurm.Pane[]
function thurm.panes() end

---@param pane integer
---@return thurm.Pane
function thurm.info(pane) end

--- The pane focused in the app now, else the one focused last.
---@return integer?
function thurm.focused() end

--- Opens a new pane; returns its id.
---@param opts thurm.OpenOptions?
---@return integer
function thurm.open(opts) end

--- `thurm.open` with `split = dir`.
---@param dir "right"|"down"|"left"|"up"?
---@param opts thurm.OpenOptions?
---@return integer
function thurm.split(dir, opts) end

--- Types `text` into a pane, then Enter unless `opts.enter == false`.
---@param pane integer
---@param text string
---@param opts { enter: boolean? }?
function thurm.send(pane, text, opts) end

--- The pane's screen as text.
---@param pane integer
---@param opts { lines: integer?, scrollback: boolean?, ansi: boolean? }?
---@return string
function thurm.capture(pane, opts) end

---@param pane integer
function thurm.close(pane) end

---@param pane integer
function thurm.focus(pane) end

--- Waits until the pane's command finishes (default), or `match` (a regex) shows, it is `idle`
--- for some seconds, its process `exit`s, or its `agent` is free. True when it came (with the
--- exit code, waiting for the exit); false and why ("timeout", "exited") otherwise.
---@param pane integer
---@param opts { match: string?, idle: number?, exit: boolean?, agent: boolean?, timeout: number? }?
---@return boolean ok, string? why, integer? exit_code
function thurm.wait(pane, opts) end

--- The TCP ports a pane's programs listen on, and whether all its processes could be read.
--- Without a pane, every pane's: `{ pane, ports, complete }` each. This Mac's panes only.
---@param pane integer
---@return thurm.Port[] ports, boolean complete
---@overload fun(): thurm.PanePorts[]
function thurm.ports(pane) end

thurm.forward = {}

--- Opens a remote host's port on this Mac through the app (or finds the forward that has it);
--- `browse` opens its URL in the browser too. Raises an error, opening nothing, when the
--- forward closed (or moved to another local port) before the app could use it.
---@param host string The `[[remote]]` host.
---@param port integer
---@param opts { browse: boolean? }?
---@return thurm.Forward
function thurm.forward.open(host, port, opts) end

--- Closes a forward.
---@param id integer
---@return thurm.Forward
function thurm.forward.close(id) end

--- Every forward the app has open.
---@return thurm.Forward[]
function thurm.forward.list() end

thurm.ui = {}

--- A short message over a pane (the focused one).
---@param text string
---@param pane integer?
function thurm.ui.toast(text, pane) end

--- The user picks an item; returns it and its index, or nil when cancelled.
---@generic T
---@param opts { title: string?, placeholder: string?, items: T[] }
---@return T?, integer?
function thurm.ui.pick(opts) end

--- The user types a line; nil when cancelled.
---@param opts string|{ title: string?, placeholder: string?, value: string? }
---@return string?
function thurm.ui.input(opts) end

---@param opts string|{ title: string?, body: string?, ok: string?, cancel: string? }
---@return boolean
function thurm.ui.confirm(opts) end

--- Runs a program to its end, through your login shell. A string runs with `sh -c`.
---@param argv string[]|string
---@param opts { cwd: string?, env: table<string, string>?, stdin: string?, timeout: number? }?
---@return thurm.ExecResult
function thurm.exec(argv, opts) end

---@param seconds number
---@param fn fun()
---@return thurm.Timer
function thurm.after(seconds, fn) end

---@param seconds number
---@param fn fun()
---@return thurm.Timer
function thurm.every(seconds, fn) end

---@param seconds number
function thurm.sleep(seconds) end

--- Stops the action, script, handler or timer with a message for the user, shown as it is
--- (`error` also says the file and line, which is for bugs).
---@param message any Shown with `tostring`.
function thurm.fail(message) end

thurm.json = {}

--- JSON's null in what `thurm.json.decode` gives (`[1, null, 3]` keeps three items). In the API's
--- own results a null field is nil, and a null in an array is this value.
thurm.json.null = nil

---@param value any
---@return string
function thurm.json.encode(value) end

---@param text string
---@return any
function thurm.json.decode(text) end

--- Writes a line to the plugin's log (a script's output).
---@param ... any
function thurm.log(...) end

---@param ... any
function thurm.warn(...) end
