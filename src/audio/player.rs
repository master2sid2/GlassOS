//! Playing files (WAV, MP3, OGG): the "player" thread decodes half a second ahead
//! and resamples to 48 kHz stereo; the mixer takes the result from a ring and mixes it with
//! other sounds. Control is by player number (`open` → number): play,
//! pause, stop, seek, volume, state. Lua programs get the same API.

use super::RATE;
use super::track::Track;
use crate::interrupts::{self, HZ};
use crate::sync::{IrqLock, YieldLock};
use crate::task;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// How many frames to keep ready ahead (~0.5 s) and decode at once.
const AHEAD: usize = RATE as usize / 2;
const CHUNK: usize = 2048;
/// How many frames to decode right away on open (~0.25 s).
const PREFILL: usize = RATE as usize / 4;
/// The program has not asked about the player for this long — so it is gone: close.
const ABANDONED: u64 = 10 * HZ;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Playing,
    Paused,
    Stopped,
    Ended,
    /// The previous track was requested (headset or keyboard button) — the program
    /// that opened the player decides.
    Back,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Playing => "playing",
            State::Paused => "paused",
            State::Stopped => "stopped",
            State::Ended => "ended",
            State::Back => "back",
        }
    }
}

/// What the mixer and the controls share (under a short lock).
pub(super) struct Shared {
    pub(super) state: State,
    pub(super) volume: u32,
    pub(super) ring: VecDeque<i16>,
    /// 48 kHz frames played (for the position).
    pub(super) played: u64,
    /// The decoder reached the end of the file.
    pub(super) drained: bool,
    /// A seek waiting for the decoder (file frame).
    seek: Option<u64>,
    polled: u64,
    /// Balance: −100 (left only) … 100 (right only).
    pub(super) balance: i32,
    /// The last data given to the mixer (stereo, before volume) — for the spectrum analyser.
    pub(super) recent: VecDeque<i16>,
}

/// How many recent frames to remember for the spectrum (~0.7 s: enough to cover the
/// output latency to headphones).
pub(super) const HISTORY: usize = 32768;
/// Fourier transform window (frames).
const FFT_N: usize = 1024;

/// The decoder (under its own lock; long operations happen outside the mixer lock).
struct Decoder {
    track: Track,
    /// File frames not yet resampled and the fractional position between them (1/65536 frame).
    src: VecDeque<i16>,
    frac: u64,
}

pub(super) struct Player {
    id: u32,
    pub(super) shared: IrqLock<Shared>,
    decoder: YieldLock<Decoder>,
    /// Thread of the program that opened it (Lua): when it closes, the sound closes too. It
    /// must also ask about the state now and then, otherwise we consider it gone.
    owner: Option<u64>,
    /// Cover art from the file tags (PNG or JPEG, as is).
    cover: Option<Arc<Vec<u8>>>,
    rate: u32,
    channels: usize,
    total: Option<u64>,
    info: (String, String, String, String),
}

pub(super) static PLAYERS: IrqLock<Vec<Arc<Player>>> = IrqLock::new(Vec::new());
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static THREAD: AtomicBool = AtomicBool::new(false);
/// The last player told to "play" — the buttons control it.
static LAST: AtomicU32 = AtomicU32::new(0);

fn find(id: u32) -> Option<Arc<Player>> {
    PLAYERS.lock().iter().find(|p| p.id == id).cloned()
}

/// Open a file (already read). `owner` is the program's thread: close together with it
/// (and if it does not ask about the player for a long time).
pub fn open(data: Vec<u8>, owner: Option<u64>) -> Result<u32, &'static str> {
    let mut track = Track::open(data)?;
    let cover = track.cover.take().map(Arc::new);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let info = (String::from(track.format), track.title.clone(), track.artist.clone(), track.album.clone());
    let p = Arc::new(Player {
        id,
        rate: track.rate,
        channels: track.channels,
        total: track.total,
        info,
        owner,
        cover,
        shared: IrqLock::new(Shared { state: State::Stopped, volume: 100, ring: VecDeque::new(), played: 0, drained: false, seek: None, polled: interrupts::ticks(), balance: 0, recent: VecDeque::new() }),
        decoder: YieldLock::new(Decoder { track, src: VecDeque::new(), frac: 0 }),
    });
    crate::kprintln!("player {id}: {} {} Hz, {} channels, {}", p.info.0, p.rate, p.channels, p.total.map_or(String::from("length unknown"), |t| alloc::format!("{} s", t / p.rate.max(1) as u64)));
    // Prepare the start of the track right away: the first "play" sounds without a stutter, without
    // waiting for the decoding thread.
    while p.shared.lock().ring.len() < PREFILL * 2 {
        let (chunk, end) = decode_chunk(&p);
        let mut s = p.shared.lock();
        s.ring.extend(chunk);
        s.drained |= end;
        if end {
            break;
        }
    }
    PLAYERS.lock().push(p);
    if !THREAD.swap(true, Ordering::Relaxed) {
        task::spawn("player", None, decode_loop);
    }
    Ok(id)
}

pub fn close(id: u32) {
    PLAYERS.lock().retain(|p| p.id != id);
}

/// The program closed — so do its players (the sound stops at once).
pub fn close_owned(owner: u64) {
    PLAYERS.lock().retain(|p| p.owner != Some(owner));
}

/// Track cover art (PNG/JPEG bytes), if present in the tags.
pub fn cover(id: u32) -> Option<Arc<Vec<u8>>> {
    find(id)?.cover.clone()
}

fn control(id: u32, f: impl FnOnce(&mut Shared)) -> bool {
    let Some(p) = find(id) else { return false };
    let mut s = p.shared.lock();
    s.polled = interrupts::ticks();
    f(&mut s);
    true
}

pub fn play(id: u32) -> bool {
    LAST.store(id, Ordering::Relaxed);
    control(id, |s| {
        if matches!(s.state, State::Ended | State::Back) {
            s.seek = Some(0);
            s.played = 0;
            s.ring.clear();
            s.drained = false;
        }
        s.state = State::Playing;
    })
}

pub fn pause(id: u32) -> bool {
    control(id, |s| {
        if s.state == State::Playing {
            s.state = State::Paused;
        }
    })
}

pub fn stop(id: u32) -> bool {
    control(id, |s| {
        s.state = State::Stopped;
        s.seek = Some(0);
        s.played = 0;
        s.ring.clear();
        s.drained = false;
    })
}

/// Seek to `ms` milliseconds from the start.
pub fn seek(id: u32, ms: u64) -> bool {
    let Some(p) = find(id) else { return false };
    let frame = ms * p.rate as u64 / 1000;
    let frame = p.total.map_or(frame, |t| frame.min(t));
    control(id, |s| {
        s.seek = Some(frame);
        s.played = ms * RATE as u64 / 1000;
        s.ring.clear();
        s.drained = false;
        if s.state == State::Ended {
            s.state = State::Paused;
        }
    })
}

pub fn set_volume(id: u32, volume: u32) -> bool {
    control(id, |s| s.volume = volume.min(100))
}

pub fn set_balance(id: u32, balance: i32) -> bool {
    control(id, |s| s.balance = balance.clamp(-100, 100))
}

/// Levels of `bands` spectrum bands (0..=100) of what is playing now: the window is taken with
/// a correction for what is still in the output buffers (otherwise the picture would run ahead of the sound).
pub fn spectrum(id: u32, bands: usize) -> Option<Vec<u8>> {
    let p = find(id)?;
    let bands = bands.clamp(1, 64);
    let lag = super::output_backlog();
    let mut re = alloc::vec![0f32; FFT_N];
    {
        let s = p.shared.lock();
        let frames = s.recent.len() / 2;
        if s.state != State::Playing || frames < FFT_N {
            return Some(alloc::vec![0; bands]);
        }
        let end = frames.saturating_sub(lag).max(FFT_N);
        let start = end - FFT_N;
        for (i, v) in re.iter_mut().enumerate() {
            let f = (start + i) * 2;
            let mono = (s.recent[f] as f32 + s.recent[f + 1] as f32) * 0.5;
            // Hann window.
            let w = 0.5 - 0.5 * libm::cosf(core::f32::consts::TAU * i as f32 / (FFT_N - 1) as f32);
            *v = mono * w;
        }
    }
    let mut im = alloc::vec![0f32; FFT_N];
    fft(&mut re, &mut im);
    // Bands on a logarithmic scale from 40 Hz to 16 kHz.
    let bin_hz = super::RATE as f32 / FFT_N as f32;
    let (lo, hi) = (40f32, 16000f32);
    let mut out = Vec::with_capacity(bands);
    for b in 0..bands {
        let f0 = lo * libm::powf(hi / lo, b as f32 / bands as f32);
        let f1 = lo * libm::powf(hi / lo, (b + 1) as f32 / bands as f32);
        let (k0, k1) = ((f0 / bin_hz) as usize, ((f1 / bin_hz) as usize).max((f0 / bin_hz) as usize + 1).min(FFT_N / 2));
        let peak = (k0..k1).map(|k| re[k] * re[k] + im[k] * im[k]).fold(0f32, f32::max);
        // A full sine wave gives an amplitude of ~N/4·32768 (Hann window).
        let db = 10.0 * libm::log10f(peak.max(1e-9)) - 20.0 * libm::log10f(FFT_N as f32 / 4.0 * 32768.0);
        out.push(((db + 60.0) * 100.0 / 60.0).clamp(0.0, 100.0) as u8);
    }
    Some(out)
}

/// In-place Fourier transform (radix 2).
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -core::f32::consts::TAU / len as f32;
        let (wr, wi) = (libm::cosf(ang), libm::sinf(ang));
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1f32, 0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let nr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = nr;
            }
        }
        len <<= 1;
    }
}

/// File information without playing: length (ms, 0 — unknown), title,
/// artist, album. Only the start of the file is read (and the end for OGG).
pub fn probe(path: &str) -> Result<(u64, String, String, String), String> {
    const HEAD: usize = 256 * 1024;
    const TAIL: u64 = 64 * 1024;
    let (head, size) = crate::fs::read_range(path, 0, HEAD).map_err(|e| alloc::format!("{e}"))?;
    let tail = if head.starts_with(b"OggS") && size > HEAD as u64 {
        crate::fs::read_range(path, size - TAIL, TAIL as usize).map(|(t, _)| t).unwrap_or_default()
    } else {
        // MP3: audio starts after the ID3 tag (a large cover may lie beyond `head`).
        let tag = super::track::id3_end(&head);
        if tag + 16 * 1024 <= head.len() {
            head[tag..].to_vec()
        } else {
            crate::fs::read_range(path, tag as u64, 16 * 1024).map(|(t, _)| t).unwrap_or_default()
        }
    };
    super::track::probe(&head, &tail, size).ok_or_else(|| String::from("not a sound file"))
}

/// State, position and length (ms; length 0 — unknown).
pub fn status(id: u32) -> Option<(State, u64, u64)> {
    let p = find(id)?;
    let mut s = p.shared.lock();
    s.polled = interrupts::ticks();
    let len = p.total.map_or(0, |t| t * 1000 / p.rate.max(1) as u64);
    Some((s.state, s.played * 1000 / RATE as u64, len))
}

/// Format, title, artist, sample rate, channels, album.
pub fn info(id: u32) -> Option<(String, String, String, u32, usize, String)> {
    let p = find(id)?;
    Some((p.info.0.clone(), p.info.1.clone(), p.info.2.clone(), p.rate, p.channels, p.info.3.clone()))
}

/// Media buttons (keyboard, headset).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Media {
    Toggle,
    Play,
    Pause,
    Stop,
    Next,
    Previous,
}

/// A button: "pause" goes to all playing players, the rest to the last one started.
pub fn media(m: Media) {
    let playing = any_playing();
    let last = LAST.load(Ordering::Relaxed);
    match m {
        Media::Pause | Media::Toggle if playing => {
            for p in PLAYERS.lock().iter() {
                let mut s = p.shared.lock();
                if s.state == State::Playing {
                    s.state = State::Paused;
                }
            }
        }
        Media::Play | Media::Toggle => {
            if !playing {
                play(last);
            }
        }
        Media::Pause => {}
        Media::Stop => {
            let ids: Vec<u32> = PLAYERS.lock().iter().map(|p| p.id).collect();
            for id in ids {
                stop(id);
            }
        }
        // Next: the track "ended" — the program moves on by itself.
        Media::Next => {
            control(last, |s| {
                s.state = State::Ended;
                s.ring.clear();
                s.drained = true;
            });
        }
        // Previous: from the start if more than three seconds were played, otherwise ask the program.
        Media::Previous => {
            let far = status(last).is_some_and(|(_, pos, _)| pos > 3000);
            if far {
                seek(last, 0);
            } else {
                control(last, |s| {
                    s.state = State::Back;
                    s.ring.clear();
                });
            }
        }
    }
}

/// Something is playing — the mixer must not stop the card.
pub fn any_playing() -> bool {
    PLAYERS.lock().iter().any(|p| p.shared.lock().state == State::Playing)
}

/// Decoding thread: keeps the rings of playing players filled, performs seeks,
/// closes abandoned players.
fn decode_loop() {
    task::set_priority(task::Prio::Ui);
    loop {
        let now = interrupts::ticks();
        let list: Vec<Arc<Player>> = PLAYERS.lock().clone();
        for p in &list {
            let (state, seek, need, polled) = {
                let mut s = p.shared.lock();
                (s.state, s.seek.take(), s.ring.len() < AHEAD * 2 && !s.drained, s.polled)
            };
            if p.owner.is_some() && now > polled + ABANDONED {
                close(p.id);
                continue;
            }
            if let Some(frame) = seek {
                let mut d = p.decoder.lock();
                d.track.seek(frame);
                d.src.clear();
                d.frac = 0;
            }
            if state != State::Playing || !need {
                continue;
            }
            let (chunk, end) = decode_chunk(p);
            let mut s = p.shared.lock();
            // While we decoded, a seek or stop may have happened — then the chunk is not needed.
            if s.seek.is_none() && s.state == State::Playing {
                s.ring.extend(chunk);
                if end && !s.drained {
                    crate::kprintln!("player {}: the file is decoded to the end", p.id);
                }
                s.drained |= end;
            }
        }
        task::sleep_until(now + 1);
    }
}

/// A file chunk → 48 kHz stereo (linear interpolation). true — the file ended.
fn decode_chunk(p: &Player) -> (Vec<i16>, bool) {
    let t0 = crate::arch::rdtsc();
    let r = decode_chunk_inner(p);
    super::diag::DECODE.fetch_add(crate::arch::rdtsc() - t0, Ordering::Relaxed);
    r
}

fn decode_chunk_inner(p: &Player) -> (Vec<i16>, bool) {
    let mut d = p.decoder.lock();
    let ch = p.channels.max(1);
    let mut raw = Vec::with_capacity(CHUNK * ch);
    let n = d.track.read(CHUNK, &mut raw);
    d.src.extend(raw);
    let end = n == 0;
    let step = p.rate as u64 * 65536 / RATE as u64;
    let frames = d.src.len() / ch;
    let mut out = Vec::with_capacity(CHUNK * 2 * 2);
    let at = |src: &VecDeque<i16>, f: usize, c: usize| src[f * ch + c.min(ch - 1)] as i32;
    while ((d.frac >> 16) as usize) + 1 < frames {
        let (f, t) = ((d.frac >> 16) as usize, (d.frac & 0xFFFF) as i32);
        for c in 0..2 {
            let (a, b) = (at(&d.src, f, c), at(&d.src, f + 1, c));
            out.push((a + (b - a) * t / 65536) as i16);
        }
        d.frac += step;
    }
    // Drop the whole frames already consumed.
    let used = ((d.frac >> 16) as usize).min(frames);
    d.src.drain(..used * ch);
    d.frac -= (used as u64) << 16;
    if end {
        d.src.clear();
        d.frac = 0;
    }
    (out, end)
}
