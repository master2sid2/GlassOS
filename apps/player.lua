-- Player: MP3, OGG Vorbis and WAV, in a dark "hardware" look.
-- Open a file: every audio file in its folder becomes the playlist. Several files
-- or folders (Open..., or dropped from Files) become the playlist as they are.
-- Decoding and the spectrum come from the system (audio.*); this is only the window.

app = { title = "Player", category = "Media", width = 640, height = 760, min_width = 640, min_height = 640, background = 0x1E2125 }
-- The spectrum moves ~30 times a second; only its box is redrawn for that.
app.tick_every = 33

local CONFIG = "player.cfg"
local EXT = { mp3 = true, ogg = true, oga = true, wav = true }
local MAX_FILES, MAX_DEPTH = 5000, 8

-- Colours.
local BG, PANEL_TOP, PANEL_BOTTOM, EDGE = 0x1E2125, 0x30353B, 0x24282D, 0x0E1012
local WELL, WELL_EDGE = 0x15181B, 0x0B0D0F
local CYAN, CYAN_DARK = 0x35B8E6, 0x1F8FBF
local TEXT, DIM, FAINT = 0xE8ECF0, 0x8C949E, 0x5A616A
local BUTTON_TOP, BUTTON_BOTTOM = 0x41474F, 0x1F2327

local ROW = 34
local BANDS = 16

local s = {
  list = {},      -- { path, name, size, len, title, artist, album }
  index = 0,      -- playing track, 0 = none
  sel = 0,        -- selected row
  id = nil,
  state = "stopped", pos = 0, len = 0,
  title = "", artist = "", album = "", format = "",
  cover = nil,
  volume = 80, balance = 0,
  scroll = 0,     -- playlist scroll, pixels
  drag = nil,     -- what the mouse holds: "seek", "volume", "balance", "scroll"
  last = "",
  levels = {},    -- spectrum bars (0..100, fractional) with a smooth rise and fall
  drawn = {},     -- bar heights in pixels as last drawn
  probe_at = 0,   -- next track to read the length of
  click = { row = 0, at = 0 },
  status = "",
}
for i = 1, BANDS do s.levels[i], s.drawn[i] = 0, 0 end
local spectrum_box   -- where the bars are: {x, y, w, h}, set by the layout

-- Height of a spectrum bar in pixels for a level 0..100.
local function bar_height(level)
  if not spectrum_box then return 0 end
  return math.floor((spectrum_box[4] - 6) * math.max(0, math.min(level, 100)) / 100)
end

local function name_of(path) return path:match("([^/]+)$") or path end
local function dir_of(path) return path:match("^(.*)/[^/]+$") or "/" end
local function ext_of(path) return (path:match("%.([%w]+)$") or ""):lower() end
local function bare(path) return (name_of(path):gsub("%.%w+$", "")) end

local function clock(ms)
  local sec = math.floor((ms or 0) / 1000)
  return string.format("%d:%02d", sec // 60, sec % 60)
end

local function long_clock(ms)
  local sec = math.floor((ms or 0) / 1000)
  return string.format("%02d:%02d:%02d", sec // 3600, sec // 60 % 60, sec % 60)
end

local function load_cfg()
  local text = fs.read(CONFIG)
  if not text then return end
  s.volume = tonumber(text:match("volume=(%d+)")) or s.volume
  s.balance = tonumber(text:match("balance=(%-?%d+)")) or s.balance
  s.last = text:match("last=([^\n]*)") or ""
end

local function save_cfg()
  fs.write(CONFIG, string.format("volume=%d\nbalance=%d\nlast=%s\n", s.volume, s.balance, s.last))
end

-- ------------------------------------------------------------------ playlist

local function entry(path, size)
  return { path = path, name = bare(path), size = size or 0 }
end

local function label(e)
  if e.title and e.title ~= "" then
    if e.artist and e.artist ~= "" then return e.artist .. " - " .. e.title end
    return e.title
  end
  return e.name
end

-- Audio files of a folder and its subfolders, sorted by path.
local function collect(dir, out, depth)
  if depth > MAX_DEPTH or #out >= MAX_FILES then return end
  local files, dirs = {}, {}
  for _, e in ipairs(fs.list(dir) or {}) do
    local p = dir .. "/" .. e.name
    if e.dir then dirs[#dirs + 1] = p
    elseif EXT[ext_of(e.name)] then files[#files + 1] = entry(p, e.size) end
  end
  table.sort(files, function(a, b) return a.path:lower() < b.path:lower() end)
  table.sort(dirs, function(a, b) return a:lower() < b:lower() end)
  for _, e in ipairs(files) do
    if #out >= MAX_FILES then return end
    out[#out + 1] = e
  end
  for _, d in ipairs(dirs) do collect(d, out, depth + 1) end
end

local function close_track()
  if s.id then audio.close(s.id) end
  s.id, s.cover = nil, nil
  s.state, s.pos, s.len = "stopped", 0, 0
  for i = 1, BANDS do s.levels[i] = 0 end
end

local list_h = 300   -- set by the layout every frame

local function keep_visible(i)
  local top = (i - 1) * ROW
  if top < s.scroll then s.scroll = top
  elseif top + ROW > s.scroll + list_h then s.scroll = top + ROW - list_h end
end

local function start(i)
  close_track()
  local e = s.list[i]
  if not e then return end
  s.index, s.sel = i, i
  keep_visible(i)
  s.reveal = true   -- once more after the layout knows the list height
  local id, err = audio.open(e.path)
  if not id then
    s.status = err or "cannot open"
    s.title, s.artist, s.album, s.format = e.name, "", "", ""
    return
  end
  s.id = id
  s.cover = audio.cover(id)
  audio.volume(id, s.volume)
  audio.balance(id, s.balance)
  audio.play(id)
  local info = audio.info(id) or {}
  s.title = (info.title and info.title ~= "") and info.title or e.name
  s.artist, s.album = info.artist or "", info.album or ""
  e.title, e.artist, e.album = info.title, info.artist, info.album
  s.format = string.format("%s  %d Hz  %s", info.format or "?", info.rate or 0, (info.channels or 2) == 1 and "mono" or "stereo")
  s.status = ""
  s.last = e.path
  save_cfg()
end

local function set_list(list, first)
  if #list == 0 then
    s.status = "No music files there."
    return
  end
  s.list, s.scroll, s.probe_at = list, 0, 1
  start(first or 1)
end

-- A file: its folder is the playlist, starting from it.
local function open_path(path)
  local dir, list = dir_of(path), {}
  for _, e in ipairs(fs.list(dir) or {}) do
    if not e.dir and EXT[ext_of(e.name)] then list[#list + 1] = entry(dir .. "/" .. e.name, e.size) end
  end
  table.sort(list, function(a, b) return a.path:lower() < b.path:lower() end)
  if #list == 0 then list = { entry(path) } end
  for i, e in ipairs(list) do
    if e.path == path then return set_list(list, i) end
  end
  set_list(list, 1)
end

-- Several files or folders at once.
local function open_list(paths)
  if #paths == 1 and not fs.isdir(paths[1]) then return open_path(paths[1]) end
  local list = {}
  for _, p in ipairs(paths) do
    if fs.isdir(p) then collect(p, list, 1)
    elseif EXT[ext_of(p)] then list[#list + 1] = entry(p) end
  end
  set_list(list, 1)
end

-- Add to the end of the playlist (the "+" button).
local function add_paths(paths)
  local more = {}
  for _, p in ipairs(paths) do
    if fs.isdir(p) then collect(p, more, 1)
    elseif EXT[ext_of(p)] then more[#more + 1] = entry(p) end
  end
  local was = #s.list
  for _, e in ipairs(more) do s.list[#s.list + 1] = e end
  if s.probe_at == 0 then s.probe_at = was + 1 end
  if was == 0 and #s.list > 0 then start(1) end
end

local function remove_selected()
  local i = s.sel
  if i < 1 or i > #s.list then return end
  table.remove(s.list, i)
  if i == s.index then
    close_track()
    s.index = 0
  elseif i < s.index then
    s.index = s.index - 1
  end
  if s.probe_at > i then s.probe_at = s.probe_at - 1 end
  s.sel = math.min(i, #s.list)
end

local function next_track(step)
  if #s.list == 0 then return end
  local i = s.index + step
  if i < 1 or i > #s.list then
    close_track()
    s.status = "End of the playlist."
    return
  end
  start(i)
end

local function toggle()
  if not s.id then
    if #s.list > 0 then start(math.max(s.sel, 1)) end
    return
  end
  if s.state == "playing" then audio.pause(s.id) else audio.play(s.id) end
end

local function stop()
  if s.id then audio.stop(s.id) end
end

function app.open(paths) open_list(paths) end

load_cfg()
if arg and arg[1] then open_path(arg[1]) end

-- ------------------------------------------------------------------ timing

local probe_next = 0

function app.tick()
  -- Lengths and tags of the playlist, one file at a time (only its header is read).
  local now = sys.ticks()
  local changed = false
  if s.probe_at > 0 and now >= probe_next then
    local e = s.list[s.probe_at]
    while e and e.len do
      s.probe_at = s.probe_at + 1
      e = s.list[s.probe_at]
    end
    if e then
      local info = audio.probe(e.path)
      e.len = info and info.len or 0
      if info then e.title, e.artist, e.album = info.title, info.artist, info.album end
      s.probe_at = s.probe_at + 1
      probe_next = now + 40
    else
      s.probe_at = 0
    end
    changed = true
  end
  if not s.id then return changed end
  local st = audio.status(s.id)
  if not st then
    s.id = nil
    return true
  end
  if st.state ~= s.state then changed = true end
  s.state, s.pos, s.len = st.state, st.pos, st.len
  -- The exact length of the playing track (the estimate from the header may be off).
  local cur = s.list[s.index]
  if cur and st.len > 0 and cur.len ~= st.len then cur.len = st.len; changed = true end
  if st.state == "ended" then
    next_track(1)
  elseif st.state == "back" then
    if s.index > 1 then next_track(-1) else audio.seek(s.id, 0); audio.play(s.id) end
  end
  local shown = s.pos // 1000
  if shown ~= s.shown_sec then s.shown_sec = shown; changed = true end
  -- Spectrum: rises fast, falls smoothly, as on a hardware meter.
  local bars = (s.id and s.state == "playing") and audio.spectrum(s.id, BANDS) or {}
  local moved = false
  for i = 1, BANDS do
    local new, old = bars[i] or 0, s.levels[i]
    local v
    if new > old then v = old + (new - old) * 0.7 else v = math.max(new, old - 3.5) end
    s.levels[i] = v
    if bar_height(v) ~= s.drawn[i] then moved = true end
  end
  if changed then return true end
  -- Only the bars moved: redraw just their box.
  if moved then return spectrum_box end
  return false
end

function app.key(k)
  if k == "space" then toggle()
  elseif k == "right" and s.id then audio.seek(s.id, s.pos + 10000)
  elseif k == "left" and s.id then audio.seek(s.id, math.max(0, s.pos - 10000))
  elseif k == "down" then s.sel = math.min(#s.list, s.sel + 1); keep_visible(s.sel)
  elseif k == "up" then s.sel = math.max(1, s.sel - 1); keep_visible(s.sel)
  elseif k == "enter" then if s.sel > 0 then start(s.sel) end
  elseif k == "delete" then remove_selected()
  else return false end
  return true
end

-- ------------------------------------------------------------------ drawing

local g -- gfx of the current frame

local function panel(x, y, w, h)
  g.rect(x, y + 2, w, h, 0x000000, 8, 90)
  g.gradient(x, y, w, h, PANEL_TOP, PANEL_BOTTOM, 8)
  g.stroke(x, y, w, h, EDGE, 8)
  g.rect(x + 8, y + 1, w - 16, 1, 0xFFFFFF, 0, 18)
end

local function well(x, y, w, h, r)
  g.rect(x, y, w, h, WELL, r or 4)
  g.stroke(x, y, w, h, WELL_EDGE, r or 4)
  g.rect(x + 2, y + h - 1, w - 4, 1, 0xFFFFFF, 0, 14)
end

-- A filled triangle pointing right (dir 1) or left (dir -1), height h.
local function triangle(x, y, h, color, dir)
  local half = h / 2
  local width = math.floor(half * 1.6)
  for i = 0, h - 1 do
    local w = math.floor((half - math.abs(i - half + 0.5)) * 1.6 + 0.5)
    if w > 0 then
      if dir == 1 then g.rect(x, y + i, w, 1, color) else g.rect(x + width - w, y + i, w, 1, color) end
    end
  end
end

local function round_button(cx, cy, r, pressed)
  g.circle(cx, cy + 2, r + 1, 0x000000, 110)
  g.circle(cx, cy, r + 1, EDGE)
  local top, bottom = BUTTON_TOP, BUTTON_BOTTOM
  if pressed then top, bottom = bottom, top end
  g.gradient(cx - r, cy - r, 2 * r, 2 * r, top, bottom, r)
  g.circle(cx, cy, r - 5, 0x000000, 40)
end

local function bar(x, y, w, h, value, max, color)
  well(x, y, w, h, h // 2)
  local fill = math.floor(w * math.max(0, math.min(value, max)) / math.max(max, 1))
  if fill > 0 then g.gradient(x, y, math.max(fill, h), h, color, CYAN_DARK, h // 2) end
end

local function fit(text, size, font, width)
  if g.measure(text, size, font) <= width then return text end
  while #text > 0 and g.measure(text .. "...", size, font) > width do
    text = text:sub(1, -2)
    -- Do not cut a UTF-8 character in half.
    while #text > 0 and (text:byte(-1) or 0) >= 0x80 and (text:byte(-1) or 0) < 0xC0 do text = text:sub(1, -2) end
    if #text > 0 and (text:byte(-1) or 0) >= 0xC0 then text = text:sub(1, -2) end
  end
  return text .. "..."
end

-- Areas the mouse can hit, set while drawing.
local hit = {}

-- tx, tw: the track of a slider (where 0 and the maximum are), when it is one.
local function region(name, x, y, w, h, tx, tw) hit[#hit + 1] = { name = name, x = x, y = y, w = w, h = h, tx = tx, tw = tw } end

local function hit_at(x, y)
  for i = #hit, 1, -1 do
    local r = hit[i]
    if x >= r.x and x < r.x + r.w and y >= r.y and y < r.y + r.h then return r end
  end
end

-- A speaker: the body, the cone and as many waves as the volume deserves.
local function speaker(x, cy, volume)
  g.rect(x, cy - 4, 4, 8, DIM, 1)
  for j = 0, 5 do g.rect(x + 4 + j, cy - 4 - j, 1, 8 + 2 * j, DIM) end
  local waves = volume <= 0 and 0 or (volume < 50 and 1 or 2)
  g.clip(x + 12, cy - 9, 12, 19)
  for k = 1, waves do
    local r = 4 * k + 1
    g.stroke(x + 9 - r, cy - r, 2 * r, 2 * r, DIM, r)
    g.stroke(x + 10 - r, cy - r + 1, 2 * r - 2, 2 * r - 2, DIM, r - 1)
  end
  g.clip()
  if volume <= 0 then
    g.line(x + 14, cy - 3, x + 20, cy + 3, DIM)
    g.line(x + 14, cy + 3, x + 20, cy - 3, DIM)
  end
end

-- The spectrum bars inside spectrum_box (its background included: a partial frame
-- redraws only this box).
local function draw_bars()
  local x0, y0, w, h = spectrum_box[1], spectrum_box[2], spectrum_box[3], spectrum_box[4]
  g.rect(x0, y0, w, h, WELL)
  local step = (w - 8) / BANDS
  for i = 1, BANDS do
    local bh = bar_height(s.levels[i])
    s.drawn[i] = bh
    if bh > 0 then
      local x = x0 + 4 + math.floor((i - 1) * step)
      g.gradient(x, y0 + h - 3 - bh, math.floor(step) - 3, bh, CYAN, CYAN_DARK)
    end
  end
end

local function draw_top(W)
  panel(10, 10, W - 20, 290)
  -- Cover.
  local cx, cy, cs = 28, 28, 184
  g.rect(cx - 2, cy - 2, cs + 4, cs + 4, 0x000000, 3)
  if s.cover then
    g.image(s.cover, cx, cy, cs, cs, "cover")
  else
    g.gradient(cx, cy, cs, cs, 0x2A2F35, 0x1A1D21)
    g.circle(cx + cs // 2 - 14, cy + cs // 2 + 26, 18, 0x454B54)
    g.rect(cx + cs // 2, cy + cs // 2 - 46, 7, 74, 0x454B54)
    g.rect(cx + cs // 2, cy + cs // 2 - 46, 32, 9, 0x454B54)
  end
  -- Balance.
  local bx, by, bw = 52, 248, 136
  g.text(cx, by - 7, "L", DIM, 13, "bold")
  g.text(bx + bw + 10, by - 7, "R", DIM, 13, "bold")
  well(bx, by, bw, 8, 4)
  -- The centre mark: the knob snaps to it.
  local mid = bx + bw // 2
  g.rect(mid - 1, by - 12, 2, 6, DIM, 1)
  g.rect(mid - 1, by + 14, 2, 6, DIM, 1)
  local kx = bx + math.floor((s.balance + 100) * bw / 200)
  g.circle(kx, by + 4, 10, EDGE)
  g.gradient(kx - 9, by - 5, 18, 18, BUTTON_TOP, BUTTON_BOTTOM, 9)
  g.rect(kx - 1, by - 1, 2, 10, DIM)
  region("balance", bx - 10, by - 10, bw + 20, 28, bx, bw)

  -- Title: artist - album, then the track.
  local tx, tw = 232, W - 232 - 30
  local head = s.artist ~= "" and s.artist or (s.title ~= "" and s.title or "No track")
  local aw = g.text(tx, 28, fit(head, 21, "bold", tw), TEXT, 21, "bold")
  if s.album ~= "" and aw + 40 < tw then
    g.text(tx + aw, 30, fit("  -  " .. s.album, 18, "regular", tw - aw), DIM, 18)
  end
  local second = s.artist ~= "" and s.title or s.format
  g.text(tx, 60, fit(second or "", 16, "regular", tw), DIM, 16)

  -- Transport: back, play/pause, stop, forward.
  local y = 118
  local buttons = { { "prev", tx + 24, 24 }, { "play", tx + 84, 30 }, { "stop", tx + 146, 24 }, { "next", tx + 200, 24 } }
  for _, b in ipairs(buttons) do
    local name, x, r = b[1], b[2], b[3]
    round_button(x, y, r, false)
    if name == "play" then
      if s.state == "playing" then
        g.rect(x - 8, y - 10, 6, 20, CYAN, 1)
        g.rect(x + 3, y - 10, 6, 20, CYAN, 1)
      else
        triangle(x - 6, y - 12, 24, CYAN, 1)
      end
    elseif name == "stop" then
      g.rect(x - 8, y - 8, 16, 16, DIM, 2)
    elseif name == "prev" then
      triangle(x - 12, y - 7, 14, DIM, -1)
      triangle(x - 2, y - 7, 14, DIM, -1)
    else
      triangle(x - 10, y - 7, 14, DIM, 1)
      triangle(x, y - 7, 14, DIM, 1)
    end
    region(name, x - r, y - r, 2 * r, 2 * r)
  end

  -- Time: what is left, big; what has played, small.
  local right = W - 32
  local left_ms = s.len > 0 and math.max(0, s.len - s.pos) or s.pos
  local big = (s.len > 0 and "-" or "") .. clock(left_ms)
  g.text(right - g.measure(big, 34, "bold"), 92, big, 0xC9CFD6, 34, "bold")
  local small = long_clock(s.pos)
  g.text(right - g.measure(small, 17), 136, small, DIM, 17)

  -- Volume on the right of the spectrum: speaker, then the slider.
  local vw = 120
  local vx, vy = W - 32 - vw, 196
  local ix = vx - 30
  speaker(ix, vy + 4, s.volume)
  bar(vx, vy - 1, vw, 10, s.volume, 100, CYAN)
  region("volume", vx - 4, vy - 10, vw + 8, 28, vx, vw)

  -- Spectrum.
  local sx, sy, sh = tx, 162, 74
  local sw = math.max(180, ix - 16 - sx)
  well(sx, sy, sw, sh)
  spectrum_box = { sx + 2, sy + 2, sw - 4, sh - 5 }
  draw_bars()

  -- Position.
  local px, py, pw = tx, 262, W - tx - 32
  bar(px, py, pw, 12, s.pos, math.max(s.len, 1), CYAN)
  region("seek", px - 4, py - 8, pw + 8, 28, px, pw)
end

local function draw_bottom(W, H)
  local top = 314
  panel(10, top, W - 20, H - top - 10)
  -- Summary: total time; songs, albums, size.
  local hx, hy, hw = 28, top + 18, W - 56
  well(hx, hy, hw, 34)
  local total, albums, seen, size, known = 0, 0, {}, 0, true
  for _, e in ipairs(s.list) do
    if e.len then total = total + e.len else known = false end
    if e.album and e.album ~= "" and not seen[e.album] then seen[e.album] = true; albums = albums + 1 end
    size = size + (e.size or 0)
  end
  g.text(hx + 14, hy + 8, long_clock(total) .. (known and "" or " ..."), DIM, 15)
  local summary = string.format("%d Songs : %d Albums : %s", #s.list, albums, sys.size(size))
  g.text(hx + hw - 14 - g.measure(summary, 15), hy + 8, summary, DIM, 15)

  -- The list.
  local lx, ly, lw = 28, hy + 46, W - 56 - 18
  local lh = H - ly - 74
  list_h = lh
  if s.reveal then
    s.reveal = false
    if s.index > 0 then keep_visible(s.index) end
  end
  well(lx, ly, lw, lh)
  local max_scroll = math.max(0, #s.list * ROW - lh)
  s.scroll = math.max(0, math.min(s.scroll, max_scroll))
  g.clip(lx + 1, ly + 1, lw - 2, lh - 2)
  local first = s.scroll // ROW + 1
  for i = first, math.min(#s.list, first + lh // ROW + 1) do
    local e = s.list[i]
    local y = ly + (i - 1) * ROW - s.scroll
    local current = i == s.index
    if current then
      g.gradient(lx + 1, y, lw - 2, ROW, CYAN, CYAN_DARK)
    elseif i == s.sel then
      g.rect(lx + 1, y, lw - 2, ROW, 0xFFFFFF, 0, 18)
    end
    local color = current and 0xFFFFFF or 0xC3C9D0
    triangle(lx + 14, y + 12, 9, current and 0xFFFFFF or FAINT, 1)
    local len = (e.len and e.len > 0) and clock(e.len) or ""
    local lenw = g.measure(len, 16)
    g.text(lx + lw - 16 - lenw, y + 8, len, color, 16)
    g.text(lx + 32, y + 8, fit(i .. ". " .. label(e), 16, "regular", lw - 64 - lenw), color, 16)
  end
  g.clip()
  region("list", lx, ly, lw, lh)
  if #s.list == 0 then
    g.text(lx + 18, ly + 14, s.status ~= "" and s.status or "Open music with + or drop files here.", FAINT, 15)
  end
  -- Scrollbar.
  local sx = lx + lw + 6
  well(sx, ly, 10, lh, 5)
  if max_scroll > 0 then
    local th = math.max(30, lh * lh // (#s.list * ROW))
    local ty = ly + (lh - th) * s.scroll // max_scroll
    g.gradient(sx + 1, ty, 8, th, BUTTON_TOP, BUTTON_BOTTOM, 4)
  end
  region("scroll", sx - 2, ly, 14, lh)

  -- Add and remove.
  local by = H - 44
  round_button(48, by, 18, false)
  g.rect(41, by - 1, 14, 3, DIM)
  g.rect(46, by - 6, 3, 14, DIM)
  round_button(96, by, 18, false)
  g.rect(89, by - 1, 14, 3, DIM)
  region("add", 30, by - 18, 36, 36)
  region("remove", 78, by - 18, 36, 36)
  if s.status ~= "" and #s.list > 0 then g.text(130, by - 9, fit(s.status, 14, "regular", W - 170), FAINT, 14) end
end

-- What a click does.
local function press(r, x, y)
  local name = r.name
  if name == "play" then toggle()
  elseif name == "stop" then stop()
  elseif name == "prev" then next_track(-1)
  elseif name == "next" then next_track(1)
  elseif name == "add" then
    sys.open_files(function(paths) if paths then add_paths(paths) end end, s.last ~= "" and dir_of(s.last) or nil)
  elseif name == "remove" then remove_selected()
  elseif name == "list" then
    local i = (y - r.y + s.scroll) // ROW + 1
    if i >= 1 and i <= #s.list then
      local now = sys.ticks()
      if s.click.row == i and now - s.click.at < 400 then start(i) else s.sel = i end
      s.click = { row = i, at = now }
    end
  end
  if name == "seek" or name == "volume" or name == "balance" or name == "scroll" then s.drag = name end
end

-- Dragging a slider (and the first click on it).
local function drag(x, y)
  local r
  for _, h in ipairs(hit) do if h.name == s.drag then r = h end end
  if not r then return end
  local t
  if r.tx then t = (x - r.tx) / math.max(r.tw, 1) else t = (x - r.x - 4) / math.max(r.w - 8, 1) end
  t = math.max(0, math.min(1, t))
  if s.drag == "seek" and s.id and s.len > 0 then
    local ms = math.floor(t * s.len)
    if math.abs(ms - s.pos) > 500 then audio.seek(s.id, ms); s.pos = ms end
  elseif s.drag == "volume" then
    local v = math.floor(t * 100 + 0.5)
    if v ~= s.volume then
      s.volume = v
      if s.id then audio.volume(s.id, v) end
    end
  elseif s.drag == "balance" then
    local b = math.floor(t * 200 - 100 + 0.5)
    -- Close to the centre mark (a few pixels) it snaps to the centre.
    if math.abs(x - (r.tx + r.tw / 2)) <= 6 then b = 0 end
    if b ~= s.balance then
      s.balance = b
      if s.id then audio.balance(s.id, b) end
    end
  elseif s.drag == "scroll" then
    local max_scroll = math.max(0, #s.list * ROW - list_h)
    local u = math.max(0, math.min(1, (y - r.y) / math.max(r.h, 1)))
    s.scroll = math.floor(u * max_scroll)
  end
end

function app.ui(ui)
  ui.fill(function(gfx, clicked, x, y, held, wheel)
    g = gfx
    -- Partial frame: only the spectrum moved (the mouse waits for a full one).
    if ui.area and spectrum_box then
      draw_bars()
      return
    end
    local W, H = gfx.w, gfx.h
    -- Mouse first (the regions are from the last frame), then draw.
    if clicked then
      local r = hit_at(x, y)
      if r then press(r, x, y) end
    end
    if held and s.drag then drag(x, y) end
    if not held and s.drag then
      if s.drag == "volume" or s.drag == "balance" then save_cfg() end
      s.drag = nil
    end
    if wheel ~= 0 then s.scroll = s.scroll + wheel * ROW end
    hit = {}
    g.clear(BG)
    draw_top(W)
    draw_bottom(W, H)
  end)
end
