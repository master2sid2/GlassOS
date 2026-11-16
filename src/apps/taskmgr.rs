//! The task manager: the CPU load with a graph, memory, disk and the list of windows
//! and kernel threads with their CPU and memory (the list scrolls with the wheel); the selected task can be ended.

use super::transfer::size_text;
use super::ui::{self, SCROLL_W, ScrollBar};
use crate::font::{self, FontId};
use crate::fs;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::{KEY_PAGE_DOWN, KEY_PAGE_UP, LEFT};
use crate::interrupts::HZ;
use crate::stats::{self, HISTORY};
use crate::wm::{self, ACCENT, App, Ctx, Request, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const BG: Color = 0x1A1D2B;
const PAD: i32 = 16;
const CARD_H: i32 = 118;
const ROW_H: i32 = 30;
const LIST_Y: i32 = PAD + CARD_H + 44;
const FOOTER_H: i32 = 48;
/// The name colour of kernel threads: it is immediately visible what is the system and what are programs.
const SYSTEM_TEXT: Color = 0xD9B36C;

pub struct TaskManager {
    /// The selected task — by window id, so the selection does not "slide" on refresh.
    selected: Option<u64>,
    seen: u64,
    /// List rows from the last draw: (rectangle in client coordinates, window id).
    rows: Vec<(Rect, u64)>,
    end_button: Rect,
    /// How many list rows are scrolled by the wheel and how many rows fit.
    offset: usize,
    visible: usize,
    total: usize,
    /// The scroll bar from the last draw (in client coordinates) and where the thumb
    /// was grabbed while it is dragged.
    bar: ScrollBar,
    thumb_grab: Option<i32>,
}

impl TaskManager {
    pub fn new() -> Self {
        let bar = ScrollBar { track: Rect::new(0, 0, 0, 0), len: 0, rows: 0, top: 0 };
        Self { selected: None, seen: 0, rows: Vec::new(), end_button: Rect::new(0, 0, 0, 0), offset: 0, visible: 0, total: 0, bar, thumb_grab: None }
    }
}

fn bytes(b: usize) -> String {
    size_text(b as u64)
}

fn percent(tenths: u32) -> String {
    format!("{}.{}%", tenths / 10, tenths % 10)
}

fn card(c: &mut Canvas, r: Rect, title: &str) {
    c.fill_round(r, 12, 0xFFFFFF, 14);
    c.stroke_round(r, 12, 0xFFFFFF, 24);
    c.text(r.x + 14, r.y + 12, title, TEXT_DIM, FontId::Regular, 12.0);
}

fn bar(c: &mut Canvas, r: Rect, value: u64, max: u64) {
    c.fill_round(r, r.h / 2, 0x3A3F55, 256);
    let w = (r.w as u64 * value.min(max) / max.max(1)) as i32;
    if w > 0 {
        c.fill_round(Rect::new(r.x, r.y, w.max(r.h), r.h), r.h / 2, ACCENT, 256);
    }
}

/// CPU load bars for the last HISTORY seconds (new ones on the right).
fn sparkline(c: &mut Canvas, r: Rect, history: &[u32]) {
    c.fill(Rect::new(r.x, r.y + r.h, r.w, 1), 0x3A3F55);
    let bw = (r.w / HISTORY as i32).max(1);
    let x0 = r.x + r.w - bw * history.len() as i32;
    for (i, &v) in history.iter().enumerate() {
        let h = ((v as i32 * r.h) / 1000).max(1);
        let x = x0 + i as i32 * bw;
        c.blend(Rect::new(x, r.y + r.h - h, bw - 1, h), ACCENT, 110);
        c.fill(Rect::new(x, r.y + r.h - h, bw - 1, 1), ACCENT);
    }
}

impl App for TaskManager {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, ctx: &Ctx) {
        let (tasks, history) = {
            let s = stats::SNAPSHOT.lock();
            (s.tasks.clone(), s.cpu_history.iter().copied().collect::<Vec<u32>>())
        };
        let cpu = history.last().copied().unwrap_or(0);

        // Cards: CPU, memory, system.
        let cw = (a.w - 2 * PAD - 2 * 12) / 3;
        let cards: Vec<Rect> = (0..3).map(|i| Rect::new(a.x + PAD + i * (cw + 12), a.y + PAD, cw, CARD_H)).collect();

        card(c, cards[0], "CPU");
        if let Some(now) = crate::power::reading().short() {
            let w = font::measure(FontId::Regular, 12.0, &now);
            c.text(cards[0].x + cw - 14 - w, cards[0].y + 12, &now, TEXT_DIM, FontId::Regular, 12.0);
        }
        c.text(cards[0].x + 14, cards[0].y + 30, &percent(cpu), TEXT, FontId::SemiBold, 24.0);
        let spark = Rect::new(cards[0].x + 14, cards[0].y + 66, cw - 28, 38);
        sparkline(c, spark, &history);

        let (used, free) = crate::heap_stats();
        let total = used + free;
        card(c, cards[1], "Memory");
        c.text(cards[1].x + 14, cards[1].y + 30, &bytes(used), TEXT, FontId::SemiBold, 24.0);
        c.text(cards[1].x + 14, cards[1].y + 64, &format!("of {}", bytes(total)), TEXT_DIM, FontId::Regular, 12.0);
        bar(c, Rect::new(cards[1].x + 14, cards[1].y + 90, cw - 28, 8), used as u64, total as u64);

        card(c, cards[2], "System");
        let up = ctx.ticks / HZ;
        let disk = match fs::disk_usage() {
            Some((used, cap)) => format!("{} / {}", bytes(used as usize), bytes(cap as usize)),
            None => "none".into(),
        };
        let lines = [
            ("Uptime", format!("{}h {:02}m {:02}s", up / 3600, up / 60 % 60, up % 60)),
            ("Disk", disk),
            ("Tasks", format!("{}", tasks.len())),
            ("CPUs", format!("{}", crate::smp::cpu_count())),
        ];
        for (i, (k, v)) in lines.iter().enumerate() {
            let y = cards[2].y + 30 + i as i32 * 21;
            c.text(cards[2].x + 14, y, k, TEXT_DIM, FontId::Regular, 13.0);
            let w = font::measure(FontId::Regular, 13.0, v);
            c.text(cards[2].x + cw - 14 - w, y, v, TEXT, FontId::Regular, 13.0);
        }

        // The task table.
        let (x0, x1) = (a.x + PAD, a.x + a.w - PAD);
        let col_type = x0 + (x1 - x0) * 46 / 100;
        let col_cpu = x0 + (x1 - x0) * 74 / 100;
        let head_y = a.y + LIST_Y - 24;
        c.text(x0 + 10, head_y, "Name", TEXT_DIM, FontId::Regular, 12.0);
        c.text(col_type, head_y, "Type", TEXT_DIM, FontId::Regular, 12.0);
        let right = |c: &mut Canvas, x: i32, y: i32, s: &str, color: Color, size: f32| {
            let w = font::measure(FontId::Regular, size, s);
            c.text(x - w, y, s, color, FontId::Regular, size);
        };
        right(c, col_cpu, head_y, "CPU", TEXT_DIM, 12.0);
        right(c, x1 - 10, head_y, "Memory", TEXT_DIM, 12.0);
        c.fill(Rect::new(x0, head_y + 20, x1 - x0, 1), 0x2E3247);

        self.rows.clear();
        let max_rows = ((a.h - LIST_Y - FOOTER_H) / ROW_H).max(0) as usize;
        (self.visible, self.total) = (max_rows, tasks.len());
        self.offset = self.offset.min(tasks.len().saturating_sub(max_rows));
        self.bar = ScrollBar { track: Rect::new(a.w - PAD + 4, LIST_Y, SCROLL_W, max_rows as i32 * ROW_H - 2), len: tasks.len(), rows: max_rows, top: self.offset };
        self.bar.draw(c, a, self.thumb_grab.is_some());
        for (i, t) in tasks.iter().skip(self.offset).take(max_rows).enumerate() {
            let r = Rect::new(x0, a.y + LIST_Y + i as i32 * ROW_H, x1 - x0, ROW_H - 2);
            if self.selected == Some(t.id) {
                c.fill_round(r, 7, ACCENT, 60);
            }
            c.set_clip(Rect::new(r.x, r.y, col_type - r.x - 8, r.h).intersect(a));
            let name = if t.kind == "system" { SYSTEM_TEXT } else { TEXT };
            c.text(r.x + 10, r.y + 6, &t.title, name, FontId::Regular, 14.0);
            c.set_clip(a);
            c.text(col_type, r.y + 6, t.kind, TEXT_DIM, FontId::Regular, 14.0);
            right(c, col_cpu, r.y + 6, &percent(t.cpu), TEXT, 14.0);
            right(c, x1 - 10, r.y + 6, &bytes(t.mem), TEXT, 14.0);
            self.rows.push((r.offset(-a.x, -a.y), t.id));
        }
        if self.selected.is_some_and(|id| !tasks.iter().any(|t| t.id == id)) {
            self.selected = None;
        }

        // The footer: a hint and the end task button.
        let fy = a.y + a.h - FOOTER_H;
        c.fill(Rect::new(a.x, fy, a.w, 1), 0x2E3247);
        c.text(x0, fy + 16, "Statistics update every second", TEXT_DIM, FontId::Regular, 12.0);
        let bw = ui::button_width("End task");
        let b = Rect::new(x1 - bw, fy + 10, bw, ui::BUTTON_H);
        ui::button(c, b, "End task", self.selected.is_some());
        self.end_button = b.offset(-a.x, -a.y);
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if let Some(grab) = self.thumb_grab {
            if buttons & LEFT == 0 {
                self.thumb_grab = None;
            } else {
                self.offset = self.bar.top_at(y - grab);
            }
            return true;
        }
        if !pressed {
            return false;
        }
        if let Some(grab) = self.bar.grab(x, y) {
            self.thumb_grab = Some(grab);
            self.offset = self.bar.top_at(y - grab);
            return true;
        }
        if self.end_button.contains(x, y) {
            match self.selected.take() {
                Some(id) if id >= stats::THREAD_ID_BASE => crate::task::kill(id - stats::THREAD_ID_BASE),
                Some(id) => wm::request(Request::Close(id)),
                None => {}
            }
            return true;
        }
        self.selected = self.rows.iter().find(|(r, _)| r.contains(x, y)).map(|&(_, id)| id);
        true
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let max = self.total.saturating_sub(self.visible) as i32;
        let before = self.offset;
        self.offset = (self.offset as i32 + lines).clamp(0, max) as usize;
        self.offset != before
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        let page = self.visible.max(1) as i32;
        match ch {
            KEY_PAGE_UP => self.scroll(-page),
            KEY_PAGE_DOWN => self.scroll(page),
            _ => false,
        }
    }

    fn tick(&mut self, _ctx: &Ctx) -> bool {
        let v = stats::version();
        let changed = v != self.seen;
        self.seen = v;
        changed
    }
}
