//! SMP — Bluetooth LE pairing. We are the initiator (the central). For now "Legacy"
//! (Bluetooth 4.0 — our controller; everyone must understand it): Just Works for devices
//! without a display and keyboard (mice), entering the code we show for keyboards. Secure
//! Connections (P-256, AES-CMAC) — when it is needed.
//!
//! The functions e, c1, s1 follow the Bluetooth specification (Core, volume 3, part H, 2.2); the values are
//! in SMP byte order (least significant first). Checked against the examples in the specification.

use super::Host;
use super::l2cap::CID_SMP;
use crate::bt::{Link, PAIRED, Paired, addr_text, name_of, save_paired, show};
use crate::kprintln;
use alloc::format;
use alloc::string::String;

const PAIRING_REQUEST: u8 = 0x01;
const PAIRING_RESPONSE: u8 = 0x02;
const PAIRING_CONFIRM: u8 = 0x03;
const PAIRING_RANDOM: u8 = 0x04;
const PAIRING_FAILED: u8 = 0x05;
const ENCRYPTION_INFO: u8 = 0x06;
const MASTER_ID: u8 = 0x07;
const IDENTITY_INFO: u8 = 0x08;
const IDENTITY_ADDR_INFO: u8 = 0x09;
const SECURITY_REQUEST: u8 = 0x0B;

/// Us: a display and "yes/no"; requirements: bonding (keys are stored) and MITM
/// protection (where the device can); the key is 16 bytes; from the device we want the LTK and IRK.
const IO_DISPLAY_YES_NO: u8 = 0x01;
const AUTH_BONDING_MITM: u8 = 0x05;
const KEY_SIZE: u8 = 16;
const DIST_ENC: u8 = 0x01;
const DIST_ID: u8 = 0x02;

// Failure reasons.
const CONFIRM_FAILED: u8 = 0x04;
const NOT_SUPPORTED: u8 = 0x05;
const KEY_TOO_SHORT: u8 = 0x06;

/// HCI: enable LE link encryption with a key.
const LE_START_ENCRYPTION: u16 = 0x2019;

// Which keys we still wait for from the device.
const WAIT_LTK: u8 = 0x01;
const WAIT_MASTER_ID: u8 = 0x02;
const WAIT_IRK: u8 = 0x04;
const WAIT_ID_ADDR: u8 = 0x08;

/// Pairing in progress.
pub(super) struct Smp {
    preq: [u8; 7],
    pres: [u8; 7],
    tk: [u8; 16],
    mrand: [u8; 16],
    sconfirm: [u8; 16],
    /// Encryption with the pairing key is on — now the device hands out its keys.
    encrypted: bool,
    waiting: u8,
    ltk: Option<[u8; 16]>,
    ediv: u16,
    rand: [u8; 8],
    irk: Option<[u8; 16]>,
}

/// e(k, d) — AES-128; the specification gives values most significant byte first, we do the opposite.
fn e(k: &[u8; 16], d: &[u8; 16]) -> [u8; 16] {
    let (mut kb, mut db) = (*k, *d);
    kb.reverse();
    db.reverse();
    let mut r = crate::crypto::aes128_encrypt(&kb, &db);
    r.reverse();
    r
}

fn xor16(a: &[u8; 16], b: &[u8; 16]) -> [u8; 16] {
    core::array::from_fn(|i| a[i] ^ b[i])
}

/// c1 — the pairing confirm value. p1 = pres‖preq‖rat‖iat, p2 = 0‖ia‖ra.
#[allow(clippy::too_many_arguments)]
fn c1(k: &[u8; 16], r: &[u8; 16], preq: &[u8; 7], pres: &[u8; 7], iat: u8, ia: &[u8; 6], rat: u8, ra: &[u8; 6]) -> [u8; 16] {
    let mut p1 = [0u8; 16];
    p1[0] = iat;
    p1[1] = rat;
    p1[2..9].copy_from_slice(preq);
    p1[9..16].copy_from_slice(pres);
    let mut p2 = [0u8; 16];
    p2[0..6].copy_from_slice(ra);
    p2[6..12].copy_from_slice(ia);
    e(k, &xor16(&e(k, &xor16(r, &p1)), &p2))
}

/// s1 — the pairing key (STK) from the low halves of both sides' random numbers.
fn s1(k: &[u8; 16], r1: &[u8; 16], r2: &[u8; 16]) -> [u8; 16] {
    let mut r = [0u8; 16];
    r[0..8].copy_from_slice(&r2[0..8]);
    r[8..16].copy_from_slice(&r1[0..8]);
    e(k, &r)
}

fn random16() -> [u8; 16] {
    let r = crate::crypto::random32();
    core::array::from_fn(|i| r[i])
}

/// The reason for a pairing failure — for people.
fn failure(code: u8) -> String {
    match code {
        0x01 => "the code was not typed on the device".into(),
        0x03 => "the device wants stronger pairing (Secure Connections): not supported yet".into(),
        0x04 => "pairing failed: the codes did not match".into(),
        0x05 => "the device does not support pairing".into(),
        0x06 => "the device's encryption key is too short".into(),
        0x08 => "pairing failed on the device".into(),
        0x09 => "the device asked to wait before pairing again".into(),
        0x0B => "the device does not allow pairing now".into(),
        other => format!("pairing failed, reason {other:#04x}"),
    }
}

impl Host {
    /// After an LE connection: with a paired device — encryption with its key, otherwise — pairing.
    pub(super) fn le_secure(&mut self, handle: u16, addr: [u8; 6]) {
        let bond = PAIRED.lock().iter().find(|p| p.addr == addr && p.le).map(|p| (p.key, p.ediv, p.rand));
        match bond {
            Some((ltk, ediv, rand)) => {
                kprintln!("bt: {} is paired: encrypting with its key", addr_text(&addr));
                self.le_encrypt(handle, &ltk, ediv, &rand);
            }
            None => self.smp_start(handle, addr),
        }
    }

    fn le_encrypt(&mut self, handle: u16, ltk: &[u8; 16], ediv: u16, rand: &[u8; 8]) {
        let mut p = handle.to_le_bytes().to_vec();
        p.extend_from_slice(rand);
        p.extend_from_slice(&ediv.to_le_bytes());
        p.extend_from_slice(ltk);
        self.outbox.push_back((LE_START_ENCRYPTION, p));
    }

    fn smp_start(&mut self, handle: u16, addr: [u8; 6]) {
        let preq = [PAIRING_REQUEST, IO_DISPLAY_YES_NO, 0, AUTH_BONDING_MITM, KEY_SIZE, 0, DIST_ENC | DIST_ID];
        let Some(c) = self.conn_mut(handle) else { return };
        c.smp = Some(Smp { preq, pres: [0; 7], tk: [0; 16], mrand: [0; 16], sconfirm: [0; 16], encrypted: false, waiting: 0, ltk: None, ediv: 0, rand: [0; 8], irk: None });
        self.send_l2cap(handle, CID_SMP, &preq);
        show(addr, Some(Link::Pairing(String::new())));
        kprintln!("bt: pairing with {} (LE)", addr_text(&addr));
    }

    /// Refuse pairing: tell the device, show the reason. The link stays
    /// (without encryption — many devices are fine with that).
    fn smp_fail(&mut self, handle: u16, addr: [u8; 6], code: u8, why: String) {
        self.send_l2cap(handle, CID_SMP, &[PAIRING_FAILED, code]);
        if let Some(c) = self.conn_mut(handle) {
            c.smp = None;
        }
        kprintln!("bt: pairing with {} failed: {why}", addr_text(&addr));
        show(addr, Some(Link::Failed(why)));
    }

    /// An SMP packet from the device.
    pub(super) fn smp(&mut self, handle: u16, pdu: &[u8]) {
        let (Some(&op), Some(addr)) = (pdu.first(), self.addr_of(handle)) else { return };
        let (peer_type, own) = (self.conn_mut(handle).map_or(0, |c| c.peer_type), self.own_addr);
        let Some(c) = self.conn_mut(handle) else { return };
        match (op, c.smp.as_mut()) {
            (SECURITY_REQUEST, None) => self.le_secure(handle, addr),
            (PAIRING_RESPONSE, Some(s)) if pdu.len() >= 7 => {
                s.pres.copy_from_slice(&pdu[..7]);
                let (io, auth, size, dist) = (pdu[1], pdu[3], pdu[4], pdu[6] & (DIST_ENC | DIST_ID));
                if size < 7 {
                    return self.smp_fail(handle, addr, KEY_TOO_SHORT, failure(KEY_TOO_SHORT));
                }
                s.waiting = if dist & DIST_ENC != 0 { WAIT_LTK | WAIT_MASTER_ID } else { 0 } | if dist & DIST_ID != 0 { WAIT_IRK | WAIT_ID_ADDR } else { 0 };
                // The method: for a keyboard we show a code — it is typed on it; for a device
                // with a display but no keyboard the code would have to be entered here (not supported yet);
                // for the rest (mice, headphones) — no code.
                let mitm = (AUTH_BONDING_MITM | auth) & 0x04 != 0;
                let mut tk = [0u8; 16];
                if mitm && io == 0x00 {
                    return self.smp_fail(handle, addr, NOT_SUPPORTED, "the device shows a code to type here: not supported yet".into());
                }
                if mitm && matches!(io, 0x02 | 0x04) {
                    let passkey = u32::from_le_bytes(random16()[..4].try_into().unwrap()) % 1_000_000;
                    tk[..4].copy_from_slice(&passkey.to_le_bytes());
                    show(addr, Some(Link::Pairing(format!("type {passkey:06} on the keyboard, then Enter"))));
                }
                s.tk = tk;
                s.mrand = random16();
                let confirm = c1(&s.tk, &s.mrand, &s.preq, &s.pres, 0, &own, peer_type, &addr);
                let mut out = [0u8; 17];
                out[0] = PAIRING_CONFIRM;
                out[1..].copy_from_slice(&confirm);
                self.send_l2cap(handle, CID_SMP, &out);
            }
            (PAIRING_CONFIRM, Some(s)) if pdu.len() >= 17 => {
                s.sconfirm.copy_from_slice(&pdu[1..17]);
                let mut out = [0u8; 17];
                out[0] = PAIRING_RANDOM;
                out[1..].copy_from_slice(&s.mrand);
                self.send_l2cap(handle, CID_SMP, &out);
            }
            (PAIRING_RANDOM, Some(s)) if pdu.len() >= 17 => {
                let srand: [u8; 16] = pdu[1..17].try_into().unwrap();
                if c1(&s.tk, &srand, &s.preq, &s.pres, 0, &own, peer_type, &addr) != s.sconfirm {
                    return self.smp_fail(handle, addr, CONFIRM_FAILED, failure(CONFIRM_FAILED));
                }
                // The pairing key, truncated to the agreed length — encryption is enabled with it.
                let mut stk = s1(&s.tk, &srand, &s.mrand);
                let size = s.preq[4].min(s.pres[4]) as usize;
                stk[size..].fill(0);
                self.le_encrypt(handle, &stk, 0, &[0; 8]);
            }
            (PAIRING_FAILED, _) => {
                if let Some(c) = self.conn_mut(handle) {
                    c.smp = None;
                }
                let why = failure(pdu.get(1).copied().unwrap_or(0));
                kprintln!("bt: {} refused pairing: {why}", addr_text(&addr));
                show(addr, Some(Link::Failed(why)));
            }
            // Keys from the device (already over the encrypted link).
            (ENCRYPTION_INFO, Some(s)) if pdu.len() >= 17 => {
                s.ltk = Some(pdu[1..17].try_into().unwrap());
                s.waiting &= !WAIT_LTK;
            }
            (MASTER_ID, Some(s)) if pdu.len() >= 11 => {
                s.ediv = u16::from_le_bytes([pdu[1], pdu[2]]);
                s.rand.copy_from_slice(&pdu[3..11]);
                s.waiting &= !WAIT_MASTER_ID;
            }
            (IDENTITY_INFO, Some(s)) if pdu.len() >= 17 => {
                s.irk = Some(pdu[1..17].try_into().unwrap());
                s.waiting &= !WAIT_IRK;
            }
            (IDENTITY_ADDR_INFO, Some(s)) => s.waiting &= !WAIT_ID_ADDR,
            // The device starts pairing by itself — we are the central, this does not happen.
            (PAIRING_REQUEST, _) => self.send_l2cap(handle, CID_SMP, &[PAIRING_FAILED, NOT_SUPPORTED]),
            _ => {}
        }
        self.smp_finish(handle, addr);
    }

    /// LE encryption is on. true — pairing is in progress (waiting for keys), otherwise the link is ready.
    pub(super) fn le_encrypted(&mut self, handle: u16) -> bool {
        let Some(addr) = self.addr_of(handle) else { return false };
        let Some(s) = self.conn_mut(handle).and_then(|c| c.smp.as_mut()) else { return false };
        s.encrypted = true;
        self.smp_finish(handle, addr);
        true
    }

    /// Encryption is on and all keys arrived — save the pairing.
    fn smp_finish(&mut self, handle: u16, addr: [u8; 6]) {
        let Some(c) = self.conn_mut(handle) else { return };
        if !c.smp.as_ref().is_some_and(|s| s.encrypted && s.waiting == 0) {
            return;
        }
        let s = c.smp.take().unwrap();
        let peer_type = c.peer_type;
        match s.ltk {
            Some(key) => {
                let name = name_of(&addr);
                {
                    let mut paired = PAIRED.lock();
                    paired.retain(|p| p.addr != addr);
                    paired.push(Paired { addr, le: true, key, name, ediv: s.ediv, rand: s.rand, irk: s.irk, addr_type: peer_type });
                }
                save_paired();
                kprintln!("bt: paired with {} (LE{})", addr_text(&addr), if s.irk.is_some() { ", with its address key" } else { "" });
            }
            None => kprintln!("bt: {}: encrypted, but the device gave no key to remember", addr_text(&addr)),
        }
        show(addr, Some(Link::Connected));
        self.secured(handle);
    }
}
