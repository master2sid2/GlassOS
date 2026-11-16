# sys — system

## Time

### sys.time() → table

The current local date and time. The time zone and daylight saving time are set in Settings → Date & Time.

| Field | What it is |
|---|---|
| `year`, `month`, `day` | the date, the month from 1 |
| `hour`, `min`, `sec` | the time on the clock |
| `wday` | the weekday, 1 — Sunday, 7 — Saturday |
| `utc` | UTC seconds since 1970-01-01, handy for comparing moments |
| `offset` | the offset of local time from UTC in minutes, including summer time |
| `summer` | `true` if summer time is in effect |
| `zone` | the zone name, for example `Europe/Berlin` or `UTC+02:00` |

```lua
local t = sys.time()
print(string.format("%02d:%02d, %s", t.hour, t.min, t.zone))
```

### sys.ticks() → ms

Milliseconds since boot. Suitable for measuring intervals.

### sys.sleep(ms)

Wait `ms` milliseconds without using the CPU. Stopping the program (Ctrl+C, the Stop button of a service) interrupts the wait.

## Timers

Timers work in windowed programs and without a window. A program without a window that still has timers does not end after its script has run: it waits for them until all are cancelled. This is how services work.

### sys.after(ms, fn) → timer

Call `fn` once after `ms` milliseconds.

### sys.every(ms, fn) → timer

Call `fn` every `ms` milliseconds; the minimum period is 10 ms. The first call comes after `ms`.

### sys.cancel(timer)

Cancel a timer.

```lua
local t = sys.every(1000, function() seconds = seconds + 1 end)
-- later
sys.cancel(t)
```

A timer firing redraws the window.

## Notifications and sound

### sys.notify(text [, title])

A popup notification; it also goes to the notification centre.

### sys.beep([hz [, ms]])

A short tone; 880 Hz, 120 ms by default. Range: 20–20000 Hz, up to 10 s.

### sys.play(path) → true | nil, err

Play a WAV file. The function returns right away, the sound plays in the background. For MP3/OGG and playback control see [audio](09-audio.md).

## Dialogs

Dialogs do not stop the program: the answer comes to a function.

### sys.open_file(fn [, dir])

The "Open" dialog. `fn(path)` gets the chosen path or `nil` on cancel.

### sys.open_files(fn [, dir])

The "Open" dialog with several files: Ctrl+click marks or unmarks a file, Shift+click marks a range, Ctrl+A marks all files of the folder. `fn(paths)` gets a list of paths (even if one file was chosen) or `nil` on cancel.

### sys.save_file(fn [, name [, dir]])

The "Save as" dialog. `name` is the suggested name.

```lua
if ui.button("Open") then
  sys.open_file(function(path)
    if path then state.text = fs.read(path) or "" end
  end)
end
```

## Launching

### sys.open(path) → true | nil, err

Open the same way as a double-click in Files: a program is started, a folder is shown in Files, a file is opened in Notepad.

## System information

### sys.user() → name, home, admin

The name of the logged-in user, their home folder and `true` if they are an administrator.

```lua
local name, home = sys.user()
print("Hello, " .. name .. ", your files are in " .. home)
```

### sys.version() → string

The API version, currently `"0.2"`.

### sys.mem() → used, free

Used and free in the kernel heap, in KiB. This is the memory of **the whole system**, not of the program.

### sys.size(bytes) → string

A size in convenient units: `sys.size(1536)` → `"1.5 KB"`, `sys.size(5 * 2^30)` → `"5.0 GB"`.
