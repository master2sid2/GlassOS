//! Sound: a software mixer and card drivers — AC97 (old Intel ICH, QEMU)
//! and Intel HDA (all modern computers).
//!
//! The card fetches sound from memory itself (DMA) over a ring of 32 buffers. The
//! "audio" thread tracks which buffer is playing now and fills the next ones ahead of time
//! with the mix of all sounding voices: tones (`beep`) and WAV recordings (`play_wav`).
//! When nothing sounds, the card stops — no CPU is spent.
//!
//! The second output is a stream (`open_stream`): another driver takes the sound at its own pace,
//! for example Bluetooth headphones. While a stream is open, sound goes to it instead of the card;
//! the mixer works without a card too (on a computer without sound, headphones are the only output).

mod ac97;
mod hda;
pub mod player;

/// Measurements for finding stutters: how much silence was substituted and who did not get the
/// CPU for long. Once every two seconds, if something bad happened, a line goes to the log.
pub mod diag {
    use crate::interrupts::{self, HZ};
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Frames of silence substituted into the stream (the mixer was late).
    pub static PADDED: AtomicU64 = AtomicU64::new(0);
    /// Frames the player was missing (the decoder was late).
    pub static STARVED: AtomicU64 = AtomicU64::new(0);
    /// The longest pauses between passes (ticks): mixer, Bluetooth.
    pub static GAP_MIX: AtomicU64 = AtomicU64::new(0);
    pub static GAP_BT: AtomicU64 = AtomicU64::new(0);
    /// Cycles spent decoding files and encoding SBC (headphones).
    pub static DECODE: AtomicU64 = AtomicU64::new(0);
    pub static ENCODE: AtomicU64 = AtomicU64::new(0);
    static REPORTED: AtomicU64 = AtomicU64::new(0);

    pub fn gap(which: &AtomicU64, last: &mut u64) {
        let now = interrupts::ticks();
        if *last != 0 {
            which.fetch_max(now - *last, Ordering::Relaxed);
        }
        *last = now;
    }

    pub fn report() {
        let now = interrupts::ticks();
        if now < REPORTED.load(Ordering::Relaxed) + 2 * HZ {
            return;
        }
        REPORTED.store(now, Ordering::Relaxed);
        let ms = |frames: u64| frames * 1000 / super::RATE as u64;
        let tick_ms = 1000 / HZ;
        let (padded, starved) = (PADDED.swap(0, Ordering::Relaxed), STARVED.swap(0, Ordering::Relaxed));
        let (mix, bt) = (GAP_MIX.swap(0, Ordering::Relaxed), GAP_BT.swap(0, Ordering::Relaxed));
        let (decode, encode) = (DECODE.swap(0, Ordering::Relaxed), ENCODE.swap(0, Ordering::Relaxed));
        if padded > 0 || starved > 0 || decode + encode > 0 {
            crate::kprintln!(
                "audio: silence filled {} ms, player short {} ms; longest wait: mixer {} ms, bluetooth {} ms; 2 s of CPU: decoding {} M, SBC {} M",
                ms(padded),
                ms(starved),
                mix * tick_ms,
                bt * tick_ms,
                decode / 1_000_000,
                encode / 1_000_000
            );
        }
    }
}

/// Whether this is a sound file, judging by the name (what the player can handle).
pub fn is_audio_file(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "mp3" | "ogg" | "oga" | "wav")
}
pub mod sbc;
pub mod track;

use crate::interrupts;
use crate::kprintln;
use crate::pci;
use crate::sync::IrqLock;
use crate::task;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

pub const RATE: u32 = 48_000;
/// Frames (left/right pairs) in one buffer: ~21 ms.
const FRAMES: usize = 1024;
const BUFFERS: usize = 32;
/// How many buffers to keep filled ahead of playback (~128 ms).
const AHEAD: usize = 6;

/// Bytes in one buffer (16-bit, stereo).
const BUFFER_BYTES: usize = FRAMES * 4;

/// A sound buffer for DMA (HDA requires an address that is a multiple of 128).
#[repr(C, align(128))]
struct Buffer([i16; FRAMES * 2]);

/// What the mixer needs from a sound card: play a ring of `BUFFERS` buffers.
trait Hw: Send {
    /// Start playing the ring from the beginning; buffers `0..=last` are already filled.
    fn start(&mut self, buffers: &[u64], last: usize);
    fn stop(&mut self);
    /// Which buffer is playing now.
    fn position(&self) -> usize;
    /// Buffer `i` is filled — playback may go up to and including it.
    fn queued(&mut self, i: usize);
    /// The card played to the end of the filled part and stopped.
    fn underrun(&mut self) -> bool;
    /// Check the headphone jack (twice a second): Some(plugged in) — if
    /// it changed. None — no change or no sensor.
    fn jack(&mut self) -> Option<bool> {
        None
    }
    /// Once a tick: follow the microphone jack and, while `recording`, move the recorded
    /// samples to the system's input.
    fn capture(&mut self, _recording: bool) {}
}

/// Where sound goes: 0 — unknown (no sensor), 1 — speakers, 2 — headphones.
static OUTPUT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Where sound goes: a stream's name (Bluetooth headphones) or "Speakers" / "Headphones" —
/// if the card can detect it.
pub fn output_name() -> Option<String> {
    if let Some(s) = active_stream() {
        return Some(s.lock().name.clone());
    }
    match OUTPUT.load(core::sync::atomic::Ordering::Relaxed) {
        1 => Some("Speakers".into()),
        2 => Some("Headphones".into()),
        _ => None,
    }
}

/// A sound stream for another driver: 16-bit stereo, `RATE` Hz. The mixer keeps it
/// filled `STREAM_AHEAD` frames ahead, the driver takes it at its own pace.
pub struct Stream {
    pub name: String,
    ring: alloc::collections::VecDeque<i16>,
    /// The device sets the volume itself (Bluetooth absolute volume): the mixer does not
    /// attenuate the sound, the system slider moves the headphones' volume.
    pub absolute: bool,
}

pub type StreamRef = Arc<IrqLock<Stream>>;

impl Stream {
    /// Take `frames` frames; if the mixer was late, the rest is padded with silence.
    pub fn take(&mut self, frames: usize, out: &mut Vec<i16>) {
        let n = (frames * 2).min(self.ring.len());
        out.extend(self.ring.drain(..n));
        out.resize(out.len() + frames * 2 - n, 0);
        if n < frames * 2 {
            diag::PADDED.fetch_add(((frames * 2 - n) / 2) as u64, Ordering::Relaxed);
        }
    }
}

/// Sound input (microphone): the device puts samples (mono, its own rate), recording takes them.
pub struct Input {
    pub name: String,
    pub rate: u32,
    ring: alloc::collections::VecDeque<i16>,
}

pub type InputRef = Arc<IrqLock<Input>>;

impl Input {
    /// Samples from the device (with input gain). If recording does not take them, old ones
    /// are dropped (~2 s are kept).
    pub fn push(&mut self, samples: &[i16]) {
        let gain = INPUT_GAIN.load(core::sync::atomic::Ordering::Relaxed) as i32;
        self.ring.extend(samples.iter().map(|&s| (s as i32 * gain / 256).clamp(-32768, 32767) as i16));
        let keep = self.rate as usize * 2;
        if self.ring.len() > keep {
            let extra = self.ring.len() - keep;
            self.ring.drain(..extra);
        }
    }
}

/// Input gain in 1/256 (×4 = +12 dB by default: headset microphones are quiet).
static INPUT_GAIN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1024);

/// Microphone volume (0..=100 %) and gain (dB, 0..=24) — from the sound settings.
pub fn set_input_level(volume: u32, boost_db: u32) {
    let g = 256.0 * volume.min(100) as f32 / 100.0 * libm::powf(10.0, boost_db.min(24) as f32 / 20.0);
    INPUT_GAIN.store(g as u32, core::sync::atomic::Ordering::Relaxed);
}

/// The "sound card" output in the device list.
pub const CARD_OUTPUT: &str = "Built-in sound";

/// The user's choice: where to output and where to record from (by device name). If nothing is
/// chosen or it is not present now — the last connected one (headphones), otherwise the card.
static OUT_CHOICE: IrqLock<Option<String>> = IrqLock::new(None);
static IN_CHOICE: IrqLock<Option<String>> = IrqLock::new(None);
static CHOICES_LOADED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
const CHOICES_FILE: &str = "/system/audio.cfg";

fn load_choices() {
    if CHOICES_LOADED.load(core::sync::atomic::Ordering::Relaxed) || !crate::fs::ready() {
        return;
    }
    CHOICES_LOADED.store(true, core::sync::atomic::Ordering::Relaxed);
    let Ok(data) = task::as_system(|| crate::fs::read(CHOICES_FILE)) else { return };
    for line in String::from_utf8_lossy(&data).lines() {
        match line.split_once('=') {
            Some(("output", v)) if !v.is_empty() => *OUT_CHOICE.lock() = Some(v.into()),
            Some(("input", v)) if !v.is_empty() => *IN_CHOICE.lock() = Some(v.into()),
            _ => {}
        }
    }
}

fn save_choices() {
    let text = format!("output={}\ninput={}\n", OUT_CHOICE.lock().clone().unwrap_or_default(), IN_CHOICE.lock().clone().unwrap_or_default());
    let _ = task::as_system(|| crate::fs::write(CHOICES_FILE, text.as_bytes()));
}

/// Output devices now: the card and open streams (headphones).
pub fn outputs() -> Vec<String> {
    let mut v = Vec::new();
    if CARD.lock().as_ref().is_some_and(|c| c.hw.is_some()) {
        v.push(String::from(CARD_OUTPUT));
    }
    if let Some(s) = STREAM.lock().as_ref() {
        v.push(s.lock().name.clone());
    }
    v
}

/// There is a sound card (do not call while holding `CARD`: the lock is not reentrant).
fn has_card() -> bool {
    CARD.lock().as_ref().is_some_and(|c| c.hw.is_some())
}

/// The stream sound goes to now (if it is chosen or there is no choice).
fn active_stream() -> Option<StreamRef> {
    stream_for(has_card())
}

/// The same for the mixer, which already holds `CARD` and knows whether there is a card.
fn stream_for(has_card: bool) -> Option<StreamRef> {
    let stream = STREAM.lock().clone()?;
    let choice = OUT_CHOICE.lock().clone();
    let card_chosen = choice.as_deref() == Some(CARD_OUTPUT) && has_card;
    (!card_chosen).then_some(stream)
}

/// Where sound goes now.
pub fn current_output() -> Option<String> {
    match active_stream() {
        Some(s) => Some(s.lock().name.clone()),
        None => CARD.lock().as_ref().filter(|c| c.hw.is_some()).map(|_| String::from(CARD_OUTPUT)),
    }
}

pub fn choose_output(name: &str) {
    *OUT_CHOICE.lock() = Some(name.into());
    save_choices();
}

/// The input we record from: the chosen one if present, otherwise the last connected one
/// (a headset plugged in or paired after boot wins over the built-in microphone).
fn active_input() -> Option<InputRef> {
    load_choices();
    let list = INPUTS.lock();
    let choice = IN_CHOICE.lock().clone();
    list.iter().find(|i| Some(&i.lock().name) == choice.as_ref()).or(list.last()).cloned()
}

pub fn current_input() -> Option<String> {
    active_input().map(|i| i.lock().name.clone())
}

pub fn choose_input(name: &str) {
    *IN_CHOICE.lock() = Some(name.into());
    save_choices();
}

/// Sound inputs (microphones) and how many recordings listen to them now.
static INPUTS: IrqLock<Vec<InputRef>> = IrqLock::new(Vec::new());
static RECORDING: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// A device appeared as a sound input (a headset connected).
pub fn add_input(name: &str, rate: u32) -> InputRef {
    let i = Arc::new(IrqLock::new(Input { name: name.into(), rate, ring: alloc::collections::VecDeque::new() }));
    INPUTS.lock().push(i.clone());
    kprintln!("audio: input {name} ({rate} Hz)");
    i
}

pub fn remove_input(i: &InputRef) {
    INPUTS.lock().retain(|x| !Arc::ptr_eq(x, i));
}

/// Input names (for the choice in settings).
pub fn inputs() -> Vec<String> {
    INPUTS.lock().iter().map(|i| i.lock().name.clone()).collect()
}

/// Someone is recording: input devices should open the microphone.
pub fn recording() -> bool {
    RECORDING.load(core::sync::atomic::Ordering::Relaxed) > 0
}

/// A recording from an input: while it lives, the microphone is open.
pub struct Capture {
    input: Option<InputRef>,
}

impl Capture {
    /// Start recording from the current input (the last connected one; the choice is in the sound settings).
    pub fn start() -> Capture {
        RECORDING.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let input = active_input();
        if let Some(i) = &input {
            i.lock().ring.clear();
        }
        Capture { input }
    }

    /// Input name and rate (None — no input).
    pub fn source(&self) -> Option<(String, u32)> {
        self.input.as_ref().map(|i| {
            let i = i.lock();
            (i.name.clone(), i.rate)
        })
    }

    /// Take what has accumulated (mono, input rate).
    pub fn take(&mut self, out: &mut Vec<i16>) {
        if self.input.is_none() {
            self.input = active_input();
        }
        if let Some(i) = &self.input {
            out.extend(i.lock().ring.drain(..));
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        RECORDING.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
    }
}

/// How many frames to keep in the stream ahead (~130 ms; the mixer tops it up to double —
/// so the sound to headphones survives when the mixer gets no CPU for ~0.2 s).
const STREAM_AHEAD: usize = 6144;

static STREAM: IrqLock<Option<StreamRef>> = IrqLock::new(None);

/// Open a stream: sound goes to it (for example, to headphones) instead of the card.
pub fn open_stream(name: &str) -> StreamRef {
    let s = Arc::new(IrqLock::new(Stream { name: name.into(), ring: alloc::collections::VecDeque::with_capacity(STREAM_AHEAD * 4), absolute: false }));
    *STREAM.lock() = Some(s.clone());
    kprintln!("audio: sound goes to {name}");
    s
}

/// Close a stream: sound goes to the card again (if there is one).
pub fn close_stream(s: &StreamRef) {
    let mut cur = STREAM.lock();
    if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, s)) {
        *cur = None;
        kprintln!("audio: the stream is closed");
    }
}

enum Voice {
    /// A tone: frequency (Hz), frames left, phase (fractions of a period in 1/65536), volume 0..=256.
    Tone { freq: u32, left: usize, phase: u32, volume: i32 },
    /// A recording (stereo, 48 kHz) and the position in it.
    Sample { data: Arc<Vec<i16>>, pos: usize, volume: i32 },
}

struct Card {
    /// The sound card; without it sound can only go to a stream.
    hw: Option<Box<dyn Hw>>,
    buffers: Vec<Box<Buffer>>,
    playing: bool,
    /// The next buffer to fill.
    next: usize,
    /// Buffers filled with silence in a row (they tell us it is time to stop).
    silent: usize,
    voices: Vec<Voice>,
    sine: Vec<i16>,
    /// Ready recordings of system sounds, by `Sound` number.
    effects: Vec<Arc<Vec<i16>>>,
}

static CARD: IrqLock<Option<Card>> = IrqLock::new(None);

/// Volume of system sounds (0..=256); 0 — muted.
static VOLUME: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(200);

/// How many frames have been mixed but not yet heard (output buffers): the spectrum
/// analyser lags the mixer by them to match the sound.
pub fn output_backlog() -> usize {
    match STREAM.lock().clone() {
        // A stream to headphones: its ring plus another ~0.15 s inside the headphones.
        Some(s) => s.lock().ring.len() / 2 + RATE as usize * 15 / 100,
        None => AHEAD * FRAMES,
    }
}

/// There is somewhere to output sound: a card or an open stream.
pub fn available() -> bool {
    CARD.lock().as_ref().is_some_and(|c| c.hw.is_some()) || STREAM.lock().is_some()
}

pub fn set_volume(v: i32) {
    VOLUME.store(v.clamp(0, 256), core::sync::atomic::Ordering::Relaxed);
}

pub fn volume() -> i32 {
    VOLUME.load(core::sync::atomic::Ordering::Relaxed)
}

/// Sound goes to a device that keeps the volume itself (see `Stream::absolute`).
static ABSOLUTE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The volume multiplier for the mixer (0..=256). Hearing perceives volume
/// logarithmically, so the slider moves in decibels: from −45 dB to 0 dB.
fn gain() -> i32 {
    let v = volume();
    if v == 0 {
        return 0;
    }
    if ABSOLUTE.load(core::sync::atomic::Ordering::Relaxed) {
        return 256;
    }
    let db = (1.0 - v as f32 / 256.0) * 45.0;
    ((256.0 * libm::powf(10.0, -db / 20.0)) as i32).max(1)
}

/// Looks for a sound card (HDA, then AC97) and starts the mixer thread. Returns a line for "About".
pub fn init(devices: &[pci::Device]) -> String {
    let found: Option<(Box<dyn Hw>, String)> = match hda::probe(devices) {
        Some((hw, name)) => Some((Box::new(hw), name)),
        None => ac97::probe(devices).map(|hw| (Box::new(hw) as Box<dyn Hw>, "AC97".into())),
    };
    // Without a card the mixer works anyway: sound may go to a stream (headphones).
    let (hw, name) = match found {
        Some((hw, name)) => (Some(hw), name),
        None => {
            kprintln!("audio: no supported sound card");
            (None, String::from("none"))
        }
    };
    let sine: Vec<i16> = (0..1024).map(|i| (libm::sinf(i as f32 * core::f32::consts::TAU / 1024.0) * 32767.0) as i16).collect();
    let effects = render_effects(&sine);
    *CARD.lock() = Some(Card {
        hw,
        buffers: (0..BUFFERS).map(|_| Box::new(Buffer([0; FRAMES * 2]))).collect(),
        playing: false,
        next: 0,
        silent: 0,
        voices: Vec::with_capacity(16),
        sine,
        effects,
    });
    task::spawn("audio", None, || {
        task::set_priority(task::Prio::Sound);
        let mut last = 0;
        loop {
        diag::gap(&diag::GAP_MIX, &mut last);
        diag::report();
        // The device choice is read here, not under the mixer lock (the disk cannot be waited for there).
        load_choices();
        if let Some(card) = CARD.lock().as_mut() {
            card.service();
        }
        task::sleep_until(interrupts::ticks() + 1);
        }
    });
    format!("Sound: {name}")
}


impl Card {
    /// Mixes the voices into buffer `i`; true — something was sounding.
    fn fill(&mut self, i: usize) -> bool {
        let mut out = core::mem::replace(&mut self.buffers[i].0, [0; FRAMES * 2]);
        let loud = self.mix(&mut out);
        self.buffers[i].0 = out;
        loud
    }

    /// Mixes the voices into `FRAMES` frames; true — something was sounding.
    fn mix(&mut self, out: &mut [i16; FRAMES * 2]) -> bool {
        let mut mix = vec![0i32; FRAMES * 2];
        let sine = &self.sine;
        for v in &mut self.voices {
            match v {
                Voice::Tone { freq, left, phase, volume } => {
                    let step = (*freq as u64 * 65536 * 1024 / RATE as u64) as u32;
                    let n = (*left).min(FRAMES);
                    for f in 0..n {
                        // Short attack and release — no clicks.
                        let edge = (f.min(*left - f) as i32).min(240) * 256 / 240;
                        let s = sine[(*phase >> 16) as usize % 1024] as i32 * *volume / 256 * edge / 256;
                        mix[2 * f] += s;
                        mix[2 * f + 1] += s;
                        *phase = phase.wrapping_add(step);
                    }
                    *left -= n;
                }
                Voice::Sample { data, pos, volume } => {
                    let n = ((data.len() - *pos) / 2).min(FRAMES);
                    for f in 0..n {
                        mix[2 * f] += data[*pos + 2 * f] as i32 * *volume / 256;
                        mix[2 * f + 1] += data[*pos + 2 * f + 1] as i32 * *volume / 256;
                    }
                    *pos += 2 * n;
                }
            }
        }
        // File players: ready data from their rings.
        let players: Vec<Arc<player::Player>> = player::PLAYERS.lock().clone();
        let g = gain();
        for p in &players {
            let mut s = p.shared.lock();
            if s.state != player::State::Playing {
                continue;
            }
            let n = (FRAMES * 2).min(s.ring.len() & !1);
            // A shortage in the middle of a track (not at the very start, while the decoder is catching up).
            if n < FRAMES * 2 && !s.drained && s.played > 0 {
                diag::STARVED.fetch_add(((FRAMES * 2 - n) / 2) as u64, Ordering::Relaxed);
            }
            let volume = g * s.volume as i32 / 100;
            // Balance: only the side being panned away from is attenuated.
            let (left, right) = (volume * (100 - s.balance.max(0)) / 100, volume * (100 + s.balance.min(0)) / 100);
            let s = &mut *s;
            for (i, v) in s.ring.drain(..n).enumerate() {
                mix[i] += v as i32 * (if i % 2 == 0 { left } else { right }) / 256;
                s.recent.push_back(v);
            }
            let extra = s.recent.len().saturating_sub(player::HISTORY * 2);
            s.recent.drain(..extra);
            s.played += (n / 2) as u64;
            if n < FRAMES * 2 && s.drained && s.ring.is_empty() {
                s.state = player::State::Ended;
                kprintln!("audio: a player ended at {} s", s.played / RATE as u64);
            }
        }
        self.voices.retain(|v| match v {
            Voice::Tone { left, .. } => *left > 0,
            Voice::Sample { data, pos, .. } => *pos < data.len(),
        });
        let loud = mix.iter().any(|&s| s != 0);
        for (d, s) in out.iter_mut().zip(mix) {
            *d = s.clamp(-32768, 32767) as i16;
        }
        loud
    }

    /// A stream is open: keep it filled (with silence if nothing sounds).
    fn feed(&mut self, stream: &StreamRef) {
        if self.playing {
            self.stop();
        }
        let mut chunk = [0i16; FRAMES * 2];
        while stream.lock().ring.len() < STREAM_AHEAD * 2 {
            self.mix(&mut chunk);
            stream.lock().ring.extend(chunk.iter().copied());
        }
    }

    fn start(&mut self) {
        // The HDA ring spins continuously: buffers not yet filled hold silence.
        for b in &mut self.buffers {
            b.0.fill(0);
        }
        for i in 0..AHEAD {
            self.fill(i);
        }
        self.next = AHEAD % BUFFERS;
        self.silent = 0;
        let addrs: Vec<u64> = self.buffers.iter().map(|b| b.0.as_ptr() as u64).collect();
        if let Some(hw) = &mut self.hw {
            hw.start(&addrs, AHEAD - 1);
            self.playing = true;
        }
    }

    fn stop(&mut self) {
        if let Some(hw) = &mut self.hw {
            hw.stop();
        }
        self.playing = false;
    }

    /// Once a tick: keeps the buffers ahead of the playing one filled.
    fn service(&mut self) {
        if let Some(hw) = &mut self.hw {
            hw.capture(recording());
        }
        let stream = stream_for(self.hw.is_some());
        ABSOLUTE.store(stream.as_ref().is_some_and(|s| s.lock().absolute), core::sync::atomic::Ordering::Relaxed);
        if let Some(s) = stream {
            self.feed(&s);
            return;
        }
        let Some(hw) = &mut self.hw else {
            // Neither a card nor a stream: nowhere to play.
            self.voices.clear();
            return;
        };
        if let Some(hp) = hw.jack() {
            OUTPUT.store(if hp { 2 } else { 1 }, core::sync::atomic::Ordering::Relaxed);
            let (title, body) = if hp {
                ("Headphones connected", "Sound goes to the headphones")
            } else {
                ("Headphones disconnected", "Sound goes to the speakers")
            };
            crate::wm::notify(title, body);
        }
        if !self.playing {
            if !self.voices.is_empty() || player::any_playing() {
                self.start();
            }
            return;
        }
        let Some(hw) = &mut self.hw else { return };
        // The card caught up with the end of the filled part (we were late) — restart it.
        if hw.underrun() {
            self.stop();
            return;
        }
        let civ = hw.position() % BUFFERS;
        // The card ran past the filled part (the thread got no time for long) — catch up with it.
        if (self.next + BUFFERS - civ) % BUFFERS > BUFFERS / 2 {
            self.next = (civ + 1) % BUFFERS;
        }
        while (self.next + BUFFERS - civ) % BUFFERS < AHEAD {
            let i = self.next;
            if self.fill(i) {
                self.silent = 0;
            } else {
                self.silent += 1;
            }
            if let Some(hw) = &mut self.hw {
                hw.queued(i);
            }
            self.next = (i + 1) % BUFFERS;
        }
        // Everything has played (and the silence too) — stop the card.
        if self.voices.is_empty() && self.silent > AHEAD && !player::any_playing() {
            self.stop();
        }
    }
}

/// A tone of frequency `freq` Hz lasting `ms`.
pub fn beep(freq: u32, ms: u32) {
    let volume = gain() * 3 / 4;
    if volume == 0 {
        return;
    }
    if let Some(card) = CARD.lock().as_mut() {
        let left = (RATE as u64 * ms.min(10_000) as u64 / 1000) as usize;
        card.voices.push(Voice::Tone { freq: freq.clamp(20, 20_000), left, phase: 0, volume });
    }
}

/// A system sound. The order is by importance: of several at once, the last one sounds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sound {
    DriveIn,
    DriveOut,
    Notification,
    Error,
    Critical,
}

/// Names for the `sound` command.
pub const SOUNDS: [(&str, Sound); 5] = [
    ("notification", Sound::Notification),
    ("error", Sound::Error),
    ("critical", Sound::Critical),
    ("drive-in", Sound::DriveIn),
    ("drive-out", Sound::DriveOut),
];

/// A note: (frequency, start in ms, volume 0..=256, pan: −256 — left, 256 — right).
type Note = (u32, u32, i32, i32);

/// Everything about a sound except the notes: length in ms, decay rate, overtones (multiple, volume)
/// and the peak volume of the finished recording.
struct Timbre {
    ms: u32,
    fade: i64,
    overtones: &'static [(u32, i32)],
    peak: i64,
}

const BELL: &[(u32, i32)] = &[(1, 256), (2, 70), (3, 22)];
/// Duller: an almost pure tone, like a jack click.
const SOFT: &[(u32, i32)] = &[(1, 256), (2, 36)];
/// Sharper: strong overtones, this sound cannot be mistaken for an ordinary notification.
const HARSH: &[(u32, i32)] = &[(1, 256), (2, 120), (3, 90), (5, 40)];

fn effect(sound: Sound) -> (&'static [Note], Timbre) {
    match sound {
        Sound::Notification => (&[(1047, 0, 150, -30), (1568, 85, 140, 30)], Timbre { ms: 520, fade: 90, overtones: BELL, peak: 20000 }),
        Sound::Error => (&[(466, 0, 170, 0), (349, 125, 180, 0)], Timbre { ms: 520, fade: 90, overtones: HARSH, peak: 22000 }),
        Sound::Critical => (
            &[(622, 0, 150, 0), (880, 0, 120, 0), (622, 175, 150, 0), (880, 175, 120, 0), (622, 350, 170, 0), (880, 350, 140, 0)],
            Timbre { ms: 900, fade: 130, overtones: HARSH, peak: 26000 },
        ),
        Sound::DriveIn => (&[(587, 0, 150, -60), (880, 75, 160, 60)], Timbre { ms: 360, fade: 150, overtones: SOFT, peak: 20000 }),
        Sound::DriveOut => (&[(880, 0, 160, 60), (587, 75, 150, -60)], Timbre { ms: 360, fade: 150, overtones: SOFT, peak: 20000 }),
    }
}

/// Recordings of system sounds. Prepared in advance: after a kernel crash memory can no longer be allocated.
fn render_effects(sine: &[i16]) -> Vec<Arc<Vec<i16>>> {
    [Sound::DriveIn, Sound::DriveOut, Sound::Notification, Sound::Error, Sound::Critical]
        .map(|s| {
            let (notes, timbre) = effect(s);
            Arc::new(render(sine, notes, &timbre))
        })
        .into_iter()
        .collect()
}

/// Plays a system sound.
pub fn play(sound: Sound) {
    if let Some(c) = CARD.lock().as_mut() {
        let data = c.effects[sound as usize].clone();
        c.voices.push(Voice::Sample { data, pos: 0, volume: gain() });
    }
}

/// The system crashed: there is no timer anymore, so we feed the card ourselves until it finishes playing
/// and stops: abandoned mid-way, it would loop its buffer forever.
/// The error screen is already shown, so hanging here is harmless.
pub fn crash_alarm() {
    const LONGEST_CYCLES: u64 = 8_000_000_000;
    let Some(mut guard) = CARD.try_lock() else { return };
    let Some(card) = guard.as_mut() else { return };
    let (data, volume) = (card.effects[Sound::Critical as usize].clone(), gain());
    card.voices.clear();
    card.voices.push(Voice::Sample { data, pos: 0, volume });
    let end = crate::arch::rdtsc() + LONGEST_CYCLES;
    while (card.playing || !card.voices.is_empty()) && crate::arch::rdtsc() < end {
        card.service();
        core::hint::spin_loop();
    }
    card.stop();
}

/// The welcome sound: a soft Cmaj9 arpeggio of "bells" — a fundamental
/// with overtones, a fast attack and a long decay; the notes are slightly spread across channels.
pub fn welcome() {
    const NOTES: [Note; 6] =
        [(262, 0, 150, 0), (392, 110, 120, -90), (587, 220, 105, 90), (659, 330, 100, -60), (988, 460, 80, 60), (1319, 600, 55, 0)];
    let Some(sine) = CARD.lock().as_ref().map(|c| c.sine.clone()) else { return };
    let data = render(&sine, &NOTES, &Timbre { ms: 2600, fade: 40, overtones: BELL, peak: 22000 });
    if let Some(c) = CARD.lock().as_mut() {
        c.voices.push(Voice::Sample { data: Arc::new(data), pos: 0, volume: gain() });
    }
}

/// Builds notes as "bells": each has a fast attack and decay, overtones fade faster.
fn render(sine: &[i16], notes: &[Note], timbre: &Timbre) -> Vec<i16> {
    let total = (RATE * timbre.ms / 1000) as usize;
    let mut mix = vec![0i32; total * 2];
    for &(freq, start, level, pan) in notes {
        let at = (RATE * start / 1000) as usize;
        for &(mult, part) in timbre.overtones {
            let step = (freq as u64 * mult as u64 * 65536 * 1024 / RATE as u64) as u32;
            let mut phase = 0u32;
            // Envelope in 1/65536: a 6 ms attack, then an exponential (faster for overtones).
            let attack = RATE as usize * 6 / 1000;
            let decay: i64 = 65536 - timbre.fade * mult as i64;
            let mut env: i64 = 65536 << 16;
            for f in 0..total - at {
                let a = if f < attack { (f * 65536 / attack) as i64 } else { env >> 16 };
                if f >= attack && f % 16 == 0 {
                    env = env * decay >> 16;
                }
                if f >= attack && a < 4 {
                    break; // faded out
                }
                let s = sine[(phase >> 16) as usize % 1024] as i64 * a >> 16;
                let s = (s * level as i64 >> 8) * part as i64 >> 8;
                mix[2 * (at + f)] += (s * (256 - pan.max(0)) as i64 >> 8) as i32;
                mix[2 * (at + f) + 1] += (s * (256 + pan.min(0)) as i64 >> 8) as i32;
                phase = phase.wrapping_add(step);
            }
        }
    }
    // Volume headroom: the chord must not hit the limit.
    let peak = mix.iter().map(|s| s.abs()).max().unwrap_or(1).max(1);
    mix.iter().map(|&s| (s as i64 * timbre.peak / peak as i64) as i16).collect()
}

/// Parses a WAV (PCM 8/16-bit, mono/stereo, any rate) into 48 kHz stereo.
pub fn decode_wav(d: &[u8]) -> Result<Vec<i16>, &'static str> {
    if d.len() < 12 || &d[0..4] != b"RIFF" || &d[8..12] != b"WAVE" {
        return Err("not a WAV file");
    }
    let le16 = |at: usize| u16::from_le_bytes([d[at], d[at + 1]]) as u32;
    let le32 = |at: usize| u32::from_le_bytes(d[at..at + 4].try_into().unwrap());
    let (mut fmt, mut data) = (None, None);
    let mut at = 12;
    while at + 8 <= d.len() {
        let (id, len) = (&d[at..at + 4], le32(at + 4) as usize);
        let body = at + 8;
        match id {
            b"fmt " if len >= 16 && body + 16 <= d.len() => {
                fmt = Some((le16(body), le16(body + 2), le32(body + 4), le16(body + 14)));
            }
            b"data" => data = Some(&d[body..(body + len).min(d.len())]),
            _ => {}
        }
        at = body + len + (len & 1);
    }
    let (format, channels, rate, bits) = fmt.ok_or("WAV without format")?;
    let data = data.ok_or("WAV without data")?;
    if format != 1 || !(bits == 8 || bits == 16) || !(1..=2).contains(&channels) || rate == 0 {
        return Err("only PCM 8/16-bit mono/stereo WAV is supported");
    }
    let bytes = (bits / 8) as usize;
    let frame = bytes * channels as usize;
    let frames = data.len() / frame;
    let sample = |f: usize, ch: usize| -> i16 {
        let at = f * frame + ch.min(channels as usize - 1) * bytes;
        if bits == 8 { ((data[at] as i16) - 128) << 8 } else { i16::from_le_bytes([data[at], data[at + 1]]) }
    };
    // Rate conversion — nearest sample (enough for system sounds).
    let out_frames = (frames as u64 * RATE as u64 / rate as u64) as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for i in 0..out_frames {
        let f = ((i as u64 * rate as u64 / RATE as u64) as usize).min(frames.saturating_sub(1));
        out.push(sample(f, 0));
        out.push(sample(f, 1));
    }
    Ok(out)
}

/// A WAV (16-bit PCM) from samples: `channels` interleaved channels, rate `rate`.
pub fn encode_wav(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
    let data = samples.len() as u32 * 2;
    let mut w = Vec::with_capacity(44 + data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    w.extend_from_slice(&(channels * 2).to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// Plays a WAV file (mixed with other sounds).
pub fn play_wav(data: &[u8]) -> Result<(), &'static str> {
    if !available() {
        return Err("no sound card");
    }
    let samples = decode_wav(data)?;
    if let Some(c) = CARD.lock().as_mut() {
        c.voices.push(Voice::Sample { data: Arc::new(samples), pos: 0, volume: gain() });
    }
    Ok(())
}

/// Plays a recording (mono, rate `rate`): resampled to 48 kHz stereo with linear
/// interpolation. Returns the length in 48 kHz frames (for the progress bar).
pub fn play_pcm(samples: &[i16], rate: u32) -> usize {
    if samples.is_empty() || rate == 0 {
        return 0;
    }
    let frames = (samples.len() as u64 * RATE as u64 / rate as u64) as usize;
    let mut out = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        // Position in the source recording with a fractional part (in 1/65536).
        let pos = i as u64 * rate as u64 * 65536 / RATE as u64;
        let (at, frac) = ((pos >> 16) as usize, (pos & 0xFFFF) as i32);
        let a = samples[at.min(samples.len() - 1)] as i32;
        let b = samples[(at + 1).min(samples.len() - 1)] as i32;
        let s = (a + (b - a) * frac / 65536) as i16;
        out.push(s);
        out.push(s);
    }
    if let Some(c) = CARD.lock().as_mut() {
        c.voices.push(Voice::Sample { data: Arc::new(out), pos: 0, volume: gain() });
    }
    frames
}

/// Stops all sounds.
pub fn stop_all() {
    if let Some(c) = CARD.lock().as_mut() {
        c.voices.clear();
    }
}
