# gfx — drawing

Drawing is only possible inside the function passed to `ui.canvas` or `ui.fill`. Outside it the `gfx` functions do nothing.

`ui.fill(fn)` is a canvas over the whole window without margins: the program draws its whole interface itself (this is how the player is made). The function gets `fn(g, clicked, x, y, held, wheel)`.

```lua
local clicked, x, y, held = ui.canvas(200, function(g)
  g.clear(0x101320)
  g.rect(10, 10, 100, 60, 0x38BDF8, 8)
  g.circle(170, 40, 30, 0xF87171)
  g.line(0, 0, g.w, g.h, 0xFFFFFF)
  g.text(10, 90, "Hello", 0xFFFFFF, 18)
end)
```

The function gets `g` — the `gfx` table with the extra fields `g.w` and `g.h`: the canvas size. The canvas width equals the available window width. Coordinates are counted from the top-left corner of the canvas, everything outside is clipped.

`ui.canvas` returns:

| Value | Meaning |
|---|---|
| `clicked` | There was a click on the canvas in this frame |
| `x`, `y` | The mouse position relative to the canvas; may be outside it |
| `held` | The left mouse button is held |
| `wheel` | The mouse wheel over the canvas in this frame (lines, down is positive) |

The mouse position is updated on redraw: that happens on a click and while dragging. Plain mouse movement with no button held does not cause a redraw.

Colours are numbers `0xRRGGBB`. Where there is `alpha`, it is the opacity 0–255 (255 by default).

Only what changed since the previous frame is recomposed: an animation in a corner of the window does not make the system rebuild the whole window.

## Functions

### gfx.clear(color)

Fill the whole canvas.

### gfx.rect(x, y, w, h, color [, radius [, alpha]])

A rectangle; `radius` rounds the corners.

### gfx.stroke(x, y, w, h, color [, radius [, alpha]])

A one-pixel outline.

### gfx.gradient(x, y, w, h, top, bottom [, radius [, alpha]])

A rectangle with a vertical transition from colour `top` to `bottom`. With `radius` equal to half the side it is a circle: this is how three-dimensional buttons are drawn.

### gfx.circle(cx, cy, radius, color [, alpha])

A filled circle.

### gfx.image(data, x, y, w, h [, "cover"])

A picture from file bytes (PNG, JPEG, GIF, BMP — for example, `audio.cover(id)` or `fs.read(path)`). Without `"cover"` it is fitted entirely with background-coloured margins, with `"cover"` it fills the rectangle, cropping the edges. The decoded picture is remembered, it can be passed every frame.

### gfx.clip(x, y, w, h) / gfx.clip()

Draw only inside a rectangle (for example, a scrolling list); without arguments — the whole canvas again.

### gfx.line(x0, y0, x1, y1, color)

A one-pixel line.

### gfx.text(x, y, text [, color [, size [, font]]]) → width

Text, size 6 to 96, 14 by default. `font` is `"regular"`, `"bold"` or `"mono"`; when not given, the font is semibold from size 20. Returns the width of the text in pixels.

### gfx.measure(text [, size [, font]]) → width

The text width without drawing: for right alignment or truncating long text.

## Animation

The canvas is redrawn together with the window. For animation, change the state from a timer: a timer firing causes a redraw.

```lua
local x = 0
sys.every(33, function() x = (x + 2) % 300 end)
```

Drawing runs on the CPU. Hundreds of primitives per frame are fine, but filling a large canvas pixel by pixel will be slow.
