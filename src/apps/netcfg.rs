//! Network settings. The Address tab — the address over DHCP or manual (IP, mask, gateway,
//! DNS, the /system/net.cfg file); the Computer tab — how the computer is named on the network
//! (name, workgroup, description, visibility, the /system/identity.cfg file); the
//! Wi-Fi tab — choosing a network, absent on a computer without a module.
//! Opens with a click on the network icon in the panel: right on whatever carries the connection.

use super::ui::{self, TAB_BAR_H, TabBar};
use alloc::format;
use alloc::vec::Vec;
use crate::font::FontId;
use crate::gfx::{Canvas, Color, Rect};
use crate::input::LEFT;
use crate::net::identity::{self, DESCRIPTION_MAX, Identity, NAME_MAX};
use crate::net::{self, Config, Ip};
use crate::wm::{ACCENT, App, Ctx, TEXT, TEXT_DIM};
use alloc::string::String;

const NOT_APPLIED: &str = "Not applied: administrators only, or the disk failed";
const BG: Color = 0x1C1F2E;
const PAD: i32 = 22;
const ESC: u8 = 0x1B;
const RED: Color = 0xF87171;
const GREEN: Color = 0x4ADE80;
const LABELS: [&str; 4] = ["IP address", "Subnet mask", "Gateway", "DNS server"];
const TABS: [&str; 3] = ["Address", "Computer", "Wi-Fi"];
const WIFI: usize = 2;
const WIFI_ROW: i32 = 30;
const WIFI_TOP: i32 = TAB_BAR_H + 56;
const WIFI_BOTTOM: i32 = 110;
const PASSWORD_MAX: usize = 63;
const ID_LABELS: [&str; 3] = ["Computer name", "Workgroup", "Description"];
const FIELD_X: i32 = 140;
const FIELDS_Y: i32 = TAB_BAR_H + 128;
const FIELD_H: i32 = 30;
const FIELD_STEP: i32 = 42;

pub struct NetSettings {
    cw: i32,
    ch: i32,
    tab: usize,
    /// The Computer tab: fields, which of them has focus, visibility on the network.
    id_fields: [ui::TextField; 3],
    id_focus: usize,
    visible: bool,
    /// Whether there is a network card.
    present: bool,
    dhcp: bool,
    fields: [ui::TextField; 4],
    focus: Option<usize>,
    /// An error: (field or None, text).
    error: Option<(Option<usize>, &'static str)>,
    /// The message after "Apply".
    applied: Option<&'static str>,
    status: String,
    cursor_on: bool,
    /// When the network state was last checked.
    checked: u64,
    done: bool,
    /// The Wi-Fi tab: networks (name, signal, security, whether we can connect), the selected
    /// network, the password and what the module is doing now.
    networks: Vec<(String, i32, &'static str, bool)>,
    chosen: Option<String>,
    password: ui::TextField,
    wifi_status: String,
    /// The network we are connected/connecting to — its row is highlighted.
    connected: Option<String>,
    /// Saved networks (names) and whether their list is shown instead of the ones found nearby.
    saved: Vec<String>,
    saved_view: bool,
    /// A page inside Settings: it has no Close button of its own, Settings closes the window.
    embedded: bool,
}

impl NetSettings {
    pub fn new(cw: i32, ch: i32, arg: Option<&str>) -> Self {
        let cfg = net::config();
        let id = identity::get();
        // Open right on the needed tab: the "wifi" argument (for example, from `app netcfg wifi`)
        // or when the connection goes over Wi-Fi.
        let on_wifi = (arg == Some("wifi") && crate::wifi::available()) || net::over_wifi();
        let mut s = Self {
            cw,
            ch,
            tab: if on_wifi { WIFI } else { 0 },
            networks: Vec::new(),
            chosen: None,
            password: ui::TextField::new("", PASSWORD_MAX).secret(),
            wifi_status: crate::wifi::status(),
            connected: crate::wifi::network(),
            saved: crate::wifi::saved_networks(),
            saved_view: false,
            id_fields: [ui::TextField::new(&id.name, NAME_MAX), ui::TextField::new(&id.workgroup, NAME_MAX), ui::TextField::new(&id.description, DESCRIPTION_MAX)],
            id_focus: 0,
            visible: id.visible,
            present: cfg.is_some(),
            dhcp: cfg.is_none_or(|c| c.dhcp),
            fields: core::array::from_fn(|_| ui::TextField::new("", 15)),
            focus: None,
            error: None,
            applied: None,
            status: net::state_text(),
            cursor_on: true,
            checked: 0,
            done: false,
            embedded: false,
        };
        if let Some(c) = cfg {
            s.fill(&c);
        }
        if s.tab == WIFI {
            s.open_wifi();
        }
        s
    }

    /// A page inside Settings (without its own Close button).
    pub fn embedded(mut self) -> Self {
        self.embedded = true;
        self
    }

    /// The tabs of this computer.
    fn tabs(&self) -> &'static [&'static str] {
        if crate::wifi::available() { &TABS } else { &TABS[..WIFI] }
    }

    /// The Wi-Fi tab shows networks right away: if there is no list yet, scan.
    fn open_wifi(&mut self) {
        self.refresh_wifi();
        if self.networks.is_empty() {
            Self::scan();
        }
    }

    fn scan() {
        if !crate::wifi::busy() {
            crate::task::spawn("wifi-scan", None, || {
                let _ = crate::wifi::scan();
            });
        }
    }

    /// Fields from the active settings (empty instead of 0.0.0.0).
    fn fill(&mut self, c: &Config) {
        for (f, ip) in self.fields.iter_mut().zip([c.ip, c.mask, c.gw, c.dns]) {
            f.set_text(&if ip == [0; 4] { String::new() } else { net::ip_str(ip) });
        }
    }

    fn radio(&self, n: usize) -> Rect {
        Rect::new(PAD, TAB_BAR_H + 50 + n as i32 * 32, self.cw - 2 * PAD, 26)
    }

    fn field(&self, i: usize) -> Rect {
        Rect::new(FIELD_X, FIELDS_Y + i as i32 * FIELD_STEP, self.cw - FIELD_X - PAD, FIELD_H)
    }

    fn bar(&self) -> TabBar {
        TabBar::fixed(Rect::new(0, 0, self.cw, TAB_BAR_H), self.tabs().len())
    }

    fn wifi_rows(&self) -> usize {
        ((self.ch - WIFI_TOP - WIFI_BOTTOM) / WIFI_ROW).max(1) as usize
    }

    fn wifi_row(&self, i: usize) -> Rect {
        Rect::new(PAD, WIFI_TOP + i as i32 * WIFI_ROW, self.cw - 2 * PAD, WIFI_ROW - 2)
    }

    fn scan_button(&self) -> Rect {
        let w = ui::button_width("Scan");
        Rect::new(self.cw - PAD - w, TAB_BAR_H + 14, w, ui::BUTTON_H)
    }

    /// The list switch: saved networks / found nearby.
    fn saved_button(&self) -> Rect {
        let w = ui::button_width(self.saved_label());
        Rect::new(self.scan_button().x - 8 - w, TAB_BAR_H + 14, w, ui::BUTTON_H)
    }

    fn saved_label(&self) -> &'static str {
        if self.saved_view { "Nearby" } else { "Saved" }
    }

    fn forget_button(&self) -> Rect {
        Rect::new(PAD, self.ch - WIFI_BOTTOM + 8, ui::button_width("Forget"), ui::BUTTON_H)
    }

    /// The network's signal, if it is nearby now.
    fn signal_of(&self, ssid: &str) -> Option<i32> {
        self.networks.iter().find(|(name, ..)| name == ssid).map(|(_, signal, ..)| *signal)
    }

    /// The rows of the current list.
    fn rows(&self) -> usize {
        if self.saved_view { self.saved.len() } else { self.networks.len() }
    }

    fn password_field(&self) -> Rect {
        let w = ui::button_width("Connect");
        Rect::new(PAD, self.ch - WIFI_BOTTOM + 8, self.cw - 2 * PAD - w - 10, FIELD_H)
    }

    fn connect_button(&self) -> Rect {
        let w = ui::button_width("Connect");
        Rect::new(self.cw - PAD - w, self.ch - WIFI_BOTTOM + 8, w, ui::BUTTON_H)
    }

    /// Refreshes the Wi-Fi tab. One name — one row: the strongest access point of the network.
    fn refresh_wifi(&mut self) -> bool {
        let status = crate::wifi::status();
        let connected = crate::wifi::network();
        let mut list: Vec<(String, i32, &'static str, bool)> = Vec::new();
        for n in crate::wifi::found().into_iter().filter(|n| !n.ssid.is_empty()) {
            if !list.iter().any(|(name, ..)| *name == n.ssid) {
                list.push((n.ssid, n.signal, n.security, n.usable));
            }
        }
        // Connected to a new network — it is already among the saved ones: re-read the list.
        if connected != self.connected {
            self.saved = crate::wifi::saved_networks();
        }
        let changed = status != self.wifi_status || list != self.networks || connected != self.connected;
        (self.wifi_status, self.networks, self.connected) = (status, list, connected);
        changed
    }

    /// A password was entered — connect with it; empty, and the network is saved — with the saved key.
    fn wifi_connect(&mut self) {
        if let Some(ssid) = &self.chosen {
            if self.password.text.is_empty() && self.saved.contains(ssid) {
                crate::wifi::connect_saved(ssid);
            } else if !self.saved_view {
                crate::wifi::connect(ssid, &self.password.text);
            }
            self.password.set_text("");
        }
    }

    fn can_connect(&self) -> bool {
        match &self.chosen {
            Some(ssid) if self.saved_view => self.signal_of(ssid).is_some(),
            Some(_) => true,
            None => false,
        }
    }

    fn draw_wifi(&mut self, c: &mut Canvas, a: Rect) {
        let off = |r: Rect| r.offset(a.x, a.y);
        // The status on the left, the Scan button on the right of the same row: trim the status so it does not overlap.
        let avail = self.saved_button().x - PAD - 12;
        c.text(a.x + PAD, a.y + TAB_BAR_H + 18, &ui::fit(&self.wifi_status, FontId::Regular, 14.0, avail), TEXT, FontId::Regular, 14.0);
        ui::button(c, off(self.scan_button()), "Scan", !crate::wifi::busy());
        ui::button(c, off(self.saved_button()), self.saved_label(), true);
        if self.saved_view {
            self.draw_saved(c, a);
            return;
        }
        for (i, (name, signal, security, usable)) in self.networks.iter().take(self.wifi_rows()).enumerate() {
            let r = off(self.wifi_row(i));
            let connected = self.connected.as_deref() == Some(name.as_str());
            if connected {
                c.fill_round(r, 7, ACCENT, 48);
            }
            if self.chosen.as_ref() == Some(name) {
                c.fill_round(r, 7, 0xFFFFFF, 26);
            }
            // The network we are connected to has a dot mark and its name in the accent colour.
            let name_x = if connected {
                c.fill_round(Rect::new(r.x + 8, r.y + r.h / 2 - 3, 6, 6), 3, ACCENT, 255);
                r.x + 22
            } else {
                r.x + 10
            };
            let color = if connected { ACCENT } else if *usable { TEXT } else { TEXT_DIM };
            c.text(name_x, r.y + 6, name, color, FontId::Regular, 14.0);
            let saved = if self.saved.contains(name) { "saved  " } else { "" };
            let detail = format!("{saved}{security}  {signal} dBm");
            let w = crate::font::measure(FontId::Regular, 13.0, &detail);
            c.text(r.x + r.w - w - 10, r.y + 7, &detail, TEXT_DIM, FontId::Regular, 13.0);
        }
        let r = off(self.password_field());
        c.fill_round(r, 7, 0x000000, 90);
        c.stroke_round(r, 7, 0xFFFFFF, 40);
        let hint = if self.chosen.as_ref().is_some_and(|s| self.saved.contains(s)) { "Saved \u{2014} no password needed" } else { "Password" };
        self.password.draw(c, r, self.chosen.is_some(), self.cursor_on, hint, TEXT);
        ui::button(c, off(self.connect_button()), "Connect", self.can_connect());
    }

    /// The saved network list: which are nearby, forget, connect without a password.
    fn draw_saved(&self, c: &mut Canvas, a: Rect) {
        let off = |r: Rect| r.offset(a.x, a.y);
        if self.saved.is_empty() {
            c.text(a.x + PAD, a.y + WIFI_TOP + 6, "No saved networks yet. A network is saved when it connects.", TEXT_DIM, FontId::Regular, 14.0);
        }
        for (i, name) in self.saved.iter().take(self.wifi_rows()).enumerate() {
            let r = off(self.wifi_row(i));
            let connected = self.connected.as_deref() == Some(name.as_str());
            if connected {
                c.fill_round(r, 7, ACCENT, 48);
            }
            if self.chosen.as_ref() == Some(name) {
                c.fill_round(r, 7, 0xFFFFFF, 26);
            }
            let signal = self.signal_of(name);
            let color = if connected { ACCENT } else if signal.is_some() { TEXT } else { TEXT_DIM };
            c.text(r.x + 10, r.y + 6, name, color, FontId::Regular, 14.0);
            let detail = match signal {
                _ if connected => String::from("connected"),
                Some(s) => format!("in range  {s} dBm"),
                None => String::from("not in range"),
            };
            let w = crate::font::measure(FontId::Regular, 13.0, &detail);
            c.text(r.x + r.w - w - 10, r.y + 7, &detail, TEXT_DIM, FontId::Regular, 13.0);
        }
        ui::button(c, off(self.forget_button()), "Forget", self.chosen.is_some());
        ui::button(c, off(self.connect_button()), "Connect", self.can_connect());
    }

    fn wifi_mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if !pressed || buttons & LEFT == 0 {
            if self.saved_view {
                return false;
            }
            let r = self.password_field();
            return self.password.mouse(r, x, y, buttons, pressed);
        }
        if self.close_button().contains(x, y) {
            self.done = true;
        } else if self.scan_button().contains(x, y) {
            Self::scan();
        } else if self.saved_button().contains(x, y) {
            self.saved_view = !self.saved_view;
            self.saved = crate::wifi::saved_networks();
            self.chosen = None;
            self.password.set_text("");
        } else if self.connect_button().contains(x, y) {
            if self.can_connect() {
                self.wifi_connect();
            }
        } else if self.saved_view && self.forget_button().contains(x, y) {
            if let Some(ssid) = self.chosen.take() {
                crate::wifi::forget(&ssid);
                self.saved = crate::wifi::saved_networks();
            }
        } else if !self.saved_view && self.password_field().contains(x, y) {
            let r = self.password_field();
            return self.password.mouse(r, x, y, buttons, pressed);
        } else if let Some(i) = (0..self.rows().min(self.wifi_rows())).find(|&i| self.wifi_row(i).contains(x, y)) {
            self.chosen = Some(if self.saved_view { self.saved[i].clone() } else { self.networks[i].0.clone() });
        } else {
            return false;
        }
        true
    }

    fn id_field(&self, i: usize) -> Rect {
        Rect::new(FIELD_X, TAB_BAR_H + 20 + i as i32 * FIELD_STEP, self.cw - FIELD_X - PAD, FIELD_H)
    }

    fn visible_rect(&self) -> Rect {
        Rect::new(PAD, TAB_BAR_H + 20 + 3 * FIELD_STEP, self.cw - 2 * PAD, 26)
    }

    /// Checks and saves the Computer tab.
    fn apply_identity(&mut self) {
        let name = self.id_fields[0].text.trim().to_uppercase();
        let workgroup = self.id_fields[1].text.trim().to_uppercase();
        if !identity::valid_name(&name, false) {
            self.error = Some((Some(0), "Letters, digits, - and _ only, up to 15"));
            self.id_focus = 0;
            return;
        }
        if !identity::valid_name(&workgroup, true) {
            self.error = Some((Some(1), "Letters, digits, - and _ only, up to 15"));
            self.id_focus = 1;
            return;
        }
        self.id_fields[0].set_text(&name);
        self.id_fields[1].set_text(&workgroup);
        let id = Identity { name, workgroup, description: String::from(self.id_fields[2].text.trim()), visible: self.visible };
        self.error = None;
        self.applied = Some(if identity::set(id) { "Settings applied" } else { NOT_APPLIED });
        net::smbd::start();
    }

    fn draw_identity(&mut self, c: &mut Canvas, a: Rect) {
        for (i, label) in ID_LABELS.iter().enumerate() {
            let r = self.id_field(i).offset(a.x, a.y);
            c.text(a.x + PAD, r.y + 6, label, TEXT, FontId::Regular, 14.0);
            let bad = self.error.is_some_and(|(f, _)| f == Some(i));
            let focused = self.id_focus == i;
            c.fill_round(r, 7, 0x000000, 90);
            let (border, alpha) = if bad {
                (RED, 220)
            } else if focused {
                (ACCENT, 160)
            } else {
                (0xFFFFFF, 30)
            };
            c.stroke_round(r, 7, border, alpha);
            self.id_fields[i].draw(c, r, focused, self.cursor_on, if i == 2 { "optional" } else { "" }, TEXT);
        }
        let v = self.visible_rect().offset(a.x, a.y);
        ui::switch(c, Rect::new(v.x, v.y + 2, 40, 22), self.visible);
        c.text(v.x + 52, v.y + 4, "Visible to other computers", TEXT, FontId::Regular, 14.0);
        let hint = if self.visible { "Windows, Linux and macOS see this name on the network" } else { "Shared folders still work by address" };
        c.text(a.x + PAD, v.y + 40, hint, TEXT_DIM, FontId::Regular, 13.0);
        let (msg, color) = match (self.error, self.applied) {
            (Some((_, e)), _) => (e, RED),
            (None, Some(m)) => (m, GREEN),
            _ => ("", TEXT_DIM),
        };
        c.text(a.x + PAD, v.y + 64, msg, color, FontId::Regular, 13.0);
    }

    fn apply_button(&self) -> Rect {
        let w = ui::button_width("Apply");
        Rect::new(self.cw - PAD - w, self.ch - 16 - ui::BUTTON_H, w, ui::BUTTON_H)
    }

    fn close_button(&self) -> Rect {
        if self.embedded {
            return Rect::new(-1000, -1000, 0, 0);
        }
        let w = ui::button_width("Close");
        Rect::new(self.apply_button().x - 8 - w, self.ch - 16 - ui::BUTTON_H, w, ui::BUTTON_H)
    }

    fn set_mode(&mut self, dhcp: bool) {
        if dhcp == self.dhcp {
            return;
        }
        self.dhcp = dhcp;
        self.error = None;
        self.applied = None;
        self.focus = if dhcp { None } else { Some(0) };
        // To manual mode — with the addresses saved last time (if any).
        let saved = crate::fs::read(net::CONFIG_FILE).ok().map(|d| Config::parse(&String::from_utf8_lossy(&d)));
        if !dhcp && let Some(c) = saved.filter(|c| c.ip != [0; 4]) {
            self.fill(&c);
        }
        if !dhcp && self.fields[1].text.is_empty() {
            self.fields[1].set_text("255.255.255.0");
        }
    }

    /// Checks the fields. Empty gateway and DNS are allowed (DNS is then the gateway).
    fn parse(&self) -> Result<Config, (Option<usize>, &'static str)> {
        if self.dhcp {
            // Manual addresses are saved too — useful when switching back.
            let ip = |i: usize| net::parse_ip(self.fields[i].text.trim()).unwrap_or([0; 4]);
            return Ok(Config { dhcp: true, ip: ip(0), mask: ip(1), gw: ip(2), dns: ip(3) });
        }
        let field = |i: usize, optional: bool| -> Result<Ip, (Option<usize>, &'static str)> {
            let text = self.fields[i].text.trim();
            if text.is_empty() && optional {
                return Ok([0; 4]);
            }
            net::parse_ip(text).ok_or((Some(i), "Not a valid address"))
        };
        let ip = field(0, false)?;
        let mask = field(1, false)?;
        let gw = field(2, true)?;
        let dns = field(3, true)?;
        if ip == [0; 4] {
            return Err((Some(0), "Address can't be 0.0.0.0"));
        }
        // The mask is ones, then zeros.
        let m = u32::from_be_bytes(mask);
        if m == 0 || m.leading_ones() + m.trailing_zeros() != 32 {
            return Err((Some(1), "Not a valid subnet mask"));
        }
        let same_net = |a: Ip| (u32::from_be_bytes(a) & m) == (u32::from_be_bytes(ip) & m);
        if gw != [0; 4] && !same_net(gw) {
            return Err((Some(2), "Gateway is outside the subnet"));
        }
        Ok(Config { dhcp: false, ip, mask, gw, dns: if dns == [0; 4] { gw } else { dns } })
    }

    fn apply(&mut self) {
        if !self.present {
            return;
        }
        match self.parse() {
            Ok(cfg) => {
                self.error = None;
                self.applied = Some(if net::save_config(&cfg) { "Settings applied" } else { NOT_APPLIED });
                self.status = net::state_text();
            }
            Err(e) => {
                self.error = Some(e);
                self.applied = None;
                self.focus = e.0.or(self.focus);
            }
        }
    }

    fn identity_mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if !pressed || buttons & LEFT == 0 {
            let f = self.id_focus;
            let r = self.id_field(f);
            return self.id_fields[f].mouse(r, x, y, buttons, pressed);
        }
        if self.close_button().contains(x, y) {
            self.done = true;
        } else if self.apply_button().contains(x, y) {
            self.apply_identity();
        } else if self.visible_rect().contains(x, y) {
            self.visible = !self.visible;
            self.applied = None;
        } else if let Some(i) = (0..3).find(|&i| self.id_field(i).contains(x, y)) {
            self.id_focus = i;
            let r = self.id_field(i);
            self.id_fields[i].mouse(r, x, y, buttons, pressed);
        } else {
            return false;
        }
        true
    }

    fn draw_radio(c: &mut Canvas, r: Rect, on: bool, label: &str, enabled: bool) {
        let dot = Rect::new(r.x, r.y + r.h / 2 - 9, 18, 18);
        c.fill_round(dot, 9, 0x000000, 90);
        c.stroke_round(dot, 9, if on { ACCENT } else { 0xFFFFFF }, if on { 220 } else { 70 });
        if on {
            c.fill_round(dot.inset(5), 4, ACCENT, 256);
        }
        c.text(r.x + 30, r.y + 4, label, if enabled { TEXT } else { TEXT_DIM }, FontId::Regular, 14.0);
    }
}

impl App for NetSettings {
    fn background(&self) -> Color {
        BG
    }

    fn draw(&mut self, c: &mut Canvas, a: Rect, _ctx: &Ctx) {
        (self.cw, self.ch) = (a.w, a.h);
        let off = |r: Rect| r.offset(a.x, a.y);
        let bar = Rect::new(a.x, a.y, a.w, TAB_BAR_H);
        TabBar::fixed(bar, self.tabs().len()).draw(c, bar, self.tabs(), self.tab);
        if !self.embedded {
            ui::button(c, off(self.close_button()), "Close", true);
        }
        if self.tab == WIFI {
            self.draw_wifi(c, a);
            return;
        }
        if self.tab == 1 {
            ui::button(c, off(self.apply_button()), "Apply", true);
            self.draw_identity(c, a);
            return;
        }
        let a = Rect::new(a.x, a.y + TAB_BAR_H, a.w, a.h);
        c.text(a.x + PAD, a.y + 16, "Network adapter", TEXT, FontId::SemiBold, 14.0);
        let adapter = net::nic_name().unwrap_or("not found");
        let w = crate::font::measure(FontId::Regular, 13.0, adapter);
        c.text(a.x + a.w - PAD - w, a.y + 17, adapter, TEXT_DIM, FontId::Regular, 13.0);

        Self::draw_radio(c, self.radio(0).offset(a.x, a.y - TAB_BAR_H), self.dhcp, "Obtain an address automatically (DHCP)", self.present);
        Self::draw_radio(c, self.radio(1).offset(a.x, a.y - TAB_BAR_H), !self.dhcp, "Use the following addresses", self.present);

        for (i, label) in LABELS.iter().enumerate() {
            let r = self.field(i).offset(a.x, a.y - TAB_BAR_H);
            let editable = !self.dhcp && self.present;
            c.text(a.x + PAD, r.y + 6, label, if editable { TEXT } else { TEXT_DIM }, FontId::Regular, 14.0);
            let bad = self.error.is_some_and(|(f, _)| f == Some(i));
            let focused = editable && self.focus == Some(i);
            c.fill_round(r, 7, 0x000000, if editable { 90 } else { 40 });
            let (border, alpha) = if bad {
                (RED, 220)
            } else if focused {
                (ACCENT, 160)
            } else {
                (0xFFFFFF, 30)
            };
            c.stroke_round(r, 7, border, alpha);
            let hint = if i >= 2 && !self.dhcp { "optional" } else { "" };
            self.fields[i].draw(c, r, focused, self.cursor_on, hint, if editable { TEXT } else { TEXT_DIM });
        }

        // The status line: an error, "applied" or the network state.
        let y = a.y + FIELDS_Y - TAB_BAR_H + 4 * FIELD_STEP + 4;
        let (msg, color) = match (self.error, self.applied) {
            (Some((_, e)), _) => (e, RED),
            (None, Some(m)) => (m, GREEN),
            _ => ("", TEXT_DIM),
        };
        c.text(a.x + PAD, y, "Status", TEXT_DIM, FontId::Regular, 13.0);
        c.text(a.x + FIELD_X, y, &self.status, TEXT, FontId::Regular, 13.0);
        if !msg.is_empty() {
            c.text(a.x + PAD, y + 22, msg, color, FontId::Regular, 13.0);
        }
        ui::button(c, Rect::new(self.apply_button().x + a.x, self.apply_button().y + a.y - TAB_BAR_H, self.apply_button().w, ui::BUTTON_H), "Apply", self.present);
    }

    fn key(&mut self, ch: u8, _ctx: &Ctx) -> bool {
        self.cursor_on = true;
        if self.tab == WIFI {
            match ch {
                ESC => self.done = !self.embedded,
                b'\n' if self.can_connect() => self.wifi_connect(),
                _ if self.saved_view => return false,
                _ => return self.password.key(ch, |_| true),
            }
            return true;
        }
        if self.tab == 1 {
            match ch {
                ESC => self.done = !self.embedded,
                b'\n' => self.apply_identity(),
                b'\t' => self.id_focus = (self.id_focus + 1) % 3,
                _ => {
                    self.error = None;
                    self.applied = None;
                    return self.id_fields[self.id_focus].key(ch, |_| true);
                }
            }
            return true;
        }
        match ch {
            ESC => self.done = !self.embedded,
            b'\n' => self.apply(),
            b'\t' if !self.dhcp => self.focus = Some(self.focus.map_or(0, |f| (f + 1) % 4)),
            _ => {
                // Addresses: only digits and dots.
                let Some(f) = self.focus.filter(|_| !self.dhcp) else { return false };
                let before = self.fields[f].text.clone();
                if !self.fields[f].key(ch, |c| c.is_ascii_digit() || c == '.') {
                    return false;
                }
                if self.fields[f].text != before {
                    self.error = None;
                    self.applied = None;
                }
            }
        }
        true
    }

    fn mouse(&mut self, x: i32, y: i32, buttons: u8, pressed: bool) -> bool {
        if pressed && buttons & LEFT != 0 && let Some((i, _)) = self.bar().hit(x, y) {
            self.tab = i;
            self.error = None;
            self.applied = None;
            if i == WIFI {
                self.open_wifi();
            }
            return true;
        }
        if self.tab == 1 {
            return self.identity_mouse(x, y, buttons, pressed);
        }
        if self.tab == WIFI {
            return self.wifi_mouse(x, y, buttons, pressed);
        }
        if !pressed || buttons & LEFT == 0 {
            let Some(f) = self.focus else { return false };
            let r = self.field(f);
            return self.fields[f].mouse(r, x, y, buttons, pressed);
        }
        if self.close_button().contains(x, y) {
            self.done = true;
        } else if self.apply_button().contains(x, y) {
            self.apply();
        } else if !self.present {
            return false;
        } else if self.radio(0).contains(x, y) {
            self.set_mode(true);
        } else if self.radio(1).contains(x, y) {
            self.set_mode(false);
        } else if let Some(i) = (0..4).find(|&i| self.field(i).contains(x, y)) {
            if self.dhcp {
                self.set_mode(false);
            }
            self.focus = Some(i);
            let r = self.field(i);
            self.fields[i].mouse(r, x, y, buttons, pressed);
        } else {
            return false;
        }
        true
    }

    fn tick(&mut self, ctx: &Ctx) -> bool {
        let on = ctx.caret_on();
        let mut changed = on != self.cursor_on && !self.dhcp;
        self.cursor_on = on;
        // Every half second — the network state; in DHCP mode the addresses too (they may have just been assigned).
        if ctx.ticks >= self.checked + crate::interrupts::HZ / 2 {
            self.checked = ctx.ticks;
            changed |= self.refresh_wifi();
            // An adapter may have appeared while the window was open: Wi-Fi connected, USB plugged in.
            let present = net::config().is_some();
            changed |= present != self.present;
            self.present = present;
            let status = net::state_text();
            if status != self.status {
                self.status = status;
                changed = true;
            }
            if self.dhcp
                && let Some(cfg) = net::config().filter(|c| c.dhcp)
            {
                let before: alloc::vec::Vec<String> = self.fields.iter().map(|f| f.text.clone()).collect();
                self.fill(&cfg);
                changed |= self.fields.iter().zip(&before).any(|(f, b)| f.text != *b);
            }
        }
        changed
    }

    fn finished(&self) -> bool {
        self.done
    }
}
