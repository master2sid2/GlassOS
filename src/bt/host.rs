//! The host stack: HCI commands and events, inquiry, connection and pairing. It runs in its own
//! thread and only knows the controller through the `Hci` trait — it works with any chip and transport.

use super::{
    Device, Hci, Kind, LINKS, Link, PAIRED, PRESENT, Paired, REQUESTS, Request, SCANS, WANT_SCAN, FOUND, INQUIRY_TICKS, LE_SCAN_TICKS, addr_text, eir_name, load_paired, name_of, realtek,
    reason, save_paired, set_status, show,
};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::task;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

mod att;
mod audio;
mod avrcp;
mod battery;
mod chan;
mod hfp;
mod hogp;
mod l2cap;
mod rfcomm;
mod sco;
mod sdp;
mod smp;

// HCI commands (OGF << 10 | OCF).
const RESET: u16 = 0x0C03;
const SET_EVENT_MASK: u16 = 0x0C01;
const WRITE_INQUIRY_MODE: u16 = 0x0C45;
const READ_LOCAL_VERSION: u16 = 0x1001;
const READ_BD_ADDR: u16 = 0x1009;
const INQUIRY: u16 = 0x0401;
const INQUIRY_CANCEL: u16 = 0x0402;
const LE_SET_EVENT_MASK: u16 = 0x2001;
const LE_SET_SCAN_PARAMETERS: u16 = 0x200B;
const LE_SET_SCAN_ENABLE: u16 = 0x200C;
const CREATE_CONNECTION: u16 = 0x0405;
const DISCONNECT: u16 = 0x0406;
const CREATE_CONNECTION_CANCEL: u16 = 0x0408;
const ACCEPT_CONNECTION: u16 = 0x0409;
const REJECT_CONNECTION: u16 = 0x040A;
const LINK_KEY_REPLY: u16 = 0x040B;
const LINK_KEY_NEGATIVE: u16 = 0x040C;
const PIN_CODE_REPLY: u16 = 0x040D;
const AUTH_REQUESTED: u16 = 0x0411;
const SET_ENCRYPTION: u16 = 0x0413;
const REMOTE_NAME_REQUEST: u16 = 0x0419;
const IO_CAP_REPLY: u16 = 0x042B;
const USER_CONFIRM_REPLY: u16 = 0x042C;
const USER_CONFIRM_NEGATIVE: u16 = 0x042D;
const USER_PASSKEY_NEGATIVE: u16 = 0x042F;
const WRITE_LOCAL_NAME: u16 = 0x0C13;
const WRITE_SCAN_ENABLE: u16 = 0x0C1A;
const WRITE_CLASS_OF_DEVICE: u16 = 0x0C24;
const WRITE_SSP_MODE: u16 = 0x0C56;
const LE_CREATE_CONNECTION: u16 = 0x200D;
const LE_CREATE_CONNECTION_CANCEL: u16 = 0x200E;
const LE_CLEAR_ACCEPT_LIST: u16 = 0x2010;
const LE_ADD_TO_ACCEPT_LIST: u16 = 0x2011;

// Events.
const EV_INQUIRY_COMPLETE: u8 = 0x01;
const EV_INQUIRY_RESULT: u8 = 0x02;
const EV_CONNECTION_COMPLETE: u8 = 0x03;
const EV_CONNECTION_REQUEST: u8 = 0x04;
const EV_DISCONNECTION_COMPLETE: u8 = 0x05;
const EV_AUTH_COMPLETE: u8 = 0x06;
const EV_REMOTE_NAME: u8 = 0x07;
const EV_ENCRYPTION_CHANGE: u8 = 0x08;
const EV_COMMAND_COMPLETE: u8 = 0x0E;
const EV_COMMAND_STATUS: u8 = 0x0F;
const EV_PIN_CODE_REQUEST: u8 = 0x16;
const EV_LINK_KEY_REQUEST: u8 = 0x17;
const EV_LINK_KEY_NOTIFICATION: u8 = 0x18;
const EV_INQUIRY_RESULT_RSSI: u8 = 0x22;
const EV_EXTENDED_INQUIRY: u8 = 0x2F;
const EV_IO_CAP_REQUEST: u8 = 0x31;
const EV_IO_CAP_RESPONSE: u8 = 0x32;
const EV_USER_CONFIRM_REQUEST: u8 = 0x33;
const EV_USER_PASSKEY_REQUEST: u8 = 0x34;
const EV_SIMPLE_PAIRING_COMPLETE: u8 = 0x36;
const EV_USER_PASSKEY_NOTIFICATION: u8 = 0x3B;
const EV_LE_META: u8 = 0x3E;
const EV_COMPLETED_PACKETS: u8 = 0x13;
const EV_SYNC_CONNECTION_COMPLETE: u8 = 0x2C;
const READ_BUFFER_SIZE: u16 = 0x1005;
const LE_READ_BUFFER_SIZE: u16 = 0x2002;
const LE_CONNECTION_COMPLETE: u8 = 0x01;
const LE_ADVERTISING_REPORT: u8 = 0x02;

/// The general inquiry access code (GIAC) and the duration in units of 1.28 s.
const GIAC: [u8; 3] = [0x33, 0x8B, 0x9E];
const INQUIRY_UNITS: u8 = 4;

/// IO Capability: a display and "yes/no"; if the peer has
/// "no input, no output" — pairing without a number (Just Works).
const IO_DISPLAY_YES_NO: u8 = 0x01;
const IO_NONE: u8 = 0x03;
/// Requirements: protection against man-in-the-middle (MITM) and bonding (the key is stored).
const AUTH_MITM_BONDING: u8 = 0x03;
/// How long to wait for a connection (a classic device page takes ~5 s, LE — until we
/// hear its advertising).
const CONNECT_TICKS: u64 = 15 * HZ;
/// How long to wait for a command's answer.
const COMMAND_TICKS: u64 = 2 * HZ;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ready,
    LeScan(u64),
    Inquiry(u64),
}

struct Conn {
    handle: Option<u16>,
    le: bool,
    /// Until which tick to wait for the connection (then cancel).
    until: u64,
    /// The peer's IO capabilities (from the IO Capability Response).
    remote_io: u8,
    /// Disconnecting at the person's request — not an error.
    quiet: bool,
    /// ACL packets the controller has not yet reported as "sent" (their credits must be
    /// returned if the link drops).
    in_flight: u16,
    /// ATT: the MTU, the request in flight (by protocol — one) and the queue of the next ones.
    att_mtu: u16,
    att_busy: Option<att::Want>,
    att_queue: VecDeque<(att::Want, Vec<u8>)>,
    /// The LE device address type (0 — public, 1 — random) — needed for pairing.
    peer_type: u8,
    /// LE pairing in progress.
    smp: Option<smp::Smp>,
    /// GATT services: start, end, 16-bit UUID (0 — a custom 128-bit one).
    services: Vec<(u16, u16, u16)>,
    /// Service discovery is finished.
    services_ready: bool,
    /// The link is encrypted (HID does not answer without it).
    encrypted: bool,
    /// A keyboard or a mouse (HID over GATT).
    hid: Option<hogp::Hogp>,
    /// Battery: whether it was asked, and the value handle (for notifications).
    battery_asked: bool,
    battery: Option<u16>,
    battery_last: Option<u8>,
    /// Classic Bluetooth L2CAP channels, the next channel id of ours and the next signal id.
    chans: Vec<chan::Chan>,
    next_cid: u16,
    ident: u8,
    /// Our questions to the device's SDP and those waiting for link encryption.
    sdp: Option<sdp::SdpClient>,
    sdp_waiting: Vec<u16>,
    /// Music to headphones (A2DP).
    a2dp: Option<audio::A2dp>,
    /// RFCOMM ports and the headset on top of them.
    rfcomm: Option<rfcomm::Rfcomm>,
    hfp: Option<hfp::Hfp>,
    /// The headset voice channel (microphone).
    sco: Option<sco::Sco>,
    /// The AVRCP remote: headphone buttons and absolute volume.
    avrcp: Option<avrcp::Avrcp>,
}

impl Conn {
    fn new(le: bool, until: u64) -> Conn {
        Conn {
            handle: None,
            le,
            until,
            remote_io: IO_DISPLAY_YES_NO,
            quiet: false,
            in_flight: 0,
            att_mtu: 23,
            att_busy: None,
            att_queue: VecDeque::new(),
            peer_type: 0,
            smp: None,
            services: Vec::new(),
            services_ready: false,
            encrypted: false,
            hid: None,
            battery_asked: false,
            battery: None,
            battery_last: None,
            chans: Vec::new(),
            next_cid: 0,
            ident: 0,
            sdp: None,
            sdp_waiting: Vec::new(),
            a2dp: None,
            rfcomm: None,
            hfp: None,
            sco: None,
            avrcp: None,
        }
    }
}

pub(super) struct Host {
    hci: Box<dyn Hci>,
    state: State,
    found: BTreeMap<[u8; 6], Device>,
    /// Connections: address → handle (once established) and details.
    conns: BTreeMap<[u8; 6], Conn>,
    /// Answers to the controller from event handling: sent from the main loop so that
    /// commands do not nest inside each other.
    outbox: VecDeque<(u16, Vec<u8>)>,
    /// ACL data: packet size and free controller buffers — shared ones and (if it
    /// has separate ones) for LE.
    acl_mtu: usize,
    acl_free: u16,
    le_mtu: usize,
    le_free: u16,
    le_own: bool,
    /// ACL packets to send (handle, LE, packet with header) — they go out while there are
    /// free buffers.
    acl_out: VecDeque<(u16, bool, Vec<u8>)>,
    /// Starts of L2CAP packets that arrived in pieces: handle → what has been assembled.
    acl_in: BTreeMap<u16, Vec<u8>>,
    /// Our own address (HCI order) — needed for LE pairing.
    own_addr: [u8; 6],
    /// Background connection: whom we wait for (paired LE devices not connected) and when we may try
    /// again if the controller refused.
    auto: Option<Vec<[u8; 6]>>,
    auto_retry: u64,
    /// Whether it is on now (follows `bt::enabled()`).
    on: bool,
    /// Whom we already retried connecting to after a drop on the air (retried once).
    retried: Vec<[u8; 6]>,
}

/// The stack thread: start the controller, then the "packets → handling → requests" loop.
pub(super) fn run(hci: Box<dyn Hci>) {
    let mut h = Host {
        hci,
        state: State::Ready,
        found: BTreeMap::new(),
        conns: BTreeMap::new(),
        outbox: VecDeque::new(),
        acl_mtu: 27,
        acl_free: 1,
        le_mtu: 27,
        le_free: 0,
        le_own: false,
        acl_out: VecDeque::new(),
        acl_in: BTreeMap::new(),
        own_addr: [0; 6],
        auto: None,
        auto_retry: 0,
        on: true,
        retried: Vec::new(),
    };
    set_status("starting the controller");
    let mut paired_loaded = load_paired();
    match h.init() {
        Ok(text) => {
            set_status(&format!("ready, {text}"));
            PRESENT.store(true, Ordering::Relaxed);
        }
        Err(e) => {
            set_status(&format!("{}: {e}", h.hci.name()));
            return;
        }
    }
    let mut bt_last = 0;
    loop {
        if !h.hci.alive() {
            PRESENT.store(false, Ordering::Relaxed);
            LINKS.lock().clear();
            set_status("the Bluetooth adapter was removed");
            return;
        }
        // Which loop step delays the thread (sound to headphones goes from here).
        let mut step_at = interrupts::ticks();
        let mut step = |name: &str| {
            let now = interrupts::ticks();
            if now - step_at > HZ / 5 {
                kprintln!("bt: the {name} step took {} ms", (now - step_at) * 1000 / HZ);
            }
            step_at = now;
        };
        let worked = h.pump();
        step("pump");
        if !paired_loaded {
            paired_loaded = load_paired();
            if paired_loaded {
                save_paired();
            }
        }
        step("paired");
        h.service();
        step("service");
        h.hid_tick(interrupts::ticks());
        step("hid");
        crate::audio::diag::gap(&crate::audio::diag::GAP_BT, &mut bt_last);
        h.audio_tick(interrupts::ticks());
        step("audio");
        h.avrcp_tick(interrupts::ticks());
        step("avrcp");
        h.sco_tick(interrupts::ticks());
        step("sco");
        if !worked {
            task::sleep_until(interrupts::ticks() + 1);
        }
    }
}

/// The answer to command `op` in an event: Command Complete parameters or the Command Status.
fn completion(op: u16, ev: &[u8]) -> Option<Vec<u8>> {
    match ev.first()? {
        &EV_COMMAND_COMPLETE if ev.len() >= 5 && u16::from_le_bytes([ev[3], ev[4]]) == op => Some(ev[5..].to_vec()),
        &EV_COMMAND_STATUS if ev.len() >= 6 && u16::from_le_bytes([ev[4], ev[5]]) == op => Some(ev[2..3].to_vec()),
        _ => None,
    }
}

/// An address from 6 event bytes (HCI order).
fn addr6(b: &[u8]) -> [u8; 6] {
    let mut a = [0u8; 6];
    a.copy_from_slice(&b[..6]);
    a
}

impl Host {
    /// Handles everything received. true — there was something.
    fn pump(&mut self) -> bool {
        let mut worked = false;
        while let Some((kind, packet)) = self.hci.recv() {
            worked = true;
            match kind {
                Kind::Event => self.event(&packet),
                Kind::Acl => self.acl(&packet),
                Kind::Sco => self.sco_data(&packet),
                Kind::Command => {}
            }
        }
        worked
    }

    /// An HCI command and its completion (Command Complete: the answer parameters; Command Status:
    /// the status). Other packets on the way are handled as usual.
    pub(super) fn command(&mut self, op: u16, params: &[u8]) -> Result<Vec<u8>, String> {
        let mut p = Vec::with_capacity(3 + params.len());
        p.extend_from_slice(&op.to_le_bytes());
        p.push(params.len() as u8);
        p.extend_from_slice(params);
        if !self.hci.send(Kind::Command, p) {
            return Err(format!("command {op:04x} was not accepted"));
        }
        let start = interrupts::ticks();
        loop {
            while let Some((kind, packet)) = self.hci.recv() {
                match kind {
                    Kind::Event => match completion(op, &packet) {
                        Some(r) => return Ok(r),
                        None => self.event(&packet),
                    },
                    Kind::Acl => self.acl(&packet),
                    Kind::Sco => self.sco_data(&packet),
                    Kind::Command => {}
                }
            }
            let now = interrupts::ticks();
            if !self.hci.alive() {
                return Err("the controller is gone".into());
            }
            if now >= start + COMMAND_TICKS {
                return Err(format!("no answer to command {op:04x}"));
            }
            // Usually the answer takes milliseconds: give up the CPU for the first tick, then sleep.
            if now <= start + 1 {
                task::yield_now();
            } else {
                task::sleep_until(now + 1);
            }
        }
    }

    /// ACL packets waiting for a free controller buffer.
    pub(super) fn acl_backlog(&self) -> usize {
        self.acl_out.len()
    }

    /// A packet to the controller as is (vendor setup: their own download protocols).
    pub(super) fn raw_send(&mut self, kind: Kind, packet: Vec<u8>) -> bool {
        self.hci.send(kind, packet)
    }

    /// A packet from the controller as is, not handled by the stack.
    pub(super) fn raw_recv(&mut self) -> Option<(Kind, Vec<u8>)> {
        self.hci.recv()
    }

    /// A command whose first answer byte is the status: 0 — success.
    pub(super) fn ok(&mut self, op: u16, params: &[u8]) -> Result<Vec<u8>, String> {
        let r = self.command(op, params)?;
        match r.first() {
            Some(0) => Ok(r),
            Some(code) => Err(format!("command {op:04x} failed, status {code:#04x}")),
            None => Err(format!("command {op:04x}: empty answer")),
        }
    }

    /// HCI version, HCI revision, manufacturer, LMP subversion.
    pub(super) fn version(&mut self) -> Result<(u8, u16, u16, u16), String> {
        let r = self.ok(READ_LOCAL_VERSION, &[])?;
        if r.len() < 9 {
            return Err("short local version".into());
        }
        // Status, HCI version, HCI revision, LMP version, manufacturer, LMP subversion.
        Ok((r[1], u16::from_le_bytes([r[2], r[3]]), u16::from_le_bytes([r[5], r[6]]), u16::from_le_bytes([r[7], r[8]])))
    }

    /// Startup: vendor setup (patch and so on), then the common configuration.
    fn init(&mut self) -> Result<String, String> {
        // Intel does not answer even the HCI version before firmware — firmware first.
        let early = match self.hci.vendor() {
            Some(super::intel::USB_VENDOR) => Some(super::intel::setup(self)?),
            _ => None,
        };
        let (hci, rev, maker, lmp) = self.version()?;
        let vendor = match (early, maker) {
            (Some(text), _) => text,
            (None, realtek::MAKER) => realtek::setup(self, rev, lmp)?,
            (None, _) => format!("manufacturer {maker:#06x}, HCI {hci}"),
        };
        let addr = self.start()?;
        Ok(format!("address {addr}; {vendor}"))
    }

    /// The common configuration of any controller: reset, address, events, scanning, pairing.
    fn start(&mut self) -> Result<String, String> {
        self.ok(RESET, &[])?;
        // Data buffers: status, ACL packet size, SCO, ACL count, SCO count. LE may
        // have its own (size 0 — shared with classic).
        let b = self.ok(READ_BUFFER_SIZE, &[])?;
        if b.len() >= 6 {
            self.acl_mtu = u16::from_le_bytes([b[1], b[2]]).max(27) as usize;
            self.acl_free = u16::from_le_bytes([b[4], b[5]]).max(1);
        }
        self.le_mtu = self.acl_mtu;
        if let Ok(b) = self.ok(LE_READ_BUFFER_SIZE, &[])
            && b.len() >= 4
            && u16::from_le_bytes([b[1], b[2]]) != 0
        {
            (self.le_mtu, self.le_free, self.le_own) = (u16::from_le_bytes([b[1], b[2]]).max(27) as usize, (b[3] as u16).max(1), true);
        }
        let addr = self.ok(READ_BD_ADDR, &[])?;
        if let Some(own) = addr.get(1..7) {
            self.own_addr.copy_from_slice(own);
        }
        let mac: Vec<String> = addr.get(1..7).unwrap_or(&[]).iter().rev().map(|b| format!("{b:02x}")).collect();
        // All Bluetooth 4.0 events and LE meta events (bit 61).
        self.ok(SET_EVENT_MASK, &[0xFF, 0xFF, 0xFB, 0xFF, 0x07, 0xF8, 0xBF, 0x3D])?;
        self.ok(LE_SET_EVENT_MASK, &[0x1F, 0, 0, 0, 0, 0, 0, 0])?;
        // Inquiry results with signal strength and extended response (name).
        self.ok(WRITE_INQUIRY_MODE, &[2])?;
        // Pairing with Secure Simple Pairing (not the old PIN code).
        self.ok(WRITE_SSP_MODE, &[1])?;
        // Name and the class "computer, desktop": this is how a phone sees us when pairing.
        let name = crate::net::identity::cached().map_or(String::from("GlassOS"), |id| id.name);
        let mut local = [0u8; 248];
        let n = name.len().min(247);
        local[..n].copy_from_slice(&name.as_bytes()[..n]);
        self.ok(WRITE_LOCAL_NAME, &local)?;
        self.ok(WRITE_CLASS_OF_DEVICE, &[0x04, 0x01, 0x00])?;
        // Answer pages (page scan) so that paired devices can connect
        // by themselves; not visible to inquiry (inquiry scan).
        self.ok(WRITE_SCAN_ENABLE, &[0x02])?;
        Ok(mac.join(":"))
    }

    /// A loop step: inquiry on request and on schedule, answers to the controller, requests from the window,
    /// connections that take too long.
    fn service(&mut self) {
        let now = interrupts::ticks();
        if super::enabled() != self.on {
            self.switch(super::enabled());
        }
        if !self.on {
            // Off: no inquiry is started, connection requests are refused.
            WANT_SCAN.store(false, Ordering::Relaxed);
            while let Some(r) = REQUESTS.lock().pop_front() {
                match r {
                    Request::Connect(addr) => show(addr, Some(Link::Failed("Bluetooth is off".into()))),
                    other => self.request(other, now),
                }
            }
            self.flush_outbox();
            self.flush_acl();
            return;
        }
        // Inquiry and connection at the person's request — first remove the background wait
        // (the controller connects to one list at a time and does not scan meanwhile).
        if self.auto.is_some() && (WANT_SCAN.load(Ordering::Relaxed) || !REQUESTS.lock().is_empty()) {
            self.auto_cancel();
        }
        match self.state {
            State::Ready if WANT_SCAN.swap(false, Ordering::Relaxed) => {
                self.found.clear();
                let started = self.ok(LE_SET_SCAN_PARAMETERS, &[0x01, 0x30, 0x00, 0x30, 0x00, 0x00, 0x00]).and_then(|_| self.ok(LE_SET_SCAN_ENABLE, &[0x01, 0x00]));
                match started {
                    Ok(_) => self.state = State::LeScan(now + LE_SCAN_TICKS),
                    Err(e) => {
                        kprintln!("bt: LE scan: {e}");
                        self.start_inquiry(now);
                    }
                }
            }
            State::LeScan(until) if now >= until => {
                if let Err(e) = self.ok(LE_SET_SCAN_ENABLE, &[0x00, 0x00]) {
                    kprintln!("bt: LE scan stop: {e}");
                }
                self.start_inquiry(now);
            }
            State::Inquiry(until) if now >= until => {
                let _ = self.command(INQUIRY_CANCEL, &[]);
                self.finish_scan();
            }
            _ => {}
        }
        self.flush_outbox();
        self.flush_acl();
        // Requests — when no inquiry is running (during an inquiry the controller would refuse to connect).
        if self.state == State::Ready {
            let request = REQUESTS.lock().pop_front();
            if let Some(r) = request {
                self.request(r, now);
                self.flush_outbox();
            }
        }
        let late: Vec<[u8; 6]> = self.conns.iter().filter(|(_, c)| c.handle.is_none() && now >= c.until).map(|(a, _)| *a).collect();
        for addr in late {
            self.drop_link(addr, false);
        }
        if self.state == State::Ready && REQUESTS.lock().is_empty() && !WANT_SCAN.load(Ordering::Relaxed) {
            self.auto_connect(now);
        }
    }

    /// Background connection, as in any OS: paired LE devices (keyboards, mice)
    /// that are not connected are awaited by the controller itself, which connects as soon as the device wakes up
    /// (a key was pressed — it calls its host). The wait list is the Filter Accept List.
    fn auto_connect(&mut self, now: u64) {
        let wanted: Vec<([u8; 6], u8)> = PAIRED.lock().iter().filter(|p| p.le && !self.conns.contains_key(&p.addr)).map(|p| (p.addr, p.addr_type)).collect();
        let addrs: Vec<[u8; 6]> = wanted.iter().map(|w| w.0).collect();
        if let Some(list) = &self.auto {
            if *list != addrs {
                self.auto_cancel();
            }
            return;
        }
        // While another connection is in progress, the controller will not start a second one.
        if wanted.is_empty() || now < self.auto_retry || self.conns.values().any(|c| c.handle.is_none()) {
            return;
        }
        let started = self.ok(LE_CLEAR_ACCEPT_LIST, &[]).and_then(|_| {
            for (addr, kind) in &wanted {
                let mut p = alloc::vec![*kind];
                p.extend_from_slice(addr);
                self.ok(LE_ADD_TO_ACCEPT_LIST, &p)?;
            }
            let mut p = Vec::with_capacity(25);
            p.extend_from_slice(&0x0200u16.to_le_bytes()); // listen every 320 ms
            p.extend_from_slice(&0x0030u16.to_le_bytes()); // for 30 ms: the air stays almost free for Wi-Fi
            p.push(1); // only from the list
            p.extend_from_slice(&[0; 7]); // address — from the list
            p.push(0); // our address — public
            p.extend_from_slice(&0x0006u16.to_le_bytes()); // connection interval 7.5–15 ms: input without delay
            p.extend_from_slice(&0x000Cu16.to_le_bytes());
            p.extend_from_slice(&0x001Eu16.to_le_bytes()); // up to 30 skipped events while silent
            p.extend_from_slice(&0x01F4u16.to_le_bytes()); // disconnect after 5 s of silence
            p.extend_from_slice(&[0, 0, 0, 0]);
            self.ok(LE_CREATE_CONNECTION, &p)
        });
        match started {
            Ok(_) => {
                let names: Vec<String> = addrs.iter().map(addr_text).collect();
                kprintln!("bt: waiting in the background for {}", names.join(", "));
                self.auto = Some(addrs);
            }
            Err(e) => {
                kprintln!("bt: background connect: {e}");
                self.auto_retry = now + 30 * HZ;
            }
        }
    }

    /// On and off: when off it closes all links, stops waiting for devices
    /// and being visible; when on it accepts connections again and waits for paired ones.
    fn switch(&mut self, on: bool) {
        self.on = on;
        if on {
            if let Err(e) = self.ok(WRITE_SCAN_ENABLE, &[0x02]) {
                kprintln!("bt: turning on: {e}");
            }
            kprintln!("bt: turned on");
            return;
        }
        self.auto_cancel();
        if matches!(self.state, State::LeScan(_)) {
            let _ = self.ok(LE_SET_SCAN_ENABLE, &[0x00, 0x00]);
        }
        if matches!(self.state, State::Inquiry(_)) {
            let _ = self.command(INQUIRY_CANCEL, &[]);
        }
        if self.state != State::Ready {
            self.finish_scan();
        }
        let addrs: Vec<[u8; 6]> = self.conns.keys().copied().collect();
        for addr in addrs {
            self.drop_link(addr, true);
        }
        if let Err(e) = self.ok(WRITE_SCAN_ENABLE, &[0x00]) {
            kprintln!("bt: turning off: {e}");
        }
        kprintln!("bt: turned off");
    }

    fn auto_cancel(&mut self) {
        if self.auto.take().is_some() {
            let _ = self.command(LE_CREATE_CONNECTION_CANCEL, &[]);
        }
    }

    fn start_inquiry(&mut self, now: u64) {
        let mut params = [0u8; 5];
        params[..3].copy_from_slice(&GIAC);
        params[3] = INQUIRY_UNITS;
        match self.command(INQUIRY, &params) {
            Ok(r) if r.first() == Some(&0) => self.state = State::Inquiry(now + INQUIRY_TICKS),
            Ok(r) => {
                kprintln!("bt: inquiry refused, status {:#04x}", r.first().copied().unwrap_or(0xFF));
                self.finish_scan();
            }
            Err(e) => {
                kprintln!("bt: inquiry: {e}");
                self.finish_scan();
            }
        }
    }

    fn finish_scan(&mut self) {
        let mut list: Vec<Device> = self.found.values().cloned().collect();
        list.sort_by(|a, b| b.rssi.cmp(&a.rssi));
        kprintln!("bt: scan: {} devices", list.len());
        for d in &list {
            kprintln!("bt:   {}", super::describe(d));
        }
        *FOUND.lock() = list;
        SCANS.fetch_add(1, Ordering::Relaxed);
        self.state = State::Ready;
    }

    /// Sends the accumulated answers to the controller.
    fn flush_outbox(&mut self) {
        while let Some((op, params)) = self.outbox.pop_front() {
            match self.command(op, &params) {
                Ok(r) if r.first().is_some_and(|&s| s != 0) => kprintln!("bt: command {op:04x}: {}", reason(r[0])),
                Ok(_) => {}
                Err(e) => kprintln!("bt: {e}"),
            }
        }
    }

    fn request(&mut self, r: Request, now: u64) {
        match r {
            Request::Connect(addr) => self.start_connect(addr, now),
            Request::Disconnect(addr) => self.drop_link(addr, true),
            Request::Forget(addr) => {
                PAIRED.lock().retain(|p| p.addr != addr);
                save_paired();
                kprintln!("bt: forgot {}", addr_text(&addr));
                if self.conns.contains_key(&addr) {
                    self.drop_link(addr, true);
                } else {
                    show(addr, None);
                }
            }
            Request::Sdp(addr, uuid) => {
                // Ask over an encrypted link; no link — connect first.
                if !self.conns.contains_key(&addr) {
                    self.start_connect(addr, now);
                }
                let ready = self.conns.get(&addr).and_then(|c| c.handle.filter(|_| c.encrypted && !c.le));
                match ready {
                    Some(h) => self.sdp_query(h, uuid),
                    None => {
                        if let Some(c) = self.conns.get_mut(&addr) {
                            c.sdp_waiting.push(uuid);
                        }
                    }
                }
            }
            Request::Confirm(addr, yes) => {
                self.outbox.push_back((if yes { USER_CONFIRM_REPLY } else { USER_CONFIRM_NEGATIVE }, addr.to_vec()));
                show(addr, Some(if yes { Link::Pairing(String::new()) } else { Link::Failed("pairing declined".into()) }));
            }
        }
    }

    fn start_connect(&mut self, addr: [u8; 6], now: u64) {
        if let Some(c) = self.conns.get(&addr) {
            // Already connecting or connected — do not start again.
            if c.handle.is_some() {
                show(addr, Some(Link::Connected));
            }
            return;
        }
        let found = FOUND.lock().iter().find(|d| d.addr == addr).cloned();
        let paired_le = PAIRED.lock().iter().find(|p| p.addr == addr).map(|p| (p.le, p.addr_type));
        let (le, addr_type) = match (found, paired_le) {
            (Some(d), _) => (d.le, d.addr_type),
            (None, Some(known)) => known,
            (None, None) => (false, 0),
        };
        let result = if le {
            let mut p = Vec::with_capacity(25);
            p.extend_from_slice(&0x0060u16.to_le_bytes()); // listen for advertising every 60 ms
            p.extend_from_slice(&0x0030u16.to_le_bytes()); // for 30 ms
            p.push(0); // no white list
            p.push(addr_type);
            p.extend_from_slice(&addr);
            p.push(0); // our address — public
            p.extend_from_slice(&0x0018u16.to_le_bytes()); // connection interval 30–50 ms
            p.extend_from_slice(&0x0028u16.to_le_bytes());
            p.extend_from_slice(&0u16.to_le_bytes()); // no skipping
            p.extend_from_slice(&0x01F4u16.to_le_bytes()); // disconnect after 5 s of silence
            p.extend_from_slice(&[0, 0, 0, 0]);
            self.command(LE_CREATE_CONNECTION, &p)
        } else {
            let mut p = addr.to_vec();
            // Packet types DM1–DH5, R1 mode, clock offset unknown, role switch allowed.
            p.extend_from_slice(&[0x18, 0xCC, 0x01, 0x00, 0x00, 0x00, 0x01]);
            self.command(CREATE_CONNECTION, &p)
        };
        match result {
            Ok(r) if r.first() == Some(&0) => {
                self.conns.insert(addr, Conn::new(le, now + CONNECT_TICKS));
                show(addr, Some(Link::Connecting));
                kprintln!("bt: connecting to {} ({})", addr_text(&addr), if le { "LE" } else { "BR/EDR" });
            }
            Ok(r) => show(addr, Some(Link::Failed(format!("the controller refused: {}", reason(r.first().copied().unwrap_or(0xFF)))))),
            Err(e) => show(addr, Some(Link::Failed(e))),
        }
    }

    /// Disconnect or cancel a connection. `quiet` — at the person's request: do not
    /// show it as an error afterwards.
    fn drop_link(&mut self, addr: [u8; 6], quiet: bool) {
        let Some(c) = self.conns.get_mut(&addr) else {
            show(addr, None);
            return;
        };
        c.quiet |= quiet;
        match c.handle {
            Some(h) => {
                let mut p = h.to_le_bytes().to_vec();
                p.push(0x13); // "remote user terminated"
                self.outbox.push_back((DISCONNECT, p));
            }
            None => {
                c.until = u64::MAX;
                if c.le {
                    self.outbox.push_back((LE_CREATE_CONNECTION_CANCEL, Vec::new()));
                } else {
                    self.outbox.push_back((CREATE_CONNECTION_CANCEL, addr.to_vec()));
                }
            }
        }
    }

    /// A connection was established (or not).
    /// Retry a connection after a drop; false — already retried.
    fn retry(&mut self, addr: [u8; 6]) -> bool {
        if self.retried.contains(&addr) {
            self.retried.retain(|a| *a != addr);
            return false;
        }
        self.retried.push(addr);
        kprintln!("bt: {} dropped while connecting, trying again", addr_text(&addr));
        show(addr, Some(Link::Connecting));
        REQUESTS.lock().push_back(Request::Connect(addr));
        true
    }

    fn connected(&mut self, addr: [u8; 6], status: u8, handle: u16, le: bool) {
        if status == 0x3E && self.conns.get(&addr).is_some_and(|c| !c.quiet) {
            self.conns.remove(&addr);
            if self.retry(addr) {
                return;
            }
        }
        if status != 0 {
            let quiet = self.conns.remove(&addr).is_some_and(|c| c.quiet);
            show(addr, if quiet { None } else { Some(Link::Failed(reason(status))) });
            kprintln!("bt: connection to {} failed: {}", addr_text(&addr), reason(status));
            return;
        }
        let c = self.conns.entry(addr).or_insert(Conn::new(le, u64::MAX));
        c.handle = Some(handle);
        c.le = le;
        kprintln!("bt: link to {} is up", addr_text(&addr));
        if le {
            // LE without pairing for now (that is the next step): the link is up. ATT right away: the MTU
            // and the device name (also a check of the data channel).
            show(addr, Some(Link::Connected));
            self.att_start(handle);
            self.le_secure(handle, addr);
            return;
        }
        // Classic: authentication — with the stored key or by pairing, then
        // encryption. The name is asked for if the inquiry did not learn it.
        show(addr, Some(Link::Connecting));
        self.outbox.push_back((AUTH_REQUESTED, handle.to_le_bytes().to_vec()));
        if name_of(&addr).is_empty() {
            let mut p = addr.to_vec();
            p.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
            self.outbox.push_back((REMOTE_NAME_REQUEST, p));
        }
    }

    fn conn_mut(&mut self, handle: u16) -> Option<&mut Conn> {
        self.conns.values_mut().find(|c| c.handle == Some(handle))
    }

    fn addr_of(&self, handle: u16) -> Option<[u8; 6]> {
        self.conns.iter().find(|(_, c)| c.handle == Some(handle)).map(|(a, _)| *a)
    }

    /// Close the connection after a failure (the error has already been shown — it stays on screen).
    fn abort(&mut self, addr: [u8; 6], handle: u16, why: String) {
        show(addr, Some(Link::Failed(why)));
        let mut p = handle.to_le_bytes().to_vec();
        p.push(0x05); // "authentication failure"
        self.outbox.push_back((DISCONNECT, p));
    }

    fn set_name(&mut self, addr: [u8; 6], name: &str) {
        if let Some(d) = self.found.get_mut(&addr) {
            d.name = name.into();
        }
        if let Some(d) = FOUND.lock().iter_mut().find(|d| d.addr == addr) {
            d.name = name.into();
        }
        let changed = match PAIRED.lock().iter_mut().find(|p| p.addr == addr) {
            Some(p) if p.name != name => {
                p.name = name.into();
                true
            }
            _ => false,
        };
        if changed {
            save_paired();
        }
    }

    fn add(&mut self, d: Device) {
        self.found
            .entry(d.addr)
            .and_modify(|old| {
                old.rssi = old.rssi.max(d.rssi);
                if old.name.is_empty() {
                    old.name = d.name.clone();
                }
                if old.class == 0 {
                    old.class = d.class;
                }
            })
            .or_insert(d);
    }

    /// A received ACL packet: header (handle, start/continuation), length, data.
    /// Pieces are assembled into an L2CAP packet, a whole one goes on to the channel.
    fn acl(&mut self, packet: &[u8]) {
        if packet.len() < 4 {
            return;
        }
        let head = u16::from_le_bytes([packet[0], packet[1]]);
        let (handle, start) = (head & 0x0FFF, (head >> 12) & 3 != 0b01);
        let len = u16::from_le_bytes([packet[2], packet[3]]) as usize;
        let Some(data) = packet.get(4..4 + len) else { return };
        let buf = self.acl_in.entry(handle).or_default();
        if start {
            buf.clear();
        }
        buf.extend_from_slice(data);
        if buf.len() < 4 {
            return;
        }
        let need = 4 + u16::from_le_bytes([buf[0], buf[1]]) as usize;
        if buf.len() < need {
            return;
        }
        let pdu = core::mem::take(buf);
        let cid = u16::from_le_bytes([pdu[2], pdu[3]]);
        self.l2cap(handle, cid, &pdu[4..need]);
    }

    /// An L2CAP packet over link `handle`: header (length, channel) and slicing by the controller's
    /// buffer size. It goes out while the controller has free buffers.
    pub(super) fn send_l2cap(&mut self, handle: u16, cid: u16, payload: &[u8]) {
        let Some(le) = self.conns.values().find(|c| c.handle == Some(handle)).map(|c| c.le) else { return };
        let mut pdu = Vec::with_capacity(4 + payload.len());
        pdu.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        pdu.extend_from_slice(&cid.to_le_bytes());
        pdu.extend_from_slice(payload);
        let mtu = if le { self.le_mtu } else { self.acl_mtu };
        for (i, chunk) in pdu.chunks(mtu).enumerate() {
            // Start: for LE — "first, non-flushable" (00), for classic — "first" (10);
            // continuation — 01.
            let pb: u16 = if i > 0 {
                0b01
            } else if le {
                0b00
            } else {
                0b10
            };
            let mut packet = Vec::with_capacity(4 + chunk.len());
            packet.extend_from_slice(&(handle | pb << 12).to_le_bytes());
            packet.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            packet.extend_from_slice(chunk);
            self.acl_out.push_back((handle, le, packet));
        }
        self.flush_acl();
    }

    /// Gives packets to the controller while it has free buffers.
    fn flush_acl(&mut self) {
        while let Some((handle, le)) = self.acl_out.front().map(|(h, le, _)| (*h, *le)) {
            let free = if le && self.le_own { &mut self.le_free } else { &mut self.acl_free };
            if *free == 0 {
                break;
            }
            *free -= 1;
            let Some((_, _, packet)) = self.acl_out.pop_front() else { break };
            if let Some(c) = self.conns.values_mut().find(|c| c.handle == Some(handle)) {
                c.in_flight += 1;
            }
            self.hci.send(Kind::Acl, packet);
        }
    }

    /// The controller sent `count` packets of link `handle` — their buffers are free.
    fn sent(&mut self, handle: u16, count: u16) {
        let Some(c) = self.conns.values_mut().find(|c| c.handle == Some(handle)) else { return };
        let n = count.min(c.in_flight);
        c.in_flight -= n;
        if c.le && self.le_own {
            self.le_free += n;
        } else {
            self.acl_free += n;
        }
    }

    fn event(&mut self, ev: &[u8]) {
        if ev.len() < 2 {
            return;
        }
        let p = &ev[2..];
        match ev[0] {
            EV_INQUIRY_RESULT_RSSI | EV_EXTENDED_INQUIRY | EV_INQUIRY_RESULT if !p.is_empty() => {
                // An extended response is one device with EIR data (name). Records of 14 bytes:
                // address, page scan modes, class, clock offset and (except 0x02) signal
                // strength; 0x02 has one more reserved byte before the class.
                let n = if ev[0] == EV_EXTENDED_INQUIRY { 1 } else { p[0] as usize };
                for k in 0..n {
                    let Some(r) = p.get(1 + k * 14..) else { break };
                    if r.len() < 14 {
                        break;
                    }
                    let addr = addr6(r);
                    let c = if ev[0] == EV_INQUIRY_RESULT { 9 } else { 8 };
                    let class = u32::from_le_bytes([r[c], r[c + 1], r[c + 2], 0]);
                    let rssi = if ev[0] == EV_INQUIRY_RESULT { 0 } else { r[13] as i8 };
                    let name = if ev[0] == EV_EXTENDED_INQUIRY { eir_name(r.get(14..).unwrap_or(&[])) } else { String::new() };
                    self.add(Device { addr, name, rssi, class, le: false, addr_type: 0 });
                }
            }
            EV_LE_META if p.first() == Some(&LE_ADVERTISING_REPORT) && p.len() >= 2 => {
                let n = p[1] as usize;
                let mut at = 2;
                for _ in 0..n {
                    // Type, address type, address (6), data length, data, RSSI.
                    let Some(head) = p.get(at..at + 9) else { break };
                    let len = head[8] as usize;
                    let Some(data) = p.get(at + 9..at + 9 + len) else { break };
                    let rssi = p.get(at + 9 + len).copied().unwrap_or(0x7F) as i8;
                    self.add(Device { addr: addr6(&head[2..8]), name: eir_name(data), rssi, class: 0, le: true, addr_type: head[1] });
                    at += 10 + len;
                }
            }
            EV_LE_META if p.first() == Some(&LE_CONNECTION_COMPLETE) && p.len() >= 12 => {
                // Status, handle, role, address type, address…
                let handle = u16::from_le_bytes([p[2], p[3]]) & 0x0FFF;
                let peer = addr6(&p[6..12]);
                if !self.conns.contains_key(&peer) {
                    // Background: the device from the list showed up (or the wait was removed).
                    self.auto = None;
                    if p[1] != 0 {
                        return;
                    }
                    kprintln!("bt: {} woke up and called us", addr_text(&peer));
                }
                if let Some(c) = self.conns.get_mut(&addr6(&p[6..12])) {
                    c.peer_type = p[5];
                }
                self.connected(addr6(&p[6..12]), p[1], handle, true);
                if let Some(c) = self.conn_mut(handle) {
                    c.peer_type = p[5];
                }
            }
            EV_INQUIRY_COMPLETE => {
                if matches!(self.state, State::Inquiry(_)) {
                    self.finish_scan();
                }
            }
            EV_CONNECTION_COMPLETE if p.len() >= 11 => {
                // Status, handle, address, link type (1 — ACL data; audio is not ours), encryption.
                if p[9] == 1 {
                    self.connected(addr6(&p[3..9]), p[0], u16::from_le_bytes([p[1], p[2]]) & 0x0FFF, false);
                }
            }
            EV_CONNECTION_REQUEST if p.len() >= 10 => {
                // A device connects by itself: only paired ones are let in.
                let addr = addr6(&p[0..6]);
                let known = self.on && PAIRED.lock().iter().any(|d| d.addr == addr);
                let mut r = addr.to_vec();
                if p[9] == 1 && known {
                    self.conns.entry(addr).or_insert(Conn::new(false, u64::MAX));
                    r.push(0x01); // remain the peripheral
                    self.outbox.push_back((ACCEPT_CONNECTION, r));
                    show(addr, Some(Link::Connecting));
                } else {
                    r.push(0x0F); // "unacceptable BD_ADDR"
                    self.outbox.push_back((REJECT_CONNECTION, r));
                }
            }
            EV_SYNC_CONNECTION_COMPLETE if p.len() >= 9 => {
                self.sco_complete(p[0], u16::from_le_bytes([p[1], p[2]]) & 0x0FFF, addr6(&p[3..9]));
            }
            EV_DISCONNECTION_COMPLETE if p.len() >= 4 && p[0] == 0 => {
                let handle = u16::from_le_bytes([p[1], p[2]]) & 0x0FFF;
                if self.addr_of(handle).is_none() && self.sco_closed(handle) {
                    return;
                }
                if let Some(addr) = self.addr_of(handle) {
                    let mut gone = self.conns.remove(&addr);
                    if let Some(h) = gone.as_mut().and_then(|c| c.hid.as_mut()) {
                        h.release();
                    }
                    Host::battery_gone(&addr);
                    if let Some(a) = gone.as_mut().and_then(|c| c.a2dp.as_mut()) {
                        Host::audio_gone(a);
                    }
                    if let Some(s) = gone.as_mut().and_then(|c| c.sco.as_mut()) {
                        Host::sco_gone(s);
                        if s.handle.is_some() {
                            self.hci.voice(false);
                        }
                    }
                    // The controller freed the buffers of this link's unsent packets — take them back.
                    if let Some(c) = &gone {
                        if c.le && self.le_own {
                            self.le_free += c.in_flight;
                        } else {
                            self.acl_free += c.in_flight;
                        }
                    }
                    self.acl_in.remove(&handle);
                    self.acl_out.retain(|(h, ..)| *h != handle);
                    // The link dropped on the air before it was encrypted (0x3E — happens on an antenna shared
                    // with Wi-Fi): retry once, show the person nothing.
                    let early = gone.as_ref().is_some_and(|c| !c.quiet && !c.encrypted);
                    if p[3] == 0x3E && early && self.retry(addr) {
                        return;
                    }
                    let quiet = gone.is_some_and(|c| c.quiet) || p[3] == 0x16;
                    // The reason for a failure (for example, of pairing) stays on screen.
                    let failed = matches!(LINKS.lock().get(&addr), Some(Link::Failed(_)));
                    if !failed {
                        show(addr, if quiet { None } else { Some(Link::Failed(reason(p[3]))) });
                    }
                    kprintln!("bt: {} disconnected: {}", addr_text(&addr), reason(p[3]));
                }
            }
            EV_COMPLETED_PACKETS if !p.is_empty() => {
                // How many packets over which links the controller sent: their buffers are free again.
                for i in 0..p[0] as usize {
                    let Some(e) = p.get(1 + 4 * i..5 + 4 * i) else { break };
                    self.sent(u16::from_le_bytes([e[0], e[1]]) & 0x0FFF, u16::from_le_bytes([e[2], e[3]]));
                }
                self.flush_acl();
            }
            EV_AUTH_COMPLETE if p.len() >= 3 => {
                let handle = u16::from_le_bytes([p[1], p[2]]) & 0x0FFF;
                let Some(addr) = self.addr_of(handle) else { return };
                if p[0] == 0 {
                    let mut r = handle.to_le_bytes().to_vec();
                    r.push(1);
                    self.outbox.push_back((SET_ENCRYPTION, r));
                } else {
                    kprintln!("bt: authentication with {} failed: {}", addr_text(&addr), reason(p[0]));
                    self.abort(addr, handle, reason(p[0]));
                }
            }
            EV_ENCRYPTION_CHANGE if p.len() >= 4 => {
                let handle = u16::from_le_bytes([p[1], p[2]]) & 0x0FFF;
                let Some(addr) = self.addr_of(handle) else { return };
                if p[0] == 0 && p[3] != 0 {
                    if let Some(c) = self.conn_mut(handle) {
                        c.encrypted = true;
                    }
                    // LE during pairing — next the device hands out the keys.
                    if self.le_encrypted(handle) {
                        kprintln!("bt: {} encrypted, waiting for its keys", addr_text(&addr));
                        return;
                    }
                    show(addr, Some(Link::Connected));
                    kprintln!("bt: {} connected, encrypted", addr_text(&addr));
                    self.secured(handle);
                    if self.conn_mut(handle).is_some_and(|c| !c.le) {
                        self.audio_connected(handle);
                    }
                    let waiting = self.conn_mut(handle).map(|c| core::mem::take(&mut c.sdp_waiting)).unwrap_or_default();
                    for uuid in waiting {
                        self.sdp_query(handle, uuid);
                    }
                } else {
                    self.abort(addr, handle, format!("encryption failed: {}", reason(p[0])));
                }
            }
            EV_REMOTE_NAME if p.len() >= 7 && p[0] == 0 => {
                let raw = &p[7..];
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                let name = String::from_utf8_lossy(&raw[..end]).into_owned();
                if !name.is_empty() {
                    self.set_name(addr6(&p[1..7]), &name);
                }
            }
            EV_PIN_CODE_REQUEST if p.len() >= 6 => {
                // An old device without Simple Pairing: the standard PIN 0000.
                let addr = addr6(&p[0..6]);
                let mut r = addr.to_vec();
                r.push(4);
                let mut pin = [0u8; 16];
                pin[..4].copy_from_slice(b"0000");
                r.extend_from_slice(&pin);
                self.outbox.push_back((PIN_CODE_REPLY, r));
                show(addr, Some(Link::Pairing("older device, PIN 0000".into())));
            }
            EV_LINK_KEY_REQUEST if p.len() >= 6 => {
                let addr = addr6(&p[0..6]);
                let key = PAIRED.lock().iter().find(|d| d.addr == addr).map(|d| d.key);
                kprintln!("bt: {} asks for the link key: {}", addr_text(&addr), if key.is_some() { "sending the saved one" } else { "none saved, pairing" });
                match key {
                    Some(key) => {
                        let mut r = addr.to_vec();
                        r.extend_from_slice(&key);
                        self.outbox.push_back((LINK_KEY_REPLY, r));
                    }
                    None => {
                        self.outbox.push_back((LINK_KEY_NEGATIVE, addr.to_vec()));
                        show(addr, Some(Link::Pairing(String::new())));
                    }
                }
            }
            EV_LINK_KEY_NOTIFICATION if p.len() >= 22 => {
                let addr = addr6(&p[0..6]);
                let mut key = [0u8; 16];
                key.copy_from_slice(&p[6..22]);
                let name = name_of(&addr);
                {
                    let mut paired = PAIRED.lock();
                    paired.retain(|d| d.addr != addr);
                    paired.push(Paired { addr, le: false, key, name, ediv: 0, rand: [0; 8], irk: None, addr_type: 0 });
                }
                save_paired();
                kprintln!("bt: paired with {}", addr_text(&addr));
            }
            EV_IO_CAP_REQUEST if p.len() >= 6 => {
                let addr = addr6(&p[0..6]);
                let mut r = addr.to_vec();
                r.extend_from_slice(&[IO_DISPLAY_YES_NO, 0x00, AUTH_MITM_BONDING]);
                self.outbox.push_back((IO_CAP_REPLY, r));
                show(addr, Some(Link::Pairing(String::new())));
            }
            EV_IO_CAP_RESPONSE if p.len() >= 7 => {
                if let Some(c) = self.conns.get_mut(&addr6(&p[0..6])) {
                    c.remote_io = p[6];
                }
            }
            EV_USER_CONFIRM_REQUEST if p.len() >= 10 => {
                let addr = addr6(&p[0..6]);
                let value = u32::from_le_bytes([p[6], p[7], p[8], p[9]]);
                if self.conns.get(&addr).is_some_and(|c| c.remote_io == IO_NONE) {
                    // The device has neither a display nor buttons (headphones, many mice) —
                    // nothing to compare, agree.
                    self.outbox.push_back((USER_CONFIRM_REPLY, addr.to_vec()));
                } else {
                    show(addr, Some(Link::Confirm(value)));
                    kprintln!("bt: {} asks to compare a 6-digit code", addr_text(&addr));
                }
            }
            EV_USER_PASSKEY_REQUEST if p.len() >= 6 => {
                let addr = addr6(&p[0..6]);
                self.outbox.push_back((USER_PASSKEY_NEGATIVE, addr.to_vec()));
                show(addr, Some(Link::Failed("the device wants a code typed on this computer: not supported yet".into())));
            }
            EV_USER_PASSKEY_NOTIFICATION if p.len() >= 10 => {
                let key = u32::from_le_bytes([p[6], p[7], p[8], p[9]]);
                show(addr6(&p[0..6]), Some(Link::Pairing(format!("type {key:06} on the device, then Enter"))));
            }
            EV_SIMPLE_PAIRING_COMPLETE if p.len() >= 7 && p[0] != 0 => {
                let addr = addr6(&p[1..7]);
                kprintln!("bt: pairing with {} failed: {}", addr_text(&addr), reason(p[0]));
                show(addr, Some(Link::Failed(reason(p[0]))));
            }
            _ => {}
        }
    }
}
