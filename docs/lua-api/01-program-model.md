# Program model

## Two kinds of programs

A **console** program does not define `app.ui`: the script runs from top to bottom and ends. `print` goes to the terminal. If the program was started from Files, its output opens in a new terminal window.

A **windowed** program defines a global `app` table with an `app.ui` function. After the script has run, the system opens a window and from then on calls the program's functions.

## The app table

| Field | Meaning | Default | Range |
|---|---|---|---|
| `title` | The window title | The file name | |
| `width` | The width of the window area | 480 | 200–1200 |
| `height` | The height of the window area | 400 | 120–760 |
| `background` | The background colour, `0xRRGGBB` | `0x1C1F2E` | |
| `min_width`, `min_height` | The window cannot be made smaller | 260×150 with the frame | 0–1200, 0–760 |
| `category` | The category in the program menu (the gear button) | `Other` | |

For a console program the category is given by a comment at the start of the file: `-- category: Examples`.

Values out of range are clamped. The window is remembered: after a reboot the system starts the program again.

## Functions the system calls

### app.ui(ui)

Describes the window contents; mandatory. Called on every redraw. The `ui` argument is the same table as the global `ui`.

A redraw happens after a click, after a key press in a text field, when a timer fired, when a network or dialog answer arrived, and when `app.key` or `app.tick` returned `true`. If a widget changed its value, the frame is repeated right away so that all the code sees the new state. That is why **`app.ui` must not do anything long and must not change state except in response to widget results**.

### app.key(name)

A key was pressed while no text field has focus. Return `true` to redraw the window.

Key names: `"up"`, `"down"`, `"left"`, `"right"`, `"home"`, `"end"`, `"delete"`, `"enter"`, `"space"`, `"tab"`, `"backspace"`, `"escape"`, characters as they are (`"a"`, `"A"`, `"7"`), `"ctrl+a"` … `"ctrl+z"`. Other keys arrive as `"unknown"`, including Shift+arrows.

Keys the system takes (volume, brightness, Print Screen, Alt+Tab) never reach the program.

### app.open(paths)

The program is given files: the user dropped them on the window (dragged them from Files) or opened a file while this program's window was already open. This is how the player gets a new song without starting a second time. `paths` is a list of full paths, which may include folders. The program may read these files. The window is redrawn afterwards.

Without `app.open` the window does not accept files.

### app.tick(ui)

Called about 20 times a second; more or less often with the `app.tick_every` field (milliseconds, at least 10). Return `true` to redraw the window. Drawing here is not possible: widgets only work inside `app.ui`. For scheduled actions timers are more convenient: `sys.every`.

If only one place of the window changed (a meter, a spectrum), return its rectangle `{x, y, w, h}` in `ui.fill` coordinates. The frame is then partial: in `app.ui` the field `ui.area` equals this rectangle, and only it needs to be drawn (the background too: before the frame it is filled with the window colour); everything outside it is clipped, and the rest of the window stays from the previous frame. A partial frame gets no clicks or keys, they come in the next full one. The system may make a full frame instead of a partial one (the window changed size, a list is open); then `ui.area` is `nil` and everything must be drawn.

```lua
app.tick_every = 33          -- ~30 times a second
function app.tick()
  update_meter()
  return { 20, 40, 200, 60 } -- redraw only the meter
end
function app.ui(ui)
  ui.fill(function(g)
    if ui.area then draw_meter(g) return end
    draw_everything(g)
  end)
end
```

## Limits

| What | Limit | What happens |
|---|---|---|
| Memory of one program | 32 MiB | The Lua error `not enough memory` |

If memory runs out, the program gets the Lua error `not enough memory` and may catch it with `pcall`. The system does not suffer.

There is no time limit: every program runs in its own process. A program stuck in a loop does not disturb the desktop or other windows, only its own window stops updating. Close it with the cross or end the program in the Task Manager. A console program is stopped with Ctrl+C.

A Lua error is shown in the program's window, the rest of the system is not affected.

## Errors

An unhandled error in a window stops the program: the error text appears instead of the contents, and the system shows a "Program error" notification. It can only be restarted by closing the window.

API functions do not throw errors. On failure they return `nil` and a message:

```lua
local text, err = fs.read("settings.txt")
if not text then
  print("cannot read: " .. err)
end
```

`print` from a window (after the start) goes to the debug port, not to the screen. For debugging use `ui.label` or `sys.notify`.

## Scrolling

If the contents are taller than the window, they scroll with the mouse wheel automatically.

## Modules

`require("name")` loads `name.lua` and returns what the module returned. Dots in the name mean folders: `require("ui.theme")` looks for `ui/theme.lua`. The file runs once, after that the saved result from `package.loaded` is returned.

A module is looked for in this order:

1. in the program's own folder;
2. in `/apps/lib`;
3. in the `lib` folder of the user's home folder.

The list of items 2 and 3 is kept in `package.path` and can be changed. The program's folder is always added.

```lua
-- lib/greet.lua in the home folder
local M = {}
function M.hello(name) return "Hello, " .. name end
return M
```

```lua
local greet = require("greet")
print(greet.hello("GlassOS"))
```

## Services

A service is a program without a window that the system starts by itself and keeps in the background. Services are added in Settings → Services.

- A **system service** is added by an administrator. It runs from boot, even while nobody is logged in.
- A **user service** can be added by anyone. It runs while that user is logged in.

A service runs with the rights of whoever added it: their home folder and the program's own folder. `print` writes to the service log, its last lines are visible in Settings. If a service ended or crashed, the system starts it again after 2 s, then after 4, 8 and so on, but at least once a minute. This can be turned off.

A service may not have a window: if it has `app.ui`, it ends with an error. Scheduled work is done with timers or `sys.sleep`:

```lua
-- Once a minute, appends the memory usage to a file
sys.every(60000, function()
  local t, used = sys.time(), sys.mem()
  fs.append("memory.log", string.format("%02d:%02d %d KiB used\n", t.hour, t.min, used))
end)
```
