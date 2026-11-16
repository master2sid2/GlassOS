//! Input: the PS/2 keyboard (scancode set 1) and mouse.
//!
//! The mouse goes through vmmouse (the VMware backdoor that QEMU emulates): it gives absolute
//! coordinates, so the cursor in VNC matches the client's cursor. Without vmmouse we
//! fall back to the ordinary relative PS/2 mouse.

use crate::arch::{inb, outb};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::arch::asm;

pub enum Event {
    Key(u8),
    /// A non-ASCII character (produced by the input language, see `keyboard`).
    Text(char),
    /// The held modifiers changed. Does not reach applications.
    Mods(u8),
    /// `wheel` — wheel clicks: > 0 down, < 0 up.
    Mouse { x: i32, y: i32, buttons: u8, wheel: i32 },
    /// Relative USB mouse: a movement delta. `Input::poll` adds it to the shared
    /// cursor — all mice and the touchpad move one cursor.
    MouseMove { dx: i32, dy: i32, buttons: u8, wheel: i32 },
}

/// Events from USB devices (the usb thread puts them, `Input::poll` takes them).
pub static USB_EVENTS: crate::sync::IrqLock<alloc::collections::VecDeque<Event>> =
    crate::sync::IrqLock::new(alloc::collections::VecDeque::new());

/// Where the cursor is, including motion the desktop has not processed yet. The hardware
/// cursor moves by it right away, from the device thread: a slow frame does not slow the mouse.
/// Changed under the `USB_EVENTS` lock; the desktop reconciles it with its own when
/// the queue is empty.
static SHADOW: crate::sync::IrqLock<(i32, i32)> = crate::sync::IrqLock::new((0, 0));

/// Mouse, keyboard and touchpad events go to the shared input queue; the cursor moves right away.
pub fn push(events: impl IntoIterator<Item = Event>) {
    let mut moved = None;
    {
        let mut queue = USB_EVENTS.lock();
        for e in events {
            match e {
                Event::MouseMove { dx, dy, .. } if dx != 0 || dy != 0 => {
                    let mut s = SHADOW.lock();
                    *s = crate::output::clamp(s.0 + dx, s.1 + dy);
                    moved = Some(*s);
                }
                Event::Mouse { x, y, .. } => {
                    let mut s = SHADOW.lock();
                    *s = crate::output::clamp(x, y);
                    moved = Some(*s);
                }
                _ => {}
            }
            queue.push_back(e);
        }
    }
    if let Some((x, y)) = moved {
        crate::output::follow_pointer(x, y);
    }
    crate::task::kick_main();
}

/// The touchpad sends reports over I2C: until this tick its PS/2 copy is ignored, otherwise the cursor
/// would get every movement twice. If I2C goes silent, PS/2 is listened to again.
pub static PS2_AUX_QUIET_UNTIL: AtomicU64 = AtomicU64::new(0);

/// How many key presses came from where (only the count — not which keys): PS/2, the shared queue
/// (USB, Bluetooth, remote input) and, out of it, USB keyboards. They show who sends
/// key presses the person did not make.
pub static KEYS_PS2: AtomicU64 = AtomicU64::new(0);
pub static KEYS_QUEUE: AtomicU64 = AtomicU64::new(0);
pub static KEYS_USB: AtomicU64 = AtomicU64::new(0);
static KEYS_TOLD: AtomicU64 = AtomicU64::new(0);

pub fn count_keys(counter: &AtomicU64, events: &[Event]) {
    let n = events.iter().filter(|e| matches!(e, Event::Key(_) | Event::Text(_))).count() as u64;
    if n > 0 {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

/// Once every 10 s, if there were key presses: how many from where.
fn keys_report(now: u64) {
    if now < KEYS_TOLD.load(Ordering::Relaxed) + 10 * crate::interrupts::HZ {
        return;
    }
    KEYS_TOLD.store(now, Ordering::Relaxed);
    let (ps2, queue, usb) = (KEYS_PS2.swap(0, Ordering::Relaxed), KEYS_QUEUE.swap(0, Ordering::Relaxed), KEYS_USB.swap(0, Ordering::Relaxed));
    let bad = crate::hid::BAD_REPORTS.swap(0, Ordering::Relaxed);
    let short = crate::hid::SHORT_PRESSES.swap(0, Ordering::Relaxed);
    if ps2 + queue + bad + short > 0 {
        crate::kprintln!(
            "input: keys in 10 s: PS/2 {ps2}, USB keyboard {usb}, other (Bluetooth, remote) {}; broken keyboard reports dropped {bad}, presses shorter than ~10 ms {short}",
            queue.saturating_sub(usb)
        );
    }
}
/// How many times the PS/2 mouse restarted (for the `touchpad` command).
pub static AUX_RESTARTS: AtomicU32 = AtomicU32::new(0);
/// The PS/2 mouse restarted (it sent "self-test passed"): it must be asked again
/// to send reports.
static AUX_RESTARTED: AtomicBool = AtomicBool::new(false);

pub const LEFT: u8 = 1;
pub const RIGHT: u8 = 2;

// Held modifiers (`mods()`): for Ctrl+click and Shift+click.
pub const MOD_CTRL: u8 = 1;
pub const MOD_SHIFT: u8 = 2;
pub const MOD_ALT: u8 = 4;
pub const MOD_WIN: u8 = 8;
static PS2_MODS: AtomicU8 = AtomicU8::new(0);
/// USB keyboard modifiers (set by the USB driver).
pub static USB_MODS: AtomicU8 = AtomicU8::new(0);

/// Which modifiers are held now (on any keyboard).
pub fn mods() -> u8 {
    PS2_MODS.load(Ordering::Relaxed) | USB_MODS.load(Ordering::Relaxed)
}

// Codes of special keys (outside ASCII).
pub const KEY_UP: u8 = 0x80;
pub const KEY_DOWN: u8 = 0x81;
pub const KEY_LEFT: u8 = 0x82;
pub const KEY_RIGHT: u8 = 0x83;
pub const KEY_HOME: u8 = 0x84;
pub const KEY_END: u8 = 0x85;
pub const KEY_DELETE: u8 = 0x86;
/// Shift + a navigation key (arrows, Home, End) — for selecting text.
pub const SHIFTED_NAV: u8 = 0x10;
/// Alt+Tab or Ctrl+Tab — window switching.
pub const KEY_SWITCH: u8 = 0x87;
/// Alt or Ctrl released — the end of window switching.
pub const KEY_MOD_UP: u8 = 0x88;
pub const KEY_PAGE_UP: u8 = 0x89;
pub const KEY_PAGE_DOWN: u8 = 0x8A;
/// Ctrl+PgUp / Ctrl+PgDn — previous and next tab.
pub const KEY_TAB_PREV: u8 = 0x8B;
pub const KEY_TAB_NEXT: u8 = 0x8C;
// Media keys (Fn+…): the WM handles them itself. The codes are above
// 0x9F: 0x90..=0x95 are taken by Shift navigation (KEY_UP + SHIFTED_NAV…).
pub const KEY_VOLUME_UP: u8 = 0xA0;
pub const KEY_VOLUME_DOWN: u8 = 0xA1;
pub const KEY_MUTE: u8 = 0xA2;
pub const KEY_BRIGHTNESS_UP: u8 = 0xA3;
pub const KEY_BRIGHTNESS_DOWN: u8 = 0xA4;
/// Print Screen — a screenshot.
pub const KEY_PRINT: u8 = 0xA5;
/// Caps Lock. `keyboard` handles it.
pub const KEY_CAPS: u8 = 0xA6;
/// Player: play/pause, next, previous, stop.
pub const KEY_PLAY_PAUSE: u8 = 0xA7;
pub const KEY_NEXT_TRACK: u8 = 0xA8;
pub const KEY_PREV_TRACK: u8 = 0xA9;
pub const KEY_STOP: u8 = 0xAA;

const NORMAL: [u8; 58] = *b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: [u8; 58] = *b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

pub struct Input {
    shift: bool,
    /// Left and right Ctrl are pressed.
    ctrl: [bool; 2],
    /// Left and right Alt are pressed.
    alt: [bool; 2],
    /// Left and right Win are pressed.
    win: [bool; 2],
    /// Caps Lock is held: auto-repeat must not toggle it again and again.
    caps_down: bool,
    /// Modifiers already reported with an event.
    reported_mods: u8,
    /// What is lit on the keyboard now.
    caps_led: bool,
    extended: bool,
    vmmouse: bool,
    packet: [u8; 4],
    packet_len: usize,
    /// PS/2 mouse: 0 — plain (3 bytes), 3 — with a wheel, 4 — with a wheel and buttons 4/5.
    mouse_id: u8,
    mx: i32,
    /// Buttons held on PS/2 (touchpad, vmmouse) and on USB — a press on one mouse
    /// must not be released by the motion of another.
    ps2_buttons: u8,
    usb_buttons: u8,
    my: i32,
    /// When the controller was polled directly (tick) and how many poll calls since then.
    last_poll: u64,
    polls: u32,
}

impl Input {
    /// The cursor moved to another screen: the screens swapped places.
    pub fn move_pointer(&mut self, x: i32, y: i32) {
        (self.mx, self.my) = (x, y);
        *SHADOW.lock() = (x, y);
    }

    pub fn new(w: i32, h: i32) -> Self {
        let mouse_id = ps2_init();
        let vmmouse = vmmouse::init();
        crate::kprintln!("input: ps/2 ready, mouse id {mouse_id}, vmmouse = {vmmouse}");
        Self { shift: false, ctrl: [false; 2], alt: [false; 2],
            win: [false; 2],
            caps_down: false,
            reported_mods: 0,
            caps_led: false, extended: false, vmmouse, packet: [0; 4], packet_len: 0, mouse_id, ps2_buttons: 0, usb_buttons: 0, mx: w / 2, my: h / 2, last_poll: 0, polls: 0 }
    }

    pub fn has_vmmouse(&self) -> bool {
        self.vmmouse
    }

    pub fn poll(&mut self, out: &mut Vec<Event>) {
        // A port access is a VM exit, and it is expensive.
        // So the controller is read in the interrupt, and here only occasionally (in case
        // interrupts do not work or an event was lost).
        let now = crate::interrupts::ticks();
        // A call counter — in case the timer is not running.
        self.polls += 1;
        let fallback = now.wrapping_sub(self.last_poll) >= crate::interrupts::HZ / 4 || self.polls >= 64;
        if fallback {
            (self.last_poll, self.polls) = (now, 0);
            crate::sync::without_interrupts(|| irq());
        }
        while let Some((status, byte)) = pop() {
            if status & 0x20 != 0 {
                // With vmmouse active QEMU sends dummy packets over PS/2 — ignore them.
                if self.vmmouse {
                    continue;
                }
                if now < PS2_AUX_QUIET_UNTIL.load(Ordering::Relaxed) {
                    // A copy of the contacts: skip it. A button held over PS/2 before
                    // I2C started talking is released, otherwise it would stay held.
                    self.packet_len = 0;
                    if self.ps2_buttons != 0 {
                        self.ps2_buttons = 0;
                        out.push(Event::Mouse { x: self.mx, y: self.my, buttons: self.usb_buttons, wheel: 0 });
                    }
                } else {
                    self.ps2_mouse_byte(byte, out);
                }
            } else {
                let before = out.len();
                self.keyboard_byte(byte, out);
                count_keys(&KEYS_PS2, &out[before..]);
            }
        }
        let usb: Vec<Event> = USB_EVENTS.lock().drain(..).collect();
        count_keys(&KEYS_QUEUE, &usb);
        keys_report(now);
        for e in usb {
            match e {
                Event::MouseMove { dx, dy, buttons, wheel } => {
                    (self.mx, self.my) = crate::output::clamp(self.mx + dx, self.my + dy);
                    self.usb_buttons = buttons;
                    out.push(Event::Mouse { x: self.mx, y: self.my, buttons: buttons | self.ps2_buttons, wheel });
                }
                Event::Mouse { x, y, buttons, wheel } => {
                    // The tablet (absolute coordinates) also moves the shared cursor.
                    (self.mx, self.my) = crate::output::clamp(x, y);
                    self.usb_buttons = buttons;
                    out.push(Event::Mouse { x: self.mx, y: self.my, buttons: buttons | self.ps2_buttons, wheel });
                }
                key => out.push(key),
            }
        }
        if AUX_RESTARTED.swap(false, Ordering::Relaxed) {
            // After a restart the mouse is silent and sends short packets: bring it back.
            // Interrupts are off meanwhile so the answer does not go to the queue.
            crate::sync::without_interrupts(|| {
                mouse_cmd(0xF4);
            });
            self.mouse_id = 0;
            self.packet_len = 0;
            crate::kprintln!("input: ps/2 mouse restarted, reports enabled again");
        }
        // On every event vmmouse also pokes the PS/2 mouse (IRQ12) — we poll on that.
        if self.vmmouse && (MOUSE_PENDING.swap(false, Ordering::Relaxed) || fallback) && !self.poll_vmmouse(out) {
            MOUSE_PENDING.store(true, Ordering::Relaxed);
        }
        // The queue is empty — reconcile the cursor shadow with the real one (PS/2, vmmouse, screen edges).
        {
            let queue = USB_EVENTS.lock();
            if queue.is_empty() {
                *SHADOW.lock() = (self.mx, self.my);
            }
        }
        crate::keyboard::process(out);
        let caps = crate::keyboard::caps();
        if caps != self.caps_led {
            self.caps_led = caps;
            set_caps_led(caps);
        }
    }

    fn keyboard_byte(&mut self, byte: u8, out: &mut Vec<Event>) {
        self.keyboard_scancode(byte, out);
        let m = (self.ctrl[0] || self.ctrl[1]) as u8 * MOD_CTRL
            | self.shift as u8 * MOD_SHIFT
            | (self.alt[0] || self.alt[1]) as u8 * MOD_ALT
            | (self.win[0] || self.win[1]) as u8 * MOD_WIN;
        PS2_MODS.store(m, Ordering::Relaxed);
        if m != self.reported_mods {
            self.reported_mods = m;
            out.push(Event::Mods(m));
        }
    }

    fn keyboard_scancode(&mut self, byte: u8, out: &mut Vec<Event>) {
        if byte == 0xE0 {
            self.extended = true;
            return;
        }
        if self.extended {
            self.extended = false;
            let down = byte & 0x80 == 0;
            match byte & 0x7F {
                0x1D => self.ctrl[1] = down, // right Ctrl
                0x38 => self.alt[1] = down,  // right Alt
                0x5B => self.win[0] = down,
                0x5C => self.win[1] = down,
                _ => {}
            }
            if matches!(byte & 0x7F, 0x5B | 0x5C) {
                return;
            }
            if matches!(byte & 0x7F, 0x1D | 0x38) {
                if !down {
                    out.push(Event::Key(KEY_MOD_UP));
                }
                return;
            }
            if byte & 0x80 != 0 {
                return;
            }
            let key = match byte {
                0x48 => KEY_UP,
                0x50 => KEY_DOWN,
                0x4B => KEY_LEFT,
                0x4D => KEY_RIGHT,
                0x47 => KEY_HOME,
                0x4F => KEY_END,
                0x53 => KEY_DELETE,
                0x1C => b'\n', // keypad Enter
                0x49 if self.ctrl[0] || self.ctrl[1] => KEY_TAB_PREV,
                0x51 if self.ctrl[0] || self.ctrl[1] => KEY_TAB_NEXT,
                0x49 => KEY_PAGE_UP,
                0x51 => KEY_PAGE_DOWN,
                0x37 => KEY_PRINT,
                0x2A => return, // the "fake" Shift before Print Screen
                0x20 => KEY_MUTE,
                0x2E => KEY_VOLUME_DOWN,
                0x30 => KEY_VOLUME_UP,
                0x22 => KEY_PLAY_PAUSE,
                0x19 => KEY_NEXT_TRACK,
                0x10 => KEY_PREV_TRACK,
                0x24 => KEY_STOP,
                // Brightness: there is no standard code; these are seen in the wild.
                0x4C | 0x54 => KEY_BRIGHTNESS_DOWN,
                0x4E | 0x55 => KEY_BRIGHTNESS_UP,
                code => {
                    // An unknown key goes to the log (this is how we learn what Fn+… sends on a laptop).
                    crate::kprintln!("kbd: unknown key E0 {code:02x}");
                    return;
                }
            };
            let shifted = self.shift && (KEY_UP..=KEY_END).contains(&key);
            out.push(Event::Key(if shifted { key + SHIFTED_NAV } else { key }));
            return;
        }
        let released = byte & 0x80 != 0;
        let code = (byte & 0x7F) as usize;
        match code {
            0x2A | 0x36 => self.shift = !released,
            0x3A => {
                if !released && !self.caps_down {
                    out.push(Event::Key(KEY_CAPS));
                }
                self.caps_down = !released;
            }
            0x1D | 0x38 => {
                let slot = if code == 0x1D { &mut self.ctrl[0] } else { &mut self.alt[0] };
                *slot = !released;
                if released {
                    out.push(Event::Key(KEY_MOD_UP));
                }
            }
            _ if !released => {
                let map = if self.shift { &SHIFTED } else { &NORMAL };
                if let Some(&ch) = map.get(code)
                    && ch != 0
                {
                    // Ctrl+letter is a control character, as in terminals: Ctrl+C = 0x03.
                    let ctrl = self.ctrl[0] || self.ctrl[1];
                    let alt = self.alt[0] || self.alt[1];
                    let key = match ch {
                        b'\t' if ctrl || alt => KEY_SWITCH,
                        _ if ctrl && ch.is_ascii_alphabetic() => ch.to_ascii_uppercase() & 0x1F,
                        _ => ch,
                    };
                    out.push(Event::Key(key));
                }
            }
            _ => {}
        }
    }

    fn ps2_mouse_byte(&mut self, byte: u8, out: &mut Vec<Event>) {
        // "Self-test passed" and the mouse id: it restarted.
        if self.packet_len == 1 && self.packet[0] == 0xAA && byte == 0x00 {
            self.packet_len = 0;
            self.ps2_buttons = 0;
            AUX_RESTARTED.store(true, Ordering::Relaxed);
            AUX_RESTARTS.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // The first byte of a packet always has bit 3, and overflow bits do not occur in a live
        // stream: such a byte is the middle of a packet, look for the start of the next one.
        // 0xAA is let through: a restart's 0x00 may follow it.
        if self.packet_len == 0 && (byte & 0x08 == 0 || (byte & 0xC0 != 0 && byte != 0xAA)) {
            return;
        }
        self.packet[self.packet_len] = byte;
        self.packet_len += 1;
        let size = if self.mouse_id >= 3 { 4 } else { 3 };
        if self.packet_len < size {
            return;
        }
        self.packet_len = 0;
        let [b0, b1, b2, b3] = self.packet;
        // Wheel: IntelliMouse uses the whole 4th byte, Explorer the low 4 bits, signed.
        let wheel = match self.mouse_id {
            3 => b3 as i8 as i32,
            4 => ((b3 << 4) as i8 >> 4) as i32,
            _ => 0,
        };
        let dx = b1 as i32 - (((b0 as i32) << 4) & 0x100);
        let dy = b2 as i32 - (((b0 as i32) << 3) & 0x100);
        (self.mx, self.my) = crate::output::clamp(self.mx + dx, self.my - dy);
        self.ps2_buttons = b0 & 0x07;
        out.push(Event::Mouse { x: self.mx, y: self.my, buttons: self.ps2_buttons | self.usb_buttons, wheel });
    }

    /// Takes vmmouse packets; false — some are still left in the queue.
    fn poll_vmmouse(&mut self, out: &mut Vec<Event>) -> bool {
        for _ in 0..32 {
            let status = vmmouse::backdoor(vmmouse::CMD_STATUS, 0)[0];
            if status & 0xFFFF_0000 == 0xFFFF_0000 {
                self.vmmouse = vmmouse::init();
                return true;
            }
            if status & 0xFFFF < 4 {
                return true;
            }
            let [flags, x, y, z] = vmmouse::backdoor(vmmouse::CMD_DATA, 4);
            let buttons = (flags & 0x20 != 0) as u8 | ((flags & 0x10 != 0) as u8) << 1 | ((flags & 0x08 != 0) as u8) << 2;
            let desk = crate::output::bounds();
            let (ax, ay) = ((x as i64 * desk.w as i64 / 0x10000) as i32, (y as i64 * desk.h as i64 / 0x10000) as i32);
            (self.mx, self.my) = crate::output::clamp(ax, ay);
            self.ps2_buttons = buttons;
            out.push(Event::Mouse { x: self.mx, y: self.my, buttons: buttons | self.usb_buttons, wheel: z as i32 });
        }
        false
    }
}

/// The PS/2 byte queue (status << 8 | byte): IRQ1/IRQ12 fill it, `poll` drains it.
/// The controller has room for only one byte — without the queue key presses were lost
/// while the main thread drew a heavy frame or gave the CPU to programs.
const QUEUE_LEN: usize = 512;
static QUEUE: [AtomicU16; QUEUE_LEN] = [const { AtomicU16::new(0) }; QUEUE_LEN];
static HEAD: AtomicUsize = AtomicUsize::new(0);
/// A byte arrived from the PS/2 mouse — so vmmouse has new data.
static MOUSE_PENDING: AtomicBool = AtomicBool::new(false);
static TAIL: AtomicUsize = AtomicUsize::new(0);

/// Moves bytes from the controller to the queue. Called from the interrupt (and from `poll`
/// with interrupts disabled), so only one writer touches the queue at a time.
pub extern "sysv64" fn irq() {
    let mut got = false;
    for _ in 0..32 {
        let status = inb(0x64);
        if status & 0x01 == 0 {
            break;
        }
        got = true;
        let byte = inb(0x60);
        if status & 0x20 != 0 {
            MOUSE_PENDING.store(true, Ordering::Relaxed);
        }
        let head = HEAD.load(Ordering::Relaxed);
        if head.wrapping_sub(TAIL.load(Ordering::Acquire)) < QUEUE_LEN {
            QUEUE[head % QUEUE_LEN].store((status as u16) << 8 | byte as u16, Ordering::Relaxed);
            HEAD.store(head.wrapping_add(1), Ordering::Release);
        }
    }
    if got {
        crate::task::kick_main();
    }
}

fn pop() -> Option<(u8, u8)> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        return None;
    }
    let v = QUEUE[tail % QUEUE_LEN].load(Ordering::Relaxed);
    TAIL.store(tail.wrapping_add(1), Ordering::Release);
    Some(((v >> 8) as u8, v as u8))
}

fn wait_write() {
    for _ in 0..100_000 {
        if inb(0x64) & 0x02 == 0 {
            return;
        }
    }
}

fn read() -> Option<u8> {
    for _ in 0..100_000 {
        if inb(0x64) & 0x01 != 0 {
            return Some(inb(0x60));
        }
    }
    None
}

fn command(c: u8) {
    wait_write();
    outb(0x64, c);
}

fn data(d: u8) {
    wait_write();
    outb(0x60, d);
}

/// A PS/2 mouse command (through the controller); the answer is the 0xFA acknowledgement.
fn mouse_cmd(b: u8) -> bool {
    command(0xD4);
    data(b);
    read() == Some(0xFA)
}

/// Three status bytes after a "secret" command sequence.
fn mouse_status() -> Option<[u8; 3]> {
    if !mouse_cmd(0xE9) {
        return None;
    }
    Some([read()?, read()?, read()?])
}

/// Who made the touchpad: Synaptics, ELAN, ALPS answer their "knocks" with a special status.
fn identify_touchpad() -> &'static str {
    // Synaptics: "resolution" 4 times (encodes request 0), then status: the middle byte is 0x47.
    for _ in 0..4 {
        mouse_cmd(0xE8);
        mouse_cmd(0x00);
    }
    if let Some(id) = mouse_status() {
        crate::kprintln!("input: synaptics probe {:02x?}", id);
        if id[1] == 0x47 {
            return "Synaptics";
        }
    }
    // ELAN: "scaling 1:1" three times, then status 3c 03 xx.
    for _ in 0..3 {
        mouse_cmd(0xE6);
    }
    if let Some(id) = mouse_status() {
        crate::kprintln!("input: elan probe {:02x?}", id);
        if id[0] == 0x3C && id[1] == 0x03 {
            return "ELAN";
        }
    }
    // ALPS: "scaling 2:1" three times, then status 73 02 xx (or 73 03 xx).
    for _ in 0..3 {
        mouse_cmd(0xE7);
    }
    if let Some(id) = mouse_status() {
        crate::kprintln!("input: alps probe {:02x?}", id);
        if id[0] == 0x73 {
            return "ALPS";
        }
    }
    "generic"
}

/// Enables the wheel: "sample rates" 200-100-80 (IntelliMouse, ID 3), then 200-200-80
/// (Explorer, ID 4). Returns the mouse ID.
fn enable_wheel() -> u8 {
    let knock = |rates: [u8; 3]| {
        for r in rates {
            mouse_cmd(0xF3);
            mouse_cmd(r);
        }
        if !mouse_cmd(0xF2) {
            return 0;
        }
        read().unwrap_or(0)
    };
    let mut id = knock([200, 100, 80]);
    if id == 3 {
        let ex = knock([200, 200, 80]);
        if ex == 4 {
            id = 4;
        }
    }
    // The normal report rate.
    mouse_cmd(0xF3);
    mouse_cmd(100);
    id
}

/// The Caps Lock LED on a PS/2 keyboard. Acknowledgements come in the ordinary keyboard
/// byte stream and affect nothing there; there is a pause between the command and the value.
fn set_caps_led(on: bool) {
    const SET_LEDS: u8 = 0xED;
    const CAPS_BIT: u8 = 4;
    const ACK_WAIT: u32 = 3000;
    data(SET_LEDS);
    (0..ACK_WAIT).for_each(|_| {
        inb(0x80);
    });
    data(if on { CAPS_BIT } else { 0 });
}

fn ps2_init() -> u8 {
    command(0xAD); // disable the keyboard
    command(0xA7); // disable the mouse
    for _ in 0..32 {
        if inb(0x64) & 0x01 == 0 {
            break;
        }
        inb(0x60);
    }
    command(0x20);
    let cfg = read().unwrap_or(0);
    // IRQ1 + IRQ12, translation to set 1, clocking of both ports.
    let cfg = (cfg | 0x43) & !0x30;
    command(0x60);
    data(cfg);
    command(0xAE);
    command(0xA8);
    data(0xF4); // keyboard: enable scanning
    read();
    if !mouse_cmd(0xF6) {
        // No PS/2 mouse (it did not acknowledge the command) — do not wait for answers to the "knocks".
        mouse_cmd(0xF4);
        crate::kprintln!("input: no ps/2 pointing device");
        return 0;
    }
    let maker = identify_touchpad();
    mouse_cmd(0xF6); // after the "knocks" — back to defaults
    let id = enable_wheel();
    crate::kprintln!("input: ps/2 pointing device: {maker}, id {id}{}", if id >= 3 { " (wheel)" } else { "" });
    mouse_cmd(0xF4); // mouse: enable reports
    if id == 3 || id == 4 { id } else { 0 }
}

mod vmmouse {
    use super::asm;

    const MAGIC: u32 = 0x564D_5868;
    const PORT: u16 = 0x5658;
    const CMD_GETVERSION: u32 = 10;
    pub const CMD_DATA: u32 = 39;
    pub const CMD_STATUS: u32 = 40;
    const CMD_COMMAND: u32 = 41;
    const READ_ID: u32 = 0x4541_4552;
    const REQUEST_ABSOLUTE: u32 = 0x5342_4152;
    const VERSION_ID: u32 = 0x3442_554A;

    pub fn backdoor(cmd: u32, arg: u32) -> [u32; 4] {
        let (a, b, c, d): (u32, u64, u32, u32);
        // rbx is reserved by LLVM, so we swap it manually.
        unsafe {
            asm!(
                "xchg rbx, {b}",
                "in eax, dx",
                "xchg rbx, {b}",
                b = inout(reg) arg as u64 => b,
                inout("eax") MAGIC => a,
                inout("ecx") cmd => c,
                inout("edx") PORT as u32 => d,
                options(nostack),
            )
        }
        [a, b as u32, c, d]
    }

    pub fn init() -> bool {
        if backdoor(CMD_GETVERSION, 0)[1] != MAGIC {
            return false;
        }
        backdoor(CMD_COMMAND, READ_ID);
        let status = backdoor(CMD_STATUS, 0)[0];
        if status & 0xFFFF == 0 || status & 0xFFFF_0000 == 0xFFFF_0000 {
            return false;
        }
        if backdoor(CMD_DATA, 1)[0] != VERSION_ID {
            return false;
        }
        backdoor(CMD_COMMAND, REQUEST_ABSOLUTE);
        true
    }
}
