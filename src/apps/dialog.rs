//! The "Open" and "Save as" dialogs: a separate window with a folder list and a name
//! field. The program that asked for the dialog gets the answer through a shared slot
//! (`Reply`) and polls it in `tick` — this way the dialog blocks neither it nor the WM.

use super::ui;
use crate::font::{self, FontId};
use crate::fs;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::{KEY_DOWN, KEY_UP, LEFT};
use crate::interrupts::{self, HZ};
use crate::sync::IrqLock;
use crate::wm::{self, ACCENT, App, Ctx, Request, TEXT, TEXT_DIM};
use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

const BG: Color = 0x181B28;
const PAD: i32 = 12;
const ROW_H: i32 = 28;
const LIST_Y: i32 = 50;
const FOOTER_H: i32 = 56;
const ESC: u8 = 0x1B;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Open,
    /// "Open" with a choice of several files and folders (Ctrl, Shift, Ctrl+A; a folder —
    /// a click and Open, a double click enters it). The paths in the answer are separated by newlines.
    OpenMany,
    Save,
}

/// The dialog answer: None — no answer yet; Some(None) — cancelled; Some(Some(path)) — chosen.
pub type Reply = Arc<IrqLock<Option<Option<String>>>>;

/// Asks the WM to open a dialog. `dir` is where to start (empty — the home folder),
/// `name` is the suggested file name (for saving).
pub fn request(mode: Mode, dir: &str, name: &str) -> Reply {
    let reply: Reply = Arc::new(IrqLock::new(None));
    let dir = if dir.starts_with('/') && fs::is_dir(dir) { dir.to_string() } else { crate::access::home() };
    wm::request(Request::Dialog(FileDialog::new(mode, dir, name.into(), reply.clone())));
    reply
}

/// Takes the answer if it is already there: Some(None) — cancelled.
pub fn take(reply: &Reply) -> Option<Option<String>> {
    reply.lock().take()
}

pub struct FileDialog {
    mode: Mode,
    dir: String,
    entries: Vec<fs::Entry>,
    selected: Option<usize>,
    /// The marked files (in OpenMany mode).
    marked: BTreeSet<usize>,
    scroll: usize,
    name: ui::TextField,
    reply: Reply,
    done: bool,
    /// The time and row of the previous click — for a double click.
    last_click: Option<(u64, usize)>,
    cursor_on: bool,
    cw: i32,
    ch: i32,
    error: Option<&'static str>,
}

impl FileDialog {
    fn new(mode: Mode, dir: String, name: String, reply: Reply) -> Self {
        let mut d = Self {
            mode,
            dir: String::new(),
            entries: Vec::new(),
            selected: None,
            marked: BTreeSet::new(),
            scroll: 0,
            name: ui::TextField::new(&name, 60),
            reply,
            done: false,
            last_click: None,
            cursor_on: true,
            cw: 0,
            ch: 0,
            error: None,
        };
        d.enter(dir);
        d
    }

    pub fn title(&self) -> &'static str {
        match self.mode {
            Mode::Open | Mode::OpenMany => "Open",
            Mode::Save => "Save As",
        }
    }

    /// Goes to a folder: subfolders first, then files, alphabetically.
    fn enter(&mut self, dir: String) {
        let mut entries = fs::list(&dir).unwrap_or_default();
        entries.retain(|e| !e.name.starts_with('.'));
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        self.entries = entries;
        self.dir = dir;
        // A file name from the previous folder does not fit here when opening; when saving it is kept.
        if self.mode != Mode::Save && !self.dir.is_empty() && !self.entries.iter().any(|e| e.name == self.name.text) {
            self.name.set_text("");
        }
        self.selected = None;
        self.marked.clear();
        self.scroll = 0;
        self.error = None;
    }

    fn up(&mut self) {
        if self.dir != "/" {
            let parent = fs::resolve(&self.dir, "..");
            self.enter(parent);
        }
    }

    fn up_button() -> Rect {
        Rect::new(PAD, 11, ui::button_width("Up"), ui::BUTTON_H)
    }

    fn rows(&self) -> usize {
        ((self.ch - LIST_Y - FOOTER_H) / ROW_H).max(1) as usize
    }

    fn row(&self, n: usize) -> Rect {
        Rect::new(PAD, LIST_Y + n as i32 * ROW_H, self.cw - 2 * PAD, ROW_H - 2)
    }

    fn name_box(&self) -> Rect {
        let buttons = ui::button_width("Cancel") + ui::button_width(self.ok_label()) + 18;
        Rect::new(PAD, self.ch - FOOTER_H + 14, self.cw - 2 * PAD - buttons, ui::BUTTON_H)
    }

    fn ok_label(&self) -> &'static str {
        match self.mode {
            Mode::Open | Mode::OpenMany => "Open",
            Mode::Save => "Save",
        }
    }

    fn ok_button(&self) -> Rect {
        let w = ui::button_width(self.ok_label());
        Rect::new(self.cw - PAD - w, self.ch - FOOTER_H + 14, w, ui::BUTTON_H)
    }

    fn cancel_button(&self) -> Rect {
        let w = ui::button_width("Cancel");
        Rect::new(self.ok_button().x - 8 - w, self.ch - FOOTER_H + 14, w, ui::BUTTON_H)
    }

    fn finish(&mut self, result: Option<String>) {
        *self.reply.lock() = Some(result);
        self.done = true;
    }

    /// Open/Save or Enter: a folder is opened, a file is handed to the program.
    fn confirm(&mut self) {
        // Several files are marked — hand them all over.
        // Several files or a folder (in OpenMany a folder can be handed over whole).
        let folder = self.marked.iter().any(|&i| self.entries[i].is_dir);
        if (self.marked.len() > 1 || (folder && self.mode == Mode::OpenMany)) && self.name.text == self.marked_text() {
            let paths: Vec<String> = self.marked.iter().map(|&i| fs::resolve(&self.dir, &self.entries[i].name)).collect();
            self.finish(Some(paths.join("\n")));
            return;
        }
        let name = self.name.text.trim().to_string();
        if name.is_empty() {
            return;
        }
        let path = fs::resolve(&self.dir, &name);
        if fs::is_dir(&path) {
            self.name.set_text("");
            self.enter(path);
        } else if self.mode != Mode::Save && !fs::exists(&path) {
            self.error = Some("No such file");
        } else {
            self.finish(Some(path));
        }
    }

    fn select(&mut self, i: usize) {
        self.selected = Some(i);
        self.marked.clear();
        let e = &self.entries[i];
        if !e.is_dir || self.mode == Mode::OpenMany {
            self.name.set_text(&e.name.clone());
            self.marked.insert(i);
        }
        self.scroll_to(i);
    }

    fn scroll_to(&mut self, i: usize) {
        let rows = self.rows();
        if i < self.scroll {
            self.scroll = i;
        } else if i >= self.scroll + rows {
            self.scroll = i + 1 - rows;
        }
    }

    /// Ctrl+click — mark or unmark; Shift+click — all files from the selected one to this one.
    fn mark(&mut self, i: usize, range: bool) {
        if range {
            let from = self.selected.unwrap_or(i);
            for j in from.min(i)..=from.max(i) {
                self.marked.insert(j);
            }
        } else if !self.marked.remove(&i) {
            self.marked.insert(i);
        }
        self.selected = Some(i);
        self.show_marked();
        self.scroll_to(i);
    }

    fn mark_all(&mut self) {
        self.marked = (0..self.entries.len()).filter(|&i| !self.entries[i].is_dir).collect();
        self.show_marked();
    }

    /// The marked files for the name field: "a.mp3" or "a.mp3" "b.mp3".
    fn marked_text(&self) -> String {
        match self.marked.len() {
            0 => String::new(),
            1 => self.entries[*self.marked.iter().next().unwrap()].name.clone(),
            _ => self.marked.iter().map(|&i| alloc::format!("\"{}\"", self.entries[i].name)).collect::<Vec<_>>().join(" "),
        }
    }

    fn show_marked(&mut self) {
        let text = self.marked_text();
        self.name.set_text(&text);
    }

    /// A double click or Enter on a list row.
    fn activate(&mut self, i: usize) {
        let e = &self.entries[i];
        if e.is_dir {
            let path = fs::resolve(&self.dir, &e.name);
            self.enter(path);
        } else {
            self.name.set_text(&e.name.clone());
            self.confirm();
        }
    }
}

impl Drop for FileDialog {
    fn drop(&mut self) {
        // The window was closed with the cross — for the program that is a cancel.
        if !self.done {
            *self.reply.lock() = Some(None);
        }
    }
}

impl App for FileDialog {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        ui::button(c, Self::up_button().offset(a.x, a.y), "Up", self.dir != "/");
        let px = PAD + Self::up_button().w + 12;
        c.text(a.x + px, a.y + 16, &self.dir, TEXT, FontId::SemiBold, 14.0);

        let list = Rect::new(a.x + PAD, a.y + LIST_Y - 4, a.w - 2 * PAD, a.h - LIST_Y - FOOTER_H + 6);
        c.fill_round(list, 8, 0x000000, 50);
        if self.entries.is_empty() {
            c.text(a.x + PAD + 14, a.y + LIST_Y + 6, "(empty folder)", TEXT_DIM, FontId::Regular, 13.0);
        }
        let saved = c.clip();
        c.set_clip(list.intersect(saved));
        for (n, e) in self.entries.iter().enumerate().skip(self.scroll).take(self.rows()) {
            let r = self.row(n - self.scroll).offset(a.x, a.y);
            if self.selected == Some(n) || self.marked.contains(&n) {
                c.fill_round(r, 6, ACCENT, 70);
            }
            let icon = ui::icon_for(&e.name, e.is_dir);
            ui::icon(c, r.x + 8, r.y + 4, 18, icon);
            c.text(r.x + 34, r.y + 5, &e.name, TEXT, FontId::Regular, 13.5);
            if !e.is_dir {
                let size = alloc::format!("{} B", e.size);
                let w = font::measure(FontId::Regular, 12.0, &size);
                c.text(r.x + r.w - 10 - w, r.y + 6, &size, TEXT_DIM, FontId::Regular, 12.0);
            }
        }
        c.set_clip(saved);

        // The name field: it is typed in (or the selected file lands there).
        let nb = self.name_box().offset(a.x, a.y);
        c.fill_round(nb, 7, 0x000000, 90);
        c.stroke_round(nb, 7, ACCENT, 120);
        match self.error {
            Some(e) => {
                c.set_clip(nb.inset(2).intersect(saved));
                c.text(nb.x + 10, nb.y + 5, e, 0xF87171, FontId::Regular, 13.5);
                c.set_clip(saved);
            }
            None => self.name.draw(c, nb, true, self.cursor_on, "File name", TEXT),
        }
        ui::button(c, self.cancel_button().offset(a.x, a.y), "Cancel", true);
        ui::button(c, self.ok_button().offset(a.x, a.y), self.ok_label(), !self.name.text.trim().is_empty());
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        self.error = None;
        self.cursor_on = true;
        match ch {
            b'\n' => match self.selected {
                Some(i) if self.entries[i].is_dir || self.name.text.is_empty() => self.activate(i),
                _ => self.confirm(),
            },
            ESC => self.finish(None),
            // Ctrl+A — all files of the folder.
            0x01 if self.mode == Mode::OpenMany => self.mark_all(),
            KEY_UP => {
                let i = self.selected.map_or(0, |i| i.saturating_sub(1));
                if i < self.entries.len() {
                    self.select(i);
                }
            }
            KEY_DOWN => {
                let i = self.selected.map_or(0, |i| i + 1);
                if i < self.entries.len() {
                    self.select(i);
                }
            }
            _ => return self.name.key(ch, |c| !c.is_control()),
        }
        true
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        self.error = None;
        self.name.type_char(ch, |c| !c.is_control())
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        let nb = self.name_box();
        if !pressed || buttons & LEFT == 0 {
            return self.name.mouse(nb, x, y, buttons, pressed);
        }
        self.error = None;
        if nb.contains(x, y) {
            self.name.mouse(nb, x, y, buttons, pressed);
        } else if Self::up_button().contains(x, y) {
            self.up();
        } else if self.ok_button().contains(x, y) {
            self.confirm();
        } else if self.cancel_button().contains(x, y) {
            self.finish(None);
        } else if let Some(n) = (0..self.rows()).find(|&n| self.row(n).contains(x, y)) {
            let i = self.scroll + n;
            if i < self.entries.len() {
                let now = interrupts::ticks();
                let double = self.last_click.is_some_and(|(t, j)| j == i && now - t < HZ * 2 / 5);
                self.last_click = Some((now, i));
                let mods = crate::input::mods();
                let (ctrl, shift) = (mods & crate::input::MOD_CTRL != 0, mods & crate::input::MOD_SHIFT != 0);
                if self.mode == Mode::OpenMany && (ctrl || shift) {
                    self.last_click = None;
                    self.mark(i, shift);
                } else if double {
                    self.last_click = None;
                    self.activate(i);
                } else {
                    self.select(i);
                }
            }
        }
        true
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let max = self.entries.len().saturating_sub(self.rows()) as i32;
        let before = self.scroll;
        self.scroll = (self.scroll as i32 + lines).clamp(0, max) as usize;
        self.scroll != before
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let on = ctx.caret_on();
        let changed = on != self.cursor_on;
        self.cursor_on = on;
        changed
    }

    fn finished(&self) -> bool {
        self.done
    }
}
