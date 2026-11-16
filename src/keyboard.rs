//! Input languages. Keyboard drivers produce US layout characters, and here, in one
//! place, they become the characters of the chosen language: this way PS/2 and USB behave
//! the same. Caps Lock and the language switch keys live here too.
//!
//! Each user has their own settings: the keyboard.cfg file in their `.config`.

use crate::input::{Event, KEY_CAPS, KEY_MOD_UP, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
use crate::sync::IrqLock;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

pub const CONFIG: &str = "keyboard.cfg";

/// The US layout keys. The layout strings go in the same order, character for character.
const US_LOWER: &str = "`1234567890-=qwertyuiop[]\\asdfghjkl;'zxcvbnm,./";
const US_UPPER: &str = "~!@#$%^&*()_+QWERTYUIOP{}|ASDFGHJKL:\"ZXCVBNM<>?";

pub struct Layout {
    pub id: &'static str,
    /// What the indicator in the panel shows.
    pub code: &'static str,
    pub name: &'static str,
    lower: &'static str,
    upper: &'static str,
}

pub const LAYOUTS: [Layout; 4] = [
    Layout { id: "en", code: "EN", name: "English", lower: US_LOWER, upper: US_UPPER },
    Layout {
        id: "ru",
        code: "RU",
        name: "Русский",
        lower: "ё1234567890-=йцукенгшщзхъ\\фывапролджэячсмитьбю.",
        upper: "Ё!\"№;%:?*()_+ЙЦУКЕНГШЩЗХЪ/ФЫВАПРОЛДЖЭЯЧСМИТЬБЮ,",
    },
    Layout {
        id: "uk",
        code: "UK",
        name: "Українська",
        lower: "'1234567890-=йцукенгшщзхїґфівапролджєячсмитьбю.",
        // The fonts have no hryvnia sign, so Shift+` gives ~.
        upper: "~!\"№;%:?*()_+ЙЦУКЕНГШЩЗХЇҐФІВАПРОЛДЖЄЯЧСМИТЬБЮ,",
    },
    Layout {
        id: "be",
        code: "BE",
        name: "Беларуская",
        lower: "ё1234567890-=йцукенгшўзх'\\фывапролджэячсмітьбю.",
        upper: "Ё!\"№;%:?*()_+ЙЦУКЕНГШЎЗХ'/ФЫВАПРОЛДЖЭЯЧСМІТЬБЮ,",
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    AltShift,
    CtrlShift,
    WinSpace,
    CtrlSpace,
    CapsLock,
}

/// Switching methods: the value, the name in the settings and in the file.
pub const SWITCHES: [(Switch, &str, &str); 5] = [
    (Switch::AltShift, "Alt+Shift", "alt-shift"),
    (Switch::CtrlShift, "Ctrl+Shift", "ctrl-shift"),
    (Switch::WinSpace, "Win+Space", "win-space"),
    (Switch::CtrlSpace, "Ctrl+Space", "ctrl-space"),
    (Switch::CapsLock, "Caps Lock", "caps"),
];

struct State {
    /// Indices of the enabled layouts in `LAYOUTS`, in switching order.
    enabled: Vec<usize>,
    active: usize,
    default: usize,
    switch: Switch,
    caps: bool,
    /// Held modifiers according to the event stream.
    mods: u8,
    /// A modifier-only combination is complete: it fires on release
    /// if no other key was pressed in between (Ctrl+Shift+T is not a switch).
    armed: bool,
}

static STATE: IrqLock<State> = IrqLock::new(State { enabled: Vec::new(), active: 0, default: 0, switch: Switch::AltShift, caps: false, mods: 0, armed: false });
/// Grows with every change: the WM refreshes the indicator and saves the settings by it.
static VERSION: AtomicU64 = AtomicU64::new(0);

fn bump() {
    VERSION.fetch_add(1, Ordering::Relaxed);
}

impl State {
    fn next(&mut self) {
        let Some(at) = self.enabled.iter().position(|&l| l == self.active) else { return };
        self.active = self.enabled[(at + 1) % self.enabled.len()];
        bump();
    }

    /// The modifier pair of this switching method.
    fn chord(&self) -> Option<u8> {
        match self.switch {
            Switch::AltShift => Some(MOD_ALT | MOD_SHIFT),
            Switch::CtrlShift => Some(MOD_CTRL | MOD_SHIFT),
            _ => None,
        }
    }

    fn mods_changed(&mut self, mods: u8) {
        if let Some(chord) = self.chord() {
            if mods == chord {
                self.armed = true;
            } else if self.armed && mods & chord != chord {
                self.armed = false;
                self.next();
            }
        }
        self.mods = mods;
    }

    fn translate(&self, ch: u8) -> char {
        let layout = &LAYOUTS[self.active];
        let found = US_LOWER.bytes().position(|b| b == ch).map(|i| (layout.lower, i)).or_else(|| US_UPPER.bytes().position(|b| b == ch).map(|i| (layout.upper, i)));
        let c = found.and_then(|(row, i)| row.chars().nth(i)).unwrap_or(ch as char);
        match () {
            _ if !self.caps => c,
            _ if c.is_lowercase() => c.to_uppercase().next().unwrap_or(c),
            _ if c.is_uppercase() => c.to_lowercase().next().unwrap_or(c),
            _ => c,
        }
    }

    fn switches_on(&self, ch: u8) -> bool {
        ch == b' '
            && match self.switch {
                Switch::WinSpace => self.mods & MOD_WIN != 0,
                Switch::CtrlSpace => self.mods & MOD_CTRL != 0,
                _ => false,
            }
    }
}

/// Passes keyboard events through the chosen language.
pub fn process(events: &mut Vec<Event>) {
    if events.is_empty() {
        return;
    }
    let mut s = STATE.lock();
    let mut out = Vec::with_capacity(events.len());
    for e in events.drain(..) {
        match e {
            Event::Mods(m) => s.mods_changed(m),
            Event::Key(KEY_MOD_UP) => out.push(e),
            Event::Key(KEY_CAPS) => {
                s.armed = false;
                if s.switch == Switch::CapsLock {
                    s.next();
                } else {
                    s.caps = !s.caps;
                    bump();
                }
            }
            Event::Key(ch) => {
                s.armed = false;
                if s.switches_on(ch) {
                    s.next();
                } else if (0x21..=0x7E).contains(&ch) && s.mods & (MOD_CTRL | MOD_ALT | MOD_WIN) == 0 {
                    let c = s.translate(ch);
                    out.push(if c.is_ascii() { Event::Key(c as u8) } else { Event::Text(c) });
                } else {
                    out.push(e);
                }
            }
            other => out.push(other),
        }
    }
    *events = out;
}

pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

/// The enabled layouts.
pub fn enabled() -> Vec<usize> {
    STATE.lock().enabled.clone()
}

pub fn active() -> usize {
    STATE.lock().active
}

pub fn default() -> usize {
    STATE.lock().default
}

pub fn switch() -> Switch {
    STATE.lock().switch
}

pub fn caps() -> bool {
    STATE.lock().caps
}

pub fn next() {
    STATE.lock().next();
}

pub fn select(layout: usize) {
    let mut s = STATE.lock();
    if s.enabled.contains(&layout) && s.active != layout {
        s.active = layout;
        bump();
    }
}

pub fn set_switch(switch: Switch) {
    STATE.lock().switch = switch;
    bump();
}

pub fn set_default(layout: usize) {
    let mut s = STATE.lock();
    if s.enabled.contains(&layout) {
        s.default = layout;
        bump();
    }
}

pub fn add(layout: usize) {
    let mut s = STATE.lock();
    if layout < LAYOUTS.len() && !s.enabled.contains(&layout) {
        s.enabled.push(layout);
        bump();
    }
}

/// English always stays: without Latin letters neither a command nor a password can be typed.
pub const LATIN: usize = 0;

/// Removes a language.
pub fn remove(layout: usize) {
    let mut s = STATE.lock();
    if layout == LATIN {
        return;
    }
    s.enabled.retain(|&l| l != layout);
    if s.default == layout {
        s.default = s.enabled[0];
    }
    if s.active == layout {
        s.active = s.default;
    }
    bump();
}

/// Loads the settings (without a file — English only), enables the default language.
pub fn load(text: Option<&str>) {
    let find = |id: &str| LAYOUTS.iter().position(|l| l.id == id);
    let mut enabled = Vec::new();
    let (mut default, mut switch) = (None, Switch::AltShift);
    for (key, value) in text.unwrap_or("").lines().filter_map(|l| l.split_once('=')) {
        match key {
            "languages" => enabled = value.split(',').filter_map(find).collect(),
            "default" => default = find(value),
            "switch" => switch = SWITCHES.iter().find(|s| s.2 == value).map_or(switch, |s| s.0),
            _ => {}
        }
    }
    if enabled.is_empty() {
        enabled = alloc::vec![LATIN];
    } else if !enabled.contains(&LATIN) {
        enabled.insert(0, LATIN);
    }
    let default = default.filter(|d| enabled.contains(d)).unwrap_or(enabled[0]);
    let mut s = STATE.lock();
    (s.enabled, s.default, s.active, s.switch, s.caps) = (enabled, default, default, switch, false);
    bump();
}

pub fn to_config() -> String {
    let s = STATE.lock();
    let languages: Vec<&str> = s.enabled.iter().map(|&l| LAYOUTS[l].id).collect();
    let switch = SWITCHES.iter().find(|w| w.0 == s.switch).map_or("alt-shift", |w| w.2);
    format!("languages={}\ndefault={}\nswitch={switch}\n", languages.join(","), LAYOUTS[s.default].id)
}
