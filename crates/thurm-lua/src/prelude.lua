-- The `thurm` table, built on the runtime's primitives (P). Every call that can wait yields
-- the coroutine the runtime runs the code in, so the daemon keeps going meanwhile.
local P = ...

local thurm = {
  plugin = P.plugin,
  dir = P.dir,
  settings = P.settings or {},
  args = P.args or {},
  pane = P.pane,
}

local function call(method, params)
  if P.blocks(method) then
    local ticket, why = P.start(method, params)
    if not ticket then
      error(why, 0)
    end
    local ok, result = coroutine.yield(ticket)
    if not ok then
      error(result, 0)
    end
    return result
  end
  local ok, result = P.call(method, params)
  if not ok then
    error(result, 0)
  end
  return result
end

thurm.call = call

function thurm.request(request)
  return call("request", { request = request })
end

local function request(request)
  return thurm.request(request)
end

local function check(cond, usage)
  if not cond then
    error(usage, 3)
  end
end

-- Actions -------------------------------------------------------------------------------------

function thurm.action(spec)
  check(type(spec) == "table" and type(spec.name) == "string" and type(spec.run) == "function",
    "usage: thurm.action{ name = \"x.y\", run = function(ctx, args) ... end }")
  P.set_action(spec.name, spec.run)
  call("actions.register", {
    actions = { {
      name = spec.name,
      title = spec.title,
      description = spec.description,
      category = spec.category,
      hidden = spec.hidden,
      args = spec.args,
    } },
  })
end

function thurm.run(name, args, pane)
  return call("actions.run", { name = name, args = args, pane = pane, depth = P.depth() })
end

function thurm.actions()
  return call("actions.list")
end

function thurm.bind(key, target, args)
  check(type(key) == "string" and (type(target) == "string" or type(target) == "function"),
    "usage: thurm.bind(\"cmd+shift+g\", \"action.name\" or function(ctx) ... end)")
  local name = target
  if type(target) == "function" then
    name = P.plugin .. ".key." .. P.next_id()
    P.set_action(name, target)
    call("actions.register", { actions = { { name = name, title = key, hidden = true } } })
  end
  call("keys.bind", { key = key, action = name, args = args })
end

function thurm.unbind(key)
  call("keys.unbind", { key = key })
end

-- Events --------------------------------------------------------------------------------------

function thurm.on(event, fn)
  check(type(event) == "string" and type(fn) == "function",
    "usage: thurm.on(\"agent_finished\", function(ev) ... end)")
  if P.on(event, fn) then
    call("events.subscribe", { names = { event } })
  end
end

function thurm.emit(name, payload)
  call("events.emit", { name = name, payload = payload })
end

-- Panes ---------------------------------------------------------------------------------------

function thurm.panes()
  return request("ListPanes").Panes
end

function thurm.info(pane)
  return request({ PaneInfo = { pane = pane } }).PaneInfo
end

function thurm.focused()
  local history = request("FocusHistory").FocusHistory or {}
  for _, r in ipairs(history) do
    if r.focused then
      return r.pane
    end
  end
  return history[1] and history[1].pane
end

function thurm.open(opts)
  return call("panes.open", opts or {}).pane
end

function thurm.split(dir, opts)
  opts = opts or {}
  opts.split = dir or "right"
  return thurm.open(opts)
end

function thurm.send(pane, text, opts)
  opts = opts or {}
  local bytes = { string.byte(text, 1, -1) }
  if #bytes > 0 then
    request({ Input = { pane = pane, data = bytes } })
  end
  if opts.enter ~= false then
    -- Agent TUIs take a burst of input as a paste, and an Enter inside it as a newline.
    if #bytes > 0 then
      thurm.sleep(0.15)
    end
    request({ Input = { pane = pane, data = { 13 } } })
  end
end

function thurm.capture(pane, opts)
  opts = opts or {}
  return request({ Capture = { pane = pane, opts = {
    lines = opts.lines,
    scrollback = opts.scrollback == true,
    ansi = opts.ansi == true,
  } } }).Text
end

function thurm.close(pane)
  request({ ClosePane = { pane = pane } })
end

function thurm.focus(pane)
  request({ Ui = { Focus = { pane = pane } } })
end

function thurm.wait(pane, opts)
  opts = opts or {}
  local until_
  if opts.match then
    until_ = { Match = { regex = opts.match } }
  elseif opts.idle then
    until_ = { Idle = { quiet_ms = math.floor(opts.idle * 1000) } }
  elseif opts.exit then
    until_ = "Exit"
  elseif opts.agent then
    until_ = "AgentFree"
  else
    until_ = "Prompt"
  end
  local r = request({ Wait = {
    pane = pane,
    ["until"] = until_,
    timeout_ms = opts.timeout and math.floor(opts.timeout * 1000) or nil,
  } }).Wait
  if r == "Satisfied" then
    return true
  elseif r == "Timeout" then
    return false, "timeout"
  end
  return false, "exited", r.Exited and r.Exited.code
end

-- The user ------------------------------------------------------------------------------------

thurm.ui = {}

function thurm.ui.toast(text, pane)
  call("ui.toast", { text = tostring(text), pane = pane })
end

function thurm.ui.pick(opts)
  check(type(opts) == "table" and type(opts.items) == "table",
    "usage: thurm.ui.pick{ title = \"...\", items = { \"a\", { title = \"b\", detail = \"...\" } } }")
  local items = {}
  for i, item in ipairs(opts.items) do
    if type(item) == "table" then
      items[i] = { id = i, title = tostring(item.title or item[1] or i), detail = item.detail or "" }
    else
      items[i] = { id = i, title = tostring(item) }
    end
  end
  if #items == 0 then
    return nil
  end
  local r = call("ui.pick", { title = opts.title, placeholder = opts.placeholder, items = items })
  if r == nil then
    return nil
  end
  return opts.items[r.index + 1], r.index + 1
end

function thurm.ui.input(opts)
  if type(opts) == "string" then
    opts = { title = opts }
  end
  return call("ui.input", opts or {})
end

function thurm.ui.confirm(opts)
  if type(opts) == "string" then
    opts = { title = opts }
  end
  return call("ui.confirm", opts or {}) == true
end

-- Programs and time ---------------------------------------------------------------------------

function thurm.exec(argv, opts)
  opts = opts or {}
  if type(argv) == "string" then
    argv = { "sh", "-c", argv }
  end
  return call("exec", {
    argv = argv,
    cwd = opts.cwd,
    env = opts.env,
    stdin = opts.stdin,
    timeout_ms = opts.timeout and math.floor(opts.timeout * 1000) or nil,
  })
end

local function timer(id)
  return { id = id, cancel = function() P.cancel(id) end }
end

function thurm.after(secs, fn)
  check(type(secs) == "number" and type(fn) == "function", "usage: thurm.after(seconds, function() ... end)")
  return timer(P.timer(secs, fn, false))
end

function thurm.every(secs, fn)
  check(type(secs) == "number" and secs > 0 and type(fn) == "function",
    "usage: thurm.every(seconds, function() ... end)")
  return timer(P.timer(secs, fn, true))
end

function thurm.sleep(secs)
  local ticket, why = P.sleep(secs or 0)
  if not ticket then
    error(why, 0)
  end
  local ok, err = coroutine.yield(ticket)
  if not ok then
    error(err, 0)
  end
end

-- Odds and ends -------------------------------------------------------------------------------

thurm.json = { encode = P.json_encode, decode = P.json_decode }

local function line(...)
  local parts = {}
  for i = 1, select("#", ...) do
    parts[i] = tostring((select(i, ...)))
  end
  return table.concat(parts, " ")
end

function thurm.log(...)
  P.log("info", line(...))
end

function thurm.warn(...)
  P.log("warn", line(...))
end

print = thurm.log

local function refuse(name)
  return function()
    error(name .. " is not available in Thurm; use thurm.exec", 2)
  end
end
os.execute = refuse("os.execute")
os.exit = refuse("os.exit")
io.popen = refuse("io.popen")

if P.dir then
  package.path = P.dir .. "/?.lua;" .. P.dir .. "/?/init.lua"
else
  package.path = ""
end
package.cpath = ""

return thurm
