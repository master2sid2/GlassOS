//! HID independent of the bus: keyboard and mouse reports in the boot format (USB and Bluetooth
//! send them the same way) → GlassOS input events. The bus only delivers the reports.

use crate::input::{self, Event};
use crate::interrupts::{self, HZ};
use alloc::vec::Vec;

pub mod report;

/// The state of one keyboard: pressed keys, modifiers and auto-repeat.
#[derive(Clone, Default)]
pub struct Keyboard {
    keys: [u8; 6],
    mods: u8,
    repeat: Option<(u8, u64)>,
    /// When (CPU cycles) each of the `keys` was pressed.
    since: [u64; 6],
}

/// Discarded corrupted reports and presses shorter than a finger can make (counts only —
/// not which keys): they show where "keys pressing themselves" come from.
pub static BAD_REPORTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static SHORT_PRESSES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// A finger does not press for less than ~10 ms (bounce or interference).
const SHORT_CYCLES: u64 = 15_000_000;

/// The report is clearly corrupted: a key twice, an empty slot before a taken one, codes outside the table.
fn broken(keys: &[u8; 6]) -> bool {
    let used = keys.iter().take_while(|&&k| k != 0).count();
    keys[used..].iter().any(|&k| k != 0)
        || keys[..used].iter().any(|&k| matches!(k, 0x02 | 0x03) || k > 0xE7)
        || (0..used).any(|i| keys[i] != 0x01 && keys[i + 1..used].contains(&keys[i]))
}

impl Keyboard {
    pub const fn new() -> Keyboard {
        Keyboard { keys: [0; 6], mods: 0, repeat: None, since: [0; 6] }
    }

    /// A boot keyboard report: modifiers, reserved, up to 6 pressed keys.
    pub fn report(&mut self, r: &[u8], out: &mut Vec<Event>) {
        if r.len() < 8 {
            return;
        }
        let mods = r[0];
        let keys: [u8; 6] = r[2..8].try_into().unwrap();
        // 0x01 in all slots means "too many keys pressed": the state is not changed.
        if keys.iter().all(|&k| k == 0x01) {
            return;
        }
        if broken(&keys) {
            BAD_REPORTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return;
        }
        let now = crate::arch::rdtsc();
        let mut since = [0u64; 6];
        for (i, &k) in keys.iter().enumerate().filter(|(_, k)| **k > 3) {
            since[i] = self.keys.iter().position(|&o| o == k).map_or(now, |j| self.since[j]);
        }
        for (j, &k) in self.keys.iter().enumerate() {
            if k > 3 && !keys.contains(&k) && now.wrapping_sub(self.since[j]) < SHORT_CYCLES {
                SHORT_PRESSES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
        self.since = since;
        // HID bits: Ctrl 0x11, Shift 0x22, Alt 0x44, Win 0x88 (left and right).
        let m = (mods & 0x11 != 0) as u8 * input::MOD_CTRL
            | (mods & 0x22 != 0) as u8 * input::MOD_SHIFT
            | (mods & 0x44 != 0) as u8 * input::MOD_ALT
            | (mods & 0x88 != 0) as u8 * input::MOD_WIN;
        input::USB_MODS.store(m, core::sync::atomic::Ordering::Relaxed);
        if mods != self.mods {
            out.push(Event::Mods(m));
        }
        // Ctrl or Alt released — the end of window switching (as with PS/2).
        let released = self.mods & !mods;
        if released & 0b0101_0101 != 0 {
            out.push(Event::Key(input::KEY_MOD_UP));
        }
        for &k in keys.iter().filter(|&&k| k > 3 && !self.keys.contains(&k)) {
            if let Some(code) = key_code(k, mods) {
                out.push(Event::Key(code));
            }
            // Caps Lock does not repeat, otherwise it would toggle while held.
            if k != 0x39 {
                self.repeat = Some((k, interrupts::ticks() + HZ / 2));
            }
        }
        if self.repeat.is_some_and(|(k, _)| !keys.contains(&k)) {
            self.repeat = None;
        }
        self.keys = keys;
        self.mods = mods;
    }

    /// Keys from a full report (any format): modifiers and the list of pressed keys.
    pub fn keys(&mut self, mods: u8, keys: &[u8], out: &mut Vec<Event>) {
        let mut r = [0u8; 8];
        r[0] = mods;
        for (slot, &k) in r[2..].iter_mut().zip(keys) {
            *slot = k;
        }
        self.report(&r, out);
    }

    /// A key is held: auto-repeat soon.
    pub fn repeating(&self) -> bool {
        self.repeat.is_some()
    }

    /// Auto-repeat: 0.5 s after the press, then 30 times a second.
    pub fn tick(&mut self, now: u64, out: &mut Vec<Event>) {
        if let Some((usage, at)) = self.repeat
            && now >= at
        {
            if let Some(k) = key_code(usage, self.mods) {
                out.push(Event::Key(k));
            }
            self.repeat = Some((usage, now + HZ / 30));
        }
    }

    /// The keyboard disappeared: everything is released (otherwise Ctrl would "stick").
    pub fn release(&mut self, out: &mut Vec<Event>) {
        self.report(&[0; 8], out);
    }
}

/// Media keys (the Consumer page). Every report with a key is a press:
/// volume knobs send "pressed" on every click without sending "released" in between,
/// and an ordinary key sends a report once (reports only come on change).
#[derive(Clone, Default)]
pub struct Media;

impl Media {
    pub fn report(&mut self, pressed: &[u16], out: &mut Vec<Event>) {
        for &u in pressed {
            let key = match u {
                0xE9 => Some(input::KEY_VOLUME_UP),
                0xEA => Some(input::KEY_VOLUME_DOWN),
                0xE2 => Some(input::KEY_MUTE),
                0x6F => Some(input::KEY_BRIGHTNESS_UP),
                0x70 => Some(input::KEY_BRIGHTNESS_DOWN),
                0xCD => Some(input::KEY_PLAY_PAUSE),
                0xB5 => Some(input::KEY_NEXT_TRACK),
                0xB6 => Some(input::KEY_PREV_TRACK),
                0xB7 => Some(input::KEY_STOP),
                _ => None,
            };
            if let Some(k) = key {
                out.push(Event::Key(k));
            }
        }
    }
}

/// A boot mouse report: buttons, x and y motion and (if present) the wheel.
pub fn mouse(r: &[u8], out: &mut Vec<Event>) {
    if r.len() < 3 {
        return;
    }
    let (dx, dy) = (r[1] as i8 as i32, r[2] as i8 as i32);
    let wheel = -(*r.get(3).unwrap_or(&0) as i8 as i32);
    out.push(Event::MouseMove { dx, dy, buttons: r[0] & 7, wheel });
}

/// Events go to the shared input queue (the main loop processes it).
pub fn push(events: Vec<Event>) {
    if !events.is_empty() {
        input::push(events);
    }
}

/// A HID code (US layout) with modifiers → a GlassOS key code (as with PS/2).
pub fn key_code(usage: u8, mods: u8) -> Option<u8> {
    let ctrl = mods & 0b0001_0001 != 0;
    let shift = mods & 0b0010_0010 != 0;
    let alt = mods & 0b0100_0100 != 0;
    let nav = match usage {
        0x52 => Some(input::KEY_UP),
        0x51 => Some(input::KEY_DOWN),
        0x50 => Some(input::KEY_LEFT),
        0x4F => Some(input::KEY_RIGHT),
        0x4A => Some(input::KEY_HOME),
        0x4D => Some(input::KEY_END),
        0x4C => Some(input::KEY_DELETE),
        0x4B if ctrl => Some(input::KEY_TAB_PREV),
        0x4E if ctrl => Some(input::KEY_TAB_NEXT),
        0x4B => Some(input::KEY_PAGE_UP),
        0x4E => Some(input::KEY_PAGE_DOWN),
        0x7F => return Some(input::KEY_MUTE),
        0x46 => return Some(input::KEY_PRINT),
        0x39 => return Some(input::KEY_CAPS),
        0x80 => return Some(input::KEY_VOLUME_UP),
        0x81 => return Some(input::KEY_VOLUME_DOWN),
        _ => None,
    };
    if let Some(k) = nav {
        let shifted = shift && (input::KEY_UP..=input::KEY_END).contains(&k);
        return Some(if shifted { k + input::SHIFTED_NAV } else { k });
    }
    const NORMAL: &[u8] = b"abcdefghijklmnopqrstuvwxyz1234567890\n\x1b\x08\t -=[]\\#;'`,./";
    const SHIFTED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ!@#$%^&*()\n\x1b\x08\t _+{}|~:\"~<>?";
    let ch = match usage {
        0x04..=0x38 => {
            let i = (usage - 0x04) as usize;
            if shift { SHIFTED[i] } else { NORMAL[i] }
        }
        0x58 => b'\n', // keypad Enter
        _ => return None,
    };
    Some(match ch {
        b'\t' if ctrl || alt => input::KEY_SWITCH,
        _ if ctrl && ch.is_ascii_alphabetic() => ch.to_ascii_uppercase() & 0x1F,
        _ => ch,
    })
}
