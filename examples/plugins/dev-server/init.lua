-- dev-server: when a dev server in a pane starts listening, open it in the browser. In a
-- remote host's pane, the app forwards the port to this Mac first, so `localhost` works here.
--
-- Install it on the Mac: link this directory into your plugins directory, then reload.
--
--   ln -s "$PWD/examples/plugins/dev-server" ~/.config/thurm/plugins/dev-server
--   thurm reload
--
-- A port opens once while it is listed: a server that stops and starts again opens again.
-- Servers already running when the plugin starts are left alone.
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

-- Addresses reachable as localhost.
local LOCAL = { ["127.0.0.1"] = true, ["::1"] = true, ["0.0.0.0"] = true, ["::"] = true, [""] = true }

local function is_dev_server(command)
  for word in command:gmatch("[^%s]+") do
    -- `node_modules/.bin/vite`, `python3 -m http.server`: the last path component.
    if commands[word:match("([^/]+)$")] then
      return true
    end
  end
  return false
end

-- What each pane listed last: host/pane -> { [port] = true }.
local listed = {}
-- The first lists come right after the plugin starts: they are servers already running.
local ready = false
thurm.after(3, function()
  ready = true
end)

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
  local before = listed[key] or {}
  local now = {}
  for _, p in ipairs(ev.ports) do
    if LOCAL[p.address] and is_dev_server(p.command) then
      now[p.port] = true
      if ready and not before[p.port] then
        open(ev.host, p.port)
      end
    end
  end
  listed[key] = next(now) and now or nil
end)
