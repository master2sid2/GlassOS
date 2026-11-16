//! The "Add language" window from the Keyboard section in Settings.

use super::ui;
use crate::font::FontId;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::keyboard::{self, LAYOUTS};
use crate::wm::{App, Ctx, TEXT, TEXT_DIM};
use alloc::vec::Vec;

const BG: Color = 0x1C1F2E;
const PAD: i32 = 16;
const ROW_H: i32 = 38;
pub const W: i32 = 320;

pub struct LanguagePicker {
    /// Languages that are not in the list yet.
    free: Vec<usize>,
    cw: i32,
    ch: i32,
    hovered: Option<usize>,
    done: bool,
}

impl LanguagePicker {
    pub fn new() -> Self {
        let on = keyboard::enabled();
        Self { free: (0..LAYOUTS.len()).filter(|i| !on.contains(i)).collect(), cw: W, ch: 0, hovered: None, done: false }
    }

    pub fn height(&self) -> i32 {
        self.free.len().max(1) as i32 * ROW_H + 2 * PAD + ui::BUTTON_H + 66
    }

    fn row(&self, i: usize) -> Rect {
        Rect::new(PAD, PAD + i as i32 * ROW_H, self.cw - 2 * PAD, ROW_H - 4)
    }

    fn cancel_button(&self) -> Rect {
        let w = ui::button_width("Cancel");
        Rect::new(self.cw - PAD - w, self.ch - PAD - ui::BUTTON_H, w, ui::BUTTON_H)
    }
}

impl App for LanguagePicker {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        if self.free.is_empty() {
            c.text(a.x + PAD, a.y + PAD + 8, "All languages are already added.", TEXT_DIM, FontId::Regular, 14.0);
        }
        for (i, &l) in self.free.iter().enumerate() {
            let r = self.row(i).offset(a.x, a.y);
            c.fill_round(r, 8, 0xFFFFFF, if self.hovered == Some(i) { 34 } else { 12 });
            c.text(r.x + 12, r.y + 9, LAYOUTS[l].name, TEXT, FontId::Regular, 14.0);
        }
        ui::button(c, self.cancel_button().offset(a.x, a.y), "Cancel", true);
    }

    fn hover(&mut self, x: i32, y: i32) -> bool {
        let now = (0..self.free.len()).find(|&i| self.row(i).contains(x, y));
        let changed = now != self.hovered;
        self.hovered = now;
        changed
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if !pressed || buttons & LEFT == 0 {
            return false;
        }
        if let Some(i) = (0..self.free.len()).find(|&i| self.row(i).contains(x, y)) {
            keyboard::add(self.free[i]);
            self.done = true;
        } else if self.cancel_button().contains(x, y) {
            self.done = true;
        }
        false
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        self.done |= ch == 0x1B;
        false
    }

    fn finished(&self) -> bool {
        self.done
    }
}
