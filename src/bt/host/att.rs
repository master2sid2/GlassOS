//! ATT — the basis of GATT. The client: our requests to the device, by protocol one in flight
//! (the next ones wait in a queue), and for each we remember why we asked. Our server is
//! empty for now: to the device's questions about our services we honestly answer "no such thing",
//! indications are confirmed.

use super::Host;
use super::l2cap::CID_ATT;
use crate::bt::addr_text;
use crate::kprintln;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub(super) const ERROR_RSP: u8 = 0x01;
const MTU_REQ: u8 = 0x02;
const MTU_RSP: u8 = 0x03;
pub(super) const FIND_INFO_REQ: u8 = 0x04;
const FIND_INFO_RSP: u8 = 0x05;
const FIND_BY_TYPE_REQ: u8 = 0x06;
pub(super) const READ_BY_TYPE_REQ: u8 = 0x08;
const READ_BY_TYPE_RSP: u8 = 0x09;
const READ_BY_GROUP_REQ: u8 = 0x10;
const READ_BY_GROUP_RSP: u8 = 0x11;
pub(super) const READ_REQ: u8 = 0x0A;
const READ_RSP: u8 = 0x0B;
pub(super) const READ_BLOB_REQ: u8 = 0x0C;
const READ_BLOB_RSP: u8 = 0x0D;
pub(super) const WRITE_REQ: u8 = 0x12;
pub(super) const WRITE_CMD: u8 = 0x52;
/// The "primary service" attribute type — services of the device are walked by it.
const PRIMARY_SERVICE: u16 = 0x2800;
const NOTIFY: u8 = 0x1B;
const INDICATE: u8 = 0x1D;
const CONFIRM: u8 = 0x1E;
/// Answers to client requests (after them — the next request from the queue).
const RESPONSES: &[u8] = &[0x01, 0x03, 0x05, 0x07, 0x09, 0x0B, 0x0D, 0x0F, 0x11, 0x13, 0x17, 0x19, 0x21];
pub(super) const ATTRIBUTE_NOT_FOUND: u8 = 0x0A;
const REQUEST_NOT_SUPPORTED: u8 = 0x06;
/// Our ATT MTU: enough for HID reports and long names.
const OUR_MTU: u16 = 185;
/// The "device name" characteristic (the GAP service).
const DEVICE_NAME: u16 = 0x2A00;

/// Why a request was sent: the answer is parsed by this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Want {
    Mtu,
    Name,
    Services,
    /// Declarations of the HID service characteristics (up to handle `end`).
    Chars { end: u16 },
    /// All attributes of the HID service with their types (up to `end`).
    Infos { end: u16 },
    /// The HID report descriptor (`at` is its handle), in pieces.
    ReportMap { at: u16 },
    /// The HID report id for characteristic `value`.
    ReportRef { value: u16 },
    /// The battery level (the service up to `end`) and the descriptors after it.
    Battery { end: u16 },
    BatteryInfo,
    /// A write (enabling notifications and the like).
    Write,
}

impl Host {
    /// Right after an LE connection: the MTU, the device name, the service list.
    pub(super) fn att_start(&mut self, handle: u16) {
        let mut mtu = vec![MTU_REQ];
        mtu.extend_from_slice(&OUR_MTU.to_le_bytes());
        self.att_request(handle, Want::Mtu, mtu);
        self.read_by_type(handle, Want::Name, 0x0001, 0xFFFF, DEVICE_NAME);
        self.discover_services(handle, 0x0001);
    }

    /// Read By Type: attributes of type `kind` in a handle range.
    pub(super) fn read_by_type(&mut self, handle: u16, want: Want, from: u16, to: u16, kind: u16) {
        let mut q = vec![READ_BY_TYPE_REQ];
        for v in [from, to, kind] {
            q.extend_from_slice(&v.to_le_bytes());
        }
        self.att_request(handle, want, q);
    }

    /// Walking the GATT services from handle `from` (a piece per answer).
    fn discover_services(&mut self, handle: u16, from: u16) {
        let mut q = vec![READ_BY_GROUP_REQ];
        for v in [from, 0xFFFF, PRIMARY_SERVICE] {
            q.extend_from_slice(&v.to_le_bytes());
        }
        self.att_request(handle, Want::Services, q);
    }

    /// The services were walked — to the log (HID 0x1812 is what keyboards and mice need).
    fn services_done(&mut self, handle: u16) {
        let Some(addr) = self.addr_of(handle) else { return };
        let list: Vec<String> = self.conn_mut(handle).map_or(Vec::new(), |c| {
            c.services_ready = true;
            c.services
                .iter()
                .map(|&(_, _, uuid)| match uuid {
                    0 => String::from("custom"),
                    0x1812 => String::from("1812 (HID)"),
                    u => alloc::format!("{u:04x}"),
                })
                .collect()
        });
        kprintln!("bt: {} GATT services: {}", addr_text(&addr), list.join(", "));
        self.secured(handle);
    }

    /// The link is encrypted and the services are known: start the profiles the device has.
    pub(super) fn secured(&mut self, handle: u16) {
        self.hid_maybe_start(handle);
        self.battery_maybe_start(handle);
    }

    fn notified(&mut self, handle: u16, at: u16, value: &[u8]) {
        if !self.battery_notify(handle, at, value) {
            self.hid_notify(handle, at, value);
        }
    }

    /// A client request: one in flight, the others wait for the answer.
    pub(super) fn att_request(&mut self, handle: u16, want: Want, pdu: Vec<u8>) {
        let Some(c) = self.conn_mut(handle) else { return };
        if c.att_busy.is_some() {
            c.att_queue.push_back((want, pdu));
            return;
        }
        c.att_busy = Some(want);
        self.send_l2cap(handle, CID_ATT, &pdu);
    }

    /// An answer arrived — the next request from the queue.
    fn att_next(&mut self, handle: u16) {
        let Some(c) = self.conn_mut(handle) else { return };
        c.att_busy = None;
        if let Some((want, pdu)) = c.att_queue.pop_front() {
            c.att_busy = Some(want);
            self.send_l2cap(handle, CID_ATT, &pdu);
        }
    }

    fn set_mtu(&mut self, handle: u16, theirs: u16) {
        if let Some(c) = self.conn_mut(handle) {
            c.att_mtu = theirs.clamp(23, OUR_MTU);
        }
    }

    pub(super) fn att(&mut self, handle: u16, pdu: &[u8]) {
        let Some(&op) = pdu.first() else { return };
        let at = |i: usize| pdu.get(i..i + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        match op {
            // The device as a client — to our server.
            MTU_REQ => {
                self.set_mtu(handle, at(1));
                let mut r = vec![MTU_RSP];
                r.extend_from_slice(&OUR_MTU.to_le_bytes());
                self.send_l2cap(handle, CID_ATT, &r);
            }
            FIND_INFO_REQ | FIND_BY_TYPE_REQ | READ_BY_TYPE_REQ | READ_BY_GROUP_REQ => {
                // It explores our services — we have none yet.
                let start = at(1).to_le_bytes();
                self.send_l2cap(handle, CID_ATT, &[ERROR_RSP, op, start[0], start[1], ATTRIBUTE_NOT_FOUND]);
            }
            INDICATE => {
                self.send_l2cap(handle, CID_ATT, &[CONFIRM]);
                self.notified(handle, at(1), &pdu[3.min(pdu.len())..]);
            }
            // Notifications: HID reports, battery and others by subscription.
            NOTIFY if pdu.len() >= 3 => self.notified(handle, at(1), &pdu[3..]),
            // Answers to our requests.
            op if RESPONSES.contains(&op) => {
                let want = self.conn_mut(handle).and_then(|c| c.att_busy);
                // First the next request from the queue: parsing the answer may add new ones.
                self.att_next(handle);
                match (want, op) {
                    (_, MTU_RSP) => self.set_mtu(handle, at(1)),
                    (Some(Want::Name), READ_BY_TYPE_RSP) if pdu.len() >= 2 => {
                        // The entry length, entries: handle (2) + value.
                        let len = pdu[1] as usize;
                        if let Some(rec) = pdu.get(2..2 + len).filter(|_| len > 2) {
                            let name = String::from(String::from_utf8_lossy(&rec[2..]).trim_end_matches('\0'));
                            if let Some(addr) = self.addr_of(handle).filter(|_| !name.is_empty()) {
                                kprintln!("bt: {} says over GATT its name is {name:?}", addr_text(&addr));
                                self.set_name(addr, &name);
                            }
                        }
                    }
                    (Some(Want::Services), READ_BY_GROUP_RSP) if pdu.len() >= 2 => {
                        // Entries: start (2), end (2), UUID (2 or 16).
                        let len = (pdu[1] as usize).max(6);
                        let mut last = 0u16;
                        if let Some(c) = self.conn_mut(handle) {
                            for r in pdu[2..].chunks_exact(len) {
                                let (start, end) = (u16::from_le_bytes([r[0], r[1]]), u16::from_le_bytes([r[2], r[3]]));
                                let uuid = if len == 6 { u16::from_le_bytes([r[4], r[5]]) } else { 0 };
                                c.services.push((start, end, uuid));
                                last = last.max(end);
                            }
                        }
                        if last != 0 && last != 0xFFFF {
                            self.discover_services(handle, last + 1);
                        } else {
                            self.services_done(handle);
                        }
                    }
                    // "No more" on the service walk — the walk is finished.
                    (Some(Want::Services), ERROR_RSP) => self.services_done(handle),
                    (Some(Want::Chars { end }), READ_BY_TYPE_RSP) => self.hid_chars(handle, end, pdu),
                    (Some(Want::Chars { .. }), ERROR_RSP) => self.hid_chars_done(handle),
                    (Some(Want::Infos { end }), FIND_INFO_RSP) => self.hid_infos(handle, end, pdu),
                    (Some(Want::Infos { .. }), ERROR_RSP) => self.hid_configure(handle),
                    (Some(Want::ReportMap { at }), READ_RSP | READ_BLOB_RSP) => self.hid_map(handle, at, Some(&pdu[1..])),
                    (Some(Want::ReportMap { at }), ERROR_RSP) => self.hid_map(handle, at, None),
                    (Some(Want::ReportRef { value }), READ_RSP) => self.hid_ref(handle, value, Some(&pdu[1..])),
                    (Some(Want::ReportRef { value }), ERROR_RSP) => self.hid_ref(handle, value, None),
                    (Some(Want::Battery { end }), READ_BY_TYPE_RSP) => self.battery_read(handle, end, pdu),
                    (Some(Want::BatteryInfo), FIND_INFO_RSP) => self.battery_infos(handle, pdu),
                    (Some(Want::Battery { .. } | Want::BatteryInfo), ERROR_RSP) => {}
                    (Some(Want::Name), ERROR_RSP) => {}
                    (_, ERROR_RSP) => kprintln!(
                        "bt: att: request {:#04x} (handle {:#06x}) refused, error {:#04x}",
                        pdu.get(1).copied().unwrap_or(0),
                        at(2),
                        pdu.get(4).copied().unwrap_or(0)
                    ),
                    _ => {}
                }
            }
            // Other requests to our server get "not supported"; commands (bit 6) and
            // confirmations get no answer.
            op if op & 0x40 == 0 && op != CONFIRM && op != NOTIFY => self.send_l2cap(handle, CID_ATT, &[ERROR_RSP, op, 0, 0, REQUEST_NOT_SUPPORTED]),
            _ => {}
        }
    }
}
