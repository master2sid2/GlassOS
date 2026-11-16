//! The Bluetooth window: found and paired devices, pairing, connecting and
//! disconnecting.
//! Numeric comparison during pairing is a banner on top: "does the device show 123456?".

use super::ui;
use crate::font::FontId;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::bt::{self, Entry, Link};
use crate::wm::{ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const BG: Color = 0x1C1F2E;
const PAD: i32 = 22;
const ESC: u8 = 0x1B;
const RED: Color = 0xF87171;
/// The status line and the Scan button are at the top; the list below; the buttons at the bottom.
const HEAD: i32 = 16;
const LIST_TOP: i32 = 58;
const ROW: i32 = 48;
const BANNER_H: i32 = 70;
const FOOT: i32 = 16 + ui::BUTTON_H + 12;

/// What to do with the selected device.
#[derive(Clone, Copy)]
enum Action {
    Connect,
    Disconnect,
}

pub struct BtSettings {
    cw: i32,
    ch: i32,
    list: Vec<Entry>,
    /// The list fingerprint — redraw only when it changed.
    shown: String,
    selected: Option<[u8; 6]>,
    checked: u64,
    done: bool,
    /// A page inside Settings: it has no Close button of its own.
    embedded: bool,
}

impl BtSettings {
    pub fn new(cw: i32, ch: i32) -> Self {
        let mut s = Self { cw, ch, list: Vec::new(), shown: String::new(), selected: None, checked: 0, done: false, embedded: false };
        s.refresh();
        // The list is empty — scan right away (like the Wi-Fi tab).
        if s.list.is_empty() {
            Self::scan();
        }
        s
    }

    /// A page inside Settings (without its own Close button).
    pub fn embedded(mut self) -> Self {
        self.embedded = true;
        self
    }

    fn scan() {
        if bt::present() && !bt::scanning() {
            crate::task::spawn("bt-scan", None, || {
                let _ = bt::scan();
            });
        }
    }

    /// Refreshes the list; true — something changed.
    fn refresh(&mut self) -> bool {
        self.list = bt::devices();
        let mut sig = format!("{}|{}|{}", bt::present(), bt::scanning(), bt::status());
        for e in &self.list {
            sig += &format!("|{}{}{}{}", bt::addr_text(&e.dev.addr), e.dev.name, e.paired, e.link.as_ref().map(bt::link_text).unwrap_or_default());
        }
        let changed = sig != self.shown;
        self.shown = sig;
        changed
    }

    fn entry(&self, addr: &[u8; 6]) -> Option<&Entry> {
        self.list.iter().find(|e| e.dev.addr == *addr)
    }

    /// The device asking to compare numbers (the banner on top).
    fn confirming(&self) -> Option<(&Entry, u32)> {
        self.list.iter().find_map(|e| match e.link {
            Some(Link::Confirm(n)) => Some((e, n)),
            _ => None,
        })
    }

    fn list_top(&self) -> i32 {
        if self.confirming().is_some() { LIST_TOP + BANNER_H + 8 } else { LIST_TOP }
    }

    fn rows(&self) -> usize {
        ((self.ch - self.list_top() - FOOT) / ROW).max(1) as usize
    }

    fn row(&self, i: usize) -> Rect {
        Rect::new(PAD, self.list_top() + i as i32 * ROW, self.cw - 2 * PAD, ROW - 4)
    }

    fn scan_button(&self) -> Rect {
        let w = ui::button_width("Scan");
        Rect::new(self.cw - PAD - w, HEAD - 2, w, ui::BUTTON_H)
    }

    fn banner(&self) -> Rect {
        Rect::new(PAD, LIST_TOP, self.cw - 2 * PAD, BANNER_H)
    }

    fn banner_buttons(&self) -> (Rect, Rect) {
        let b = self.banner();
        let (wy, wn) = (ui::button_width("Pair"), ui::button_width("Cancel"));
        let y = b.y + b.h - ui::BUTTON_H - 8;
        let yes = Rect::new(b.x + b.w - 10 - wy, y, wy, ui::BUTTON_H);
        (yes, Rect::new(yes.x - 8 - wn, y, wn, ui::BUTTON_H))
    }

    /// The action button for the selected device: the label and the action.
    fn action(&self) -> Option<(&'static str, Action)> {
        let e = self.entry(&self.selected?)?;
        Some(match &e.link {
            Some(Link::Connecting | Link::Pairing(_) | Link::Confirm(_)) => ("Cancel", Action::Disconnect),
            Some(Link::Connected) => ("Disconnect", Action::Disconnect),
            _ if e.paired || e.dev.le => ("Connect", Action::Connect),
            _ => ("Pair", Action::Connect),
        })
    }

    fn foot_y(&self) -> i32 {
        self.ch - 16 - ui::BUTTON_H
    }

    fn action_button(&self) -> Option<Rect> {
        let (label, _) = self.action()?;
        Some(Rect::new(PAD, self.foot_y(), ui::button_width(label), ui::BUTTON_H))
    }

    fn forget_button(&self) -> Option<Rect> {
        let e = self.entry(&self.selected?)?;
        if !e.paired {
            return None;
        }
        let x = self.action_button().map_or(PAD, |r| r.x + r.w + 8);
        Some(Rect::new(x, self.foot_y(), ui::button_width("Forget"), ui::BUTTON_H))
    }

    fn close_button(&self) -> Rect {
        if self.embedded {
            return Rect::new(-1000, -1000, 0, 0);
        }
        let w = ui::button_width("Close");
        Rect::new(self.cw - PAD - w, self.foot_y(), w, ui::BUTTON_H)
    }

    fn act(&mut self) {
        let (Some(addr), Some((_, action))) = (self.selected, self.action()) else { return };
        match action {
            Action::Connect => bt::connect(addr),
            Action::Disconnect => bt::disconnect(addr),
        }
        self.refresh();
    }

    fn status_text(&self) -> String {
        if !bt::present() {
            return bt::status();
        }
        if bt::scanning() {
            return "Looking for devices nearby\u{2026}".into();
        }
        match self.list.len() {
            0 => "Nothing found. Put the device into pairing mode and press Scan".into(),
            n => format!("{} \u{b7} new device: pairing mode on it, then Scan", ui::items(n).replace("item", "device")),
        }
    }
}

/// What kind of device it is — by class (classic Bluetooth) or "LE".
fn kind(e: &Entry) -> &'static str {
    if e.dev.le {
        return "LE";
    }
    match (e.dev.class >> 8) & 0x1F {
        1 => "computer",
        2 => "phone",
        4 => "audio",
        5 => "keyboard / mouse",
        _ => "classic",
    }
}

impl App for BtSettings {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        let off = |r: Rect| r.offset(a.x, a.y);
        // Status on the left, Scan on the right: the status is trimmed so it does not go under the button.
        let avail = self.scan_button().x - PAD - 12;
        c.text(a.x + PAD, a.y + HEAD + 4, &ui::fit(&self.status_text(), FontId::Regular, 14.0, avail), TEXT, FontId::Regular, 14.0);
        ui::button(c, off(self.scan_button()), "Scan", bt::present() && !bt::scanning());

        if let Some((e, n)) = self.confirming() {
            let b = off(self.banner());
            c.fill_round(b, 8, ACCENT, 40);
            c.stroke_round(b, 8, ACCENT, 160);
            let name = if e.dev.name.is_empty() { bt::addr_text(&e.dev.addr) } else { e.dev.name.clone() };
            let line = ui::fit(&format!("Pair with {name}?"), FontId::SemiBold, 14.0, b.w - 20);
            c.text(b.x + 10, b.y + 8, &line, TEXT, FontId::SemiBold, 14.0);
            c.text(b.x + 10, b.y + 30, &format!("Does it show {n:06}?"), TEXT, FontId::Regular, 14.0);
            let (yes, no) = self.banner_buttons();
            ui::button(c, off(no), "Cancel", true);
            ui::button(c, off(yes), "Pair", true);
        }

        for (i, e) in self.list.iter().take(self.rows()).enumerate() {
            let r = off(self.row(i));
            let connected = e.link == Some(Link::Connected);
            if connected {
                c.fill_round(r, 7, ACCENT, 48);
            }
            if self.selected == Some(e.dev.addr) {
                c.fill_round(r, 7, 0xFFFFFF, 26);
            }
            // Without a name — say so (the address is on the second line).
            let unnamed = e.dev.name.is_empty();
            let name = if unnamed { String::from("Unnamed device") } else { e.dev.name.clone() };
            let detail = if e.dev.rssi != 0 { format!("{}  {} dBm", kind(e), e.dev.rssi) } else { String::from(kind(e)) };
            let dw = crate::font::measure(FontId::Regular, 13.0, &detail);
            c.text(r.x + r.w - dw - 10, r.y + 7, &detail, TEXT_DIM, FontId::Regular, 13.0);
            let name = ui::fit(&name, FontId::Regular, 14.0, r.w - dw - 34);
            let color = if connected {
                ACCENT
            } else if unnamed {
                TEXT_DIM
            } else {
                TEXT
            };
            c.text(r.x + 10, r.y + 6, &name, color, FontId::Regular, 14.0);
            // The second line: what is up with the link; otherwise "paired" or the address.
            let (second, color) = match &e.link {
                Some(Link::Failed(why)) => (why.clone(), RED),
                Some(l) => (bt::link_text(l), ACCENT),
                None if e.paired => (format!("paired \u{b7} {}", bt::addr_text(&e.dev.addr)), TEXT_DIM),
                None => (bt::addr_text(&e.dev.addr), TEXT_DIM),
            };
            c.text(r.x + 10, r.y + 25, &ui::fit(&second, FontId::Regular, 13.0, r.w - 20), color, FontId::Regular, 13.0);
        }

        if let (Some(r), Some((label, _))) = (self.action_button(), self.action()) {
            ui::button(c, off(r), label, true);
        }
        if let Some(r) = self.forget_button() {
            ui::button(c, off(r), "Forget", true);
        }
        if !self.embedded {
            ui::button(c, off(self.close_button()), "Close", true);
        }
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        match ch {
            ESC => self.done = !self.embedded,
            b'\n' => self.act(),
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if !pressed || buttons & LEFT == 0 {
            return false;
        }
        if self.close_button().contains(x, y) {
            self.done = true;
        } else if self.scan_button().contains(x, y) {
            Self::scan();
        } else if let Some((e, _)) = self.confirming() {
            let addr = e.dev.addr;
            let (yes, no) = self.banner_buttons();
            if yes.contains(x, y) {
                bt::confirm(addr, true);
            } else if no.contains(x, y) {
                bt::confirm(addr, false);
            } else if !self.select_at(x, y) && !self.foot_click(x, y) {
                return false;
            }
        } else if !self.select_at(x, y) && !self.foot_click(x, y) {
            return false;
        }
        self.refresh();
        true
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        // Every half second — the list and the link state.
        if ctx.ticks < self.checked + crate::interrupts::HZ / 2 {
            return false;
        }
        self.checked = ctx.ticks;
        self.refresh()
    }

    fn finished(&self) -> bool {
        self.done
    }
}

impl BtSettings {
    fn select_at(&mut self, x: i32, y: i32) -> bool {
        let n = self.list.len().min(self.rows());
        match (0..n).find(|&i| self.row(i).contains(x, y)) {
            Some(i) => {
                self.selected = Some(self.list[i].dev.addr);
                true
            }
            None => false,
        }
    }

    fn foot_click(&mut self, x: i32, y: i32) -> bool {
        if self.action_button().is_some_and(|r| r.contains(x, y)) {
            self.act();
            true
        } else if let (Some(r), Some(addr)) = (self.forget_button(), self.selected) {
            if r.contains(x, y) {
                bt::forget(addr);
                true
            } else {
                false
            }
        } else {
            false
        }
    }
}
