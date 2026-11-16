//! The Bluetooth icon in the top bar and its menu: turn on or off, devices
//! (paired and found nearby — a click connects or disconnects), a link to
//! Settings. The icon is there when the computer has a controller.

use super::{ACCENT, SHADOW_PAD, TEXT, TEXT_DIM, Wm};
use crate::apps::ui;
use crate::bt::{self, Link};
use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect};
use crate::interrupts::HZ;
use alloc::string::String;
use alloc::vec::Vec;

const MENU_W: i32 = 300;
const HEAD_H: i32 = 48;
const ROW_H: i32 = 34;
/// The menu shows no more device rows than this (the rest are in Settings).
const MAX_ROWS: usize = 8;
const FOOT_H: i32 = 40;
/// How often to refresh the list while the menu is open.
const REFRESH: u64 = HZ / 2;
const HOVER: Color = 0x3B82F6;

/// A menu row: a device and how to show it.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Row {
    addr: [u8; 6],
    name: String,
    state: String,
    connected: bool,
}

/// The menu state (lives in `Tray`).
#[derive(Default)]
pub(super) struct BtMenu {
    open: bool,
    hover: Option<Hit>,
    rows: Vec<Row>,
    /// What was in the menu at the last check: on, scanning.
    on: bool,
    searching: bool,
    checked: u64,
    /// The icon: on, something connected.
    icon: (bool, bool, bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Switch,
    Row(usize),
    Settings,
}

fn rows() -> Vec<Row> {
    bt::devices()
        .into_iter()
        .filter(|e| e.paired || !e.dev.name.trim().is_empty())
        .take(MAX_ROWS)
        .map(|e| {
            let connected = e.link == Some(Link::Connected);
            let state = match &e.link {
                Some(Link::Connected) => "Connected".into(),
                Some(Link::Connecting) => "Connecting\u{2026}".into(),
                Some(Link::Pairing(_)) | Some(Link::Confirm(_)) => "Pairing\u{2026}".into(),
                Some(Link::Failed(_)) => "Failed".into(),
                None if e.paired => "Paired".into(),
                None => String::new(),
            };
            let name = String::from(e.dev.name.trim());
            Row { addr: e.dev.addr, name: if name.is_empty() { bt::addr_text(&e.dev.addr) } else { name }, state, connected }
        })
        .collect()
}

impl Wm {
    /// The Bluetooth icon: between network and volume; without a controller it is absent.
    pub(super) fn bt_icon(&self) -> Rect {
        let n = self.net_icon();
        if bt::present() { Rect::new(n.x - 32, n.y, 28, 26) } else { Rect::new(n.x, n.y, 0, 26) }
    }

    pub(super) fn bt_menu_rect(&self) -> Rect {
        let i = self.bt_icon();
        let m = &self.tray.bt;
        let list = if !m.on { 1 } else { m.rows.len().max(1) as i32 };
        let h = HEAD_H + list as i32 * ROW_H + FOOT_H + 12;
        Rect::new((i.x + i.w / 2 - MENU_W / 2).min(self.right() - MENU_W - 8), self.top() + super::TOPBAR_H + 6, MENU_W, h)
    }

    fn bt_switch(&self) -> Rect {
        let r = self.bt_menu_rect();
        Rect::new(r.x + r.w - 16 - 44, r.y + 12, 44, 24)
    }

    fn bt_row(&self, i: usize) -> Rect {
        let r = self.bt_menu_rect();
        Rect::new(r.x + 6, r.y + HEAD_H + i as i32 * ROW_H, r.w - 12, ROW_H)
    }

    fn bt_settings_row(&self) -> Rect {
        let r = self.bt_menu_rect();
        Rect::new(r.x + 6, r.y + r.h - FOOT_H - 6, r.w - 12, FOOT_H - 6)
    }

    fn bt_hit(&self, x: i32, y: i32) -> Option<Hit> {
        if self.bt_switch().inset(-4).contains(x, y) {
            return Some(Hit::Switch);
        }
        if self.bt_settings_row().contains(x, y) {
            return Some(Hit::Settings);
        }
        if self.tray.bt.on {
            return (0..self.tray.bt.rows.len()).find(|&i| self.bt_row(i).contains(x, y)).map(Hit::Row);
        }
        None
    }

    pub(super) fn bt_menu_open(&self) -> bool {
        self.tray.bt.open
    }

    pub(super) fn close_bt_menu(&mut self) {
        if self.tray.bt.open {
            self.mark(self.bt_menu_rect().inset(-SHADOW_PAD));
            self.tray.bt.open = false;
            self.tray.bt.hover = None;
        }
    }

    fn bt_refresh(&mut self) {
        let before = self.bt_menu_rect();
        let m = &mut self.tray.bt;
        m.on = bt::enabled();
        m.searching = bt::scanning();
        m.rows = if m.on { rows() } else { Vec::new() };
        self.mark(before.inset(-SHADOW_PAD));
        self.mark(self.bt_menu_rect().inset(-SHADOW_PAD));
    }

    /// A click. true — handled here (the menu or the icon).
    pub(super) fn bt_mouse(&mut self, x: i32, y: i32) -> bool {
        if self.tray.bt.open {
            if !self.bt_menu_rect().contains(x, y) {
                self.close_bt_menu();
                // A click on the icon only closes the menu.
                return self.bt_icon().contains(x, y);
            }
            match self.bt_hit(x, y) {
                Some(Hit::Switch) => {
                    bt::set_enabled(!bt::enabled());
                    if bt::enabled() {
                        bt::start_scan();
                    }
                    self.bt_refresh();
                }
                Some(Hit::Row(i)) => {
                    let row = self.tray.bt.rows[i].clone();
                    if row.connected {
                        bt::disconnect(row.addr);
                    } else {
                        bt::connect(row.addr);
                    }
                    self.bt_refresh();
                }
                Some(Hit::Settings) => {
                    self.close_bt_menu();
                    self.open_settings("bluetooth", None);
                }
                None => {}
            }
            return true;
        }
        if self.bt_icon().w > 0 && self.bt_icon().contains(x, y) {
            self.close_volume();
            self.mark_tray();
            self.tray.bt.open = true;
            self.tray.bt.hover = None;
            bt::start_scan();
            self.bt_refresh();
            return true;
        }
        false
    }

    pub(super) fn bt_hover(&mut self, x: i32, y: i32) {
        if !self.tray.bt.open {
            return;
        }
        let hit = self.bt_hit(x, y);
        if hit != self.tray.bt.hover {
            self.tray.bt.hover = hit;
            self.mark(self.bt_menu_rect());
        }
    }

    /// Every half second: the icon (on/connected) and, if the menu is open, the list.
    pub(super) fn bt_tick(&mut self, ticks: u64) {
        if ticks < self.tray.bt.checked + REFRESH {
            return;
        }
        self.tray.bt.checked = ticks;
        let connected = bt::enabled() && bt::devices().iter().any(|e| e.link == Some(Link::Connected));
        let icon = (bt::present(), bt::enabled(), connected);
        if icon != self.tray.bt.icon {
            self.tray.bt.icon = icon;
            self.mark_tray();
        }
        if self.tray.bt.open {
            let m = &self.tray.bt;
            let now = (bt::enabled(), bt::scanning(), if bt::enabled() { rows() } else { Vec::new() });
            if (m.on, m.searching, &m.rows) != (now.0, now.1, &now.2) {
                self.bt_refresh();
            }
        }
    }

    pub(super) fn draw_bt_icon(&self, c: &mut Canvas) {
        let r = self.bt_icon();
        if r.w == 0 {
            return;
        }
        if self.tray.bt.open {
            c.fill_round(r, 7, 0xFFFFFF, 40);
        }
        let (_, on, connected) = self.tray.bt.icon;
        let color = if !on { TEXT_DIM } else if connected { ACCENT } else { TEXT };
        rune(c, r.x + r.w / 2, r.y + 4, color);
        if !on {
            super::tray::cross(c, r.x + 19, r.y + 15);
        }
    }

    pub(super) fn draw_bt_menu(&self, c: &mut Canvas) {
        if !self.tray.bt.open {
            return;
        }
        let r = self.bt_menu_rect();
        let m = &self.tray.bt;
        super::tray::panel(c, r, 14);
        c.text(r.x + 16, r.y + 15, "Bluetooth", TEXT, FontId::SemiBold, 15.0);
        if m.on && m.searching {
            let tw = font::measure(FontId::SemiBold, 15.0, "Bluetooth");
            c.text(r.x + 16 + tw + 10, r.y + 17, "searching\u{2026}", TEXT_DIM, FontId::Regular, 12.0);
        }
        ui::switch(c, self.bt_switch(), m.on);
        c.fill(Rect::new(r.x + 14, r.y + HEAD_H - 6, r.w - 28, 1), 0x3A3F55);
        if !m.on {
            c.text(r.x + 16, r.y + HEAD_H + 9, "Bluetooth is off", TEXT_DIM, FontId::Regular, 14.0);
        } else if m.rows.is_empty() {
            let text = if m.searching { "Looking for devices\u{2026}" } else { "No devices found" };
            c.text(r.x + 16, r.y + HEAD_H + 9, text, TEXT_DIM, FontId::Regular, 14.0);
        }
        for (i, row) in m.rows.iter().enumerate() {
            let at = self.bt_row(i);
            if m.hover == Some(Hit::Row(i)) {
                c.fill_round(at, 7, HOVER, 220);
            }
            let sw = font::measure(FontId::Regular, 12.0, &row.state);
            let room = at.w - 20 - sw - 12;
            let name = ui::fit(&row.name, FontId::Regular, 14.0, room);
            c.text(at.x + 10, at.y + 8, &name, if row.connected { ACCENT } else { TEXT }, FontId::Regular, 14.0);
            c.text(at.x + at.w - 10 - sw, at.y + 10, &row.state, TEXT_DIM, FontId::Regular, 12.0);
        }
        let s = self.bt_settings_row();
        c.fill(Rect::new(s.x + 8, s.y - 4, s.w - 16, 1), 0x3A3F55);
        if m.hover == Some(Hit::Settings) {
            c.fill_round(s, 7, HOVER, 220);
        }
        c.text(s.x + 10, s.y + 9, "Bluetooth settings\u{2026}", TEXT, FontId::Regular, 14.0);
    }
}

/// The Bluetooth rune 18 high with its axis at `x`: a vertical line and two "arrows" to the right.
fn rune(c: &mut Canvas, x: i32, y: i32, color: Color) {
    let (top, bottom, mid) = (y, y + 18, y + 9);
    let right = x + 5;
    let left = x - 5;
    line(c, x, top, x, bottom, color);
    line(c, x, top, right, top + 4, color);
    line(c, right, top + 4, left, bottom - 5, color);
    line(c, x, bottom, right, bottom - 4, color);
    line(c, right, bottom - 4, left, top + 5, color);
    let _ = mid;
}

/// A 2-pixel-thick line segment (Bresenham).
fn line(c: &mut Canvas, x0: i32, y0: i32, x1: i32, y1: i32, color: Color) {
    let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let (mut x, mut y, mut err) = (x0, y0, dx + dy);
    loop {
        c.fill(Rect::new(x, y, 2, 2), color);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}
