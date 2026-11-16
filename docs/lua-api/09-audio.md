# audio — playing files

The system decodes **MP3, OGG Vorbis and WAV** (PCM 8/16/24/32-bit and float) itself and
mixes them with the other sounds. The program only controls playback: open a file, play,
pause, seek, volume. The sound goes to the chosen device (Settings → Sound):
the speakers or Bluetooth headphones.

Players belong to the program: when its window closes, the sound stops at once. Also,
if `audio.status` has not been called for 10 seconds, the system considers the program gone
and closes the player itself. For a window, an `app.tick` that calls `audio.status` is enough.

The media keys (Play/Pause, Next, Previous, Stop) and the buttons of Bluetooth headphones
control the last player that was told to `play`. "Next" puts it into the `"ended"` state,
"Previous" into `"back"` (or to the start of the track if more than three seconds have played):
the program decides which track to play.

### audio.open(path) → id | nil, err

Open a file. The format is detected by the contents. It does not start playing.

### audio.play(id) / audio.pause(id) / audio.stop(id)

Play, pause, stop (stop returns to the start). After the end of the file `play` starts
from the beginning.

### audio.seek(id, ms)

Seek to `ms` milliseconds from the start.

### audio.volume(id, 0..100)

The volume of this player (on top of the system volume).

### audio.status(id) → table | nil, err

`{ state = "playing" | "paused" | "stopped" | "ended" | "back", pos = ms, len = ms }`.
`len` is 0 if the length is unknown.

### audio.info(id) → table | nil, err

`{ format = "MP3" | "OGG Vorbis" | "WAV", title = …, artist = …, album = …, rate = Hz, channels = 1 | 2 }`.
The title and artist come from the tags (ID3v2 for MP3, Vorbis comments for OGG) and are empty
if there are no tags.

### audio.cover(id) → data | nil

The cover art from the tags (the APIC frame for MP3, METADATA_BLOCK_PICTURE for OGG): PNG or JPEG bytes
for `ui.image`. `nil` if there is no cover.

### audio.balance(id, -100..100)

The balance: −100 — the left channel only, 100 — the right only, 0 — both.

### audio.spectrum(id [, bands]) → { levels } | nil

The spectrum of what is playing now: `bands` bands (16 by default, up to 64) from 40 Hz to 16 kHz on a logarithmic scale, levels 0–100. The output latency is taken into account, so the bars match the sound even in Bluetooth headphones. Zeros while paused.

### audio.probe(path) → table | nil, err

File information without playing: `{ len = ms, title, artist, album }`. Reads only the header (and the end of the file for OGG), so it is fast: this is how the player learns the durations of a whole playlist. For an MP3 without a Xing/VBRI header the length is estimated from the bitrate.

### audio.close(id)

Close the player.

```lua
local id = assert(audio.open("/home/user/Music/song.mp3"))
audio.play(id)

function app.tick()
  local st = audio.status(id)
  return st and st.state == "playing"
end
```

The complete example is `/apps/player.lua`.
