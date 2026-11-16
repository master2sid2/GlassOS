//! Screenshots: the screen "freezes", an area is selected with the mouse, it can be drawn on
//! (pencil, lines, arrows, rectangles, ellipses, numbered markers),
//! and the result can be copied to the clipboard or saved as a PNG.
//!
//! This is a layer above everything (like a menu), not a window: the panel and the dock end up in the shot too.

use super::{ACCENT, BLUR, TEXT, TEXT_DIM, Wm};
use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect};
use crate::input::RIGHT;
use alloc::format;
use alloc::vec::Vec;

const COLORS: [Color; 7] = [0xEF4444, 0xF59E0B, 0xFACC15, 0x22C55E, 0x3B82F6, 0xFFFFFF, 0x111111];
const TOOLS: [Tool; 7] = [Tool::Select, Tool::Pen, Tool::Line, Tool::Arrow, Tool::Rect, Tool::Ellipse, Tool::Number];
const TOOL_HINTS: [&str; 7] = ["Move or resize", "Pen", "Line", "Arrow", "Rectangle", "Ellipse", "Numbered mark"];
const ACTIONS: [&str; 3] = ["Undo", "Copy", "Save"];
const BTN: i32 = 34;
const GAP: i32 = 4;
const SWATCH: i32 = 24;
const BAR_H: i32 = 46;
const WIDTH: i32 = 2; // half the line thickness
const MARK_R: i32 = 13;
/// The width of the zone at the area edge by which it can be dragged.
const GRIP: i32 = 7;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Select,
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Number,
}

#[derive(Clone)]
enum Mark {
    Pen(Vec<(i32, i32)>, Color),
    Line((i32, i32), (i32, i32), Color),
    Arrow((i32, i32), (i32, i32), Color),
    Rect((i32, i32), (i32, i32), Color),
    Ellipse((i32, i32), (i32, i32), Color),
    Number((i32, i32), u32, Color),
}

enum Drag {
    /// A new area from a point.
    Select(i32, i32),
    /// Moving the area: the grab offset.
    Move(i32, i32),
    /// An area edge: which edges (left, right, top, bottom) and the original rectangle.
    Resize(u8, Rect, i32, i32),
    /// Drawing a mark (the last one in the list).
    Draw,
}

pub struct Shot {
    image: Canvas,
    w: i32,
    h: i32,
    sel: Option<Rect>,
    tool: Tool,
    color: Color,
    marks: Vec<Mark>,
    number: u32,
    drag: Option<Drag>,
    /// Something changed — redraw.
    dirty: bool,
    /// The toolbar button under the cursor (for highlighting).
    hover: Option<usize>,
    /// How many more frames to draw as usual (so a closed menu does not end up in the shot).
    pub warmup: u8,
}

fn norm(a: (i32, i32), b: (i32, i32)) -> Rect {
    Rect::new(a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())
}

// ---------------------------------------------------------------- drawing marks

fn dot(c: &mut Canvas, x: i32, y: i32, r: i32, color: Color) {
    c.fill_round(Rect::new(x - r, y - r, 2 * r + 1, 2 * r + 1), r, color, 256);
}

fn line(c: &mut Canvas, a: (i32, i32), b: (i32, i32), r: i32, color: Color) {
    let steps = (b.0 - a.0).abs().max((b.1 - a.1).abs()).max(1);
    for i in 0..=steps {
        let x = a.0 + (b.0 - a.0) * i / steps;
        let y = a.1 + (b.1 - a.1) * i / steps;
        dot(c, x, y, r, color);
    }
}

fn arrow(c: &mut Canvas, a: (i32, i32), b: (i32, i32), r: i32, color: Color) {
    line(c, a, b, r, color);
    let angle = libm::atan2f((b.1 - a.1) as f32, (b.0 - a.0) as f32);
    let len = (12 + r * 4) as f32;
    for side in [-0.45f32, 0.45] {
        let t = angle + core::f32::consts::PI + side;
        let p = (b.0 + (libm::cosf(t) * len) as i32, b.1 + (libm::sinf(t) * len) as i32);
        line(c, b, p, r, color);
    }
}

fn ellipse(c: &mut Canvas, a: (i32, i32), b: (i32, i32), r: i32, color: Color) {
    let box_ = norm(a, b);
    let (cx, cy) = (box_.x as f32 + box_.w as f32 / 2.0, box_.y as f32 + box_.h as f32 / 2.0);
    let (rx, ry) = (box_.w as f32 / 2.0, box_.h as f32 / 2.0);
    let steps = ((rx + ry) * 3.2) as i32 + 8;
    for i in 0..steps {
        let t = i as f32 * core::f32::consts::TAU / steps as f32;
        dot(c, (cx + libm::cosf(t) * rx) as i32, (cy + libm::sinf(t) * ry) as i32, r, color);
    }
}

/// A circle marker with a number; on a light colour the number is black.
fn number(c: &mut Canvas, p: (i32, i32), n: u32, color: Color) {
    c.fill_round(Rect::new(p.0 - MARK_R - 1, p.1 - MARK_R + 1, 2 * MARK_R + 2, 2 * MARK_R + 2), MARK_R + 1, 0x000000, 90);
    dot(c, p.0, p.1, MARK_R, color);
    let light = ((color >> 16 & 0xFF) * 3 + (color >> 8 & 0xFF) * 6 + (color & 0xFF)) / 10 > 160;
    let label = format!("{n}");
    let tw = font::measure(FontId::SemiBold, 14.0, &label);
    c.text(p.0 - tw / 2, p.1 - 9, &label, if light { 0x111111 } else { 0xFFFFFF }, FontId::SemiBold, 14.0);
}

/// A mark shifted by `(dx, dy)` (for the cropped picture).
fn draw_mark(c: &mut Canvas, m: &Mark, dx: i32, dy: i32) {
    let o = |p: (i32, i32)| (p.0 + dx, p.1 + dy);
    match m {
        Mark::Pen(points, color) => {
            for w in points.windows(2) {
                line(c, o(w[0]), o(w[1]), WIDTH, *color);
            }
            if let [p] = points.as_slice() {
                dot(c, o(*p).0, o(*p).1, WIDTH, *color);
            }
        }
        Mark::Line(a, b, color) => line(c, o(*a), o(*b), WIDTH, *color),
        Mark::Arrow(a, b, color) => arrow(c, o(*a), o(*b), WIDTH, *color),
        Mark::Rect(a, b, color) => {
            let r = norm(o(*a), o(*b));
            let corners = [(r.x, r.y), (r.x + r.w, r.y), (r.x + r.w, r.y + r.h), (r.x, r.y + r.h)];
            for i in 0..4 {
                line(c, corners[i], corners[(i + 1) % 4], WIDTH, *color);
            }
        }
        Mark::Ellipse(a, b, color) => ellipse(c, o(*a), o(*b), WIDTH, *color),
        Mark::Number(p, n, color) => number(c, o(*p), *n, *color),
    }
}

// ---------------------------------------------------------------- tool icons

fn tool_icon(c: &mut Canvas, r: Rect, tool: Tool, color: Color, next: u32) {
    let (x, y) = (r.x + 9, r.y + 9);
    match tool {
        Tool::Select => {
            // A dashed frame.
            for i in (0..16).step_by(4) {
                c.fill(Rect::new(x + i, y, 2, 1), TEXT);
                c.fill(Rect::new(x + i, y + 15, 2, 1), TEXT);
                c.fill(Rect::new(x, y + i, 1, 2), TEXT);
                c.fill(Rect::new(x + 15, y + i, 1, 2), TEXT);
            }
        }
        Tool::Pen => {
            let pts = [(x, y + 12), (x + 4, y + 6), (x + 8, y + 10), (x + 12, y + 3), (x + 15, y + 6)];
            for w in pts.windows(2) {
                line(c, w[0], w[1], 1, TEXT);
            }
        }
        Tool::Line => line(c, (x, y + 15), (x + 15, y), 1, TEXT),
        Tool::Arrow => arrow(c, (x, y + 15), (x + 15, y), 1, TEXT),
        Tool::Rect => {
            for (a, b) in [((x, y + 2), (x + 15, y + 2)), ((x + 15, y + 2), (x + 15, y + 13)), ((x + 15, y + 13), (x, y + 13)), ((x, y + 13), (x, y + 2))] {
                line(c, a, b, 1, TEXT);
            }
        }
        Tool::Ellipse => ellipse(c, (x, y + 1), (x + 15, y + 14), 1, TEXT),
        Tool::Number => number(c, (x + 8, y + 8), next, color),
    }
}

impl Shot {
    fn bar(&self) -> Option<Rect> {
        let sel = self.sel?;
        let w = TOOLS.len() as i32 * (BTN + GAP) + 10 + COLORS.len() as i32 * (SWATCH + GAP) + 10 + ACTIONS.len() as i32 * (56 + GAP) + BTN + 16;
        let x = (sel.x + sel.w / 2 - w / 2).clamp(8, self.w - w - 8);
        let below = sel.y + sel.h + 10;
        let y = if below + BAR_H < self.h - 8 {
            below
        } else if sel.y - 10 - BAR_H > 8 {
            sel.y - 10 - BAR_H
        } else {
            sel.y + sel.h - BAR_H - 10
        };
        Some(Rect::new(x, y, w, BAR_H))
    }

    /// Toolbar buttons in order: tools, colours, actions, close.
    fn buttons(&self) -> Vec<Rect> {
        let Some(bar) = self.bar() else { return Vec::new() };
        let mut out = Vec::new();
        let mut x = bar.x + 8;
        let y = bar.y + (BAR_H - BTN) / 2;
        for _ in TOOLS {
            out.push(Rect::new(x, y, BTN, BTN));
            x += BTN + GAP;
        }
        x += 10;
        for _ in COLORS {
            out.push(Rect::new(x, bar.y + (BAR_H - SWATCH) / 2, SWATCH, SWATCH));
            x += SWATCH + GAP;
        }
        x += 10;
        for _ in ACTIONS {
            out.push(Rect::new(x, y, 56, BTN));
            x += 56 + GAP;
        }
        out.push(Rect::new(x, y, BTN, BTN));
        out
    }

    /// The area edges under a point: bits left 1, right 2, top 4, bottom 8.
    fn edges_at(&self, x: i32, y: i32) -> u8 {
        let Some(s) = self.sel else { return 0 };
        if !s.inset(-GRIP).contains(x, y) {
            return 0;
        }
        let mut e = 0;
        if (x - s.x).abs() <= GRIP {
            e |= 1;
        }
        if (x - (s.x + s.w)).abs() <= GRIP {
            e |= 2;
        }
        if (y - s.y).abs() <= GRIP {
            e |= 4;
        }
        if (y - (s.y + s.h)).abs() <= GRIP {
            e |= 8;
        }
        e
    }

    /// The cropped area with the marks.
    fn result(&self) -> Option<(i32, i32, Vec<u32>)> {
        let s = self.sel.filter(|s| s.w > 0 && s.h > 0)?;
        let mut c = Canvas::new(s.w, s.h);
        for row in 0..s.h {
            let src = ((s.y + row) * self.w + s.x) as usize;
            let dst = (row * s.w) as usize;
            c.px[dst..dst + s.w as usize].copy_from_slice(&self.image.px[src..src + s.w as usize]);
        }
        for m in &self.marks {
            draw_mark(&mut c, m, -s.x, -s.y);
        }
        Some((s.w, s.h, c.px.iter().map(|p| p & 0xFF_FFFF).collect()))
    }
}

impl Wm {
    /// Starts a screenshot: the picture is taken from the next frame (see `render_shot`).
    pub(super) fn start_shot(&mut self) {
        if self.shot.is_some() {
            return;
        }
        self.close_menu();
        self.shot = Some(Shot {
            image: Canvas::new(0, 0),
            w: self.desk.w,
            h: self.desk.h,
            sel: None,
            tool: Tool::Arrow,
            color: COLORS[0],
            marks: Vec::new(),
            number: 1,
            drag: None,
            dirty: true,
            hover: None,
            warmup: 2,
        });
    }

    fn end_shot(&mut self) {
        self.shot = None;
        self.mark(self.desk);
    }

    /// A frame in screenshot mode. Returns the areas to output to the screen.
    pub(super) fn render_shot(&mut self, scenes: &mut [Canvas]) -> Vec<Rect> {
        let screen = self.desk;
        let shot = self.shot.as_mut().unwrap();
        if shot.image.px.is_empty() {
            // The screen "freezes": the scenes hold the last composed frame (without the cursor). The shot
            // is one for the whole desktop, in desktop units.
            let mut all = Canvas::new(screen.w, screen.h);
            for scene in scenes.iter() {
                let at = scene.bounds();
                all.replace(scene, at.x, at.y);
            }
            shot.image = all;
        }
        if !shot.dirty {
            return Vec::new();
        }
        shot.dirty = false;
        for c in scenes.iter_mut() {
            self.draw_shot(c);
        }
        alloc::vec![screen]
    }

    fn draw_shot(&mut self, c: &mut Canvas) {
        let screen = self.desk;
        let shot = self.shot.as_mut().unwrap();
        c.reset_clip();
        c.replace(&shot.image, 0, 0);
        let Some(sel) = shot.sel.filter(|s| s.w > 0 && s.h > 0) else {
            c.blend(screen, 0x000000, 110);
            let hint = "Drag to select an area  \u{00B7}  Esc to cancel";
            let tw = font::measure(FontId::SemiBold, 16.0, hint);
            let r = Rect::new(self.home.x + (self.w - tw) / 2 - 20, self.home.y + self.h / 2 - 22, tw + 40, 44);
            c.fill_round(r, 14, 0x161A2C, 220);
            c.text(r.x + 20, r.y + 12, hint, TEXT, FontId::SemiBold, 16.0);
            return;
        };
        // Dimming around the area.
        for r in [
            Rect::new(0, 0, self.desk.w, sel.y),
            Rect::new(0, sel.y + sel.h, self.desk.w, self.desk.h - sel.y - sel.h),
            Rect::new(0, sel.y, sel.x, sel.h),
            Rect::new(sel.x + sel.w, sel.y, self.desk.w - sel.x - sel.w, sel.h),
        ] {
            c.blend(r, 0x000000, 130);
        }
        c.set_clip(sel);
        for m in &shot.marks {
            draw_mark(c, m, 0, 0);
        }
        c.reset_clip();
        c.stroke_round(sel.inset(-1), 2, ACCENT, 256);
        for (hx, hy) in [(sel.x, sel.y), (sel.x + sel.w, sel.y), (sel.x, sel.y + sel.h), (sel.x + sel.w, sel.y + sel.h)] {
            c.fill_round(Rect::new(hx - 4, hy - 4, 9, 9), 4, 0xFFFFFF, 256);
            c.stroke_round(Rect::new(hx - 4, hy - 4, 9, 9), 4, ACCENT, 256);
        }
        // The area size.
        let size = format!("{} \u{00D7} {}", sel.w, sel.h);
        let sw = font::measure(FontId::Regular, 12.0, &size);
        let ly = if sel.y > 30 { sel.y - 26 } else { sel.y + 6 };
        let label = Rect::new(sel.x, ly, sw + 16, 20);
        c.fill_round(label, 6, 0x161A2C, 220);
        c.text(label.x + 8, label.y + 3, &size, TEXT, FontId::Regular, 12.0);

        // The toolbar.
        let shot = self.shot.as_ref().unwrap();
        let bar = shot.bar().unwrap();
        c.shadow(bar, 14, 18, 4, 150);
        c.blur(bar, BLUR);
        c.fill_round(bar, 14, 0x161A2C, 225);
        c.stroke_round(bar, 14, 0xFFFFFF, 45);
        let buttons = shot.buttons();
        let n_tools = TOOLS.len();
        for (i, r) in buttons.iter().enumerate() {
            let hover = shot.hover == Some(i);
            if i < n_tools {
                if TOOLS[i] == shot.tool {
                    c.fill_round(*r, 8, ACCENT, 80);
                } else if hover {
                    c.fill_round(*r, 8, 0xFFFFFF, 30);
                }
                tool_icon(c, *r, TOOLS[i], shot.color, shot.number);
            } else if i < n_tools + COLORS.len() {
                let color = COLORS[i - n_tools];
                if color == shot.color {
                    c.fill_round(r.inset(-3), SWATCH / 2 + 3, ACCENT, 256);
                    c.fill_round(r.inset(-1), SWATCH / 2 + 1, 0x161A2C, 256);
                }
                c.fill_round(*r, SWATCH / 2, color, 256);
                c.stroke_round(*r, SWATCH / 2, 0xFFFFFF, 50);
            } else if i < buttons.len() - 1 {
                let label = ACTIONS[i - n_tools - COLORS.len()];
                let enabled = label != "Undo" || !shot.marks.is_empty();
                c.fill_round(*r, 8, 0xFFFFFF, if hover && enabled { 40 } else { 18 });
                let tw = font::measure(FontId::Regular, 13.0, label);
                c.text(r.x + (r.w - tw) / 2, r.y + 9, label, if enabled { TEXT } else { TEXT_DIM }, FontId::Regular, 13.0);
            } else {
                // Close: a cross.
                c.fill_round(*r, 8, if hover { 0xEF4444 } else { 0xFFFFFF }, if hover { 200 } else { 18 });
                let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
                line(c, (cx - 6, cy - 6), (cx + 6, cy + 6), 1, TEXT);
                line(c, (cx - 6, cy + 6), (cx + 6, cy - 6), 1, TEXT);
            }
        }
        // The tooltip for the tool under the cursor.
        if let Some(i) = shot.hover.filter(|&i| i < n_tools) {
            let hint = TOOL_HINTS[i];
            let hw = font::measure(FontId::Regular, 12.0, hint);
            let r = buttons[i];
            let tip = Rect::new(r.x + r.w / 2 - hw / 2 - 8, bar.y - 26, hw + 16, 20);
            c.fill_round(tip, 6, 0x161A2C, 230);
            c.text(tip.x + 8, tip.y + 3, hint, TEXT, FontId::Regular, 12.0);
        }
    }

    /// The mouse in screenshot mode (all input goes here).
    pub(super) fn shot_mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool, released: bool) {
        let right = buttons & RIGHT != 0;
        let Some(shot) = self.shot.as_mut() else { return };
        let x = x.clamp(0, shot.w - 1);
        let y = y.clamp(0, shot.h - 1);
        if right {
            self.end_shot();
            return;
        }
        // Toolbar button highlighting.
        let hover = shot.buttons().iter().position(|r| r.contains(x, y));
        if hover != shot.hover {
            shot.hover = hover;
            shot.dirty = true;
        }
        if pressed {
            if let Some(i) = hover {
                self.shot_button(i);
                return;
            }
            let edges = shot.edges_at(x, y);
            let inside = shot.sel.is_some_and(|s| s.contains(x, y));
            shot.drag = Some(match shot.tool {
                _ if shot.sel.is_none() => Drag::Select(x, y),
                Tool::Select if edges != 0 => Drag::Resize(edges, shot.sel.unwrap(), x, y),
                Tool::Select if inside => {
                    let s = shot.sel.unwrap();
                    Drag::Move(x - s.x, y - s.y)
                }
                Tool::Select => {
                    // Outside the area — select anew (the marks are reset).
                    shot.marks.clear();
                    shot.number = 1;
                    Drag::Select(x, y)
                }
                tool if inside => {
                    let (p, color) = ((x, y), shot.color);
                    shot.marks.push(match tool {
                        Tool::Pen => Mark::Pen(alloc::vec![p], color),
                        Tool::Line => Mark::Line(p, p, color),
                        Tool::Arrow => Mark::Arrow(p, p, color),
                        Tool::Rect => Mark::Rect(p, p, color),
                        Tool::Ellipse => Mark::Ellipse(p, p, color),
                        _ => {
                            shot.number += 1;
                            Mark::Number(p, shot.number - 1, color)
                        }
                    });
                    Drag::Draw
                }
                _ => return,
            });
            shot.dirty = true;
            return;
        }
        let Some(drag) = &shot.drag else { return };
        match *drag {
            Drag::Select(sx, sy) => shot.sel = Some(norm((sx, sy), (x, y))),
            Drag::Move(dx, dy) => {
                let s = shot.sel.unwrap();
                shot.sel = Some(Rect::new((x - dx).clamp(0, shot.w - s.w), (y - dy).clamp(0, shot.h - s.h), s.w, s.h));
            }
            Drag::Resize(edges, orig, sx, sy) => {
                let (dx, dy) = (x - sx, y - sy);
                let (mut l, mut t, mut r, mut b) = (orig.x, orig.y, orig.x + orig.w, orig.y + orig.h);
                if edges & 1 != 0 {
                    l += dx;
                }
                if edges & 2 != 0 {
                    r += dx;
                }
                if edges & 4 != 0 {
                    t += dy;
                }
                if edges & 8 != 0 {
                    b += dy;
                }
                shot.sel = Some(norm((l, t), (r, b)));
            }
            Drag::Draw => {
                let s = shot.sel.unwrap();
                let p = (x.clamp(s.x, s.x + s.w - 1), y.clamp(s.y, s.y + s.h - 1));
                match shot.marks.last_mut() {
                    Some(Mark::Pen(points, _)) => points.push(p),
                    Some(Mark::Line(_, b, _) | Mark::Arrow(_, b, _) | Mark::Rect(_, b, _) | Mark::Ellipse(_, b, _)) => *b = p,
                    Some(Mark::Number(at, _, _)) => *at = p,
                    None => {}
                }
            }
        }
        shot.dirty = true;
        if released {
            shot.drag = None;
            // A click without dragging — the whole screen.
            if shot.sel.is_some_and(|s| s.w < 4 || s.h < 4) {
                shot.sel = Some(Rect::new(0, 0, shot.w, shot.h));
            }
        }
    }

    fn shot_button(&mut self, i: usize) {
        let shot = self.shot.as_mut().unwrap();
        let (nt, nc) = (TOOLS.len(), COLORS.len());
        shot.dirty = true;
        if i < nt {
            shot.tool = TOOLS[i];
        } else if i < nt + nc {
            shot.color = COLORS[i - nt];
        } else if i == nt + nc {
            // Undo: the last mark (for a marker — its number too).
            if let Some(Mark::Number(..)) = shot.marks.pop() {
                shot.number -= 1;
            }
        } else if i == nt + nc + 1 {
            self.shot_copy();
        } else if i == nt + nc + 2 {
            self.shot_save();
        } else {
            self.end_shot();
        }
    }

    fn shot_copy(&mut self) {
        let Some((w, h, px)) = self.shot.as_ref().and_then(|s| s.result()) else { return };
        super::set_clipboard_image(w, h, px);
        self.end_shot();
        super::notify("Screenshot copied", &format!("{w} \u{00D7} {h} \u{2014} paste it in Paint with Ctrl+V"));
    }

    fn shot_save(&mut self) {
        let Some((w, h, px)) = self.shot.as_ref().and_then(|s| s.result()) else { return };
        let t = crate::time::now();
        let dir = format!("{}/Pictures", crate::access::home());
        let dir = dir.as_str();
        let _ = crate::fs::mkdir(dir);
        let name = format!(
            "Screenshot {}-{:02}-{:02} {:02}-{:02}-{:02}.png",
            t.year, t.month, t.day, t.hour, t.min, t.sec
        );
        let path = format!("{dir}/{}", crate::fs::unique_name(dir, &name));
        let data = crate::png::encode(w, h, &px);
        self.end_shot();
        match crate::fs::write(&path, &data) {
            Ok(()) => super::notify("Screenshot saved", &path),
            Err(e) => super::notify("Screenshot not saved", &format!("{e}")),
        }
    }

    /// Keys in screenshot mode: Esc — exit, Enter / Ctrl+C — copy,
    /// Ctrl+S — save, Ctrl+Z — undo a mark.
    pub(super) fn shot_key(&mut self, ch: u8) {
        let Some(shot) = self.shot.as_mut() else { return };
        let (nt, nc) = (TOOLS.len(), COLORS.len());
        match ch {
            0x1B => self.end_shot(),
            b'\n' | 0x03 => self.shot_copy(),
            0x13 => self.shot_save(),
            0x1A => self.shot_button(nt + nc),
            b'1'..=b'7' => {
                shot.tool = TOOLS[(ch - b'1') as usize];
                shot.dirty = true;
            }
            _ => {}
        }
    }

    pub(super) fn shot_active(&self) -> bool {
        self.shot.is_some()
    }
}
