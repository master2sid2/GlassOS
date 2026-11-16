# Examples

All examples were run in GlassOS. More examples are in `/apps`: `widgets` shows every widget, `snake` is a game on a canvas, `web` works with the network.

## A clock: a timer and drawing

```lua
-- file: analog.lua
app = { title = "Analog", width = 260, height = 290 }

sys.every(1000, function() end)

function app.ui(ui)
  ui.canvas(220, function(g)
    local cx, cy, t = g.w / 2, 110, sys.time()
    g.clear(0x101320)
    g.circle(cx, cy, 100, 0x232738)
    local function hand(value, len, color)
      local a = value * 2 * math.pi - math.pi / 2
      g.line(cx // 1, cy, (cx + math.cos(a) * len) // 1, (cy + math.sin(a) * len) // 1, color)
    end
    hand((t.hour % 12 + t.min / 60) / 12, 50, 0xFFFFFF)
    hand(t.min / 60, 75, 0x38BDF8)
    hand(t.sec / 60, 90, 0xF87171)
  end)
end
```

## Notes: files and a dialog

```lua
-- file: notes.lua
app = { title = "Notes", width = 420, height = 330 }

local FILE = "notes.txt"
local s = { text = fs.read(FILE) or "", status = "" }

function app.ui(ui)
  s.text = ui.textarea("text", s.text, 190)
  if ui.button("Save") then
    local ok, err = fs.write(FILE, s.text)
    s.status = ok and "Saved" or err
  end
  ui.sameline()
  if ui.button("Save as...") then
    sys.save_file(function(path)
      if path then fs.write(path, s.text) end
    end, "notes.txt")
  end
  ui.label(s.status, 0x8A90A8)
end
```

## A sketch pad: the mouse on a canvas

```lua
-- file: sketch.lua
app = { title = "Sketch", width = 420, height = 360 }

local dots, color = {}, 0x38BDF8

function app.ui(ui)
  local _, x, y, held = ui.canvas(260, function(g)
    g.clear(0x101320)
    for _, d in ipairs(dots) do g.circle(d.x, d.y, 4, d.c) end
  end)
  if held and x >= 0 and y >= 0 and y < 260 then
    dots[#dots + 1] = { x = x, y = y, c = color }
  end
  color = ui.color(color)
end
```

## The keyboard: moving with keys

```lua
-- file: mover.lua
app = { title = "Mover", width = 320, height = 280 }

local x, y = 150, 100
local moves = { left = { -10, 0 }, right = { 10, 0 }, up = { 0, -10 }, down = { 0, 10 } }

function app.key(k)
  local m = moves[k]
  if m then
    x, y = x + m[1], y + m[2]
    return true
  end
end

function app.ui(ui)
  ui.label("Use the arrow keys")
  ui.canvas(200, function(g)
    g.clear(0x101320)
    g.circle(x, y, 12, 0x4ADE80)
  end)
end
```

## The network: fetching a page

```lua
-- file: status.lua
app = { title = "Status", width = 420, height = 200 }

local s = { url = "http://example.com/", result = "", busy = false }

function app.ui(ui)
  s.url = ui.input("url", s.url)
  if ui.button(s.busy and "Loading..." or "Check") and not s.busy then
    s.busy = true
    net.fetch(s.url, function(body, status)
      s.busy = false
      s.result = body and ("HTTP " .. status .. ", " .. #body .. " bytes") or status
    end)
  end
  ui.label(s.result)
end
```

## Settings: saving and reading

```lua
-- file: prefs.lua
app = { title = "Prefs", width = 340, height = 200 }

local FILE = "prefs.cfg"
local s = { volume = 40, dark = true }

for key, value in (fs.read(FILE) or ""):gmatch("(%w+)=([^\n]*)") do
  if key == "volume" then s.volume = tonumber(value) or 40 end
  if key == "dark" then s.dark = value == "true" end
end

function app.ui(ui)
  s.volume = ui.slider("volume", s.volume, 0, 100)
  s.dark = ui.checkbox("Dark mode", s.dark)
  if ui.button("Save") then
    fs.write(FILE, "volume=" .. s.volume .. "\ndark=" .. tostring(s.dark) .. "\n")
    sys.notify("Saved", "Prefs")
  end
end
```
