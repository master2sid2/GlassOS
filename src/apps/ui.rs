//! Shared UI elements of applications: buttons, sliders, colour swatches, icons.

use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect};
use crate::wm::{ACCENT, TEXT, TEXT_DIM};
use alloc::string::String;
use crate::sync::IrqLock;
use alloc::vec::Vec;

pub const BUTTON_H: i32 = 28;

/// «1 item», «5 items».
pub fn items(n: usize) -> String {
    alloc::format!("{n} item{}", if n == 1 { "" } else { "s" })
}

/// Text truncated with an ellipsis to fit into `width` pixels.
pub fn fit(text: &str, font_id: FontId, size: f32, width: i32) -> String {
    if font::measure(font_id, size, text) <= width {
        return text.into();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let cut: String = chars.iter().collect::<String>().trim_end().into();
        let candidate = alloc::format!("{cut}\u{2026}");
        if font::measure(font_id, size, &candidate) <= width {
            return candidate;
        }
    }
    String::new()
}

pub fn button_width(label: &str) -> i32 {
    font::measure(FontId::Regular, 13.0, label) + 24
}

/// Lays out buttons in a row from left to right.
pub fn button_row(labels: &[&str], x: i32, y: i32) -> Vec<Rect> {
    let mut x = x;
    labels
        .iter()
        .map(|label| {
            let r = Rect::new(x, y, button_width(label), BUTTON_H);
            x += r.w + 6;
            r
        })
        .collect()
}

/// The cursor for the buttons of the window being drawn now (in its canvas coordinates),
/// and whether the left button is held over it. Buttons drawn during the window's paint
/// are remembered — the WM uses them to highlight the button under the cursor and to defer a click
/// until release.
struct Pointer {
    at: Option<(i32, i32)>,
    down: bool,
    collect: bool,
    buttons: Vec<Rect>,
}

static POINTER: IrqLock<Pointer> = IrqLock::new(Pointer { at: None, down: false, collect: false, buttons: Vec::new() });

/// WM: before painting a window — where the cursor is and whether a mouse button is held.
pub fn begin(at: Option<(i32, i32)>, down: bool) {
    let mut p = POINTER.lock();
    (p.at, p.down, p.collect) = (at, down, true);
    p.buttons.clear();
}

/// WM: after painting — where the window drew (active) buttons.
pub fn end() -> Vec<Rect> {
    let mut p = POINTER.lock();
    (p.at, p.down, p.collect) = (None, false, false);
    core::mem::take(&mut p.buttons)
}

/// Button state: (under the cursor, pressed); also remembers the button.
fn state(r: Rect, enabled: bool) -> (bool, bool) {
    let mut p = POINTER.lock();
    if !p.collect || !enabled {
        return (false, false);
    }
    p.buttons.push(r);
    let hover = p.at.is_some_and(|(x, y)| r.contains(x, y));
    (hover, hover && p.down)
}

pub fn button(c: &mut Canvas, r: Rect, label: &str, enabled: bool) {
    let (hover, pressed) = state(r, enabled);
    let fill = if pressed { 0x262938 } else if hover { 0x41465E } else { 0x32364A };
    c.fill_round(r, r.h / 2, fill, 256);
    c.stroke_round(r, r.h / 2, 0xFFFFFF, if hover { 50 } else { 22 });
    let tw = font::measure(FontId::Regular, 13.0, label);
    let color = if enabled { TEXT } else { 0x6B7088 };
    // A pressed button is "sunk": the label is one pixel lower.
    c.text(r.x + (r.w - tw) / 2, r.y + (r.h - 16) / 2 + pressed as i32, label, color, FontId::Regular, 13.0);
}

/// The main (accent) button of a dialog.
pub fn accent_button(c: &mut Canvas, r: Rect, label: &str, enabled: bool) {
    let (hover, pressed) = state(r, enabled);
    let alpha = if !enabled { 90 } else if pressed { 170 } else if hover { 256 } else { 225 };
    c.fill_round(r, r.h / 2, ACCENT, alpha);
    if hover && !pressed {
        c.stroke_round(r, r.h / 2, 0xFFFFFF, 90);
    }
    let tw = font::measure(FontId::SemiBold, 13.0, label);
    c.text(r.x + (r.w - tw) / 2, r.y + (r.h - 16) / 2 + pressed as i32, label, 0x0B1020, FontId::SemiBold, 13.0);
}

/// A button for a dangerous action (delete, erase) — red.
pub fn danger_button(c: &mut Canvas, r: Rect, label: &str, enabled: bool) {
    let (hover, pressed) = state(r, enabled);
    let alpha = if !enabled { 90 } else if pressed { 170 } else if hover { 256 } else { 215 };
    c.fill_round(r, r.h / 2, 0xF87171, alpha);
    if hover && !pressed {
        c.stroke_round(r, r.h / 2, 0xFFFFFF, 90);
    }
    let tw = font::measure(FontId::SemiBold, 13.0, label);
    c.text(r.x + (r.w - tw) / 2, r.y + (r.h - 16) / 2 + pressed as i32, label, 0x1C1F2E, FontId::SemiBold, 13.0);
}

/// A round colour swatch; the selected one has an accent ring.
pub fn swatch(c: &mut Canvas, r: Rect, color: Color, selected: bool, bg: Color) {
    if selected {
        c.fill_round(r.inset(-4), r.w / 2 + 4, ACCENT, 256);
        c.fill_round(r.inset(-2), r.w / 2 + 2, bg, 256);
    }
    c.fill_round(r, r.w / 2, color, 256);
    c.stroke_round(r, r.w / 2, 0xFFFFFF, 40);
}

/// An on/off switch.
pub fn switch(c: &mut Canvas, r: Rect, on: bool) {
    let color = if on { ACCENT } else { 0x3A3F55 };
    c.fill_round(r, r.h / 2, color, 256);
    let d = r.h - 6;
    let x = if on { r.x + r.w - d - 3 } else { r.x + 3 };
    let knob = Rect::new(x, r.y + 3, d, d);
    c.fill_round(knob.offset(0, 1), d / 2, 0x000000, 80);
    c.fill_round(knob, d / 2, 0xFFFFFF, 256);
}

pub fn slider(c: &mut Canvas, r: Rect, value: u32, min: u32, max: u32) {
    let track = Rect::new(r.x, r.y + r.h / 2 - 3, r.w, 6);
    c.fill_round(track, 3, 0x3A3F55, 256);
    let knob_x = r.x + ((value - min) * r.w as u32 / (max - min)) as i32;
    c.fill_round(Rect::new(track.x, track.y, knob_x - r.x, track.h), 3, ACCENT, 256);
    let knob = Rect::new(knob_x - 9, r.y + r.h / 2 - 9, 18, 18);
    c.fill_round(knob.offset(0, 1), 9, 0x000000, 90);
    c.fill_round(knob, 9, 0xFFFFFF, 256);
}

pub fn slider_value(r: Rect, x: i32, min: u32, max: u32) -> u32 {
    let t = (x - r.x).clamp(0, r.w) as u32;
    min + t * (max - min) / r.w as u32
}

const SAVE_PROMPT: (i32, i32) = (400, 128);

fn save_prompt_rect(area: (i32, i32)) -> Rect {
    Rect::new((area.0 - SAVE_PROMPT.0) / 2, (area.1 - SAVE_PROMPT.1) / 2 - 20, SAVE_PROMPT.0, SAVE_PROMPT.1)
}

/// Buttons for the unsaved-changes question: "Save", "Don't save", "Cancel".
/// `area` is the size of the window's client area.
pub fn save_prompt_buttons(area: (i32, i32)) -> [Rect; 3] {
    let r = save_prompt_rect(area);
    let (save, skip, cancel) = (button_width("Save"), button_width("Don't Save"), button_width("Cancel"));
    let y = r.y + r.h - BUTTON_H - 16;
    let save_x = r.x + r.w - 18 - save;
    let skip_x = save_x - 8 - skip;
    [Rect::new(save_x, y, save, BUTTON_H), Rect::new(skip_x, y, skip, BUTTON_H), Rect::new(r.x + 18, y, cancel, BUTTON_H)]
}

/// A question over the window: whether to save changes to `name`.
pub fn draw_save_prompt(c: &mut Canvas, a: Rect, area: (i32, i32), name: &str) {
    c.blend(a, 0x000000, 110);
    let r = save_prompt_rect(area).offset(a.x, a.y);
    c.shadow(r, 12, 18, 6, 150);
    c.fill_round(r, 12, 0x232738, 256);
    c.stroke_round(r, 12, 0xFFFFFF, 35);
    c.text(r.x + 18, r.y + 16, &alloc::format!("Save changes to {name}?"), TEXT, FontId::SemiBold, 15.0);
    c.text(r.x + 18, r.y + 42, "Otherwise your changes will be lost.", TEXT_DIM, FontId::Regular, 13.0);
    let [save, skip, cancel] = save_prompt_buttons(area).map(|b| b.offset(a.x, a.y));
    accent_button(c, save, "Save", true);
    button(c, skip, "Don't Save", true);
    button(c, cancel, "Cancel", true);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Folder,
    File,
    Program,
    Audio,
    Picture,
}

/// File icon by name: folder, program, music, picture or just a file.
pub fn icon_for(name: &str, is_dir: bool) -> Icon {
    if is_dir {
        Icon::Folder
    } else if crate::lua::is_program(name) {
        Icon::Program
    } else if crate::audio::is_audio_file(name) {
        Icon::Audio
    } else if crate::image::is_picture(name) {
        Icon::Picture
    } else {
        Icon::File
    }
}

/// An icon `s` pixels in size (drawn for 20, the rest is scaling).
pub fn icon(c: &mut Canvas, x: i32, y: i32, s: i32, kind: Icon) {
    let k = |v: i32| v * s / 20;
    match kind {
        Icon::Folder => {
            c.fill_round(Rect::new(x, y + k(1), k(9), k(6)), k(2), 0xD9A53A, 256);
            c.fill_round(Rect::new(x, y + k(4), s, k(14)), k(3), 0xF2C14E, 256);
            c.fill(Rect::new(x + k(1), y + k(7), s - k(2), k(1).max(1)), 0xFBD87A);
        }
        Icon::Program => {
            // A blue square with a "play" triangle.
            c.fill_round(Rect::new(x, y, s, s), k(5), 0x3B82F6, 256);
            let h0 = k(9);
            for i in 0..h0 {
                let h = h0 - i;
                c.fill(Rect::new(x + k(7) + i, y + k(10) - h / 2, 1, h), 0xFFFFFF);
            }
        }
        Icon::Audio => {
            // A pink square with a note.
            c.fill_round(Rect::new(x, y, s, s), k(5), 0xE0508F, 256);
            c.fill_round(Rect::new(x + k(4), y + k(11), k(7), k(6)), k(3), 0xFFFFFF, 256);
            c.fill(Rect::new(x + k(9), y + k(3), k(2).max(1), k(11)), 0xFFFFFF);
            c.fill(Rect::new(x + k(9), y + k(3), k(6), k(2).max(1)), 0xFFFFFF);
            c.fill(Rect::new(x + k(13), y + k(3), k(2).max(1), k(4)), 0xFFFFFF);
        }
        Icon::Picture => {
            // A green square: sun and mountain.
            c.fill_round(Rect::new(x, y, s, s), k(5), 0x2E9E6E, 256);
            c.fill_round(Rect::new(x + k(12), y + k(3), k(5), k(5)), k(3), 0xFDE68A, 256);
            let (base, peak) = (y + k(16), k(9));
            for i in 0..peak {
                c.fill(Rect::new(x + k(9) - i, base - peak + i, 2 * i + 1, 1), 0xFFFFFF);
            }
            c.fill(Rect::new(x + k(3), base, s - k(6), k(1).max(1)), 0xFFFFFF);
        }
        Icon::File => {
            c.fill_round(Rect::new(x + k(2), y, k(15), k(19)), k(3), 0xDDE3F0, 256);
            c.fill(Rect::new(x + k(12), y, k(5), k(5)), 0xA9B2C8);
            for i in 0..3 {
                c.fill(Rect::new(x + k(5), y + k(8) + i * k(3), k(9), k(1).max(1)), 0x9AA3BA);
            }
        }
    }
}

pub fn folder_icon(c: &mut Canvas, x: i32, y: i32) {
    icon(c, x, y, 20, Icon::Folder);
}

pub fn file_icon(c: &mut Canvas, x: i32, y: i32) {
    icon(c, x, y, 20, Icon::File);
}

const MENU_BG: Color = 0x232738;
const MENU_BLUR: i32 = 6;
const MENU_ITEM_H: i32 = 28;
const MENU_W: i32 = 190;
const MENU_GAP: i32 = 9;

/// A window's context menu. Item: (label, action, enabled); an empty label is a separator.
/// Labels are static strings or (`L = String`) built on the fly.
pub struct Menu<A, L = &'static str> {
    x: i32,
    y: i32,
    pub items: Vec<(L, A, bool)>,
    /// The item under the cursor.
    pub hover: Option<usize>,
}

impl<A, L: AsRef<str>> Menu<A, L> {
    /// A menu at point (x, y), shifted to fit into a cw x ch window.
    pub fn new(x: i32, y: i32, items: Vec<(L, A, bool)>, cw: i32, ch: i32) -> Self {
        let mut m = Self { x, y, items, hover: None };
        let r = m.rect();
        m.x = x.min(cw - r.w - 4).max(4);
        m.y = y.min(ch - r.h - 4).max(4);
        m
    }

    fn height(label: &str) -> i32 {
        if label.is_empty() { MENU_GAP } else { MENU_ITEM_H }
    }

    pub fn rect(&self) -> Rect {
        Rect::new(self.x, self.y, MENU_W, self.items.iter().map(|i| Self::height(i.0.as_ref())).sum::<i32>() + 10)
    }

    pub fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        let r = self.rect();
        if !r.contains(x, y) {
            return None;
        }
        let mut top = r.y + 5;
        for (i, item) in self.items.iter().enumerate() {
            let h = Self::height(item.0.as_ref());
            if y >= top && y < top + h {
                return Some(i);
            }
            top += h;
        }
        None
    }

    /// `a` is the window's client area; `danger` — items that destroy something.
    pub fn draw(&self, c: &mut Canvas, a: Rect, danger: impl Fn(&A) -> bool) {
        let r = self.rect().offset(a.x, a.y);
        c.shadow(r, 10, 16, 4, 140);
        c.blur_round(r, MENU_BLUR, 10);
        c.fill_round(r, 10, MENU_BG, crate::theme::get().menu_alpha);
        c.stroke_round(r, 10, 0xFFFFFF, 35);
        let mut y = r.y + 5;
        for (n, (label, action, enabled)) in self.items.iter().enumerate() {
            let label = label.as_ref();
            if label.is_empty() {
                c.fill(Rect::new(r.x + 10, y + 4, r.w - 20, 1), 0x3A3F55);
                y += MENU_GAP;
                continue;
            }
            if *enabled && self.hover == Some(n) {
                c.fill_round(Rect::new(r.x + 5, y, r.w - 10, MENU_ITEM_H), 6, ACCENT, 70);
            }
            let color = if !enabled { 0x6B7088 } else if danger(action) { 0xF87171 } else { TEXT };
            c.text(r.x + 16, y + 6, label, color, FontId::Regular, 13.5);
            y += MENU_ITEM_H;
        }
    }
}

/// A tab strip: layout (for mouse hit testing) and painting.
pub struct TabBar {
    /// Tabs and the close crosses on them.
    pub tabs: Vec<(Rect, Rect)>,
    /// The "+" button (new tab); permanent tabs do not have it (an empty rectangle).
    pub add: Rect,
    /// Tabs can be added and closed (terminal) or they are permanent (settings).
    closable: bool,
}

pub const TAB_BAR_H: i32 = 34;

impl TabBar {
    /// Splits strip `r` evenly between tabs (no wider than 200 pixels), with "+" on the right.
    pub fn layout(r: Rect, count: usize) -> TabBar {
        let n = count.max(1) as i32;
        let add_w = 30;
        let tab_w = ((r.w - add_w - 12 - 4 * n) / n).clamp(60, 200);
        let tabs = (0..n)
            .map(|i| {
                let t = Rect::new(r.x + 6 + i * (tab_w + 4), r.y + 5, tab_w, r.h - 8);
                (t, Rect::new(t.x + t.w - 24, t.y + (t.h - 18) / 2, 18, 18))
            })
            .collect();
        let add = Rect::new(r.x + 6 + n * (tab_w + 4), r.y + 5, add_w, r.h - 8);
        TabBar { tabs, add, closable: true }
    }

    /// Permanent tabs (settings sections): no "+" and no crosses, full width.
    pub fn fixed(r: Rect, count: usize) -> TabBar {
        let n = count.max(1) as i32;
        let tab_w = (r.w - 12 - 4 * (n - 1)) / n;
        let tabs = (0..n)
            .map(|i| (Rect::new(r.x + 6 + i * (tab_w + 4), r.y + 5, tab_w, r.h - 8), Rect::new(0, 0, 0, 0)))
            .collect();
        TabBar { tabs, add: Rect::new(0, 0, 0, 0), closable: false }
    }

    /// Draws the tabs over the window background (the strip is slightly darker — glass stays glass);
    /// the active tab is lighter and underlined. Crosses only if there is more than one tab.
    pub fn draw(&self, c: &mut Canvas, r: Rect, labels: &[&str], active: usize) {
        c.blend(r, 0x000000, 60);
        c.blend(Rect::new(r.x, r.y + r.h - 1, r.w, 1), 0xFFFFFF, 30);
        let saved = c.clip();
        for (i, (&(tab, close), label)) in self.tabs.iter().zip(labels).enumerate() {
            let on = i == active;
            c.fill_round(tab, 8, 0xFFFFFF, if on { 34 } else { 12 });
            if on {
                c.fill_round(Rect::new(tab.x + 10, tab.y + tab.h - 3, tab.w - 20, 2), 1, ACCENT, 256);
            }
            let color = if on { TEXT } else { 0x9AA0B8 };
            if self.closable {
                c.set_clip(Rect::new(tab.x, tab.y, tab.w - 28, tab.h).intersect(saved));
                c.text(tab.x + 12, tab.y + (tab.h - 17) / 2, label, color, FontId::Regular, 13.0);
            } else {
                // Permanent tabs — the label is centred.
                let w = font::measure(FontId::Regular, 13.0, label);
                c.text(tab.x + (tab.w - w) / 2, tab.y + (tab.h - 17) / 2, label, color, FontId::Regular, 13.0);
            }
            c.set_clip(saved);
            if self.closable && labels.len() > 1 {
                let (cx, cy) = (close.x + 9, close.y + 9);
                for d in -3..=3 {
                    c.fill(Rect::new(cx + d, cy + d, 2, 2), if on { TEXT } else { 0x7A8098 });
                    c.fill(Rect::new(cx + d, cy - d, 2, 2), if on { TEXT } else { 0x7A8098 });
                }
            }
        }
        if !self.closable {
            return;
        }
        c.fill_round(self.add, 8, 0xFFFFFF, 12);
        let (ax, ay) = (self.add.x + self.add.w / 2, self.add.y + self.add.h / 2);
        c.fill(Rect::new(ax - 6, ay - 1, 12, 2), TEXT);
        c.fill(Rect::new(ax - 1, ay - 6, 2, 12), TEXT);
    }

    /// What is under the point: Some((tab, on the cross or not)) or None.
    pub fn hit(&self, x: i32, y: i32) -> Option<(usize, bool)> {
        self.tabs.iter().position(|(t, _)| t.contains(x, y)).map(|i| (i, self.closable && self.tabs[i].1.inset(-3).contains(x, y)))
    }
}

/// A single-line text field: cursor anywhere in the line, selection with the mouse and
/// Shift+arrows, double click selects a word, Ctrl+A/C/X/V. Positions are in bytes
/// (always on a character boundary).
/// Scroll bar width.
pub const SCROLL_W: i32 = 8;

/// A scroll bar: `len` lines in total, `rows` visible, the top visible one is `top`.
/// `track` is in window coordinates.
#[derive(Clone, Copy)]
pub struct ScrollBar {
    pub track: Rect,
    pub len: usize,
    pub rows: usize,
    pub top: usize,
}

impl ScrollBar {
    pub fn needed(&self) -> bool {
        self.len > self.rows
    }

    /// The thumb: its height is proportional to the visible part of the list.
    pub fn thumb(&self) -> Rect {
        let t = self.track;
        let (n, rows) = (self.len.max(1) as i32, self.rows as i32);
        let h = (t.h * rows / n).clamp(24.min(t.h), t.h);
        let max = (n - rows).max(1);
        Rect::new(t.x, t.y + (t.h - h) * self.top as i32 / max, t.w, h)
    }

    /// The top line at which the top of the thumb is at point `y`.
    pub fn top_at(&self, y: i32) -> usize {
        let (t, th) = (self.track, self.thumb());
        let max = self.len.saturating_sub(self.rows) as i32;
        let room = (t.h - th.h).max(1);
        ((((y - t.y).clamp(0, room) * max + room / 2) / room) as usize).min(max as usize)
    }

    /// A press on the bar: how far below the thumb's top it was grabbed
    /// (a press outside the thumb centres it under the mouse). None — outside the bar.
    pub fn grab(&self, x: i32, y: i32) -> Option<i32> {
        let thumb = self.thumb();
        (self.needed() && self.track.inset(-4).contains(x, y)).then(|| if thumb.inset(-4).contains(x, y) { y - thumb.y } else { thumb.h / 2 })
    }

    /// `a` is the client area `track` is measured from.
    pub fn draw(&self, c: &mut Canvas, a: Rect, active: bool) {
        if self.needed() {
            c.fill_round(self.track.offset(a.x, a.y), SCROLL_W / 2, 0xFFFFFF, 14);
            c.fill_round(self.thumb().offset(a.x, a.y), SCROLL_W / 2, 0xFFFFFF, if active { 200 } else { 110 });
        }
    }
}

pub struct TextField {
    pub text: String,
    caret: usize,
    /// The other end of the selection (None or equal to the cursor — no selection).
    anchor: Option<usize>,
    /// Horizontal text scroll so the cursor stays visible (changes when painting).
    scroll: core::cell::Cell<i32>,
    drag: bool,
    last_click: u64,
    pub max: usize,
    /// Password: dots instead of characters.
    pub secret: bool,
    pub size: f32,
}

impl TextField {
    pub fn new(text: &str, max: usize) -> Self {
        Self { text: text.into(), caret: text.len(), anchor: None, scroll: core::cell::Cell::new(0), drag: false, last_click: 0, max, secret: false, size: 13.5 }
    }

    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    /// New text (cursor at the end, selection cleared).
    pub fn set_text(&mut self, s: &str) {
        self.text = s.into();
        self.caret = s.len();
        self.anchor = None;
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.caret = self.text.len();
    }

    /// Select from the start of the line to `end` (for example, a file name without extension).
    pub fn select_to(&mut self, end: usize) {
        self.anchor = Some(0);
        self.caret = end.min(self.text.len());
    }

    fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor.filter(|&a| a != self.caret)?;
        Some((a.min(self.caret), a.max(self.caret)))
    }

    fn delete_selection(&mut self) -> bool {
        // An empty selection is cleared too: a click sets the anchor, and without this the first
        // typed letter would end up selected and the second would replace it.
        let Some((a, b)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        self.text.replace_range(a..b, "");
        self.caret = a;
        self.anchor = None;
        true
    }

    fn insert(&mut self, s: &str) {
        self.delete_selection();
        let room = self.max.saturating_sub(self.text.len());
        let mut end = s.len().min(room);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.text.insert_str(self.caret, &s[..end]);
        self.caret += end;
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().next_back().map_or(0, |(k, _)| k)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    /// Word boundary to the left / right (Ctrl+arrows, double click).
    fn word_left(&self, i: usize) -> usize {
        let s = &self.text[..i];
        let s = s.trim_end_matches(|c: char| !c.is_alphanumeric());
        s.trim_end_matches(|c: char| c.is_alphanumeric()).len()
    }

    fn word_right(&self, i: usize) -> usize {
        let s = &self.text[i..];
        let rest = s.trim_start_matches(|c: char| !c.is_alphanumeric());
        let rest = rest.trim_start_matches(|c: char| c.is_alphanumeric());
        self.text.len() - rest.len()
    }

    /// Move the cursor; with `extend`, extending the selection.
    fn move_to(&mut self, i: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = i;
    }

    /// A key. `accept` — which characters may be typed. true — handled
    /// (the field leaves Enter, Esc and Tab alone — the owner handles them).
    pub fn key(&mut self, ch: u8, accept: impl Fn(char) -> bool) -> bool {
        use crate::input::{KEY_DELETE, KEY_END, KEY_HOME, KEY_LEFT, KEY_RIGHT, MOD_CTRL, SHIFTED_NAV};
        let ctrl = crate::input::mods() & MOD_CTRL != 0;
        let (base, extend) = if (KEY_LEFT + SHIFTED_NAV..=KEY_END + SHIFTED_NAV).contains(&ch) { (ch - SHIFTED_NAV, true) } else { (ch, false) };
        match base {
            KEY_LEFT => {
                let to = match self.selection() {
                    Some((a, _)) if !extend && !ctrl => a,
                    _ if ctrl => self.word_left(self.caret),
                    _ => self.prev(self.caret),
                };
                self.move_to(to, extend);
            }
            KEY_RIGHT => {
                let to = match self.selection() {
                    Some((_, b)) if !extend && !ctrl => b,
                    _ if ctrl => self.word_right(self.caret),
                    _ => self.next(self.caret),
                };
                self.move_to(to, extend);
            }
            KEY_HOME => self.move_to(0, extend),
            KEY_END => self.move_to(self.text.len(), extend),
            0x08 => {
                if !self.delete_selection() && self.caret > 0 {
                    let from = if ctrl { self.word_left(self.caret) } else { self.prev(self.caret) };
                    self.text.replace_range(from..self.caret, "");
                    self.caret = from;
                }
            }
            KEY_DELETE => {
                if !self.delete_selection() && self.caret < self.text.len() {
                    let to = self.next(self.caret);
                    self.text.replace_range(self.caret..to, "");
                }
            }
            0x01 => self.select_all(),
            0x03 | 0x18 if !self.secret => {
                let (a, b) = self.selection().unwrap_or((0, self.text.len()));
                crate::wm::set_clipboard(&self.text[a..b]);
                if ch == 0x18 {
                    if self.selection().is_none() {
                        self.select_all();
                    }
                    self.delete_selection();
                }
            }
            0x16 => {
                let paste: String = crate::wm::clipboard().chars().filter(|&c| c != '\n' && accept(c)).collect();
                self.insert(&paste);
            }
            0x20..=0x7E if accept(ch as char) => {
                let mut buf = [0u8; 4];
                self.insert((ch as char).encode_utf8(&mut buf));
            }
            _ => return false,
        }
        true
    }

    /// A non-ASCII character typed in another language layout.
    pub fn type_char(&mut self, c: char, accept: impl Fn(char) -> bool) -> bool {
        if !accept(c) {
            return false;
        }
        let mut buf = [0u8; 4];
        self.insert(c.encode_utf8(&mut buf));
        true
    }

    /// How the text looks on screen (dots for a password).
    fn shown<'a>(&self, s: &'a str) -> alloc::borrow::Cow<'a, str> {
        if self.secret { "\u{2022}".repeat(s.chars().count()).into() } else { s.into() }
    }

    fn px(&self, i: usize) -> i32 {
        font::measure(FontId::Regular, self.size, &self.shown(&self.text[..i]))
    }

    /// The position in the text under coordinate `x` (the field is `r`).
    fn hit(&self, r: Rect, x: i32) -> usize {
        let x = x - (r.x + 10 - self.scroll.get());
        let mut best = (0, i32::MAX);
        for i in self.text.char_indices().map(|(k, _)| k).chain(core::iter::once(self.text.len())) {
            let d = (self.px(i) - x).abs();
            if d < best.1 {
                best = (i, d);
            }
        }
        best.0
    }

    /// Mouse: a press in the field places the cursor (with Shift, selects up to it),
    /// dragging selects, a double click selects a word. `r` is in mouse coordinates.
    pub fn mouse(&mut self, r: Rect, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        use crate::input::LEFT;
        if buttons & LEFT == 0 {
            let was = self.drag;
            self.drag = false;
            return was;
        }
        if !pressed {
            if self.drag {
                self.caret = self.hit(r, x);
                return true;
            }
            return false;
        }
        if !r.contains(x, y) {
            return false;
        }
        let i = self.hit(r, x);
        let now = crate::interrupts::ticks();
        if now - self.last_click < crate::interrupts::HZ * 2 / 5 {
            // Double click: the word under the cursor.
            let start = self.word_left(self.next(i).min(self.text.len()));
            let end = self.word_right(start);
            self.anchor = Some(start.min(i));
            self.caret = end.max(i);
            self.last_click = 0;
            return true;
        }
        self.last_click = now;
        let shift = crate::input::mods() & crate::input::MOD_SHIFT != 0;
        self.move_to(i, shift);
        if !shift {
            self.anchor = Some(i);
        }
        self.drag = true;
        true
    }

    /// Draws the field contents (the owner draws the frame): text, selection, cursor.
    pub fn draw(&self, c: &mut Canvas, r: Rect, focused: bool, caret_on: bool, hint: &str, color: Color) {
        let room = r.w - 20;
        let caret_px = self.px(self.caret);
        let total = self.px(self.text.len());
        let mut scroll = self.scroll.get();
        if caret_px - scroll > room {
            scroll = caret_px - room;
        } else if caret_px < scroll {
            scroll = caret_px;
        }
        let scroll = scroll.min((total - room).max(0)).max(0);
        self.scroll.set(scroll);
        let saved = c.clip();
        c.set_clip(r.inset(2).intersect(saved));
        let (x, ty) = (r.x + 10 - scroll, r.y + (r.h - (self.size as i32 + 4)) / 2);
        if self.text.is_empty() && !hint.is_empty() {
            c.text(r.x + 10, ty, hint, TEXT_DIM, FontId::Regular, self.size);
        }
        if focused && let Some((a, b)) = self.selection() {
            let (xa, xb) = (self.px(a), self.px(b));
            c.fill_round(Rect::new(x + xa, r.y + 5, xb - xa, r.h - 10), 3, ACCENT, 90);
        }
        c.text(x, ty, &self.shown(&self.text), color, FontId::Regular, self.size);
        if focused && caret_on {
            c.fill(Rect::new(x + caret_px, r.y + 6, 2, r.h - 12), ACCENT);
        }
        c.set_clip(saved);
    }
}
