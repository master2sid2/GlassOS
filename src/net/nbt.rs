//! NetBIOS over TCP/IP: names (UDP 137) and browser announcements (UDP 138).
//!
//! This is how Windows and Samba find the computer by name, and the announcements show it
//! in the "Network Neighbourhood" of old Windows versions. We learn the name and
//! workgroup of neighbours the same way: a status request (NBSTAT) to their address.

use super::discovery::{self, Found};
use super::identity::Identity;
use super::{Ip, UdpSocket, be16, be32};
use crate::interrupts::{self, HZ};
use crate::task;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

pub const NAME_PORT: u16 = 137;
pub const DATAGRAM_PORT: u16 = 138;
const TYPE_NB: u16 = 0x20;
const TYPE_NBSTAT: u16 = 0x21;
const SUFFIX_WORKSTATION: u8 = 0x00;
const SUFFIX_SERVER: u8 = 0x20;
/// The name the workgroup listens to announcements on.
const SUFFIX_BROWSERS: u8 = 0x1D;
/// The name flag in an NBSTAT answer: group.
const NAME_GROUP: u16 = 0x8000;
const NAME_ACTIVE: u16 = 0x0400;

/// The name encoding: each of the 16 bytes becomes two letters from 'A'.
fn encode(name: &str, suffix: u8) -> Vec<u8> {
    let mut raw = [b' '; 16];
    if name == "*" {
        raw = [0; 16];
        raw[0] = b'*';
    }
    for (slot, b) in raw.iter_mut().zip(name.bytes().take(15)) {
        *slot = b.to_ascii_uppercase();
    }
    raw[15] = suffix;
    let mut out = vec![32];
    for b in raw {
        out.push(b'A' + (b >> 4));
        out.push(b'A' + (b & 0xF));
    }
    out.push(0);
    out
}

/// The name from a request: (the text without spaces, the suffix) and what follows it.
fn decode(b: &[u8]) -> Option<(String, u8, usize)> {
    if *b.first()? != 32 || b.len() < 34 {
        return None;
    }
    let bytes: Vec<u8> = b[1..33].chunks(2).map(|c| (c[0].wrapping_sub(b'A') << 4) | (c[1].wrapping_sub(b'A') & 0xF)).collect();
    let name = String::from_utf8_lossy(&bytes[..15]).trim_end_matches([' ', '\0']).to_string();
    Some((name, bytes[15], 34))
}

fn header(id: u16, flags: u16, questions: u16, answers: u16) -> Vec<u8> {
    let mut h = Vec::with_capacity(12);
    h.extend_from_slice(&id.to_be_bytes());
    h.extend_from_slice(&flags.to_be_bytes());
    h.extend_from_slice(&questions.to_be_bytes());
    h.extend_from_slice(&answers.to_be_bytes());
    h.extend_from_slice(&[0; 4]);
    h
}

fn question(id: u16, flags: u16, name: &[u8], kind: u16) -> Vec<u8> {
    let mut q = header(id, flags, 1, 0);
    q.extend_from_slice(name);
    q.extend_from_slice(&kind.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes());
    q
}

/// An NBSTAT request: who are you (names, workgroup).
pub fn status_query(id: u16) -> Vec<u8> {
    question(id, 0, &encode("*", 0), TYPE_NBSTAT)
}

/// A broadcast search for an address by name.
fn name_query(id: u16, name: &str) -> Vec<u8> {
    question(id, 0x0110, &encode(name, SUFFIX_WORKSTATION), TYPE_NB)
}

/// The answer to someone else's request, or None if it is not for us.
fn answer(p: &[u8], id: &Identity, me: Ip, mac: [u8; 6]) -> Option<Vec<u8>> {
    let (name, suffix, used) = decode(p.get(12..)?)?;
    if p.len() < 12 + used + 4 {
        return None;
    }
    let kind = be16(p, 12 + used);
    let tx = be16(p, 0);
    let encoded = &p[12..12 + used];
    match kind {
        TYPE_NB if name.eq_ignore_ascii_case(&id.name) && matches!(suffix, SUFFIX_WORKSTATION | SUFFIX_SERVER) => {
            let mut r = header(tx, 0x8500, 0, 1);
            r.extend_from_slice(encoded);
            r.extend_from_slice(&TYPE_NB.to_be_bytes());
            r.extend_from_slice(&1u16.to_be_bytes());
            r.extend_from_slice(&300u32.to_be_bytes());
            r.extend_from_slice(&6u16.to_be_bytes());
            r.extend_from_slice(&0u16.to_be_bytes());
            r.extend_from_slice(&me);
            Some(r)
        }
        TYPE_NBSTAT if name == "*" || name.eq_ignore_ascii_case(&id.name) => {
            let names: [(&str, u8, u16); 3] =
                [(&id.name, SUFFIX_WORKSTATION, 0), (&id.name, SUFFIX_SERVER, 0), (&id.workgroup, SUFFIX_WORKSTATION, NAME_GROUP)];
            let mut d = vec![names.len() as u8];
            for (n, suffix, flags) in names {
                let mut raw = [b' '; 15];
                for (slot, b) in raw.iter_mut().zip(n.bytes()) {
                    *slot = b;
                }
                d.extend_from_slice(&raw);
                d.push(suffix);
                d.extend_from_slice(&(flags | NAME_ACTIVE).to_be_bytes());
            }
            d.extend_from_slice(&mac);
            d.extend_from_slice(&[0; 40]); // adapter statistics
            let mut r = header(tx, 0x8400, 0, 1);
            r.extend_from_slice(encoded);
            r.extend_from_slice(&TYPE_NBSTAT.to_be_bytes());
            r.extend_from_slice(&1u16.to_be_bytes());
            r.extend_from_slice(&0u32.to_be_bytes());
            r.extend_from_slice(&(d.len() as u16).to_be_bytes());
            r.extend_from_slice(&d);
            Some(r)
        }
        _ => None,
    }
}

/// Parses an NBSTAT answer: the computer name, the workgroup, whether there is a file server.
fn parse_status(p: &[u8]) -> Option<(String, String, bool)> {
    let (_, _, used) = decode(p.get(12..)?)?;
    let at = 12 + used + 10;
    let count = *p.get(at)? as usize;
    let (mut computer, mut workgroup, mut server) = (String::new(), String::new(), false);
    for i in 0..count {
        let e = p.get(at + 1 + i * 18..at + 1 + (i + 1) * 18)?;
        let name = String::from_utf8_lossy(&e[..15]).trim_end().to_string();
        let group = be16(e, 16) & NAME_GROUP != 0;
        match (e[15], group) {
            (SUFFIX_WORKSTATION, false) if computer.is_empty() => computer = name,
            (SUFFIX_WORKSTATION, true) if workgroup.is_empty() => workgroup = name,
            (SUFFIX_SERVER, false) => server = true,
            _ => {}
        }
    }
    (!computer.is_empty()).then_some((computer, workgroup, server))
}

/// A packet on the name port: someone else's request (we answer) or the answer to our request.
pub fn on_name_packet(sock: &UdpSocket, src: Ip, sport: u16, p: &[u8], id: &Identity, me: Ip, mac: [u8; 6]) {
    if p.len() < 12 {
        return;
    }
    if be16(p, 2) & 0x8000 == 0 {
        if id.visible
            && let Some(r) = answer(p, id, me, mac)
        {
            sock.send_to(src, sport, &r);
        }
    } else {
        on_status_reply(src, p);
    }
}

/// An answer to an NBSTAT request: a neighbour named itself.
pub fn on_status_reply(src: Ip, p: &[u8]) {
    if p.len() >= 12
        && be16(p, 2) & 0x8000 != 0
        && be16(p, 6) == 1
        && let Some((name, workgroup, server)) = parse_status(p)
    {
        discovery::found(Found { ip: src, name, workgroup, smb: server, via: "NetBIOS" });
    }
}

/// A NetBIOS datagram with a letter to the browser mailslot.
fn browser_datagram(id: &Identity, me: Ip, opcode: u8, period_ms: u32) -> Vec<u8> {
    let mut data = vec![opcode, 1];
    data.extend_from_slice(&period_ms.to_le_bytes());
    let mut server = [0u8; 16];
    server[..id.name.len().min(15)].copy_from_slice(&id.name.as_bytes()[..id.name.len().min(15)]);
    data.extend_from_slice(&server);
    data.extend_from_slice(&[6, 1]); // version: as in Windows 7
    data.extend_from_slice(&0x0000_9003u32.to_le_bytes()); // workstation, server, NT
    data.extend_from_slice(&[15, 1]);
    data.extend_from_slice(&0xAA55u16.to_le_bytes());
    data.extend_from_slice(id.description.as_bytes());
    data.push(0);

    // SMB1 Transaction — a letter to \MAILSLOT\BROWSE.
    const SLOT: &[u8] = b"\\MAILSLOT\\BROWSE\0";
    let mut smb = vec![0xFF, b'S', b'M', b'B', 0x25];
    smb.extend_from_slice(&[0; 27]);
    let data_offset = (32 + 1 + 17 * 2 + 2 + SLOT.len()) as u16;
    // No parameters, the data is the letter; three Setup words: mailslot write, priority, class.
    let words: [u16; 17] = [0, data.len() as u16, 0, 0, 0, 0, 0, 0, 0, 0, data_offset, data.len() as u16, data_offset, 3, 1, 1, 2];
    smb.push(17);
    for w in words {
        smb.extend_from_slice(&w.to_le_bytes());
    }
    smb.extend_from_slice(&((SLOT.len() + data.len()) as u16).to_le_bytes());
    smb.extend_from_slice(SLOT);
    smb.extend_from_slice(&data);

    let (source, dest) = (encode(&id.name, SUFFIX_WORKSTATION), encode(&id.workgroup, SUFFIX_BROWSERS));
    let mut d = vec![0x11, 0x02];
    d.extend_from_slice(&((crate::arch::rdtsc() as u16) | 1).to_be_bytes());
    d.extend_from_slice(&me);
    d.extend_from_slice(&DATAGRAM_PORT.to_be_bytes());
    d.extend_from_slice(&((source.len() + dest.len() + smb.len()) as u16).to_be_bytes());
    d.extend_from_slice(&0u16.to_be_bytes());
    d.extend_from_slice(&source);
    d.extend_from_slice(&dest);
    d.extend_from_slice(&smb);
    d
}

/// The "I am here" announcement for the workgroup's browsers.
pub fn announce(sock: &UdpSocket, id: &Identity, me: Ip, broadcast: Ip, period_ms: u32) {
    sock.send_to(broadcast, DATAGRAM_PORT, &browser_datagram(id, me, 1, period_ms));
}

/// Someone else's announcement: a new computer in the list.
pub fn on_datagram(src: Ip, p: &[u8]) {
    if p.len() < 82 || !matches!(p[0], 0x10 | 0x11) {
        return;
    }
    let Some((_, _, used)) = decode(&p[14..]) else { return };
    let Some((workgroup, _, _)) = decode(&p[14 + used..]) else { return };
    const SLOT: &[u8] = b"\\MAILSLOT\\BROWSE\0";
    let Some(at) = p.windows(SLOT.len()).position(|w| w == SLOT) else { return };
    let d = &p[at + SLOT.len()..];
    // HostAnnouncement and LocalMasterAnnouncement are laid out the same way.
    if d.len() < 30 || !matches!(d[0], 0x01 | 0x0F) {
        return;
    }
    let raw = &d[6..22];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let name = String::from_utf8_lossy(&raw[..end]).to_string();
    let kind = u32::from_le_bytes(d[24..28].try_into().unwrap());
    if !name.is_empty() {
        discovery::found(Found { ip: src, name, workgroup, smb: kind & 2 != 0, via: "NetBIOS" });
    }
}

/// A computer's address by name: a broadcast request (one and a half seconds).
pub fn lookup(name: &str) -> Option<Ip> {
    let (_, _, broadcast) = super::subnet()?;
    let sock = UdpSocket::bind(0)?;
    let tx = (crate::arch::rdtsc() as u16) | 1;
    let query = name_query(tx, name);
    let until = interrupts::ticks() + HZ * 3 / 2;
    let mut next = 0;
    loop {
        let now = interrupts::ticks();
        if now >= until {
            return None;
        }
        if now >= next {
            sock.send_to(broadcast, NAME_PORT, &query);
            next = now + HZ / 2;
        }
        while let Some((_, _, p)) = sock.try_recv() {
            // The address is at the end of the answer record.
            if p.len() >= 62 && be16(&p, 0) == tx && be16(&p, 2) & 0x8000 != 0 && be16(&p, 6) > 0 {
                let ip = be32(&p, 58).to_be_bytes();
                return Some(ip);
            }
        }
        task::sleep_until(now + 1);
    }
}
