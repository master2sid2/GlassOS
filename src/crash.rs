//! What happened to the previous boot: a report after a hang, a panic or a sudden
//! restart.
//!
//! On every boot the kernel writes to firmware memory which build is running and whether it is
//! a trial one, marked "not finished". A normal reboot or shutdown sets
//! "finished". The next boot looks at what is left of the previous one:
//! - whether the watchdog fired (a bit in TCO — survives a board reset);
//! - a note from the hang detector or a panic (`bootvars::take_note`);
//! - the "not finished" mark: power was lost, reset was pressed or the board restarted by itself.
//!
//! The report goes to /system/crashes/<time>.txt (with the trial kernel log, if there is one),
//! and after login it is shown in a window.

use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect};
use crate::sync::IrqLock;
use crate::wm::{self, App, Ctx, Request};
use crate::{bootvars, fs, kprintln, task, time};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The build of this kernel: the commit and the time (build.rs).
pub const BUILD: &str = env!("GLASSOS_BUILD");

const DIR: &str = "/system/crashes";
const TRIAL_LOG: &str = "/system/trial.log";

/// The boot as it was recorded in firmware memory.
struct Run {
    build: String,
    trial: bool,
    clean: bool,
    started: i64,
}

impl Run {
    fn encode(&self) -> String {
        format!("{}|{}|{}|{}", self.build, self.trial as u8, self.clean as u8, self.started)
    }

    fn decode(text: &str) -> Option<Run> {
        let mut parts = text.split('|');
        let build = parts.next()?.to_string();
        let trial = parts.next()? == "1";
        let clean = parts.next()? == "1";
        let started = parts.next()?.parse().ok()?;
        Some(Run { build, trial, clean, started })
    }
}

pub struct Report {
    title: String,
    lines: Vec<String>,
    file: Option<String>,
}

static PENDING: IrqLock<Option<Report>> = IrqLock::new(None);
static THIS_RUN: IrqLock<Option<Run>> = IrqLock::new(None);

/// At boot (files and the clock are available): deal with the previous boot and mark this one.
pub fn check() {
    let previous = bootvars::run_record().and_then(|t| Run::decode(&t));
    let note = bootvars::take_note();
    let watchdog = crate::watchdog::fired_last_time();
    let trial = crate::net::remote::trial();
    let run = Run { build: BUILD.into(), trial, clean: false, started: time::unix() };
    bootvars::set_run_record(&run.encode());
    *THIS_RUN.lock() = Some(run);
    kprintln!("build {BUILD}{}", if trial { " (trial kernel)" } else { "" });
    if let Some(n) = &note {
        kprintln!("the previous start: {n}");
    }
    let unclean = previous.as_ref().is_some_and(|p| !p.clean);
    if note.is_none() && !watchdog && !unclean {
        return;
    }
    let panic = note.as_deref().is_some_and(|n| n.starts_with("panic") || n.starts_with("exception"));
    // There is a note, but the previous boot ended normally: the stall resolved by itself.
    let recovered = note.is_some() && !watchdog && !unclean && !panic;
    let title = if panic {
        "The system stopped with a kernel error"
    } else if recovered {
        "The previous session had a problem"
    } else if watchdog || note.is_some() {
        "The system hung and restarted by itself"
    } else {
        "The previous session ended unexpectedly"
    };
    let mut lines = Vec::new();
    match (&note, watchdog) {
        (Some(n), _) => lines.push(format!("What happened: {}", describe(n))),
        (None, true) => lines.push("What happened: the kernel stopped answering; the watchdog timer restarted the computer after 30-60 seconds.".into()),
        (None, false) => lines.push("What happened: the kernel did not finish properly. The power may have gone off or the reset button was pressed; it could also have been a hang or a processor fault.".into()),
    }
    if recovered {
        lines.push("It got going again by itself; the computer was then restarted normally.".into());
    }
    if watchdog && note.is_some() {
        lines.push("The watchdog timer restarted the computer after 30-60 seconds.".into());
    }
    if let Some(p) = &previous {
        if p.started > 0 {
            lines.push(format!("The previous start began at {}.", time::stamp(p.started)));
        }
        if p.trial && !trial {
            lines.push(format!("It ran a trial kernel (build {}). The computer went back to the main kernel (build {BUILD}).", p.build));
        } else if p.build != BUILD {
            lines.push(format!("Kernel then: build {}. Kernel now: build {BUILD}.", p.build));
        } else {
            lines.push(format!("Kernel: build {BUILD}."));
        }
    }
    let trial_log = previous.as_ref().filter(|p| p.trial).and_then(|_| task::as_system(|| fs::read(TRIAL_LOG).ok()));
    if trial_log.is_some() {
        lines.push(format!("The trial kernel's log (first 3 minutes) is in {TRIAL_LOG} and in the report."));
    }
    let file = save(title, &lines, trial_log.as_deref());
    if let Some(f) = &file {
        lines.push(format!("Report: {f}"));
    }
    for l in &lines {
        kprintln!("crash report: {l}");
    }
    *PENDING.lock() = Some(Report { title: title.into(), lines, file });
}

/// The detector's note — made clearer for a person.
fn describe(note: &str) -> String {
    if let Some(rest) = note.strip_prefix("panic: ") {
        return format!("kernel panic — {rest}");
    }
    if let Some(rest) = note.strip_prefix("exception: ") {
        return format!("the processor hit an error in the kernel — {rest}");
    }
    if let Some(rest) = note.strip_prefix("stuck: ") {
        return format!("a processor waited for a lock that never came free (deadlock) — {rest}");
    }
    note.into()
}

fn save(title: &str, lines: &[String], log: Option<&[u8]>) -> Option<String> {
    let now = time::now();
    let path = format!("{DIR}/{}-{:02}-{:02}_{:02}-{:02}-{:02}.txt", now.year, now.month, now.day, now.hour, now.min, now.sec);
    let mut text = format!("{title}\n\n");
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    if let Some(log) = log {
        text.push_str("\n---- log of the trial kernel ----\n");
        text.push_str(&String::from_utf8_lossy(log));
    }
    task::as_system(|| {
        if !fs::exists(DIR) {
            let _ = fs::mkdir(DIR);
        }
        fs::write(&path, text.as_bytes()).ok()?;
        let _ = fs::sync();
        Some(path)
    })
}

/// Change this boot's record.
fn update_run(change: impl FnOnce(&mut Run)) {
    let text = THIS_RUN.lock().as_mut().map(|r| {
        change(r);
        r.encode()
    });
    if let Some(t) = text {
        bootvars::set_run_record(&t);
    }
}

/// The kernel died (a panic, a CPU exception): stop the other cores, write
/// a note for the report and stop kicking the watchdog — in 30–60 s the board
/// resets, and after login the report is shown. Without waiting for locks.
pub fn fatal(note: &str) {
    if STOPPING.swap(true, core::sync::atomic::Ordering::SeqCst) {
        return;
    }
    crate::watchdog::give_up();
    crate::smp::stop_others();
    crate::sync::claim_note();
    let uptime = crate::interrupts::ticks() / crate::interrupts::HZ;
    let mut buf = [0u8; 600];
    let mut len = 0;
    for part in [note.as_bytes(), b"; uptime ", u64_text(uptime, &mut [0u8; 20]), b" s"] {
        let n = part.len().min(buf.len() - len);
        buf[len..len + n].copy_from_slice(&part[..n]);
        len += n;
    }
    bootvars::save_note(&buf[..len]);
}

fn u64_text(mut v: u64, out: &mut [u8; 20]) -> &[u8] {
    let mut i = out.len();
    loop {
        i -= 1;
        out[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            return &out[i..];
        }
    }
}

static STOPPING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The kernel is dying: an NMI from it is an order to stop.
pub fn stopping() -> bool {
    STOPPING.load(core::sync::atomic::Ordering::Relaxed)
}

/// Before a normal reboot and shutdown: this boot ended properly.
pub fn mark_clean() {
    update_run(|r| r.clean = true);
}

/// The trial kernel was kept as the main one: if it hangs later, there will be no rollback — the
/// report says so.
pub fn kept() {
    update_run(|r| r.trial = false);
}

/// When to show the report (ticks); 0 — not needed.
static DUE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Login: show the report a little later, once the previous session's windows are open
/// (Lua programs open their windows themselves when they start) — otherwise they would cover it.
pub fn arm() {
    if PENDING.lock().is_some() {
        DUE.store(crate::interrupts::ticks() + 3 * crate::interrupts::HZ, core::sync::atomic::Ordering::Relaxed);
    }
}

/// From the main loop: whether it is time to show the report.
pub fn poll() {
    let due = DUE.load(core::sync::atomic::Ordering::Relaxed);
    if due != 0 && crate::interrupts::ticks() >= due {
        DUE.store(0, core::sync::atomic::Ordering::Relaxed);
        show();
    }
}

/// Show the report (once).
fn show() {
    let Some(report) = PENDING.lock().take() else { return };
    let width = W - 2 * PAD;
    let mut wrapped = Vec::new();
    for l in &report.lines {
        wrapped.extend(font::wrap(FontId::Regular, 14.0, l, width, 6));
        wrapped.push(String::new());
    }
    let h = 30 + 46 + wrapped.len() as i32 * LINE + crate::apps::ui::BUTTON_H + 24;
    wm::request(Request::Popup { title: "What happened".into(), app: Box::new(CrashWindow { report, wrapped, open: Rect::new(0, 0, 0, 0), close: Rect::new(0, 0, 0, 0), done: false }), w: W, h });
}

const W: i32 = 560;
const PAD: i32 = 22;
const LINE: i32 = 21;

struct CrashWindow {
    report: Report,
    wrapped: Vec<String>,
    open: Rect,
    close: Rect,
    done: bool,
}

impl App for CrashWindow {
    fn background(&self) -> Color {
        0x1C1F2E
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        let (x, mut y) = (a.x + PAD, a.y + 16);
        c.text(x, y, &self.report.title, 0xFF9A6A, FontId::SemiBold, 19.0);
        y += 40;
        for l in &self.wrapped {
            c.text(x, y, l, 0xC3C8D8, FontId::Regular, 14.0);
            y += LINE;
        }
        let row = crate::apps::ui::button_row(&["Open report", "Close"], PAD, a.h - crate::apps::ui::BUTTON_H - 16);
        (self.open, self.close) = (row[0], row[1]);
        crate::apps::ui::button(c, self.open.offset(a.x, a.y), "Open report", self.report.file.is_some());
        crate::apps::ui::button(c, self.close.offset(a.x, a.y), "Close", true);
    }

    fn mouse(&mut self, x: i32, y: i32, _buttons: u8, pressed: bool) -> bool {
        if !pressed {
            return false;
        }
        if self.open.contains(x, y) {
            if let Some(f) = &self.report.file {
                wm::request(Request::OpenFile(f.clone()));
                self.done = true;
            }
        } else if self.close.contains(x, y) {
            self.done = true;
        }
        false
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        if ch == 0x1B || ch == b'\n' || ch == b'\r' {
            self.done = true;
        }
        false
    }

    fn finished(&self) -> bool {
        self.done
    }
}
