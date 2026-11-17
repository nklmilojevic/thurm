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
-- than one opened twice).
--
-- Settings, in config.toml:
--
--   [plugins.settings]
--   dev-server = { commands = ["vite", "next"] }   -- which listening programs count
--
-- A program counts when a word of its command line is one of `commands`; the default list
-- has the common ones (vite, next, astro, webpack, rails, uvicorn...).

local DEFAULT_COMMANDS = {
  "vite", "next", "nuxt", "astro", "remix", "webpack", "webpack-dev-server", "parcel",
  "rails", "puma", "uvicorn", "gunicorn", "flask", "manage.py", "hugo", "jekyll",
  "storybook", "wrangler", "http.server",
}

local commands = {}
for _, c in ipairs(thurm.settings.commands or DEFAULT_COMMANDS) do
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

local function open(host, port)
  if host then
    local ok, err = pcall(thurm.forward.open, host, port, { browse = true })
    if not ok then
      thurm.warn("cannot forward " .. host .. ":" .. port .. ": " .. tostring(err))
    end
  else
    thurm.exec({ "open", "http://localhost:" .. port .. "/" })
  end
end

thurm.on("ports_changed", function(ev)
  local key = (ev.host or "") .. "/" .. ev.id
  -- The app lost the host (or quit): nothing is known of its ports until it is back.
  if ev.gone then
    listed[key] = nil
    unsure[key] = nil
    return
  end
  local before = listed[key] or {}
  local now = servers(ev.ports) or {}
  if not ev.complete then
    -- Some processes could not be read: a server missing from this list may still run.
    for port in pairs(before) do
      now[port] = true
    end
  end
  listed[key] = next(now) and now or nil
  if unsure[key] then
    if ev.complete then
      unsure[key] = nil
    end
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
      open(ev.host, port)
    end
  end
end)
