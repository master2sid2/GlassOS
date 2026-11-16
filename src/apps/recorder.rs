//! Sound recording: from the chosen input (Settings → Sound) into memory, with a level meter and a
//! waveform; play it back, save it as WAV, start over. While recording, the input is open
//! (for a Bluetooth headset — the voice channel, with music paused).

use super::dialog::{self, Mode, Reply};
use super::ui;
use crate::audio::{self, Capture};
use crate::font::{self, FontId};
use crate::fs;
use crate::gfx::{Canvas, Color, Rect};
use crate::interrupts::{self, HZ};
use crate::wm::{ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const BG: Color = 0x161925;
const PAD: i32 = 16;
const WAVE_H: i32 = 70;
const RED: Color = 0xE5484D;
const GREEN: Color = 0x4DC27A;
const YELLOW: Color = 0xE5B84D;
/// No more than this is written to memory (an hour at 8 kHz ≈ 55 MB — plenty).
const MAX_SECONDS: usize = 3600;

#[derive(Clone, Copy, PartialEq)]
enum Button {
    Record,
    Play,
    Save,
    New,
}

pub struct Recorder {
    /// Recording is in progress (the input is open) — and since which tick.
    capture: Option<Capture>,
    samples: Vec<i16>,
    rate: u32,
    source: Option<String>,
    /// The peak of the last samples (0..=100) for the level bar.
    level: u32,
    /// Playback: since which tick and how long it lasts (ticks).
    playing: Option<(u64, u64)>,
    saved: bool,
    dialog: Option<Reply>,
    status: String,
    buttons: Vec<(Rect, Button)>,
    shown: u64,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            capture: None,
            samples: Vec::new(),
            rate: 8000,
            source: audio::current_input(),
            level: 0,
            playing: None,
            saved: true,
            dialog: None,
            status: String::new(),
            buttons: Vec::new(),
            shown: 0,
        }
    }

    fn seconds(&self) -> f32 {
        self.samples.len() as f32 / self.rate.max(1) as f32
    }

    fn record(&mut self) {
        if self.capture.is_some() {
            self.capture = None;
            self.level = 0;
            self.status = format!("Recorded {}", time_text(self.seconds()));
            return;
        }
        self.stop_play();
        let cap = Capture::start();
        if let Some((name, rate)) = cap.source() {
            // Continue the recording if the rate is the same; otherwise a new one.
            if rate != self.rate {
                self.samples.clear();
            }
            self.rate = rate;
            self.source = Some(name);
        }
        self.capture = Some(cap);
        self.saved = false;
        self.status.clear();
    }

    fn play(&mut self) {
        if self.playing.is_some() {
            self.stop_play();
            return;
        }
        if self.samples.is_empty() || self.capture.is_some() {
            return;
        }
        let frames = audio::play_pcm(&self.samples, self.rate);
        let ticks = frames as u64 * HZ / audio::RATE as u64 + 1;
        self.playing = Some((interrupts::ticks(), ticks));
    }

    fn stop_play(&mut self) {
        if self.playing.take().is_some() {
            audio::stop_all();
        }
    }

    fn save(&mut self) {
        if self.samples.is_empty() || self.dialog.is_some() {
            return;
        }
        let t = crate::time::now();
        let name = format!("recording-{:02}{:02}{:02}.wav", t.hour, t.min, t.sec);
        self.dialog = Some(dialog::request(Mode::Save, "", &name));
    }

    fn new_recording(&mut self) {
        self.capture = None;
        self.stop_play();
        self.samples.clear();
        self.saved = true;
        self.level = 0;
        self.status.clear();
    }

    fn press(&mut self, b: Button) {
        match b {
            Button::Record => self.record(),
            Button::Play => self.play(),
            Button::Save => self.save(),
            Button::New => self.new_recording(),
        }
    }
}

/// «1:05.3».
fn time_text(seconds: f32) -> String {
    let tenths = (seconds * 10.0) as u64;
    format!("{}:{:02}.{}", tenths / 600, tenths / 10 % 60, tenths % 10)
}

impl App for Recorder {
    fn background(&self) -> Color {
        BG
    }

    fn tick(&mut self, _ctx: &Ctx) -> bool {
        let mut changed = false;
        if let Some(cap) = &mut self.capture {
            let mut fresh = Vec::new();
            cap.take(&mut fresh);
            if self.source.is_none()
                && let Some((name, rate)) = cap.source()
            {
                self.source = Some(name);
                self.rate = rate;
                changed = true;
            }
            if !fresh.is_empty() {
                let peak = fresh.iter().map(|s| (*s as i32).unsigned_abs()).max().unwrap_or(0).max(1) as f32;
                let level = (((20.0 * libm::log10f(peak / 32768.0)) + 60.0) * 100.0 / 60.0).clamp(0.0, 100.0) as u32;
                self.level = level.max(self.level.saturating_sub(6));
                let room = (MAX_SECONDS * self.rate as usize).saturating_sub(self.samples.len());
                self.samples.extend_from_slice(&fresh[..fresh.len().min(room)]);
                if room == 0 {
                    self.record();
                }
                changed = true;
            }
        } else if self.source.as_deref() != audio::current_input().as_deref() && self.samples.is_empty() {
            self.source = audio::current_input();
            changed = true;
        }
        if let Some((start, len)) = self.playing
            && interrupts::ticks() >= start + len
        {
            self.playing = None;
            changed = true;
        }
        if let Some(reply) = &self.dialog
            && let Some(answer) = dialog::take(reply)
        {
            self.dialog = None;
            if let Some(path) = answer {
                let path = if path.to_ascii_lowercase().ends_with(".wav") { path } else { format!("{path}.wav") };
                self.status = match fs::write(&path, &audio::encode_wav(&self.samples, self.rate, 1)) {
                    Ok(()) => {
                        self.saved = true;
                        format!("Saved to {}", fs::name(&path))
                    }
                    Err(e) => format!("Not saved: {e}"),
                };
            }
            changed = true;
        }
        // Playback time is running — redraw the bar.
        let now = interrupts::ticks() / (HZ / 10);
        if self.playing.is_some() && now != self.shown {
            self.shown = now;
            changed = true;
        }
        changed
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        let (x, w) = (a.x + PAD, a.w - 2 * PAD);
        let mut y = a.y + PAD;
        // Time and state.
        let recording = self.capture.is_some();
        let time = time_text(self.seconds());
        let color = if recording { RED } else { TEXT };
        c.text(x, y - 4, &time, color, FontId::SemiBold, 40.0);
        let state = match (&self.source, recording, self.playing.is_some()) {
            (None, _, _) => String::from("No microphone. Connect a Bluetooth headset, or pick one in Settings \u{2192} Sound."),
            (Some(name), true, _) => format!("Recording from {name}"),
            (_, _, true) => String::from("Playing"),
            (Some(name), false, false) if self.status.is_empty() => format!("Ready \u{00B7} {name}"),
            _ => self.status.clone(),
        };
        let state = ui::fit(&state, FontId::Regular, 13.0, w);
        c.text(x, y + 50, &state, TEXT_DIM, FontId::Regular, 13.0);
        y += 78;
        // The level bar.
        let bar = Rect::new(x, y, w, 6);
        c.fill_round(bar, 3, 0xFFFFFF, 30);
        if recording {
            let lc = match self.level {
                0..=70 => GREEN,
                71..=90 => YELLOW,
                _ => RED,
            };
            c.fill_round(Rect::new(x, y, (w * self.level as i32 / 100).max(6), 6), 3, lc, 256);
        }
        y += 18;
        // The waveform: peaks as columns over the whole recording.
        let wave = Rect::new(x, y, w, WAVE_H);
        c.fill_round(wave, 8, 0x000000, 60);
        let mid = y + WAVE_H / 2;
        if !self.samples.is_empty() {
            let cols = (w - 8).max(1) as usize;
            let per = self.samples.len().div_ceil(cols).max(1);
            for (i, chunk) in self.samples.chunks(per).enumerate() {
                let peak = chunk.iter().map(|s| (*s as i32).abs()).max().unwrap_or(0);
                let h = (peak * (WAVE_H - 10) / 32768).max(1);
                c.fill(Rect::new(x + 4 + i as i32, mid - h / 2, 1, h), ACCENT);
            }
        } else {
            c.fill(Rect::new(x + 4, mid, w - 8, 1), 0x3A3F55);
        }
        // Where playback is now.
        if let Some((start, len)) = self.playing {
            let done = (interrupts::ticks() - start).min(len) as i32;
            let px = x + 4 + (w - 8) * done / len.max(1) as i32;
            c.fill(Rect::new(px, y + 4, 2, WAVE_H - 8), 0xFFFFFF);
        }
        y += WAVE_H + 16;
        // Buttons.
        // Before "Record" — room for the red dot.
        let record_label = if recording { "Stop" } else { "    Record" };
        let play_label = if self.playing.is_some() { "Stop" } else { "Play" };
        let labels = [record_label, play_label, "Save\u{2026}", "New"];
        let rects = ui::button_row(&labels, x, y);
        let kinds = [Button::Record, Button::Play, Button::Save, Button::New];
        let has = !self.samples.is_empty();
        self.buttons.clear();
        for ((r, label), kind) in rects.iter().zip(labels).zip(kinds) {
            let enabled = match kind {
                Button::Record => self.source.is_some() || recording,
                Button::Play => has && !recording,
                Button::Save => has && !recording && self.dialog.is_none(),
                Button::New => has,
            };
            if kind == Button::Record && !recording {
                // The record button — a red dot and a label.
                ui::accent_button(c, *r, label, enabled);
                c.fill_round(Rect::new(r.x + 14, r.y + r.h / 2 - 4, 8, 8), 4, RED, if enabled { 256 } else { 120 });
            } else {
                ui::button(c, *r, label, enabled);
            }
            if enabled {
                self.buttons.push((r.offset(-a.x, -a.y), kind));
            }
        }
        let size = format!("{:.0} KB in memory", self.samples.len() as f32 * 2.0 / 1024.0);
        let sw = font::measure(FontId::Regular, 12.0, &size);
        if has {
            c.text(a.x + a.w - PAD - sw, y + 7, &size, TEXT_DIM, FontId::Regular, 12.0);
        }
    }

    fn mouse(&mut self, x: i32, y: i32, _buttons: u8, pressed: bool) -> bool {
        if !pressed {
            return false;
        }
        let Some(&(_, b)) = self.buttons.iter().find(|(r, _)| r.contains(x, y)) else { return false };
        self.press(b);
        true
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        match ch {
            b' ' => self.record(),
            b'\n' => self.play(),
            _ => return false,
        }
        true
    }

    fn unsaved(&self) -> bool {
        !self.saved && !self.samples.is_empty()
    }

    fn memory(&self) -> usize {
        self.samples.capacity() * 2
    }
}
