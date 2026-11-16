//! Copying and moving files in the background — with a progress window.
//!
//! First a plan is made (all files and folders with their sizes), then the
//! "transfer" thread walks it file by file and updates the shared state; the progress
//! window draws it and can cancel the operation between files.

use super::ui;
use crate::font::{self, FontId};
use crate::fs;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::interrupts::{self, HZ};
use crate::sync::IrqLock;
use crate::task;
use crate::wm::{self, ACCENT, App, Ctx, Request, TEXT, TEXT_DIM};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const BG: Color = 0x1C1F2E;
const W: i32 = 440;
const H: i32 = 250;
const PAD: i32 = 20;
/// The stripes move 10 times a second: otherwise the glass window would be recomputed
/// every frame for them and would occupy all cores.
const PHASE_TICKS: u64 = 10;
const PHASE_SHIFT: u64 = 3;
const BAR_H: i32 = 14;
const BAR_RADIUS: i32 = 7;
/// How many pixels narrower the rounded bar is at each edge in each row.
const BAR_EDGE: [i32; BAR_H as usize] = [5, 3, 2, 1, 1, 0, 0, 0, 0, 1, 1, 2, 3, 5];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Move,
}

/// The operation state (read by the window and the file manager).
pub struct Progress {
    pub mode: Mode,
    pub dest: String,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub total_files: usize,
    pub done_files: usize,
    pub current: String,
    pub started: u64,
    pub finished_at: Option<u64>,
    pub error: Option<String>,
    pub cancel: bool,
    /// Still counting files: how many there are and how much they weigh.
    pub counting: bool,
    /// The name in the destination folder is taken: the person decides what to do.
    pub question: Option<Question>,
    pub answer: Option<(Choice, bool)>,
}

/// The destination folder already has such a name.
#[derive(Clone)]
pub struct Question {
    pub name: String,
    /// We are putting a folder (not a file), and whether what lies there is a folder.
    pub dir: bool,
    pub there_dir: bool,
    /// A folder both there and here: replacing means merging them.
    pub folders: bool,
    /// How many more conflicts follow this one.
    pub more: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Replace the file, merge the folders (matching files inside are replaced).
    Replace,
    /// Put it next to it under another name ("name 2").
    KeepBoth,
    Skip,
    Cancel,
}

/// How to put one item.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Overwrite {
    /// The name is free (or a new one was taken).
    No,
    /// Remove what lies there and put ours.
    Replace,
    /// A folder into a folder: the contents go on top.
    Merge,
}

pub type Job = Arc<IrqLock<Progress>>;

/// A plan step: create a folder or copy a file (`clear` — first remove what
/// lies at this place).
struct Step {
    src: String,
    dst: String,
    dir: bool,
    size: u64,
    clear: bool,
}

/// Walks a folder: the sizes are already in the listings, no extra questions are asked of the server,
/// which makes the decisive difference on a network folder with thousands of files.
fn plan(state: &Job, src: &str, dst: &str, out: &mut Vec<Step>) {
    out.push(Step { src: src.into(), dst: dst.into(), dir: true, size: 0, clear: false });
    for e in fs::list(src).unwrap_or_default() {
        if state.lock().cancel {
            return;
        }
        let (from, to) = (fs::resolve(src, &e.name), fs::resolve(dst, &e.name));
        if e.is_dir {
            plan(state, &from, &to, out);
        } else {
            let size = e.size as u64;
            let mut p = state.lock();
            p.total_files += 1;
            p.total_bytes += size;
            drop(p);
            out.push(Step { src: from, dst: to, dir: false, size, clear: false });
        }
    }
}

/// Starts copying (or moving) `sources` into folder `dest`. If a name there
/// is already taken, the progress window asks what to do. Everything that touches disks
/// and the network runs in the background: the progress window opens right away.
pub fn start(mode: Mode, sources: Vec<String>, dest: String) -> Result<Job, &'static str> {
    if sources.is_empty() {
        return Err("nothing to copy");
    }
    for s in &sources {
        if dest == *s || dest.starts_with(&format!("{s}/")) {
            return Err("can't put a folder inside itself");
        }
    }
    let job: Job = Arc::new(IrqLock::new(Progress {
        mode,
        dest: dest.clone(),
        total_bytes: 0,
        done_bytes: 0,
        total_files: 0,
        done_files: 0,
        current: String::new(),
        started: interrupts::ticks(),
        finished_at: None,
        error: None,
        cancel: false,
        counting: true,
        question: None,
        answer: None,
    }));
    let state = job.clone();
    task::spawn("transfer", None, move || prepare(state, mode, sources, dest));
    wm::request(Request::Popup { title: title(mode).into(), app: Box::new(ProgressWindow::new(job.clone())), w: W, h: H });
    Ok(job)
}

/// Copying failed: the user already knows about a cancellation.
fn complain(error: &str) {
    if error != "Cancelled" {
        crate::audio::play(crate::audio::Sound::Error);
    }
}

/// Makes the plan, checks that everything fits, and starts copying.
fn prepare(state: Job, mode: Mode, sources: Vec<String>, dest: String) {
    let stop = |error: String| {
        complain(&error);
        let mut p = state.lock();
        p.error = Some(error);
        p.counting = false;
        p.finished_at = Some(interrupts::ticks());
    };
    // "From — to" pairs. Moving into the same folder — nothing to do.
    let sources: Vec<String> = sources.into_iter().filter(|s| fs::exists(s) && !(mode == Mode::Move && fs::parent(s) == dest)).collect();
    // A copy into the same folder always goes next to it, nothing to ask.
    let taken: Vec<bool> = sources.iter().map(|s| fs::parent(s) != dest && fs::exists(&fs::resolve(&dest, fs::name(s)))).collect();
    let mut left = taken.iter().filter(|t| **t).count();
    let mut for_all: Option<Choice> = None;
    let mut pairs: Vec<(String, String, Overwrite)> = Vec::new();
    for (s, taken) in sources.into_iter().zip(taken) {
        let target = fs::resolve(&dest, fs::name(&s));
        let choice = if !taken {
            Choice::KeepBoth
        } else {
            left -= 1;
            match for_all {
                Some(c) => c,
                None => {
                    let folders = fs::is_dir(&s) && fs::is_dir(&target);
                    let (dir, there_dir) = (fs::is_dir(&s), fs::is_dir(&target));
                    match ask(&state, Question { name: fs::name(&s).into(), dir, there_dir, folders, more: left }) {
                        Some((c, all)) => {
                            if all {
                                for_all = Some(c);
                            }
                            c
                        }
                        None => Choice::Cancel,
                    }
                }
            }
        };
        match choice {
            Choice::Cancel => return stop("Cancelled".into()),
            Choice::Skip => {}
            Choice::KeepBoth => {
                let name = fs::unique_name(&dest, fs::name(&s));
                pairs.push((s, fs::resolve(&dest, &name), Overwrite::No));
            }
            Choice::Replace => {
                let how = if fs::is_dir(&s) && fs::is_dir(&target) { Overwrite::Merge } else { Overwrite::Replace };
                pairs.push((s, target, how));
            }
        }
    }
    // Folders cannot be merged by renaming: then copy and delete.
    let rename_only = mode == Mode::Move && pairs.iter().all(|(s, d, how)| fs::same_volume(s, d) && *how != Overwrite::Merge);
    let mut steps = Vec::new();
    if !rename_only {
        for (s, d, how) in &pairs {
            state.lock().current = String::from(fs::name(s));
            let first = steps.len();
            if fs::is_dir(s) {
                plan(&state, s, d, &mut steps);
            } else {
                let size = fs::file_len(s).unwrap_or(0);
                let mut p = state.lock();
                p.total_files += 1;
                p.total_bytes += size;
                drop(p);
                steps.push(Step { src: s.clone(), dst: d.clone(), dir: false, size, clear: false });
            }
            if *how == Overwrite::Replace
                && let Some(step) = steps.get_mut(first)
            {
                step.clear = true;
            }
        }
        if state.lock().cancel {
            return stop("Cancelled".into());
        }
        let (bytes, largest) = (state.lock().total_bytes, steps.iter().map(|s| s.size).max().unwrap_or(0));
        // Right away, not after an hour of copying.
        if let Err(e) = fs::can_hold(&dest, bytes, largest) {
            return stop(format!("{e}: {} needed", size_text(bytes)));
        }
    }
    state.lock().counting = false;
    run(state, mode, pairs, steps);
}

/// Asks the progress window a question and waits for the answer. None — the window was closed or cancel was pressed.
fn ask(state: &Job, question: Question) -> Option<(Choice, bool)> {
    {
        let mut p = state.lock();
        p.answer = None;
        p.question = Some(question);
    }
    loop {
        {
            let mut p = state.lock();
            if p.cancel {
                p.question = None;
                return None;
            }
            if let Some(a) = p.answer.take() {
                p.question = None;
                return Some(a);
            }
        }
        task::sleep_until(interrupts::ticks() + HZ / 20);
    }
}

fn title(mode: Mode) -> &'static str {
    match mode {
        Mode::Copy => "Copying",
        Mode::Move => "Moving",
    }
}

/// Copies a file in pieces, the progress bar moves as the writing goes.
fn copy_file(state: &Job, step: &Step) -> fs::Result<()> {
    let mut seen = 0;
    fs::copy_file(&step.src, &step.dst, |done| {
        let mut s = state.lock();
        s.done_bytes += done - seen;
        seen = done;
        !s.cancel
    })
}

fn run(state: Job, mode: Mode, pairs: Vec<(String, String, Overwrite)>, steps: Vec<Step>) {
    let finish = |error: Option<String>| {
        if let Some(e) = &error {
            complain(e);
        }
        let mut p = state.lock();
        p.error = error;
        p.finished_at = Some(interrupts::ticks());
    };
    // A move within one volume is just a rename (instant).
    if mode == Mode::Move && pairs.iter().all(|(s, d, how)| fs::same_volume(s, d) && *how != Overwrite::Merge) {
        for (s, d, how) in &pairs {
            state.lock().current = fs::name(s).into();
            if *how == Overwrite::Replace
                && let Err(e) = fs::remove(d)
            {
                return finish(Some(format!("{}: {e}", fs::name(d))));
            }
            if let Err(e) = fs::rename(s, d) {
                return finish(Some(format!("{}: {e}", fs::name(s))));
            }
        }
        let mut p = state.lock();
        (p.done_files, p.done_bytes) = (p.total_files, p.total_bytes);
        drop(p);
        return finish(None);
    }
    for step in &steps {
        if state.lock().cancel {
            return finish(Some("Cancelled".into()));
        }
        let name = String::from(fs::name(&step.src));
        state.lock().current = name.clone();
        if step.clear
            && fs::exists(&step.dst)
            && let Err(e) = fs::remove(&step.dst)
        {
            return finish(Some(format!("{}: {e}", fs::name(&step.dst))));
        }
        // The folder already exists when merging: put things into it.
        let r = if step.dir {
            if fs::is_dir(&step.dst) { Ok(()) } else { fs::mkdir(&step.dst) }
        } else {
            copy_file(&state, step)
        };
        if let Err(e) = r {
            let cancelled = state.lock().cancel;
            return finish(Some(if cancelled { "Cancelled".into() } else { format!("{name}: {e}") }));
        }
        if !step.dir {
            state.lock().done_files += 1;
        }
        // Let the others work (the UI, saving to disk).
        task::yield_now();
    }
    if mode == Mode::Move {
        for (s, _, _) in &pairs {
            if let Err(e) = fs::remove(s) {
                return finish(Some(format!("copied, but {} was not removed: {e}", fs::name(s))));
            }
        }
    }
    finish(None);
}

/// A size for people: 512 B, 12.4 KB, 3.1 MB.
pub fn size_text(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f32 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    let tenths = (v * 10.0 + 0.5) as u64;
    if tenths >= 100 { format!("{} {}", tenths / 10, UNITS[u]) } else { format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[u]) }
}

/// The progress window: a bar with running stripes, how much is done and how much is left.
pub struct ProgressWindow {
    job: Job,
    /// The bar animation frame.
    phase: u64,
    cw: i32,
    ch: i32,
    closed: bool,
    /// "Do the same for the remaining conflicts".
    all: bool,
}

/// Answers to the taken-name question: label and choice. The main one is on the right.
fn choices(folders: bool) -> [(&'static str, Choice); 4] {
    [("Cancel", Choice::Cancel), ("Skip", Choice::Skip), ("Keep Both", Choice::KeepBoth), (if folders { "Merge" } else { "Replace" }, Choice::Replace)]
}

impl ProgressWindow {
    fn new(job: Job) -> Self {
        Self { job, phase: 0, cw: W, ch: H, closed: false, all: false }
    }

    /// Answer buttons from right to left.
    fn choice_buttons(&self, folders: bool) -> Vec<(Rect, &'static str, Choice)> {
        let mut x = self.cw - PAD;
        let y = self.ch - PAD - ui::BUTTON_H;
        let mut out = Vec::new();
        for (label, choice) in choices(folders).into_iter().rev() {
            let w = ui::button_width(label);
            x -= w;
            out.push((Rect::new(x, y, w, ui::BUTTON_H), label, choice));
            x -= 8;
        }
        out
    }

    fn all_switch(&self) -> Rect {
        Rect::new(PAD, self.ch - PAD - ui::BUTTON_H - 40, self.cw - 2 * PAD, 26)
    }

    fn answer(&mut self, choice: Choice) {
        let mut p = self.job.lock();
        if p.question.is_some() {
            p.answer = Some((choice, self.all));
        }
    }

    /// The taken-name question instead of the progress bar.
    fn draw_question(&self, c: &mut Canvas, a: Rect, q: &Question, dest: &str) {
        let saved = c.clip();
        c.set_clip(Rect::new(a.x + PAD, a.y, a.w - 2 * PAD, a.h).intersect(saved));
        c.text(a.x + PAD, a.y + 16, &format!("\u{201C}{}\u{201D} is already there", q.name), TEXT, FontId::SemiBold, 15.0);
        c.text(a.x + PAD, a.y + 40, &format!("in {dest}"), TEXT_DIM, FontId::Regular, 13.0);
        let lines: [&str; 2] = if q.folders {
            ["Merge puts the contents into the existing folder;", "files with the same names are replaced."]
        } else if q.dir {
            ["There is a file with this name. Replace it with", "the folder, keep both or skip it."]
        } else if q.there_dir {
            ["There is a folder with this name. Replace deletes", "it with everything inside. Keep both or skip it?"]
        } else {
            ["Replace it with the one you are copying,", "keep both (the new one gets a number) or skip it."]
        };
        for (i, line) in lines.iter().enumerate() {
            c.text(a.x + PAD, a.y + 74 + i as i32 * 20, line, TEXT, FontId::Regular, 13.0);
        }
        c.set_clip(saved);
        if q.more > 0 {
            let r = self.all_switch().offset(a.x, a.y);
            ui::switch(c, Rect::new(r.x, r.y + 2, 40, 22), self.all);
            let others = if q.more == 1 { "the other one".into() } else { format!("the other {}", q.more) };
            c.text(r.x + 52, r.y + 5, &format!("Do the same for {others}"), TEXT, FontId::Regular, 13.0);
        }
        for (r, label, choice) in self.choice_buttons(q.folders) {
            if choice == Choice::Replace {
                ui::accent_button(c, r.offset(a.x, a.y), label, true);
            } else {
                ui::button(c, r.offset(a.x, a.y), label, true);
            }
        }
    }

    fn button(&self) -> Rect {
        let label = if self.job.lock().finished_at.is_some() { "Close" } else { "Cancel" };
        let w = ui::button_width(label);
        Rect::new(self.cw - PAD - w, self.ch - PAD - ui::BUTTON_H, w, ui::BUTTON_H)
    }
}

impl App for ProgressWindow {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        let p = self.job.lock();
        if let Some(q) = p.question.clone() {
            let dest = p.dest.clone();
            drop(p);
            return self.draw_question(c, a, &q, &dest);
        }
        let done = p.finished_at.is_some();
        let verb = match (p.mode, done, &p.error) {
            (_, true, Some(_)) => "Stopped",
            (Mode::Copy, true, None) => "Copied",
            (Mode::Move, true, None) => "Moved",
            (Mode::Copy, false, _) => "Copying",
            (Mode::Move, false, _) => "Moving",
        };
        let files = if p.total_files == 1 { "1 file".into() } else { format!("{} files", p.total_files) };
        let heading = if p.counting { format!("Counting: {files}") } else { format!("{verb} {files}") };
        c.text(a.x + PAD, a.y + 16, &heading, TEXT, FontId::SemiBold, 15.0);
        let saved = c.clip();
        c.set_clip(Rect::new(a.x + PAD, a.y, a.w - 2 * PAD, a.h).intersect(saved));
        c.text(a.x + PAD, a.y + 40, &format!("to {}", p.dest), TEXT_DIM, FontId::Regular, 13.0);
        c.set_clip(saved);

        // The bar: the filled part and light stripes running over it.
        let bar = Rect::new(a.x + PAD, a.y + 74, a.w - 2 * PAD, BAR_H);
        c.fill_round(bar, BAR_RADIUS, 0xFFFFFF, 24);
        let frac = if done && p.error.is_none() {
            1000
        } else if p.total_bytes > 0 {
            (p.done_bytes * 1000 / p.total_bytes) as i32
        } else if p.total_files > 0 {
            (p.done_files * 1000 / p.total_files) as i32
        } else {
            0
        };
        let fill = Rect::new(bar.x, bar.y, (bar.w * frac / 1000).max(bar.h), bar.h);
        let color = if p.error.is_some() { 0xF87171 } else { ACCENT };
        c.fill_round(fill, BAR_RADIUS, color, 230);
        if !done {
            let shift = (self.phase * PHASE_SHIFT % 24) as i32;
            for (row, edge) in (0..).zip(BAR_EDGE) {
                // The stripes do not go beyond the rounded edges of the filled part.
                c.set_clip(Rect::new(fill.x + edge, bar.y + row, fill.w - 2 * edge, 1).intersect(saved));
                for k in (-2..fill.w / 12 + 2).map(|k| fill.x + k * 24 + shift) {
                    // A slanted stripe: shifted by a pixel with each row.
                    c.blend(Rect::new(k + row, bar.y + row, 10, 1), 0xFFFFFF, 60);
                }
            }
            c.set_clip(saved);
        }
        let pct = format!("{}%", frac / 10);
        let pw = font::measure(FontId::SemiBold, 13.0, &pct);
        c.text(a.x + a.w - PAD - pw, a.y + 96, &pct, TEXT, FontId::SemiBold, 13.0);
        c.text(a.x + PAD, a.y + 96, &format!("{} of {}", size_text(p.done_bytes), size_text(p.total_bytes)), TEXT, FontId::Regular, 13.0);

        let now = interrupts::ticks();
        let elapsed = (p.finished_at.unwrap_or(now) - p.started).max(1);
        let speed = p.done_bytes * HZ / elapsed;
        let left_files = p.total_files - p.done_files;
        let left_bytes = p.total_bytes - p.done_bytes;
        let info = if let Some(e) = &p.error {
            (e.clone(), 0xF87171)
        } else if done {
            (format!("Done in {}.{} s", elapsed / HZ, elapsed % HZ * 10 / HZ), 0x4ADE80)
        } else {
            let eta = if speed > 0 { format!(", about {} s left", left_bytes / speed + 1) } else { String::new() };
            (if p.counting { format!("Found {} so far", size_text(p.total_bytes)) } else { format!("Remaining: {left_files} files, {}{eta}", size_text(left_bytes)) }, TEXT_DIM)
        };
        c.text(a.x + PAD, a.y + 128, &info.0, info.1, FontId::Regular, 13.0);
        if !done {
            c.set_clip(Rect::new(a.x + PAD, a.y, a.w - 2 * PAD, a.h).intersect(saved));
            c.text(a.x + PAD, a.y + 150, &format!("{}  \u{00B7}  {}/s", p.current, size_text(speed)), TEXT_DIM, FontId::Regular, 12.5);
            c.set_clip(saved);
        }
        drop(p);
        let b = self.button();
        let label = if done { "Close" } else { "Cancel" };
        ui::button(c, b.offset(a.x, a.y), label, true);
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if !pressed || buttons & LEFT == 0 {
            return false;
        }
        let question = self.job.lock().question.clone();
        if let Some(q) = question {
            if q.more > 0 && self.all_switch().contains(x, y) {
                self.all = !self.all;
                return true;
            }
            if let Some((_, _, choice)) = self.choice_buttons(q.folders).into_iter().find(|(r, _, _)| r.contains(x, y)) {
                self.answer(choice);
                return true;
            }
            return false;
        }
        if !self.button().contains(x, y) {
            return false;
        }
        let mut p = self.job.lock();
        if p.finished_at.is_some() {
            self.closed = true;
        } else {
            p.cancel = true;
        }
        true
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        // On a question: Esc — cancel, Enter — keep both (nothing is lost).
        if self.job.lock().question.is_some() {
            match ch {
                0x1B => self.answer(Choice::Cancel),
                b'\n' => self.answer(Choice::KeepBoth),
                _ => return false,
            }
            return true;
        }
        if ch == 0x1B || ch == b'\n' {
            let mut p = self.job.lock();
            if p.finished_at.is_some() { self.closed = true } else { p.cancel = true }
            return true;
        }
        false
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let phase = ctx.ticks / PHASE_TICKS;
        let changed = phase != self.phase;
        self.phase = phase;
        // This is also how the question asked by the copy thread appears.
        changed
    }

    fn finished(&self) -> bool {
        // A successful operation closes the window by itself (half a second after "100%"),
        // an error is shown until it is closed.
        let p = self.job.lock();
        self.closed || p.error.is_none() && p.finished_at.is_some_and(|t| interrupts::ticks() > t + HZ / 2)
    }
}
