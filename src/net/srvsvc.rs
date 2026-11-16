//! The srvsvc service (DCE/RPC over the IPC$ named pipe): Windows gets the share
//! list through it when just `\\computer` is opened. Implemented:
//! NetrShareEnum, NetrShareGetInfo and NetrServerGetInfo — enough for Explorer.

use alloc::string::String;
use alloc::vec::Vec;

const BIND: u8 = 11;
const BIND_ACK: u8 = 12;
const REQUEST: u8 = 0;
const RESPONSE: u8 = 2;
const FAULT: u8 = 3;
/// The NDR 2.0 transfer syntax — the only one we understand.
const NDR: [u8; 20] = [
    0x04, 0x5d, 0x88, 0x8a, 0xeb, 0x1c, 0xc9, 0x11, 0x9f, 0xe8, 0x08, 0x00, 0x2b, 0x10, 0x48, 0x60, 2, 0, 0, 0,
];
const STYPE_DISKTREE: u32 = 0;
const STYPE_IPC_SPECIAL: u32 = 0x8000_0003;
const ERROR_INVALID_LEVEL: u32 = 124;
const NERR_NET_NAME_NOT_FOUND: u32 = 2310;
/// The RPC error "no such operation".
const NCA_OP_RNG_ERROR: u32 = 0x1C01_0002;

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

/// An NDR writer: alignment and "deferred" strings after the structures.
struct Ndr {
    b: Vec<u8>,
    referent: u32,
}

impl Ndr {
    fn new() -> Self {
        Self { b: Vec::new(), referent: 0x0002_0000 }
    }

    fn u32(&mut self, v: u32) {
        while self.b.len() % 4 != 0 {
            self.b.push(0);
        }
        self.b.extend_from_slice(&v.to_le_bytes());
    }

    /// A non-null pointer (the data itself comes later).
    fn ptr(&mut self) {
        self.referent += 4;
        let r = self.referent;
        self.u32(r);
    }

    /// A zero-terminated UTF-16 string: maximum, offset, actual length, characters.
    fn string(&mut self, s: &str) {
        let units: Vec<u16> = s.encode_utf16().chain(core::iter::once(0)).collect();
        self.u32(units.len() as u32);
        self.u32(0);
        self.u32(units.len() as u32);
        for u in units {
            self.b.extend_from_slice(&u.to_le_bytes());
        }
    }
}

/// The request NDR reader.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn u32(&mut self) -> u32 {
        self.at = self.at.next_multiple_of(4);
        let v = le32(self.b, self.at);
        self.at += 4;
        v
    }

    fn string(&mut self) -> String {
        let (_max, _off, n) = (self.u32(), self.u32(), self.u32() as usize);
        let units: Vec<u16> = (0..n).map(|i| le16(self.b, self.at + 2 * i)).take_while(|&u| u != 0).collect();
        self.at += 2 * n;
        String::from_utf16_lossy(&units)
    }

    /// An optional string (a unique pointer).
    fn opt_string(&mut self) -> Option<String> {
        (self.u32() != 0).then(|| self.string())
    }
}

/// The PDU header (16 bytes); the fragment length is taken from the finished message.
fn pdu(kind: u8, call_id: u32, body: &[u8]) -> Vec<u8> {
    let mut m = alloc::vec![5, 0, kind, 0x03, 0x10, 0, 0, 0];
    m.extend_from_slice(&((16 + body.len()) as u16).to_le_bytes());
    m.extend_from_slice(&0u16.to_le_bytes());
    m.extend_from_slice(&call_id.to_le_bytes());
    m.extend_from_slice(body);
    m
}

// ---------------------------------------------------------------- the client

/// The srvsvc 3.0 interface.
const SRVSVC: [u8; 20] = [
    0xc8, 0x4f, 0x32, 0x4b, 0x70, 0x16, 0xd3, 0x01, 0x12, 0x78, 0x5a, 0x47, 0xbf, 0x6e, 0xe1, 0x88, 3, 0, 0, 0,
];
const OP_SHARE_ENUM: u16 = 15;
const STYPE_MASK: u32 = 0x0FFF_FFFF;

/// The BIND request: bind the pipe to srvsvc.
pub fn bind_request() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&4280u16.to_le_bytes());
    b.extend_from_slice(&4280u16.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&[1, 0, 0, 0, 0, 0, 1, 0]); // one context, id 0, one syntax
    b.extend_from_slice(&SRVSVC);
    b.extend_from_slice(&NDR);
    pdu(BIND, 1, &b)
}

pub fn is_bind_ack(reply: &[u8]) -> bool {
    reply.get(2) == Some(&BIND_ACK)
}

/// NetrShareEnum level 1 (names, types, comments).
pub fn enum_request(host: &str) -> Vec<u8> {
    let mut w = Ndr::new();
    w.ptr();
    w.string(&alloc::format!("\\\\{host}"));
    w.u32(1);
    w.u32(1);
    w.ptr();
    w.u32(0);
    w.u32(0);
    w.u32(0xFFFF_FFFF);
    w.ptr();
    w.u32(0);
    let mut b = Vec::new();
    b.extend_from_slice(&(w.b.len() as u32).to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&OP_SHARE_ENUM.to_le_bytes());
    b.extend_from_slice(&w.b);
    pdu(REQUEST, 2, &b)
}

/// The answer data from one or several fragments; None — the answer is not complete yet
/// (waiting for the next fragment). An error — the answer is not a RESPONSE.
pub fn reassemble(reply: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 24 <= reply.len() {
        if reply[at + 2] != RESPONSE {
            return Err("the server refused the request");
        }
        let (len, last) = (le16(reply, at + 8) as usize, reply[at + 3] & 2 != 0);
        let Some(stub) = reply.get(at + 24..at + len) else { return Ok(None) };
        out.extend_from_slice(stub);
        if last {
            return Ok(Some(out));
        }
        at += len;
    }
    Ok(None)
}

/// The names of ordinary folders from the NetrShareEnum answer (without the service C$, IPC$).
pub fn parse_enum(stub: &[u8]) -> Option<Vec<String>> {
    let mut r = Reader { b: stub, at: 0 };
    let (_level, _tag) = (r.u32(), r.u32());
    if r.u32() == 0 {
        return None;
    }
    let count = r.u32() as usize;
    if r.u32() == 0 || count > 4096 {
        return Some(Vec::new());
    }
    let _max = r.u32();
    let kinds: Vec<u32> = (0..count)
        .map(|_| {
            let (_name, kind, _remark) = (r.u32(), r.u32(), r.u32());
            kind
        })
        .collect();
    let mut out = Vec::new();
    for kind in kinds {
        let (name, _remark) = (r.string(), r.string());
        if kind & STYPE_MASK == STYPE_DISKTREE && !name.ends_with('$') {
            out.push(name);
        }
    }
    Some(out)
}

/// The answer to one PDU of the pipe (or None if there is nothing to answer).
pub fn handle(req: &[u8], shares: &[String]) -> Option<Vec<u8>> {
    if req.len() < 16 || req[0] != 5 {
        return None;
    }
    let call_id = le32(req, 12);
    match req[2] {
        BIND => Some(pdu(BIND_ACK, call_id, &bind_ack(req))),
        REQUEST => {
            let ctx = le16(req, 20);
            let opnum = le16(req, 22);
            // The "object UUID present" flag — 16 bytes before the data.
            let stub = &req[if req[3] & 0x80 != 0 { 40 } else { 24 }..];
            let Some(out) = call(opnum, stub, shares) else {
                let mut b = Vec::new();
                b.extend_from_slice(&0u32.to_le_bytes());
                b.extend_from_slice(&ctx.to_le_bytes());
                b.extend_from_slice(&[0, 0]);
                b.extend_from_slice(&NCA_OP_RNG_ERROR.to_le_bytes());
                b.extend_from_slice(&0u32.to_le_bytes());
                return Some(pdu(FAULT, call_id, &b));
            };
            let mut b = Vec::new();
            b.extend_from_slice(&(out.len() as u32).to_le_bytes());
            b.extend_from_slice(&ctx.to_le_bytes());
            b.extend_from_slice(&[0, 0]);
            b.extend_from_slice(&out);
            Some(pdu(RESPONSE, call_id, &b))
        }
        _ => None,
    }
}

/// BIND_ACK: accept contexts with NDR 2.0, reject the others (NDR64, capability
/// negotiation).
fn bind_ack(req: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&4280u16.to_le_bytes());
    b.extend_from_slice(&4280u16.to_le_bytes());
    b.extend_from_slice(&0x0001_2345u32.to_le_bytes());
    let port = b"\\PIPE\\srvsvc\0";
    b.extend_from_slice(&(port.len() as u16).to_le_bytes());
    b.extend_from_slice(port);
    while (16 + b.len()) % 4 != 0 {
        b.push(0);
    }
    let count = req.get(24).copied().unwrap_or(0) as usize;
    b.extend_from_slice(&[count as u8, 0, 0, 0]);
    let mut at = 28;
    for _ in 0..count {
        let n = req.get(at + 2).copied().unwrap_or(0) as usize;
        let syntaxes = at + 24;
        let ndr = (0..n).any(|i| req.get(syntaxes + 20 * i..syntaxes + 20 * i + 20) == Some(&NDR[..]));
        if ndr {
            b.extend_from_slice(&[0, 0, 0, 0]); // accepted
            b.extend_from_slice(&NDR);
        } else {
            b.extend_from_slice(&[2, 0, 2, 0]); // rejected: the syntax is not supported
            b.extend_from_slice(&[0; 20]);
        }
        at = syntaxes + 20 * n;
    }
    b
}

fn call(opnum: u16, stub: &[u8], shares: &[String]) -> Option<Vec<u8>> {
    let mut r = Reader { b: stub, at: 0 };
    let mut w = Ndr::new();
    // Disk shares and the service IPC$.
    let all: Vec<(&str, u32)> = shares.iter().map(|s| (s.as_str(), STYPE_DISKTREE)).chain([("IPC$", STYPE_IPC_SPECIAL)]).collect();
    let remark = |kind: u32| if kind == STYPE_DISKTREE { "" } else { "Remote IPC" };
    match opnum {
        // NetrShareEnum(ServerName, InfoStruct, PreferedMaximumLength, TotalEntries, ResumeHandle)
        15 => {
            r.opt_string();
            let level = r.u32();
            let _tag = r.u32();
            if r.u32() != 0 {
                let _entries = r.u32();
                let _buffer = r.u32();
            }
            let _max = r.u32();
            let resume = r.u32() != 0;
            w.u32(level);
            w.u32(level);
            if level != 0 && level != 1 {
                w.u32(0);
                w.u32(0);
                if resume {
                    w.ptr();
                    w.u32(0);
                } else {
                    w.u32(0);
                }
                w.u32(ERROR_INVALID_LEVEL);
                return Some(w.b);
            }
            w.ptr();
            w.u32(all.len() as u32);
            w.ptr();
            w.u32(all.len() as u32);
            for (_, kind) in &all {
                w.ptr();
                if level == 1 {
                    w.u32(*kind);
                    w.ptr();
                }
            }
            for (name, kind) in &all {
                w.string(name);
                if level == 1 {
                    w.string(remark(*kind));
                }
            }
            w.u32(all.len() as u32);
            if resume {
                w.ptr();
                w.u32(0);
            } else {
                w.u32(0);
            }
            w.u32(0);
            Some(w.b)
        }
        // NetrShareGetInfo(ServerName, NetName, Level, InfoStruct)
        16 => {
            r.opt_string();
            let name = r.string();
            let level = r.u32();
            let Some(&(name, kind)) = all.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)) else {
                w.u32(level);
                w.u32(0);
                w.u32(NERR_NET_NAME_NOT_FOUND);
                return Some(w.b);
            };
            w.u32(level);
            match level {
                0 => {
                    w.ptr();
                    w.ptr();
                    w.string(name);
                }
                1 => {
                    w.ptr();
                    w.ptr();
                    w.u32(kind);
                    w.ptr();
                    w.string(name);
                    w.string(remark(kind));
                }
                1005 => {
                    w.ptr();
                    w.u32(0); // shi1005_flags: manual caching
                }
                _ => {
                    w.u32(0);
                    w.u32(ERROR_INVALID_LEVEL);
                    return Some(w.b);
                }
            }
            w.u32(0);
            Some(w.b)
        }
        // NetrServerGetInfo(ServerName, Level, InfoStruct)
        21 => {
            r.opt_string();
            let level = r.u32();
            w.u32(level);
            if level != 100 && level != 101 {
                w.u32(0);
                w.u32(ERROR_INVALID_LEVEL);
                return Some(w.b);
            }
            w.ptr();
            w.u32(500); // PLATFORM_ID_NT
            w.ptr();
            if level == 101 {
                w.u32(10);
                w.u32(0);
                w.u32(0x0000_9003); // workstation, server, NT
                w.ptr();
            }
            let id = super::identity::get();
            w.string(&id.name);
            if level == 101 {
                w.string(&id.description);
            }
            w.u32(0);
            Some(w.b)
        }
        _ => None,
    }
}
