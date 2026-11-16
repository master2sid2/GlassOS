//! The login screen: creating the first account, choosing a user, the password and
//! the lock screen. It lives outside the window manager: while it is on screen, windows get
//! neither the keyboard nor the mouse, and without a logged-in user there are no windows at all.

use super::ui;
use crate::font::{self, FontId};
use crate::gfx::{Canvas, Rect};
use crate::input::{KEY_LEFT, KEY_RIGHT, LEFT};
use crate::interrupts::HZ;
use crate::keyboard;
use crate::sync::IrqLock;
use crate::users::{self, Account};
use crate::wallpaper;
use crate::wm::{ACCENT, Ctx, OS_NAME, TEXT, TEXT_DIM};
use crate::{task, theme};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const CARD_W: i32 = 380;

fn home() -> (i32, i32) {
    let screen = crate::output::primary();
    (screen.x, screen.y)
}
const FIELD_H: i32 = 36;
const PAD: i32 = 28;
const TILE_W: i32 = 132;
const TILE_H: i32 = 136;
const AVATAR: i32 = 72;
const ERROR: u32 = 0xF87171;

enum Screen {
    /// There are no accounts yet: name, password, repeat.
    Setup,
    /// Several users: the selected tile.
    Pick(usize),
    Password(String),
}

/// How the screen ended.
pub enum Outcome {
    /// Log in under this name.
    Enter(String),
    /// Return to the locked desktop.
    Unlock,
}

type Job = Arc<IrqLock<Option<Result<(), &'static str>>>>;

pub struct Login {
    /// The primary screen on the desktop: everything is drawn in its middle.
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    wallpaper: Vec<Vec<u32>>,
    screen: Screen,
    /// This user's desktop is locked.
    locked: Option<String>,
    accounts: Vec<Account>,
    fields: [ui::TextField; 3],
    focus: usize,
    error: Option<&'static str>,
    /// The password hash is computed in the background: it takes long, and the screen must stay alive.
    job: Option<Job>,
    outcome: Option<Outcome>,
    dirty: bool,
    caret: bool,
    minute: u8,
    buttons: u8,
    hovered: Option<Rect>,
    keyboard_seen: u64,
    /// Show the Restart and Shut Down buttons.
    power: bool,
}

impl Login {
    /// The screen at boot and after logout.
    pub fn new(w: i32, h: i32) -> Self {
        if let Some(text) = users::greeting_file(crate::wm::SETTINGS_FILE) {
            let saved = theme::from_config(&text);
            theme::update(|t| *t = saved);
        }
        crate::keyboard::load(users::greeting_file(crate::keyboard::CONFIG).as_deref());
        let accounts = users::list();
        let screen = match &accounts[..] {
            [] => Screen::Setup,
            [only] => Screen::Password(only.name.clone()),
            many => {
                let last = users::last();
                Screen::Pick(many.iter().position(|a| a.name == last).unwrap_or(0))
            }
        };
        Self::with(w, h, screen, accounts, None)
    }

    /// The lock: only whoever was working can log in again. `power` — whether the computer
    /// may be turned off from here: not with unsaved data in windows, there is nobody to ask.
    pub fn locked(w: i32, h: i32, user: &str, power: bool) -> Self {
        let mut login = Self::with(w, h, Screen::Password(user.into()), users::list(), Some(user.into()));
        login.power = power;
        login
    }

    fn with(w: i32, h: i32, screen: Screen, accounts: Vec<Account>, locked: Option<String>) -> Self {
        let (x, y) = home();
        Self {
            x,
            y,
            w,
            h,
            wallpaper: wallpaper::desktop(theme::get().wallpaper),
            screen,
            locked,
            accounts,
            fields: [ui::TextField::new("", users::MAX_NAME), ui::TextField::new("", 64).secret(), ui::TextField::new("", 64).secret()],
            focus: 0,
            error: None,
            job: None,
            outcome: None,
            dirty: true,
            caret: true,
            minute: 0xFF,
            buttons: 0,
            hovered: None,
            keyboard_seen: keyboard::version(),
            power: true,
        }
    }

    /// The screen is ours again: redraw it.
    pub fn redraw(&mut self) {
        self.dirty = true;
    }

    pub fn take_outcome(&mut self) -> Option<Outcome> {
        self.outcome.take()
    }

    /// The screens were rearranged: a different primary size and different wallpaper.
    pub fn relayout(&mut self, w: i32, h: i32) {
        (self.w, self.h) = (w, h);
        (self.x, self.y) = home();
        self.wallpaper = wallpaper::desktop(theme::get().wallpaper);
        self.dirty = true;
    }

    fn card(&self) -> Rect {
        let h = if matches!(self.screen, Screen::Setup) { 370 } else { 262 };
        Rect::new(self.x + (self.w - CARD_W) / 2, self.y + (self.h - h) / 2 + 30, CARD_W, h)
    }

    fn field(&self, i: usize) -> Rect {
        let c = self.card();
        match self.screen {
            Screen::Setup => Rect::new(c.x + PAD, c.y + 104 + i as i32 * 66, CARD_W - 2 * PAD, FIELD_H),
            _ => Rect::new(c.x + PAD, c.y + 142, CARD_W - 2 * PAD, FIELD_H),
        }
    }

    fn submit_label(&self) -> &'static str {
        match self.screen {
            Screen::Setup => "Create account",
            _ if self.locked.is_some() => "Unlock",
            _ => "Sign In",
        }
    }

    fn has_back(&self) -> bool {
        matches!(self.screen, Screen::Password(_)) && self.locked.is_none() && self.accounts.len() > 1
    }

    /// The main button: in the centre of the card, and next to "Back" — opposite it.
    fn submit_button(&self) -> Rect {
        let c = self.card();
        let w = ui::button_width(self.submit_label());
        let x = if self.has_back() { c.x + c.w - PAD - w } else { c.x + (c.w - w) / 2 };
        Rect::new(x, c.y + c.h - PAD - ui::BUTTON_H + 6, w, ui::BUTTON_H)
    }

    fn back_button(&self) -> Option<Rect> {
        self.has_back().then(|| Rect::new(self.card().x + PAD, self.submit_button().y, ui::button_width("Back"), ui::BUTTON_H))
    }

    fn power_buttons(&self) -> [Rect; 2] {
        let shut = ui::button_width("Shut Down");
        let restart = ui::button_width("Restart");
        let y = self.y + self.h - ui::BUTTON_H - 20;
        let right = self.x + self.w - 20;
        [Rect::new(right - shut - 8 - restart, y, restart, ui::BUTTON_H), Rect::new(right - shut, y, shut, ui::BUTTON_H)]
    }

    fn tile(&self, i: usize) -> Rect {
        let n = self.accounts.len() as i32;
        let total = n * TILE_W + (n - 1) * 16;
        Rect::new(self.x + (self.w - total) / 2 + i as i32 * (TILE_W + 16), self.y + (self.h - TILE_H) / 2 + 30, TILE_W, TILE_H)
    }

    /// Which language is typing now: otherwise the password could be typed in the wrong one.
    fn lang_button(&self) -> Option<Rect> {
        let w = font::measure(FontId::SemiBold, 13.0, "EN") + 24;
        let x = if self.power { self.power_buttons()[0].x - 8 - w } else { self.x + self.w - 20 - w };
        (keyboard::enabled().len() > 1).then(|| Rect::new(x, self.y + self.h - ui::BUTTON_H - 20, w, ui::BUTTON_H))
    }

    /// Everything that can be pressed: for highlighting under the cursor.
    fn clickable(&self) -> Vec<Rect> {
        let mut out = if self.power { self.power_buttons().to_vec() } else { Vec::new() };
        out.extend(self.lang_button());
        match self.screen {
            Screen::Pick(_) => out.extend((0..self.accounts.len()).map(|i| self.tile(i))),
            _ => {
                out.push(self.submit_button());
                out.extend(self.back_button());
            }
        }
        out
    }

    fn run(&mut self, work: impl FnOnce() -> Result<(), &'static str> + Send + 'static) {
        let job: Job = Arc::new(IrqLock::new(None));
        let slot = job.clone();
        task::spawn("login", None, move || {
            let r = work();
            *slot.lock() = Some(r);
        });
        self.job = Some(job);
        self.error = None;
        self.dirty = true;
    }

    fn submit(&mut self) {
        if self.job.is_some() {
            return;
        }
        match &self.screen {
            Screen::Setup => {
                let name = self.fields[0].text.trim().to_ascii_lowercase();
                let password = self.fields[1].text.clone();
                if let Err(e) = users::check_name(&name) {
                    self.fail(e, 0);
                } else if password != self.fields[2].text {
                    self.fail("The passwords do not match", 2);
                } else {
                    self.fields[0].set_text(&name);
                    self.run(move || users::create(&name, &password, true));
                }
            }
            Screen::Password(name) => {
                let (name, password) = (name.clone(), self.fields[1].text.clone());
                self.run(move || {
                    if users::verify(&name, &password) {
                        return Ok(());
                    }
                    // Guessing passwords from the keyboard must be slow.
                    task::sleep_until(crate::interrupts::ticks() + HZ);
                    Err("Wrong password")
                });
            }
            Screen::Pick(i) => self.choose(*i),
        }
    }

    fn fail(&mut self, message: &'static str, focus: usize) {
        self.error = Some(message);
        self.focus = focus;
        self.dirty = true;
    }

    fn choose(&mut self, i: usize) {
        let Some(account) = self.accounts.get(i) else { return };
        let name = account.name.clone();
        if !account.has_password() {
            self.outcome = Some(Outcome::Enter(name));
            return;
        }
        // The password is typed in this user's language, not the one of whoever logged in before.
        keyboard::load(users::settings_file(&name, keyboard::CONFIG).as_deref());
        self.screen = Screen::Password(name);
        self.focus = 1;
        self.dirty = true;
    }

    fn back(&mut self) {
        if self.back_button().is_some() {
            self.fields[1].set_text("");
            self.error = None;
            self.screen = Screen::Pick(0);
            self.dirty = true;
        }
    }

    pub fn key(&mut self, ch: u8) {
        self.caret = true;
        self.dirty = true;
        if self.job.is_some() {
            return;
        }
        if let Screen::Pick(i) = &mut self.screen {
            let last = self.accounts.len() - 1;
            match ch {
                KEY_LEFT => *i = i.saturating_sub(1),
                KEY_RIGHT => *i = (*i + 1).min(last),
                b'\n' => self.submit(),
                _ => {}
            }
            return;
        }
        match ch {
            b'\n' => self.submit(),
            0x1B => self.back(),
            b'\t' if matches!(self.screen, Screen::Setup) => self.focus = (self.focus + 1) % 3,
            _ => {
                let i = self.active();
                self.fields[i].key(ch, |c| i != 0 || c.is_ascii());
            }
        }
    }

    /// A non-ASCII character was typed: it is not suitable for a name, which will become a folder name.
    pub fn text(&mut self, ch: char) {
        self.dirty = true;
        if self.job.is_some() || matches!(self.screen, Screen::Pick(_)) {
            return;
        }
        let i = self.active();
        if i == 0 {
            self.fail("Use Latin letters for the name", 0);
            return;
        }
        self.fields[i].type_char(ch, |c| !c.is_control());
    }

    /// The field with the cursor: on the password screen there is only one.
    fn active(&self) -> usize {
        if matches!(self.screen, Screen::Password(_)) { 1 } else { self.focus }
    }

    pub fn mouse(&mut self, x: i32, y: i32, buttons: u8) {
        let hovered = self.clickable().into_iter().find(|r| r.contains(x, y));
        if hovered != self.hovered {
            self.hovered = hovered;
            self.dirty = true;
        }
        let pressed = buttons & LEFT != 0 && self.buttons & LEFT == 0;
        self.buttons = buttons;
        let i = self.active();
        let r = self.field(i);
        if !pressed {
            self.dirty |= self.fields[i].mouse(r, x, y, buttons, false);
            return;
        }
        let [restart, shutdown] = self.power_buttons();
        if self.power && restart.contains(x, y) {
            crate::reboot();
        } else if self.power && shutdown.contains(x, y) {
            crate::shutdown();
        }
        self.dirty = true;
        if self.lang_button().is_some_and(|b| b.contains(x, y)) {
            keyboard::next();
            return;
        }
        match self.screen {
            Screen::Pick(_) => {
                if let Some(i) = (0..self.accounts.len()).find(|&i| self.tile(i).contains(x, y)) {
                    self.choose(i);
                }
            }
            Screen::Setup | Screen::Password(_) => {
                if self.submit_button().contains(x, y) {
                    self.submit();
                } else if self.back_button().is_some_and(|b| b.contains(x, y)) {
                    self.back();
                } else if matches!(self.screen, Screen::Setup)
                    && let Some(i) = (0..3).find(|&i| self.field(i).contains(x, y))
                {
                    self.focus = i;
                    let r = self.field(i);
                    self.fields[i].mouse(r, x, y, buttons, true);
                } else {
                    self.fields[i].mouse(r, x, y, buttons, true);
                }
            }
        }
    }

    pub fn tick(&mut self, ctx: &Ctx) {
        if keyboard::version() != self.keyboard_seen {
            self.keyboard_seen = keyboard::version();
            self.dirty = true;
        }
        let caret = ctx.caret_on();
        if caret != self.caret && !matches!(self.screen, Screen::Pick(_)) {
            self.caret = caret;
            self.dirty = true;
        }
        if ctx.time.min != self.minute {
            self.minute = ctx.time.min;
            self.dirty = true;
        }
        let Some(result) = self.job.as_ref().and_then(|j| j.lock().take()) else { return };
        self.job = None;
        self.dirty = true;
        match result {
            Ok(()) if self.locked.is_some() => self.outcome = Some(Outcome::Unlock),
            Ok(()) => {
                let name = match &self.screen {
                    Screen::Password(name) => name.clone(),
                    _ => self.fields[0].text.clone(),
                };
                self.outcome = Some(Outcome::Enter(name));
            }
            Err(e) => {
                self.error = Some(e);
                self.fields[1].set_text("");
                self.fields[2].set_text("");
            }
        }
    }

    /// Draws the screen if something changed; returns what to output.
    pub fn render(&mut self, scenes: &mut [Canvas], ctx: &Ctx) -> Vec<Rect> {
        if !core::mem::take(&mut self.dirty) {
            return Vec::new();
        }
        for (i, c) in scenes.iter_mut().enumerate() {
            self.draw_scene(c, i, ctx);
        }
        alloc::vec![crate::output::bounds()]
    }

    fn draw_scene(&mut self, c: &mut Canvas, index: usize, ctx: &Ctx) {
        let screen = c.bounds();
        c.reset_clip();
        c.copy_from(&self.wallpaper[index], screen);
        c.blend(screen, 0x000000, 70);
        let time = format!("{:02}:{:02}", ctx.time.hour, ctx.time.min);
        let tw = font::measure(FontId::SemiBold, 54.0, &time);
        let top = (self.card().y - 110).max(self.y + 16);
        c.text(self.x + (self.w - tw) / 2, top, &time, TEXT, FontId::SemiBold, 54.0);
        ui::begin(self.hovered.map(|r| (r.x + 1, r.y + 1)), false);
        match &self.screen {
            Screen::Setup => self.draw_setup(c),
            Screen::Pick(i) => self.draw_pick(c, *i),
            Screen::Password(name) => {
                let name = name.clone();
                self.draw_password(c, &name);
            }
        }
        let [restart, shutdown] = self.power_buttons();
        if self.power {
            ui::button(c, restart, "Restart", true);
            ui::button(c, shutdown, "Shut Down", true);
        }
        if let Some(b) = self.lang_button() {
            ui::button(c, b, keyboard::LAYOUTS[keyboard::active()].code, true);
        }
        ui::end();
    }

    fn panel(&self, c: &mut Canvas) -> Rect {
        let r = self.card();
        c.shadow(r, 16, 24, 8, 150);
        c.fill_round(r, 16, 0x151827, 200);
        c.stroke_round(r, 16, 0xFFFFFF, 40);
        r
    }

    fn centered(&self, c: &mut Canvas, y: i32, s: &str, color: u32, font_id: FontId, size: f32) {
        let w = font::measure(font_id, size, s);
        c.text(self.x + (self.w - w) / 2, y, s, color, font_id, size);
    }

    fn input(&self, c: &mut Canvas, i: usize, hint: &str) {
        let r = self.field(i);
        c.fill_round(r, 8, 0x000000, 100);
        let focused = self.active() == i;
        c.stroke_round(r, 8, if focused { ACCENT } else { 0xFFFFFF }, if focused { 170 } else { 30 });
        self.fields[i].draw(c, r, focused, self.caret, hint, TEXT);
    }

    /// A message under the fields, centred.
    fn status(&self, c: &mut Canvas, y: i32, working: &str) {
        if let Some(e) = self.error {
            self.centered(c, y, e, ERROR, FontId::Regular, 13.0);
        } else if self.job.is_some() {
            self.centered(c, y, working, TEXT_DIM, FontId::Regular, 13.0);
        } else if keyboard::caps() {
            self.centered(c, y, "Caps Lock is on", TEXT_DIM, FontId::Regular, 13.0);
        }
    }

    fn avatar(c: &mut Canvas, r: Rect, name: &str) {
        c.fill_round(r, r.w / 2, ACCENT, 220);
        let letter = name.chars().next().unwrap_or('?').to_ascii_uppercase();
        let mut buf = [0u8; 4];
        let s = letter.encode_utf8(&mut buf);
        let size = r.w as f32 * 0.5;
        let w = font::measure(FontId::SemiBold, size, s);
        c.text(r.x + (r.w - w) / 2, r.y + (r.h - size as i32) / 2 - 3, s, 0x0B1020, FontId::SemiBold, size);
    }

    fn draw_setup(&self, c: &mut Canvas) {
        let r = self.panel(c);
        self.centered(c, r.y + 22, &format!("Welcome to {OS_NAME}"), TEXT, FontId::SemiBold, 21.0);
        self.centered(c, r.y + 54, "Create your account to get started", TEXT_DIM, FontId::Regular, 13.5);
        for (i, label) in ["Name", "Password", "Repeat password"].iter().enumerate() {
            c.text(r.x + PAD, self.field(i).y - 20, label, TEXT_DIM, FontId::Regular, 13.0);
            self.input(c, i, if i == 1 { "empty: sign in without a password" } else { "" });
        }
        self.status(c, self.field(2).y + FIELD_H + 12, "Creating\u{2026}");
        ui::accent_button(c, self.submit_button(), self.submit_label(), self.job.is_none());
    }

    fn draw_pick(&self, c: &mut Canvas, selected: usize) {
        self.centered(c, self.tile(0).y - 44, "Who is signing in?", TEXT, FontId::SemiBold, 20.0);
        for (i, a) in self.accounts.iter().enumerate() {
            let r = self.tile(i);
            let lit = i == selected || self.hovered == Some(r);
            c.fill_round(r, 16, 0x151827, if lit { 225 } else { 170 });
            c.stroke_round(r, 16, if i == selected { ACCENT } else { 0xFFFFFF }, if i == selected { 170 } else { 30 });
            Self::avatar(c, Rect::new(r.x + (r.w - AVATAR) / 2, r.y + 16, AVATAR, AVATAR), &a.name);
            let w = font::measure(FontId::Regular, 15.0, &a.name);
            c.text(r.x + (r.w - w) / 2, r.y + 100, &a.name, TEXT, FontId::Regular, 15.0);
        }
    }

    fn draw_password(&self, c: &mut Canvas, name: &str) {
        let r = self.panel(c);
        Self::avatar(c, Rect::new(r.x + (r.w - AVATAR) / 2, r.y + 18, AVATAR, AVATAR), name);
        self.centered(c, r.y + 100, name, TEXT, FontId::SemiBold, 17.0);
        let protected = self.accounts.iter().find(|a| a.name == name).is_none_or(|a| a.has_password());
        self.input(c, 1, if protected { "Password" } else { "No password: press Enter" });
        self.status(c, self.field(1).y + FIELD_H + 6, "Checking\u{2026}");
        if let Some(back) = self.back_button() {
            ui::button(c, back, "Back", true);
        }
        ui::accent_button(c, self.submit_button(), self.submit_label(), self.job.is_none());
    }
}
