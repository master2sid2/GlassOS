-- Widgets Demo: what a Lua app in GlassOS can do.
-- The window is described by app.ui(), which runs on every redraw:
-- each widget draws itself and returns its new value.

app = { title = "Widgets Demo", category = "Examples", width = 560, height = 640 }

local CONFIG = "widgets.cfg"
local LABEL_W = 110
local GRAY = 0x8A90A8

local s = {
  name = "", email = "",
  newsletter = true, compact = false,
  size = "Medium", fruit = "Banana",
  color = 0x38BDF8, volume = 40,
  clicks = 0,
  status = "Fill in the form and press Save.",
}

-- Settings live in a plain text file: key=value per line.
local function load()
  local text = fs.read(CONFIG)
  if not text then return end
  for key, value in text:gmatch("(%w+)=([^\n]*)") do
    if type(s[key]) == "number" then s[key] = tonumber(value) or s[key]
    elseif type(s[key]) == "boolean" then s[key] = value == "true"
    elseif s[key] ~= nil then s[key] = value end
  end
  s.status = "Loaded settings from " .. CONFIG
end

local function save()
  local keys = { "name", "email", "newsletter", "compact", "size", "fruit", "color", "volume" }
  local lines = {}
  for _, key in ipairs(keys) do lines[#lines + 1] = key .. "=" .. tostring(s[key]) end
  local ok, err = fs.write(CONFIG, table.concat(lines, "\n") .. "\n")
  local t = sys.time()
  s.status = ok and string.format("Saved at %02d:%02d:%02d to %s", t.hour, t.min, t.sec, CONFIG)
             or ("Save failed: " .. err)
end

load()

function app.ui(ui)
  ui.heading("Profile")
  ui.label("Name", GRAY, LABEL_W); ui.sameline()
  s.name = ui.input("name", s.name, "Your name")
  ui.label("Email", GRAY, LABEL_W); ui.sameline()
  s.email = ui.input("email", s.email, "you@example.com")

  ui.separator()
  ui.heading("Preferences")
  s.newsletter = ui.checkbox("Newsletter", s.newsletter); ui.sameline()
  s.compact = ui.checkbox("Compact mode", s.compact)
  ui.label("Size", GRAY, LABEL_W); ui.sameline()
  s.size = ui.radio({ "Small", "Medium", "Large" }, s.size)
  ui.label("Fruit", GRAY, LABEL_W); ui.sameline()
  s.fruit = ui.dropdown("fruit", { "Apple", "Banana", "Cherry", "Mango", "Kiwi" }, s.fruit)
  ui.label("Color", GRAY, LABEL_W); ui.sameline()
  s.color = ui.color(s.color)
  ui.label("Volume", GRAY, LABEL_W); ui.sameline()
  s.volume = ui.slider("volume", s.volume, 0, 100)
  ui.progress(s.volume, 100)

  ui.separator()
  local who = s.name ~= "" and s.name or "stranger"
  ui.panel(string.format("Hello, %s! %s %s, volume %d%%", who, s.size, s.fruit:lower(), s.volume), s.color)

  if ui.button("Save") then save() end
  ui.sameline()
  if ui.button("Click me") then s.clicks = s.clicks + 1 end
  ui.sameline()
  if ui.button("Open file...") then
    -- The dialog does not block the program: the function is called when a file is chosen.
    sys.open_file(function(path)
      if path then
        local data = fs.read(path) or ""
        s.status = string.format("%s: %d bytes", path, #data)
      else
        s.status = "Open cancelled"
      end
    end)
  end
  ui.sameline()
  ui.label("Clicks: " .. s.clicks, GRAY)
  ui.label(s.status, GRAY)
end
