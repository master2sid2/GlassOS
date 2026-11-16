//! The windows of the Date & Time section in Settings: choosing the time zone, setting
//! the date and time manually, the time server.

use super::ui;
use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect};
use crate::input::{KEY_DOWN, KEY_UP, LEFT};
use crate::rtc::Time;
use crate::time::{self, Zone};
use crate::wm::{ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const BG: Color = 0x1C1F2E;
const PAD: i32 = 16;
const FIELD_H: i32 = 30;
const ROW_H: i32 = 30;
const ERROR: Color = 0xF87171;
pub const ZONES_W: i32 = 440;
pub const ZONES_H: i32 = 520;
pub const FORM_W: i32 = 440;

/// The zone's offset now by its rule: marked in summer.
fn offset_now(z: &Zone) -> String {
    let (minutes, summer) = time::zone_now(z);
    if summer { format!("{}  summer", time::offset_text(minutes)) } else { time::offset_text(minutes) }
}

/// The zone picker: a search field and a list.
pub struct ZonePicker {
    cw: i32,
    ch: i32,
    filter: ui::TextField,
    all: Vec<Zone>,
    shown: Vec<usize>,
    top: usize,
    hovered: Option<usize>,
    /// The row selected with the keys.
    cursor: usize,
    current: String,
    caret: bool,
    error: Option<&'static str>,
    done: bool,
}

impl ZonePicker {
    pub fn new() -> Self {
        let all = time::zones();
        let current = if time::zone_chosen() { time::zone().name } else { String::new() };
        let mut p = Self {
            cw: ZONES_W,
            ch: ZONES_H,
            filter: ui::TextField::new("", 40),
            shown: (0..all.len()).collect(),
            all,
            top: 0,
            hovered: None,
            cursor: 0,
            current,
            caret: true,
            error: None,
            done: false,
        };
        if let Some(i) = p.all.iter().position(|z| z.name == p.current) {
            p.cursor = i;
            p.top = i.saturating_sub(p.rows() / 2);
        }
        p
    }

    fn field(&self) -> Rect {
        Rect::new(PAD, PAD, self.cw - 2 * PAD, FIELD_H)
    }

    fn list(&self) -> Rect {
        let y = PAD + FIELD_H + 10;
        Rect::new(PAD, y, self.cw - 2 * PAD, self.ch - y - PAD - ui::BUTTON_H - 12)
    }

    fn rows(&self) -> usize {
        (self.list().h / ROW_H).max(1) as usize
    }

    fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        let l = self.list();
        if !l.contains(x, y) {
            return None;
        }
        let i = self.top + ((y - l.y) / ROW_H) as usize;
        (i < self.shown.len()).then_some(i)
    }

    fn cancel_button(&self) -> Rect {
        let w = ui::button_width("Cancel");
        Rect::new(self.cw - PAD - w, self.ch - PAD - ui::BUTTON_H, w, ui::BUTTON_H)
    }

    fn refilter(&mut self) {
        let words: Vec<String> = self.filter.text.to_ascii_lowercase().split_whitespace().map(String::from).collect();
        self.shown = (0..self.all.len())
            .filter(|&i| {
                let z = &self.all[i];
                let hay = format!("{} {}", z.name.to_ascii_lowercase().replace('_', " "), time::offset_text(z.offset).to_ascii_lowercase());
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect();
        (self.top, self.cursor) = (0, 0);
    }

    fn pick(&mut self, i: usize) {
        let Some(&n) = self.shown.get(i) else { return };
        let name = self.all[n].name.clone();
        match time::change(|s| s.zone = Some(name)) {
            Ok(()) => self.done = true,
            Err(e) => self.error = Some(e),
        }
    }

    fn keep_cursor_visible(&mut self) {
        let rows = self.rows();
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + rows {
            self.top = self.cursor + 1 - rows;
        }
    }
}

impl App for ZonePicker {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        let f = self.field().offset(a.x, a.y);
        c.fill_round(f, 7, 0x000000, 90);
        c.stroke_round(f, 7, ACCENT, 160);
        self.filter.draw(c, f, true, self.caret, "City, region or UTC+3", TEXT);
        let l = self.list();
        let saved = c.clip();
        c.set_clip(l.offset(a.x, a.y).intersect(saved));
        for (row, &n) in self.shown.iter().enumerate().skip(self.top).take(self.rows() + 1) {
            let z = &self.all[n];
            let r = Rect::new(l.x, l.y + (row - self.top) as i32 * ROW_H, l.w, ROW_H - 2).offset(a.x, a.y);
            let current = z.name == self.current;
            let lit = if self.hovered == Some(row) || self.cursor == row { 34 } else if current { 22 } else { 0 };
            if lit > 0 {
                c.fill_round(r, 7, if current { ACCENT } else { 0xFFFFFF }, lit);
            }
            let (region, city) = z.name.split_once('/').unwrap_or(("", &z.name));
            let city = city.replace('_', " ");
            c.text(r.x + 10, r.y + 6, &city, if current { ACCENT } else { TEXT }, FontId::Regular, 14.0);
            if !region.is_empty() {
                let cw = font::measure(FontId::Regular, 14.0, &city);
                c.text(r.x + 18 + cw, r.y + 7, region, TEXT_DIM, FontId::Regular, 12.5);
            }
            let off = offset_now(z);
            let ow = font::measure(FontId::Regular, 12.5, &off);
            c.text(r.x + r.w - 10 - ow, r.y + 7, &off, TEXT_DIM, FontId::Regular, 12.5);
        }
        if self.shown.is_empty() {
            c.text(a.x + l.x + 10, a.y + l.y + 8, "Nothing found", TEXT_DIM, FontId::Regular, 14.0);
        }
        c.set_clip(saved);
        let cancel = self.cancel_button().offset(a.x, a.y);
        if let Some(e) = self.error {
            c.text(a.x + PAD, cancel.y + 7, e, ERROR, FontId::Regular, 13.0);
        } else {
            let n = self.shown.len();
            let note = format!("{n} {}", if n == 1 { "zone" } else { "zones" });
            c.text(a.x + PAD, cancel.y + 7, &note, TEXT_DIM, FontId::Regular, 13.0);
        }
        ui::button(c, cancel, "Cancel", true);
    }

    fn hover(&mut self, x: i32, y: i32) -> bool {
        let now = self.row_at(x, y);
        let changed = now != self.hovered;
        self.hovered = now;
        changed
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let before = self.top;
        let most = self.shown.len().saturating_sub(self.rows());
        self.top = (self.top as i32 + lines * 3).clamp(0, most as i32) as usize;
        self.top != before
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if !pressed || buttons & LEFT == 0 {
            return self.filter.mouse(self.field(), x, y, buttons, pressed);
        }
        if let Some(i) = self.row_at(x, y) {
            self.pick(i);
        } else if self.cancel_button().contains(x, y) {
            self.done = true;
        } else if self.field().contains(x, y) {
            return self.filter.mouse(self.field(), x, y, buttons, pressed);
        }
        true
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        self.caret = true;
        match ch {
            0x1B => self.done = true,
            b'\n' => self.pick(self.cursor),
            KEY_UP => {
                self.cursor = self.cursor.saturating_sub(1);
                self.keep_cursor_visible();
            }
            KEY_DOWN => {
                self.cursor = (self.cursor + 1).min(self.shown.len().saturating_sub(1));
                self.keep_cursor_visible();
            }
            _ => {
                let before = self.filter.text.clone();
                let changed = self.filter.key(ch, |c| !c.is_control());
                if self.filter.text != before {
                    self.refilter();
                }
                return changed;
            }
        }
        true
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        let changed = self.filter.type_char(ch, |c| !c.is_control());
        self.refilter();
        changed
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let on = ctx.caret_on();
        let changed = on != self.caret;
        self.caret = on;
        changed
    }

    fn finished(&self) -> bool {
        self.done
    }
}

/// What the window with fields asks for.
#[derive(Clone, Copy, PartialEq)]
pub enum Form {
    /// The date and time manually.
    Clock,
    /// The NTP server.
    Server,
    /// The metrics exporter port.
    Port,
    /// The addresses allowed to read the metrics.
    Allowed,
}

pub struct FormDialog {
    form: Form,
    cw: i32,
    ch: i32,
    labels: Vec<&'static str>,
    fields: Vec<ui::TextField>,
    focus: usize,
    error: Option<&'static str>,
    caret: bool,
    done: bool,
}

/// "2026-11-02" and "11:08:51" as local time.
pub fn parse_clock(date: &str, clock: &str) -> Option<Time> {
    let mut d = date.trim().split(['-', '.', '/']).map(|p| p.trim().parse::<u16>().ok());
    let (a, b, c) = (d.next()??, d.next()??, d.next()??);
    if d.next().is_some() {
        return None;
    }
    // 2026-11-02 or 02.11.2026.
    let (year, month, day) = if a > 31 { (a, b, c) } else { (c, b, a) };
    let mut t = clock.trim().split(':').map(|p| p.trim().parse::<u8>().ok());
    let (hour, min) = (t.next()??, t.next()??);
    let sec = t.next().map_or(Some(0), |s| s)?;
    if t.next().is_some() {
        return None;
    }
    Some(Time { hour, min, sec, day: day.min(255) as u8, month: month.min(255) as u8, year })
}

impl FormDialog {
    pub fn new(form: Form) -> Self {
        let (labels, fields) = match form {
            Form::Clock => {
                let t = time::now();
                let date = format!("{}-{:02}-{:02}", t.year, t.month, t.day);
                let clock = format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec);
                (alloc::vec!["Date", "Time"], alloc::vec![ui::TextField::new(&date, 10), ui::TextField::new(&clock, 8)])
            }
            Form::Server => {
                let s = time::settings();
                (alloc::vec!["Server"], alloc::vec![ui::TextField::new(s.server(), 64)])
            }
            Form::Port => {
                let port = crate::net::exporter::settings().port;
                (alloc::vec!["Port"], alloc::vec![ui::TextField::new(&format!("{port}"), 5)])
            }
            Form::Allowed => {
                let list: Vec<String> = crate::net::exporter::settings().allowed.iter().map(|ip| crate::net::ip_str(*ip)).collect();
                (alloc::vec!["Addresses"], alloc::vec![ui::TextField::new(&list.join(", "), 200)])
            }
        };
        let mut dialog = Self { form, cw: FORM_W, ch: 0, labels, fields, focus: 0, error: None, caret: true, done: false };
        dialog.fields[0].select_all();
        dialog
    }

    pub fn title(&self) -> &'static str {
        match self.form {
            Form::Clock => "Set Date and Time",
            Form::Server => "Time Server",
            Form::Port => "Metrics Port",
            Form::Allowed => "Who Can Read Metrics",
        }
    }

    pub fn height(&self) -> i32 {
        PAD + self.fields.len() as i32 * 44 + 30 + 40 + ui::BUTTON_H + PAD
    }

    fn hint(&self) -> &'static str {
        match self.form {
            Form::Clock => "Year-month-day and hours:minutes, local time",
            Form::Server => "Empty means pool.ntp.org",
            Form::Port => "Prometheus usually reads port 9100",
            Form::Allowed => "IP addresses, separated by commas",
        }
    }

    fn field(&self, i: usize) -> Rect {
        Rect::new(110, PAD + i as i32 * 44, self.cw - 110 - PAD, FIELD_H)
    }

    fn ok_button(&self) -> Rect {
        let w = ui::button_width("Set");
        Rect::new(self.cw - PAD - w, self.ch - PAD - ui::BUTTON_H, w, ui::BUTTON_H)
    }

    fn cancel_button(&self) -> Rect {
        let w = ui::button_width("Cancel");
        let ok = self.ok_button();
        Rect::new(ok.x - 8 - w, ok.y, w, ui::BUTTON_H)
    }

    fn submit(&mut self) {
        let result = match self.form {
            Form::Clock => match parse_clock(&self.fields[0].text, &self.fields[1].text) {
                Some(t) => time::set_local(&t),
                None => Err("Write the date as 2026-11-02 and the time as 14:05"),
            },
            Form::Server => {
                let server: String = self.fields[0].text.trim().into();
                if server.contains(char::is_whitespace) {
                    Err("A server name has no spaces")
                } else {
                    time::change(|s| s.server = if server == time::DEFAULT_SERVER { String::new() } else { server }).map(|()| time::request_sync())
                }
            }
            Form::Port => match self.fields[0].text.trim().parse::<u16>() {
                Ok(port) if port > 0 && port != 445 => crate::net::exporter::change(|s| s.port = port),
                Ok(445) => Err("Port 445 is taken by shared folders"),
                _ => Err("A port is a number from 1 to 65535"),
            },
            Form::Allowed => {
                let parts: Vec<&str> = self.fields[0].text.split([',', ' ']).filter(|p| !p.is_empty()).collect();
                let list: Vec<crate::net::Ip> = parts.iter().filter_map(|p| crate::net::parse_ip(p)).collect();
                if list.len() != parts.len() {
                    Err("Write addresses like 192.168.1.10")
                } else {
                    crate::net::exporter::change(|s| s.allowed = list)
                }
            }
        };
        match result {
            Ok(()) => self.done = true,
            Err(e) => self.error = Some(e),
        }
    }
}

impl App for FormDialog {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        for i in 0..self.fields.len() {
            let r = self.field(i).offset(a.x, a.y);
            c.text(a.x + PAD, r.y + 6, self.labels[i], TEXT, FontId::Regular, 14.0);
            c.fill_round(r, 7, 0x000000, 90);
            let focused = self.focus == i;
            c.stroke_round(r, 7, if focused { ACCENT } else { 0xFFFFFF }, if focused { 160 } else { 30 });
            self.fields[i].draw(c, r, focused, self.caret, "", TEXT);
        }
        let below = self.field(self.fields.len() - 1).offset(a.x, a.y);
        c.text(below.x, below.y + FIELD_H + 8, self.hint(), TEXT_DIM, FontId::Regular, 12.5);
        let ok = self.ok_button().offset(a.x, a.y);
        if let Some(e) = self.error {
            c.text(a.x + PAD, ok.y - 24, e, ERROR, FontId::Regular, 13.0);
        }
        ui::button(c, self.cancel_button().offset(a.x, a.y), "Cancel", true);
        ui::accent_button(c, ok, "Set", true);
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        self.caret = true;
        match ch {
            0x1B => self.done = true,
            b'\n' => self.submit(),
            b'\t' => {
                self.focus = (self.focus + 1) % self.fields.len();
                self.fields[self.focus].select_all();
            }
            _ => {
                let focus = self.focus;
                return self.fields[focus].key(ch, |c| c.is_ascii() && !c.is_control());
            }
        }
        true
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        let focus = self.focus;
        self.fields[focus].type_char(ch, |c| c.is_ascii() && !c.is_control())
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if !pressed || buttons & LEFT == 0 {
            let (r, focus) = (self.field(self.focus), self.focus);
            return self.fields[focus].mouse(r, x, y, buttons, pressed);
        }
        if self.ok_button().contains(x, y) {
            self.submit();
        } else if self.cancel_button().contains(x, y) {
            self.done = true;
        } else if let Some(i) = (0..self.fields.len()).find(|&i| self.field(i).contains(x, y)) {
            self.focus = i;
            let r = self.field(i);
            self.fields[i].mouse(r, x, y, buttons, pressed);
        } else {
            return false;
        }
        true
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let on = ctx.caret_on();
        let changed = on != self.caret;
        self.caret = on;
        changed
    }

    fn finished(&self) -> bool {
        self.done
    }
}
