//! A simple text editor.

use super::dialog::{self, Mode, Reply};
use super::lua_syntax::{Open, spans};
use super::ui;
use crate::font::{self, FontId};
use crate::fs;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::{KEY_DELETE, KEY_DOWN, KEY_END, KEY_HOME, KEY_LEFT, KEY_RIGHT, KEY_UP, LEFT, SHIFTED_NAV};
use crate::wm::{self, ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

const BG: Color = 0x161925;
const TOOLBAR_H: i32 = 46;
const FONT: f32 = 14.0;
const LINE: i32 = 20;
const GUTTER: i32 = 46;
/// How many undo steps and how much memory they may take.
const UNDO_STEPS: usize = 50;
const UNDO_BYTES: usize = 8 << 20;
/// The scroll bar thickness and the gap to the edge.
const BAR: i32 = 8;
const BAR_GAP: i32 = 3;
const THUMB_MIN: i32 = 24;
/// How many pixels to keep between the cursor and the edge when scrolling horizontally.
const CARET_MARGIN: i32 = 24;

/// Context menu items.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Delete,
    SelectAll,
    Separator,
}

/// A scroll bar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bar {
    Vertical,
    Horizontal,
}

/// What to do once the fate of the unsaved changes is decided.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Then {
    Close,
    OpenFile,
}

pub struct Notepad {
    cw: i32,
    ch: i32,
    path: String,
    /// The cursor and the selection are bytes, always on a character boundary; columns are characters.
    text: String,
    cursor: usize,
    /// The other end of the selection (the first is the cursor); None — nothing is selected.
    anchor: Option<usize>,
    scroll: usize,
    rows: usize,
    modified: bool,
    status: Option<String>,
    cursor_on: bool,
    /// The open "Open" / "Save as" dialog and its answer.
    dialog: Option<(Mode, Reply)>,
    menu: Option<ui::Menu<Command>>,
    /// States for undo: (text, cursor), the last one is the newest.
    undo: Vec<(String, usize)>,
    /// What Redo brings back: undone edits, a new edit forgets them.
    redo: Vec<(String, usize)>,
    /// Typing is in progress: consecutive letters are undone in one step.
    typing: bool,
    /// The unsaved-changes question is on screen.
    prompt: Option<Then>,
    done: bool,
    /// The horizontal text scroll in pixels.
    hscroll: i32,
    /// A bar is being dragged: which one and by which point of the thumb.
    drag: Option<(Bar, i32)>,
}

impl Notepad {
    pub fn new(path: String, cw: i32, ch: i32) -> Self {
        let text = read_text(&path);
        Self {
            cw,
            ch,
            path,
            text,
            cursor: 0,
            anchor: None,
            scroll: 0,
            rows: rows_for(ch),
            modified: false,
            status: None,
            cursor_on: true,
            dialog: None,
            menu: None,
            undo: Vec::new(),
            redo: Vec::new(),
            typing: false,
            prompt: None,
            done: false,
            hscroll: 0,
            drag: None,
        }
    }

    /// The width of the text area.
    fn view_w(&self) -> i32 {
        (self.cw - GUTTER - BAR - 2 * BAR_GAP).max(1)
    }

    /// The longest line in pixels.
    fn content_w(&self) -> i32 {
        let longest = self.text.split('\n').map(|l| l.chars().count()).max().unwrap_or(0) as i32;
        longest * Self::px("MMMMMMMMMM") / 10 + CARET_MARGIN
    }

    fn max_scroll(&self) -> usize {
        self.line_count().saturating_sub(self.rows)
    }

    fn max_hscroll(&self) -> i32 {
        (self.content_w() - self.view_w()).max(0)
    }

    fn track(&self, bar: Bar) -> Rect {
        let top = TOOLBAR_H + 4;
        let height = self.rows as i32 * LINE;
        match bar {
            Bar::Vertical => Rect::new(self.cw - BAR - BAR_GAP, top, BAR, height),
            Bar::Horizontal => Rect::new(GUTTER, top + height + BAR_GAP, self.view_w(), BAR),
        }
    }

    /// (visible part, all, current offset) in this bar's units.
    fn extent(&self, bar: Bar) -> (i32, i32, i32) {
        match bar {
            Bar::Vertical => (self.rows as i32, self.line_count() as i32, self.scroll as i32),
            Bar::Horizontal => (self.view_w(), self.content_w(), self.hscroll),
        }
    }

    /// The thumb: start and length along the bar, None — everything is visible anyway.
    fn thumb(&self, bar: Bar) -> Option<(i32, i32)> {
        let (view, total, offset) = self.extent(bar);
        let t = self.track(bar);
        let length = if bar == Bar::Vertical { t.h } else { t.w };
        if total <= view {
            return None;
        }
        let size = (length as i64 * view as i64 / total as i64).max(THUMB_MIN as i64).min(length as i64) as i32;
        Some((((length - size) as i64 * offset as i64 / (total - view) as i64) as i32, size))
    }

    /// Sets the offset from the mouse position on the bar.
    fn drag_bar(&mut self, bar: Bar, x: i32, y: i32, grab: i32) {
        let Some((_, size)) = self.thumb(bar) else { return };
        let (view, total, _) = self.extent(bar);
        let t = self.track(bar);
        let (mouse, length) = if bar == Bar::Vertical { (y - t.y, t.h) } else { (x - t.x, t.w) };
        let room = (length - size).max(1) as i64;
        let value = ((mouse - grab).clamp(0, room as i32) as i64 * (total - view) as i64 / room) as i32;
        match bar {
            Bar::Vertical => self.scroll = value as usize,
            Bar::Horizontal => self.hscroll = value,
        }
    }

    /// Remembers the state before an edit; `typing` — an ordinary letter.
    fn remember(&mut self, typing: bool) {
        self.redo.clear();
        let chained = typing && self.typing && self.selection().is_none();
        self.typing = typing;
        if chained {
            return;
        }
        self.undo.push((self.text.clone(), self.cursor));
        while self.undo.len() > UNDO_STEPS || (self.undo.len() > 1 && self.undo.iter().map(|u| u.0.len()).sum::<usize>() > UNDO_BYTES) {
            self.undo.remove(0);
        }
    }

    fn undo(&mut self) {
        let Some((text, cursor)) = self.undo.pop() else { return };
        self.redo.push((core::mem::take(&mut self.text), self.cursor));
        (self.text, self.cursor, self.anchor, self.typing, self.modified) = (text, cursor, None, false, true);
        self.ensure_visible();
    }

    fn redo(&mut self) {
        let Some((text, cursor)) = self.redo.pop() else { return };
        self.undo.push((core::mem::take(&mut self.text), self.cursor));
        (self.text, self.cursor, self.anchor, self.typing, self.modified) = (text, cursor, None, false, true);
        self.ensure_visible();
    }

    fn paste(&mut self) {
        self.remember(false);
        self.delete_selection();
        let text: String = wm::clipboard().chars().filter(|&c| c == '\n' || !c.is_control()).collect();
        self.insert(&text);
    }

    fn run(&mut self, command: Command) {
        let selection = self.selection();
        match command {
            Command::Undo => self.undo(),
            Command::Redo => self.redo(),
            Command::Copy | Command::Cut => {
                if let Some((from, to)) = selection {
                    wm::set_clipboard(&self.text[from..to]);
                }
                if command == Command::Cut {
                    self.remember(false);
                    self.delete_selection();
                }
            }
            Command::Paste => self.paste(),
            Command::Delete => {
                self.remember(false);
                self.delete_selection();
            }
            Command::SelectAll => {
                self.anchor = Some(0);
                self.cursor = self.text.len();
            }
            Command::Separator => {}
        }
        self.status = None;
        self.ensure_visible();
    }

    /// The choice in the unsaved-changes question: save or not. Cancel is `self.prompt = None`.
    fn finish_prompt(&mut self, save: bool) {
        let Some(then) = self.prompt.take() else { return };
        if save {
            self.save();
            if self.modified {
                return; // the write failed: the reason is already in the status line, the window stays
            }
        }
        match then {
            Then::Close => self.done = true,
            Then::OpenFile => {
                self.modified = false;
                self.ask(Mode::Open);
            }
        }
    }

    fn open_button(&self) -> Rect {
        let sa = self.save_as_button();
        let w = ui::button_width("Open\u{2026}");
        Rect::new(sa.x - 6 - w, 9, w, ui::BUTTON_H)
    }

    fn save_as_button(&self) -> Rect {
        let w = ui::button_width("Save as\u{2026}");
        Rect::new(self.save_button().x - 6 - w, 9, w, ui::BUTTON_H)
    }

    fn ask(&mut self, mode: Mode) {
        if mode == Mode::Open && self.modified {
            self.prompt = Some(Then::OpenFile);
            return;
        }
        if self.dialog.is_none() {
            let dir = fs::parent(&self.path);
            self.dialog = Some((mode, dialog::request(mode, &dir, fs::name(&self.path))));
        }
    }

    /// Loads another file into the same editor.
    fn load(&mut self, path: String) {
        self.text = read_text(&path);
        self.path = path;
        (self.cursor, self.anchor, self.scroll, self.modified) = (0, None, 0, false);
        self.undo.clear();
        self.redo.clear();
        self.typing = false;
        self.status = Some("Opened".into());
    }

    fn save_button(&self) -> Rect {
        Rect::new(self.cw - 10 - ui::button_width("Save"), 9, ui::button_width("Save"), ui::BUTTON_H)
    }

    fn line_col(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let line = before.matches('\n').count();
        let col = before[before.rfind('\n').map_or(0, |i| i + 1)..].chars().count();
        (line, col)
    }

    fn line_count(&self) -> usize {
        self.text.matches('\n').count() + 1
    }

    fn line_start(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        self.text.match_indices('\n').nth(line - 1).map_or(self.text.len(), |(i, _)| i + 1)
    }

    fn line_len(&self, line: usize) -> usize {
        let start = self.line_start(line);
        self.text[start..].find('\n').unwrap_or(self.text.len() - start)
    }

    /// The byte where character number `col` of the line starts (or the end of the line).
    fn col_byte(&self, line: usize, col: usize) -> usize {
        let start = self.line_start(line);
        let text = &self.text[start..start + self.line_len(line)];
        start + text.char_indices().nth(col).map_or(text.len(), |(i, _)| i)
    }

    fn move_to(&mut self, line: usize, col: usize) {
        let line = line.min(self.line_count() - 1);
        self.cursor = self.col_byte(line, col);
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().next_back().map_or(0, |(k, _)| k)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map_or(i, |c| i + c.len_utf8())
    }

    fn ensure_visible(&mut self) {
        let (line, col) = self.line_col();
        if line < self.scroll {
            self.scroll = line;
        } else if line >= self.scroll + self.rows {
            self.scroll = line + 1 - self.rows;
        }
        let start = self.line_start(line);
        let caret = Self::px(&self.text[start..start + self.text[start..].char_indices().nth(col).map_or(self.line_len(line), |(i, _)| i)]);
        if caret - self.hscroll > self.view_w() - CARET_MARGIN {
            self.hscroll = caret + CARET_MARGIN - self.view_w();
        } else if caret - CARET_MARGIN < self.hscroll {
            self.hscroll = (caret - CARET_MARGIN).max(0);
        }
    }

    fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.modified = true;
    }

    /// The selected byte range (an empty selection does not count).
    fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.cursor).then(|| (a.min(self.cursor), a.max(self.cursor)))
    }

    /// Deletes the selection; true — there was something to delete.
    fn delete_selection(&mut self) -> bool {
        let Some((from, to)) = self.selection() else { return false };
        self.text.replace_range(from..to, "");
        self.cursor = from;
        self.anchor = None;
        self.modified = true;
        true
    }

    /// The position in the text under a point in client coordinates.
    fn pos_at(&self, x: i32, y: i32) -> usize {
        let line = self.scroll + ((y - TOOLBAR_H - 4).max(0) / LINE) as usize;
        let line = line.min(self.line_count() - 1);
        let start = self.line_start(line);
        let text = &self.text[start..start + self.line_len(line)];
        let ends = text.char_indices().map(|(i, _)| i).chain(core::iter::once(text.len()));
        start + ends.min_by_key(|&i| (Self::px(&text[..i]) - (x - GUTTER + self.hscroll)).abs()).unwrap_or(0)
    }

    fn save(&mut self) {
        self.status = match fs::write(&self.path, self.text.as_bytes()) {
            Ok(()) => {
                self.modified = false;
                Some(String::from("Saved"))
            }
            Err(e) => Some(format!("Error: {e}")),
        };
    }

    /// The text width. A character is wider than a whole number of pixels, so multiplying
    /// a rounded width by the column number is not allowed: the error would pile up towards the end of the line.
    fn px(text: &str) -> i32 {
        font::measure(FontId::Mono, FONT, text)
    }
}

/// How many lines fit: room for the horizontal bar at the bottom.
fn rows_for(ch: i32) -> usize {
    ((ch - TOOLBAR_H - 8 - BAR - BAR_GAP) / LINE).max(1) as usize
}

/// The file text. Bytes that are not UTF-8 are replaced with U+FFFD.
fn read_text(path: &str) -> String {
    fs::read(path).map(|data| String::from_utf8_lossy(&data).into_owned()).unwrap_or_default()
}

impl App for Notepad {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        let mut name = String::from(&self.path);
        if self.modified {
            name.push_str("  \u{2022}");
        }
        c.text(a.x + 14, a.y + 14, &name, TEXT_DIM, FontId::Regular, 13.0);
        if let Some(status) = &self.status {
            let sb = self.open_button();
            let w = font::measure(FontId::Regular, 12.0, status);
            c.text(a.x + sb.x - 12 - w, a.y + 15, status, ACCENT, FontId::Regular, 12.0);
        }
        ui::button(c, self.open_button().offset(a.x, a.y), "Open\u{2026}", self.dialog.is_none());
        ui::button(c, self.save_as_button().offset(a.x, a.y), "Save as\u{2026}", self.dialog.is_none());
        ui::button(c, self.save_button().offset(a.x, a.y), "Save", self.modified);
        c.fill(Rect::new(a.x, a.y + TOOLBAR_H - 1, a.w, 1), 0x2E3247);

        let (cur_line, cur_col) = self.line_col();
        let hscroll = self.hscroll.min(self.max_hscroll());
        self.hscroll = hscroll;
        let x = a.x + GUTTER - hscroll;
        let selection = self.selection();
        let mut start = self.line_start(self.scroll);
        let saved = c.clip();
        let view = Rect::new(a.x + GUTTER - 2, a.y + TOOLBAR_H + 4, self.view_w() + 2, self.rows as i32 * LINE).intersect(saved);
        // Lua highlighting: long strings and comments may have started above the screen.
        let lua = self.path.to_ascii_lowercase().ends_with(".lua");
        let mut open = Open::Nothing;
        if lua {
            for line in self.text.split('\n').take(self.scroll) {
                spans(line, &mut open);
            }
        }
        for (row, (n, line)) in self.text.split('\n').enumerate().skip(self.scroll).take(self.rows).enumerate() {
            let y = a.y + TOOLBAR_H + 4 + row as i32 * LINE;
            if n == cur_line {
                c.blend(Rect::new(a.x, y, a.w, LINE), 0xFFFFFF, 10);
            }
            let num = format!("{}", n + 1);
            let nw = font::measure(FontId::Mono, 12.0, &num);
            c.text(a.x + GUTTER - 12 - nw, y + 2, &num, 0x4B5068, FontId::Mono, 12.0);
            c.set_clip(view);
            // The selection on this line (a newline in the selection is another half character).
            if let Some((from, to)) = selection {
                let end = start + line.len();
                if from <= end && to > start {
                    let x0 = Self::px(&line[..from.max(start) - start]);
                    let x1 = Self::px(&line[..to.min(end) - start]);
                    let w = x1 - x0 + if to > end { Self::px(" ") / 2 } else { 0 };
                    c.fill_round(Rect::new(x + x0, y + 1, w, LINE - 2), 3, ACCENT, 80);
                }
            }
            start += line.len() + 1;
            if lua {
                for (from, to, color) in spans(line, &mut open) {
                    c.text(x + Self::px(&line[..from]), y, &line[from..to], color, FontId::Mono, FONT);
                }
            } else {
                c.text(x, y, line, TEXT, FontId::Mono, FONT);
            }
            if n == cur_line && self.cursor_on {
                let head: String = line.chars().take(cur_col).collect();
                c.fill(Rect::new(x + Self::px(&head), y + 2, 2, LINE - 4), ACCENT);
            }
            c.set_clip(saved);
        }
        for bar in [Bar::Vertical, Bar::Horizontal] {
            let Some((at, size)) = self.thumb(bar) else { continue };
            let t = self.track(bar).offset(a.x, a.y);
            let thumb = if bar == Bar::Vertical { Rect::new(t.x, t.y + at, t.w, size) } else { Rect::new(t.x + at, t.y, size, t.h) };
            let active = self.drag.is_some_and(|(b, _)| b == bar);
            c.fill_round(t, BAR / 2, 0xFFFFFF, 14);
            c.fill_round(thumb, BAR / 2, 0xFFFFFF, if active { 200 } else { 110 });
        }
        if let Some(m) = &self.menu {
            m.draw(c, a, |command| *command == Command::Delete);
        }
        if self.prompt.is_some() {
            ui::draw_save_prompt(c, a, (self.cw, self.ch), fs::name(&self.path));
        }
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        if self.prompt.is_some() {
            match ch {
                b'\n' => self.finish_prompt(true),
                0x1B => self.prompt = None,
                _ => return false,
            }
            return true;
        }
        if self.menu.take().is_some() {
            return true;
        }
        // Shift+navigation extends the selection, plain navigation clears it.
        let nav = ch & !SHIFTED_NAV;
        let is_nav = (KEY_UP..=KEY_END).contains(&nav);
        if is_nav {
            if ch != nav {
                self.anchor.get_or_insert(self.cursor);
            } else {
                self.anchor = None;
            }
        }
        let ch = if is_nav { nav } else { ch };
        // The anchor from a click without a selection must not "stretch" while typing.
        if !is_nav && !matches!(ch, 0x01 | 0x03 | 0x0F | 0x13 | 0x18 | 0x1A) && self.selection().is_none() {
            self.anchor = None;
        }
        // Before an edit, remember the state for undo.
        match ch {
            0x20..=0x7E => self.remember(true),
            b'\n' | b'\t' | 0x08 | 0x18 | KEY_DELETE => self.remember(false),
            _ => self.typing = false,
        }
        let (line, col) = self.line_col();
        match ch {
            0x1A if crate::input::mods() & crate::input::MOD_SHIFT != 0 => self.redo(), // Ctrl+Shift+Z
            0x1A => self.undo(), // Ctrl+Z
            0x19 => self.redo(), // Ctrl+Y
            0x13 => {
                // Ctrl+S
                self.save();
                return true;
            }
            0x0F => {
                // Ctrl+O
                self.ask(Mode::Open);
                return true;
            }
            0x01 => {
                // Ctrl+A
                self.anchor = Some(0);
                self.cursor = self.text.len();
            }
            0x03 | 0x18 => {
                // Ctrl+C / Ctrl+X: the selection, and without one — the current line
                let (from, to) = self.selection().unwrap_or_else(|| {
                    let start = self.line_start(line);
                    (start, (start + self.line_len(line) + 1).min(self.text.len()))
                });
                wm::set_clipboard(&self.text[from..to]);
                if ch == 0x18 {
                    self.anchor = Some(from);
                    self.cursor = to;
                    self.delete_selection();
                }
            }
            0x16 => {
                // Ctrl+V
                self.paste();
            }
            0x20..=0x7E => {
                self.delete_selection();
                self.insert(&(ch as char).to_string());
            }
            b'\n' => {
                self.delete_selection();
                self.insert("\n");
            }
            b'\t' => {
                self.delete_selection();
                self.insert("    ");
            }
            0x08 | KEY_DELETE if self.delete_selection() => {}
            0x08 if self.cursor > 0 => {
                self.cursor = self.prev(self.cursor);
                self.text.remove(self.cursor);
                self.modified = true;
            }
            KEY_DELETE if self.cursor < self.text.len() => {
                self.text.remove(self.cursor);
                self.modified = true;
            }
            KEY_LEFT => self.cursor = self.prev(self.cursor),
            KEY_RIGHT => self.cursor = self.next(self.cursor),
            KEY_UP if line > 0 => self.move_to(line - 1, col),
            KEY_DOWN => self.move_to(line + 1, col),
            KEY_HOME => self.move_to(line, 0),
            KEY_END => self.move_to(line, usize::MAX),
            _ => return false,
        }
        self.status = None;
        self.cursor_on = true;
        self.ensure_visible();
        true
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        if self.prompt.is_some() {
            return false;
        }
        self.menu = None;
        self.remember(true);
        if self.selection().is_none() {
            self.anchor = None;
        }
        self.delete_selection();
        self.insert(ch.encode_utf8(&mut [0; 4]));
        self.status = None;
        self.cursor_on = true;
        self.ensure_visible();
        true
    }

    fn hover(&mut self, x: i32, y: i32) -> bool {
        let Some(m) = &mut self.menu else { return false };
        let now = m.item_at(x, y);
        let changed = now != m.hover;
        m.hover = now;
        changed
    }

    fn context_click(&mut self, x: i32, y: i32) -> bool {
        if self.prompt.is_some() || y < TOOLBAR_H {
            return false;
        }
        // A click outside the selection puts the cursor there, on the selection — keeps it.
        let pos = self.pos_at(x, y);
        if !self.selection().is_some_and(|(from, to)| (from..to).contains(&pos)) {
            self.cursor = pos;
            self.anchor = None;
        }
        let selected = self.selection().is_some();
        let items = alloc::vec![
            ("Undo", Command::Undo, !self.undo.is_empty()),
            ("Redo", Command::Redo, !self.redo.is_empty()),
            ("", Command::Separator, true),
            ("Cut", Command::Cut, selected),
            ("Copy", Command::Copy, selected),
            ("Paste", Command::Paste, !wm::clipboard().is_empty()),
            ("Delete", Command::Delete, selected),
            ("", Command::Separator, true),
            ("Select All", Command::SelectAll, !self.text.is_empty()),
        ];
        self.menu = Some(ui::Menu::new(x, y, items, self.cw, self.ch));
        true
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if self.prompt.is_some() {
            if pressed {
                let [save, skip, cancel] = ui::save_prompt_buttons((self.cw, self.ch));
                if save.contains(x, y) {
                    self.finish_prompt(true);
                } else if skip.contains(x, y) {
                    self.finish_prompt(false);
                } else if cancel.contains(x, y) {
                    self.prompt = None;
                }
            }
            return true;
        }
        if pressed && let Some(m) = self.menu.take() {
            if let Some(i) = m.item_at(x, y)
                && m.items[i].2
            {
                self.run(m.items[i].1);
            }
            return true;
        }
        if let Some((bar, grab)) = self.drag {
            if buttons & LEFT == 0 {
                self.drag = None;
            } else {
                self.drag_bar(bar, x, y, grab);
            }
            return true;
        }
        if pressed {
            for bar in [Bar::Vertical, Bar::Horizontal] {
                let t = self.track(bar);
                let Some((at, size)) = self.thumb(bar).filter(|_| t.inset(-2).contains(x, y)) else { continue };
                let mouse = if bar == Bar::Vertical { y - t.y } else { x - t.x };
                // The thumb is dragged from the grab point, a click elsewhere puts it under the cursor.
                let grab = if (at..at + size).contains(&mouse) { mouse - at } else { size / 2 };
                self.drag = Some((bar, grab));
                self.drag_bar(bar, x, y, grab);
                return true;
            }
        }
        if pressed && self.save_button().contains(x, y) {
            self.save();
            return true;
        }
        if pressed && self.open_button().contains(x, y) {
            self.ask(Mode::Open);
            return true;
        }
        if pressed && self.save_as_button().contains(x, y) {
            self.ask(Mode::Save);
            return true;
        }
        if pressed && y >= TOOLBAR_H {
            // A press puts the cursor and starts a mouse selection.
            self.cursor = self.pos_at(x, y);
            self.anchor = Some(self.cursor);
            return true;
        }
        if !pressed && buttons & LEFT != 0 && self.anchor.is_some() {
            let pos = self.pos_at(x, y);
            let changed = pos != self.cursor;
            self.cursor = pos;
            return changed;
        }
        false
    }

    fn location(&self) -> Option<String> {
        Some(self.path.clone())
    }

    fn scroll(&mut self, lines: i32) -> bool {
        // Shift with the wheel scrolls sideways.
        if crate::input::mods() & crate::input::MOD_SHIFT != 0 {
            let before = self.hscroll;
            self.hscroll = (self.hscroll + lines * Self::px("MMMM")).clamp(0, self.max_hscroll());
            return self.hscroll != before;
        }
        let before = self.scroll;
        self.scroll = (self.scroll as i32 + lines).clamp(0, self.max_scroll() as i32) as usize;
        self.scroll != before
    }

    fn resize(&mut self, cw: i32, ch: i32) {
        (self.cw, self.ch) = (cw, ch);
        self.rows = rows_for(ch);
        self.ensure_visible();
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let on = ctx.caret_on();
        let mut changed = on != self.cursor_on;
        self.cursor_on = on;
        if let Some((mode, reply)) = &self.dialog
            && let Some(answer) = dialog::take(reply)
        {
            let mode = *mode;
            self.dialog = None;
            changed = true;
            match (mode, answer) {
                (Mode::Open | Mode::OpenMany, Some(path)) => self.load(path),
                (Mode::Save, Some(path)) => {
                    self.path = path;
                    self.save();
                }
                (_, None) => {}
            }
        }
        changed
    }

    fn title(&self) -> Option<String> {
        Some(format!("{} \u{2014} Notepad", fs::name(&self.path)))
    }

    fn finished(&self) -> bool {
        self.done
    }

    fn unsaved(&self) -> bool {
        self.modified
    }

    fn ask_close(&mut self) -> bool {
        if self.modified {
            self.prompt = Some(Then::Close);
            self.menu = None;
        }
        !self.modified
    }

    fn asking(&self) -> bool {
        self.prompt.is_some()
    }
}
