-- Web: a tiny text-mode browser. Shows how to use net.fetch from a window:
-- the request runs in the background and the answer arrives in a function.
-- Only http:// works (no TLS yet). Links are numbered: type the number to follow.

app = { title = "Web", category = "Internet", width = 680, height = 560 }

local GRAY = 0x8A90A8
local WIDTH = 78
local s = { url = "http://example.com", status = "Type an address and press Go.", lines = {}, links = {}, busy = false }

local entities = { amp = "&", lt = "<", gt = ">", quot = '"', apos = "'", nbsp = " ", copy = "(c)", mdash = "--", ndash = "-" }

-- HTML -> plain text lines; <a href> become [n] markers.
local function render(html, base)
  local links = {}
  html = html:gsub("<[sS][cC][rR][iI][pP][tT].-</[sS][cC][rR][iI][pP][tT]>", "")
             :gsub("<[sS][tT][yY][lL][eE].-</[sS][tT][yY][lL][eE]>", "")
             :gsub("<!%-%-.-%-%->", "")
             :gsub("<[hH][eE][aA][dD]>.-</[hH][eE][aA][dD]>", "")
  html = html:gsub("<[aA]%s[^>]-[hH][rR][eE][fF]%s*=%s*[\"']?([^\"'%s>]+)[\"']?[^>]*>", function(href)
    if href:sub(1, 1) == "/" then href = base:match("^(https?://[^/]+)") .. href end
    links[#links + 1] = href
    return "[" .. #links .. "]"
  end)
  html = html:gsub("<[bB][rR]%s*/?>", "\n")
             :gsub("</?[pP][^>]*>", "\n\n")
             :gsub("</?[hH][1-6][^>]*>", "\n\n")
             :gsub("</?[lL][iI][^>]*>", "\n * ")
             :gsub("</?[dD][iI][vV][^>]*>", "\n")
             :gsub("<[^>]*>", "")
             :gsub("&(%a+);", function(e) return entities[e] or ("&" .. e .. ";") end)
             :gsub("&#(%d+);", function(n) n = tonumber(n) return n < 128 and string.char(n) or "?" end)
  local lines, blank = {}, true
  for raw in (html .. "\n"):gmatch("([^\n]*)\n") do
    local line = raw:gsub("%s+", " "):gsub("^ ", "")
    if line == "" then
      if not blank then lines[#lines + 1] = "" end
      blank = true
    else
      blank = false
      while #line > WIDTH do
        local cut = line:sub(1, WIDTH):match("^.*() ") or WIDTH
        lines[#lines + 1] = line:sub(1, cut - 1)
        line = line:sub(cut + 1)
      end
      lines[#lines + 1] = line
    end
  end
  return lines, links
end

local function go(url)
  if not url:match("^%a+://") then url = "http://" .. url end
  s.url, s.busy, s.status = url, true, "Loading " .. url .. " ..."
  net.fetch(url, function(body, status)
    s.busy = false
    if not body then
      s.status, s.lines, s.links = "Error: " .. tostring(status), {}, {}
      return
    end
    s.status = string.format("HTTP %d, %d bytes", status, #body)
    local text = body:find("<") and body or nil
    if text then
      s.lines, s.links = render(body, url)
    else
      s.lines, s.links = {}, {}
      for line in (body .. "\n"):gmatch("([^\n]*)\n") do s.lines[#s.lines + 1] = line end
    end
  end)
end

local follow = ""

function app.ui(ui)
  if ui.button(s.busy and "..." or "Go") and not s.busy then go(s.url) end
  ui.sameline()
  s.url = ui.input("url", s.url, "http://...")
  ui.label(s.status, GRAY)
  if #s.links > 0 then
    if ui.button("Open link") then
      local href = s.links[tonumber(follow) or 0]
      if href then follow = "" go(href) end
    end
    ui.sameline()
    follow = ui.input("link", follow, "link number, then Enter")
  end
  ui.separator()
  for _, line in ipairs(s.lines) do
    ui.label(line == "" and " " or line)
  end
end

function app.key(k)
  if k == "enter" and not s.busy then
    local href = s.links[tonumber(follow) or 0]
    if href then follow = "" go(href) else go(s.url) end
    return true
  end
end
