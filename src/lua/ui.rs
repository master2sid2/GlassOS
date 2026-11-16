//! Widgets and drawing for Lua programs. The program itself lives in a
//! ring 3 process (see /bin/lua), and each widget is a system call: the kernel draws it into
//! the window's back buffer and returns the new value. When it finishes a frame, the program swaps
//! the buffers. The compositor shows the last finished frame and does not wait for the
//! program, so a hung program does not stop the desktop.

use super::wire::Value;
use super::{Args, LuaWindow};
use crate::apps::dialog::Reply;
use crate::apps::ui as widgets;
use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect};
use crate::input::{KEY_DELETE, KEY_DOWN, KEY_END, KEY_HOME, KEY_LEFT, KEY_RIGHT, KEY_UP, LEFT};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::sync::{IrqLock, YieldLock};
use crate::task;
use crate::wm::{self, ACCENT, App, Ctx, Request, TEXT, TEXT_DIM};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

const PAD: i32 = 16;
const GAP: i32 = 8;
const SPACING: i32 = 10;
const ROW_H: i32 = 30;
const ITEM_H: i32 = 28;
const FIELD_BG: Color = 0x0E1018;
const BORDER: Color = 0x3A3F55;
const DEFAULT_BG: Color = 0x1C1F2E;
const PALETTE: [Color; 10] =
    [0xF87171, 0xFB923C, 0xFACC15, 0x4ADE80, 0x2DD4BF, 0x38BDF8, 0x818CF8, 0xC084FC, 0xF472B6, 0xE5E7EB];

/// Widget state between frames.
#[derive(Default)]
struct UiState {
    /// The text field with keyboard focus.
    focus: Option<String>,
    /// The slider (or text field) being dragged with the mouse.
    active: Option<String>,
    /// Cursor and selection of the focused field: (id, state).
    edit: Option<(String, widgets::TextField)>,
    popup: Option<Popup>,
    /// Ready-to-show ui.image pictures: (data fingerprint, size in pixels, pixels).
    images: Vec<(u64, i32, i32, Arc<Vec<u32>>)>,
    /// ui.list lists: scroll offset and the selected item of the previous frame.
    lists: BTreeMap<String, (i32, i64)>,
}

/// How many different pictures to keep ready (they are not decoded again every frame).
const IMAGE_CACHE: usize = 6;

/// A byte fingerprint (FNV-1a) — to recognise a picture already decoded.
fn fingerprint(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &c| (h ^ c as u64).wrapping_mul(0x0100_0000_01b3)) ^ b.len() as u64
}

/// An open dropdown list.
struct Popup {
    id: String,
    rect: Rect,
    items: Vec<String>,
    selected: Option<usize>,
}

/// The size limit a program may request for a widget or a shape.
const MAX_SIZE: i32 = 8192;

/// The context of one redraw; lives while `app.ui` runs.
pub struct Frame {
    canvas: Canvas,
    state: UiState,
    area: Rect,
    bg: Color,
    y: i32,
    row_y: i32,
    row_h: i32,
    row_right: i32,
    same_line: bool,
    /// A click in canvas coordinates (None if an open list took it).
    click: Option<(i32, i32)>,
    mouse: (i32, i32),
    held: bool,
    keys: Vec<Typed>,
    /// Whose list was open before this frame and which item in it was clicked.
    was_open: Option<String>,
    popup_click: Option<usize>,
    new_popup: Option<Popup>,
    focus_claimed: bool,
    changed: bool,
    cursor_on: bool,
    /// The ui.canvas canvas the gfx.* functions are drawing into now.
    target: Option<Rect>,
    /// The mouse wheel (lines, down is positive): a scrollable widget under the
    /// cursor takes it, the rest scrolls the window.
    wheel: i32,
    /// A partial frame: only this rectangle is redrawn, the rest comes from the previous one.
    part: Option<Rect>,
}

impl Frame {
    fn c(&mut self) -> &mut Canvas {
        &mut self.canvas
    }

    fn st(&mut self) -> &mut UiState {
        &mut self.state
    }

    fn right(&self) -> i32 {
        self.area.x + self.area.w - PAD
    }

    /// How much width is available to the next widget.
    fn avail(&self) -> i32 {
        if self.same_line { self.right() - (self.row_right + GAP) } else { self.area.w - 2 * PAD }
    }

    /// Reserves room for a widget and returns its rectangle.
    fn place(&mut self, w: i32, h: i32) -> Rect {
        let w = w.min(self.avail()).max(0);
        if self.same_line {
            self.same_line = false;
            let r = Rect::new(self.row_right + GAP, self.row_y + (self.row_h - h).max(0) / 2, w, h);
            self.row_h = self.row_h.max(h);
            self.y = self.row_y + self.row_h + SPACING;
            self.row_right = r.x + r.w;
            r
        } else {
            let r = Rect::new(self.area.x + PAD, self.y, w, h);
            self.row_y = self.y;
            self.row_h = h;
            self.row_right = r.x + r.w;
            self.y += h + SPACING;
            r
        }
    }

    fn clicked(&self, r: Rect) -> bool {
        self.click.is_some_and(|(x, y)| r.contains(x, y))
    }

    fn text(&mut self, x: i32, y: i32, s: &str, color: Color, font: FontId, size: f32) -> i32 {
        self.c().text(x, y, s, color, font, size)
    }
}

fn line(c: &mut Canvas, (x0, y0): (i32, i32), (x1, y1): (i32, i32), color: Color) {
    let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
    for i in 0..=steps {
        let (x, y) = (x0 + (x1 - x0) * i / steps, y0 + (y1 - y0) * i / steps);
        c.fill_round(Rect::new(x - 1, y - 1, 3, 3), 1, color, 256);
    }
}

fn field(c: &mut Canvas, r: Rect, active: bool) {
    c.fill_round(r, 8, FIELD_BG, 220);
    c.stroke_round(r, 8, if active { ACCENT } else { BORDER }, 256);
}

/// Dark or light text depending on the background brightness.
fn contrast(bg: Color) -> Color {
    let (r, g, b) = ((bg >> 16) & 0xFF, (bg >> 8) & 0xFF, bg & 0xFF);
    if r * 299 + g * 587 + b * 114 > 150_000 { 0x111318 } else { 0xFFFFFF }
}

// ---------------------------------------------------------------- widgets

fn ui_heading(f: &mut Frame, a: &Args) -> Vec<Value> {
    let text = a.text(1);
    {
        let w = font::measure(FontId::SemiBold, 18.0, &text);
        let r = f.place(w, ROW_H);
        f.text(r.x, r.y + 3, &text, TEXT, FontId::SemiBold, 18.0);
    }
    Vec::new()
}

/// ui.label(text [, color [, width]]) — the width is needed to align a column of labels.
fn ui_label(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (text, color, width) = (a.text(1), a.int(2, TEXT as i64) as Color, a.int(3, 0) as i32);
    {
        let w = font::measure(FontId::Regular, 14.0, &text).max(width);
        let r = f.place(w, ROW_H);
        f.text(r.x, r.y + 6, &text, color, FontId::Regular, 14.0);
    }
    Vec::new()
}

fn ui_separator(f: &mut Frame, _a: &Args) -> Vec<Value> {
    {
        let w = f.avail();
        let r = f.place(w, 9);
        f.c().blend(Rect::new(r.x, r.y + 4, r.w, 1), 0xFFFFFF, 30);
    }
    Vec::new()
}

fn ui_space(f: &mut Frame, a: &Args) -> Vec<Value> {
    let h = a.int(1, 10) as i32;
    {
        f.same_line = false;
        f.y += h;
    }
    Vec::new()
}

fn ui_sameline(f: &mut Frame, _a: &Args) -> Vec<Value> {
    {
        f.same_line = true;
    }
    Vec::new()
}

fn ui_button(f: &mut Frame, a: &Args) -> Vec<Value> {
    let label = a.text(1);
    let r = f.place(widgets::button_width(&label) + 10, ROW_H);
    let hit = f.clicked(r);
    f.changed |= hit;
    widgets::button(f.c(), r, &label, true);
    vec![Value::Bool(hit)]
}

/// ui.input(id, text [, placeholder]) -> text
fn ui_input(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (id, mut value, placeholder) =
        (a.text(1), a.text(2), a.opt(3));
    {
        let w = f.avail();
        let r = f.place(w, ROW_H + 2);
        let mut focused = f.st().focus.as_deref() == Some(id.as_str());
        if f.clicked(r) {
            f.st().focus = Some(id.clone());
            f.focus_claimed = true;
            focused = true;
        }
        if focused {
            // The field state (cursor, selection) lives while it has focus; if
            // the program changed the text itself — start over.
            let mut edit = match f.st().edit.take() {
                Some((i, e)) if i == id && e.text == value => e,
                _ => widgets::TextField::new(&value, 200),
            };
            edit.size = 14.0;
            if let Some((x, y)) = f.click.filter(|&(x, y)| r.contains(x, y)) {
                edit.mouse(r, x, y, LEFT, true);
                f.st().active = Some(id.clone());
            } else if f.st().active.as_deref() == Some(id.as_str()) {
                let (x, y) = f.mouse;
                edit.mouse(r, x, y, if f.held { LEFT } else { 0 }, false);
            }
            for k in core::mem::take(&mut f.keys) {
                match k {
                    Typed::Key(b'\n' | 0x1B) => {
                        f.st().focus = None;
                        focused = false;
                    }
                    Typed::Key(k) => {
                        edit.key(k, |c| !c.is_control());
                    }
                    Typed::Char(c) => {
                        edit.type_char(c, |c| !c.is_control());
                    }
                }
            }
            f.changed |= edit.text != value;
            value = edit.text.clone();
            field(f.c(), r, focused);
            let inner = r.inset(2).intersect(f.area);
            let saved = f.c().clip();
            f.c().set_clip(inner);
            let cursor_on = f.cursor_on;
            edit.draw(f.c(), r, focused, cursor_on, "", TEXT);
            f.c().set_clip(saved);
            if focused {
                f.st().edit = Some((id.clone(), edit));
            }
        } else {
            field(f.c(), r, false);
            let inner = r.inset(2).intersect(f.area);
            f.c().set_clip(inner);
            let x = r.x + 10;
            if value.is_empty() {
                f.text(x, r.y + 7, placeholder.as_deref().unwrap_or(""), 0x5D6380, FontId::Regular, 14.0);
            } else {
                f.text(x, r.y + 7, &value, TEXT, FontId::Regular, 14.0);
            }
        }
        let area = f.area;
        f.c().set_clip(area);
    }
    vec![Value::text(&value)]
}

fn ui_checkbox(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (label, mut value) = (a.text(1), a.flag(2));
    {
        let tw = font::measure(FontId::Regular, 14.0, &label);
        let r = f.place(30 + tw, ROW_H);
        if f.clicked(r) {
            value = !value;
            f.changed = true;
        }
        let b = Rect::new(r.x, r.y + 5, 20, 20);
        let c = f.c();
        if value {
            c.fill_round(b, 5, ACCENT, 256);
            line(c, (b.x + 5, b.y + 10), (b.x + 8, b.y + 14), 0x0B1220);
            line(c, (b.x + 8, b.y + 14), (b.x + 15, b.y + 6), 0x0B1220);
        } else {
            c.fill_round(b, 5, FIELD_BG, 220);
            c.stroke_round(b, 5, 0x5A6078, 256);
        }
        f.text(r.x + 30, r.y + 6, &label, TEXT, FontId::Regular, 14.0);
    }
    vec![Value::Bool(value)]
}

/// ui.radio({options}, selected) -> selected
fn ui_radio(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (options, mut selected) = (a.list(1), a.text(2));
    {
        let widths: Vec<i32> = options.iter().map(|o| 28 + font::measure(FontId::Regular, 14.0, o)).collect();
        let total = widths.iter().sum::<i32>() + 18 * (widths.len() as i32 - 1).max(0);
        let r = f.place(total, ROW_H);
        let mut x = r.x;
        for (opt, w) in options.iter().zip(widths) {
            let item = Rect::new(x, r.y, w, ROW_H);
            if f.clicked(item) && selected != *opt {
                selected = opt.clone();
                f.changed = true;
            }
            let on = selected == *opt;
            let circle = Rect::new(x, r.y + 5, 20, 20);
            let c = f.c();
            c.fill_round(circle, 10, FIELD_BG, 220);
            c.stroke_round(circle, 10, if on { ACCENT } else { 0x5A6078 }, 256);
            if on {
                c.fill_round(circle.inset(5), 5, ACCENT, 256);
            }
            f.text(x + 28, r.y + 6, opt, TEXT, FontId::Regular, 14.0);
            x += w + 18;
        }
    }
    vec![Value::text(&selected)]
}

/// ui.dropdown(id, {options}, selected) -> selected
fn ui_dropdown(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (id, options, mut selected) = (a.text(1), a.list(2), a.text(3));
    {
        let w = f.avail().min(260);
        let r = f.place(w, ROW_H + 2);
        let was_open = f.was_open.as_deref() == Some(id.as_str());
        let mut open = was_open && f.click.is_none() && f.popup_click.is_none();
        if f.clicked(r) {
            open = !was_open;
        }
        if was_open && let Some(i) = f.popup_click {
            if let Some(item) = options.get(i) {
                f.changed |= *item != selected;
                selected = item.clone();
            }
            open = false;
        }
        field(f.c(), r, open);
        let text = if selected.is_empty() { "Select..." } else { selected.as_str() };
        let color = if selected.is_empty() { 0x5D6380 } else { TEXT };
        let area = f.area;
        f.c().set_clip(Rect::new(r.x, r.y, r.w - 30, r.h).intersect(area));
        f.text(r.x + 10, r.y + 7, text, color, FontId::Regular, 14.0);
        f.c().set_clip(area);
        let (cx, cy) = (r.x + r.w - 17, r.y + r.h / 2);
        line(f.c(), (cx - 5, cy - 2), (cx, cy + 3), TEXT_DIM);
        line(f.c(), (cx, cy + 3), (cx + 5, cy - 2), TEXT_DIM);
        if open {
            let rect = Rect::new(r.x, r.y + r.h + 4, r.w, options.len() as i32 * ITEM_H + 8);
            let selected_idx = options.iter().position(|o| *o == selected);
            f.new_popup = Some(Popup { id, rect, items: options, selected: selected_idx });
        }
    }
    vec![Value::text(&selected)]
}

/// ui.slider(id, value, min, max) -> value
fn ui_slider(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (id, mut value, min, max) = (a.text(1), a.int(2, 0), a.int(3, 0), a.int(4, 100));
    let max = max.max(min + 1);
    value = value.clamp(min, max);
    {
        let w = f.avail();
        let r = f.place(w, ROW_H);
        let track = Rect::new(r.x + 9, r.y + 4, r.w - 18 - 50, 22);
        if f.clicked(track.inset(-8)) {
            f.st().active = Some(id.clone());
        }
        if f.held && f.st().active.as_deref() == Some(id.as_str()) && track.w > 0 {
            let t = (f.mouse.0 - track.x).clamp(0, track.w) as i64;
            let v = min + (t * (max - min) + track.w as i64 / 2) / track.w as i64;
            f.changed |= v != value;
            value = v;
        }
        widgets::slider(f.c(), track, (value - min) as u32, 0, (max - min) as u32);
        let label = value.to_string();
        let lw = font::measure(FontId::Regular, 14.0, &label);
        f.text(r.x + r.w - lw, r.y + 6, &label, TEXT_DIM, FontId::Regular, 14.0);
    }
    vec![Value::Int(value)]
}

/// ui.color(value) -> value: a palette and a swatch of the selected colour.
fn ui_color(f: &mut Frame, a: &Args) -> Vec<Value> {
    let mut value = a.int(1, PALETTE[0] as i64) & 0xFFFFFF;
    {
        let w = f.avail();
        let r = f.place(w, ROW_H);
        for (i, &color) in PALETTE.iter().enumerate() {
            let s = Rect::new(r.x + 4 + i as i32 * 30, r.y + 4, 22, 22);
            if f.clicked(s.inset(-3)) {
                f.changed |= value != color as i64;
                value = color as i64;
            }
            let bg = f.bg;
            widgets::swatch(f.c(), s, color, value == color as i64, bg);
        }
        let hex = format!("#{value:06X}");
        f.text(r.x + 4 + PALETTE.len() as i32 * 30 + 8, r.y + 6, &hex, TEXT_DIM, FontId::Mono, 13.0);
    }
    vec![Value::Int(value)]
}

/// ui.progress(value [, max])
fn ui_progress(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (value, max) = (a.int(1, 0), a.int(2, 100).max(1));
    {
        let w = f.avail();
        let r = f.place(w, 12);
        let c = f.c();
        c.fill_round(r, 6, 0x3A3F55, 256);
        let fill = (r.w as i64 * value.clamp(0, max) / max) as i32;
        if fill > 0 {
            c.fill_round(Rect::new(r.x, r.y, fill.max(12), r.h), 6, ACCENT, 256);
        }
    }
    Vec::new()
}

/// ui.panel(text, color [, height]) — a coloured plate with centred text.
fn ui_panel(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (text, color, height) = (a.text(1), a.int(2, ACCENT as i64) as Color & 0xFFFFFF, a.int(3, 44) as i32);
    {
        let w = f.avail();
        let r = f.place(w, height.clamp(20, MAX_SIZE));
        f.c().fill_round(r, 10, color, 256);
        let tw = font::measure(FontId::SemiBold, 15.0, &text);
        f.text(r.x + (r.w - tw) / 2, r.y + (r.h - 18) / 2, &text, contrast(color), FontId::SemiBold, 15.0);
    }
    Vec::new()
}

/// ui.tabs(id, {tabs}, selected) -> selected: a "pill" switcher.
fn ui_tabs(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (items, mut selected) = (a.list(2), a.text(3));
    {
        let widths: Vec<i32> = items.iter().map(|t| font::measure(FontId::Regular, 14.0, t) + 28).collect();
        let total = widths.iter().sum::<i32>() + 8;
        let r = f.place(total, ROW_H + 4);
        f.c().fill_round(r, r.h / 2, FIELD_BG, 200);
        let mut x = r.x + 4;
        for (item, w) in items.iter().zip(widths) {
            let tab = Rect::new(x, r.y + 3, w, r.h - 6);
            if f.clicked(tab) && selected != *item {
                selected = item.clone();
                f.changed = true;
            }
            let on = selected == *item;
            if on {
                f.c().fill_round(tab, tab.h / 2, ACCENT, 220);
            }
            let color = if on { 0x0B1220 } else { TEXT };
            f.text(x + 14, tab.y + 5, item, color, FontId::Regular, 14.0);
            x += w;
        }
    }
    vec![Value::text(&selected)]
}

/// ui.image(data, width [, height]) -> whether it was clicked. A picture (PNG, JPEG, GIF, BMP —
/// the file bytes) is fitted into width×height preserving proportions, centred.
/// Data that is not a picture gives an empty frame.
fn ui_image(f: &mut Frame, a: &Args) -> Vec<Value> {
    let w = a.int(2, 128).clamp(1, MAX_SIZE as i64) as i32;
    let h = a.int(3, w as i64).clamp(1, MAX_SIZE as i64) as i32;
    let r = f.place(w, h);
    let data = a.bytes(1).unwrap_or(&[]);
    match picture(f, data, r, false) {
        Some(px) => {
            let dev = f.c().device(r);
            f.c().blit_into(r, dev.w, &px);
        }
        None => {
            let c = f.c();
            c.fill_round(r, 8, 0x000000, 50);
            c.stroke_round(r, 8, BORDER, 256);
        }
    }
    vec![Value::Bool(f.clicked(r))]
}

/// A picture from file bytes, ready to be output into `r` (pixel for pixel): fitted
/// in the centre with background-coloured margins or (`cover`) filling `r` with the edges cropped.
/// The decoded result is remembered — the picture is not decoded again every frame.
fn picture(f: &mut Frame, data: &[u8], r: Rect, cover: bool) -> Option<Arc<Vec<u32>>> {
    let dev = f.c().device(r);
    if dev.w <= 0 || dev.h <= 0 || data.is_empty() {
        return None;
    }
    let key = fingerprint(data) ^ cover as u64;
    if let Some(i) = f.st().images.iter().position(|(k, pw, ph, _)| *k == key && *pw == dev.w && *ph == dev.h) {
        let e = f.st().images.remove(i);
        let px = e.3.clone();
        f.st().images.push(e);
        return Some(px);
    }
    let img = crate::image::decode(data).ok().filter(|i| !i.frames.is_empty())?;
    let (sw, sh) = (img.w as i64, img.h as i64);
    let (dw, dh) = (dev.w as i64, dev.h as i64);
    let px = if cover {
        // Cut the middle out of the picture with the target's proportions.
        let (cw, ch) = if sw * dh > sh * dw { ((sh * dw / dh).max(1), sh) } else { (sw, (sw * dh / dw).max(1)) };
        let (cx, cy) = ((sw - cw) / 2, (sh - ch) / 2);
        let mut crop = Vec::with_capacity((cw * ch) as usize);
        for y in cy..cy + ch {
            let row = (y * sw + cx) as usize;
            crop.extend_from_slice(&img.frames[0].px[row..row + cw as usize]);
        }
        crate::image::resize(&crop, cw as usize, ch as usize, dw as usize, dh as usize, f.bg)
    } else {
        let (tw, th) = if sw * dh > sh * dw { (dw, (sh * dw / sw).max(1)) } else { ((sw * dh / sh).max(1), dh) };
        let fitted = crate::image::resize(&img.frames[0].px, img.w, img.h, tw as usize, th as usize, f.bg);
        let mut px = alloc::vec![f.bg | 0xFF00_0000; (dw * dh) as usize];
        let (ox, oy) = ((dw - tw) / 2, (dh - th) / 2);
        for y in 0..th {
            let d = ((oy + y) * dw + ox) as usize;
            px[d..d + tw as usize].copy_from_slice(&fitted[(y * tw) as usize..((y + 1) * tw) as usize]);
        }
        px
    };
    let px = Arc::new(px);
    let images = &mut f.st().images;
    if images.len() >= IMAGE_CACHE {
        images.remove(0);
    }
    images.push((key, dev.w, dev.h, px.clone()));
    Some(px)
}

/// ui.list(id, {rows}, selected index [, height]) -> index (from 1; 0 — nothing).
/// If the rows do not fit, the list scrolls with the wheel and a bar on the right; when
/// the program changes the selected item, the list scrolls to it by itself.
fn ui_list(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (id, items, mut selected, height) = (a.text(1), a.list(2), a.int(3, 0), a.int(4, 160) as i32);
    {
        let w = f.avail();
        let r = f.place(w, height.clamp(ITEM_H + 8, MAX_SIZE));
        let (mut offset, last) = f.st().lists.get(&id).copied().unwrap_or((0, 0));
        let content = items.len() as i32 * ITEM_H + 8;
        let max = (content - r.h).max(0);
        let bar = Rect::new(r.x + r.w - 12, r.y + 4, 8, r.h - 8);
        // The selected item changed without a click — show it.
        if selected != last && selected > 0 {
            let top = (selected as i32 - 1) * ITEM_H;
            if top < offset {
                offset = top;
            } else if top + ITEM_H + 8 > offset + r.h {
                offset = top + ITEM_H + 8 - r.h;
            }
        }
        if max > 0 && f.wheel != 0 && r.contains(f.mouse.0, f.mouse.1) {
            offset += f.wheel * ITEM_H;
            f.wheel = 0;
        }
        // The bar is dragged with the mouse (or was clicked).
        let drag_id = format!("list:{id}");
        let thumb_h = if max > 0 { (bar.h * r.h / content).clamp(24, bar.h) } else { bar.h };
        if max > 0 && f.clicked(bar.inset(-3)) {
            f.st().active = Some(drag_id.clone());
        }
        if max > 0 && f.held && f.st().active.as_deref() == Some(drag_id.as_str()) && bar.h > thumb_h {
            offset = (f.mouse.1 - bar.y - thumb_h / 2) * max / (bar.h - thumb_h);
        }
        offset = offset.clamp(0, max);
        field(f.c(), r, false);
        let area = f.area;
        f.c().set_clip(r.inset(2).intersect(area));
        let row_w = if max > 0 { r.w - 8 - 14 } else { r.w - 8 };
        let first = (offset / ITEM_H) as usize;
        let shown = (r.h / ITEM_H + 2) as usize;
        for (i, item) in items.iter().enumerate().skip(first).take(shown) {
            let row = Rect::new(r.x + 4, r.y + 4 + i as i32 * ITEM_H - offset, row_w, ITEM_H);
            if f.clicked(row.intersect(r)) && !f.st().active.as_deref().is_some_and(|a| a == drag_id) {
                f.changed |= selected != i as i64 + 1;
                selected = i as i64 + 1;
            }
            if selected == i as i64 + 1 {
                f.c().fill_round(row, 6, ACCENT, 70);
            }
            f.text(row.x + 10, row.y + 5, item, TEXT, FontId::Regular, 14.0);
        }
        f.c().set_clip(area);
        if max > 0 {
            let y = bar.y + (bar.h - thumb_h) * offset / max;
            f.c().fill_round(bar, 4, 0xFFFFFF, 18);
            f.c().fill_round(Rect::new(bar.x, y, bar.w, thumb_h), 4, 0xFFFFFF, 90);
        }
        f.st().lists.insert(id, (offset, selected));
    }
    vec![Value::Int(selected)]
}

/// ui.textarea(id, text [, height]) -> text: a multi-line field (Enter — a new line).
fn ui_textarea(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (id, mut value, height) = (a.text(1), a.text(2), a.int(3, 120) as i32);
    {
        let w = f.avail();
        let r = f.place(w, height.clamp(40, MAX_SIZE));
        let mut focused = f.st().focus.as_deref() == Some(id.as_str());
        if f.clicked(r) {
            f.st().focus = Some(id.clone());
            f.focus_claimed = true;
            focused = true;
        }
        if focused {
            let before = value.clone();
            for k in core::mem::take(&mut f.keys) {
                match k {
                    Typed::Key(0x08) => {
                        value.pop();
                    }
                    Typed::Key(0x1B) => {
                        f.st().focus = None;
                        focused = false;
                    }
                    Typed::Key(b'\n') => value.push('\n'),
                    Typed::Key(0x16) => value.extend(crate::wm::clipboard().chars().filter(|&c| c == '\n' || !c.is_control())),
                    Typed::Key(k @ 0x20..=0x7E) if value.len() < 4000 => value.push(k as char),
                    Typed::Char(c) if value.len() < 4000 => value.push(c),
                    _ => {}
                }
            }
            f.changed |= value != before;
        }
        field(f.c(), r, focused);
        let area = f.area;
        f.c().set_clip(r.inset(2).intersect(area));
        // Show the last lines that fit.
        let lines: Vec<&str> = value.split('\n').collect();
        let visible = ((r.h - 12) / 20).max(1) as usize;
        let first = lines.len().saturating_sub(visible);
        let mut y = r.y + 6;
        for (i, line) in lines[first..].iter().enumerate() {
            let w = f.text(r.x + 10, y, line, TEXT, FontId::Regular, 14.0);
            if focused && f.cursor_on && first + i == lines.len() - 1 {
                f.c().fill(Rect::new(r.x + 11 + w, y + 1, 2, 18), ACCENT);
            }
            y += 20;
        }
        f.c().set_clip(area);
    }
    vec![Value::text(&value)]
}

/// ui._canvas(height) -> clicked, x, y, held, w, h: the start of the program's own drawing area.
/// The Lua wrapper (ui.canvas) calls the program's function and closes the area.
fn ui_canvas(f: &mut Frame, a: &Args) -> Vec<Value> {
    let r = if a.flag(2) {
        // ui.fill: the whole window, no margins; nothing will come below.
        let area = f.area;
        f.y = area.y + area.h - PAD;
        area
    } else {
        let height = a.int(1, 200) as i32;
        let w = f.avail();
        f.place(w, height.clamp(10, MAX_SIZE))
    };
    let clicked = f.clicked(r);
    // The wheel over the canvas goes to the program.
    let wheel = if r.contains(f.mouse.0, f.mouse.1) { core::mem::take(&mut f.wheel) } else { 0 };
    let (mx, my, held) = (f.mouse.0 - r.x, f.mouse.1 - r.y, f.held);
    let area = f.area;
    f.target = Some(r);
    f.c().set_clip(r.intersect(area));
    vec![Value::Bool(clicked), Value::Int(mx as i64), Value::Int(my as i64), Value::Bool(held), Value::Int(r.w as i64), Value::Int(r.h as i64), Value::Int(wheel as i64)]
}

fn ui_canvas_end(f: &mut Frame, _a: &Args) -> Vec<Value> {
    let area = f.area;
    f.target = None;
    f.c().set_clip(area);
    Vec::new()
}

/// Opacity 0..=255 from an argument → 0..=256 for the canvas.
fn alpha(a: &Args, n: usize) -> u32 {
    let v = a.int(n, 255).clamp(0, 255) as u32;
    if v >= 255 { 256 } else { v }
}

/// The ui.canvas rectangle from arguments x, y, w, h starting at index `n`.
fn local_rect(target: Rect, a: &Args, n: usize) -> Rect {
    Rect::new(target.x + a.pos(n) as i32, target.y + a.pos(n + 1) as i32, a.pos(n + 2).clamp(0, MAX_SIZE as i64) as i32, a.pos(n + 3).clamp(0, MAX_SIZE as i64) as i32)
}

/// gfx.rect(x, y, w, h, color [, radius [, opacity 0..255]])
fn gfx_rect(f: &mut Frame, a: &Args) -> Vec<Value> {
    if let Some(t) = f.target {
        let rect = local_rect(t, a, 1);
        let radius = a.int(6, 0).clamp(0, MAX_SIZE as i64) as i32;
        f.canvas.fill_round(rect, radius, a.int(5, 0xFFFFFF) as Color & 0xFFFFFF, alpha(a, 7));
    }
    Vec::new()
}

/// gfx.stroke(x, y, w, h, color [, radius [, opacity]]) — a one-pixel outline.
fn gfx_stroke(f: &mut Frame, a: &Args) -> Vec<Value> {
    if let Some(t) = f.target {
        let rect = local_rect(t, a, 1);
        let radius = a.int(6, 0).clamp(0, MAX_SIZE as i64) as i32;
        f.canvas.stroke_round(rect, radius, a.int(5, 0xFFFFFF) as Color & 0xFFFFFF, alpha(a, 7));
    }
    Vec::new()
}

/// gfx.gradient(x, y, w, h, top color, bottom color [, radius [, opacity]])
fn gfx_gradient(f: &mut Frame, a: &Args) -> Vec<Value> {
    let Some(t) = f.target else { return Vec::new() };
    let r = local_rect(t, a, 1);
    let (top, bottom) = (a.int(5, 0) as Color & 0xFFFFFF, a.int(6, 0) as Color & 0xFFFFFF);
    let radius = (a.int(7, 0).clamp(0, MAX_SIZE as i64) as i32).min(r.w / 2).min(r.h / 2);
    let op = alpha(a, 8);
    let channel = |c: Color, s: u32| (c >> s & 0xFF) as i32;
    for y in 0..r.h {
        let k = if r.h > 1 { y * 256 / (r.h - 1) } else { 0 };
        let mix = |s: u32| ((channel(top, s) * (256 - k) + channel(bottom, s) * k) / 256) as Color;
        let color = mix(16) << 16 | mix(8) << 8 | mix(0);
        // Rounded corners: the row is shorter by the curve.
        let edge = if y < radius { radius - y } else if y >= r.h - radius { y - (r.h - radius) + 1 } else { 0 };
        let inset = if edge > 0 { radius - libm::sqrtf((radius * radius - (edge - 1).pow(2).min(radius * radius)) as f32) as i32 } else { 0 };
        let row = Rect::new(r.x + inset, r.y + y, r.w - 2 * inset, 1);
        if op >= 256 { f.canvas.fill(row, color) } else { f.canvas.blend(row, color, op) }
    }
    Vec::new()
}

/// gfx.image(data, x, y, w, h [, "cover"]) — a picture (PNG, JPEG, GIF, BMP) in a
/// rectangle: fit with margins or ("cover") fill with the edges cropped.
fn gfx_image(f: &mut Frame, a: &Args) -> Vec<Value> {
    let Some(t) = f.target else { return Vec::new() };
    let r = local_rect(t, a, 2);
    let cover = a.text(6) == "cover";
    let data = a.bytes(1).unwrap_or(&[]);
    if let Some(px) = picture(f, data, r, cover) {
        let dev = f.c().device(r);
        f.c().blit_into(r, dev.w, &px);
    }
    Vec::new()
}

fn font_of(name: &str, size: i64) -> FontId {
    match name {
        "bold" => FontId::SemiBold,
        "mono" => FontId::Mono,
        "regular" => FontId::Regular,
        _ if size >= 20 => FontId::SemiBold,
        _ => FontId::Regular,
    }
}

/// gfx.measure(text [, size [, font]]) -> width: font "regular", "bold", "mono".
fn gfx_measure(_f: &mut Frame, a: &Args) -> Vec<Value> {
    let size = a.int(2, 14).clamp(6, 96);
    vec![Value::Int(font::measure(font_of(&a.text(3), size), size as f32, &a.text(1)) as i64)]
}

/// gfx.clip(x, y, w, h) — draw only inside; gfx.clip() — the whole canvas again.
fn gfx_clip(f: &mut Frame, a: &Args) -> Vec<Value> {
    let Some(t) = f.target else { return Vec::new() };
    let area = f.area.intersect(t);
    let r = if a.opt(1).is_some() { local_rect(t, a, 1).intersect(area) } else { area };
    f.c().set_clip(r);
    Vec::new()
}

/// gfx.circle(cx, cy, radius, color [, opacity])
fn gfx_circle(f: &mut Frame, a: &Args) -> Vec<Value> {
    let v: [i64; 4] = [a.pos(1), a.pos(2), a.int(3, 4).clamp(0, MAX_SIZE as i64), a.int(4, 0xFFFFFF)];
    if let Some(r) = f.target {
        let c = &mut f.canvas;
        let rad = v[2] as i32;
        let rect = Rect::new(r.x + v[0] as i32 - rad, r.y + v[1] as i32 - rad, rad * 2, rad * 2);
        c.fill_round(rect, rad, v[3] as Color & 0xFFFFFF, alpha(a, 5));
    }
    Vec::new()
}

/// gfx.line(x0, y0, x1, y1, color)
fn gfx_line(f: &mut Frame, a: &Args) -> Vec<Value> {
    let v: [i64; 5] = [a.pos(1), a.pos(2), a.pos(3), a.pos(4), a.int(5, 0xFFFFFF)];
    if let Some(r) = f.target {
        let c = &mut f.canvas;
        let p = |x: i64, y: i64| (r.x + x as i32, r.y + y as i32);
        line(c, p(v[0], v[1]), p(v[2], v[3]), v[4] as Color & 0xFFFFFF);
    }
    Vec::new()
}

/// gfx.text(x, y, text [, color [, size [, font]]]) -> width
fn gfx_text(f: &mut Frame, a: &Args) -> Vec<Value> {
    let (x, y, text, color, size) = (a.pos(1), a.pos(2), a.text(3), a.int(4, TEXT as i64), a.int(5, 14));
    let mut w = 0;
    if let Some(r) = f.target {
        let c = &mut f.canvas;
        let font = font_of(&a.text(6), size);
        w = c.text(r.x + x as i32, r.y + y as i32, &text, color as Color & 0xFFFFFF, font, size.clamp(6, 96) as f32);
    }
    vec![Value::Int(w as i64)]
}

/// gfx.clear(color)
fn gfx_clear(f: &mut Frame, a: &Args) -> Vec<Value> {
    let color = a.int(1, 0) as Color & 0xFFFFFF;
    if let Some(r) = f.target {
        let c = &mut f.canvas;
        c.fill(r, color);
    }
    Vec::new()
}

// ---------------------------------------------------------------- talking to the program

/// What the kernel tells the process besides "draw a frame".
enum Event {
    Key(String),
    /// The program is given files: a file it already shows was opened, or files were
    /// dropped on its window.
    Open(Vec<String>),
    Fetch(i64, Result<crate::net::http::Response, String>),
}

/// The window's mailbox: both the main thread and the process write here.
struct Inbox {
    click: Option<(i32, i32)>,
    mouse: (i32, i32),
    held: bool,
    keys: Vec<Typed>,
    events: VecDeque<Event>,
    /// A new frame is needed.
    want_frame: bool,
    /// A new frame is ready — the window should show it.
    fresh: bool,
    size: (i32, i32),
    /// The scale of the screen the window is on: the frame is drawn for its pixels.
    scale: i32,
    scroll: i32,
    content_h: i32,
    /// Wheel motion not yet given to a frame.
    wheel: i32,
    /// A text field has focus; something is being dragged or a list is open.
    focused: bool,
    tracking: bool,
    cursor_on: bool,
    error: Option<String>,
    memory: usize,
    /// What changed in the frames since the last show (None — unknown, the whole window).
    damage: Option<Rect>,
}

/// A program window as the kernel sees it.
pub struct Port {
    thread: u64,
    bg: Color,
    /// The program accepts files (it has app.open).
    takes_files: bool,
    /// The minimum window size (app.min_width, app.min_height).
    min_size: Option<(i32, i32)>,
    inbox: IrqLock<Inbox>,
    /// The last finished frame.
    front: IrqLock<Option<Canvas>>,
    /// State only the process itself works with.
    painter: YieldLock<Painter>,
    dialogs: IrqLock<Vec<(i64, Reply)>>,
}

#[derive(Default)]
struct Painter {
    back: Option<Canvas>,
    /// Where the back buffer differs from the front one (None — unknown, for example another size).
    behind: Option<Rect>,
    state: UiState,
    frame: Option<Frame>,
}

/// Process windows: thread id → window.
static PORTS: IrqLock<BTreeMap<u64, Arc<Port>>> = IrqLock::new(BTreeMap::new());

impl Port {
    /// The process has something to handle: wake it from `ui._wait`.
    fn poke(&self) {
        task::wake(self.thread);
    }
}

fn my_port() -> Option<Arc<Port>> {
    PORTS.lock().get(&task::current_id()).cloned()
}

/// A press for a text field: a key or a character of another language.
#[derive(Clone, Copy)]
enum Typed {
    Key(u8),
    Char(char),
}

/// Whether the thread has a window.
pub fn has_window(thread: u64) -> bool {
    PORTS.lock().contains_key(&thread)
}

/// There is a window — answers to a dialog and a network request will come to it.
pub fn watch_dialog(id: i64, reply: Reply) -> bool {
    match my_port() {
        Some(p) => {
            p.dialogs.lock().push((id, reply));
            true
        }
        None => false,
    }
}

/// The result of a net.fetch request: a background thread delivers it to the window.
pub fn fetch_target() -> Option<impl FnOnce(i64, Result<crate::net::http::Response, String>) + Send + 'static> {
    let port = my_port()?;
    Some(move |id, r| {
        port.inbox.lock().events.push_back(Event::Fetch(id, r));
        port.poke();
    })
}

/// The ui.* and gfx.* operations.
pub fn call(lib: &str, name: &str, a: &Args) -> Vec<Value> {
    match name {
        "_open" => open(a),
        "_wait" => wait(a),
        "_begin" => begin(a),
        "_end" => end(a),
        "_fail" => {
            fail(&a.text(1));
            Vec::new()
        }
        _ => {
            let Some(port) = my_port() else { return Vec::new() };
            let mut painter = port.painter.lock();
            let Some(f) = painter.frame.as_mut() else { return Vec::new() };
            // ui and gfx have functions with the same name (image): canvas drawing is separate.
            if lib == "gfx" && name == "image" {
                return gfx_image(f, a);
            }
            widget(name, f, a)
        }
    }
}

fn widget(name: &str, f: &mut Frame, a: &Args) -> Vec<Value> {
    match name {
        "heading" => ui_heading(f, a),
        "label" => ui_label(f, a),
        "separator" => ui_separator(f, a),
        "space" => ui_space(f, a),
        "sameline" => ui_sameline(f, a),
        "button" => ui_button(f, a),
        "input" => ui_input(f, a),
        "checkbox" => ui_checkbox(f, a),
        "radio" => ui_radio(f, a),
        "dropdown" => ui_dropdown(f, a),
        "slider" => ui_slider(f, a),
        "color" => ui_color(f, a),
        "progress" => ui_progress(f, a),
        "panel" => ui_panel(f, a),
        "tabs" => ui_tabs(f, a),
        "list" => ui_list(f, a),
        "image" => ui_image(f, a),
        "textarea" => ui_textarea(f, a),
        "_canvas" => ui_canvas(f, a),
        "_canvas_end" => ui_canvas_end(f, a),
        "rect" => gfx_rect(f, a),
        "circle" => gfx_circle(f, a),
        "line" => gfx_line(f, a),
        "text" => gfx_text(f, a),
        "clear" => gfx_clear(f, a),
        "stroke" => gfx_stroke(f, a),
        "gradient" => gfx_gradient(f, a),
        "measure" => gfx_measure(f, a),
        "clip" => gfx_clip(f, a),
        _ => Vec::new(),
    }
}

/// ui._open(title, width, height, background, accepts files, min width, min height).
fn open(a: &Args) -> Vec<Value> {
    let thread = task::current_id();
    if has_window(thread) {
        return Vec::new();
    }
    if crate::services::is_service(thread) {
        if let Some(out) = task::output() {
            out.lock().extend_from_slice(b"a service runs without a window, but this program wants one\n");
        }
        task::exit_with(1);
    }
    let (w, h) = (a.int(2, 480).clamp(200, 1200) as i32, a.int(3, 400).clamp(120, 760) as i32);
    let bg = a.int(4, DEFAULT_BG as i64) as Color & 0xFFFFFF;
    let port = Arc::new(Port {
        thread,
        bg,
        takes_files: a.flag(5),
        min_size: match (a.int(6, 0), a.int(7, 0)) {
            (0, 0) => None,
            (w, h) => Some((w.clamp(0, 1200) as i32, h.clamp(0, 760) as i32)),
        },
        inbox: IrqLock::new(Inbox {
            click: None,
            mouse: (0, 0),
            held: false,
            keys: Vec::new(),
            events: VecDeque::new(),
            want_frame: true,
            fresh: false,
            size: (0, 0),
            scale: crate::gfx::SCALE_100,
            scroll: 0,
            content_h: 0,
            wheel: 0,
            focused: false,
            tracking: false,
            cursor_on: true,
            error: None,
            memory: 0,
            damage: None,
        }),
        front: IrqLock::new(None),
        painter: YieldLock::new(Painter::default()),
        dialogs: IrqLock::new(Vec::new()),
    });
    PORTS.lock().insert(thread, port.clone());
    let place = super::take_placement(thread);
    wm::request(Request::Window(LuaWindow { path: super::script_of(thread), title: a.text(1), w, h, app: LuaApp::new(port), place }));
    // For the terminal the program has started: from now on it lives in its own window.
    task::detach();
    Vec::new()
}

/// ui._wait(ms) -> kind, ... the next event, or nothing if the time ran out.
fn wait(a: &Args) -> Vec<Value> {
    let Some(port) = my_port() else { return Vec::new() };
    let until = interrupts::ticks() + (a.int(1, 0).clamp(0, 60_000) as u64 * HZ).div_ceil(1000);
    loop {
        // The event is taken under the lock and handled afterwards: handling may go to disk
        // and to the network (a folder on an SMB share), and the lock disables interrupts, so the main thread
        // waiting for it on another core would stall together with the timer.
        let event = {
            let mut inbox = port.inbox.lock();
            if inbox.want_frame {
                inbox.want_frame = false;
                return vec![Value::text("redraw")];
            }
            inbox.events.pop_front()
        };
        if let Some(e) = event {
            return match e {
                Event::Key(name) => vec![Value::text("key"), Value::text(&name)],
                Event::Open(paths) => {
                    // Files handed over by the person may be read by the program, and the file's folder
                    // too (the player takes neighbouring songs into the playlist).
                    for p in &paths {
                        crate::access::grant(p);
                        if !crate::fs::is_dir(p) {
                            crate::access::grant(&crate::fs::resolve(p, ".."));
                        }
                    }
                    vec![Value::text("open"), Value::List(paths)]
                }
                Event::Fetch(id, Ok(r)) => vec![Value::text("fetch"), Value::Int(id), Value::Bytes(r.body), Value::Int(r.status as i64)],
                Event::Fetch(id, Err(e)) => vec![Value::text("fetch"), Value::Int(id), Value::Nil, Value::text(&e)],
            };
        }
        let answered = {
            let mut pending = port.dialogs.lock();
            let i = pending.iter().position(|(_, reply)| reply.lock().is_some());
            i.map(|i| pending.remove(i))
        };
        if let Some((id, reply)) = answered {
            let path = crate::apps::dialog::take(&reply).flatten();
            // Several files chosen — paths separated by newlines.
            for p in path.iter().flat_map(|p| p.split('\n')) {
                crate::access::grant(p);
            }
            return vec![Value::text("dialog"), Value::Int(id), path.map_or(Value::Nil, |p| Value::text(&p))];
        }
        let now = interrupts::ticks();
        if now >= until {
            return Vec::new();
        }
        // Sleep until an event from the window (`Port::poke` wakes it) or the deadline. A dialog answer
        // cannot wake — it is caught by checking every quarter of a second.
        task::park(until.min(now + HZ / 4));
    }
}

/// ui._begin([x, y, w, h]) -> whether drawing is possible, whether the frame is partial: starts a frame in
/// the back buffer. With a rectangle — a partial frame: the program redraws only
/// it (the player's spectrum 30 times a second), the rest is taken from the previous frame.
/// The frame cannot be partial if there is no previous one, the window changed size or a list is open.
fn begin(a: &Args) -> Vec<Value> {
    let Some(port) = my_port() else { return vec![Value::Bool(false)] };
    let mut painter = port.painter.lock();
    let p = &mut *painter;
    let ask = a.opt(1).is_some().then(|| Rect::new(a.int(1, 0) as i32, a.int(2, 0) as i32, a.int(3, 0).clamp(0, MAX_SIZE as i64) as i32, a.int(4, 0).clamp(0, MAX_SIZE as i64) as i32));
    let (size, scale) = {
        let i = port.inbox.lock();
        (i.size, i.scale)
    };
    let (w, h) = size;
    if w <= 0 || h <= 0 {
        return vec![Value::Bool(false)];
    }
    let area = Rect::new(0, 0, w, h);
    let part = ask.map(|r| r.intersect(area)).filter(|_| {
        p.behind.is_some() && p.state.popup.is_none() && p.back.as_ref().is_some_and(|c| (c.w, c.h, c.scale) == (w, h, scale)) && port.front.lock().is_some()
    });
    // A click, keys and the wheel do not go to a partial frame: they wait for a full
    // one (the window has already asked for it).
    let (click, mouse, held, keys, scroll, cursor_on, wheel) = {
        let mut i = port.inbox.lock();
        if part.is_some() {
            (None, i.mouse, i.held, Vec::new(), i.scroll, i.cursor_on, 0)
        } else {
            let wheel = core::mem::take(&mut i.wheel);
            (i.click.take(), i.mouse, i.held, core::mem::take(&mut i.keys), i.scroll, i.cursor_on, wheel)
        }
    };
    let mut canvas = match p.back.take() {
        Some(c) if (c.w, c.h, c.scale) == (w, h, scale) => c,
        _ => {
            // Frame memory comes from the kernel heap: better to skip a frame than to crash.
            if crate::heap_stats().1 < (w as usize * h as usize * 4).saturating_add(crate::fs::READ_RESERVE) {
                return vec![Value::Bool(false)];
            }
            Canvas::scaled(w, h, scale)
        }
    };
    if let Some(r) = part {
        // The back buffer is the frame before last: bring it up to the last one, then draw only in `r`.
        if let (Some(stale), Some(front)) = (p.behind.take(), port.front.lock().as_ref())
            && !stale.is_empty()
        {
            canvas.copy_from(&front.px, stale);
        }
        canvas.set_limit(Some(r));
        canvas.fill(r, port.bg);
    } else {
        canvas.set_limit(None);
        canvas.fill(area, port.bg);
    }
    let mut state = core::mem::take(&mut p.state);
    if !held {
        state.active = None;
    }
    // A click on an open list goes only to it.
    let prev = state.popup.take();
    let (mut click, mut popup_click) = (click, None);
    if let (Some(popup), Some((x, y))) = (&prev, click)
        && popup.rect.contains(x, y)
    {
        popup_click = Some(((y - popup.rect.y - 4).max(0) / ITEM_H) as usize);
        click = None;
    }
    p.frame = Some(Frame {
        canvas,
        state,
        area,
        bg: port.bg,
        y: PAD - 4 - scroll,
        row_y: 0,
        row_h: 0,
        row_right: 0,
        same_line: false,
        click,
        mouse,
        held,
        keys,
        was_open: prev.map(|p| p.id),
        popup_click,
        new_popup: None,
        focus_claimed: false,
        changed: false,
        cursor_on,
        target: None,
        wheel,
        part,
    });
    vec![Value::Bool(true), Value::Bool(part.is_some())]
}

/// ui._end(memory in KiB) -> whether another frame is needed: finishes the frame and shows it.
fn end(a: &Args) -> Vec<Value> {
    let Some(port) = my_port() else { return Vec::new() };
    let mut painter = port.painter.lock();
    let p = &mut *painter;
    let Some(mut f) = p.frame.take() else { return Vec::new() };
    if f.click.is_some() && !f.focus_claimed {
        f.state.focus = None;
    }
    f.state.popup = f.new_popup.take();
    let mouse = f.mouse;
    f.canvas.reset_clip();
    if let Some(popup) = &f.state.popup {
        draw_popup(&mut f.canvas, popup, mouse);
    }
    let mut again = f.changed;
    let scroll = port.inbox.lock().scroll;
    let content = f.y + scroll + PAD;
    // The wheel no list took goes to the window.
    let (w, h) = (f.area.w, f.area.h);
    let max = (content - h).max(0);
    if f.wheel != 0 {
        let to = (scroll + f.wheel * 12).clamp(0, max);
        if to != scroll {
            port.inbox.lock().scroll = to;
            again = true;
        }
    }
    // Contents taller than the window — a scroll bar on the right.
    if max > 0 {
        let track = h - 8;
        let thumb = (track * h / content).clamp(24, track);
        let y = 4 + (track - thumb) * scroll.min(max) / max;
        f.canvas.fill_round(Rect::new(w - 7, y, 4, thumb), 2, 0xFFFFFF, 70);
    }
    let (focused, tracking) = (f.state.focus.is_some(), f.state.active.is_some() || f.state.popup.is_some());
    p.state = f.state;
    // What changed against the previous frame: the window only needs to recompose this piece.
    // A partial frame changed only its own rectangle — no need to compare the whole canvas.
    let diff = match f.part {
        Some(r) => Some(r),
        None => port.front.lock().as_ref().and_then(|old| f.canvas.diff_bounds(old)),
    };
    f.canvas.set_limit(None);
    p.behind = diff;
    p.back = port.front.lock().replace(f.canvas);
    let mut i = port.inbox.lock();
    i.damage = match (i.fresh, i.damage, diff) {
        // The previous frame has not been shown yet — accumulate.
        (true, Some(a), Some(b)) => Some(if a.w == 0 { b } else if b.w == 0 { a } else { a.union(b) }),
        (true, None, _) => None,
        (false, _, d) => d,
        (true, Some(_), None) => None,
    };
    i.content_h = content;
    i.focused = focused;
    i.tracking = tracking;
    i.fresh = true;
    i.memory = a.int(1, 0).max(0) as usize * 1024;
    drop(i);
    // The desktop should show the frame right away, not on the next tick.
    task::kick_main();
    vec![Value::Bool(again)]
}

/// The program stopped with an error: the window shows its text.
fn fail(msg: &str) {
    let Some(port) = my_port() else { return };
    kprintln!("lua: {msg}");
    wm::notify_with(Some(crate::audio::Sound::Error), "Program error", msg.lines().next().unwrap_or_default());
    let mut i = port.inbox.lock();
    i.error = Some(msg.into());
    i.fresh = true;
    drop(i);
    task::kick_main();
}

// ---------------------------------------------------------------- the window

fn draw_error(c: &mut Canvas, a: Rect, err: &str) {
    c.text(a.x + PAD, a.y + 16, "Program error", 0xF87171, FontId::SemiBold, 16.0);
    let per_line = ((a.w - 2 * PAD) / font::measure(FontId::Mono, 13.0, "M")).max(10) as usize;
    let chars: Vec<char> = err.chars().collect();
    for (i, chunk) in chars.chunks(per_line).take(20).enumerate() {
        let s: String = chunk.iter().collect();
        c.text(a.x + PAD, a.y + 46 + i as i32 * 18, &s, 0xFCA5A5, FontId::Mono, 13.0);
    }
}

fn draw_popup(c: &mut Canvas, p: &Popup, mouse: (i32, i32)) {
    c.shadow(p.rect, 8, 14, 4, 140);
    c.fill_round(p.rect, 8, 0x23273A, 256);
    c.stroke_round(p.rect, 8, 0xFFFFFF, 40);
    for (i, item) in p.items.iter().enumerate() {
        let r = Rect::new(p.rect.x + 4, p.rect.y + 4 + i as i32 * ITEM_H, p.rect.w - 8, ITEM_H);
        if r.contains(mouse.0, mouse.1) {
            c.fill_round(r, 6, ACCENT, 70);
        } else if p.selected == Some(i) {
            c.fill_round(r, 6, 0xFFFFFF, 18);
        }
        c.text(r.x + 10, r.y + 5, item, TEXT, FontId::Regular, 14.0);
    }
}

/// A program window for the WM. Closing the window ends the process.
pub struct LuaApp {
    port: Arc<Port>,
    done: bool,
}

impl LuaApp {
    fn new(port: Arc<Port>) -> Self {
        Self { port, done: false }
    }
}

impl App for LuaApp {
    fn background(&self) -> Color {
        self.port.bg
    }

    fn takes_files(&self) -> bool {
        self.port.takes_files
    }

    fn take_damage(&mut self) -> Option<Rect> {
        self.port.inbox.lock().damage.take()
    }

    fn min_size(&self) -> Option<(i32, i32)> {
        self.port.min_size
    }

    fn open_files(&mut self, paths: &[String]) -> bool {
        if !self.port.takes_files {
            return false;
        }
        self.port.inbox.lock().events.push_back(Event::Open(paths.to_vec()));
        self.port.poke();
        true
    }

    fn draw(&mut self, c: &mut Canvas, area: Rect, _ctx: &Ctx) {
        let (error, resized) = {
            let mut i = self.port.inbox.lock();
            let resized = i.size != (area.w, area.h);
            if resized {
                i.size = (area.w, area.h);
                i.want_frame = true;
            }
            i.scroll = i.scroll.min((i.content_h - area.h).max(0));
            (i.error.clone(), resized)
        };
        if resized {
            self.port.poke();
        }
        if let Some(err) = error {
            draw_error(c, area, &err);
            return;
        }
        if let Some(front) = self.port.front.lock().as_ref() {
            c.replace(front, area.x, area.y);
        }
    }

    fn scaled(&mut self, scale: i32) {
        let mut i = self.port.inbox.lock();
        i.scale = scale;
        i.want_frame = true;
        drop(i);
        self.port.poke();
    }

    fn memory(&self) -> usize {
        self.port.inbox.lock().memory
    }

    fn scroll(&mut self, lines: i32) -> bool {
        // Where to scroll (a list under the cursor or the window) is decided by the next frame.
        let mut i = self.port.inbox.lock();
        i.wheel += lines;
        i.want_frame = true;
        drop(i);
        self.port.poke();
        false
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        let mut i = self.port.inbox.lock();
        i.mouse = (x, y);
        let held = buttons & LEFT != 0;
        // The button was released — the program needs to know too (end of a drag).
        let mut wake = held != i.held;
        i.held = held;
        if pressed {
            i.click = Some((x, y));
            i.want_frame = true;
            wake = true;
        } else if i.held || i.tracking {
            // Dragging (slider, field, canvas) and highlighting items of an open list.
            i.want_frame = true;
            wake = true;
        }
        drop(i);
        if wake {
            self.port.poke();
        }
        false
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        let mut i = self.port.inbox.lock();
        if i.focused {
            i.keys.push(Typed::Key(ch));
            i.cursor_on = true;
            i.want_frame = true;
        } else if i.error.is_none() {
            // No text field has focus — the key goes to the program: app.key("left").
            i.events.push_back(Event::Key(key_name(ch)));
        }
        drop(i);
        self.port.poke();
        true
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        let mut i = self.port.inbox.lock();
        if i.focused {
            i.keys.push(Typed::Char(ch));
            i.cursor_on = true;
            i.want_frame = true;
        } else if i.error.is_none() {
            i.events.push_back(Event::Key(ch.to_string()));
        }
        drop(i);
        self.port.poke();
        true
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        // The process exited or crashed — the window has nothing to show.
        if !task::is_alive(self.port.thread) {
            self.done = true;
            return false;
        }
        let mut i = self.port.inbox.lock();
        let on = ctx.caret_on();
        let blink = i.focused && on != i.cursor_on;
        if blink {
            i.want_frame = true;
        }
        i.cursor_on = on;
        let fresh = core::mem::take(&mut i.fresh);
        drop(i);
        if blink {
            self.port.poke();
        }
        fresh
    }

    fn finished(&self) -> bool {
        self.done
    }
}

impl Drop for LuaApp {
    fn drop(&mut self) {
        task::kill(self.port.thread);
        PORTS.lock().remove(&self.port.thread);
        crate::audio::player::close_owned(self.port.thread);
    }
}

/// The key name for app.key: "left", "enter", "a", "ctrl+s"...
fn key_name(ch: u8) -> String {
    let named = match ch {
        KEY_UP => "up",
        KEY_DOWN => "down",
        KEY_LEFT => "left",
        KEY_RIGHT => "right",
        KEY_HOME => "home",
        KEY_END => "end",
        KEY_DELETE => "delete",
        b'\n' => "enter",
        b' ' => "space",
        b'\t' => "tab",
        0x08 => "backspace",
        0x1B => "escape",
        0x01..=0x1A => return format!("ctrl+{}", (b'a' + ch - 1) as char),
        0x21..=0x7E => return (ch as char).to_string(),
        _ => "unknown",
    };
    named.into()
}
