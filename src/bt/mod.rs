//! Bluetooth: a host stack independent of both the chip and the transport.
//!
//! Layers:
//! - transport — the [`Hci`] trait: a driver only moves HCI packets (commands, events,
//!   ACL data) between the controller and the stack. Standard-class USB adapters
//!   (`usb/bt.rs`) — any vendor; UART (laptops) will plug in the same way later;
//! - vendor initialization (firmware patch and so on) — by the vendor code from the
//!   controller version, regardless of transport: `realtek.rs`, later Intel, Broadcom;
//! - the stack itself (`host.rs`) — its own thread: commands and events, inquiry, connection and
//!   pairing; L2CAP, SMP, GATT and profiles are built on top.
//!
//! Adding a controller = implement `Hci` and call [`attach`] — the rest is picked up.

mod host;
mod intel;
mod realtek;

use crate::interrupts::{self, HZ};
use crate::sync::IrqLock;
use crate::task;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// HCI packet type (as in the H4 UART protocol).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Command,
    Acl,
    Event,
    /// Voice (SCO): a header (handle, length) and samples.
    Sco,
}

/// HCI transport: the controller driver moves packets, the stack does the rest.
pub trait Hci: Send {
    /// Queue a packet for sending (a command or ACL data). false — not accepted.
    fn send(&mut self, kind: Kind, packet: Vec<u8>) -> bool;
    /// A received packet (event or ACL data), immediately, without waiting.
    fn recv(&mut self) -> Option<(Kind, Vec<u8>)>;
    /// The controller is present (the USB adapter was not unplugged).
    fn alive(&self) -> bool {
        true
    }
    /// Where the controller is — for the log: "USB 13d3:3410".
    fn name(&self) -> String;
    /// Vendor on the bus (USB VID), if known: some (Intel) need firmware
    /// loaded before the controller answers ordinary commands.
    fn vendor(&self) -> Option<u16> {
        None
    }
    /// The voice channel was opened/closed: the transport prepares or removes the path for
    /// voice (USB: the second interface with isochronous endpoints).
    fn voice(&mut self, _on: bool) {}
}

/// Queues between the driver thread and the stack — for transports whose I/O runs in
/// their own thread (USB): the driver takes packets to send from here and puts received ones here.
#[derive(Default)]
pub struct Pipe {
    pub to_controller: VecDeque<(Kind, Vec<u8>)>,
    pub from_controller: VecDeque<(Kind, Vec<u8>)>,
    /// The device was disconnected — the stack stops.
    pub gone: bool,
    /// The stack asks for a voice path (the transport switches in its own thread).
    pub voice: bool,
}

pub type Shared = Arc<IrqLock<Pipe>>;

/// How many received packets to hold until the stack processes them.
const PIPE_MAX: usize = 512;

pub fn pipe() -> Shared {
    Arc::new(IrqLock::new(Pipe::default()))
}

/// A received packet — to the stack (from the driver thread).
pub fn deliver(pipe: &Shared, kind: Kind, packet: Vec<u8>) {
    let mut p = pipe.lock();
    if p.from_controller.len() < PIPE_MAX {
        p.from_controller.push_back((kind, packet));
    }
}

/// `Hci` on top of [`Pipe`] queues.
pub struct PipeHci {
    pipe: Shared,
    name: String,
    vendor: Option<u16>,
}

impl PipeHci {
    pub fn new(pipe: Shared, name: String, vendor: Option<u16>) -> PipeHci {
        PipeHci { pipe, name, vendor }
    }
}

impl Hci for PipeHci {
    fn send(&mut self, kind: Kind, packet: Vec<u8>) -> bool {
        let mut p = self.pipe.lock();
        if p.gone || p.to_controller.len() >= PIPE_MAX {
            return false;
        }
        p.to_controller.push_back((kind, packet));
        true
    }
    fn recv(&mut self) -> Option<(Kind, Vec<u8>)> {
        self.pipe.lock().from_controller.pop_front()
    }
    fn alive(&self) -> bool {
        !self.pipe.lock().gone
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    fn vendor(&self) -> Option<u16> {
        self.vendor
    }
    fn voice(&mut self, on: bool) {
        self.pipe.lock().voice = on;
    }
}

/// A new controller: the stack starts in its own thread. While one controller works,
/// a second one waits until the first disappears.
pub fn attach(hci: Box<dyn Hci>) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    let name = hci.name();
    task::spawn("bt", None, move || {
        while RUNNING.swap(true, Ordering::AcqRel) {
            if !hci.alive() {
                return;
            }
            task::sleep_until(interrupts::ticks() + HZ);
        }
        crate::kprintln!("bt: controller {name}");
        host::run(hci);
        RUNNING.store(false, Ordering::Release);
    });
}

// ---------------------------------------------------------------- for the window and the terminal

/// A discovered device.
#[derive(Clone)]
pub struct Device {
    pub addr: [u8; 6],
    pub name: String,
    pub rssi: i8,
    /// Device class (classic Bluetooth), 0 — unknown.
    pub class: u32,
    pub le: bool,
    /// LE address type: 0 — public, 1 — random (needed to connect).
    pub addr_type: u8,
}

/// What is happening with the connection to a device right now.
#[derive(Clone, PartialEq)]
pub enum Link {
    Connecting,
    /// Pairing is in progress; the text says what to do on the device (may be empty).
    Pairing(String),
    /// Numeric comparison: the device shows this number — confirm it here (confirm).
    Confirm(u32),
    Connected,
    /// It failed or the connection closed: why.
    Failed(String),
}

/// A device for the window: found by inquiry and/or paired, with the connection state.
#[derive(Clone)]
pub struct Entry {
    pub dev: Device,
    pub paired: bool,
    pub link: Option<Link>,
}

/// A paired device: key and name. For classic, `key` is the link key; for LE it is the
/// long-term key (LTK) with its id (EDIV, Rand) and the identity resolving key (IRK),
/// if the device provided one.
#[derive(Clone)]
struct Paired {
    addr: [u8; 6],
    le: bool,
    key: [u8; 16],
    name: String,
    ediv: u16,
    rand: [u8; 8],
    irk: Option<[u8; 16]>,
    /// LE address type: 0 — public, 1 — random (cannot connect without it).
    addr_type: u8,
}

/// The address type for old records without it: a public (IEEE-assigned) address never has both
/// top bits set, a static random one always does.
fn guess_type(addr: &[u8; 6], le: bool) -> u8 {
    (le && addr[5] & 0xC0 == 0xC0) as u8
}

enum Request {
    Connect([u8; 6]),
    Disconnect([u8; 6]),
    Forget([u8; 6]),
    Confirm([u8; 6], bool),
    /// Ask the device's SDP about a service (UUID; 0x1002 — about all of them).
    Sdp([u8; 6], u16),
}

static STATUS: IrqLock<String> = IrqLock::new(String::new());
static FOUND: IrqLock<Vec<Device>> = IrqLock::new(Vec::new());
static SCANS: AtomicU64 = AtomicU64::new(0);
static WANT_SCAN: AtomicBool = AtomicBool::new(false);
static SCANNING: AtomicBool = AtomicBool::new(false);
static PRESENT: AtomicBool = AtomicBool::new(false);
/// Bluetooth is turned on by the user (when off: connections are closed, no inquiry, nothing accepted).
static ENABLED: AtomicBool = AtomicBool::new(true);
/// An inquiry requested without waiting (the tray menu): up to which inquiry number it runs.
static SCAN_UNTIL: AtomicU64 = AtomicU64::new(0);
/// Where we remember that Bluetooth was turned off.
const STATE_FILE: &str = "/system/bluetooth.state";
static PAIRED: IrqLock<Vec<Paired>> = IrqLock::new(Vec::new());
static LINKS: IrqLock<BTreeMap<[u8; 6], Link>> = IrqLock::new(BTreeMap::new());
static REQUESTS: IrqLock<VecDeque<Request>> = IrqLock::new(VecDeque::new());

/// Paired devices and their link keys.
const PAIRED_FILE: &str = "/system/bluetooth.cfg";
const LE_SCAN_TICKS: u64 = 5 * HZ;
const INQUIRY_TICKS: u64 = 7 * HZ;

fn set_status(text: &str) {
    crate::kprintln!("bt: {text}");
    *STATUS.lock() = text.into();
}

pub fn status() -> String {
    if present() && !enabled() {
        return "off".into();
    }
    let s = STATUS.lock().clone();
    if s.is_empty() { "no Bluetooth controller".into() } else { s }
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Discover nearby devices: LE (5 s) and classic Bluetooth (7 s).
pub fn scan() -> Result<Vec<Device>, String> {
    if !present() {
        return Err(status());
    }
    SCANNING.store(true, Ordering::Relaxed);
    let seen = SCANS.load(Ordering::Relaxed);
    WANT_SCAN.store(true, Ordering::Relaxed);
    let end = interrupts::ticks() + LE_SCAN_TICKS + INQUIRY_TICKS + 5 * HZ;
    while SCANS.load(Ordering::Relaxed) == seen && interrupts::ticks() < end {
        task::sleep_until(interrupts::ticks() + HZ / 4);
    }
    SCANNING.store(false, Ordering::Relaxed);
    if SCANS.load(Ordering::Relaxed) == seen {
        return Err(format!("the scan did not finish: {}", status()));
    }
    Ok(FOUND.lock().clone())
}

/// An inquiry is in progress (the window does not start a second one and shows "searching").
pub fn scanning() -> bool {
    SCANNING.load(Ordering::Relaxed) || SCANS.load(Ordering::Relaxed) < SCAN_UNTIL.load(Ordering::Relaxed)
}

/// Start an inquiry and do not wait: the results appear in `devices()`.
pub fn start_scan() {
    if !present() || !enabled() || scanning() {
        return;
    }
    SCAN_UNTIL.store(SCANS.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    WANT_SCAN.store(true, Ordering::Relaxed);
}

/// Bluetooth is on.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turn Bluetooth on or off (remembered until and after reboot).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if !on {
        WANT_SCAN.store(false, Ordering::Relaxed);
        SCAN_UNTIL.store(0, Ordering::Relaxed);
    }
    let _ = task::as_system(|| crate::fs::write(STATE_FILE, if on { b"on\n" as &[u8] } else { b"off\n" }));
}

fn show(addr: [u8; 6], link: Option<Link>) {
    match link {
        Some(l) => LINKS.lock().insert(addr, l),
        None => LINKS.lock().remove(&addr),
    };
}

/// Connect (and pair the first time).
pub fn connect(addr: [u8; 6]) {
    show(addr, Some(Link::Connecting));
    REQUESTS.lock().push_back(Request::Connect(addr));
}

pub fn disconnect(addr: [u8; 6]) {
    REQUESTS.lock().push_back(Request::Disconnect(addr));
}

/// Ask the device about its services (SDP): the answer goes to the log. `uuid` 0x1002 — all.
pub fn sdp(addr: [u8; 6], uuid: u16) {
    REQUESTS.lock().push_back(Request::Sdp(addr, uuid));
}

/// Forget the pairing (and disconnect).
pub fn forget(addr: [u8; 6]) {
    REQUESTS.lock().push_back(Request::Forget(addr));
}

/// Answer to the numeric comparison: true — the number matched.
pub fn confirm(addr: [u8; 6], yes: bool) {
    REQUESTS.lock().push_back(Request::Confirm(addr, yes));
}

/// Devices for the window: paired ones, those found by the last inquiry and those
/// connected now. Paired first, then by signal strength.
pub fn devices() -> Vec<Entry> {
    let found = FOUND.lock().clone();
    let paired = PAIRED.lock().clone();
    let links = LINKS.lock().clone();
    let mut out: Vec<Entry> = Vec::new();
    let link_of = |a: &[u8; 6]| links.get(a).cloned();
    for p in &paired {
        let dev = found.iter().find(|d| d.addr == p.addr).cloned().unwrap_or(Device { addr: p.addr, name: p.name.clone(), rssi: 0, class: 0, le: p.le, addr_type: 0 });
        out.push(Entry { dev, paired: true, link: link_of(&p.addr) });
    }
    for d in &found {
        if !out.iter().any(|e| e.dev.addr == d.addr) {
            out.push(Entry { dev: d.clone(), paired: false, link: link_of(&d.addr) });
        }
    }
    for (a, l) in &links {
        if !out.iter().any(|e| e.dev.addr == *a) {
            out.push(Entry { dev: Device { addr: *a, name: String::new(), rssi: 0, class: 0, le: false, addr_type: 0 }, paired: false, link: Some(l.clone()) });
        }
    }
    out
}

/// An address as people write it (most significant byte first).
pub fn addr_text(addr: &[u8; 6]) -> String {
    let parts: Vec<String> = addr.iter().rev().map(|b| format!("{b:02x}")).collect();
    parts.join(":")
}

/// "aa:bb:cc:dd:ee:ff" → an address in HCI order (least significant byte first).
pub fn parse_addr(s: &str) -> Option<[u8; 6]> {
    let mut addr = [0u8; 6];
    let mut parts = s.trim().split(':');
    for i in (0..6).rev() {
        addr[i] = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(addr)
}

/// The connection state line for people.
pub fn link_text(l: &Link) -> String {
    match l {
        Link::Connecting => "connecting\u{2026}".into(),
        Link::Pairing(t) if t.is_empty() => "pairing\u{2026}".into(),
        Link::Pairing(t) => format!("pairing: {t}"),
        Link::Confirm(n) => format!("does the device show {n:06}? confirm to pair"),
        Link::Connected => "connected".into(),
        Link::Failed(why) => why.clone(),
    }
}

/// A device list line.
pub fn describe(d: &Device) -> String {
    let kind = if d.le { "LE" } else { "BR/EDR" };
    let name = if d.name.is_empty() { "(no name)" } else { &d.name };
    format!("{:>4} dBm  {kind:6}  {}  {name}", d.rssi, addr_text(&d.addr))
}

/// The reason for an HCI error code.
fn reason(code: u8) -> String {
    match code {
        0x02 => "the device did not answer".into(),
        0x04 => "the device did not answer (is it on and nearby?)".into(),
        0x05 => "pairing failed: codes did not match or it was declined".into(),
        0x06 => "the device no longer knows this computer: forget it and pair again".into(),
        0x08 => "the connection timed out".into(),
        0x0B => "already connected".into(),
        0x13 => "the device disconnected".into(),
        0x16 => "disconnected".into(),
        0x18 => "the device does not allow pairing now".into(),
        0x22 | 0x28 => "the device stopped answering".into(),
        0x3B | 0x3D => "the connection broke".into(),
        0x3E => "could not connect".into(),
        other => format!("error {other:#04x}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Bytes from a hex string (exactly N).
fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Tab-separated file lines: address, LE (0/1), key, name; for LE also EDIV, Rand and IRK
/// ("-" — none).
fn parse_paired(text: &str) -> Vec<Paired> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let addr = parse_addr(f.next()?)?;
            let le = f.next()? == "1";
            let key = unhex::<16>(f.next()?)?;
            let name = f.next().unwrap_or("").into();
            let ediv = f.next().and_then(|s| u16::from_str_radix(s, 16).ok()).unwrap_or(0);
            let rand = f.next().and_then(unhex::<8>).unwrap_or([0; 8]);
            let irk = f.next().and_then(unhex::<16>);
            let addr_type = f.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| guess_type(&addr, le));
            Some(Paired { addr, le, key, name, ediv, rand, irk, addr_type })
        })
        .collect()
}

/// The paired list has been read from disk: do not save before that — otherwise an empty list
/// would overwrite the file (the stack starts before the system disk is mounted).
static PAIRED_LOADED: AtomicBool = AtomicBool::new(false);

/// Saves paired devices (from the stack thread — it does not hold USB).
fn save_paired() {
    if !PAIRED_LOADED.load(Ordering::Relaxed) {
        return;
    }
    let text: String = PAIRED
        .lock()
        .iter()
        .map(|p| {
            let irk = p.irk.map_or(String::from("-"), |k| hex(&k));
            format!(
                "{}\t{}\t{}\t{}\t{:04x}\t{}\t{irk}\t{}\n",
                addr_text(&p.addr),
                p.le as u8,
                hex(&p.key),
                p.name.replace(['\t', '\n'], " "),
                p.ediv,
                hex(&p.rand),
                p.addr_type
            )
        })
        .collect();
    let _ = task::as_system(|| {
        let _ = crate::fs::mkdir("/system");
        crate::fs::write(PAIRED_FILE, text.as_bytes())
    });
}

/// Reads paired devices once the system disk is mounted. false — no disk
/// yet, try later.
fn load_paired() -> bool {
    if !crate::fs::ready() {
        return false;
    }
    if let Ok(data) = task::as_system(|| crate::fs::read(PAIRED_FILE)) {
        // Do not lose devices paired while waiting for the disk.
        let mut list = parse_paired(&String::from_utf8_lossy(&data));
        let mut paired = PAIRED.lock();
        for p in paired.drain(..) {
            list.retain(|d| d.addr != p.addr);
            list.push(p);
        }
        *paired = list;
    }
    if let Ok(state) = task::as_system(|| crate::fs::read(STATE_FILE)) {
        ENABLED.store(!state.starts_with(b"off"), Ordering::Relaxed);
    }
    PAIRED_LOADED.store(true, Ordering::Relaxed);
    true
}

/// Device name: from the inquiry or from the paired list.
fn name_of(addr: &[u8; 6]) -> String {
    let found = FOUND.lock().iter().find(|d| d.addr == *addr).map(|d| d.name.clone()).filter(|n| !n.is_empty());
    found.or_else(|| PAIRED.lock().iter().find(|p| p.addr == *addr).map(|p| p.name.clone())).unwrap_or_default()
}

/// Name from EIR/LE advertising data: complete (0x09) or shortened (0x08).
fn eir_name(data: &[u8]) -> String {
    let (mut at, mut short) = (0, String::new());
    while at + 1 < data.len() {
        let len = data[at] as usize;
        if len == 0 || at + 1 + len > data.len() {
            break;
        }
        let (kind, value) = (data[at + 1], &data[at + 2..at + 1 + len]);
        match kind {
            0x09 => return String::from_utf8_lossy(value).into(),
            0x08 => short = String::from_utf8_lossy(value).into(),
            _ => {}
        }
        at += 1 + len;
    }
    short
}
