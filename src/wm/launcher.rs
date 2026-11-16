//! The program menu: a round gear button at the bottom left, next to the Dock. A click turns
//! the gear, and a panel with Lua programs by category slides up from the bottom
//! (`crate::appmenu`). A right click edits: open, order, remove from
//! the menu, add your own program, delete a category, the "Edit Menu" window (categories are
//! created and renamed there too).

use super::{ACCENT, BLUR, DOCK_MARGIN, Request, SHADOW_PAD, TEXT, TEXT_DIM, TOPBAR_H, Wm};
use crate::appmenu::{self, AppMenu};
use crate::apps::dialog::{self, Mode, Reply};
use crate::apps::ui::{self as widgets, Icon};
use crate::font::{self, FontId};
use crate::gfx::{Canvas, Rect};
use crate::interrupts::{self, HZ};
use crate::lua;
use alloc::string::String;
use alloc::vec::Vec;

const BUTTON: i32 = 44;
const PANEL_W: i32 = 300;
const HEAD_H: i32 = 30;
const ROW_H: i32 = 32;
const PAD: i32 = 8;
/// The duration of the panel slide and the gear turn.
const ANIM: u64 = HZ * 22 / 100;
/// How far the gear turns per opening (two teeth of eight).
const TURN: f32 = core::f32::consts::FRAC_PI_2;
const HOVER: u32 = 0x3B82F6;
const ESC: u8 = 0x1B;

/// What is under the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Hit {
    Button,
    Head(usize),
    App(usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    None,
    Open,
    Up,
    Down,
    Remove,
    CategoryUp,
    CategoryDown,
    DeleteCategory,
    Add,
    Edit,
}

#[derive(Default)]
pub(super) struct Launcher {
    /// Open (or opening); a closed one is still visible while it slides away.
    open: bool,
    /// The tick of the last opening or closing — for the animation.
    since: u64,
    menu: AppMenu,
    hover: Option<Hit>,
    scroll: i32,
    /// A right click: the action menu and what was clicked.
    context: Option<(widgets::Menu<Action>, Option<Hit>)>,
    /// Waiting for the "Add program…" dialog's answer: into which category.
    adding: Option<(Reply, String)>,
}

/// Draw timing: cycles of the button, the panel, the number of panel passes, when it was printed.
static COST: (core::sync::atomic::AtomicU64, core::sync::atomic::AtomicU64, core::sync::atomic::AtomicU64, core::sync::atomic::AtomicU64) =
    (core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0));

/// Panel rows from top to bottom: category headers and programs.
fn lines(m: &AppMenu) -> Vec<Hit> {
    let mut out = Vec::new();
    for (c, cat) in m.categories.iter().enumerate() {
        out.push(Hit::Head(c));
        out.extend((0..cat.entries.len()).map(|e| Hit::App(c, e)));
    }
    out
}

fn line_h(h: Hit) -> i32 {
    if matches!(h, Hit::Head(_)) { HEAD_H } else { ROW_H }
}

/// Smooth easing towards the end: 0..1 → 0..1.
fn ease(t: f32) -> f32 {
    let r = 1.0 - t.clamp(0.0, 1.0);
    1.0 - r * r * r
}

impl Wm {
    pub(super) fn launcher_button(&self) -> Rect {
        let d = self.dock_rect();
        Rect::new(self.left() + DOCK_MARGIN, d.y + (d.h - BUTTON) / 2, BUTTON, BUTTON)
    }

    /// How far the panel has slid out: 0 — not at all, 1 — fully.
    fn launcher_shown(&self) -> f32 {
        let l = &self.launcher;
        let t = ease(interrupts::ticks().saturating_sub(l.since) as f32 / ANIM as f32);
        if l.open { t } else { 1.0 - t }
    }

    fn launcher_animating(&self) -> bool {
        interrupts::ticks() < self.launcher.since + ANIM + 1
    }

    fn launcher_content_h(&self) -> i32 {
        let h: i32 = lines(&self.launcher.menu).into_iter().map(line_h).sum();
        h.max(ROW_H)
    }

    /// The whole panel (when slid out).
    fn launcher_panel(&self) -> Rect {
        let b = self.launcher_button();
        let bottom = b.y - 10;
        let room = bottom - (self.top() + TOPBAR_H + 10);
        let h = (self.launcher_content_h() + 2 * PAD).min(room).max(ROW_H + 2 * PAD);
        Rect::new(b.x, bottom - h, PANEL_W, h)
    }

    /// The currently visible part of the panel (during the slide — the bottom strip).
    fn launcher_visible(&self) -> Option<Rect> {
        if !self.launcher.open && !self.launcher_animating() {
            return None;
        }
        let p = self.launcher_panel();
        let h = (p.h as f32 * self.launcher_shown()) as i32;
        (h > 0).then(|| Rect::new(p.x, p.y, p.w, h))
    }

    /// The glass areas of the program menu (for blurring under them).
    pub(super) fn launcher_regions(&self) -> Vec<(Rect, i32)> {
        let mut out = Vec::new();
        if let Some(v) = self.launcher_visible() {
            out.push((v, BLUR));
        }
        if let Some((m, _)) = &self.launcher.context {
            out.push((m.rect(), BLUR));
        }
        out
    }

    fn launcher_max_scroll(&self) -> i32 {
        (self.launcher_content_h() + 2 * PAD - self.launcher_panel().h).max(0)
    }

    /// Rows with their rectangles (in the slid-out panel).
    fn launcher_rows(&self) -> Vec<(Hit, Rect)> {
        let p = self.launcher_panel();
        let mut y = p.y + PAD - self.launcher.scroll;
        let mut out = Vec::new();
        for h in lines(&self.launcher.menu) {
            let lh = line_h(h);
            out.push((h, Rect::new(p.x + 6, y, p.w - 12, lh)));
            y += lh;
        }
        out
    }

    fn launcher_hit(&self, x: i32, y: i32) -> Option<Hit> {
        if self.launcher_button().contains(x, y) {
            return Some(Hit::Button);
        }
        let p = self.launcher_panel();
        if !self.launcher.open || !p.contains(x, y) {
            return None;
        }
        self.launcher_rows().into_iter().find(|(_, r)| r.contains(x, y) && r.intersect(p).contains(x, y)).map(|(h, _)| h)
    }

    fn mark_launcher(&mut self) {
        let mut r = self.launcher_panel().inset(-SHADOW_PAD);
        r = r.union(self.launcher_button().inset(-SHADOW_PAD));
        self.mark(r);
        if let Some((m, _)) = &self.launcher.context {
            let r = m.rect().inset(-SHADOW_PAD);
            self.mark(r);
        }
    }

    pub(super) fn launcher_open(&self) -> bool {
        self.launcher.open
    }

    pub(super) fn open_launcher(&mut self) {
        if self.launcher.open {
            return;
        }
        self.close_menu();
        self.close_volume();
        self.close_bt_menu();
        self.launcher.menu = AppMenu::load();
        self.launcher.scroll = 0;
        self.launcher.hover = None;
        self.launcher_toggle(true);
    }

    pub(super) fn close_launcher(&mut self) {
        self.close_launcher_context();
        if self.launcher.open {
            self.launcher_toggle(false);
        }
    }

    fn launcher_toggle(&mut self, open: bool) {
        let now = interrupts::ticks();
        // An unfinished animation was interrupted — continue from the same place.
        let done = (now.saturating_sub(self.launcher.since)).min(ANIM);
        self.launcher.since = now - (ANIM - done);
        self.launcher.open = open;
        self.mark_launcher();
    }

    fn close_launcher_context(&mut self) {
        if let Some((m, _)) = self.launcher.context.take() {
            self.mark(m.rect().inset(-SHADOW_PAD));
        }
    }

    /// The mouse. true — the program menu took the event.
    pub(super) fn launcher_mouse(&mut self, x: i32, y: i32, pressed: bool, right: bool) -> bool {
        // The action menu is open: it takes everything.
        if let Some((m, _)) = &mut self.launcher.context {
            let item = m.item_at(x, y);
            if pressed || right {
                let (m, on) = self.launcher.context.take().unwrap();
                self.mark(m.rect().inset(-SHADOW_PAD));
                if let Some(i) = item
                    && m.items[i].2
                {
                    self.launcher_run(m.items[i].1, on);
                }
                return true;
            }
            if m.hover != item {
                m.hover = item;
                let r = m.rect();
                self.mark(r);
            }
            return true;
        }
        let hit = self.launcher_hit(x, y);
        if hit != self.launcher.hover {
            self.launcher.hover = hit;
            self.mark_launcher();
        }
        if right && (hit.is_some() || (self.launcher.open && self.launcher_panel().contains(x, y))) {
            self.launcher_context(x, y, hit);
            return true;
        }
        if !pressed {
            return self.launcher.open && self.launcher_panel().contains(x, y);
        }
        match hit {
            Some(Hit::Button) => {
                if self.launcher.open {
                    self.close_launcher();
                } else {
                    self.open_launcher();
                }
                true
            }
            Some(Hit::App(c, e)) => {
                let path = self.launcher.menu.categories[c].entries[e].path.clone();
                self.close_launcher();
                self.handle_request(Request::Run(path));
                true
            }
            Some(Hit::Head(_)) => true,
            None if self.launcher.open && self.launcher_panel().contains(x, y) => true,
            None => {
                // A click elsewhere closes the panel and goes on (to a window, the desktop).
                if self.launcher.open {
                    self.close_launcher();
                }
                false
            }
        }
    }

    fn launcher_context(&mut self, x: i32, y: i32, hit: Option<Hit>) {
        let m = &self.launcher.menu;
        let sep = ("", Action::None, false);
        let common = [("Add program\u{2026}", Action::Add, true), ("Edit menu\u{2026}", Action::Edit, true)];
        let mut items: Vec<(&'static str, Action, bool)> = match hit {
            Some(Hit::App(c, e)) => {
                let last = m.categories[c].entries.len() - 1;
                alloc::vec![("Open", Action::Open, true), ("Move up", Action::Up, e > 0), ("Move down", Action::Down, e < last), ("Remove from menu", Action::Remove, true), sep]
            }
            Some(Hit::Head(c)) => {
                let last = m.categories.len() - 1;
                alloc::vec![("Move up", Action::CategoryUp, c > 0), ("Move down", Action::CategoryDown, c < last), ("Delete category", Action::DeleteCategory, true), sep]
            }
            _ => Vec::new(),
        };
        items.extend(common);
        if !self.launcher.open {
            self.open_launcher();
        }
        self.close_launcher_context();
        let menu = widgets::Menu::new(x, y, items, self.right(), self.bottom());
        self.launcher.context = Some((menu, hit));
        self.mark_launcher();
    }

    fn launcher_run(&mut self, action: Action, on: Option<Hit>) {
        let m = &mut self.launcher.menu;
        let category = match on {
            Some(Hit::App(c, _) | Hit::Head(c)) => m.categories.get(c).map(|c| c.name.clone()),
            _ => None,
        };
        let mut changed = true;
        match (action, on) {
            (Action::Open, Some(Hit::App(c, e))) => {
                let path = m.categories[c].entries[e].path.clone();
                self.close_launcher();
                self.handle_request(Request::Run(path));
                return;
            }
            (Action::Up | Action::Down, Some(Hit::App(c, e))) => {
                m.move_entry(c, e, if action == Action::Up { -1 } else { 1 });
            }
            (Action::Remove, Some(Hit::App(c, e))) => {
                let path = m.categories[c].entries[e].path.clone();
                m.remove(&path);
            }
            (Action::CategoryUp | Action::CategoryDown, Some(Hit::Head(c))) => {
                m.move_category(c, if action == Action::CategoryUp { -1 } else { 1 });
            }
            (Action::DeleteCategory, Some(Hit::Head(c))) => m.delete_category(c),
            (Action::Add, _) => {
                changed = false;
                let reply = dialog::request(Mode::Open, "/apps", "");
                self.launcher.adding = Some((reply, category.unwrap_or_else(|| String::from(appmenu::OTHER))));
                self.close_launcher();
            }
            (Action::Edit, _) => {
                changed = false;
                self.close_launcher();
                self.launch("menuedit", None, None);
            }
            _ => changed = false,
        }
        if changed {
            self.launcher.menu.save();
            self.launcher.scroll = self.launcher.scroll.min(self.launcher_max_scroll());
            self.mark_launcher();
        }
    }

    pub(super) fn launcher_wheel(&mut self, x: i32, y: i32, clicks: i32) -> bool {
        if !self.launcher.open || !self.launcher_panel().contains(x, y) {
            return false;
        }
        let s = (self.launcher.scroll + clicks * ROW_H).clamp(0, self.launcher_max_scroll());
        if s != self.launcher.scroll {
            self.launcher.scroll = s;
            self.mark_launcher();
        }
        true
    }

    pub(super) fn launcher_key(&mut self, ch: u8) -> bool {
        if ch == ESC && (self.launcher.open || self.launcher.context.is_some()) {
            if self.launcher.context.is_some() {
                self.close_launcher_context();
            } else {
                self.close_launcher();
            }
            return true;
        }
        false
    }

    /// Every tick: animation frames; the "Add program…" dialog's answer.
    pub(super) fn launcher_tick(&mut self) {
        use core::sync::atomic::Ordering::Relaxed;
        let now = interrupts::ticks();
        if now >= COST.3.load(Relaxed) + 2 * HZ {
            COST.3.store(now, Relaxed);
            let (b, p, n) = (COST.0.swap(0, Relaxed), COST.1.swap(0, Relaxed), COST.2.swap(0, Relaxed));
            if b + p > 20_000_000 {
                crate::kprintln!("launcher: 2 s of drawing: button {} Mcycles, panel {} Mcycles in {n} passes", b / 1_000_000, p / 1_000_000);
            }
        }
        if self.launcher_animating() {
            self.mark_launcher();
        }
        if let Some((reply, _)) = &self.launcher.adding
            && let Some(answer) = dialog::take(reply)
        {
            let (_, category) = self.launcher.adding.take().unwrap();
            if let Some(path) = answer {
                if lua::is_program(&path) {
                    let mut m = AppMenu::load();
                    m.add(&category, &path);
                    m.save();
                    self.launcher.menu = m;
                    super::notify("Added to the app menu", &alloc::format!("{} \u{2192} {category}", appmenu::title_of(&path)));
                } else {
                    super::notify("Not a program", "Only Lua programs (*.lua) can go to the app menu");
                }
            }
        }
    }

    /// The button (on every screen with a Dock).
    pub(super) fn draw_launcher_button(&self, c: &mut Canvas) {
        let t0 = crate::arch::rdtsc();
        self.draw_launcher_button_inner(c);
        COST.0.fetch_add(crate::arch::rdtsc() - t0, core::sync::atomic::Ordering::Relaxed);
    }

    fn draw_launcher_button_inner(&self, c: &mut Canvas) {
        let b = self.launcher_button();
        let hot = self.launcher.hover == Some(Hit::Button) || self.launcher.open;
        c.shadow(b, BUTTON / 2, 16, 4, 120);
        c.blur_round(b, BLUR, BUTTON / 2);
        c.fill_round(b, BUTTON / 2, 0x10132A, crate::theme::get().bar_alpha);
        c.stroke_round(b, BUTTON / 2, 0xFFFFFF, if hot { 90 } else { 45 });
        if hot {
            c.fill_round(b, BUTTON / 2, 0xFFFFFF, 22);
        }
        // The turn: by TURN forward when opening, back when closing.
        let angle = TURN * self.launcher_shown();
        let color = if self.launcher.open { ACCENT } else { TEXT };
        c.gear(b.x + BUTTON / 2, b.y + BUTTON / 2, 13, 8, angle, color, 256);
    }

    pub(super) fn draw_launcher_panel(&self, c: &mut Canvas) {
        let t0 = crate::arch::rdtsc();
        self.draw_launcher_panel_inner(c);
        COST.1.fetch_add(crate::arch::rdtsc() - t0, core::sync::atomic::Ordering::Relaxed);
        COST.2.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    fn draw_launcher_panel_inner(&self, c: &mut Canvas) {
        let Some(vis) = self.launcher_visible() else { return };
        let p = self.launcher_panel();
        // The panel slides up from the bottom: draw it shifted down and clip it to the visible part.
        let dy = p.h - vis.h;
        let saved = c.clip();
        c.shadow(vis, 14, 24, 8, 150);
        c.set_clip(vis.intersect(saved));
        let at = p.offset(0, dy);
        c.blur_round(vis, BLUR, 14);
        c.fill_round(at, 14, 0x1A1D2E, crate::theme::get().menu_alpha);
        c.stroke_round(at, 14, 0xFFFFFF, 50);
        c.set_clip(vis.intersect(p.inset(2).offset(0, dy)).intersect(saved));
        let m = &self.launcher.menu;
        if m.categories.iter().all(|c| c.entries.is_empty()) && m.categories.is_empty() {
            c.text(at.x + 16, at.y + PAD + 7, "No programs. Right-click to add one.", TEXT_DIM, FontId::Regular, 13.0);
        }
        for (h, r) in self.launcher_rows() {
            let r = r.offset(0, dy);
            match h {
                Hit::Head(ci) => {
                    let name = &m.categories[ci].name;
                    c.text(r.x + 10, r.y + 10, &name.to_uppercase(), TEXT_DIM, FontId::SemiBold, 11.5);
                    let tw = font::measure(FontId::SemiBold, 11.5, &name.to_uppercase());
                    c.fill(Rect::new(r.x + 18 + tw, r.y + 17, r.w - 28 - tw, 1), 0x3A3F55);
                }
                Hit::App(ci, ei) => {
                    let e = &m.categories[ci].entries[ei];
                    let hovered = self.launcher.hover == Some(h) || self.launcher.context.as_ref().is_some_and(|(_, on)| *on == Some(h));
                    if hovered {
                        c.fill_round(r, 8, HOVER, 220);
                    }
                    widgets::icon(c, r.x + 10, r.y + 7, 18, Icon::Program);
                    let title = widgets::fit(&e.title, FontId::Regular, 14.0, r.w - 50);
                    c.text(r.x + 38, r.y + 7, &title, TEXT, FontId::Regular, 14.0);
                }
                Hit::Button => {}
            }
        }
        // A scroll bar, if not everything fits.
        let max = self.launcher_max_scroll();
        if max > 0 {
            let track = at.h - 2 * PAD;
            let content = self.launcher_content_h() + 2 * PAD;
            let thumb = (track * at.h / content).clamp(24, track);
            let y = at.y + PAD + (track - thumb) * self.launcher.scroll / max;
            c.fill_round(Rect::new(at.x + at.w - 7, y, 4, thumb), 2, 0xFFFFFF, 70);
        }
        c.set_clip(saved);
        if let Some((menu, _)) = &self.launcher.context {
            menu.draw(c, Rect::new(0, 0, 0, 0), |a| matches!(a, Action::Remove | Action::DeleteCategory));
        }
    }
}
