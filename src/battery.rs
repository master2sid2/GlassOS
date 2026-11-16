//! The laptop battery. A full ACPI (AML) parse is not needed for it: on known models
//! the charge lies in the EC controller's memory at addresses taken from their DSDT. On other
//! computers the EC ports are not touched at all, and there is no battery icon.
//!
//! The lid state lies there too, in the flags byte (the ECG3 method in the DSDT, called by
//! _LID), so this same thread polls it.
//!
//! The batteries of connected devices (keyboards, mice, headphones) live here as well: their charge
//! is reported by the bus drivers (`set_device`), and the tray icon and the Power section show it.

use crate::acpi;
use crate::arch::{inb, outb};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::sync::IrqLock;
use crate::task;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// A model: how it names itself in the DSDT, the EC ports and the addresses in its memory.
struct Profile {
    oem: &'static [u8],
    table: &'static [u8],
    data: u16,
    command: u16,
}

/// The addresses are the same on Dell laptops of this generation: see the ECG2, ECG5, ECG6,
/// ECG9 methods in their DSDT. The ports come from _CRS of the ECDV device.
const PROFILES: [Profile; 1] = [Profile { oem: b"DELL", table: b"CBX3", data: 0x930, command: 0x934 }];

const EC_FLAGS: u8 = 0x00;
const EC_FLAG_AC: u8 = 1;
const EC_FLAG_LID_OPEN: u8 = 0x10;
const EC_PRESENT: u8 = 0x06;
const EC_PRESENT_MAIN: u8 = 0x02;
/// Which battery to show in the address window below.
const EC_SELECT: u8 = 0x03;
const EC_STATE: u8 = 0x10;
const EC_RATE: u8 = 0x12;
const EC_REMAINING: u8 = 0x16;
const EC_FULL: u8 = 0x1E;
const STATE_DISCHARGING: u8 = 1;
const STATE_CHARGING: u8 = 2;

const CMD_READ: u8 = 0x80;
const CMD_WRITE: u8 = 0x81;
const STATUS_OUTPUT_FULL: u8 = 1;
const STATUS_INPUT_FULL: u8 = 2;
/// The port is polled this many times, each poll takes about a microsecond.
const SPINS: u32 = 200_000;
const PERIOD: u64 = 10 * HZ;
/// The lid is checked more often: the screen must turn off right away.
const LID_PERIOD: u64 = HZ / 2;
/// After this many failures in a row we assume the model was detected wrongly, and leave.
const MAX_FAILURES: u32 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    pub percent: u32,
    pub charging: bool,
    pub on_ac: bool,
    /// How many minutes are left if the computer runs on battery.
    pub minutes: Option<u32>,
}

static STATE: IrqLock<Option<Battery>> = IrqLock::new(None);
static VERSION: AtomicU64 = AtomicU64::new(0);

pub fn get() -> Option<Battery> {
    *STATE.lock()
}

pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

/// The battery of a connected device.
#[derive(Clone, PartialEq, Eq)]
pub struct Device {
    /// Where it is known from (for example, "bt:ff:2a:…"): it is updated and removed by this.
    pub id: String,
    pub name: String,
    /// keyboard, mouse, headphones, device…
    pub kind: &'static str,
    pub percent: u32,
}

static DEVICES: IrqLock<Vec<Device>> = IrqLock::new(Vec::new());

/// A device's charge arrived or changed.
pub fn set_device(id: &str, name: &str, kind: &'static str, percent: u32) {
    let d = Device { id: id.into(), name: name.into(), kind, percent: percent.min(100) };
    let mut list = DEVICES.lock();
    match list.iter_mut().find(|x| x.id == id) {
        Some(x) if *x == d => return,
        Some(x) => *x = d,
        None => list.push(d),
    }
    drop(list);
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// A device disconnected.
pub fn remove_device(id: &str) {
    let mut list = DEVICES.lock();
    let before = list.len();
    list.retain(|x| x.id != id);
    if list.len() != before {
        drop(list);
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Device batteries, by name.
pub fn devices() -> Vec<Device> {
    let mut list = DEVICES.lock().clone();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

struct Ec<'a>(&'a Profile);

impl Ec<'_> {
    fn wait(&self, ready: impl Fn(u8) -> bool) -> Option<()> {
        (0..SPINS).find(|_| ready(inb(self.0.command))).map(|_| ())
    }

    fn send(&self, port: u16, value: u8) -> Option<()> {
        self.wait(|s| s & STATUS_INPUT_FULL == 0)?;
        outb(port, value);
        Some(())
    }

    fn byte(&self, addr: u8) -> Option<u8> {
        self.send(self.0.command, CMD_READ)?;
        self.send(self.0.data, addr)?;
        self.wait(|s| s & STATUS_OUTPUT_FULL != 0)?;
        Some(inb(self.0.data))
    }

    fn word(&self, addr: u8) -> Option<u32> {
        Some(self.byte(addr)? as u32 | (self.byte(addr + 1)? as u32) << 8)
    }

    fn write(&self, addr: u8, value: u8) -> Option<()> {
        self.send(self.0.command, CMD_WRITE)?;
        self.send(self.0.data, addr)?;
        self.send(self.0.data, value)
    }

    fn lid_open(&self) -> Option<bool> {
        Some(self.byte(EC_FLAGS)? & EC_FLAG_LID_OPEN != 0)
    }

    fn read_battery(&self) -> Option<Option<Battery>> {
        if self.byte(EC_PRESENT)? & EC_PRESENT_MAIN == 0 {
            return Some(None);
        }
        let on_ac = self.byte(EC_FLAGS)? & EC_FLAG_AC != 0;
        self.write(EC_SELECT, 1)?;
        let (state, rate, remaining, full) = (self.byte(EC_STATE)?, self.word(EC_RATE)?, self.word(EC_REMAINING)?, self.word(EC_FULL)?);
        // A signed current: negative while discharging.
        let draw = if rate & 0x8000 != 0 { 0x1_0000 - rate } else { 0 };
        let minutes = (state & STATE_DISCHARGING != 0 && !on_ac && draw > 0).then(|| remaining * 60 / draw);
        let percent = if full > 0 { ((remaining * 100 + full / 2) / full).min(100) } else { 0 };
        Some(Some(Battery { percent, charging: state & STATE_CHARGING != 0, on_ac, minutes }))
    }
}

/// Talking to the EC: the field is declared with a lock, which is held only for this time.
fn with_ec<T>(profile: &Profile, f: impl Fn(&Ec) -> Option<T>) -> Option<T> {
    for _ in 0..100 {
        if acpi::lock_global() {
            let result = f(&Ec(profile));
            acpi::unlock_global();
            return result;
        }
        task::sleep_until(interrupts::ticks() + HZ / 100);
    }
    None
}

/// Starts battery polling if the model is known.
pub fn start() {
    let Some((oem, table)) = acpi::dsdt_id() else { return };
    let Some(profile) = PROFILES.iter().find(|p| oem.starts_with(p.oem) && table.starts_with(p.table)) else { return };
    kprintln!("battery: known model, EC at {:#x}/{:#x}", profile.data, profile.command);
    task::spawn("battery", None, move || {
        let mut failures = 0;
        let mut battery_at = 0;
        while failures < MAX_FAILURES {
            if let Some(open) = with_ec(profile, |ec| ec.lid_open()) {
                crate::output::lid_seen(open);
            }
            if interrupts::ticks() >= battery_at {
                battery_at = interrupts::ticks() + PERIOD;
                match with_ec(profile, |ec| ec.read_battery()) {
                    Some(state) => {
                        failures = 0;
                        let mut slot = STATE.lock();
                        if *slot != state {
                            *slot = state;
                            VERSION.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    None => failures += 1,
                }
            }
            task::sleep_until(interrupts::ticks() + LID_PERIOD);
        }
        kprintln!("battery: EC does not answer, giving up");
    });
}
