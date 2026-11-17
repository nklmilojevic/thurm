-- dev-server: when a dev server in a pane starts listening, open it in the browser. In a
-- remote host's pane, the app forwards the port to this Mac first, so `localhost` works here.
--
-- Install it on the Mac: link this directory into your plugins directory, then reload.
--
--   ln -s "$PWD/examples/plugins/dev-server" ~/.config/thurm/plugins/dev-server
--   thurm reload
--
-- A port opens once while it is listed: a server that stops and starts again opens again.
-- Servers already running are left alone: this Mac's when the plugin starts, a remote host's
-- when Thurm connects to it (its lists then come marked `initial`). A list that could not
-- read every process keeps the servers it left out: they open again only after a complete
-- list without them. A pane first read that way (at load, or as Thurm connects to its host)
-- may have servers the read missed, which its next complete list shows: that list settles
-- what runs and opens nothing, even for one that started meanwhile (a tab not opened rather
-- than one opened twice). A server that shows up in a list that is still incomplete opens.
--
-- Settings, in config.toml:
--
--   [plugins.settings]
--   dev-server = { commands = ["vite", "next"], retry_seconds = 5 }
--
-- `commands` says which listening programs count; `retry_seconds` is the wait between tries
-- of a forward that failed.
--
-- A program counts when a word of its command line is one of `commands`; the default list
-- has the common ones (vite, next, astro, webpack, rails, uvicorn...).

local DEFAULT_COMMANDS = {
  "vite", "next", "nuxt", "astro", "remix", "webpack", "webpack-dev-server", "parcel",
  "rails", "puma", "uvicorn", "gunicorn", "flask", "manage.py", "hugo", "jekyll",
  "storybook", "wrangler", "http.server",
}

local commands = {}
local wanted = thurm.settings.commands
if type(wanted) == "string" then
  wanted = { wanted }
elseif wanted ~= nil and type(wanted) ~= "table" then
  thurm.warn("dev-server: commands should be a list of program names; using the default ones")
  wanted = nil
end
for _, c in ipairs(wanted or DEFAULT_COMMANDS) do
  commands[tostring(c)] = true
end

-- Addresses reachable as localhost: loopback, or every address (`*` is how macOS says it).
local LOCAL = {
  ["127.0.0.1"] = true, ["::1"] = true, ["0.0.0.0"] = true, ["::"] = true, ["*"] = true, [""] = true,
}

local function is_dev_server(command)
  for word in command:gmatch("[^%s]+") do
    -- `node_modules/.bin/vite`, `python3 -m http.server`: the last path component.
    if commands[word:match("([^/]+)$")] then
      return true
    end
  end
  return false
end

-- The dev servers' ports in a list: { [port] = true }, or nil for none.
local function servers(ports)
  local found = {}
  for _, p in ipairs(ports) do
    if LOCAL[p.address] and is_dev_server(p.command) then
      found[p.port] = true
    end
  end
  return next(found) and found or nil
end

-- What each pane listed last: host/pane -> { [port] = true }. This Mac's panes are read
-- before subscribing: what runs then is left alone. The first lists the subscription brings
-- (marked `initial`) are checked against it, so a server that started in between opens.
local listed = {}
-- Panes whose read was not complete (some processes could not be read): a server it missed
-- would look new later. Their first complete list settles what runs, and opens nothing.
local unsure = {}
for _, entry in ipairs(thurm.ports()) do
  local key = "/" .. entry.pane
  listed[key] = servers(entry.ports)
  if not entry.complete then
    unsure[key] = true
  end
end

-- A forward that could not be opened (the host's tunnel was down a moment, the app busy) is
-- tried again a few times while the server is still listed.
local RETRY_SECONDS = tonumber(thurm.settings.retry_seconds) or 5
local RETRIES = 3
-- The try under way for a pane's port (key#port): a later open of it, or the port going away,
-- ends the tries of an earlier one.
local tries = {}

local function open(host, port, key, attempt, token)
  if not host then
    -- `open` that fails (no program opens the link) is warned of: there is nothing to retry.
    local url = "http://localhost:" .. port .. "/"
    local ok, r = pcall(thurm.exec, { "open", url })
    local why = not ok and tostring(r)
      or not r.ok and ((r.stderr or "") ~= "" and r.stderr:gsub("%s+$", "") or "exit " .. tostring(r.code))
    if why then
      thurm.warn("cannot open " .. url .. ": " .. why)
    end
    return
  end
  local id = key .. "#" .. port
  if not token then
    token = (tries[id] or 0) + 1
    tries[id] = token
  end
  local ok, err = pcall(thurm.forward.open, host, port, { browse = true })
  if ok then
    return
  end
  attempt = attempt or 1
  thurm.warn("cannot forward " .. host .. ":" .. port .. ": " .. tostring(err))
  if attempt < RETRIES then
    thurm.after(RETRY_SECONDS, function()
      if tries[id] == token and listed[key] and listed[key][port] then
        open(host, port, key, attempt + 1, token)
      end
    end)
  end
end

-- Ends the tries of a pane's port: a forward tried again later must not open what is no longer
-- the server it was for.
local function end_tries(key, port)
  local id = key .. "#" .. port
  tries[id] = (tries[id] or 0) + 1
end

thurm.on("ports_changed", function(ev)
  local key = (ev.host or "") .. "/" .. ev.id
  local before = listed[key] or {}
  -- The app lost the host (or quit): nothing is known of its ports until it is back, and what
  -- was being tried for them ends (when it is back, its lists say what runs already).
  if ev.gone then
    for port in pairs(before) do
      end_tries(key, port)
    end
    listed[key] = nil
    unsure[key] = nil
    return
  end
  local now = servers(ev.ports) or {}
  if not ev.complete then
    -- Some processes could not be read: a server missing from this list may still run.
    for port in pairs(before) do
      now[port] = true
    end
  end
  -- A port that went: tries of it end (it opens anew if it comes back), whatever this list is.
  for port in pairs(before) do
    if not now[port] then
      end_tries(key, port)
    end
  end
  listed[key] = next(now) and now or nil
  -- A pane first read in part: its first complete list may show servers that read missed, and
  -- settles what runs. Until then, a server an incomplete list shows anew listens for sure (a
  -- pane with a process this user cannot read, such as a `sudo` one, stays incomplete), and
  -- opens as usual.
  if unsure[key] and ev.complete then
    unsure[key] = nil
    return
  end
  -- A remote host's lists sent as the app connected to it: running already. One that could
  -- not read every process may have missed some: the pane's first complete list settles what
  -- runs, as for a pane read in part at load.
  if ev.initial and ev.host then
    if not ev.complete then
      unsure[key] = true
    end
    return
  end
  for port in pairs(now) do
    if not before[port] then
      open(ev.host, port, key)
    end
  end
end)
