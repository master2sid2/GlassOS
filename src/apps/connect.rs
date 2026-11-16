//! "Connect to server": an SMB network folder (Windows, Samba) in /media,
//! a computer by address into the network neighbourhood, or a login to a computer.

use super::ui;
use crate::font::FontId;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::net::{self, Ip, smb};
use crate::sync::IrqLock;
use crate::task;
use crate::wm::{self, ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

const BG: Color = 0x1C1F2E;
const PAD: i32 = 20;
const LABELS: [&str; 3] = ["Address", "User", "Password"];
const HINTS: [&str; 3] = ["smb://server or smb://server/share", "guest if empty", ""];
const FIELD_X: i32 = 110;

pub const W: i32 = 460;
pub const H: i32 = 306;

/// The connection result from the background thread: Ok(path) or Err(reason).
type Outcome = Arc<IrqLock<Option<Result<String, String>>>>;

pub struct ConnectDialog {
    cw: i32,
    ch: i32,
    fields: [ui::TextField; 3],
    focus: usize,
    status: Option<(String, bool)>,
    pending: Option<Outcome>,
    cursor_on: bool,
    done: bool,
    /// Mount at boot (and keep the password).
    auto: bool,
    /// A login to a found computer: (name, address, port).
    login: Option<(String, Ip, u16)>,
    /// The folder name and the computer address, when mounting a found folder.
    last_ip: String,
    /// Whom to tell about success (the path of the mounted folder, or empty): the Files window itself
    /// goes where it was going. Without it we open the folder or show a notification.
    report: Option<Arc<IrqLock<Option<String>>>>,
}

impl ConnectDialog {
    pub fn new() -> Self {
        Self {
            cw: W,
            ch: H,
            fields: [ui::TextField::new("smb://", 200), ui::TextField::new("", 120), ui::TextField::new("", 120).secret()],
            focus: 0,
            status: None,
            pending: None,
            cursor_on: true,
            done: false,
            auto: true,
            login: None,
            last_ip: String::new(),
            report: None,
        }
    }

    /// Report success to `slot` instead of opening the folder ourselves.
    pub fn report_to(mut self, slot: Arc<IrqLock<Option<String>>>) -> Self {
        self.report = Some(slot);
        self
    }

    /// A folder of a found computer that did not let us in as a guest.
    pub fn for_share(host: &str, ip: Ip, port: u16, share: &str, error: &str) -> Self {
        let mut d = Self::new();
        // In the address a resource may be followed by a folder inside it.
        let address = if port == 445 { format!("smb://{host}/{share}") } else { format!("smb://{host}:{port}/{share}") };
        d.fields[0].set_text(&address);
        d.focus = 1;
        d.last_ip = net::ip_str(ip);
        d.status = Some((String::from(error), true));
        d
    }

    /// A login to a computer: to see its folders.
    pub fn for_login(host: &str, ip: Ip, port: u16) -> Self {
        let mut d = Self::new();
        d.fields[0].set_text(&format!("smb://{host}"));
        d.focus = 1;
        d.login = Some((host.into(), ip, port));
        d
    }

    /// Show why we are asking.
    pub fn because(mut self, error: &str) -> Self {
        self.status = Some((String::from(error), true));
        self
    }

    fn auto_rect(&self) -> Rect {
        Rect::new(PAD, 20 + 3 * 44, self.cw - 2 * PAD, 26)
    }

    fn field(&self, i: usize) -> Rect {
        Rect::new(FIELD_X, 20 + i as i32 * 44, self.cw - FIELD_X - PAD, 30)
    }

    fn connect_button(&self) -> Rect {
        let w = ui::button_width("Connect");
        Rect::new(self.cw - PAD - w, self.ch - PAD - ui::BUTTON_H, w, ui::BUTTON_H)
    }

    fn cancel_button(&self) -> Rect {
        let w = ui::button_width("Cancel");
        let ok = self.connect_button();
        Rect::new(ok.x - 8 - w, ok.y, w, ui::BUTTON_H)
    }

    fn connect(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let address = self.fields[0].text.trim();
        let mut t = match smb::parse_url(address, None) {
            Ok(t) if !t.host.is_empty() => t,
            Ok(_) => {
                self.status = Some(("Write the address as smb://server/share".into(), true));
                return;
            }
            Err(e) => {
                self.status = Some((e, true));
                return;
            }
        };
        let user = self.fields[1].text.trim();
        if !user.is_empty() {
            let (domain, name) = user.split_once('\\').unwrap_or((t.domain.as_str(), user));
            (t.domain, t.user) = (String::from(domain), String::from(name));
            t.password = self.fields[2].text.clone();
        }
        t.auto = self.auto;
        t.last_ip = self.last_ip.clone();
        let login = self.login.clone();
        let outcome: Outcome = Arc::new(IrqLock::new(None));
        let slot = outcome.clone();
        task::spawn("smb-connect", None, move || {
            let r = if !t.share.is_empty() {
                smb::connect(t)
            } else if let Some((host, ip, port)) = login {
                let auto = t.auto;
                smb::log_in(&host, ip, port, &t.user, &t.password, &t.domain, auto).map(|()| String::new())
            } else {
                // Only an address — the computer goes into the network neighbourhood.
                let (user, password, domain, auto, port) = (t.user.clone(), t.password.clone(), t.domain.clone(), t.auto, t.port);
                net::discovery::add_host(&t.host, port).and_then(|path| {
                    if !user.is_empty()
                        && let Some((host, ip, port)) = net::discovery::host_of(&path)
                    {
                        smb::log_in(&host, ip, port, &user, &password, &domain, auto)?;
                    }
                    Ok(path)
                })
            };
            *slot.lock() = Some(r);
        });
        self.pending = Some(outcome);
        self.status = Some(("Connecting\u{2026}".into(), false));
    }
}

impl App for ConnectDialog {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        for i in 0..3 {
            let r = self.field(i).offset(a.x, a.y);
            c.text(a.x + PAD, r.y + 6, LABELS[i], TEXT, FontId::Regular, 14.0);
            c.fill_round(r, 7, 0x000000, 90);
            let focused = self.focus == i;
            c.stroke_round(r, 7, if focused { ACCENT } else { 0xFFFFFF }, if focused { 160 } else { 30 });
            self.fields[i].draw(c, r, focused, self.cursor_on, HINTS[i], TEXT);
        }
        let sw = self.auto_rect().offset(a.x, a.y);
        ui::switch(c, Rect::new(sw.x, sw.y + 2, 40, 22), self.auto);
        c.text(sw.x + 52, sw.y + 4, "Reconnect at startup", TEXT, FontId::Regular, 14.0);
        if let Some((msg, error)) = &self.status {
            c.text(a.x + PAD, a.y + 200, msg, if *error { 0xF87171 } else { TEXT_DIM }, FontId::Regular, 13.0);
        }
        let busy = self.pending.is_some();
        ui::button(c, self.cancel_button().offset(a.x, a.y), "Cancel", true);
        ui::accent_button(c, self.connect_button().offset(a.x, a.y), "Connect", !busy);
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        self.cursor_on = true;
        match ch {
            0x1B => self.done = true,
            b'\n' => self.connect(),
            b'\t' => self.focus = (self.focus + 1) % 3,
            _ => return self.fields[self.focus].key(ch, |_| true),
        }
        true
    }

    fn text(&mut self, ch: char, _ctx: &Ctx) -> bool {
        self.fields[self.focus].type_char(ch, |c| !c.is_control())
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        // Mouse selection in the focused field (and the button release).
        if !pressed || buttons & LEFT == 0 {
            let r = self.field(self.focus);
            return self.fields[self.focus].mouse(r, x, y, buttons, pressed);
        }
        if self.connect_button().contains(x, y) {
            self.connect();
        } else if self.cancel_button().contains(x, y) {
            self.done = true;
        } else if self.auto_rect().contains(x, y) {
            self.auto = !self.auto;
        } else if let Some(i) = (0..3).find(|&i| self.field(i).contains(x, y)) {
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
        let mut changed = on != self.cursor_on;
        self.cursor_on = on;
        let result = self.pending.as_ref().and_then(|o| o.lock().take());
        if let Some(r) = result {
            self.pending = None;
            match r {
                Ok(path) => {
                    if let Some(slot) = &self.report {
                        *slot.lock() = Some(path);
                    } else if path.starts_with(crate::fs::NETWORK) {
                        wm::request(wm::Request::OpenFolder(path));
                    } else if !path.is_empty() {
                        wm::notify("Network folder connected", &path);
                    }
                    self.done = true;
                }
                Err(e) => self.status = Some((e, true)),
            }
            changed = true;
        }
        changed
    }

    fn finished(&self) -> bool {
        self.done
    }
}
