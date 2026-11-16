//! USB: the xHCI controller and HID input devices — keyboard, mouse and tablet
//! (absolute coordinates, like QEMU's `usb-tablet`). Events go to the shared
//! input queue (`input::USB_EVENTS`), so the WM does not care where they come from.
//!
//! At startup the "usb" thread walks the ports, configures the devices it finds and
//! then processes controller events (HID reports, connections) once a tick.

pub mod bt;
mod hub;
pub mod net;
mod storage;
mod xhci;

use crate::input::Event;
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::pci;
use crate::sync::YieldLock;
use crate::task;
use alloc::string::String;
use alloc::vec::Vec;
use xhci::{Ring, Trb, Xhci, dma};

/// Where a device is connected: the controller's root port and the path through hubs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Loc {
    root: u32,
    /// Route string: 4 bits per hub along the path — the port number on that hub.
    route: u32,
    /// How many hubs are between the device and the controller.
    depth: u32,
    /// A low/full-speed device behind a USB 2.0 hub: (hub slot, its port) — the TT "translator".
    tt: Option<(u8, u8)>,
}

impl Loc {
    fn root(port: u32) -> Loc {
        Loc { root: port, route: 0, depth: 0, tt: None }
    }

    /// Device `other` is connected here or deeper (behind hubs at this place).
    fn contains(&self, other: &Loc) -> bool {
        let mask = (1u64 << (4 * self.depth)) as u32 - 1;
        other.root == self.root && other.depth >= self.depth && other.route & mask == self.route
    }
}

impl core::fmt::Display for Loc {
    /// "port 3" or "port 3.2.1" (through hubs).
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "port {}", self.root)?;
        for i in 0..self.depth {
            write!(f, ".{}", (self.route >> (4 * i)) & 0xF)?;
        }
        Ok(())
    }
}

/// Slot context: route, speed, number of entries, root port, TT.
unsafe fn slot_ctx(sl: *mut u32, loc: &Loc, speed: u32, entries: u32) {
    unsafe {
        *sl = loc.route | speed << 20 | entries << 27;
        *sl.add(1) = loc.root << 16;
        if let Some((hub, port)) = loc.tt {
            *sl.add(2) = hub as u32 | (port as u32) << 8;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Keyboard,
    Mouse,
    Tablet,
}

struct Hid {
    slot: u8,
    loc: Loc,
    kind: Kind,
    dci: u32,
    ring: Ring,
    buf: *mut u8,
    len: usize,
    /// Keyboard: pressed keys, modifiers, auto-repeat.
    kb: crate::hid::Keyboard,
    buttons: u8,
    /// Report descriptor (the device's own format); None — the boot format.
    layout: Option<crate::hid::report::Layout>,
    /// Keyboard reports by id (NKRO keyboards may have several).
    keyboards: Vec<(u8, crate::hid::Keyboard)>,
    media: crate::hid::Media,
    /// Report ids already reported as "nothing useful".
    told: Vec<u8>,
}

unsafe impl Send for Hid {}

struct Usb {
    hc: Xhci,
    devices: Vec<Hid>,
    disks: Vec<storage::Storage>,
    hubs: Vec<hub::Hub>,
    /// USB Ethernet adapters.
    nets: Vec<net::UsbNet>,
    /// Bluetooth controllers.
    bts: Vec<bt::Bt>,
    /// What is visible on the bus (for `lsusb`): devices, including those without a driver, and errors.
    seen: Vec<(Loc, String)>,
    /// When the hub ports were last polled.
    hubs_polled: u64,
    /// Ports that already have a configured (or unsupported) device.
    attached: Vec<u32>,
    pos: (i32, i32),
}

/// How many times to try configuring a device.
const TRIES: u32 = 3;
/// Pause after a port reset before talking to the device (10 ms by the standard).
const RESET_RECOVERY: u64 = 50;

/// The controller and its devices. The "usb" thread takes the lock for each polling
/// step, and disk reads and writes take it for the duration of their transfer.
static USB: YieldLock<Option<Usb>> = YieldLock::new(None);

/// Find the controller, enumerate devices (right away, so a USB stick is available
/// to the file system already during boot) and start the USB thread.
pub fn init(devices: &[pci::Device], screen: (i32, i32)) -> String {
    let Some(dev) = devices.iter().find(|d| d.class == 0x0C && d.subclass == 0x03 && (d.read(0x08) >> 8) & 0xFF == 0x30)
    else {
        return "USB: no xHCI controller".into();
    };
    let Some(hc) = Xhci::init(dev) else { return "USB: controller failed to start".into() };
    let mut usb =
        Usb {
            hc,
            devices: Vec::new(),
            disks: Vec::new(),
            hubs: Vec::new(),
            nets: Vec::new(),
            bts: Vec::new(),
            seen: Vec::new(),
            hubs_polled: 0,
            attached: Vec::new(),
            pos: (screen.0 / 2, screen.1 / 2),
        };
    // Devices need a little time after the ports are powered.
    let t = interrupts::ticks();
    while interrupts::ticks() < t + HZ / 5 {
        core::hint::spin_loop();
    }
    usb.scan_ports();
    let hubs = if usb.hubs.is_empty() { String::new() } else { alloc::format!(", {} hubs", usb.hubs.len()) };
    let summary = alloc::format!("USB: xHCI, {} input, {} storage{hubs}", usb.devices.len(), usb.disks.len());
    // Controller events (key presses, mouse motion) wake the thread with an interrupt, not with a tick.
    let irq = crate::irq::msi(dev, "usb");
    if irq.is_some() {
        usb.hc.enable_interrupts();
    }
    *USB.lock() = Some(usb);
    task::spawn("usb", None, move || {
        // Keyboard and mouse: a person is waiting for a response.
        task::set_priority(task::Prio::Ui);
        loop {
            let mut wait = 1;
            if let Some(usb) = USB.lock().as_mut() {
                usb.poll();
                if irq.is_some() {
                    usb.hc.ack_interrupt();
                    wait = usb.quiet_ticks();
                }
            }
            let deadline = interrupts::ticks() + wait;
            match &irq {
                Some(irq) => {
                    irq.wait(deadline);
                }
                None => task::sleep_until(deadline),
            }
        }
    });
    summary
}

/// The list for `lsusb`: "location: what it is".
pub fn list() -> Vec<String> {
    let guard = USB.lock();
    let Some(usb) = guard.as_ref() else { return alloc::vec![String::from("no xHCI controller")] };
    let mut items: Vec<&(Loc, String)> = usb.seen.iter().collect();
    // By path: port 5, 5.4, 5.4.1, 5.5 — devices behind a hub right after it.
    items.sort_by_key(|(l, _)| (l.root, (0..l.depth).map(|i| (l.route >> (4 * i)) & 0xF).collect::<Vec<_>>()));
    items.iter().map(|(l, t)| alloc::format!("{l}: {t}")).collect()
}

/// Grows when a USB disk is connected or removed (the FS refreshes /media by it).
static DISKS_VERSION: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Disk numbers are never reused: unplugged and plugged back in means a new number.
static NEXT_DISK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

pub fn disks_version() -> u64 {
    DISKS_VERSION.load(core::sync::atomic::Ordering::Relaxed)
}

/// Connected USB disks: (number, 512-byte sectors).
pub fn disks() -> Vec<(u32, u64)> {
    USB.lock().as_ref().map_or(Vec::new(), |u| u.disks.iter().map(|d| (d.id, d.blocks)).collect())
}

pub fn disk_read(disk: u32, lba: u32, buf: &mut [u8]) -> bool {
    let mut guard = USB.lock();
    let Some(usb) = guard.as_mut() else { return false };
    let Some(d) = usb.disks.iter_mut().find(|d| d.id == disk) else { return false };
    let done = d.transfer(&mut usb.hc, lba, buf, false);
    give_way(guard);
    done
}

pub fn disk_write(disk: u32, lba: u32, data: &[u8]) -> bool {
    let mut guard = USB.lock();
    let Some(usb) = guard.as_mut() else { return false };
    let Some(d) = usb.disks.iter_mut().find(|d| d.id == disk) else { return false };
    let mut copy = data.to_vec();
    let done = d.transfer(&mut usb.hc, lba, &mut copy, true);
    give_way(guard);
    done
}

/// Releases the controller and yields: otherwise the next chunk would take it right away, and the
/// keyboard and mouse thread would not get its turn during a whole write to the stick.
fn give_way(guard: crate::sync::YieldGuard<'_, Option<Usb>>) {
    drop(guard);
    task::yield_now();
}

fn ctx(base: *mut u8, size: usize, index: usize) -> *mut u32 {
    unsafe { base.add(size * index) as *mut u32 }
}

impl Usb {
    fn scan_ports(&mut self) {
        for port in 1..=self.hc.ports {
            if self.hc.port_connected(port) && !self.attached.contains(&port) {
                self.attached.push(port);
                self.attach_root(port);
            }
        }
        // Devices behind hubs (and behind hubs in hubs — the list grows as we go).
        let mut i = 0;
        while i < self.hubs.len() {
            self.scan_hub(i);
            i += 1;
        }
    }

    /// A device on a controller port. If it fails — reset the port and try again
    /// (as Linux does: some devices do not answer the first time).
    fn attach_root(&mut self, port: u32) {
        let loc = Loc::root(port);
        for attempt in 1..=TRIES {
            if !self.hc.reset_port(port, attempt > 1) {
                kprintln!("usb: {loc}: port did not enable");
                return;
            }
            hub::sleep_ms(RESET_RECOVERY);
            let speed = self.hc.port_speed(port);
            match self.attach(loc, speed) {
                Ok(()) => return,
                Err(e) => {
                    kprintln!("usb: {loc}: {e} (attempt {attempt} of {TRIES})");
                    self.failed(loc, e);
                }
            }
            if !self.hc.port_connected(port) {
                return;
            }
            hub::sleep_ms(100);
        }
    }

    /// The device (and everything behind it) was disconnected: forget it and free the slots.
    fn detach(&mut self, at: Loc) {
        let mut slots = Vec::new();
        self.devices.retain(|d| !at.contains(&d.loc) || (slots.push(d.slot), false).1);
        let disks = self.disks.len();
        self.disks.retain(|d| !at.contains(&d.loc) || (slots.push(d.slot), false).1);
        if self.disks.len() != disks {
            DISKS_VERSION.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        self.hubs.retain(|h| !at.contains(&h.loc) || (slots.push(h.slot), false).1);
        self.bts.retain(|b| !at.contains(&b.loc) || (slots.push(b.slot), false).1);
        let nets = self.nets.len();
        self.nets.retain(|n| !at.contains(&n.loc) || (slots.push(n.slot), false).1);
        if self.nets.len() != nets {
            net::unplugged();
        }
        for slot in slots {
            self.hc.disable_slot(slot);
        }
        self.seen.retain(|(l, _)| !at.contains(l));
        kprintln!("usb: {at}: disconnected");
    }

    /// The report descriptor of a HID interface (GET_DESCRIPTOR 0x22): usable if it contains a
    /// keyboard. The log gets which reports exist (without contents).
    fn report_layout(&mut self, slot: u8, ep0: &mut Ring, buf: *mut u8, iface: u8, len: u16, (vendor, product): (u16, u16)) -> Option<crate::hid::report::Layout> {
        let why = if len == 0 || len > 4096 {
            "no report descriptor length"
        } else if self.hc.control_retry(slot, ep0, [0x81, 6, 0, 0x22, iface, 0, len as u8, (len >> 8) as u8], buf, len).is_none() {
            "the report descriptor did not come"
        } else {
            let desc = unsafe { core::slice::from_raw_parts(buf, len as usize) };
            match crate::hid::report::parse(desc) {
                Some(l) if l.reports.iter().any(|r| r.has_keyboard()) => {
                    let list: Vec<String> = l.reports.iter().map(|r| alloc::format!("{} {}", r.id, r.kind())).collect();
                    kprintln!("usb: {vendor:04x}:{product:04x} interface {iface}: own report format ({})", list.join(", "));
                    return Some(l);
                }
                Some(_) => "no keyboard in the report descriptor",
                None => "the report descriptor did not parse",
            }
        };
        kprintln!("usb: {vendor:04x}:{product:04x} interface {iface}: boot format ({why})");
        None
    }

    /// The device is configured (or recognized): to the log and the `lsusb` list.
    fn report(&mut self, loc: Loc, text: String) {
        kprintln!("usb: {loc}: {text}");
        self.seen.retain(|(l, t)| !(*l == loc && t.starts_with("error")));
        self.seen.push((loc, text));
    }

    /// Configuration failed: the `lsusb` list keeps the last error.
    fn failed(&mut self, loc: Loc, e: String) {
        self.seen.retain(|(l, _)| *l != loc);
        self.seen.push((loc, alloc::format!("error: {e}")));
    }

    /// Configures a device. An error carries the controller's code; the slot is freed.
    fn attach(&mut self, loc: Loc, speed: u32) -> Result<(), String> {
        let slot = self.hc.enable_slot().ok_or("no free device slot")?;
        self.hc.last_error = xhci::SUCCESS;
        let result = self.setup(slot, loc, speed);
        result.map_err(|e| {
            let code = self.hc.last_error;
            self.hc.disable_slot(slot);
            let speed = ["?", "full speed", "low speed", "high speed", "SuperSpeed"].get(speed as usize).unwrap_or(&"SuperSpeed+");
            if code == xhci::SUCCESS {
                alloc::format!("{e}, {speed}")
            } else {
                alloc::format!("{e}: {} (code {code}), {speed}", xhci::code_name(code))
            }
        })
    }

    /// Address, descriptors, configuration and endpoints of the device in slot `slot`.
    fn setup(&mut self, slot: u8, loc: Loc, speed: u32) -> Result<(), &'static str> {
        let cs = self.hc.ctx_size;
        let output = dma(cs * 32);
        self.hc.set_output_context(slot, output as u64);
        let input = dma(cs * 33);
        let mut ep0 = Ring::new();
        let mps0: u32 = match speed {
            3 => 64,
            4.. => 512,
            _ => 8,
        };
        unsafe {
            *ctx(input, cs, 0).add(1) = 0b11; // add the slot and EP0
            slot_ctx(ctx(input, cs, 1), &loc, speed, 1);
            let ep = ctx(input, cs, 2);
            *ep.add(1) = 3 << 1 | 4 << 3 | mps0 << 16; // CErr 3, control endpoint
            let deq = ep0.addr() | 1;
            *ep.add(2) = deq as u32;
            *ep.add(3) = (deq >> 32) as u32;
            *ep.add(4) = 8;
        }
        if !self.hc.address_device(slot, input as u64) {
            return Err("address device failed");
        }
        // A device is given 2 ms to change its address; some need more.
        hub::sleep_ms(10);
        let buf = dma(4096);
        let get = |hc: &mut Xhci, ep0: &mut Ring, kind: u16, len: u16| {
            hc.control_retry(slot, ep0, [0x80, 6, kind as u8, (kind >> 8) as u8, 0, 0, len as u8, (len >> 8) as u8], buf, len)
        };
        // First 8 bytes: they hold the real EP0 packet size (if it differs, we fix it).
        get(&mut self.hc, &mut ep0, 0x0100, 8).ok_or("no device descriptor")?;
        let real = unsafe { *buf.add(7) } as u32;
        if speed < 3 && real != mps0 && real != 0 {
            unsafe {
                *ctx(input, cs, 0).add(1) = 0b10;
                *ctx(input, cs, 2).add(1) = 3 << 1 | 4 << 3 | real << 16;
            }
            self.hc.evaluate_context(slot, input as u64);
        }
        get(&mut self.hc, &mut ep0, 0x0100, 18).ok_or("no device descriptor")?;
        let d = unsafe { core::slice::from_raw_parts(buf, 18) };
        let (vendor, product) = (u16::from_le_bytes([d[8], d[9]]), u16::from_le_bytes([d[10], d[11]]));
        // Device class and number of configurations — now: the buffer will be reused for configurations.
        let (dev_class, configs) = (d[4], d[17].max(1));

        // Like Windows: 255 bytes at once (cheap devices only test this way),
        // and if the configuration is longer — once more in full.
        get(&mut self.hc, &mut ep0, 0x0200, 255).ok_or("no configuration")?;
        let total = (u16::from_le_bytes(unsafe { [*buf.add(2), *buf.add(3)] }) as usize).clamp(9, 1024);
        if total > 255 {
            get(&mut self.hc, &mut ep0, 0x0200, total as u16).ok_or("no configuration")?;
        }
        let conf = unsafe { core::slice::from_raw_parts(buf, total) }.to_vec();
        let config_value = conf[5];
        // USB networking (CDC Ethernet) is not always in the first configuration (RTL8153 has it in the second).
        let mut rndis = None;
        for index in 0..configs {
            let c = if index == 0 {
                conf.clone()
            } else {
                let kind = 0x0200 | index as u16;
                if get(&mut self.hc, &mut ep0, kind, 9).is_none() {
                    continue;
                }
                let len = (u16::from_le_bytes(unsafe { [*buf.add(2), *buf.add(3)] }) as usize).clamp(9, 1024);
                if get(&mut self.hc, &mut ep0, kind, len as u16).is_none() {
                    continue;
                }
                unsafe { core::slice::from_raw_parts(buf, len) }.to_vec()
            };
            if let Some(ecm) = net::find_ecm(&c) {
                return self.attach_net(slot, loc, speed, &mut ep0, buf, input, c[5], ecm, (vendor, product));
            }
            rndis = rndis.or_else(|| net::find_rndis(&c).map(|r| (c[5], r)));
        }
        // No ECM found — RNDIS (Android phones in tethering mode).
        if let Some((config, r)) = rndis {
            return self.attach_net(slot, loc, speed, &mut ep0, buf, input, config, r, (vendor, product));
        }
        if let Some(found) = bt::find(&conf) {
            return self.attach_bt(slot, loc, speed, ep0, buf, input, config_value, found, (vendor, product));
        }
        // A hub: class 9 on the device (or on its first interface).
        if dev_class == 9 || conf.get(9 + 5) == Some(&9) {
            return self.attach_hub(slot, loc, speed, ep0, buf, input, config_value, (vendor, product));
        }

        // HID interfaces and their interrupt IN endpoints (a wireless keyboard and mouse
        // receiver has several), or a mass storage device with two bulk endpoints.
        // kind, interface, endpoint, packet, interval, report descriptor length (HID descriptor)
        let mut found: Vec<(Kind, u8, u8, u16, u8, u16)> = Vec::new();
        let mut report_len = 0u16;
        let mut mass = false;
        let (mut bulk_in, mut bulk_out): (Option<(u8, u16)>, Option<(u8, u16)>) = (None, None);
        let (mut i, mut iface, mut kind) = (0, 0u8, None);
        while i + 2 <= conf.len() {
            let (len, ty) = (conf[i] as usize, conf[i + 1]);
            if len == 0 {
                break;
            }
            match ty {
                4 if i + 9 <= conf.len() => {
                    iface = conf[i + 2];
                    report_len = 0;
                    let (class, sub, proto) = (conf[i + 5], conf[i + 6], conf[i + 7]);
                    mass = class == 8 && sub == 6 && proto == 0x50;
                    kind = match (class, sub, proto) {
                        (3, 1, 1) => Some(Kind::Keyboard),
                        (3, 1, 2) => Some(Kind::Mouse),
                        // The QEMU tablet (and similar): HID without the boot protocol.
                        (3, 0, _) if vendor == 0x0627 => Some(Kind::Tablet),
                        _ => None,
                    };
                }
                // HID descriptor: the report descriptor length (type 0x22).
                0x21 if i + 9 <= conf.len() && conf[i + 6] == 0x22 => {
                    report_len = u16::from_le_bytes([conf[i + 7], conf[i + 8]]);
                }
                5 if i + 7 <= conf.len() && mass => {
                    let (addr, attr) = (conf[i + 2], conf[i + 3]);
                    let mps = u16::from_le_bytes([conf[i + 4], conf[i + 5]]);
                    if attr & 3 == 2 {
                        if addr & 0x80 != 0 { bulk_in.get_or_insert((addr & 0xF, mps)) } else { bulk_out.get_or_insert((addr & 0xF, mps)) };
                    }
                }
                5 if i + 7 <= conf.len() => {
                    let (addr, attr) = (conf[i + 2], conf[i + 3]);
                    if let Some(k) = kind
                        && addr & 0x80 != 0
                        && attr & 3 == 3
                    {
                        found.push((k, iface, addr & 0xF, u16::from_le_bytes([conf[i + 4], conf[i + 5]]), conf[i + 6], report_len));
                        kind = None; // one endpoint per interface
                    }
                }
                _ => {}
            }
            i += len;
        }
        if let (Some(bin), Some(bout)) = (bulk_in, bulk_out) {
            return self.attach_storage(slot, loc, speed, &mut ep0, buf, input, config_value, bin, bout, (vendor, product));
        }
        if found.is_empty() {
            let class = conf.get(9 + 5).copied().unwrap_or(0);
            self.report(loc, alloc::format!("device {vendor:04x}:{product:04x} class {class:02x} (no driver)"));
            return Ok(());
        }

        let ctl = |hc: &mut Xhci, ep0: &mut Ring, req: [u8; 8]| hc.control_retry(slot, ep0, req, buf, 0);
        ctl(&mut self.hc, &mut ep0, [0, 9, config_value, 0, 0, 0, 0, 0]).ok_or("set configuration failed")?;
        let mut layouts = Vec::new();
        for &(kind, iface, _, _, _, report_len) in &found {
            let mut layout = None;
            if kind == Kind::Keyboard {
                // A keyboard is read in its own format via the report descriptor: many gaming keyboards
                // interpret the "boot" protocol their own way (report ids, NKRO, lighting service
                // reports) — and stray bytes turned into key presses.
                layout = self.report_layout(slot, &mut ep0, buf, iface, report_len, (vendor, product));
            }
            if kind != Kind::Tablet {
                // Protocol: 1 — own format (by descriptor), 0 — boot (simple, no parsing).
                let _ = ctl(&mut self.hc, &mut ep0, [0x21, 0x0B, layout.is_some() as u8, 0, iface, 0, 0, 0]);
            }
            let _ = ctl(&mut self.hc, &mut ep0, [0x21, 0x0A, 0, 0, iface, 0, 0, 0]); // SET_IDLE
            layouts.push(layout);
        }

        // Interrupt endpoints: context index = 2 * number + 1 (IN). All in one command.
        let rings: Vec<Ring> = found.iter().map(|_| Ring::new()).collect();
        let last_dci = found.iter().map(|f| f.2 as u32 * 2 + 1).max().unwrap_or(1);
        unsafe {
            for k in 0..cs * 33 / 4 {
                *(input as *mut u32).add(k) = 0;
            }
            slot_ctx(ctx(input, cs, 1), &loc, speed, last_dci);
            let mut add = 1;
            for (&(_, _, ep, mps, interval, _), ring) in found.iter().zip(&rings) {
                let dci = ep as u32 * 2 + 1;
                add |= 1 << dci;
                let xinterval = if speed >= 3 {
                    (interval as u32).clamp(1, 16) - 1
                } else {
                    // Milliseconds → a power of two in microframes (125 µs).
                    31 - ((interval as u32).max(1) * 8).leading_zeros()
                };
                let e = ctx(input, cs, 1 + dci as usize);
                *e = xinterval << 16;
                *e.add(1) = 3 << 1 | 7 << 3 | (mps as u32) << 16;
                let deq = ring.addr() | 1;
                *e.add(2) = deq as u32;
                *e.add(3) = (deq >> 32) as u32;
                *e.add(4) = mps as u32 | (mps as u32) << 16;
            }
            *ctx(input, cs, 0).add(1) = add;
        }
        if !self.hc.configure_endpoint(slot, input as u64) {
            return Err("configure endpoint failed");
        }
        for (((kind, _, ep, mps, ..), ring), layout) in found.into_iter().zip(rings).zip(layouts) {
            let name = match kind {
                Kind::Keyboard => "keyboard",
                Kind::Mouse => "mouse",
                Kind::Tablet => "tablet",
            };
            self.report(loc, alloc::format!("{name} {vendor:04x}:{product:04x}"));
            let mut hid = Hid {
                slot,
                loc,
                kind,
                dci: ep as u32 * 2 + 1,
                ring,
                buf: dma(64),
                len: (mps as usize).min(64),
                kb: crate::hid::Keyboard::new(),
                buttons: 0,
                layout,
                keyboards: Vec::new(),
                media: crate::hid::Media,
                told: Vec::new(),
            };
            hid.request(&self.hc);
            self.devices.push(hid);
        }
        Ok(())
    }

    /// Mass storage: two bulk endpoints, then SCSI — the medium size.
    #[allow(clippy::too_many_arguments)]
    fn attach_storage(
        &mut self,
        slot: u8,
        loc: Loc,
        speed: u32,
        ep0: &mut Ring,
        buf: *mut u8,
        input: *mut u8,
        config: u8,
        (in_ep, in_mps): (u8, u16),
        (out_ep, out_mps): (u8, u16),
        (vendor, product): (u16, u16),
    ) -> Result<(), &'static str> {
        self.hc.control_retry(slot, ep0, [0, 9, config, 0, 0, 0, 0, 0], buf, 0).ok_or("set configuration failed")?;
        let cs = self.hc.ctx_size;
        let (in_dci, out_dci) = (in_ep as u32 * 2 + 1, out_ep as u32 * 2);
        let (in_ring, out_ring) = (Ring::new(), Ring::new());
        unsafe {
            for k in 0..cs * 33 / 4 {
                *(input as *mut u32).add(k) = 0;
            }
            *ctx(input, cs, 0).add(1) = 1 | 1 << in_dci | 1 << out_dci;
            slot_ctx(ctx(input, cs, 1), &loc, speed, in_dci.max(out_dci));
            for (dci, ty, mps, ring) in [(in_dci, 6u32, in_mps, &in_ring), (out_dci, 2, out_mps, &out_ring)] {
                let e = ctx(input, cs, 1 + dci as usize);
                *e.add(1) = 3 << 1 | ty << 3 | (mps as u32) << 16;
                let deq = ring.addr() | 1;
                *e.add(2) = deq as u32;
                *e.add(3) = (deq >> 32) as u32;
                *e.add(4) = 3072;
            }
        }
        if !self.hc.configure_endpoint(slot, input as u64) {
            return Err("configure bulk endpoints failed");
        }
        let id = NEXT_DISK.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let mut disk = storage::Storage::new(id, slot, loc, in_dci, out_dci, in_ring, out_ring);
        if !disk.start(&mut self.hc) {
            return Err("storage device not ready (or not 512-byte sectors)");
        }
        self.report(loc, alloc::format!("storage {vendor:04x}:{product:04x}, {} MiB", disk.blocks / 2048));
        self.disks.push(disk);
        DISKS_VERSION.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// How many ticks we may sleep if the controller does not call with an interrupt: key
    /// auto-repeat, Bluetooth and network cards keep their own clocks; hubs are polled twice a second.
    fn quiet_ticks(&self) -> u64 {
        let repeating = self.devices.iter().any(|d| d.kb.repeating() || d.keyboards.iter().any(|(_, k)| k.repeating()));
        if repeating || !self.bts.is_empty() || !self.nets.is_empty() {
            return 1;
        }
        if !self.hubs.is_empty() {
            return (self.hubs_polled + HZ / 2).saturating_sub(interrupts::ticks()).max(1);
        }
        HZ
    }

    fn poll(&mut self) {
        let started = interrupts::ticks();
        // Bluetooth goes before event processing: while it waits for a controller answer, keyboard reports
        // are deferred, and they must be processed before the auto-repeat check below — otherwise a key
        // released during the wait would "repeat".
        for b in self.bts.iter_mut() {
            b.service(&mut self.hc);
        }
        let bt_ticks = interrupts::ticks() - started;
        while let Some(e) = self.hc.next_event() {
            match e.kind() {
                xhci::EV_TRANSFER => self.on_transfer(e),
                xhci::EV_PORT => {
                    let port = (e.0[0] >> 24) & 0xFF;
                    if self.hc.port_connected(port) {
                        if !self.attached.contains(&port) {
                            self.attached.push(port);
                            self.attach_root(port);
                        }
                    } else if self.attached.contains(&port) {
                        // Disconnected: forget the device on this port (and everything behind it).
                        self.attached.retain(|&p| p != port);
                        self.detach(Loc::root(port));
                    }
                    // Clear the port change flags.
                    let p = self.hc.portsc(port);
                    let v = unsafe { p.read_volatile() };
                    unsafe { p.write_volatile(v & !(1 << 1 | 0x7F << 17) | (v & 0x7F << 17)) };
                }
                _ => {}
            }
        }
        for n in &mut self.nets {
            n.flush(&self.hc);
        }
        // Hubs report connections through their interrupt endpoint; it is simpler
        // to ask their ports twice a second.
        let now = interrupts::ticks();
        if !self.hubs.is_empty() && now >= self.hubs_polled + HZ / 2 {
            self.hubs_polled = now;
            for i in 0..self.hubs.len() {
                if i < self.hubs.len() {
                    self.scan_hub(i);
                }
            }
        }
        // Key auto-repeat.
        let mut out = Vec::new();
        for d in &mut self.devices {
            d.kb.tick(now, &mut out);
            for (_, kb) in &mut d.keyboards {
                kb.tick(now, &mut out);
            }
        }
        crate::input::count_keys(&crate::input::KEYS_USB, &out);
        crate::hid::push(out);
        // Monitor: a long pass delays input (keyboard, mouse). Only the time is logged —
        // not what was pressed; at most once a second.
        static LOGGED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let took = interrupts::ticks() - started;
        if took > HZ / 10 && interrupts::ticks() >= LOGGED.load(core::sync::atomic::Ordering::Relaxed) + HZ {
            LOGGED.store(interrupts::ticks(), core::sync::atomic::Ordering::Relaxed);
            crate::kprintln!("usb: slow pass {} ms (bluetooth {} ms)", took * 1000 / HZ, bt_ticks * 1000 / HZ);
        }
    }

    fn on_transfer(&mut self, e: Trb) {
        for n in &mut self.nets {
            if n.on_transfer(&self.hc, &e) {
                return;
            }
        }
        for b in &mut self.bts {
            if b.on_transfer(&self.hc, &e) {
                return;
            }
        }
        let Some(i) = self.devices.iter().position(|d| d.slot == e.slot() && d.dci == (e.0[3] >> 16) & 0x1F) else { return };
        let ok = e.code() == xhci::SUCCESS || e.code() == xhci::SHORT_PACKET;
        let desk = crate::output::bounds();
        let (w, h) = (desk.w, desk.h);
        let d = &mut self.devices[i];
        // Only what arrived: the tail of the buffer holds bytes of previous reports.
        let got = d.len.saturating_sub((e.0[2] & 0xFF_FFFF) as usize).min(d.len);
        let r = unsafe { core::slice::from_raw_parts(d.buf, got) }.to_vec();
        let mut out = Vec::new();
        if ok {
            match d.kind {
                Kind::Keyboard if d.layout.is_some() => d.own_report(&r, &mut out),
                Kind::Keyboard => d.kb.report(&r, &mut out),
                Kind::Mouse => crate::hid::mouse(&r, &mut out),
                Kind::Tablet if r.len() >= 5 => {
                    let x = u16::from_le_bytes([r[1], r[2]]) as i32 * w / 0x8000;
                    let y = u16::from_le_bytes([r[3], r[4]]) as i32 * h / 0x8000;
                    let wheel = -(*r.get(5).unwrap_or(&0) as i8 as i32);
                    self.pos = (x.clamp(0, w - 1), y.clamp(0, h - 1));
                    out.push(Event::Mouse { x: self.pos.0, y: self.pos.1, buttons: r[0] & 7, wheel });
                }
                _ => {}
            }
        }
        crate::input::count_keys(&crate::input::KEYS_USB, &out);
        let d = &mut self.devices[i];
        d.buttons = r.first().copied().unwrap_or(0);
        d.request(&self.hc);
        crate::hid::push(out);
    }
}


impl Hid {
    /// Queues the read of the next report.
    /// A report in the device's own format (by descriptor): keys, media.
    fn own_report(&mut self, packet: &[u8], out: &mut Vec<Event>) {
        let Some(layout) = &self.layout else { return };
        let Some((report, data)) = layout.find(packet) else { return };
        let id = report.id;
        let d = report.decode(data);
        if let Some((mods, keys)) = &d.keyboard {
            let kb = match self.keyboards.iter().position(|k| k.0 == id) {
                Some(i) => &mut self.keyboards[i].1,
                None => {
                    self.keyboards.push((id, crate::hid::Keyboard::new()));
                    &mut self.keyboards.last_mut().unwrap().1
                }
            };
            kb.keys(*mods, keys, out);
        }
        if let Some(pressed) = &d.consumer {
            self.media.report(pressed, out);
        }
        if d.keyboard.is_none() && d.consumer.is_none() && !self.told.contains(&id) {
            // Only the id and size — not the contents (they may contain key presses).
            self.told.push(id);
            kprintln!("usb: {}: keyboard report {id} ({} bytes) has nothing we use", self.loc, packet.len());
        }
    }

    fn request(&mut self, hc: &Xhci) {
        self.ring.push(self.buf as u64, self.len as u32, xhci::TRB_NORMAL << 10 | 1 << 5 | 1 << 2);
        hc.ring_doorbell(self.slot, self.dci);
    }
}
