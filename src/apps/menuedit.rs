//! "Edit App Menu": editing the program menu (the gear button). On the left are categories:
//! create, rename, delete, order. On the right are the programs of the selected category:
//! add your own (any *.lua on disk), remove from the menu, move to another category,
//! order. Every change is saved to ~/.config/appmenu.cfg right away.

use super::dialog::{self, Mode, Reply};
use super::ui::{self, Icon, TextField};
use crate::appmenu::{self, AppMenu};
use crate::font::FontId;
use crate::fs;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::{KEY_DOWN, KEY_UP, LEFT};
use crate::lua;
use crate::wm::{ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

const BG: Color = 0x181B28;
const PAD: i32 = 16;
const LEFT_W: i32 = 220;
const GAP: i32 = 20;
const LIST_Y: i32 = 40;
const CAT_H: i32 = 30;
const APP_H: i32 = 34;
/// Below the lists: a name field and two rows of buttons (on the right — one, and a hint).
const FOOT_H: i32 = 3 * (ui::BUTTON_H + 8) + 10;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    New,
    Rename,
    Delete,
    CatUp,
    CatDown,
    Add,
    Remove,
    MoveTo,
    Up,
    Down,
}

pub struct MenuEditor {
    menu: AppMenu,
    cat: usize,
    entry: Option<usize>,
    name: TextField,
    /// Typing in the name field.
    typing: bool,
    cw: i32,
    ch: i32,
    scroll: (i32, i32),
    mouse: (i32, i32),
    /// "Add…": waiting for a file.
    adding: Option<Reply>,
    /// "Move to…": the category menu.
    move_to: Option<ui::Menu<usize, String>>,
    buttons: Vec<(Rect, Btn)>,
    caret_on: bool,
    status: String,
}

impl MenuEditor {
    pub fn new(cw: i32, ch: i32) -> Self {
        let menu = AppMenu::load();
        let name = menu.categories.first().map_or(String::new(), |c| c.name.clone());
        Self {
            menu,
            cat: 0,
            entry: None,
            name: TextField::new(&name, 40),
            typing: false,
            cw,
            ch,
            scroll: (0, 0),
            mouse: (0, 0),
            adding: None,
            move_to: None,
            buttons: Vec::new(),
            caret_on: true,
            status: String::new(),
        }
    }

    fn left_list(&self) -> Rect {
        Rect::new(PAD, LIST_Y, LEFT_W, self.ch - LIST_Y - FOOT_H - PAD)
    }

    fn right_list(&self) -> Rect {
        let x = PAD + LEFT_W + GAP;
        Rect::new(x, LIST_Y, self.cw - x - PAD, self.ch - LIST_Y - FOOT_H - PAD)
    }

    fn name_box(&self) -> Rect {
        let l = self.left_list();
        Rect::new(l.x, l.y + l.h + 10, l.w, ui::BUTTON_H)
    }

    fn entries_len(&self) -> usize {
        self.menu.categories.get(self.cat).map_or(0, |c| c.entries.len())
    }

    fn select_category(&mut self, i: usize) {
        self.cat = i.min(self.menu.categories.len().saturating_sub(1));
        self.entry = None;
        self.scroll.1 = 0;
        let name = self.menu.categories.get(self.cat).map_or(String::new(), |c| c.name.clone());
        self.name.set_text(&name);
        self.typing = false;
    }

    fn changed(&mut self) {
        self.menu.save();
        self.status.clear();
    }

    fn rename(&mut self) {
        let text = self.name.text.clone();
        if self.menu.categories.is_empty() {
            return;
        }
        if self.menu.rename(self.cat, &text) {
            self.changed();
            self.typing = false;
        } else {
            self.status = String::from("The name is empty or already taken");
        }
    }

    fn press(&mut self, b: Btn) {
        match b {
            Btn::New => {
                let i = self.menu.new_category("New category");
                self.changed();
                self.select_category(i);
                self.name.select_all();
                self.typing = true;
            }
            Btn::Rename => self.rename(),
            Btn::Delete => {
                if !self.menu.categories.is_empty() {
                    self.menu.delete_category(self.cat);
                    self.changed();
                    self.select_category(self.cat);
                }
            }
            Btn::CatUp | Btn::CatDown => {
                let j = self.menu.move_category(self.cat, if b == Btn::CatUp { -1 } else { 1 });
                self.changed();
                self.cat = j;
            }
            Btn::Add => {
                if self.adding.is_none() {
                    self.adding = Some(dialog::request(Mode::Open, "/apps", ""));
                }
            }
            Btn::Remove => {
                if let Some(e) = self.entry {
                    let path = self.menu.categories[self.cat].entries[e].path.clone();
                    self.menu.remove(&path);
                    self.changed();
                    self.entry = None;
                }
            }
            Btn::MoveTo => {
                let items: Vec<(String, usize, bool)> = self.menu.categories.iter().enumerate().map(|(i, c)| (c.name.clone(), i, i != self.cat)).collect();
                let r = self.buttons.iter().find(|(_, k)| *k == Btn::MoveTo).map(|(r, _)| *r).unwrap_or(Rect::new(self.cw / 2, self.ch / 2, 0, 0));
                self.move_to = Some(ui::Menu::new(r.x, r.y + r.h + 4, items, self.cw, self.ch));
            }
            Btn::Up | Btn::Down => {
                if let Some(e) = self.entry {
                    self.entry = Some(self.menu.move_entry(self.cat, e, if b == Btn::Up { -1 } else { 1 }));
                    self.changed();
                }
            }
        }
    }

    fn move_entry_to(&mut self, target: usize) {
        let (Some(e), Some(name)) = (self.entry, self.menu.categories.get(target).map(|c| c.name.clone())) else { return };
        let path = self.menu.categories[self.cat].entries[e].path.clone();
        self.menu.add(&name, &path);
        self.changed();
        self.select_category(target);
        self.entry = Some(self.entries_len().saturating_sub(1));
    }

    fn enabled(&self, b: Btn) -> bool {
        let has_cat = !self.menu.categories.is_empty();
        match b {
            Btn::New | Btn::Add => true,
            Btn::Rename => has_cat && self.name.text.trim() != self.menu.categories.get(self.cat).map_or("", |c| c.name.as_str()),
            Btn::Delete => has_cat,
            Btn::CatUp => self.cat > 0,
            Btn::CatDown => self.cat + 1 < self.menu.categories.len(),
            Btn::Remove => self.entry.is_some(),
            Btn::MoveTo => self.entry.is_some() && self.menu.categories.len() > 1,
            Btn::Up => self.entry.is_some_and(|e| e > 0),
            Btn::Down => self.entry.is_some_and(|e| e + 1 < self.entries_len()),
        }
    }

    fn draw_list_frame(c: &mut Canvas, r: Rect) {
        c.fill_round(r, 8, 0x000000, 50);
        c.stroke_round(r, 8, 0xFFFFFF, 20);
    }
}

impl App for MenuEditor {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        let at = |r: Rect| r.offset(a.x, a.y);
        let saved = c.clip();
        // Categories.
        let l = self.left_list();
        c.text(a.x + l.x, a.y + 14, "Categories", TEXT, FontId::SemiBold, 14.0);
        Self::draw_list_frame(c, at(l));
        c.set_clip(at(l).inset(2).intersect(saved));
        for (i, cat) in self.menu.categories.iter().enumerate() {
            let r = Rect::new(l.x + 4, l.y + 4 + i as i32 * CAT_H - self.scroll.0, l.w - 8, CAT_H - 2);
            if i == self.cat {
                c.fill_round(at(r), 6, ACCENT, 70);
            }
            let label = ui::fit(&cat.name, FontId::Regular, 14.0, r.w - 50);
            c.text(a.x + r.x + 10, a.y + r.y + 6, &label, TEXT, FontId::Regular, 14.0);
            let n = format!("{}", cat.entries.len());
            let nw = crate::font::measure(FontId::Regular, 12.0, &n);
            c.text(a.x + r.x + r.w - 10 - nw, a.y + r.y + 8, &n, TEXT_DIM, FontId::Regular, 12.0);
        }
        c.set_clip(saved);
        // The programs of the selected category.
        let rl = self.right_list();
        let title = match self.menu.categories.get(self.cat) {
            Some(cat) => format!("Programs in \u{201C}{}\u{201D}", cat.name),
            None => String::from("Programs"),
        };
        c.text(a.x + rl.x, a.y + 14, &ui::fit(&title, FontId::SemiBold, 14.0, rl.w), TEXT, FontId::SemiBold, 14.0);
        Self::draw_list_frame(c, at(rl));
        c.set_clip(at(rl).inset(2).intersect(saved));
        if let Some(cat) = self.menu.categories.get(self.cat) {
            if cat.entries.is_empty() {
                c.text(a.x + rl.x + 14, a.y + rl.y + 12, "Empty. Add a program with \u{201C}Add\u{2026}\u{201D}.", TEXT_DIM, FontId::Regular, 13.0);
            }
            for (i, e) in cat.entries.iter().enumerate() {
                let r = Rect::new(rl.x + 4, rl.y + 4 + i as i32 * APP_H - self.scroll.1, rl.w - 8, APP_H - 2);
                if self.entry == Some(i) {
                    c.fill_round(at(r), 6, ACCENT, 70);
                }
                ui::icon(c, a.x + r.x + 8, a.y + r.y + 7, 18, Icon::Program);
                let tw = crate::font::measure(FontId::Regular, 14.0, &e.title).min(r.w / 2);
                c.text(a.x + r.x + 36, a.y + r.y + 8, &ui::fit(&e.title, FontId::Regular, 14.0, r.w / 2), TEXT, FontId::Regular, 14.0);
                let path = ui::fit(&e.path, FontId::Regular, 12.0, r.w - 56 - tw);
                c.text(a.x + r.x + 48 + tw, a.y + r.y + 10, &path, TEXT_DIM, FontId::Regular, 12.0);
            }
        }
        c.set_clip(saved);
        // The category name field and buttons.
        let nb = self.name_box();
        c.fill_round(at(nb), 7, 0x000000, 90);
        c.stroke_round(at(nb), 7, ACCENT, if self.typing { 160 } else { 60 });
        self.name.draw(c, at(nb), self.typing, self.caret_on, "Category name", TEXT);
        self.buttons.clear();
        let row1 = nb.y + nb.h + 8;
        let row2 = row1 + ui::BUTTON_H + 8;
        let groups: [(&[(&str, Btn)], i32, i32); 3] = [
            (&[("New", Btn::New), ("Rename", Btn::Rename), ("Delete", Btn::Delete)], l.x, row1),
            (&[("Up", Btn::CatUp), ("Down", Btn::CatDown)], l.x, row2),
            (&[("Add\u{2026}", Btn::Add), ("Remove", Btn::Remove), ("Move to\u{2026}", Btn::MoveTo), ("Up", Btn::Up), ("Down", Btn::Down)], rl.x, nb.y),
        ];
        for (items, x, y) in groups {
            let labels: Vec<&str> = items.iter().map(|(l, _)| *l).collect();
            for (r, (_, k)) in ui::button_row(&labels, x, y).into_iter().zip(items.iter()) {
                self.buttons.push((r, *k));
            }
        }
        let buttons = self.buttons.clone();
        for (r, k) in &buttons {
            let label = match k {
                Btn::New => "New",
                Btn::Rename => "Rename",
                Btn::Delete => "Delete",
                Btn::CatUp | Btn::Up => "Up",
                Btn::CatDown | Btn::Down => "Down",
                Btn::Add => "Add\u{2026}",
                Btn::Remove => "Remove",
                Btn::MoveTo => "Move to\u{2026}",
            };
            ui::button(c, at(*r), label, self.enabled(*k));
        }
        if !self.status.is_empty() {
            c.text(a.x + rl.x, a.y + nb.y + nb.h + 14, &self.status, 0xF87171, FontId::Regular, 13.0);
        } else {
            c.text(a.x + rl.x, a.y + nb.y + nb.h + 14, "Programs from /apps join the menu by themselves.", TEXT_DIM, FontId::Regular, 12.5);
        }
        if let Some(m) = &self.move_to {
            m.draw(c, a, |_| false);
        }
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        self.mouse = (x, y);
        if let Some(m) = &mut self.move_to {
            let item = m.item_at(x, y);
            if pressed {
                let m = self.move_to.take().unwrap();
                if let Some(i) = item
                    && m.items[i].2
                {
                    self.move_entry_to(m.items[i].1);
                }
                return true;
            }
            let changed = m.hover != item;
            m.hover = item;
            return changed;
        }
        let nb = self.name_box();
        if !pressed || buttons & LEFT == 0 {
            return self.typing && self.name.mouse(nb, x, y, buttons, pressed);
        }
        self.status.clear();
        if nb.contains(x, y) {
            self.typing = true;
            self.name.mouse(nb, x, y, buttons, pressed);
            return true;
        }
        if let Some(&(_, b)) = self.buttons.iter().find(|(r, b)| r.contains(x, y) && self.enabled(*b)) {
            self.press(b);
            return true;
        }
        let (l, rl) = (self.left_list(), self.right_list());
        if l.contains(x, y) {
            let i = ((y - l.y - 4 + self.scroll.0) / CAT_H) as usize;
            if i < self.menu.categories.len() {
                self.select_category(i);
            }
            return true;
        }
        if rl.contains(x, y) {
            let i = ((y - rl.y - 4 + self.scroll.1) / APP_H) as usize;
            self.entry = (i < self.entries_len()).then_some(i);
            self.typing = false;
            return true;
        }
        self.typing = false;
        true
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let (x, y) = self.mouse;
        let (l, rl) = (self.left_list(), self.right_list());
        let before = self.scroll;
        if l.contains(x, y) {
            let max = (self.menu.categories.len() as i32 * CAT_H + 8 - l.h).max(0);
            self.scroll.0 = (self.scroll.0 + lines * CAT_H).clamp(0, max);
        } else if rl.contains(x, y) {
            let max = (self.entries_len() as i32 * APP_H + 8 - rl.h).max(0);
            self.scroll.1 = (self.scroll.1 + lines * APP_H).clamp(0, max);
        }
        self.scroll != before
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        if self.move_to.is_some() {
            if ch == 0x1B {
                self.move_to = None;
                return true;
            }
            return false;
        }
        if self.typing {
            match ch {
                b'\n' => self.rename(),
                0x1B => {
                    self.select_category(self.cat);
                }
                _ => {
                    self.status.clear();
                    return self.name.key(ch, |c| !c.is_control());
                }
            }
            return true;
        }
        match ch {
            KEY_UP | KEY_DOWN => {
                let step = if ch == KEY_UP { -1 } else { 1 };
                match self.entry {
                    Some(e) => self.entry = Some((e as i32 + step).clamp(0, self.entries_len() as i32 - 1) as usize),
                    None => self.select_category((self.cat as i32 + step).max(0) as usize),
                }
                true
            }
            _ => false,
        }
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        self.typing && self.name.type_char(ch, |c| !c.is_control())
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let mut changed = false;
        let on = ctx.caret_on();
        if on != self.caret_on {
            self.caret_on = on;
            changed = self.typing;
        }
        if let Some(reply) = &self.adding
            && let Some(answer) = dialog::take(reply)
        {
            self.adding = None;
            if let Some(path) = answer {
                if lua::is_program(&path) && fs::exists(&path) {
                    let name = match self.menu.categories.get(self.cat) {
                        Some(c) => c.name.clone(),
                        None => String::from(appmenu::OTHER),
                    };
                    self.menu.add(&name, &path);
                    self.changed();
                    if let Some(i) = self.menu.find(&name) {
                        self.cat = i;
                        self.entry = Some(self.entries_len().saturating_sub(1));
                    }
                } else {
                    self.status = String::from("Only Lua programs (*.lua) can go to the menu");
                }
            }
            changed = true;
        }
        changed
    }
}
