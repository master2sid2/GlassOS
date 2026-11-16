-- Snake: a small game that shows ui.canvas, sys.every timers and app.key.
-- Arrows (or WASD) to move, Space to pause, Enter to restart.

app = { title = "Snake", category = "Games", width = 460, height = 560 }

local N, CELL = 20, 20
local BEST_FILE = ".snake_best"
local snake, dir, next_dir, food, score, over, paused
local best = tonumber(fs.read(BEST_FILE) or "") or 0

local function free_cell()
  while true do
    local c = { x = math.random(0, N - 1), y = math.random(0, N - 1) }
    local taken = false
    for _, s in ipairs(snake) do
      if s.x == c.x and s.y == c.y then taken = true break end
    end
    if not taken then return c end
  end
end

local function reset()
  snake = { { x = 10, y = 10 }, { x = 9, y = 10 }, { x = 8, y = 10 } }
  dir, next_dir = { x = 1, y = 0 }, { x = 1, y = 0 }
  score, over, paused = 0, false, false
  food = free_cell()
end

local function game_over()
  over = true
  sys.beep(196, 350)
  if score > best then
    best = score
    fs.write(BEST_FILE, tostring(best))
    sys.notify("New record: " .. best, "Snake")
  end
end

local function step()
  if over or paused then return end
  dir = next_dir
  local head = { x = snake[1].x + dir.x, y = snake[1].y + dir.y }
  if head.x < 0 or head.y < 0 or head.x >= N or head.y >= N then return game_over() end
  for i = 1, #snake - 1 do
    if snake[i].x == head.x and snake[i].y == head.y then return game_over() end
  end
  table.insert(snake, 1, head)
  if head.x == food.x and head.y == food.y then
    score = score + 1
    sys.beep(988, 50)
    food = free_cell()
  else
    table.remove(snake)
  end
end

local DIRS = {
  up = { 0, -1 }, w = { 0, -1 }, down = { 0, 1 }, s = { 0, 1 },
  left = { -1, 0 }, a = { -1, 0 }, right = { 1, 0 }, d = { 1, 0 },
}

function app.key(k)
  local d = DIRS[k]
  if d and (d[1] ~= -dir.x or d[2] ~= -dir.y) then
    next_dir = { x = d[1], y = d[2] }
  elseif k == "space" then
    paused = not paused
  elseif k == "enter" and over then
    reset()
  end
  return true
end

function app.ui(ui)
  ui.heading("Snake")
  ui.sameline()
  ui.label(string.format("Score: %d    Best: %d", score, best), 0x8A90A8)
  ui.canvas(N * CELL, function(g)
    local ox = (g.w - N * CELL) // 2
    g.rect(ox, 0, N * CELL, N * CELL, 0x0E1018, 8)
    g.rect(ox + food.x * CELL + 4, food.y * CELL + 4, CELL - 8, CELL - 8, 0xF87171, 6)
    for i, s in ipairs(snake) do
      local color = i == 1 and 0x7DD3FC or 0x38BDF8
      g.rect(ox + s.x * CELL + 1, s.y * CELL + 1, CELL - 2, CELL - 2, color, 5)
    end
    if over then
      g.text(ox + 118, 160, "Game over", 0xFFFFFF, 30)
      g.text(ox + 116, 210, "Press Enter to play again", 0x8A90A8, 14)
    elseif paused then
      g.text(ox + 150, 180, "Paused", 0xFFFFFF, 30)
    end
  end)
  ui.label("Arrows or WASD to move, Space to pause, Enter to restart", 0x8A90A8)
end

reset()
sys.every(120, step)
