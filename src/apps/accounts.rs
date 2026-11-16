//! The windows of the Users section in Settings: a new user, changing a password, deletion.

use super::ui;
use crate::access;
use crate::font::FontId;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::sync::IrqLock;
use crate::task;
use crate::users;
use crate::wm::{ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const BG: Color = 0x1C1F2E;
const PAD: i32 = 20;
const FIELD_X: i32 = 150;
pub const W: i32 = 440;

#[derive(Clone)]
pub enum Action {
    Add,
    /// A new password; one's own is changed knowing the old one.
    Password { name: String, check_old: bool },
    Remove(String),
}

type Job = Arc<IrqLock<Option<Result<(), &'static str>>>>;

pub struct AccountDialog {
    action: Action,
    cw: i32,
    ch: i32,
    labels: Vec<&'static str>,
    fields: Vec<ui::TextField>,
    focus: usize,
    admin: bool,
    error: Option<&'static str>,
    job: Option<Job>,
    caret: bool,
    done: bool,
}

impl AccountDialog {
    pub fn new(action: Action) -> Self {
        let labels = match &action {
            Action::Add => alloc::vec!["Name", "Password", "Repeat"],
            Action::Password { check_old: true, .. } => alloc::vec!["Current password", "New password", "Repeat"],
            Action::Password { .. } => alloc::vec!["New password", "Repeat"],
            Action::Remove(_) => Vec::new(),
        };
        let fields = labels
            .iter()
            .map(|l| if *l == "Name" { ui::TextField::new("", users::MAX_NAME) } else { ui::TextField::new("", 64).secret() })
            .collect();
        Self { action, cw: W, ch: 0, labels, fields, focus: 0, admin: false, error: None, job: None, caret: true, done: false }
    }

    /// The window height for the contents.
    pub fn height(&self) -> i32 {
        70 + self.fields.len() as i32 * 44 + if matches!(self.action, Action::Add) { 36 } else { 0 } + 90
    }

    pub fn title(&self) -> String {
        match &self.action {
            Action::Add => "Add User".into(),
            Action::Password { name, .. } => format!("Password for {name}"),
            Action::Remove(name) => format!("Remove {name}"),
        }
    }

    fn field(&self, i: usize) -> Rect {
        Rect::new(FIELD_X, 20 + i as i32 * 44, self.cw - FIELD_X - PAD, 30)
    }

    fn admin_row(&self) -> Option<Rect> {
        matches!(self.action, Action::Add).then(|| Rect::new(PAD, 20 + self.fields.len() as i32 * 44, self.cw - 2 * PAD, 26))
    }

    fn ok_label(&self) -> &'static str {
        match self.action {
            Action::Add => "Add",
            Action::Password { .. } => "Change",
            Action::Remove(_) => "Remove",
        }
    }

    fn ok_button(&self) -> Rect {
        let w = ui::button_width(self.ok_label());
        Rect::new(self.cw - PAD - w, self.ch - PAD - ui::BUTTON_H, w, ui::BUTTON_H)
    }

    fn cancel_button(&self) -> Rect {
        let w = ui::button_width("Cancel");
        let ok = self.ok_button();
        Rect::new(ok.x - 8 - w, ok.y, w, ui::BUTTON_H)
    }

    fn run(&mut self, work: impl FnOnce() -> Result<(), &'static str> + Send + 'static) {
        let job: Job = Arc::new(IrqLock::new(None));
        let slot = job.clone();
        task::spawn("users", None, move || {
            let r = work();
            *slot.lock() = Some(r);
        });
        self.job = Some(job);
        self.error = None;
    }

    fn submit(&mut self) {
        if self.job.is_some() {
            return;
        }
        let text = |i: usize| self.fields[i].text.clone();
        match self.action.clone() {
            Action::Add => {
                let name = text(0).trim().to_ascii_lowercase();
                let password = text(1);
                let admin = self.admin;
                if let Err(e) = users::check_name(&name) {
                    self.error = Some(e);
                } else if password != text(2) {
                    self.error = Some("The passwords do not match");
                } else {
                    self.run(move || users::create(&name, &password, admin));
                }
            }
            Action::Password { name, check_old } => {
                let (old, new, repeat) = if check_old { (text(0), text(1), text(2)) } else { (String::new(), text(0), text(1)) };
                if new != repeat {
                    self.error = Some("The passwords do not match");
                } else {
                    self.run(move || {
                        if check_old && !users::verify(&name, &old) {
                            return Err("The current password is wrong");
                        }
                        users::set_password(&name, &new)
                    });
                }
            }
            Action::Remove(name) => self.run(move || users::remove(&name)),
        }
    }
}

impl App for AccountDialog {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        for i in 0..self.fields.len() {
            let r = self.field(i).offset(a.x, a.y);
            c.text(a.x + PAD, r.y + 6, self.labels[i], TEXT, FontId::Regular, 14.0);
            c.fill_round(r, 7, 0x000000, 90);
            let focused = self.focus == i;
            c.stroke_round(r, 7, if focused { ACCENT } else { 0xFFFFFF }, if focused { 160 } else { 30 });
            self.fields[i].draw(c, r, focused, self.caret, "", TEXT);
        }
        if let Some(row) = self.admin_row() {
            let row = row.offset(a.x, a.y);
            ui::switch(c, Rect::new(row.x, row.y + 2, 40, 22), self.admin);
            c.text(row.x + 52, row.y + 4, "Administrator", TEXT, FontId::Regular, 14.0);
        }
        if let Action::Remove(name) = &self.action {
            c.text(a.x + PAD, a.y + 24, &format!("Remove {name} and all files in {}?", access::home_of(name)), TEXT, FontId::Regular, 14.0);
            c.text(a.x + PAD, a.y + 48, "This cannot be undone.", TEXT_DIM, FontId::Regular, 13.0);
        }
        let ok = self.ok_button().offset(a.x, a.y);
        if let Some(e) = self.error {
            c.text(a.x + PAD, ok.y - 26, e, 0xF87171, FontId::Regular, 13.0);
        } else if self.job.is_some() {
            c.text(a.x + PAD, ok.y - 26, "Working\u{2026}", TEXT_DIM, FontId::Regular, 13.0);
        }
        ui::button(c, self.cancel_button().offset(a.x, a.y), "Cancel", true);
        ui::accent_button(c, ok, self.ok_label(), self.job.is_none());
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        self.caret = true;
        match ch {
            0x1B => self.done = true,
            b'\n' => self.submit(),
            b'\t' if !self.fields.is_empty() => self.focus = (self.focus + 1) % self.fields.len(),
            _ => {
                let focus = self.focus;
                return self.fields.get_mut(focus).is_some_and(|f| f.key(ch, |c| c.is_ascii()));
            }
        }
        true
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        // The name becomes a folder name, so Latin letters only.
        if self.labels.get(self.focus) == Some(&"Name") {
            self.error = Some("Use Latin letters for the name");
            return true;
        }
        let focus = self.focus;
        self.fields.get_mut(focus).is_some_and(|f| f.type_char(ch, |c| !c.is_control()))
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if !pressed || buttons & LEFT == 0 {
            let r = self.field(self.focus);
            let focus = self.focus;
            return self.fields.get_mut(focus).is_some_and(|f| f.mouse(r, x, y, buttons, pressed));
        }
        if self.ok_button().contains(x, y) {
            self.submit();
        } else if self.cancel_button().contains(x, y) {
            self.done = true;
        } else if self.admin_row().is_some_and(|r| r.contains(x, y)) {
            self.admin = !self.admin;
        } else if let Some(i) = (0..self.fields.len()).find(|&i| self.field(i).contains(x, y)) {
            self.focus = i;
            let r = self.field(i);
            self.fields[i].mouse(r, x, y, buttons, pressed);
        } else {
            return false;
        }
        true
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let on = ctx.caret_on();
        let mut changed = on != self.caret;
        self.caret = on;
        if let Some(result) = self.job.as_ref().and_then(|j| j.lock().take()) {
            self.job = None;
            changed = true;
            match result {
                Ok(()) => {
                    crate::fs::touch();
                    self.done = true;
                }
                Err(e) => self.error = Some(e),
            }
        }
        changed
    }

    fn finished(&self) -> bool {
        self.done
    }
}
