//! A laptop touchpad over HID over I2C (the Microsoft HID over I2C 1.0 specification) in
//! Windows Precision Touchpad mode: the device sends contacts, `gesture` builds gestures.
//!
//! A report is read only when the touchpad pulls its interrupt line low: a read without a ready
//! report is held by the device until a report appears, and the bus and the CPU stall. The line is a
//! GPIO pin; firmware puts its number in the GPDI field of the GNVS region (so on Intel and Dell), and
//! the address of its register is computed from the GPIO group table in the DSDT. The pin level is read
//! by polling: it is a single memory access. If the line cannot be found, we read blindly.
//!
//! The touchpad may reset itself: then it sends an empty report and forgets touch mode,
//! and mouse reports follow. We understand those too (the cursor does not freeze) and restore touch
//! mode so gestures work again.

pub mod gesture;
pub mod hid;

use crate::i2c::{self, Bus, HidDevice};
use crate::input::Event;
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::sync::IrqLock;
use crate::task;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use gesture::{Contact, Frame, Gestures};
use hid::{Kind, Layout};

/// Where touchpads usually answer on the bus. These are tried first so as not to wake the others.
const ADDRESSES: [u8; 12] = [0x2C, 0x15, 0x2A, 0x10, 0x20, 0x38, 0x41, 0x4B, 0x5D, 0x14, 0x24, 0x40];
/// HID over I2C commands (section 7.2). We do not send RESET: without the interrupt line
/// we cannot answer it properly, and the touchpad of this laptop leaves I2C after a reset
/// and sends service bytes to the PS/2 mouse.
const OP_GET_REPORT: u8 = 0x02;
const OP_SET_REPORT: u8 = 0x03;
const OP_SET_POWER: u8 = 0x08;
const POWER_ON: u8 = 0x00;
const TYPE_FEATURE: u8 = 0x03;
/// Touch mode (the "Input Mode" feature).
const MODE_TOUCHES: i32 = 3;
/// How many reports to take in one go: with two fingers there are two per frame.
const DRAIN: usize = 8;
/// How many first reports to log in full, and for how many contacts to log a summary.
const LOGGED_REPORTS: u32 = 6;
const LOGGED_TOUCHES: u32 = 10;
/// How many unusual reports (mouse, empty, foreign) to log and keep for the
/// `touchpad` command.
const LOGGED_ODD: u32 = 12;
const KEPT_ODD: usize = 6;
/// Touch mode is restored at most once per this many ticks.
const RESTORE_EVERY: u64 = HZ * 2;
/// The line calls but there is nothing to read this many times in a row — the touchpad apparently lost
/// its state: enable touch mode again.
const STUCK_LINE: u32 = 50;
/// While fingers are on the pad, poll this often (the touchpad sends a frame every ~9 ms).
const ACTIVE_POLL_MS: u64 = 2;
/// How long after the last contact to keep polling often.
const ACTIVE_TAIL: u64 = HZ / 2;
/// GPIO pin register (DW0): input level and pin mode.
const PAD_RX_STATE: u32 = 1 << 1;
const PAD_RX_DISABLED: u32 = 1 << 9;
const PAD_MODE: u32 = 7 << 10;
/// Driver switch: `touchpad off` in the terminal.
const CONFIG: &str = "/system/touchpad.cfg";

/// The driver is enabled (yes by default).
pub fn enabled() -> bool {
    let text = task::as_system(|| crate::fs::read(CONFIG).ok()).unwrap_or_default();
    !String::from_utf8_lossy(&text).lines().any(|l| l.trim() == "enabled=0")
}

/// Enables or disables the driver from the next boot.
pub fn set_enabled(on: bool) -> Result<(), String> {
    task::as_system(|| {
        let _ = crate::fs::mkdir("/system");
        crate::fs::write(CONFIG, if on { b"enabled=1\n" } else { b"enabled=0\n" }).map_err(|e| format!("{e}"))
    })
}

/// The touchpad interrupt line: the DW0 register of its pin. It calls with a low level (GpioInt
/// ActiveLow in the DSDT).
struct Line {
    dw0: usize,
    /// How many times in a row the line called while there was nothing to read.
    empty: u32,
}

impl Line {
    fn asserted(&self) -> bool {
        crate::hwprobe::rd(self.dw0, 0) & PAD_RX_STATE == 0
    }
}

/// Finds the touchpad interrupt pin in the DSDT. Logs everything it finds.
fn interrupt_line() -> Option<Line> {
    let d = crate::acpi::dsdt()?;
    let read32 = |at: u64| unsafe { core::ptr::read_volatile(at as *const u32) };
    let (gnvs, _) = crate::aml::memory_region(d, b"GNVS")?;
    let (gpdi_at, _) = crate::aml::field(d, b"GNVS", b"GPDI")?;
    let gpdi = read32(gnvs + gpdi_at);
    let mode = crate::aml::field(d, b"GNVS", b"TPDM").map(|(at, _)| read32(gnvs + at) & 0xFF);
    let (pnva, _) = crate::aml::memory_region(d, b"PNVA")?;
    let sbrg = read32(pnva + crate::aml::field(d, b"PNVA", b"SBRG")?.0) as u64;
    let series = read32(pnva + crate::aml::field(d, b"PNVA", b"PCHS")?.0) & 0xFFFF;
    let low_power = crate::aml::name_int(d, b"PCHL").is_some_and(|l| l == series as u64);
    let groups = crate::aml::packages(d, if low_power { b"GPCL" } else { b"GPCH" })?;
    let (group, pad) = ((gpdi >> 16 & 0xFF) as usize, (gpdi & 0xFFFF) as u64);
    let g = groups.get(group)?;
    kprintln!("touchpad: interrupt pad {gpdi:#x} (group {group}, pad {pad}), mode {mode:?}, side bus at {sbrg:#x}");
    if g.len() < 3 || pad >= g[1] || sbrg == 0 {
        kprintln!("touchpad: the pad is outside its group {g:x?}");
        return None;
    }
    let dw0 = sbrg + g[0] + g[2] + pad * 16;
    crate::paging::map_mmio(dw0 & !0xFFF, 0x1000);
    let value = crate::hwprobe::rd(dw0 as usize, 0);
    kprintln!("touchpad: pad register at {dw0:#x} = {value:#010x}");
    set_line(format!("pad {gpdi:#x} (group {group}, pad {pad}), register {dw0:#x}, mode {mode:?}"), dw0 as usize);
    if value == u32::MAX || value & (PAD_MODE | PAD_RX_DISABLED) != 0 {
        kprintln!("touchpad: the pad is not a plain GPIO input, reading blind");
        return None;
    }
    Some(Line { dw0: dw0 as usize, empty: 0 })
}

#[derive(Default)]
struct Status {
    text: String,
    /// The interrupt line: description and register (the level is read on every request).
    line: Option<(String, usize)>,
    reports: u64,
    frames: u64,
    stale: u64,
    failed: u64,
    /// The line called but there was nothing to read.
    empty: u64,
    /// Mouse reports, empty reports (touchpad reset), reports with other ids.
    mouse: u64,
    resets: u64,
    other: u64,
    /// How many times touch mode was restored.
    restored: u64,
    /// The last unusual reports in full.
    odd: alloc::collections::VecDeque<String>,
}

static STATUS: IrqLock<Option<Status>> = IrqLock::new(None);

fn set_text(text: String) {
    kprintln!("touchpad: {text}");
    STATUS.lock().get_or_insert_with(Status::default).text = text;
}

fn set_line(text: String, dw0: usize) {
    STATUS.lock().get_or_insert_with(Status::default).line = Some((text, dw0));
}

/// Lines for the `touchpad` command (and the touchpad.txt file of a `hwprobe` snapshot).
pub fn status() -> Vec<String> {
    let s = STATUS.lock();
    let Some(s) = s.as_ref() else { return alloc::vec!["touchpad: not started".into()] };
    let mut out = alloc::vec![format!("touchpad: {}", s.text)];
    match &s.line {
        Some((text, dw0)) => {
            let value = crate::hwprobe::rd(*dw0, 0);
            let level = if value & PAD_RX_STATE == 0 { "low (calls)" } else { "high" };
            out.push(format!("line: {text}, now {value:#010x}, {level}"));
        }
        None => out.push("line: not found, reading blind".into()),
    }
    out.push(format!("reports {}, frames {}, repeated reads {}, failed reads {}", s.reports, s.frames, s.stale, s.failed));
    out.push(format!(
        "line called with nothing to read {}, mouse reports {}, resets {}, other reports {}, touch mode restored {}, ps/2 restarts {}",
        s.empty,
        s.mouse,
        s.resets,
        s.other,
        s.restored,
        crate::input::AUX_RESTARTS.load(core::sync::atomic::Ordering::Relaxed)
    ));
    for r in &s.odd {
        out.push(format!("  {r}"));
    }
    out
}

fn count(f: impl FnOnce(&mut Status)) {
    if let Some(s) = STATUS.lock().as_mut() {
        f(s);
    }
}

/// Finds the touchpad and starts its thread.
pub fn start(devices: &[crate::pci::Device]) {
    if !enabled() {
        set_text("turned off (touchpad on, then restart)".into());
        return;
    }
    let devices = devices.to_vec();
    task::spawn("touchpad", None, move || {
        task::set_priority(task::Prio::Ui);
        let Some((name, mut bus, addr, dev, fields, layout)) = find(&devices) else {
            set_text("no I2C touchpad".into());
            return;
        };
        // Fast mode, if the device answers the same way in it.
        bus.set_fast();
        if i2c::hid_device(&bus, addr).is_none_or(|d| d.vendor != dev.vendor) {
            bus = match reopen(&devices, &name) {
                Some(b) => b,
                None => return set_text("lost the bus".into()),
            };
            kprintln!("touchpad: stays at 100 kHz");
        }
        if let Err(e) = init(&bus, addr, &dev, &layout) {
            return set_text(format!("{:04x}:{:04x}: {e}", dev.vendor, dev.product));
        }
        let mouse = hid::mouse(&fields);
        set_text(format!(
            "{:04x}:{:04x} on {name} at {addr:#04x}, {:.0}x{:.0} mm, {} finger(s) per report",
            dev.vendor,
            dev.product,
            layout.width_mm,
            layout.height_mm,
            layout.fingers.len()
        ));
        run(&bus, addr, &dev, &layout, mouse.as_ref());
    });
}

fn reopen(devices: &[crate::pci::Device], name: &str) -> Option<Bus> {
    let mut lines = Vec::new();
    i2c::controllers(devices, &mut lines).into_iter().find(|(n, _)| n == name).map(|(_, b)| b)
}

/// The first HID device on the I2C buses that has a touchpad descriptor.
fn find(devices: &[crate::pci::Device]) -> Option<(String, Bus, u8, HidDevice, Vec<hid::Field>, Layout)> {
    let mut lines = Vec::new();
    for (name, bus) in i2c::controllers(devices, &mut lines) {
        for addr in ADDRESSES {
            let Some(dev) = i2c::hid_device(&bus, addr) else { continue };
            let Some(desc) = bus.transfer(addr, &dev.report_register.to_le_bytes(), dev.report_length.min(4096)) else { continue };
            let fields = hid::parse(&desc);
            match hid::touchpad(&fields) {
                Some(layout) => return Some((name, bus, addr, dev, fields, layout)),
                None => kprintln!("touchpad: {:04x}:{:04x} on {name} at {addr:#04x} is not a touchpad", dev.vendor, dev.product),
            }
        }
    }
    for line in lines {
        kprintln!("touchpad: {line}");
    }
    None
}

/// A command to the device: report type, report id and opcode go to the command register, then data.
fn command(bus: &Bus, addr: u8, dev: &HidDevice, op: u8, kind: u8, report: u8, data: &[u8]) -> bool {
    let mut out = Vec::from(dev.command_register.to_le_bytes());
    if report < 15 {
        out.extend_from_slice(&[kind << 4 | report, op]);
    } else {
        out.extend_from_slice(&[kind << 4 | 0x0F, op, report]);
    }
    if !data.is_empty() {
        out.extend_from_slice(&dev.data_register.to_le_bytes());
        // The length counts itself (2 bytes) and the report id.
        let len = 2 + 1 + data.len();
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.push(report);
        out.extend_from_slice(data);
    }
    bus.transfer(addr, &out, 0).is_some()
}

/// A feature report: builds the data from fields and their values.
fn set_feature(bus: &Bus, addr: u8, dev: &HidDevice, values: &[(hid::Field, i32)]) -> bool {
    let Some(&(first, _)) = values.first() else { return true };
    let bytes = values.iter().map(|(f, _)| (f.offset + f.size).div_ceil(8) as usize).max().unwrap_or(1);
    let mut data = alloc::vec![0u8; bytes];
    for (field, value) in values {
        field.set(&mut data, *value);
    }
    command(bus, addr, dev, OP_SET_REPORT, TYPE_FEATURE, first.report, &data)
}

/// A feature report from the device: a command, then a read from the data register.
fn get_feature(bus: &Bus, addr: u8, dev: &HidDevice, field: &hid::Field) -> Option<i32> {
    let mut out = Vec::from(dev.command_register.to_le_bytes());
    out.extend_from_slice(&[TYPE_FEATURE << 4 | field.report, OP_GET_REPORT]);
    out.extend_from_slice(&dev.data_register.to_le_bytes());
    let bytes = (field.offset + field.size).div_ceil(8) as usize;
    let reply = bus.transfer(addr, &out, 3 + bytes)?;
    kprintln!("touchpad: feature {} reads {:02x?}", field.report, reply);
    (reply.get(2) == Some(&field.report)).then(|| field.get(&reply[3..]))
}

fn init(bus: &Bus, addr: u8, dev: &HidDevice, layout: &Layout) -> Result<(), &'static str> {
    let Some(mode) = layout.input_mode else { return Err("has no input mode switch") };
    if !command(bus, addr, dev, OP_SET_POWER, 0, POWER_ON, &[]) {
        return Err("does not take the power command");
    }
    task::sleep_until(interrupts::ticks() + 3);
    kprintln!("touchpad: input mode before: {:?}", get_feature(bus, addr, dev, &mode));
    if !set_feature(bus, addr, dev, &[(mode, MODE_TOUCHES)]) {
        return Err("does not take the input mode");
    }
    task::sleep_until(interrupts::ticks() + 2);
    match get_feature(bus, addr, dev, &mode) {
        Some(MODE_TOUCHES) => {}
        other => kprintln!("touchpad: input mode reads {other:?} after setting it, going on"),
    }
    let switches: Vec<(hid::Field, i32)> = [layout.surface_switch, layout.button_switch].into_iter().flatten().map(|f| (f, 1)).collect();
    if !set_feature(bus, addr, dev, &switches) {
        kprintln!("touchpad: does not take the surface and button switches");
    }
    Ok(())
}

/// Builds a frame from reports: in "hybrid" mode the contacts of one frame arrive as
/// separate reports, and only the first knows how many there are.
struct Assembler {
    expected: usize,
    seen: usize,
    contacts: Vec<Contact>,
    button: bool,
    time: f32,
    last_scan: Option<i32>,
}

impl Assembler {
    fn report(&mut self, layout: &Layout, data: &[u8]) -> Option<Frame> {
        let count = layout.count.map_or(layout.fingers.len() as i32, |f| f.get(data)) as usize;
        if count > 0 || self.seen >= self.expected {
            self.expected = count.max(1);
            self.seen = 0;
            self.contacts.clear();
            self.button = layout.button.is_some_and(|f| f.get(data) != 0);
            self.time = self.scan_time(layout, data);
        }
        for finger in &layout.fingers {
            if self.seen >= self.expected {
                break;
            }
            self.seen += 1;
            let tip = finger.tip.get(data) != 0;
            let sure = finger.confidence.is_none_or(|f| f.get(data) != 0);
            if tip && sure {
                let at = |f: &hid::Field, mm: f32| (f.get(data) - f.logical_min) as f32 / (f.logical_max - f.logical_min).max(1) as f32 * mm;
                self.contacts.push(Contact {
                    id: finger.id.map_or(self.seen as i32, |f| f.get(data)) as u32,
                    x: at(&finger.x, layout.width_mm),
                    y: at(&finger.y, layout.height_mm),
                });
            }
        }
        (self.seen >= self.expected).then(|| Frame { contacts: core::mem::take(&mut self.contacts), button: self.button, time: self.time })
    }

    /// Frame time in milliseconds: from the device's scan counter, or from the timer without it.
    fn scan_time(&mut self, layout: &Layout, data: &[u8]) -> f32 {
        let Some(field) = layout.scan_time else { return (interrupts::ticks() * 1000 / HZ) as f32 };
        let scan = field.get(data);
        let range = field.logical_max - field.logical_min + 1;
        // The unit is 10^exp seconds (usually 100 µs).
        let mut per_ms = 1000.0f32;
        let mut e = field.unit_exponent;
        while e < 0 {
            per_ms /= 10.0;
            e += 1;
        }
        let step = match self.last_scan {
            Some(last) => (scan - last).rem_euclid(range.max(1)) as f32 * per_ms,
            None => 0.0,
        };
        self.last_scan = Some(scan);
        // A long pause: the exact time does not matter, as long as it is not a huge jump.
        self.time + step.min(1000.0)
    }
}

/// CPU cycles per millisecond: measured against the timer so that pauses shorter than a tick have
/// the intended length.
fn cycles_per_ms() -> u64 {
    let start = interrupts::ticks() + 1;
    task::sleep_until(start);
    let t0 = crate::arch::rdtsc();
    task::sleep_until(start + HZ / 10);
    (crate::arch::rdtsc() - t0) / 100
}

/// Summary of one contact for the log: how many frames arrived and how many were lost.
#[derive(Default)]
struct Touch {
    frames: u32,
    missed: u32,
    longest: f32,
    started: f32,
    last: f32,
}

/// An unusual report: to the log (the first few) and to the list for the `touchpad` command.
fn odd(logged: &mut u32, what: &str, report: &[u8]) {
    let hex: String = report.iter().take(32).map(|b| format!("{b:02x}")).collect();
    let text = format!("{what}: {hex}");
    if *logged < LOGGED_ODD {
        *logged += 1;
        kprintln!("touchpad: {text}");
    }
    count(|s| {
        s.odd.push_back(text);
        if s.odd.len() > KEPT_ODD {
            s.odd.pop_front();
        }
    });
}

/// Re-enables touch mode (and the surface and button) unless it was done recently.
fn restore(bus: &Bus, addr: u8, dev: &HidDevice, layout: &Layout, last: &mut Option<u64>, why: &str) {
    let now = interrupts::ticks();
    if last.is_some_and(|t| now < t + RESTORE_EVERY) {
        return;
    }
    *last = Some(now);
    let mode = layout.input_mode.is_some_and(|m| set_feature(bus, addr, dev, &[(m, MODE_TOUCHES)]));
    let switches: Vec<(hid::Field, i32)> = [layout.surface_switch, layout.button_switch].into_iter().flatten().map(|f| (f, 1)).collect();
    let switches = set_feature(bus, addr, dev, &switches);
    let n = STATUS.lock().as_mut().map_or(0, |s| {
        s.restored += 1;
        s.restored
    });
    if n <= LOGGED_ODD as u64 {
        kprintln!("touchpad: {why}: touch mode set again ({}, switches {})", if mode { "taken" } else { "refused" }, if switches { "taken" } else { "refused" });
    }
}

fn run(bus: &Bus, addr: u8, dev: &HidDevice, layout: &Layout, mouse: Option<&hid::Mouse>) {
    let mut line = interrupt_line();
    if line.is_none() {
        kprintln!("touchpad: no interrupt line, reading blind");
    }
    // Read as much as the touchpad can send: it may not release a long report
    // that was not read completely.
    let length = dev.max_input.clamp(3 + report_len(layout), 256);
    let per_ms = cycles_per_ms();
    let nap = per_ms * ACTIVE_POLL_MS;
    // The touchpad releases the line shortly after a read: wait this long before deciding
    // that another report follows this one.
    let release = per_ms * 3 / 10;
    let mut touch = Touch::default();
    let mut touches = 0;
    let mut gestures = Gestures::new(layout.width_mm, layout.height_mm);
    let mut assembler = Assembler { expected: 0, seen: 0, contacts: Vec::new(), button: false, time: 0.0, last_scan: None };
    let mut last = Vec::new();
    let mut logged = 0;
    let mut odd_logged = 0;
    let mut restored = None;
    // Buttons held by a mouse report: they must be released when switching to touch.
    let mut mouse_buttons = 0u8;
    let mut active_until = 0;
    loop {
        for _ in 0..DRAIN {
            // The line is not calling — nothing to read (and the touchpad does not hold the bus).
            if line.as_ref().is_some_and(|l| !l.asserted()) {
                break;
            }
            let Some(buf) = bus.transfer(addr, &[], length) else {
                count(|s| s.failed += 1);
                break;
            };
            if let Some(l) = &line {
                let end = crate::hwprobe::deadline(release);
                while l.asserted() && !crate::hwprobe::expired(end) {
                    core::hint::spin_loop();
                }
            }
            let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
            let report = &buf[..len.min(buf.len())];
            if len <= 3 {
                // It called but returned nothing: this is how the touchpad reports its reset. If it
                // repeats, the line is "stuck" — the touchpad lost its state, wake touch mode.
                if let Some(l) = line.as_mut() {
                    l.empty += 1;
                    count(|s| s.empty += 1);
                    if l.empty == 1 {
                        odd(&mut odd_logged, "empty report while the line calls", &buf[..4.min(buf.len())]);
                    }
                    if l.empty % STUCK_LINE == 0 {
                        restore(bus, addr, dev, layout, &mut restored, "the line keeps calling with nothing to read");
                    }
                }
                break;
            }
            if let Some(l) = line.as_mut() {
                l.empty = 0;
            }
            // The same touch report a second time is a repeated read (touch reports have different
            // scan times). Identical mouse reports are just smooth steady motion.
            let from_mouse = mouse.is_some_and(|m| m.report == report[2]);
            if !from_mouse && report == last.as_slice() {
                count(|s| s.stale += 1);
                break;
            }
            last = report.to_vec();
            count(|s| s.reports += 1);
            if logged < LOGGED_REPORTS {
                logged += 1;
                let hex: String = report.iter().map(|b| format!("{b:02x}")).collect();
                kprintln!("touchpad: report {hex}");
            }
            // The touchpad talks over I2C: do not listen to its PS/2 copy for now.
            crate::input::PS2_AUX_QUIET_UNTIL.store(interrupts::ticks() + HZ, core::sync::atomic::Ordering::Relaxed);
            if let Some(m) = mouse.filter(|_| from_mouse) {
                // Mouse mode: the touchpad reset and forgot about contacts. Move the cursor from
                // these reports, and restore touch mode.
                let (buttons, dx, dy, wheel) = m.read(&report[3..]);
                mouse_buttons = buttons;
                crate::input::push([Event::MouseMove { dx, dy, buttons, wheel }]);
                count(|s| s.mouse += 1);
                odd(&mut odd_logged, "mouse report", report);
                restore(bus, addr, dev, layout, &mut restored, "mouse reports");
                active_until = interrupts::ticks() + ACTIVE_TAIL;
                continue;
            }
            if report[2] != layout.report {
                count(|s| s.other += 1);
                odd(&mut odd_logged, "other report", report);
                continue;
            }
            active_until = interrupts::ticks() + ACTIVE_TAIL;
            if mouse_buttons != 0 {
                mouse_buttons = 0;
                crate::input::push([Event::MouseMove { dx: 0, dy: 0, buttons: 0, wheel: 0 }]);
            }
            if let Some(frame) = assembler.report(layout, &report[3..]) {
                count(|s| s.frames += 1);
                // A frame comes every ~9 ms: a gap longer than one and a half frames is a loss.
                if touch.frames > 0 {
                    let gap = frame.time - touch.last;
                    touch.longest = touch.longest.max(gap);
                    if gap > 14.0 {
                        touch.missed += (gap / 9.0) as u32;
                    }
                } else {
                    touch.started = frame.time;
                }
                touch.frames += 1;
                touch.last = frame.time;
                if frame.contacts.is_empty() {
                    if touches < LOGGED_TOUCHES {
                        touches += 1;
                        kprintln!(
                            "touchpad: touch {:.0} ms, {} frames, about {} missed, longest gap {:.0} ms",
                            touch.last - touch.started,
                            touch.frames,
                            touch.missed,
                            touch.longest
                        );
                    }
                    touch = Touch::default();
                }
                for step in gestures.frame(&frame) {
                    crate::input::push([Event::MouseMove { dx: step.dx, dy: step.dy, buttons: step.buttons, wheel: step.wheel }]);
                }
            }
        }
        // While fingers are on the pad, poll every few milliseconds (the fast timer
        // wakes us from a short pause), otherwise every two ticks. With the interrupt line, polling is
        // one register read, and without fingers a tick is enough.
        let active = interrupts::ticks() < active_until;
        if line.is_some() && !active {
            interrupts::fast_wakeups_for(interrupts::FAST_FOR_TOUCHPAD, false);
            task::sleep_until(interrupts::ticks() + 1);
            continue;
        }
        if interrupts::fast_wakeups_for(interrupts::FAST_FOR_TOUCHPAD, active) && active {
            task::nap(nap);
        } else {
            task::sleep_until(interrupts::ticks() + 2);
        }
    }
}

/// Length of a touch report's data from the descriptor fields.
fn report_len(layout: &Layout) -> usize {
    let mut fields: Vec<hid::Field> = Vec::new();
    for f in &layout.fingers {
        fields.extend([f.tip, f.x, f.y]);
        fields.extend(f.confidence);
        fields.extend(f.id);
    }
    fields.extend(layout.count);
    fields.extend(layout.button);
    fields.extend(layout.scan_time);
    hid::report_bytes(&fields, Kind::Input, layout.report)
}
