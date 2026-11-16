//! Sound and network icons in the top bar: tooltips, the volume control and network details.

use super::{ACCENT, BLUR, DOCK_H, DOCK_MARGIN, SHADOW_PAD, TEXT, TEXT_DIM, TOPBAR_H, Wm};
use crate::input::{KEY_BRIGHTNESS_DOWN, KEY_BRIGHTNESS_UP, KEY_MUTE, KEY_NEXT_TRACK, KEY_PLAY_PAUSE, KEY_PREV_TRACK, KEY_STOP, KEY_VOLUME_DOWN, KEY_VOLUME_UP};
use crate::battery::{self, Battery};
use crate::font::{self, FontId};
use crate::gfx::{Canvas, Color, Rect};
use crate::interrupts::{self, HZ};
use crate::keyboard::{self, LAYOUTS};
use crate::theme;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Volume,
    Net,
    Battery,
}

/// How many ticks after hovering to show a tooltip.
const HOVER_DELAY: u64 = HZ / 3;
/// How often to poll the network.
const NET_POLL: u64 = HZ / 10;
/// How long the arrow stays lit after a packet (so a single packet is visible).
const NET_GLOW: u64 = HZ * 3 / 10;
const VOLUME_W: i32 = 56;
const VOLUME_H: i32 = 210;
const NET_W: i32 = 270;
/// Value indent in the network details.
const NET_VALUE_X: i32 = 112;
/// Colour of the wave on the Wi-Fi icon while connecting.
const CONNECTING: Color = 0xF2A93B;
/// How many packets per second count as real traffic rather than network background noise.
const BUSY_PACKETS: u64 = 50;
/// The wave on the Wi-Fi icon: empty, a dot and three arcs.
const WAVE_STEPS: u32 = 5;
const BATTERY_W: i32 = 62;
/// At this charge and not charging — a warning with the critical error sound.
const EMPTY_BATTERY: u32 = 5;
/// Below this charge the icon is red.
const LOW_BATTERY: u32 = 15;
const LANG_W: i32 = 220;
const LANG_ROW: i32 = 32;
const RED: Color = 0xE5484D;
const OSD_W: i32 = 280;
const OSD_H: i32 = 54;

/// What the on-screen indicator shows (after Fn keys).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Osd {
    Volume,
    Brightness,
}

#[derive(Default)]
pub struct Tray {
    /// The "Remote" label in the bar: there was a remote request recently.
    pub(super) remote: bool,
    /// The icon under the cursor and since which tick.
    hover: Option<(Item, u64)>,
    /// A tooltip (or network details) is visible.
    shown: bool,
    /// The volume control is open; its slider is being dragged.
    volume_open: bool,
    volume_drag: bool,
    /// Volume before muting (to restore it).
    unmute: u32,
    /// There is a network card / there is a link and an address.
    net_present: bool,
    net_online: bool,
    /// The Wi-Fi icon instead of the cable icon; the module is scanning or connecting.
    net_wifi: bool,
    net_connecting: bool,
    /// How many parts of the Wi-Fi icon are lit, from the dot to the outer arc.
    wave: u32,
    /// Real traffic is flowing; when and at which packet counter this was checked.
    net_busy: bool,
    traffic_at: (u64, u64),
    rx: u64,
    tx: u64,
    /// Packets went recently — the arrow is lit.
    rx_lit: bool,
    tx_lit: bool,
    /// Tick of the last received / sent packet.
    rx_at: u64,
    tx_at: u64,
    net_checked: u64,
    /// Lines of the network details window: (name, value).
    net_rows: Vec<(&'static str, String)>,
    /// The on-screen volume/brightness indicator and until which tick it is visible.
    osd: Option<(Osd, u64)>,
    /// The language list is open, and the line under the cursor.
    lang_open: bool,
    lang_hover: Option<usize>,
    battery_seen: u64,
    /// The almost-empty battery warning has already been shown.
    battery_warned: bool,
    /// The Bluetooth icon and menu.
    pub(super) bt: super::btmenu::BtMenu,
}

impl Wm {
    pub(super) fn volume_icon(&self) -> Rect {
        let n = self.bt_icon();
        Rect::new(n.x - 32, n.y, 28, 26)
    }

    pub(super) fn net_icon(&self) -> Rect {
        let b = self.bell_rect();
        Rect::new(b.x - 32, b.y, 28, 26)
    }

    /// The battery icon: the laptop's or (on a desktop) the most discharged device's —
    /// keyboard, mouse. When there are no batteries, there is no icon.
    fn battery_icon(&self) -> Rect {
        let v = self.volume_icon();
        let w = if shown_battery().is_some() { BATTERY_W } else { 0 };
        Rect::new(v.x - w - 4, v.y, w, 26)
    }

    /// The language indicator. With a single language there is nothing to switch, and it is hidden.
    pub(super) fn lang_icon(&self) -> Rect {
        let v = self.battery_icon();
        let w = if keyboard::enabled().len() > 1 { 34 } else { 0 };
        Rect::new(v.x - w - 4, v.y, w, 26)
    }

    /// How many lines are in the list: languages and the link to settings.
    fn lang_rows() -> i32 {
        keyboard::enabled().len() as i32 + 1
    }

    fn lang_rect(&self) -> Rect {
        let i = self.lang_icon();
        Rect::new((i.x + i.w / 2 - LANG_W / 2).min(self.right() - LANG_W - 8), self.top() + TOPBAR_H + 6, LANG_W, Self::lang_rows() * LANG_ROW + 19)
    }

    fn lang_row(&self, i: i32) -> Rect {
        let r = self.lang_rect();
        let gap = if i == Self::lang_rows() - 1 { 9 } else { 0 };
        Rect::new(r.x + 6, r.y + 5 + i * LANG_ROW + gap, r.w - 12, LANG_ROW)
    }

    fn volume_rect(&self) -> Rect {
        let v = self.volume_icon();
        Rect::new(v.x + v.w / 2 - VOLUME_W / 2, self.top() + TOPBAR_H + 6, VOLUME_W, VOLUME_H)
    }

    /// The slider's vertical track (bottom — silence, top — maximum).
    fn volume_track(&self) -> Rect {
        let r = self.volume_rect();
        Rect::new(r.x + r.w / 2 - 12, r.y + 40, 24, r.h - 90)
    }

    fn mute_button(&self) -> Rect {
        let r = self.volume_rect();
        Rect::new(r.x + 8, r.y + r.h - 40, r.w - 16, 30)
    }

    /// The icon's tooltip: the laptop battery and below it the devices, one per line.
    fn battery_text(&self) -> String {
        let mut lines = Vec::new();
        if let Some(b) = battery::get() {
            lines.push(match (b.charging, b.on_ac, b.minutes) {
                (true, _, _) => format!("Battery: {}%, charging", b.percent),
                (_, true, _) => format!("Battery: {}%, on AC power", b.percent),
                (_, _, Some(m)) => format!("Battery: {}%, about {}h {:02}m left", b.percent, m / 60, m % 60),
                _ => format!("Battery: {}%", b.percent),
            });
        }
        for d in battery::devices() {
            lines.push(format!("{} ({}): {}%", d.name, d.kind, d.percent));
        }
        lines.join("\n")
    }

    fn tooltip_text(&self) -> String {
        if matches!(self.tray.hover, Some((Item::Battery, _))) {
            return self.battery_text();
        }
        if !crate::audio::available() {
            return "No sound card".into();
        }
        let level = match theme::get().sound {
            0 => "Volume: muted".into(),
            v => format!("Volume: {}%", v * 100 / 256),
        };
        match crate::audio::output_name() {
            Some(out) => format!("{level} \u{00B7} {out}"),
            None => level,
        }
    }

    fn tooltip_rect(&self) -> Rect {
        let v = if matches!(self.tray.hover, Some((Item::Battery, _))) { self.battery_icon() } else { self.volume_icon() };
        let text = self.tooltip_text();
        let w = text.lines().map(|l| font::measure(FontId::Regular, 13.0, l)).max().unwrap_or(0) + 20;
        let h = 28 + (text.lines().count().max(1) as i32 - 1) * TOOLTIP_LINE;
        Rect::new((v.x + v.w / 2 - w / 2).min(self.right() - w - 8), self.top() + TOPBAR_H + 6, w, h)
    }

    fn net_rect(&self) -> Rect {
        let n = self.net_icon();
        let h = 52 + self.tray.net_rows.len() as i32 * 22;
        let widest = self.tray.net_rows.iter().map(|(_, v)| font::measure(FontId::Regular, 13.0, v)).max().unwrap_or(0);
        let w = NET_W.max(NET_VALUE_X + widest + 16);
        Rect::new((n.x + n.w / 2 - w / 2).min(self.right() - w - 8), self.top() + TOPBAR_H + 6, w, h)
    }

    fn osd_rect(&self) -> Rect {
        let home = self.home;
        Rect::new(home.x + (home.w - OSD_W) / 2, home.y + home.h - DOCK_H - DOCK_MARGIN - OSD_H - 28, OSD_W, OSD_H)
    }

    fn show_osd(&mut self, kind: Osd) {
        self.mark(self.osd_rect().inset(-SHADOW_PAD));
        self.tray.osd = Some((kind, interrupts::ticks() + HZ * 3 / 2));
    }

    /// Media keys. true — the key was handled.
    pub(super) fn media_key(&mut self, ch: u8) -> bool {
        match ch {
            KEY_VOLUME_UP | KEY_VOLUME_DOWN => {
                let step = if ch == KEY_VOLUME_UP { 16 } else { -16 };
                let v = (theme::get().sound as i32 + step).clamp(0, 256) as u32;
                self.set_volume(v);
                crate::audio::set_volume(v as i32);
                crate::audio::beep(660, 40);
                self.show_osd(Osd::Volume);
            }
            KEY_MUTE => {
                self.toggle_mute();
                self.show_osd(Osd::Volume);
            }
            KEY_PLAY_PAUSE | KEY_NEXT_TRACK | KEY_PREV_TRACK | KEY_STOP => {
                use crate::audio::player::{Media, media};
                media(match ch {
                    KEY_PLAY_PAUSE => Media::Toggle,
                    KEY_NEXT_TRACK => Media::Next,
                    KEY_PREV_TRACK => Media::Previous,
                    _ => Media::Stop,
                });
            }
            KEY_BRIGHTNESS_UP | KEY_BRIGHTNESS_DOWN => {
                if let Some(now) = crate::backlight::get() {
                    let now = match theme::get().brightness {
                        0 => now,
                        b => b,
                    };
                    let step = if ch == KEY_BRIGHTNESS_UP { 10 } else { -10 };
                    let b = (now as i32 + step).clamp(crate::backlight::MIN as i32, 100) as u32;
                    theme::update(|t| t.brightness = b);
                    crate::backlight::set(b);
                    self.show_osd(Osd::Brightness);
                }
            }
            _ => return false,
        }
        true
    }

    /// What has popped up under the icons now (for blur and repaint).
    pub(super) fn tray_popups(&self) -> Vec<Rect> {
        let mut out = Vec::new();
        if self.tray.osd.is_some() {
            out.push(self.osd_rect());
        }
        if self.tray.volume_open {
            out.push(self.volume_rect());
        }
        if self.tray.lang_open {
            out.push(self.lang_rect());
        }
        if self.bt_menu_open() {
            out.push(self.bt_menu_rect());
        }
        out.extend(self.calendar_rect());
        if self.tray.shown {
            match self.tray.hover {
                Some((Item::Volume, _)) if !self.tray.volume_open => out.push(self.tooltip_rect()),
                Some((Item::Battery, _)) => out.push(self.tooltip_rect()),
                Some((Item::Net, _)) => out.push(self.net_rect()),
                _ => {}
            }
        }
        out
    }

    pub(super) fn mark_tray(&mut self) {
        for r in self.tray_popups() {
            self.mark(r.inset(-SHADOW_PAD));
        }
        self.mark_bars();
    }

    /// The cursor moved: track which icon it is over.
    pub(super) fn tray_hover(&mut self, x: i32, y: i32) {
        self.bt_hover(x, y);
        if self.tray.lang_open {
            let row = (0..Self::lang_rows()).find(|&i| self.lang_row(i).contains(x, y)).map(|i| i as usize);
            if row != self.tray.lang_hover {
                self.tray.lang_hover = row;
                self.mark(self.lang_rect());
            }
        }
        let now = if self.volume_icon().contains(x, y) {
            Some(Item::Volume)
        } else if self.net_icon().contains(x, y) {
            Some(Item::Net)
        } else if self.battery_icon().contains(x, y) {
            Some(Item::Battery)
        } else {
            None
        };
        if now != self.tray.hover.map(|(i, _)| i) {
            self.mark_tray();
            self.tray.hover = now.map(|i| (i, interrupts::ticks()));
            self.tray.shown = false;
        }
    }

    pub(super) fn tray_tick(&mut self, ticks: u64) {
        self.bt_tick(ticks);
        // Laptop brightness keys delivered through firmware: like regular keys.
        if let (Some(want), Some(now)) = (crate::backlight::requested(), crate::backlight::get()) {
            if want != now {
                self.media_key(if want > now { KEY_BRIGHTNESS_UP } else { KEY_BRIGHTNESS_DOWN });
            }
        }
        if self.tray.osd.is_some_and(|(_, until)| ticks >= until) {
            self.mark(self.osd_rect().inset(-SHADOW_PAD));
            self.tray.osd = None;
        }
        if let Some((item, since)) = self.tray.hover
            && !self.tray.shown
            && ticks >= since.saturating_add(HOVER_DELAY)
            && self.buttons == 0
        {
            if item == Item::Net {
                self.tray.net_rows = net_rows();
            }
            self.tray.shown = true;
            self.mark_tray();
        }
        if battery::version() != self.tray.battery_seen {
            self.tray.battery_seen = battery::version();
            self.mark_tray();
            let empty = battery::get().is_some_and(|b| b.percent <= EMPTY_BATTERY && !b.on_ac);
            if empty && !self.tray.battery_warned {
                super::notify_with(Some(crate::audio::Sound::Critical), "Battery almost empty", "Connect the charger");
            }
            self.tray.battery_warned = empty;
        }
        if ticks >= self.tray.net_checked + NET_POLL {
            self.tray.net_checked = ticks;
            self.poll_net(ticks);
            let remote = crate::net::remote::active();
            if remote != self.tray.remote {
                self.tray.remote = remote;
                self.mark_bars();
            }
        }
    }

    fn poll_net(&mut self, ticks: u64) {
        let t = &mut self.tray;
        let (present, online, rx, tx) = match crate::net::status() {
            Some((online, rx, tx)) => (true, online, rx, tx),
            None => (false, false, 0, 0),
        };
        if rx != t.rx {
            t.rx_at = ticks;
        }
        if tx != t.tx {
            t.tx_at = ticks;
        }
        let wifi = crate::net::over_wifi();
        let lit = |at: u64| !wifi && at > 0 && ticks < at + NET_GLOW;
        let (rx_lit, tx_lit) = (lit(t.rx_at), lit(t.tx_at));
        let connecting = wifi && !online && crate::wifi::busy();
        // Once a second: whether real traffic is flowing. Network background noise does not start the wave,
        // otherwise the CPU would wake up ten times a second for an icon.
        if ticks >= t.traffic_at.0 + HZ {
            t.net_busy = (rx + tx).saturating_sub(t.traffic_at.1) >= BUSY_PACKETS;
            t.traffic_at = (ticks, rx + tx);
        }
        // A started wave reaches the outer arc even if the traffic has already ended.
        let wave = if wifi && (connecting || t.net_busy || t.wave != 0) { (t.wave + 1) % WAVE_STEPS } else { 0 };
        let changed = (present, online, rx_lit, tx_lit, wifi, connecting, wave) != (t.net_present, t.net_online, t.rx_lit, t.tx_lit, t.net_wifi, t.net_connecting, t.wave);
        (t.net_present, t.net_online, t.rx, t.tx, t.rx_lit, t.tx_lit) = (present, online, rx, tx, rx_lit, tx_lit);
        (t.net_wifi, t.net_connecting, t.wave) = (wifi, connecting, wave);
        let showing = t.shown && matches!(t.hover, Some((Item::Net, _)));
        self.net_changed |= changed;
        // The details window is refreshed once a second (an address may have arrived over DHCP).
        if showing && ticks % HZ < NET_POLL {
            let rows = net_rows();
            if rows != self.tray.net_rows {
                self.mark(self.net_rect().inset(-SHADOW_PAD));
                self.tray.net_rows = rows;
                self.mark(self.net_rect().inset(-SHADOW_PAD));
            }
        }
    }

    fn set_volume(&mut self, v: u32) {
        theme::update(|t| t.sound = v.min(256));
        self.mark_tray();
    }

    fn set_volume_at(&mut self, y: i32) {
        let t = self.volume_track();
        let v = (t.y + t.h - y).clamp(0, t.h) as u32 * 256 / t.h as u32;
        self.set_volume(v);
    }

    /// Mouse buttons (called before other handling). true — the event was handled here.
    pub(super) fn tray_mouse(&mut self, x: i32, y: i32, pressed: bool, released: bool) -> bool {
        if self.tray.volume_drag {
            self.set_volume_at(y);
            if released {
                self.tray.volume_drag = false;
                crate::audio::beep(660, 70);
            }
            return true;
        }
        if !pressed {
            return false;
        }
        if self.bt_mouse(x, y) {
            return true;
        }
        if self.tray.lang_open {
            let row = (0..Self::lang_rows()).find(|&i| self.lang_row(i).contains(x, y));
            let on_icon = self.lang_icon().contains(x, y);
            self.mark_tray();
            self.tray.lang_open = false;
            let languages = keyboard::enabled();
            match row.map(|i| i as usize) {
                Some(i) if i < languages.len() => keyboard::select(languages[i]),
                Some(_) => self.open_settings("keyboard", None),
                None => return on_icon,
            }
            return true;
        }
        if self.lang_icon().contains(x, y) {
            self.close_menu();
            self.close_volume();
            self.close_bt_menu();
            if self.center.is_some() {
                self.toggle_center();
            }
            self.mark_tray();
            self.tray.lang_open = true;
            self.tray.lang_hover = None;
            return true;
        }
        if self.tray.volume_open {
            if self.volume_rect().contains(x, y) {
                if self.mute_button().contains(x, y) {
                    self.toggle_mute();
                } else if self.volume_track().inset(-6).contains(x, y) {
                    self.tray.volume_drag = true;
                    self.set_volume_at(y);
                }
                return true;
            }
            self.mark_tray();
            self.tray.volume_open = false;
            // A click on the icon only closes the control.
            return self.volume_icon().contains(x, y);
        }
        if self.volume_icon().contains(x, y) {
            self.close_menu();
            self.close_bt_menu();
            if self.center.is_some() {
                self.toggle_center();
            }
            self.mark_tray();
            self.tray.volume_open = true;
            if self.tray.unmute == 0 {
                self.tray.unmute = 200;
            }
            return true;
        }
        if self.net_icon().contains(x, y) {
            // A click is a shortcut to the "Network" section in Settings (an already open window switches).
            self.mark_tray();
            // Until the cursor leaves the icon, the details do not pop up again.
            self.tray.shown = false;
            self.tray.hover = Some((Item::Net, u64::MAX));
            self.open_settings(if crate::net::over_wifi() { "network:wifi" } else { "network" }, None);
            return true;
        }
        if self.battery_icon().contains(x, y) {
            // A click shows battery details in the Power section.
            self.mark_tray();
            self.tray.shown = false;
            self.tray.hover = Some((Item::Battery, u64::MAX));
            self.open_settings("power", None);
            return true;
        }
        false
    }

    pub(super) fn close_volume(&mut self) {
        if self.tray.volume_open || self.tray.lang_open {
            self.mark_tray();
            self.tray.volume_open = false;
            self.tray.lang_open = false;
        }
    }

    fn toggle_mute(&mut self) {
        let v = theme::get().sound;
        if v == 0 {
            let back = if self.tray.unmute == 0 { 200 } else { self.tray.unmute };
            self.set_volume(back);
        } else {
            self.tray.unmute = v;
            self.set_volume(0);
        }
    }

    /// The wheel over the volume icon or over the control. true — handled.
    pub(super) fn tray_wheel(&mut self, x: i32, y: i32, clicks: i32) -> bool {
        let over = self.volume_icon().contains(x, y) || (self.tray.volume_open && self.volume_rect().contains(x, y));
        if !over {
            return false;
        }
        // Wheel down (clicks > 0) — quieter.
        let v = theme::get().sound as i32 - clicks * 16;
        self.set_volume(v.clamp(0, 256) as u32);
        true
    }

    // ------------------------------------------------------------ drawing

    pub(super) fn draw_tray_icons(&self, c: &mut Canvas) {
        if let Some(b) = shown_battery() {
            draw_battery(c, self.battery_icon(), &b);
        }
        let l = self.lang_icon();
        if !l.is_empty() {
            if self.tray.lang_open {
                c.fill_round(l, 7, 0xFFFFFF, 40);
            }
            let code = LAYOUTS[keyboard::active()].code;
            let w = font::measure(FontId::SemiBold, 13.0, code);
            c.text(l.x + (l.w - w) / 2, l.y + 4, code, TEXT, FontId::SemiBold, 13.0);
        }
        // Caps Lock on: a bar under the indicator, and a dot with a single language.
        if keyboard::caps() {
            let w = l.w.max(6) - 12;
            c.fill_round(Rect::new(l.x + 6 - if l.w == 0 { 10 } else { 0 }, l.y + 21, w.max(6), 2), 1, ACCENT, 256);
        }
        let v = self.volume_icon();
        if self.tray.volume_open {
            c.fill_round(v, 7, 0xFFFFFF, 40);
        }
        let sound = theme::get().sound;
        let level = match sound {
            0 => 0,
            1..=85 => 1,
            86..=170 => 2,
            _ => 3,
        };
        let muted = sound == 0 || !crate::audio::available();
        speaker(c, v.x + 5, v.y + 6, if muted { TEXT_DIM } else { TEXT }, level, muted);
        self.draw_bt_icon(c);

        let n = self.net_icon();
        let t = &self.tray;
        if t.net_wifi {
            // Connecting — an amber wave over a dim icon, traffic — a light blue one over a bright icon.
            let (color, lit) = if t.net_online { (TEXT, ACCENT) } else { (TEXT_DIM, CONNECTING) };
            wifi_icon(c, n, color, lit, t.wave);
            if !t.net_online && !t.net_connecting {
                cross(c, n.x + 17, n.y + 14);
            }
            return;
        }
        let base = if t.net_online { TEXT } else { TEXT_DIM };
        // The screens flash on traffic: the back one for receive, the front one for send.
        let glow = |lit: bool| if t.net_online && lit { Some(ACCENT) } else { None };
        monitors(c, n.x + 2, n.y + 2, base, glow(t.rx_lit), glow(t.tx_lit));
        if !t.net_online {
            cross(c, n.x + 17, n.y + 14);
        }
    }

    pub(super) fn draw_tray_popups(&self, c: &mut Canvas) {
        if let Some((kind, _)) = self.tray.osd {
            self.draw_osd(c, kind);
        }
        if self.tray.volume_open {
            self.draw_volume(c);
        }
        if self.tray.lang_open {
            self.draw_languages(c);
        }
        self.draw_bt_menu(c);
        self.draw_calendar(c);
        if !self.tray.shown {
            return;
        }
        match self.tray.hover {
            Some((Item::Volume | Item::Battery, _)) if !self.tray.volume_open => {
                let r = self.tooltip_rect();
                panel(c, r, 8);
                for (i, line) in self.tooltip_text().lines().enumerate() {
                    c.text(r.x + 10, r.y + 6 + i as i32 * TOOLTIP_LINE, line, TEXT, FontId::Regular, 13.0);
                }
            }
            Some((Item::Net, _)) => self.draw_net(c),
            _ => {}
        }
    }

    fn draw_languages(&self, c: &mut Canvas) {
        let r = self.lang_rect();
        panel(c, r, 12);
        let active = keyboard::active();
        let languages = keyboard::enabled();
        for (i, &l) in languages.iter().enumerate() {
            let row = self.lang_row(i as i32);
            if self.tray.lang_hover == Some(i) {
                c.fill_round(row, 7, 0x3B82F6, 220);
            }
            c.text(row.x + 10, row.y + 7, LAYOUTS[l].code, if l == active { ACCENT } else { TEXT_DIM }, FontId::SemiBold, 13.0);
            c.text(row.x + 44, row.y + 7, LAYOUTS[l].name, TEXT, FontId::Regular, 14.0);
        }
        let row = self.lang_row(languages.len() as i32);
        c.fill(Rect::new(row.x + 8, row.y - 6, row.w - 16, 1), 0x3A3F55);
        if self.tray.lang_hover == Some(languages.len()) {
            c.fill_round(row, 7, 0x3B82F6, 220);
        }
        c.text(row.x + 10, row.y + 7, "Keyboard settings\u{2026}", TEXT, FontId::Regular, 14.0);
    }

    fn draw_volume(&self, c: &mut Canvas) {
        let r = self.volume_rect();
        panel(c, r, 14);
        let sound = theme::get().sound;
        let label = if sound == 0 { "Off".into() } else { format!("{}%", sound * 100 / 256) };
        let w = font::measure(FontId::SemiBold, 13.0, &label);
        c.text(r.x + (r.w - w) / 2, r.y + 12, &label, TEXT, FontId::SemiBold, 13.0);

        let t = self.volume_track();
        let line = Rect::new(t.x + t.w / 2 - 3, t.y, 6, t.h);
        c.fill_round(line, 3, 0x3A3F55, 256);
        let fill = (sound as i32 * t.h / 256).clamp(0, t.h);
        let knob_y = t.y + t.h - fill;
        c.fill_round(Rect::new(line.x, knob_y, line.w, fill), 3, ACCENT, 256);
        let knob = Rect::new(t.x + t.w / 2 - 9, knob_y - 9, 18, 18);
        c.fill_round(knob.offset(0, 1), 9, 0x000000, 90);
        c.fill_round(knob, 9, 0xFFFFFF, 256);

        let m = self.mute_button();
        c.fill_round(m, 8, 0xFFFFFF, if sound == 0 { 40 } else { 18 });
        let color = if sound == 0 { TEXT_DIM } else { TEXT };
        speaker(c, m.x + m.w / 2 - 9, m.y + 7, color, 0, sound == 0);
    }

    /// The indicator above the dock: icon, bar and percent.
    fn draw_osd(&self, c: &mut Canvas, kind: Osd) {
        let r = self.osd_rect();
        panel(c, r, 16);
        let (value, muted) = match kind {
            Osd::Volume => {
                let s = theme::get().sound;
                (s * 100 / 256, s == 0)
            }
            Osd::Brightness => (crate::backlight::get().unwrap_or(0), false),
        };
        let icon_y = r.y + (r.h - 16) / 2;
        match kind {
            Osd::Volume => {
                let level = if muted { 0 } else { 1 + (value * 3 / 101) };
                speaker(c, r.x + 18, icon_y, if muted { TEXT_DIM } else { TEXT }, level, muted);
            }
            Osd::Brightness => sun(c, r.x + 26, icon_y + 8, TEXT),
        }
        let bar = Rect::new(r.x + 54, r.y + r.h / 2 - 3, r.w - 54 - 62, 6);
        c.fill_round(bar, 3, 0xFFFFFF, 40);
        let w = bar.w * value as i32 / 100;
        if w > 0 {
            c.fill_round(Rect::new(bar.x, bar.y, w.max(6), bar.h), 3, ACCENT, 256);
        }
        let label = if muted { "Off".into() } else { format!("{value}%") };
        let lw = font::measure(FontId::SemiBold, 14.0, &label);
        c.text(r.x + r.w - 18 - lw, r.y + (r.h - 18) / 2, &label, TEXT, FontId::SemiBold, 14.0);
    }

    fn draw_net(&self, c: &mut Canvas) {
        let r = self.net_rect();
        panel(c, r, 14);
        c.text(r.x + 16, r.y + 14, "Network", TEXT, FontId::SemiBold, 15.0);
        for (i, (name, value)) in self.tray.net_rows.iter().enumerate() {
            let y = r.y + 44 + i as i32 * 22;
            c.text(r.x + 16, y, name, TEXT_DIM, FontId::Regular, 13.0);
            c.text(r.x + NET_VALUE_X, y, value, TEXT, FontId::Regular, 13.0);
        }
    }
}

/// A tooltip line in a multi-line tooltip: the line step.
const TOOLTIP_LINE: i32 = 18;

/// What the icon shows: the laptop battery, otherwise the most discharged device.
fn shown_battery() -> Option<Battery> {
    battery::get().or_else(|| {
        battery::devices().iter().map(|d| d.percent).min().map(|percent| Battery { percent, charging: false, on_ac: false, minutes: None })
    })
}

/// Battery icon: a body with the charge level, the percent next to it.
fn draw_battery(c: &mut Canvas, r: Rect, b: &Battery) {
    let body = Rect::new(r.x + 2, r.y + 7, 22, 12);
    let color = if b.percent <= LOW_BATTERY && !b.on_ac { RED } else if b.charging { ACCENT } else { TEXT };
    c.stroke_round(body, 3, TEXT, 220);
    c.fill_round(Rect::new(body.x + body.w, body.y + 3, 2, 6), 1, TEXT, 220);
    let inner = body.inset(2);
    let fill = (inner.w * b.percent as i32 / 100).max(1);
    c.fill_round(Rect::new(inner.x, inner.y, fill, inner.h), 1, color, 256);
    let text = format!("{}%", b.percent);
    let w = font::measure(FontId::Regular, 13.0, &text);
    c.text(r.x + r.w - w, r.y + 5, &text, TEXT, FontId::Regular, 13.0);
}

/// Network details lines — for the connection that works now (like the icon):
/// Wi-Fi with the network and rate, or the cable with the link speed, and its addresses.
pub fn net_rows() -> Vec<(&'static str, String)> {
    let mut rows = alloc::vec![("Status", crate::net::state_text())];
    let wifi = crate::net::over_wifi();
    if let Some(name) = crate::wifi::network().filter(|_| wifi) {
        rows.push(("Wi-Fi", name));
        rows.extend(crate::wifi::link_mode().map(|mode| ("Mode", mode)));
        if crate::wifi::speed() != 0 {
            rows.push(("Link rate", format!("{} Mbit/s", crate::wifi::speed())));
        }
    }
    let Some(info) = crate::net::info() else { return rows };
    if let Some((mbit, full)) = info.speed.filter(|_| !wifi) {
        rows.push(("Link rate", format!("{mbit} Mbit/s, {} duplex", if full { "full" } else { "half" })));
    }
    let ip = crate::net::ip_str;
    if info.ip != [0; 4] {
        rows.push(("IP address", ip(info.ip)));
        rows.push(("Subnet mask", ip(info.mask)));
        rows.push(("Gateway", ip(info.gw)));
        rows.push(("DNS", ip(info.dns)));
    }
    rows.push(("MAC", info.mac));
    rows
}

/// Glass backing of a popup panel.
pub(super) fn panel(c: &mut Canvas, r: Rect, radius: i32) {
    c.shadow(r, radius, 22, 6, 150);
    c.blur_round(r, BLUR, radius);
    c.fill_round(r, radius, 0x161A2C, crate::theme::get().panel_alpha);
    c.stroke_round(r, radius, 0xFFFFFF, 50);
}

/// A pixel with transparency (for anti-aliased arcs).
fn dot(c: &mut Canvas, x: i32, y: i32, color: Color, alpha: u32) {
    if alpha > 0 {
        c.blend(Rect::new(x, y, 1, 1), color, alpha.min(256));
    }
}

/// An 18×16 speaker at point (x, y): body, horn and `level` waves (or a cross).
fn speaker(c: &mut Canvas, x: i32, y: i32, color: Color, level: u32, muted: bool) {
    c.fill_round(Rect::new(x, y + 5, 4, 6), 1, color, 256);
    // The horn: columns growing to the right.
    for i in 0..5 {
        let h = 6 + i * 2;
        c.fill(Rect::new(x + 4 + i, y + 8 - h / 2, 1, h), color);
    }
    if muted {
        cross_lines(c, x + 13, y + 8, 3, color);
        return;
    }
    // Waves are arcs around the horn's centre.
    let (cx, cy) = (x as f32 + 6.0, y as f32 + 8.0);
    for k in 0..level {
        let radius = 6.0 + k as f32 * 3.5;
        let reach = radius as i32 + 2;
        for py in (cy as i32 - reach)..=(cy as i32 + reach) {
            for px in (cx as i32 + 3)..=(cx as i32 + reach) {
                let (dx, dy) = (px as f32 + 0.5 - cx, py as f32 + 0.5 - cy);
                if dy.abs() > dx * 1.1 {
                    continue;
                }
                let d = libm::sqrtf(dx * dx + dy * dy);
                let cover = 1.0 - ((d - radius).abs() - 0.6).max(0.0) / 0.8;
                dot(c, px, py, color, (cover.clamp(0.0, 1.0) * 256.0) as u32);
            }
        }
    }
}

/// A diagonal cross centred at (cx, cy) with half-span `s`.
fn cross_lines(c: &mut Canvas, cx: i32, cy: i32, s: i32, color: Color) {
    for i in -s..=s {
        c.fill(Rect::new(cx + i, cy + i, 2, 2), color);
        c.fill(Rect::new(cx + i, cy - i, 2, 2), color);
    }
}

/// A red circle with a cross — no network.
pub(super) fn cross(c: &mut Canvas, x: i32, y: i32) {
    c.fill_round(Rect::new(x - 1, y - 1, 12, 12), 6, 0x0B0D17, 200);
    c.fill_round(Rect::new(x, y, 10, 10), 5, RED, 256);
    for i in 0..5 {
        c.fill(Rect::new(x + 2 + i, y + 2 + i, 2, 1), 0xFFFFFF);
        c.fill(Rect::new(x + 6 - i, y + 2 + i, 2, 1), 0xFFFFFF);
    }
}

/// The sun (brightness): a circle and eight rays around the centre (cx, cy).
fn sun(c: &mut Canvas, cx: i32, cy: i32, color: Color) {
    c.fill_round(Rect::new(cx - 4, cy - 4, 9, 9), 4, color, 256);
    for (dx, dy) in [(0, -8), (0, 7), (-8, 0), (7, 0)] {
        c.fill(Rect::new(cx + dx, cy + dy, if dx == 0 { 2 } else { 3 }, if dx == 0 { 3 } else { 2 }), color);
    }
    for (dx, dy) in [(-6, -6), (5, -6), (-6, 5), (5, 5)] {
        c.fill(Rect::new(cx + dx, cy + dy, 2, 2), color);
    }
}

/// The Wi-Fi icon: a dot and three arcs above it. The first `lit` parts, counting from the dot, use colour `glow`.
fn wifi_icon(c: &mut Canvas, r: Rect, color: Color, glow: Color, lit: u32) {
    let (cx, cy) = (r.x + r.w / 2, r.y + 21);
    let shade = |part: u32| if part < lit { glow } else { color };
    c.fill_round(Rect::new(cx - 2, cy - 2, 4, 4), 2, shade(0), 256);
    for (part, radius) in [(1, 6), (2, 11), (3, 16)] {
        c.arc_up(cx, cy, radius, 2, shade(part), 256);
    }
}

/// A monitor: a 13×10 frame, a screen (dark or lit), a stand.
fn monitor(c: &mut Canvas, x: i32, y: i32, color: Color, glow: Option<Color>) {
    c.fill_round(Rect::new(x, y, 13, 10), 2, color, 256);
    match glow {
        Some(g) => c.fill(Rect::new(x + 2, y + 2, 9, 6), g),
        None => c.fill(Rect::new(x + 2, y + 2, 9, 6), 0x161A2C),
    }
    c.fill(Rect::new(x + 5, y + 10, 3, 2), color);
    c.fill_round(Rect::new(x + 3, y + 12, 7, 2), 1, color, 256);
}

/// The network icon: two monitors in a cascade connected by a cable.
fn monitors(c: &mut Canvas, x: i32, y: i32, color: Color, back: Option<Color>, front: Option<Color>) {
    // The cable: from the back one's stand down and to the right to the front one.
    c.fill(Rect::new(x + 2, y + 10, 2, 9), color);
    c.fill(Rect::new(x + 2, y + 17, 9, 2), color);
    monitor(c, x, y, color, back);
    // The front one covers the back one's corner: under it is a "cut-out" in the panel colour.
    c.fill_round(Rect::new(x + 9, y + 7, 15, 12), 3, 0x0B0D17, 256);
    monitor(c, x + 10, y + 8, color, front);
}

