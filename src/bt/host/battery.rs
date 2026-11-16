//! The battery service (0x180F): the device charge in percent. Read on connection,
//! subscribed to changes; the battery icon in the tray and the Power section show it.

use super::Host;
use super::att::{FIND_INFO_REQ, WRITE_REQ, Want};
use crate::bt::{addr_text, name_of};
use alloc::format;
use alloc::string::String;
use alloc::vec;

const BATTERY_SERVICE: u16 = 0x180F;
const BATTERY_LEVEL: u16 = 0x2A19;
const CCCD: u16 = 0x2902;

/// The key in the shared battery list.
fn id(addr: &[u8; 6]) -> String {
    format!("bt:{}", addr_text(addr))
}

impl Host {
    /// The link is encrypted and the services are known: read the charge.
    pub(super) fn battery_maybe_start(&mut self, handle: u16) {
        let Some(c) = self.conn_mut(handle) else { return };
        if !c.encrypted || !c.services_ready || c.smp.is_some() || c.battery_asked {
            return;
        }
        let Some(&(start, end, _)) = c.services.iter().find(|s| s.2 == BATTERY_SERVICE) else { return };
        c.battery_asked = true;
        self.read_by_type(handle, Want::Battery { end }, start, end, BATTERY_LEVEL);
    }

    /// An answer: the value handle and the charge. Then — where to enable notifications.
    pub(super) fn battery_read(&mut self, handle: u16, end: u16, pdu: &[u8]) {
        let len = pdu.get(1).copied().unwrap_or(0) as usize;
        let Some(rec) = pdu.get(2..2 + len).filter(|_| len >= 3) else { return };
        let at = u16::from_le_bytes([rec[0], rec[1]]);
        if let Some(c) = self.conn_mut(handle) {
            c.battery = Some(at);
        }
        self.battery_level(handle, rec[2]);
        if let Some(addr) = self.addr_of(handle) {
            crate::kprintln!("bt: {} battery {}%", addr_text(&addr), rec[2]);
        }
        let mut q = vec![FIND_INFO_REQ];
        q.extend_from_slice(&(at + 1).to_le_bytes());
        q.extend_from_slice(&end.to_le_bytes());
        self.att_request(handle, Want::BatteryInfo, q);
    }

    /// The descriptors after the value: the first CCCD — enable notifications.
    pub(super) fn battery_infos(&mut self, handle: u16, pdu: &[u8]) {
        if pdu.get(1) != Some(&1) {
            return;
        }
        let cccd = pdu[2..].chunks_exact(4).find(|r| u16::from_le_bytes([r[2], r[3]]) == CCCD).map(|r| u16::from_le_bytes([r[0], r[1]]));
        if let Some(at) = cccd {
            let mut w = vec![WRITE_REQ];
            w.extend_from_slice(&at.to_le_bytes());
            w.extend_from_slice(&[0x01, 0x00]);
            self.att_request(handle, Want::Write, w);
        }
    }

    /// A notification on `at` — is it the charge? Then update and say "yes".
    pub(super) fn battery_notify(&mut self, handle: u16, at: u16, value: &[u8]) -> bool {
        if self.conn_mut(handle).and_then(|c| c.battery) != Some(at) {
            return false;
        }
        if let Some(&level) = value.first() {
            self.battery_level(handle, level);
        }
        true
    }

    pub(super) fn battery_level(&mut self, handle: u16, level: u8) {
        let Some(addr) = self.addr_of(handle) else { return };
        if let Some(c) = self.conn_mut(handle) {
            c.battery_last = Some(level);
        }
        let kind = self
            .conn_mut(handle)
            .map(|c| match &c.hid {
                Some(h) => h.kind(),
                None if c.hfp.is_some() || c.a2dp.is_some() => "headphones",
                None => "device",
            })
            .unwrap_or("device");
        let name = name_of(&addr);
        let name = if name.trim().is_empty() { addr_text(&addr) } else { String::from(name.trim()) };
        crate::battery::set_device(&id(&addr), &name, kind, level as u32);
    }

    /// The link dropped — the charge is no longer known.
    pub(super) fn battery_gone(addr: &[u8; 6]) {
        crate::battery::remove_device(&id(addr));
    }
}
