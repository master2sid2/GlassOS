//! Multicast DNS (224.0.0.251:5353) and LLMNR (224.0.0.252:5355): the computer name
//! without a DNS server. mDNS shows us in Linux and macOS file managers
//! (the `_smb._tcp` service) and finds them; LLMNR answers Windows name queries.

use super::discovery::{self, Found};
use super::identity::Identity;
use super::{Ip, UdpSocket, be16};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

pub const MDNS_GROUP: Ip = [224, 0, 0, 251];
pub const MDNS_PORT: u16 = 5353;
pub const LLMNR_GROUP: Ip = [224, 0, 0, 252];
pub const LLMNR_PORT: u16 = 5355;
const SERVICE: &str = "_smb._tcp.local";
const META: &str = "_services._dns-sd._udp.local";
const A: u16 = 1;
const PTR: u16 = 12;
const TXT: u16 = 16;
const SRV: u16 = 33;
const ANY: u16 = 255;
const TTL: u32 = 120;

fn name(out: &mut Vec<u8>, n: &str) {
    for label in n.split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
}

/// Reads a name (with compression): dots between the labels and the position after the name.
fn read_name(p: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut end = None;
    for _ in 0..64 {
        let len = *p.get(at)? as usize;
        if len == 0 {
            return Some((out, end.unwrap_or(at + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let low = *p.get(at + 1)? as usize;
            end.get_or_insert(at + 2);
            at = ((len & 0x3F) << 8) | low;
            continue;
        }
        if !out.is_empty() {
            out.push('.');
        }
        out.push_str(&String::from_utf8_lossy(p.get(at + 1..at + 1 + len)?));
        at += 1 + len;
    }
    None
}

fn record(out: &mut Vec<u8>, owner: &str, kind: u16, class: u16, ttl: u32, data: &[u8]) {
    name(out, owner);
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&class.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
}

fn host_name(id: &Identity) -> String {
    format!("{}.local", id.name.to_lowercase())
}

fn instance(id: &Identity) -> String {
    format!("{}.{SERVICE}", id.name)
}

/// The full announcement: the service, its address and port, the address of the name.
fn announcement(id: &Identity, me: Ip) -> Vec<u8> {
    let (host, inst) = (host_name(id), instance(id));
    let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 4, 0, 0, 0, 0];
    let mut target = Vec::new();
    name(&mut target, &inst);
    record(&mut m, SERVICE, PTR, 1, 4500, &target);
    let mut srv = vec![0, 0, 0, 0, 0x01, 0xBD]; // priority, weight, port 445
    name(&mut srv, &host);
    record(&mut m, &inst, SRV, 0x8001, TTL, &srv);
    record(&mut m, &inst, TXT, 0x8001, 4500, &[0]);
    record(&mut m, &host, A, 0x8001, TTL, &me);
    m
}

pub fn announce(sock: &UdpSocket, id: &Identity, me: Ip) {
    sock.send_to(MDNS_GROUP, MDNS_PORT, &announcement(id, me));
}

/// The query "who shares files".
pub fn query(sock: &UdpSocket) {
    let mut m = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    name(&mut m, SERVICE);
    m.extend_from_slice(&PTR.to_be_bytes());
    m.extend_from_slice(&1u16.to_be_bytes());
    sock.send_to(MDNS_GROUP, MDNS_PORT, &m);
}

/// The answer: to everyone on the network, and if the asker asked personally (the QU bit or a query not from
/// port 5353) — only to them. A multicast frame needlessly wakes the whole network, and on
/// a bridge over VMware it also loses us for a fraction of a second.
fn reply(sock: &UdpSocket, src: Ip, sport: u16, query: &[u8], personal: bool, id: &Identity, me: Ip) {
    if !personal && sport == MDNS_PORT {
        announce(sock, id, me);
        return;
    }
    let mut m = announcement(id, me);
    m[..2].copy_from_slice(&query[..2]);
    sock.send_to(src, sport, &m);
}

pub fn on_mdns(sock: &UdpSocket, src: Ip, sport: u16, p: &[u8], id: &Identity, me: Ip) {
    if p.len() < 12 {
        return;
    }
    let (questions, answers) = (be16(p, 4) as usize, be16(p, 6) as usize + be16(p, 8) as usize + be16(p, 10) as usize);
    let mut at = 12;
    let (mut asked, mut personal) = (false, false);
    let (host, inst) = (host_name(id), instance(id));
    for _ in 0..questions {
        let Some((n, next)) = read_name(p, at) else { return };
        let Some(q) = p.get(next..next + 4) else { return };
        at = next + 4;
        if [SERVICE, META, &host, &inst].iter().any(|k| k.eq_ignore_ascii_case(&n)) {
            asked = true;
            personal |= q[2] & 0x80 != 0;
        }
    }
    if be16(p, 2) & 0x8000 == 0 {
        if asked && id.visible {
            reply(sock, src, sport, p, personal, id, me);
        }
        return;
    }
    // Someone else's answer: look for the service instance and its name.
    let mut found: Option<String> = None;
    for _ in 0..answers {
        let Some((owner, next)) = read_name(p, at) else { break };
        let Some(rr) = p.get(next..next + 10) else { break };
        let (kind, len) = (be16(rr, 0), be16(rr, 8) as usize);
        if kind == PTR
            && owner.eq_ignore_ascii_case(SERVICE)
            && let Some((target, _)) = read_name(p, next + 10)
        {
            found = target.split('.').next().map(|s| s.to_string());
        }
        at = next + 10 + len;
    }
    if let Some(name) = found
        && !name.eq_ignore_ascii_case(&id.name)
    {
        discovery::found(Found { ip: src, name, workgroup: String::new(), smb: true, via: "mDNS" });
    }
}

/// LLMNR: a query for the address of our name.
pub fn on_llmnr(sock: &UdpSocket, src: Ip, sport: u16, p: &[u8], id: &Identity, me: Ip) {
    if p.len() < 12 || be16(p, 2) & 0x8000 != 0 || be16(p, 4) != 1 || !id.visible {
        return;
    }
    let Some((n, next)) = read_name(p, 12) else { return };
    let Some(q) = p.get(next..next + 4) else { return };
    let kind = be16(q, 0);
    if !n.eq_ignore_ascii_case(&id.name) {
        return;
    }
    // An IPv6 question gets an answer too: "the name is ours, there are no such addresses", so they do not wait for a timeout.
    let has_address = matches!(kind, A | ANY);
    let mut r = Vec::new();
    r.extend_from_slice(&p[..2]);
    r.extend_from_slice(&[0x80, 0, 0, 1, 0, has_address as u8, 0, 0, 0, 0]);
    r.extend_from_slice(&p[12..next + 4]);
    if has_address {
        record(&mut r, &id.name, A, 1, 30, &me);
    }
    sock.send_to(src, sport, &r);
}
