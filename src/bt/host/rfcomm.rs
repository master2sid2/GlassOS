//! RFCOMM — "serial ports" over an L2CAP channel (PSM 3), per TS 07.10 with
//! Bluetooth credits. The headset lives on it (HFP: AT commands). The multiplexer is on
//! channel 0, the ports are DLCIs (channel number × 2). We connect ourselves (the channel number from SDP)
//! or accept. A port's data goes to the protocol by channel number (`rfcomm_*` → HFP).

use super::Host;
use super::chan::PSM_RFCOMM;
use crate::kprintln;
use alloc::vec;
use alloc::vec::Vec;

const SABM: u8 = 0x2F;
const UA: u8 = 0x63;
const DM: u8 = 0x0F;
const DISC: u8 = 0x43;
const UIH: u8 = 0xEF;
const PF: u8 = 0x10;
// Multiplexer commands (the type with the EA bit; C/R separately).
const MCC_PN: u8 = 0x80;
const MCC_MSC: u8 = 0xE0;
const MCC_RPN: u8 = 0x90;
const MCC_RLS: u8 = 0x50;
const MCC_TEST: u8 = 0x20;
const MCC_NSC: u8 = 0x10;
/// Our port frame size and how many frames we allow to be sent at once.
const OUR_FRAME: u16 = 127;
const OUR_CREDITS: u8 = 7;
/// Our server channels: the headset (the HFP gateway).
pub(super) const CHANNEL_HFP_AG: u8 = 1;

/// A port.
pub(super) struct Dlc {
    dlci: u8,
    open: bool,
    /// How many more frames may be sent to us / we are allowed to send.
    credits_in: u8,
    credits_out: u8,
    frame: u16,
    /// Waiting for credits.
    waiting: Vec<Vec<u8>>,
}

pub(super) struct Rfcomm {
    chan: Option<u16>,
    open: bool,
    /// We started the session (the C/R bit depends on it).
    initiator: bool,
    mux: bool,
    dlcs: Vec<Dlc>,
    /// The port to open once the multiplexer comes up.
    want: Option<u8>,
}

/// The TS 07.10 CRC: x^8 + x^2 + x + 1, reflected, initial 0xFF; FCS = 0xFF − CRC.
fn fcs(bytes: &[u8]) -> u8 {
    let mut crc = 0xFFu8;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xE0 } else { crc >> 1 };
        }
    }
    0xFF - crc
}

impl Host {
    fn rfcomm(&mut self, handle: u16) -> Option<&mut Rfcomm> {
        self.conn_mut(handle).and_then(|c| c.rfcomm.as_mut())
    }

    /// Open port `channel` of the device (the number from SDP).
    pub(super) fn rfcomm_connect(&mut self, handle: u16, channel: u8) {
        let Some(c) = self.conn_mut(handle) else { return };
        if let Some(r) = &c.rfcomm {
            // The session already exists: the port right away.
            if r.mux {
                let dlci = channel << 1;
                self.dlc_open(handle, dlci);
            }
            return;
        }
        c.rfcomm = Some(Rfcomm { chan: None, open: false, initiator: true, mux: false, dlcs: Vec::new(), want: Some(channel) });
        let local = self.chan_open(handle, PSM_RFCOMM);
        if let Some(r) = self.rfcomm(handle) {
            r.chan = local;
        }
    }

    /// A frame: address, control, length, [credits], data, FCS.
    fn rfcomm_frame(&mut self, handle: u16, dlci: u8, command: bool, control: u8, credits: Option<u8>, data: &[u8]) {
        let Some(r) = self.rfcomm(handle) else { return };
        let Some(local) = r.chan else { return };
        // C/R: commands of the session initiator are 1, responses the opposite; UIH is like a command.
        let cr = (command == r.initiator) as u8;
        let mut f = vec![dlci << 2 | cr << 1 | 1, control | if credits.is_some() { PF } else { 0 }];
        if data.len() < 128 {
            f.push((data.len() as u8) << 1 | 1);
        } else {
            f.push((data.len() << 1) as u8 & 0xFE);
            f.push((data.len() >> 7) as u8);
        }
        let covered = if control & !PF == UIH { 2 } else { f.len() };
        let check = fcs(&f[..covered]);
        if let Some(c) = credits {
            f.push(c);
        }
        f.extend_from_slice(data);
        f.push(check);
        self.chan_send(handle, local, &f);
    }

    /// A multiplexer command (channel 0): type, length, data.
    fn rfcomm_mcc(&mut self, handle: u16, kind: u8, command: bool, data: &[u8]) {
        let mut m = vec![kind | (command as u8) << 1 | 1, (data.len() as u8) << 1 | 1];
        m.extend_from_slice(data);
        self.rfcomm_frame(handle, 0, true, UIH, None, &m);
    }

    fn dlc_open(&mut self, handle: u16, dlci: u8) {
        let Some(r) = self.rfcomm(handle) else { return };
        if !r.dlcs.iter().any(|d| d.dlci == dlci) {
            r.dlcs.push(Dlc { dlci, open: false, credits_in: OUR_CREDITS, credits_out: 0, frame: OUR_FRAME, waiting: Vec::new() });
        }
        // Port parameters: credits (request 0xF0), frame size, how many we allow at once.
        let f = OUR_FRAME.to_le_bytes();
        self.rfcomm_mcc(handle, MCC_PN, true, &[dlci, 0xF0, 0, 0, f[0], f[1], 0, OUR_CREDITS]);
    }

    /// A line to the port (respecting credits: without them it waits).
    pub(super) fn rfcomm_send(&mut self, handle: u16, dlci: u8, data: &[u8]) {
        let Some(d) = self.rfcomm(handle).and_then(|r| r.dlcs.iter_mut().find(|d| d.dlci == dlci)) else { return };
        if d.credits_out == 0 || !d.open {
            d.waiting.push(data.to_vec());
            return;
        }
        d.credits_out -= 1;
        // Return credits when there are few left.
        let give = if d.credits_in < OUR_CREDITS / 2 {
            let g = OUR_CREDITS - d.credits_in;
            d.credits_in = OUR_CREDITS;
            Some(g)
        } else {
            None
        };
        self.rfcomm_frame(handle, dlci, true, UIH, give, data);
    }

    fn rfcomm_flush(&mut self, handle: u16, dlci: u8) {
        let pending = self.rfcomm(handle).and_then(|r| r.dlcs.iter_mut().find(|d| d.dlci == dlci)).map(|d| core::mem::take(&mut d.waiting)).unwrap_or_default();
        for p in pending {
            self.rfcomm_send(handle, dlci, &p);
        }
    }

    pub(super) fn rfcomm_open(&mut self, handle: u16, local: u16) {
        let c = match self.conn_mut(handle) {
            Some(c) => c,
            None => return,
        };
        let r = c.rfcomm.get_or_insert(Rfcomm { chan: None, open: false, initiator: false, mux: false, dlcs: Vec::new(), want: None });
        r.chan = Some(local);
        r.open = true;
        if r.initiator {
            self.rfcomm_frame(handle, 0, true, SABM | PF, None, &[]);
        }
    }

    pub(super) fn rfcomm_closed(&mut self, handle: u16, local: u16) {
        let Some(c) = self.conn_mut(handle) else { return };
        if c.rfcomm.as_ref().is_some_and(|r| r.chan == Some(local)) {
            let dlcis: Vec<u8> = c.rfcomm.as_ref().map(|r| r.dlcs.iter().filter(|d| d.open).map(|d| d.dlci).collect()).unwrap_or_default();
            c.rfcomm = None;
            for dlci in dlcis {
                self.port_closed(handle, dlci >> 1);
            }
        }
    }

    pub(super) fn rfcomm_data(&mut self, handle: u16, _local: u16, f: &[u8]) {
        if f.len() < 4 {
            return;
        }
        let (dlci, control) = (f[0] >> 2, f[1] & !PF);
        let poll = f[1] & PF != 0;
        let (len, mut at) = if f[2] & 1 != 0 { ((f[2] >> 1) as usize, 3) } else { ((f[2] >> 1) as usize | (f[3] as usize) << 7, 4) };
        match control {
            SABM => {
                // The device opens the session (channel 0) or a port.
                self.rfcomm_frame(handle, dlci, false, UA | PF, None, &[]);
                if dlci == 0 {
                    if let Some(r) = self.rfcomm(handle) {
                        r.mux = true;
                    }
                } else {
                    self.dlc_up(handle, dlci);
                }
            }
            UA => {
                if dlci == 0 {
                    let want = self.rfcomm(handle).map(|r| {
                        r.mux = true;
                        r.want.take()
                    });
                    if let Some(Some(ch)) = want {
                        self.dlc_open(handle, ch << 1);
                    }
                } else {
                    self.dlc_up(handle, dlci);
                }
            }
            DM | DISC => {
                if control == DISC {
                    self.rfcomm_frame(handle, dlci, false, UA | PF, None, &[]);
                }
                if dlci != 0 {
                    if let Some(r) = self.rfcomm(handle) {
                        r.dlcs.retain(|d| d.dlci != dlci);
                    }
                    if control == DM {
                        kprintln!("bt: rfcomm: channel {} refused", dlci >> 1);
                    }
                    self.port_closed(handle, dlci >> 1);
                }
            }
            UIH => {
                // Credits are in the first data byte if P/F is set.
                if poll && dlci != 0 {
                    let more = f.get(at).copied().unwrap_or(0);
                    at += 1;
                    if let Some(d) = self.rfcomm(handle).and_then(|r| r.dlcs.iter_mut().find(|d| d.dlci == dlci)) {
                        d.credits_out = d.credits_out.saturating_add(more);
                    }
                    self.rfcomm_flush(handle, dlci);
                }
                let Some(data) = f.get(at..at + len) else { return };
                if dlci == 0 {
                    self.rfcomm_mux(handle, data);
                } else if !data.is_empty() {
                    if let Some(d) = self.rfcomm(handle).and_then(|r| r.dlcs.iter_mut().find(|d| d.dlci == dlci)) {
                        d.credits_in = d.credits_in.saturating_sub(1);
                    }
                    self.port_data(handle, dlci >> 1, dlci, data);
                }
            }
            _ => {}
        }
    }

    /// The port is open on both sides: report the line state and hand it to the protocol.
    fn dlc_up(&mut self, handle: u16, dlci: u8) {
        let Some(r) = self.rfcomm(handle) else { return };
        if !r.dlcs.iter().any(|d| d.dlci == dlci) {
            r.dlcs.push(Dlc { dlci, open: false, credits_in: OUR_CREDITS, credits_out: 0, frame: OUR_FRAME, waiting: Vec::new() });
        }
        if let Some(d) = r.dlcs.iter_mut().find(|d| d.dlci == dlci) {
            d.open = true;
        }
        // The line state: ready, may send (DV, RTR, RTC).
        self.rfcomm_mcc(handle, MCC_MSC, true, &[dlci << 2 | 0x03, 0x8D]);
        self.port_open(handle, dlci >> 1, dlci);
        self.rfcomm_flush(handle, dlci);
    }

    /// Multiplexer commands from the device.
    fn rfcomm_mux(&mut self, handle: u16, m: &[u8]) {
        if m.len() < 2 {
            return;
        }
        let (kind, command) = (m[0] & 0xFC, m[0] & 0x02 != 0);
        let len = (m[1] >> 1) as usize;
        let Some(v) = m.get(2..2 + len) else { return };
        match kind {
            MCC_PN if v.len() >= 8 => {
                let dlci = v[0] & 0x3F;
                let frame = u16::from_le_bytes([v[4], v[5]]);
                let credits = v[7] & 7;
                let credit_based = if command { v[1] >> 4 == 0xF } else { v[1] >> 4 == 0xE };
                let r = match self.rfcomm(handle) {
                    Some(r) => r,
                    None => return,
                };
                if !r.dlcs.iter().any(|d| d.dlci == dlci) {
                    r.dlcs.push(Dlc { dlci, open: false, credits_in: OUR_CREDITS, credits_out: 0, frame: OUR_FRAME, waiting: Vec::new() });
                }
                if let Some(d) = r.dlcs.iter_mut().find(|d| d.dlci == dlci) {
                    d.frame = frame.min(OUR_FRAME);
                    d.credits_out = if credit_based { credits } else { u8::MAX };
                }
                if command {
                    let f = frame.min(OUR_FRAME).to_le_bytes();
                    self.rfcomm_mcc(handle, MCC_PN, false, &[dlci, if credit_based { 0xE0 } else { 0 }, v[2], 0, f[0], f[1], 0, OUR_CREDITS]);
                } else {
                    // Our request was accepted — open the port.
                    self.rfcomm_frame(handle, dlci, true, SABM | PF, None, &[]);
                }
            }
            MCC_MSC if command => {
                let mut r = vec![kind | 1, m[1]];
                r.extend_from_slice(v);
                self.rfcomm_frame(handle, 0, true, UIH, None, &r);
            }
            MCC_RPN | MCC_RLS | MCC_TEST if command => {
                // Line parameters and tests: agree, echoing the data.
                let mut r = vec![kind | 1, m[1]];
                r.extend_from_slice(v);
                self.rfcomm_frame(handle, 0, true, UIH, None, &r);
            }
            MCC_MSC | MCC_RPN | MCC_RLS | MCC_TEST | MCC_NSC => {}
            _ if command => self.rfcomm_mcc(handle, MCC_NSC, false, &[m[0]]),
            _ => {}
        }
    }
}
