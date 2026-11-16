//! HID over GATT (HOGP): Bluetooth LE keyboards and mice. After encryption we walk
//! the HID service (0x1812): characteristics and their descriptors. Then we read the report
//! descriptor (Report Map) and report ids (Report Reference) — and work in full report mode:
//! media keys, the wheel, all buttons. If the descriptor cannot be understood — the boot protocol (only
//! ordinary keys and three mouse buttons). The reports themselves are parsed by the shared `crate::hid`.

use super::Host;
use super::att::{FIND_INFO_REQ, READ_BLOB_REQ, READ_REQ, WRITE_CMD, WRITE_REQ, Want};
use super::l2cap::CID_ATT;
use crate::bt::addr_text;
use crate::hid;
use crate::kprintln;
use alloc::vec;
use alloc::vec::Vec;

const HID_SERVICE: u16 = 0x1812;
const CHARACTERISTIC: u16 = 0x2803;
const CCCD: u16 = 0x2902;
const PROTOCOL_MODE: u16 = 0x2A4E;
const REPORT: u16 = 0x2A4D;
const REPORT_MAP: u16 = 0x2A4B;
const REPORT_REFERENCE: u16 = 0x2908;
/// The report type in Report Reference: input.
const INPUT_REPORT: u8 = 1;
const REPORT_PROTOCOL: u8 = 1;
/// A descriptor longer than this is corrupted.
const MAP_MAX: usize = 4096;
const BOOT_KEYBOARD_INPUT: u16 = 0x2A22;
const BOOT_MOUSE_INPUT: u16 = 0x2A33;
/// The characteristic property "can notify".
const PROP_NOTIFY: u8 = 0x10;
const BOOT_PROTOCOL: u8 = 0;

/// What the device sends on a value handle.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    BootKeyboard,
    BootMouse,
    /// An input report of report mode with this id.
    Report(u8),
}

/// The HID service of one link.
pub(super) struct Hogp {
    start: u16,
    end: u16,
    /// Characteristics: declaration handle, properties, value handle, UUID.
    chars: Vec<(u16, u8, u16, u16)>,
    /// All the service's attributes: handle and 16-bit type.
    infos: Vec<(u16, u16)>,
    /// What we subscribed to.
    sources: Vec<(u16, Source)>,
    /// Reports whose length has already been logged (not understood).
    told: Vec<u16>,
    keyboard: hid::Keyboard,
    /// The report descriptor: how it was read and what was understood.
    map: Vec<u8>,
    layout: Option<hid::report::Layout>,
    /// Report ids by value handle (from Report Reference): id, type.
    refs: Vec<(u16, u8, u8)>,
    /// How many descriptor reads we still wait for.
    pending: usize,
    /// Full-mode keyboards — each report has its own (otherwise the reports would cancel each
    /// other's key presses), and media.
    keyboards: Vec<(u8, hid::Keyboard)>,
    media: hid::Media,
    /// The first report arrived — log it (without contents: these are key presses).
    working: bool,
}

impl Host {
    /// Start when the link is encrypted, the services are known and HID is among them.
    pub(super) fn hid_maybe_start(&mut self, handle: u16) {
        let Some(c) = self.conn_mut(handle) else { return };
        if !c.encrypted || !c.services_ready || c.smp.is_some() || c.hid.is_some() {
            return;
        }
        let Some(&(start, end, _)) = c.services.iter().find(|s| s.2 == HID_SERVICE) else { return };
        c.hid = Some(Hogp {
            start,
            end,
            chars: Vec::new(),
            infos: Vec::new(),
            sources: Vec::new(),
            told: Vec::new(),
            keyboard: hid::Keyboard::new(),
            map: Vec::new(),
            layout: None,
            refs: Vec::new(),
            pending: 0,
            keyboards: Vec::new(),
            media: hid::Media::default(),
            working: false,
        });
        self.read_by_type(handle, Want::Chars { end }, start, end, CHARACTERISTIC);
    }

    /// A piece of characteristic declarations: handle (2), properties (1), value (2), UUID.
    pub(super) fn hid_chars(&mut self, handle: u16, end: u16, pdu: &[u8]) {
        let len = pdu.get(1).copied().unwrap_or(0) as usize;
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hid.as_mut()) else { return };
        let mut last = 0;
        if len >= 7 {
            for r in pdu[2..].chunks_exact(len) {
                let decl = u16::from_le_bytes([r[0], r[1]]);
                let uuid = if len == 7 { u16::from_le_bytes([r[5], r[6]]) } else { 0 };
                h.chars.push((decl, r[2], u16::from_le_bytes([r[3], r[4]]), uuid));
                last = decl;
            }
        }
        if last != 0 && last < end {
            self.read_by_type(handle, Want::Chars { end }, last + 1, end, CHARACTERISTIC);
        } else {
            self.hid_chars_done(handle);
        }
    }

    /// The characteristics are known — now the descriptors (where to enable notifications).
    pub(super) fn hid_chars_done(&mut self, handle: u16) {
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hid.as_ref()) else { return };
        let (start, end) = (h.start, h.end);
        self.find_info(handle, start, end);
    }

    fn find_info(&mut self, handle: u16, from: u16, end: u16) {
        let mut q = vec![FIND_INFO_REQ];
        q.extend_from_slice(&from.to_le_bytes());
        q.extend_from_slice(&end.to_le_bytes());
        self.att_request(handle, Want::Infos { end }, q);
    }

    /// A piece of the attribute list: format 1 — pairs (handle, 16-bit type).
    pub(super) fn hid_infos(&mut self, handle: u16, end: u16, pdu: &[u8]) {
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hid.as_mut()) else { return };
        let step = if pdu.get(1) == Some(&1) { 4 } else { 18 };
        let mut last = 0;
        for r in pdu.get(2..).unwrap_or(&[]).chunks_exact(step) {
            let at = u16::from_le_bytes([r[0], r[1]]);
            h.infos.push((at, if step == 4 { u16::from_le_bytes([r[2], r[3]]) } else { 0 }));
            last = at;
        }
        if last != 0 && last < end {
            self.find_info(handle, last + 1, end);
        } else {
            self.hid_configure(handle);
        }
    }

    /// The characteristics and descriptors are known: read the report descriptor and the ids
    /// of the input reports, then subscribe.
    pub(super) fn hid_configure(&mut self, handle: u16) {
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hid.as_mut()) else { return };
        if !h.sources.is_empty() || h.pending != 0 {
            return;
        }
        let map = h.chars.iter().find(|c| c.3 == REPORT_MAP).map(|c| c.2);
        let reports: Vec<u16> = h.chars.iter().filter(|c| c.3 == REPORT && c.1 & PROP_NOTIFY != 0).map(|c| c.2).collect();
        let Some(map) = map.filter(|_| !reports.is_empty()) else {
            self.hid_subscribe(handle);
            return;
        };
        let refs: Vec<(u16, u16)> = reports.iter().filter_map(|&v| h.descriptor(v, REPORT_REFERENCE).map(|d| (v, d))).collect();
        h.pending = 1 + refs.len();
        let mut q = vec![READ_REQ];
        q.extend_from_slice(&map.to_le_bytes());
        self.att_request(handle, Want::ReportMap { at: map }, q);
        for (value, at) in refs {
            let mut q = vec![READ_REQ];
            q.extend_from_slice(&at.to_le_bytes());
            self.att_request(handle, Want::ReportRef { value }, q);
        }
    }

    /// A piece of the report descriptor. A full packet means there is more (Read Blob).
    pub(super) fn hid_map(&mut self, handle: u16, at: u16, piece: Option<&[u8]>) {
        let mtu = self.conn_mut(handle).map_or(23, |c| c.att_mtu) as usize;
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hid.as_mut()) else { return };
        if let Some(p) = piece {
            h.map.extend_from_slice(p);
            if p.len() == mtu - 1 && h.map.len() < MAP_MAX {
                let mut q = vec![READ_BLOB_REQ];
                q.extend_from_slice(&at.to_le_bytes());
                q.extend_from_slice(&(h.map.len() as u16).to_le_bytes());
                self.att_request(handle, Want::ReportMap { at }, q);
                return;
            }
        }
        self.hid_read_done(handle);
    }

    /// The report id and type for characteristic `value`.
    pub(super) fn hid_ref(&mut self, handle: u16, value: u16, data: Option<&[u8]>) {
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hid.as_mut()) else { return };
        if let Some(&[id, kind, ..]) = data {
            h.refs.push((value, id, kind));
        }
        self.hid_read_done(handle);
    }

    fn hid_read_done(&mut self, handle: u16) {
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hid.as_mut()) else { return };
        h.pending = h.pending.saturating_sub(1);
        if h.pending == 0 {
            self.hid_subscribe(handle);
        }
    }

    /// Choose the protocol and subscribe to the reports.
    fn hid_subscribe(&mut self, handle: u16) {
        let Some(addr) = self.addr_of(handle) else { return };
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hid.as_mut()) else { return };
        if !h.sources.is_empty() {
            return;
        }
        h.layout = hid::report::parse(&h.map);
        let protocol = h.chars.iter().find(|c| c.3 == PROTOCOL_MODE).map(|c| c.2);
        let notify = |uuid: u16| h.chars.iter().filter(move |c| c.3 == uuid && c.1 & PROP_NOTIFY != 0).map(|c| c.2);
        // Full mode: input reports the descriptor knows.
        let full: Vec<(u16, Source)> = match &h.layout {
            Some(layout) => notify(REPORT)
                .filter_map(|v| {
                    let (id, kind) = h.refs.iter().find(|r| r.0 == v).map_or((0, INPUT_REPORT), |r| (r.1, r.2));
                    (kind == INPUT_REPORT && layout.reports.iter().any(|r| r.id == id)).then_some((v, Source::Report(id)))
                })
                .collect(),
            None => Vec::new(),
        };
        let boot: Vec<(u16, Source)> = notify(BOOT_KEYBOARD_INPUT)
            .map(|v| (v, Source::BootKeyboard))
            .chain(notify(BOOT_MOUSE_INPUT).map(|v| (v, Source::BootMouse)))
            .collect();
        let use_boot = full.is_empty() && protocol.is_some() && !boot.is_empty();
        h.sources = if use_boot { boot } else { full };
        let cccds: Vec<u16> = h.sources.iter().filter_map(|&(value, _)| h.descriptor(value, CCCD)).collect();
        let what: Vec<alloc::string::String> = h
            .sources
            .iter()
            .map(|s| match s.1 {
                Source::BootKeyboard => "boot keyboard".into(),
                Source::BootMouse => "boot mouse".into(),
                Source::Report(id) => {
                    let kind = h.layout.as_ref().and_then(|l| l.reports.iter().find(|r| r.id == id)).map_or("?", |r| r.kind());
                    alloc::format!("report {id} ({kind})")
                }
            })
            .collect();
        kprintln!(
            "bt: {} HID: {} characteristics, report map {} bytes{}; {}; listening to {}",
            addr_text(&addr),
            h.chars.len(),
            h.map.len(),
            if h.layout.is_some() || h.map.is_empty() { "" } else { " (not understood)" },
            if use_boot { "boot protocol" } else { "report protocol" },
            if what.is_empty() { "nothing".into() } else { what.join(", ") }
        );
        if let Some(p) = protocol.filter(|_| !h.sources.is_empty()) {
            let mut w = vec![WRITE_CMD];
            w.extend_from_slice(&p.to_le_bytes());
            w.push(if use_boot { BOOT_PROTOCOL } else { REPORT_PROTOCOL });
            self.send_l2cap(handle, CID_ATT, &w);
        }
        for at in cccds {
            let mut w = vec![WRITE_REQ];
            w.extend_from_slice(&at.to_le_bytes());
            w.extend_from_slice(&[0x01, 0x00]); // notifications
            self.att_request(handle, Want::Write, w);
        }
        // The battery may have been learned before we understood what device this is: refresh the view.
        if let Some(level) = self.conn_mut(handle).and_then(|c| c.battery_last) {
            self.battery_level(handle, level);
        }
    }

    /// A notification on handle `at`: a HID report goes to input.
    pub(super) fn hid_notify(&mut self, handle: u16, at: u16, value: &[u8]) {
        let Some(addr) = self.addr_of(handle) else { return };
        let Some(h) = self.conn_mut(handle).and_then(|c| c.hid.as_mut()) else { return };
        let Some(&(_, source)) = h.sources.iter().find(|s| s.0 == at) else { return };
        let mut out = Vec::new();
        match source {
            Source::BootKeyboard => h.keyboard.report(value, &mut out),
            Source::BootMouse => hid::mouse(value, &mut out),
            Source::Report(id) => {
                let Some(layout) = &h.layout else { return };
                let mut packet = Vec::with_capacity(value.len() + 1);
                if layout.numbered {
                    packet.push(id);
                }
                packet.extend_from_slice(value);
                let Some((report, data)) = layout.find(&packet) else { return };
                let d = report.decode(data);
                if let Some((mods, keys)) = &d.keyboard {
                    let kb = match h.keyboards.iter().position(|k| k.0 == id) {
                        Some(i) => &mut h.keyboards[i].1,
                        None => {
                            h.keyboards.push((id, hid::Keyboard::new()));
                            &mut h.keyboards.last_mut().unwrap().1
                        }
                    };
                    kb.keys(*mods, keys, &mut out);
                }
                if let Some(pressed) = &d.consumer {
                    h.media.report(pressed, &mut out);
                }
                if let Some((dx, dy, buttons, wheel)) = d.mouse {
                    out.push(crate::input::Event::MouseMove { dx, dy, buttons, wheel: -wheel });
                }
                if !h.told.contains(&at) && d.keyboard.is_none() && d.consumer.is_none() && d.mouse.is_none() {
                    h.told.push(at);
                    kprintln!("bt: {} HID: report {id} has nothing we use yet", addr_text(&addr));
                }
            }
        }
        if !h.working && !out.is_empty() {
            h.working = true;
            kprintln!("bt: {} HID: input works", addr_text(&addr));
        }
        hid::push(out);
    }

    /// Key auto-repeat for Bluetooth keyboards (called from the thread loop).
    pub(super) fn hid_tick(&mut self, now: u64) {
        let mut out = Vec::new();
        for h in self.conns.values_mut().filter_map(|c| c.hid.as_mut()) {
            h.keyboard.tick(now, &mut out);
            for (_, kb) in &mut h.keyboards {
                kb.tick(now, &mut out);
            }
        }
        hid::push(out);
    }
}

impl Hogp {
    /// What kind of device this is — by its reports (for the battery list).
    pub(super) fn kind(&self) -> &'static str {
        let Some(l) = &self.layout else {
            return if self.sources.iter().any(|s| s.1 == Source::BootKeyboard) { "keyboard" } else { "mouse" };
        };
        if l.reports.iter().any(|r| r.has_keyboard()) {
            "keyboard"
        } else if l.reports.iter().any(|r| r.has_mouse()) {
            "mouse"
        } else {
            "device"
        }
    }

    /// The handle of descriptor type `kind` of the characteristic with value `value`: after the value and before
    /// the next declaration.
    fn descriptor(&self, value: u16, kind: u16) -> Option<u16> {
        let next = self.chars.iter().map(|c| c.0).filter(|&d| d > value).min().unwrap_or(u16::MAX);
        self.infos.iter().find(|&&(at, k)| at > value && at < next && k == kind).map(|&(at, _)| at)
    }

    /// The link dropped: release everything that was pressed.
    pub(super) fn release(&mut self) {
        let mut out = Vec::new();
        self.keyboard.release(&mut out);
        for (_, kb) in &mut self.keyboards {
            kb.release(&mut out);
        }
        hid::push(out);
    }
}
