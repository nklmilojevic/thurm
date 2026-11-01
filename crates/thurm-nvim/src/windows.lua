-- thurm-nvim inside Neovim. Every window a pane shows is external; each tab page keeps one
-- hidden ordinary "anchor" window, so Neovim's own layout never empties. Run once with
-- nvim_exec_lua(chan, cols, rows); returns { first window, its anchor }.
local chan, cols, rows = ...
local api = vim.api
local M = {}
_G.thurm_nvim = M

-- tab page -> its anchor window
local anchors = {}
-- the window last reported as current
local last
local group = api.nvim_create_augroup('thurm_nvim', {})

local function is_anchor(win)
  for _, a in pairs(anchors) do
    if a == win then return true end
  end
  return false
end

local function anchor_of(tab)
  local a = anchors[tab]
  if a and api.nvim_win_is_valid(a) then return a end
  anchors[tab] = nil
end

-- A window a pane shows: external, or ordinary and not an anchor (not adopted yet).
local function shown(win)
  if not api.nvim_win_is_valid(win) or is_anchor(win) then return false end
  local c = api.nvim_win_get_config(win)
  return c.external or c.relative == ''
end

local function others_shown(win)
  for _, w in ipairs(api.nvim_list_wins()) do
    if w ~= win and shown(w) then return true end
  end
  return false
end

local making_anchor = false

--- Sizes external `win` to `w` x `h` cells of text.
function M.size(win, w, h)
  if not shown(win) or not api.nvim_win_get_config(win).external then return end
  w, h = math.max(w, 1), math.max(h, 1)
  -- every resize redraws the window
  if api.nvim_win_get_width(win) == w and api.nvim_win_get_height(win) == h then return end
  api.nvim_win_set_config(win, { width = w, height = h })
end

--- Makes ordinary `win` external at `w` x `h`, giving its tab page an anchor first.
function M.adopt(win, w, h)
  if not shown(win) or api.nvim_win_get_config(win).external then return end
  local tab = api.nvim_win_get_tabpage(win)
  if not anchor_of(tab) then
    local buf = api.nvim_create_buf(false, true)
    vim.bo[buf].bufhidden = 'wipe'
    making_anchor = true
    anchors[tab] = api.nvim_open_win(buf, false, { split = 'left', win = win })
    making_anchor = false
  end
  api.nvim_win_set_config(win, { external = true, width = math.max(w, 1), height = math.max(h, 1) })
end

--- Closes `win`, whose pane went away. Unsaved changes stay in its buffer.
function M.close(win)
  if api.nvim_win_is_valid(win) then pcall(api.nvim_win_close, win, true) end
end

-- What Neovim shows when 'statusline' is empty (its default, with 'ruler').
local default_statusline = '%<%f %h%w%m%r%=%-14.(%l,%c%V%) %P'

local hl_cache = {}
local function hl_attrs(name)
  local a = hl_cache[name]
  if a then return a end
  a = api.nvim_get_hl(0, { name = name, link = false, create = false })
  hl_cache[name] = a
  return a
end
api.nvim_create_autocmd('ColorScheme', { group = group, callback = function() hl_cache = {} end })

-- `str` from nvim_eval_statusline as cells: { { text, width, highlight attributes }, ... },
-- each highlight running from its start byte to the next one's.
local function cells_of(str, highlights)
  local out = {}
  local hl, next_hl = nil, 1
  local byte = 0
  for _, ch in ipairs(vim.fn.split(str, '\\zs')) do
    while highlights[next_hl] and highlights[next_hl].start <= byte do
      hl = hl_attrs(highlights[next_hl].group)
      next_hl = next_hl + 1
    end
    local width = vim.fn.strdisplaywidth(ch)
    if width == 0 and #out > 0 then
      -- A combining character: it belongs to the cell before.
      out[#out][1] = out[#out][1] .. ch
    else
      table.insert(out, { ch, width, hl or vim.empty_dict() })
    end
    byte = byte + #ch
  end
  return out
end

--- The statuslines of `wins` at `widths`: per window, nil when it shows none, else its cells
--- (see cells_of).
function M.statuslines(wins, widths)
  local ls = vim.o.laststatus
  local out = {}
  local count = #vim.tbl_filter(shown, api.nvim_tabpage_list_wins(0))
  for i, win in ipairs(wins) do
    local show = api.nvim_win_is_valid(win) and (ls >= 2 or (ls == 1 and count > 1))
    if show then
      local fmt = vim.wo[win].statusline
      if fmt == '' then fmt = default_statusline end
      local ok, r = pcall(api.nvim_eval_statusline, fmt, { winid = win, maxwidth = widths[i], highlights = true })
      if ok then
        out[i] = cells_of(r.str, r.highlights or {})
      else
        out[i] = vim.NIL
      end
    else
      out[i] = vim.NIL
    end
  end
  return out
end

-- The layout node holding `win`: 'row' or 'col', and whether `win` comes first.
local function placement(win)
  local function find(node)
    if node[1] == 'leaf' then return end
    for i, c in ipairs(node[2]) do
      if c[1] == 'leaf' and c[2] == win then return node[1], i == 1 end
      local dir, first = find(c)
      if dir then return dir, first end
    end
  end
  return find(vim.fn.winlayout(api.nvim_tabpage_get_number(api.nvim_win_get_tabpage(win))))
end

-- The window being left: when a window is new, the one that was split. (The alternate window,
-- `winnr('#')`, is the one before that.)
local left
api.nvim_create_autocmd('WinLeave', {
  group = group,
  callback = function() left = api.nvim_get_current_win() end,
})

-- Before the new window is adopted, so its pane opens where Neovim put the window.
api.nvim_create_autocmd('WinNew', {
  group = group,
  callback = function()
    if making_anchor then return end
    local win = api.nvim_get_current_win()
    local dir, first = placement(win)
    if not dir then return end
    local from = (left and left ~= win and api.nvim_win_is_valid(left)) and left or vim.fn.win_getid(vim.fn.winnr('#'))
    -- Splitting an external window loses its scroll: carry it over.
    if shown(from) and api.nvim_win_get_buf(from) == api.nvim_win_get_buf(win) then
      local view = api.nvim_win_call(from, vim.fn.winsaveview)
      vim.fn.winrestview(view)
    end
    vim.rpcnotify(chan, 'thurm_split', win, dir == 'row', first, shown(from) and from or 0)
  end,
})

local pending = false
local function report()
  pending = false
  local cur = api.nvim_get_current_win()
  if is_anchor(cur) then
    -- Never shown: move on the way <C-w>w or <C-w>W was going.
    local wins = vim.tbl_filter(shown, api.nvim_tabpage_list_wins(0))
    if #wins == 0 then return end
    local prev = vim.fn.win_getid(vim.fn.winnr('#'))
    local to = (last and shown(last)) and last or wins[1]
    if prev == wins[#wins] then
      to = wins[1]
    elseif prev == wins[1] then
      to = wins[#wins]
    end
    api.nvim_set_current_win(to)
    return
  end
  -- A float shows in the pane it floats over, which keeps the focus.
  if api.nvim_win_get_config(cur).relative ~= '' then
    vim.rpcnotify(chan, 'thurm_float', cur)
    return
  end
  last = cur
  vim.rpcnotify(chan, 'thurm_win', cur)
end

-- After the command that moved, so :windo and the like report once.
local function schedule_report()
  if pending then return end
  pending = true
  vim.schedule(report)
end
api.nvim_create_autocmd('WinEnter', { group = group, callback = schedule_report })

-- :q in the last window quits Neovim: make it the tab's only ordinary window first. If Neovim
-- then refuses (unsaved changes), the daemon adopts it again.
api.nvim_create_autocmd('QuitPre', {
  group = group,
  callback = function()
    local cur = api.nvim_get_current_win()
    local tab = api.nvim_get_current_tabpage()
    local anchor = anchor_of(tab)
    if not anchor or not shown(cur) or others_shown(cur) then return end
    local w, h = api.nvim_win_get_width(cur), api.nvim_win_get_height(cur)
    api.nvim_win_set_config(cur, { split = 'left', win = anchor })
    anchors[tab] = nil
    api.nvim_win_close(anchor, true)
    -- Still here afterwards: Neovim refused to quit (unsaved changes). Back to external.
    vim.schedule(function()
      if api.nvim_win_is_valid(cur) and not api.nvim_win_get_config(cur).external then M.adopt(cur, w, h) end
    end)
  end,
})

-- Neovim never closes its last window, but the anchor lets :close do it. So reopen it, and
-- the daemon gives the new window the same pane.
api.nvim_create_autocmd('WinClosed', {
  group = group,
  callback = function(ev)
    local win = tonumber(ev.match)
    if vim.v.exiting ~= vim.NIL or not shown(win) or others_shown(win) then return end
    local c = api.nvim_win_get_config(win)
    if not c.external then return end
    local buf = api.nvim_win_get_buf(win)
    local view = api.nvim_win_call(win, vim.fn.winsaveview)
    vim.rpcnotify(chan, 'thurm_reopen', win)
    vim.schedule(function()
      if not api.nvim_buf_is_valid(buf) then buf = api.nvim_create_buf(true, false) end
      local new = api.nvim_open_win(buf, true, { external = true, width = c.width, height = c.height })
      api.nvim_win_call(new, function() vim.fn.winrestview(view) end)
    end)
  end,
})

local first = api.nvim_get_current_win()
M.adopt(first, cols, rows)
schedule_report()
return { first, anchor_of(api.nvim_get_current_tabpage()) }
