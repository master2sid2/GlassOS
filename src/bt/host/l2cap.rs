//! L2CAP: dispatching packets to channels and signalling (the device's service requests).
//! LE has fixed channels: ATT, LE signalling, SMP; classic has signalling (1),
//! and dynamic channels for the profiles.

use super::Host;
use crate::kprintln;
use alloc::vec::Vec;

pub(super) const CID_SIGNAL: u16 = 0x0001;
pub(super) const CID_ATT: u16 = 0x0004;
pub(super) const CID_LE_SIGNAL: u16 = 0x0005;
pub(super) const CID_SMP: u16 = 0x0006;

// Signalling commands.
const COMMAND_REJECT: u8 = 0x01;
pub(super) const CONNECTION_REQUEST: u8 = 0x02;
pub(super) const CONNECTION_RESPONSE: u8 = 0x03;
pub(super) const CONFIGURE_REQUEST: u8 = 0x04;
pub(super) const CONFIGURE_RESPONSE: u8 = 0x05;
pub(super) const DISCONNECTION_REQUEST: u8 = 0x06;
pub(super) const DISCONNECTION_RESPONSE: u8 = 0x07;
const ECHO_REQUEST: u8 = 0x08;
const ECHO_RESPONSE: u8 = 0x09;
const INFO_REQUEST: u8 = 0x0A;
const INFO_RESPONSE: u8 = 0x0B;
const CONN_PARAM_UPDATE_REQUEST: u8 = 0x12;
const CONN_PARAM_UPDATE_RESPONSE: u8 = 0x13;
const LE_CREDIT_CONNECTION_REQUEST: u8 = 0x14;
const LE_CREDIT_CONNECTION_RESPONSE: u8 = 0x15;

/// HCI: new LE connection parameters (at the device's request).
const LE_CONNECTION_UPDATE: u16 = 0x2013;

impl Host {
    /// A whole L2CAP packet over link `handle` — to its channel.
    pub(super) fn l2cap(&mut self, handle: u16, cid: u16, payload: &[u8]) {
        let Some(le) = self.conn_mut(handle).map(|c| c.le) else { return };
        match (le, cid) {
            (true, CID_ATT) => self.att(handle, payload),
            (true, CID_LE_SIGNAL) => self.signal(handle, CID_LE_SIGNAL, payload),
            (false, CID_SIGNAL) => self.signal(handle, CID_SIGNAL, payload),
            (true, CID_SMP) => self.smp(handle, payload),
            (false, cid) if cid >= 0x0040 => self.chan_data(handle, cid, payload),
            _ => kprintln!("bt: l2cap: {} bytes on channel {cid:#06x}, not handled yet", payload.len()),
        }
    }

    /// Signalling commands: code, id, length, data (classic may have
    /// several in one packet, LE — one).
    fn signal(&mut self, handle: u16, cid: u16, mut p: &[u8]) {
        while p.len() >= 4 {
            let (code, id, len) = (p[0], p[1], u16::from_le_bytes([p[2], p[3]]) as usize);
            let Some(data) = p.get(4..4 + len) else { break };
            self.signal_command(handle, cid, code, id, data);
            p = &p[4 + len..];
            if cid == CID_LE_SIGNAL {
                break;
            }
        }
    }

    pub(super) fn reply(&mut self, handle: u16, cid: u16, code: u8, id: u8, data: &[u8]) {
        let mut r = Vec::with_capacity(4 + data.len());
        r.push(code);
        r.push(id);
        r.extend_from_slice(&(data.len() as u16).to_le_bytes());
        r.extend_from_slice(data);
        self.send_l2cap(handle, cid, &r);
    }

    fn signal_command(&mut self, handle: u16, cid: u16, code: u8, id: u8, d: &[u8]) {
        let at = |i: usize| d.get(i..i + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        // Classic Bluetooth profile channels.
        if cid == CID_SIGNAL && self.chan_signal(handle, code, id, d) {
            return;
        }
        match code {
            DISCONNECTION_REQUEST if d.len() >= 4 => self.reply(handle, cid, DISCONNECTION_RESPONSE, id, &d[..4]),
            ECHO_REQUEST => self.reply(handle, cid, ECHO_RESPONSE, id, d),
            INFO_REQUEST => {
                // No extended features; the only fixed channel is signalling.
                let kind = at(0);
                let mut r = kind.to_le_bytes().to_vec();
                match kind {
                    0x0002 => r.extend_from_slice(&[0, 0, 0, 0, 0, 0]),
                    0x0003 => r.extend_from_slice(&[0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0]),
                    _ => r.extend_from_slice(&[1, 0]),
                }
                self.reply(handle, cid, INFO_RESPONSE, id, &r);
            }
            CONN_PARAM_UPDATE_REQUEST if d.len() >= 8 => {
                // The device asks for another connection interval (mice — more often, sensors — less often):
                // agree and ask the controller.
                self.reply(handle, cid, CONN_PARAM_UPDATE_RESPONSE, id, &[0, 0]);
                let mut c = handle.to_le_bytes().to_vec();
                c.extend_from_slice(&d[..8]); // interval from/to, latency, timeout
                c.extend_from_slice(&[0, 0, 0, 0]);
                self.outbox.push_back((LE_CONNECTION_UPDATE, c));
            }
            LE_CREDIT_CONNECTION_REQUEST => {
                // An LE credit-based channel: "LE_PSM not supported".
                self.reply(handle, cid, LE_CREDIT_CONNECTION_RESPONSE, id, &[0, 0, 0, 0, 0, 0, 0, 0, 0x02, 0]);
            }
            // Answers (we ask nothing yet) and rejections — no answer.
            COMMAND_REJECT | CONNECTION_RESPONSE | CONFIGURE_RESPONSE | DISCONNECTION_RESPONSE | ECHO_RESPONSE | INFO_RESPONSE | CONN_PARAM_UPDATE_RESPONSE | LE_CREDIT_CONNECTION_RESPONSE => {}
            // Everything else — "command not understood".
            _ => self.reply(handle, cid, COMMAND_REJECT, id, &[0, 0]),
        }
    }
}
