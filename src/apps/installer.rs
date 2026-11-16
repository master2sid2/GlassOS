//! The "Install GlassOS" window: choose a SATA disk, then a place on it (the whole disk,
//! free space or one of the partitions), confirm and install the system
//! (`crate::install`) in a background thread.

use super::ui;
use crate::font::FontId;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::install::{self, Choice, Installed, Place, Target};
use crate::sync::YieldLock;
use crate::wm::{ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const PAD: i32 = 22;
const ROW_H: i32 = 52;
const ROW_GAP: i32 = 6;
const LIST_Y: i32 = 80;
const DANGER: Color = 0xF87171;

/// The installation progress: the step, the percent, the result (None — still running).
type Progress = Arc<YieldLock<(String, u32, Option<Result<Installed, String>>)>>;

enum Stage {
    /// Which disk.
    Disk,
    /// Where on the disk.
    Place,
    Confirm,
    Working(Progress),
    Done(Result<Installed, String>),
}

pub struct Installer {
    disks: Vec<Target>,
    disk: Option<usize>,
    choices: Vec<Choice>,
    choice: Option<usize>,
    stage: Stage,
    /// The first visible row of the list (scrolling).
    top: usize,
    /// When the window opened: the Enter that opened it from the terminal is not a press here.
    opened: u64,
    cw: i32,
    ch: i32,
    closed: bool,
}

impl Installer {
    pub fn new(cw: i32, ch: i32) -> Self {
        let disks = install::targets();
        // If there is one suitable disk — it is selected right away.
        let free: Vec<usize> = (0..disks.len()).filter(|&i| !disks[i].system).collect();
        let disk = (free.len() == 1).then(|| free[0]);
        let opened = crate::interrupts::ticks();
        Self { disks, disk, choices: Vec::new(), choice: None, stage: Stage::Disk, top: 0, opened, cw, ch, closed: false }
    }

    fn target(&self) -> Option<&Target> {
        self.disk.and_then(|i| self.disks.get(i))
    }

    /// How many list rows fit above the buttons.
    fn visible(&self) -> usize {
        ((self.ch - LIST_Y - PAD - ui::BUTTON_H - 12) / (ROW_H + ROW_GAP)).max(1) as usize
    }

    fn rows(&self) -> usize {
        match self.stage {
            Stage::Disk => self.disks.len(),
            Stage::Place => self.choices.len(),
            _ => 0,
        }
    }

    /// List row `i` (with scrolling); None — not visible.
    fn row(&self, i: usize) -> Option<Rect> {
        (i >= self.top && i < self.top + self.visible()).then(|| Rect::new(PAD, LIST_Y + (i - self.top) as i32 * (ROW_H + ROW_GAP), self.cw - 2 * PAD, ROW_H))
    }

    /// The buttons at the bottom right: the main one last.
    fn buttons(&self, labels: &[&str]) -> Vec<Rect> {
        let width: i32 = labels.iter().map(|l| ui::button_width(l) + 6).sum::<i32>() - 6;
        ui::button_row(labels, self.cw - PAD - width, self.ch - PAD - ui::BUTTON_H)
    }

    fn labels(&self) -> &'static [&'static str] {
        match &self.stage {
            Stage::Disk => &["Close", "Next"],
            Stage::Place => &["Back", "Next"],
            Stage::Confirm => &["Back", "Install"],
            Stage::Working(_) => &[],
            Stage::Done(Ok(_)) => &["Close", "Restart"],
            Stage::Done(Err(_)) => &["Close", "Back"],
        }
    }

    /// The main button is enabled: on the choice steps — once something is chosen.
    fn ready(&self) -> bool {
        match self.stage {
            Stage::Disk => self.target().is_some_and(|t| !t.system),
            Stage::Place => self.choice.is_some(),
            _ => true,
        }
    }

    fn start(&mut self) {
        let (Some(t), Some(c)) = (self.target(), self.choice.and_then(|i| self.choices.get(i))) else { return };
        let (disk, place) = (t.disk, c.place);
        let progress: Progress = Arc::new(YieldLock::new((String::from("Starting"), 0, None)));
        let shared = progress.clone();
        crate::task::spawn("install", None, move || {
            let result = install::install(disk, place, &mut |step, pct| {
                let mut p = shared.lock();
                (p.0, p.1) = (String::from(step), pct);
            });
            if let Err(e) = &result {
                crate::kprintln!("install: failed: {e}");
            }
            shared.lock().2 = Some(result);
        });
        self.stage = Stage::Working(progress);
    }

    fn go(&mut self, stage: Stage) {
        self.stage = stage;
        self.top = 0;
    }

    fn press(&mut self, label: &str) {
        if label != "Back" && label != "Close" && !self.ready() {
            return;
        }
        match (label, &self.stage) {
            ("Close", _) => self.closed = true,
            ("Next", Stage::Disk) => {
                self.choices = self.target().map_or(Vec::new(), |t| t.choices());
                // One option (an empty disk) — selected right away.
                self.choice = (self.choices.len() == 1).then_some(0);
                self.go(Stage::Place);
            }
            ("Next", Stage::Place) => self.go(Stage::Confirm),
            ("Back", Stage::Place) => self.go(Stage::Disk),
            ("Back", Stage::Confirm) => self.go(Stage::Place),
            ("Back", Stage::Done(_)) => {
                self.disks = install::targets();
                self.disk = self.disk.filter(|&i| i < self.disks.len());
                self.go(Stage::Disk);
            }
            ("Install", Stage::Confirm) => self.start(),
            ("Restart", _) => crate::wm::request(crate::wm::Request::Restart),
            _ => {}
        }
    }

    /// Text wrapped at words to width `w`; returns the y below the last line.
    fn wrap(c: &mut Canvas, x: i32, mut y: i32, w: i32, text: &str, color: Color) -> i32 {
        let mut line = String::new();
        for word in text.split(' ') {
            let next = if line.is_empty() { String::from(word) } else { format!("{line} {word}") };
            if crate::font::measure(FontId::Regular, 14.0, &next) > w && !line.is_empty() {
                c.text(x, y, &line, color, FontId::Regular, 14.0);
                y += 21;
                line = String::from(word);
            } else {
                line = next;
            }
        }
        if !line.is_empty() {
            c.text(x, y, &ui::fit(&line, FontId::Regular, 14.0, w), color, FontId::Regular, 14.0);
            y += 21;
        }
        y
    }

    /// A list row: a title (and the size on the right), with a description below it.
    fn draw_row(c: &mut Canvas, r: Rect, chosen: bool, enabled: bool, title: &str, right: &str, note: &str, note_color: Color) {
        c.fill_round(r, 9, if chosen { ACCENT } else { 0xFFFFFF }, if chosen { 45 } else { 14 });
        c.stroke_round(r, 9, if chosen { ACCENT } else { 0xFFFFFF }, if chosen { 180 } else { 30 });
        let color = if enabled { TEXT } else { TEXT_DIM };
        let rw = if right.is_empty() { 0 } else { crate::font::measure(FontId::Regular, 14.0, right) + 12 };
        c.text(r.x + 14, r.y + 8, &ui::fit(title, FontId::SemiBold, 14.0, r.w - 28 - rw), color, FontId::SemiBold, 14.0);
        if !right.is_empty() {
            c.text(r.x + r.w - 14 - rw + 12, r.y + 8, right, color, FontId::Regular, 14.0);
        }
        c.text(r.x + 14, r.y + 29, &ui::fit(note, FontId::Regular, 13.0, r.w - 28), note_color, FontId::Regular, 13.0);
    }
}

impl App for Installer {
    fn background(&self) -> Color {
        0x1C1F2E
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        let (x, w) = (a.x + PAD, a.w - 2 * PAD);
        c.text(x, a.y + 16, "Install GlassOS", ACCENT, FontId::SemiBold, 22.0);
        match &self.stage {
            Stage::Disk => {
                c.text(x, a.y + 52, "Choose the disk.", TEXT_DIM, FontId::Regular, 14.0);
                if self.disks.is_empty() {
                    Self::wrap(c, x, a.y + LIST_Y + 6, w, "No SATA disks found. USB and NVMe disks are not supported yet; in the firmware setup SATA should be in AHCI mode.", TEXT);
                }
                for (i, t) in self.disks.iter().enumerate() {
                    let Some(r) = self.row(i) else { continue };
                    let note = if t.system { String::from("GlassOS started from this disk") } else { format!("Now: {}", t.contents) };
                    Self::draw_row(c, r.offset(a.x, a.y), self.disk == Some(i), !t.system, &t.model, &t.size(), &note, TEXT_DIM);
                }
            }
            Stage::Place => {
                let model = self.target().map_or(String::new(), |t| format!("{}, {}", t.model, t.size()));
                c.text(x, a.y + 52, &ui::fit(&format!("Where to put GlassOS on {model}?"), FontId::Regular, 14.0, w), TEXT_DIM, FontId::Regular, 14.0);
                for (i, ch) in self.choices.iter().enumerate() {
                    let Some(r) = self.row(i) else { continue };
                    let (note, color) = match &ch.warning {
                        Some(warn) if matches!(ch.place, Place::Part { .. }) => (warn.clone(), DANGER),
                        _ => (ch.detail.clone(), TEXT_DIM),
                    };
                    Self::draw_row(c, r.offset(a.x, a.y), self.choice == Some(i), true, &ch.title, "", &note, color);
                }
            }
            Stage::Confirm => {
                if let Some(ch) = self.choice.and_then(|i| self.choices.get(i)) {
                    let mut y = a.y + 56;
                    y = Self::wrap(c, x, y, w, &ch.question, TEXT) + 6;
                    y = Self::wrap(c, x, y, w, &ch.detail, TEXT_DIM) + 6;
                    if let Some(warn) = &ch.warning {
                        y = Self::wrap(c, x, y, w, &format!("{warn} It will be lost."), DANGER) + 6;
                    }
                    Self::wrap(c, x, y, w, "GlassOS is also added to the firmware boot list, first.", TEXT_DIM);
                }
            }
            Stage::Working(p) => {
                let (step, pct) = {
                    let p = p.lock();
                    (p.0.clone(), p.1)
                };
                c.text(x, a.y + 70, &step, TEXT, FontId::Regular, 14.0);
                let bar = Rect::new(x, a.y + 100, w, 10);
                c.fill_round(bar, 5, 0xFFFFFF, 25);
                let done = Rect::new(bar.x, bar.y, (bar.w * pct.min(100) as i32 / 100).max(bar.h), bar.h);
                c.fill_round(done, 5, ACCENT, 230);
                c.text(x, a.y + 124, "Do not turn the computer off.", TEXT_DIM, FontId::Regular, 13.0);
            }
            Stage::Done(Ok(done)) => {
                let model = self.target().map_or(String::new(), |t| t.model.clone());
                let what = if done.updated { "updated" } else { "installed" };
                let mut y = Self::wrap(c, x, a.y + 56, w, &format!("GlassOS is {what} on {model}."), TEXT) + 6;
                y = match &done.boot_entry {
                    Ok(n) => Self::wrap(c, x, y, w, &format!("It is first in the firmware boot list (Boot{n:04X}). Remove the USB stick and restart."), TEXT_DIM),
                    Err(e) => {
                        let y = Self::wrap(c, x, y, w, &format!("Could not add it to the firmware boot list: {e}."), DANGER);
                        Self::wrap(c, x, y + 4, w, "Remove the USB stick, restart and choose the disk in the boot menu (F12, F11 or Esc on most computers).", TEXT_DIM)
                    }
                };
                Self::wrap(c, x, y + 6, w, "If the computer still says there is no boot device, switch the firmware setup to UEFI boot (or put UEFI first) and turn Fast Boot off.", TEXT_DIM);
            }
            Stage::Done(Err(e)) => {
                let y = Self::wrap(c, x, a.y + 56, w, "Installation failed:", TEXT);
                Self::wrap(c, x, y + 4, w, e, DANGER);
            }
        }
        // Scrolling: "N more" below the list.
        let (rows, visible) = (self.rows(), self.visible());
        if rows > visible {
            let more = format!("{}\u{2013}{} of {rows}, scroll for more", self.top + 1, (self.top + visible).min(rows));
            c.text(x, a.y + a.h - PAD - ui::BUTTON_H + 6, &more, TEXT_DIM, FontId::Regular, 13.0);
        }
        let labels = self.labels();
        let ready = self.ready();
        for (i, (r, label)) in self.buttons(labels).into_iter().zip(labels).enumerate() {
            let r = r.offset(a.x, a.y);
            if i + 1 == labels.len() { ui::accent_button(c, r, label, ready) } else { ui::button(c, r, label, true) }
        }
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if !pressed || buttons & LEFT == 0 {
            return false;
        }
        let labels = self.labels();
        if let Some(label) = self.buttons(labels).into_iter().zip(labels).find(|(r, _)| r.contains(x, y)).map(|(_, l)| *l) {
            self.press(label);
            return true;
        }
        let Some(i) = (0..self.rows()).find(|&i| self.row(i).is_some_and(|r| r.contains(x, y))) else { return false };
        match self.stage {
            Stage::Disk if !self.disks[i].system => self.disk = Some(i),
            Stage::Place => self.choice = Some(i),
            _ => return false,
        }
        true
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let max = self.rows().saturating_sub(self.visible());
        let top = (self.top as i32 + lines.signum()).clamp(0, max as i32) as usize;
        let changed = top != self.top;
        self.top = top;
        changed
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        if crate::interrupts::ticks() < self.opened + crate::interrupts::HZ / 2 {
            return false;
        }
        match (ch, &self.stage) {
            (0x1B, Stage::Disk) => self.press("Close"),
            (0x1B, Stage::Place | Stage::Confirm) => self.press("Back"),
            (b'\n', Stage::Disk | Stage::Place) => self.press("Next"),
            _ => return false,
        }
        true
    }

    fn tick(&mut self, _ctx: &Ctx) -> bool {
        if let Stage::Working(p) = &self.stage {
            let result = p.lock().2.take();
            if let Some(r) = result {
                self.stage = Stage::Done(r);
            }
            return true;
        }
        false
    }

    fn finished(&self) -> bool {
        self.closed
    }

    fn ask_close(&mut self) -> bool {
        !matches!(self.stage, Stage::Working(_))
    }
}
