//! The calendar: a click on the date in the top bar. Two views: a month, and a year of twelve
//! month squares. The week starts on Monday.

use super::tray::panel;
use super::{ACCENT, SHADOW_PAD, TEXT, TEXT_DIM, TOPBAR_H, Wm};
use crate::font::{self, FontId};
use crate::gfx::{Canvas, Rect};
use crate::input::{KEY_LEFT, KEY_RIGHT};
use alloc::format;
use alloc::vec::Vec;

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const WEEKDAYS: [&str; 7] = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
const PAD: i32 = 16;
const HEADER_H: i32 = 32;
const TABS_H: i32 = 28;
/// Where the grid starts: below the header and the tabs.
const GRID_Y: i32 = 12 + HEADER_H + 8 + TABS_H + 10;
const TAB_W: i32 = 70;
const DAY: i32 = 44;
const DAY_H: i32 = 40;
const WEEKDAY_H: i32 = 24;
/// A year: four columns, three rows.
const COLS: i32 = 4;
const ROWS: i32 = 3;
const SQUARE_W: i32 = 172;
const SQUARE_H: i32 = 172;
const SQUARE_HEAD: i32 = 42;
const FAINT: u32 = 0x5A6078;
const WEEKEND: u32 = 0xF2A0A0;
const TODAY_TEXT: u32 = 0x0B1020;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Month,
    Year,
}

pub struct Calendar {
    view: View,
    year: i32,
    /// 1..=12.
    month: i32,
    hover: Option<Rect>,
}

fn days_in(year: i32, month: i32) -> i32 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The weekday of the first day: 0 — Monday (Sakamoto's method).
fn first_weekday(year: i32, month: i32) -> i32 {
    const SHIFT: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = if month < 3 { year - 1 } else { year };
    (y + y / 4 - y / 100 + y / 400 + SHIFT[month as usize - 1] + 1 + 6) % 7
}

/// The month before or after the given one.
fn shift_month(year: i32, month: i32, by: i32) -> (i32, i32) {
    let n = year * 12 + (month - 1) + by;
    (n.div_euclid(12), n.rem_euclid(12) + 1)
}

/// Six weeks of a month: the days and the month offset (-1, 0, 1), counting the neighbouring ones.
fn cells(year: i32, month: i32) -> [(i32, i32); 42] {
    let first = first_weekday(year, month);
    let (py, pm) = shift_month(year, month, -1);
    let before = days_in(py, pm);
    let days = days_in(year, month);
    core::array::from_fn(|i| {
        let d = i as i32 - first + 1;
        if d < 1 {
            (before + d, -1)
        } else if d > days {
            (d - days, 1)
        } else {
            (d, 0)
        }
    })
}

fn text_center(c: &mut Canvas, r: Rect, s: &str, color: u32, font_id: FontId, size: f32) {
    let w = font::measure(font_id, size, s);
    c.text(r.x + (r.w - w) / 2, r.y + (r.h - size as i32 - 4) / 2, s, color, font_id, size);
}

impl Wm {
    /// The date and time in the panel.
    pub(super) fn clock_rect(&self) -> Rect {
        Rect::new(self.right() - 18 - self.clock_w - 4, self.top() + 2, self.clock_w + 8, 26)
    }

    /// The size of a month square: on a small screen they shrink.
    fn square(&self) -> (i32, i32) {
        let w = (self.w - 16 - 2 * PAD) / COLS;
        let h = (self.h - TOPBAR_H - 20 - GRID_Y - 12) / ROWS;
        (w.min(SQUARE_W), h.min(SQUARE_H))
    }

    pub(super) fn calendar_rect(&self) -> Option<Rect> {
        let cal = self.calendar.as_ref()?;
        let (w, h) = match cal.view {
            View::Month => (7 * DAY + 2 * PAD, GRID_Y + WEEKDAY_H + 6 * DAY_H + 12),
            View::Year => {
                let (sw, sh) = self.square();
                (COLS * sw + 2 * PAD, GRID_Y + ROWS * sh + 12)
            }
        };
        Some(Rect::new(self.right() - w - 8, self.top() + TOPBAR_H + 6, w, h))
    }

    fn prev_button(&self, r: Rect) -> Rect {
        Rect::new(r.x + PAD, r.y + 12, HEADER_H, HEADER_H)
    }

    fn next_button(&self, r: Rect) -> Rect {
        Rect::new(r.x + r.w - PAD - HEADER_H, r.y + 12, HEADER_H, HEADER_H)
    }

    fn tab(&self, r: Rect, i: i32) -> Rect {
        Rect::new(r.x + PAD + i * (TAB_W + 4), r.y + 12 + HEADER_H + 8, TAB_W, TABS_H)
    }

    fn today_button(&self, r: Rect) -> Rect {
        Rect::new(r.x + r.w - PAD - TAB_W, r.y + 12 + HEADER_H + 8, TAB_W, TABS_H)
    }

    /// The cell of day `i` in the month view.
    fn day_rect(&self, r: Rect, i: i32) -> Rect {
        Rect::new(r.x + PAD + (i % 7) * DAY, r.y + GRID_Y + WEEKDAY_H + (i / 7) * DAY_H, DAY, DAY_H)
    }

    /// The square of month `m` (0..12) in the year view.
    fn square_rect(&self, r: Rect, m: i32) -> Rect {
        let (sw, sh) = self.square();
        Rect::new(r.x + PAD + (m % COLS) * sw, r.y + GRID_Y + (m / COLS) * sh, sw, sh)
    }

    /// Everything that highlights under the cursor.
    fn calendar_targets(&self, r: Rect) -> Vec<Rect> {
        let mut out = alloc::vec![self.prev_button(r), self.next_button(r), self.tab(r, 0), self.tab(r, 1), self.today_button(r)];
        match self.calendar.as_ref().map(|c| c.view) {
            Some(View::Year) => out.extend((0..12).map(|m| self.square_rect(r, m))),
            _ => out.extend((0..42).map(|i| self.day_rect(r, i))),
        }
        out
    }

    /// Changes the calendar and redraws the old and the new place.
    fn update_calendar(&mut self, f: impl FnOnce(&mut Calendar)) {
        if let Some(r) = self.calendar_rect() {
            self.mark(r.inset(-SHADOW_PAD));
        }
        if let Some(cal) = &mut self.calendar {
            f(cal);
        }
        if let Some(r) = self.calendar_rect() {
            self.mark(r.inset(-SHADOW_PAD));
        }
    }

    pub(super) fn close_calendar(&mut self) {
        if let Some(r) = self.calendar_rect() {
            self.mark(r.inset(-SHADOW_PAD));
            self.calendar = None;
        }
    }

    pub(super) fn open_calendar(&mut self) {
        self.close_menu();
        self.close_volume();
        if self.center.is_some() {
            self.toggle_center();
        }
        let t = crate::time::now();
        self.calendar = Some(Calendar { view: View::Month, year: t.year as i32, month: t.month as i32, hover: None });
        self.update_calendar(|_| {});
    }

    /// A step forward or back: by a month, and in the year view by a year.
    fn calendar_step(&mut self, by: i32) {
        self.update_calendar(|c| match c.view {
            View::Month => (c.year, c.month) = shift_month(c.year, c.month, by),
            View::Year => c.year += by,
        });
    }

    /// The mouse (called before other handling). true — the event was handled here.
    pub(super) fn calendar_mouse(&mut self, x: i32, y: i32, pressed: bool) -> bool {
        let Some(r) = self.calendar_rect() else {
            if pressed && self.clock_rect().contains(x, y) {
                self.open_calendar();
                return true;
            }
            return false;
        };
        let hover = self.calendar_targets(r).into_iter().find(|t| t.contains(x, y));
        if self.calendar.as_ref().is_some_and(|c| c.hover != hover) {
            self.update_calendar(|c| c.hover = hover);
        }
        if !pressed {
            return r.contains(x, y);
        }
        if !r.contains(x, y) {
            self.close_calendar();
            // A click on the date only closes it, the others go further.
            return self.clock_rect().contains(x, y);
        }
        let hit = |t: Rect| t.contains(x, y);
        if hit(self.prev_button(r)) {
            self.calendar_step(-1);
        } else if hit(self.next_button(r)) {
            self.calendar_step(1);
        } else if hit(self.tab(r, 0)) {
            self.update_calendar(|c| (c.view, c.hover) = (View::Month, None));
        } else if hit(self.tab(r, 1)) {
            self.update_calendar(|c| (c.view, c.hover) = (View::Year, None));
        } else if hit(self.today_button(r)) {
            let t = crate::time::now();
            self.update_calendar(|c| (c.year, c.month) = (t.year as i32, t.month as i32));
        } else if self.calendar.as_ref().is_some_and(|c| c.view == View::Year) {
            if let Some(m) = (0..12).find(|&m| hit(self.square_rect(r, m))) {
                self.update_calendar(|c| (c.view, c.month, c.hover) = (View::Month, m + 1, None));
            }
        } else if let Some(i) = (0..42).find(|&i| hit(self.day_rect(r, i)))
            && let Some(cal) = &self.calendar
        {
            // A day of a neighbouring month opens that month.
            let offset = cells(cal.year, cal.month)[i as usize].1;
            if offset != 0 {
                self.calendar_step(offset);
            }
        }
        true
    }

    /// The wheel over the calendar pages it. true — handled.
    pub(super) fn calendar_wheel(&mut self, x: i32, y: i32, clicks: i32) -> bool {
        let over = self.calendar_rect().is_some_and(|r| r.contains(x, y));
        if over {
            self.calendar_step(clicks.signum());
        }
        over
    }

    /// Keys while the calendar is open. true — the key was handled.
    pub(super) fn calendar_key(&mut self, ch: u8) -> bool {
        if self.calendar.is_none() {
            return false;
        }
        match ch {
            0x1B => self.close_calendar(),
            KEY_LEFT => self.calendar_step(-1),
            KEY_RIGHT => self.calendar_step(1),
            _ => return false,
        }
        true
    }

    pub(super) fn draw_calendar(&self, c: &mut Canvas) {
        let (Some(cal), Some(r)) = (&self.calendar, self.calendar_rect()) else { return };
        panel(c, r, 14);
        let lit = |c: &mut Canvas, t: Rect, radius: i32| {
            if cal.hover == Some(t) {
                c.fill_round(t, radius, 0xFFFFFF, 26);
            }
        };
        let today = crate::time::now();
        let today = (today.year as i32, today.month as i32, today.day as i32);

        let (prev, next) = (self.prev_button(r), self.next_button(r));
        lit(c, prev, 8);
        lit(c, next, 8);
        text_center(c, prev, "\u{2039}", TEXT, FontId::SemiBold, 20.0);
        text_center(c, next, "\u{203A}", TEXT, FontId::SemiBold, 20.0);
        let title = match cal.view {
            View::Month => format!("{} {}", MONTHS[cal.month as usize - 1], cal.year),
            View::Year => format!("{}", cal.year),
        };
        text_center(c, Rect::new(r.x, r.y + 12, r.w, HEADER_H), &title, TEXT, FontId::SemiBold, 16.0);

        for (i, label) in ["Month", "Year"].iter().enumerate() {
            let t = self.tab(r, i as i32);
            let on = (i == 0) == (cal.view == View::Month);
            lit(c, t, 8);
            c.fill_round(t, 8, 0xFFFFFF, if on { 34 } else { 12 });
            text_center(c, t, label, if on { TEXT } else { TEXT_DIM }, FontId::Regular, 13.0);
        }
        let t = self.today_button(r);
        lit(c, t, 8);
        c.stroke_round(t, 8, 0xFFFFFF, 30);
        text_center(c, t, "Today", TEXT, FontId::Regular, 13.0);

        match cal.view {
            View::Month => self.draw_month(c, cal, r, today),
            View::Year => self.draw_year(c, cal, r, today),
        }
    }

    fn draw_month(&self, c: &mut Canvas, cal: &Calendar, r: Rect, today: (i32, i32, i32)) {
        for (i, name) in WEEKDAYS.iter().enumerate() {
            let t = Rect::new(r.x + PAD + i as i32 * DAY, r.y + GRID_Y, DAY, WEEKDAY_H);
            text_center(c, t, name, if i >= 5 { WEEKEND } else { TEXT_DIM }, FontId::SemiBold, 12.0);
        }
        for (i, &(day, offset)) in cells(cal.year, cal.month).iter().enumerate() {
            let t = self.day_rect(r, i as i32);
            let mark = Rect::new(t.x + (t.w - 32) / 2, t.y + (t.h - 32) / 2, 32, 32);
            let now = offset == 0 && (cal.year, cal.month, day) == today;
            if now {
                c.fill_round(mark, 16, ACCENT, 256);
            } else if cal.hover == Some(t) {
                c.fill_round(mark, 16, 0xFFFFFF, 26);
            }
            let color = match () {
                _ if now => TODAY_TEXT,
                _ if offset != 0 => FAINT,
                _ if i % 7 >= 5 => WEEKEND,
                _ => TEXT,
            };
            text_center(c, t, &format!("{day}"), color, if now { FontId::SemiBold } else { FontId::Regular }, 14.0);
        }
    }

    fn draw_year(&self, c: &mut Canvas, cal: &Calendar, r: Rect, today: (i32, i32, i32)) {
        for m in 1..=12 {
            let s = self.square_rect(r, m - 1);
            if cal.hover == Some(s) {
                c.fill_round(s.inset(2), 10, 0xFFFFFF, 18);
            }
            let this = (cal.year, m) == (today.0, today.1);
            c.text(s.x + 12, s.y + 6, MONTHS[m as usize - 1], if this { ACCENT } else { TEXT }, FontId::SemiBold, 13.0);
            let cw = (s.w - 16) / 7;
            let ch = (s.h - SQUARE_HEAD - 6) / 6;
            for (i, name) in WEEKDAYS.iter().enumerate() {
                let t = Rect::new(s.x + 8 + i as i32 * cw, s.y + 25, cw, 16);
                let short = &name[..1];
                text_center(c, t, short, if i >= 5 { WEEKEND } else { TEXT_DIM }, FontId::Regular, 10.0);
            }
            for (i, &(day, offset)) in cells(cal.year, m).iter().enumerate() {
                if offset != 0 {
                    continue;
                }
                let i = i as i32;
                let t = Rect::new(s.x + 8 + (i % 7) * cw, s.y + SQUARE_HEAD + (i / 7) * ch, cw, ch);
                let now = (cal.year, m, day) == today;
                if now {
                    let d = cw.min(ch);
                    c.fill_round(Rect::new(t.x + (t.w - d) / 2, t.y + (t.h - d) / 2, d, d), d / 2, ACCENT, 256);
                }
                let color = if now { TODAY_TEXT } else if i % 7 >= 5 { WEEKEND } else { TEXT };
                text_center(c, t, &format!("{day}"), color, FontId::Regular, 11.0);
            }
        }
    }
}
