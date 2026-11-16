# GlassOS Lua API: getting started

Programs for GlassOS are written in Lua 5.4. There is nothing to compile: a `.lua` file is the program. This reference describes API version **0.2** (`sys.version()`).

## Contents

- [Program model](01-program-model.md) — how a program starts, lives and redraws
- [ui — widgets](02-ui.md) — buttons, fields, lists, tabs
- [gfx — drawing](03-gfx.md) — your own graphics on a canvas
- [fs — files](04-fs.md)
- [sys — system](05-sys.md) — time, timers, notifications, dialogs, sound
- [net — network](06-net.md)
- [audio — playing files](09-audio.md)
- [Examples](07-examples.md) — complete programs
- [Limitations](08-limitations.md) — what is missing and what to keep in mind

## The first program

A console program just prints its result:

```lua
-- file: hello.lua
local name = arg[1] or "world"
print("Hello, " .. name .. "!")
sys.notify("Hello, " .. name .. "!", "hello.lua")
```

Save the file in your home folder and type `hello Alice` in the terminal. The `.lua` extension may be omitted.

A windowed program describes an `app` table and an `app.ui` function:

```lua
-- file: counter.lua
app = { title = "Counter", width = 300, height = 180 }

local count = 0

function app.ui(ui)
  ui.heading("Clicks: " .. count)
  if ui.button("Add one") then
    count = count + 1
  end
end
```

Note that the button gets no handler. `ui.button` draws the button and returns `true` right away if it was clicked. The whole interface is ordinary code that runs again on every redraw (an immediate-mode UI). More in [Program model](01-program-model.md).

## How to run it

| From | How |
|---|---|
| Terminal | The program name: looked up in the current folder, then in `/apps` |
| Terminal, in the background | `name &`; list with `jobs`, stop with `kill %N` |
| Files | Double-click the `.lua` file |
| The program menu | It shows all programs from `/apps` |

Command line arguments are in the `arg` table: `arg[0]` is the program path, `arg[1]` and on are the arguments.

To try code quickly, type `lua` in the terminal: an interactive mode opens. Leave it with `exit` or Ctrl+D.

## What Lua has here

Available: the basic functions, `coroutine`, `table`, `string`, `math`, `utf8`, and `load`.

Missing: `os`, `io`, `debug`, `dofile`, `loadfile`. `require` works, see [Program model](01-program-model.md#modules). Instead of `io` use [fs](04-fs.md), instead of `os.time` use [sys](05-sys.md).
