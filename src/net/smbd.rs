//! SMB2 server (dialects 2.0.2 and 2.1): GlassOS folders over the network for Windows,
//! Linux (cifs, smbclient) and macOS.
//!
//! Shares are in /system/shares.cfg: lines `name=/path`, plus `password=...`
//! (log in as a user with this password; without a password — guest login). Each
//! connection is served by its own thread. An open file is kept entirely in memory:
//! reads and writes go to a buffer, and on close a modified file is written to the FS.

use super::{TcpListener, TcpStream};
use crate::crypto::{hmac_md5, hmac_sha256, md4, rc4};
use crate::fs;
use crate::interrupts::HZ;
use crate::kprintln;
use crate::task;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

pub const CONFIG: &str = "/system/shares.cfg";
pub const PORT: u16 = 445;

const STATUS_OK: u32 = 0;
const STATUS_MORE_PROCESSING: u32 = 0xC000_0016;
const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;
const STATUS_END_OF_FILE: u32 = 0xC000_0011;
const STATUS_NOT_FOUND: u32 = 0xC000_0034;
const STATUS_PATH_NOT_FOUND: u32 = 0xC000_003A;
const STATUS_COLLISION: u32 = 0xC000_0035;
const STATUS_NOT_SUPPORTED: u32 = 0xC000_00BB;
/// How a server without DFS answers a DFS referral request.
const STATUS_FS_DRIVER_REQUIRED: u32 = 0xC000_019C;
const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;
const STATUS_LOGON_FAILURE: u32 = 0xC000_006D;
const STATUS_BAD_NETWORK_NAME: u32 = 0xC000_00CC;
const STATUS_FILE_CLOSED: u32 = 0xC000_0128;
const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
const STATUS_NOT_A_DIRECTORY: u32 = 0xC000_0103;
const STATUS_FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
const STATUS_DISK_FULL: u32 = 0xC000_007F;
const STATUS_FILE_TOO_LARGE: u32 = 0xC000_0904;
const STATUS_USER_SESSION_DELETED: u32 = 0xC000_0203;
const STATUS_NETWORK_NAME_DELETED: u32 = 0xC000_00C9;
const STATUS_INFO_LENGTH_MISMATCH: u32 = 0xC000_0004;
const STATUS_BUFFER_OVERFLOW: u32 = 0x8000_0005;
const STATUS_BUFFER_TOO_SMALL: u32 = 0xC000_0023;
const STATUS_PIPE_EMPTY: u32 = 0xC000_00D9;
/// IOCTL: a request and answer over a named pipe in one go.
const FSCTL_PIPE_TRANSCEIVE: u32 = 0x0011_C017;
const FSCTL_DFS_GET_REFERRALS: u32 = 0x0006_0194;
const FSCTL_DFS_GET_REFERRALS_EX: u32 = 0x0006_01B0;

const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_ARCHIVE: u32 = 0x20;

static RUNNING: AtomicBool = AtomicBool::new(false);

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}
fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}
fn le64(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
}
fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}
fn from_utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

/// Time in Windows format (hundreds of nanoseconds since 1601).
fn filetime() -> u64 {
    ((crate::time::unix().max(0) as u64) + 11_644_473_600) * 10_000_000
}

fn fs_status(e: fs::FsError) -> u32 {
    match e {
        fs::FsError::NotFound => STATUS_NOT_FOUND,
        fs::FsError::AlreadyExists => STATUS_COLLISION,
        fs::FsError::NotADirectory => STATUS_NOT_A_DIRECTORY,
        fs::FsError::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
        fs::FsError::NoSpace => STATUS_DISK_FULL,
        fs::FsError::InvalidName => STATUS_INVALID_PARAMETER,
        fs::FsError::TooLarge => STATUS_DISK_FULL,
        fs::FsError::FileTooBig => STATUS_FILE_TOO_LARGE,
        fs::FsError::Io | fs::FsError::ReadOnly | fs::FsError::Denied => STATUS_ACCESS_DENIED,
    }
}

// ---------------------------------------------------------------- configuration

pub struct Config {
    pub shares: Vec<(String, String)>,
    pub password: String,
}

pub fn config() -> Config {
    let text = task::as_system(|| fs::read(CONFIG)).map(|d| String::from_utf8_lossy(&d).into_owned()).unwrap_or_default();
    let mut c = Config { shares: Vec::new(), password: String::new() };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k == "password" {
            c.password = v.into();
        } else if !k.is_empty() && v.starts_with('/') {
            c.shares.push((k.into(), v.into()));
        }
    }
    c
}

/// Shares expose files to the whole network, so only an administrator may change them.
pub fn save(c: &Config) -> bool {
    if !crate::access::is_admin() {
        return false;
    }
    let mut text = String::new();
    if !c.password.is_empty() {
        text.push_str(&format!("password={}\n", c.password));
    }
    for (name, path) in &c.shares {
        text.push_str(&format!("{name}={path}\n"));
    }
    task::as_system(|| {
        let _ = fs::mkdir("/system");
        fs::write(CONFIG, text.as_bytes()).is_ok()
    })
}

/// Starts the server (if there is something to share and it is not running yet).
pub fn start() {
    // A computer visible on the network needs the server even without shares: otherwise Explorer
    // shows it but says "path not found" instead of an empty folder.
    if (config().shares.is_empty() && !super::identity::get().visible) || RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    task::spawn("smbd", None, || {
        let listener = loop {
            match TcpListener::bind(PORT) {
                Ok(l) => break l,
                Err(_) => task::sleep_until(crate::interrupts::ticks() + HZ),
            }
        };
        kprintln!("smbd: listening on port {PORT}");
        loop {
            if let Some(stream) = listener.accept(HZ) {
                task::spawn("smbd-conn", None, move || serve(stream));
            }
        }
    });
}

// ---------------------------------------------------------------- SPNEGO

const OID_SPNEGO: &[u8] = &[0x06, 0x06, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x02];
const OID_NTLMSSP: &[u8] = &[0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x0a];

/// A DER element: tag, length, contents.
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = content.len();
    if n < 0x80 {
        out.push(n as u8);
    } else if n < 0x100 {
        out.extend_from_slice(&[0x81, n as u8]);
    } else {
        out.extend_from_slice(&[0x82, (n >> 8) as u8, n as u8]);
    }
    out.extend_from_slice(content);
    out
}

/// negTokenInit with the single NTLMSSP mechanism (in the reply to NEGOTIATE).
fn spnego_init() -> Vec<u8> {
    let mech_list = der(0x30, OID_NTLMSSP);
    let init = der(0x30, &der(0xa0, &mech_list));
    let mut inner = OID_SPNEGO.to_vec();
    inner.extend_from_slice(&der(0xa0, &init));
    der(0x60, &inner)
}

/// negTokenResp: state (0 — accepted, 1 — continue) and the NTLMSSP token.
fn spnego_resp(state: u8, token: Option<&[u8]>) -> Vec<u8> {
    let mut seq = der(0xa0, &der(0x0a, &[state]));
    if token.is_some() {
        seq.extend_from_slice(&der(0xa1, OID_NTLMSSP));
    }
    if let Some(t) = token {
        seq.extend_from_slice(&der(0xa2, &der(0x04, t)));
    }
    der(0xa1, &der(0x30, &seq))
}

// ---------------------------------------------------------------- connection

struct Open {
    path: String,
    is_dir: bool,
    /// Length for the client: Windows announces it in advance, before writing.
    size: u64,
    /// How many bytes of the file are already on the volume.
    stored: u64,
    /// The file was changed: on close, tell open folders.
    changed: bool,
    delete_on_close: bool,
    /// A directory listing and how many entries were already returned.
    listing: Option<(Vec<fs::Entry>, usize)>,
    /// The srvsvc named pipe (in IPC$): an answer not yet read.
    pipe: Option<Vec<u8>>,
}

struct Conn {
    stream: TcpStream,
    inbox: Vec<u8>,
    dialect: u16,
    challenge: [u8; 8],
    session: u64,
    authenticated: bool,
    spnego: bool,
    signing_key: Option<[u8; 16]>,
    trees: BTreeMap<u32, Option<String>>,
    next_tree: u32,
    opens: BTreeMap<u64, Open>,
    next_file: u64,
    config: Config,
    /// Tree id from the last TREE_CONNECT — for the reply header.
    pending_tree: Option<u32>,
}

/// Open connections to our server (for metrics).
pub static CLIENTS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

struct Counted;

impl Drop for Counted {
    fn drop(&mut self) {
        CLIENTS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn serve(stream: TcpStream) {
    CLIENTS.fetch_add(1, Ordering::Relaxed);
    let _counted = Counted;
    let mut c = Conn {
        stream,
        inbox: Vec::new(),
        dialect: 0x0210,
        challenge: (crate::arch::rdtsc() ^ 0x9E37_79B9_7F4A_7C15).to_le_bytes(),
        session: 0,
        authenticated: false,
        spnego: false,
        signing_key: None,
        trees: BTreeMap::new(),
        next_tree: 1,
        opens: BTreeMap::new(),
        next_file: 1,
        config: config(),
        pending_tree: None,
    };
    while let Some(msg) = c.receive() {
        let reply = if msg.starts_with(b"\xFFSMB") {
            c.smb1_negotiate(&msg)
        } else if msg.starts_with(b"\xFESMB") {
            c.handle(&msg)
        } else {
            break;
        };
        let mut frame = (reply.len() as u32).to_be_bytes().to_vec();
        frame[0] = 0;
        frame.extend_from_slice(&reply);
        if c.stream.send(&frame).is_err() {
            break;
        }
    }
    // The connection closed: save files not yet written out.
    let ids: Vec<u64> = c.opens.keys().copied().collect();
    for id in ids {
        c.close(id);
    }
}

impl Conn {
    fn receive(&mut self) -> Option<Vec<u8>> {
        loop {
            if self.inbox.len() >= 4 {
                let len = u32::from_be_bytes([0, self.inbox[1], self.inbox[2], self.inbox[3]]) as usize;
                if self.inbox.len() >= 4 + len {
                    let msg = self.inbox[4..4 + len].to_vec();
                    self.inbox.drain(..4 + len);
                    return Some(msg);
                }
            }
            match self.stream.recv(600 * HZ) {
                Ok(chunk) if !chunk.is_empty() => self.inbox.extend_from_slice(&chunk),
                _ => return None,
            }
        }
    }

    /// An old client starts with SMB1 NEGOTIATE — answer in SMB2 (2.0.2 or "2.???").
    fn smb1_negotiate(&mut self, msg: &[u8]) -> Vec<u8> {
        let wildcard = msg.windows(9).any(|w| w == b"SMB 2.???");
        self.dialect = if wildcard { 0x02FF } else { 0x0202 };
        let mut hdr = vec![0u8; 64];
        self.header(&mut hdr, 0, 0, STATUS_OK, 0, 1);
        let body = self.negotiate_body();
        hdr.extend_from_slice(&body);
        hdr
    }

    fn negotiate_body(&self) -> Vec<u8> {
        let token = spnego_init();
        let mut b = Vec::new();
        put16(&mut b, 65);
        // With a password signing is mandatory (modern Windows expects it too); not for guests.
        put16(&mut b, if self.config.password.is_empty() { 1 } else { 3 });
        put16(&mut b, self.dialect);
        put16(&mut b, 0);
        b.extend_from_slice(b"GlassOS-SMB-srv!"); // ServerGuid
        put32(&mut b, 0); // capabilities
        put32(&mut b, 65536);
        put32(&mut b, 65536);
        put32(&mut b, 65536);
        put64(&mut b, filetime());
        put64(&mut b, 0);
        put16(&mut b, 128);
        put16(&mut b, token.len() as u16);
        put32(&mut b, 0);
        b.extend_from_slice(&token);
        b
    }

    /// A reply header in place of `h` (64 bytes).
    fn header(&self, h: &mut [u8], command: u16, message_id: u64, status: u32, tree: u32, credits: u16) {
        h[0..4].copy_from_slice(b"\xFESMB");
        h[4..6].copy_from_slice(&64u16.to_le_bytes());
        h[8..12].copy_from_slice(&status.to_le_bytes());
        h[12..14].copy_from_slice(&command.to_le_bytes());
        // Credits (how many requests the client may send without waiting for answers): with a margin.
        h[14..16].copy_from_slice(&credits.clamp(32, 128).to_le_bytes());
        h[16..20].copy_from_slice(&1u32.to_le_bytes()); // response
        h[24..32].copy_from_slice(&message_id.to_le_bytes());
        h[32..36].copy_from_slice(&0xFEFFu32.to_le_bytes());
        h[36..40].copy_from_slice(&tree.to_le_bytes());
        h[40..48].copy_from_slice(&self.session.to_le_bytes());
    }

    /// An SMB2 packet — possibly a chain of several requests (compound).
    fn handle(&mut self, msg: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 0;
        let mut last_file: Option<[u8; 16]> = None;
        let mut prev_start = None;
        loop {
            let req = &msg[at..];
            if req.len() < 64 {
                break;
            }
            let next = le32(req, 20) as usize;
            let this = if next > 0 { &req[..next.min(req.len())] } else { req };
            let related = le32(this, 16) & 0x04 != 0;
            let (status, body) = self.dispatch(this, related, &mut last_file);
            let command = le16(this, 12);
            let credits = le16(this, 14);
            let mut resp = vec![0u8; 64];
            let tree = if command == 3 { self.pending_tree.take().unwrap_or(0) } else { le32(this, 36) };
            self.header(&mut resp, command, le64(this, 24), status, tree, credits);
            resp[6..8].copy_from_slice(&le16(this, 6).to_le_bytes()); // CreditCharge — as in the request
            if related {
                resp[16..20].copy_from_slice(&(1u32 | 4).to_le_bytes());
            }
            if body.is_empty() {
                // An error: the body is 9 bytes by the standard.
                resp.extend_from_slice(&[9, 0, 0, 0, 0, 0, 0, 0, 0]);
            } else {
                resp.extend_from_slice(&body);
            }
            // Signing: if the session signs and the request is signed.
            // The last reply of a login is always signed (the key has just appeared):
            // without it Windows drops the connection.
            let signed = le32(this, 16) & 0x08 != 0 || (command == 1 && status == STATUS_OK);
            if next > 0 {
                while resp.len() % 8 != 0 {
                    resp.push(0);
                }
            }
            if let Some(key) = self.signing_key
                && signed
            {
                let flags = le32(&resp, 16) | 0x08;
                resp[16..20].copy_from_slice(&flags.to_le_bytes());
                let sig = hmac_sha256(&key, &resp);
                resp[48..64].copy_from_slice(&sig[..16]);
            }
            if let Some(p) = prev_start {
                let off = (out.len() - p) as u32;
                let o: &mut Vec<u8> = &mut out;
                o[p + 20..p + 24].copy_from_slice(&off.to_le_bytes());
                // The previous reply's signature covers NextCommand — recompute it.
                if let Some(key) = self.signing_key
                    && le32(&o[p..], 16) & 0x08 != 0
                {
                    let len = off as usize;
                    o[p + 48..p + 64].fill(0);
                    let sig = hmac_sha256(&key, &o[p..p + len]);
                    o[p + 48..p + 64].copy_from_slice(&sig[..16]);
                }
            }
            prev_start = Some(out.len());
            out.extend_from_slice(&resp);
            if next == 0 {
                break;
            }
            at += next;
        }
        out
    }

    fn dispatch(&mut self, req: &[u8], related: bool, last: &mut Option<[u8; 16]>) -> (u32, Vec<u8>) {
        let command = le16(req, 12);
        let body = &req[64..];
        if command > 1 && !self.authenticated {
            return (STATUS_USER_SESSION_DELETED, Vec::new());
        }
        let tree = le32(req, 36);
        // FileId: in a related chain 0xFF… means "from the previous request".
        let file_at = match command {
            5 => None,
            6 => Some(8),
            8 => Some(16),
            9 => Some(16),
            0x0E => Some(8),
            0x10 => Some(24),
            0x11 => Some(16),
            0x0B => Some(8),
            _ => None,
        };
        let mut file_id = file_at.and_then(|o| body.get(o..o + 16)).map(|s| <[u8; 16]>::try_from(s).unwrap());
        if related && file_id.is_some_and(|f| f == [0xFF; 16]) {
            file_id = *last;
        }
        let fid = file_id.map(|f| u64::from_le_bytes(f[0..8].try_into().unwrap()));
        let r = match command {
            0 => {
                // Pick the best offered one: 2.1, otherwise 2.0.2.
                let count = le16(body, 2) as usize;
                let dialects: Vec<u16> = (0..count).map(|i| le16(body, 36 + 2 * i)).collect();
                self.dialect = if dialects.contains(&0x0210) { 0x0210 } else { 0x0202 };
                if !dialects.contains(&0x0210) && !dialects.contains(&0x0202) {
                    return (STATUS_NOT_SUPPORTED, Vec::new());
                }
                (STATUS_OK, self.negotiate_body())
            }
            1 => self.session_setup(body),
            2 => {
                self.authenticated = false;
                (STATUS_OK, vec![4, 0, 0, 0])
            }
            3 => self.tree_connect(body),
            4 => {
                self.trees.remove(&tree);
                (STATUS_OK, vec![4, 0, 0, 0])
            }
            5 => {
                let r = self.create(tree, body);
                if r.0 == STATUS_OK {
                    *last = Some(r.1[64..80].try_into().unwrap());
                }
                r
            }
            6 => match fid {
                Some(id) => {
                    let (st, attrs) = self.close(id);
                    let mut b = vec![0u8; 60];
                    b[0] = 60;
                    if le16(body, 2) & 1 != 0
                        && let Some((size, dir)) = attrs
                    {
                        let t = filetime();
                        for at in [8, 16, 24, 32] {
                            b[at..at + 8].copy_from_slice(&t.to_le_bytes());
                        }
                        b[40..48].copy_from_slice(&size.to_le_bytes());
                        b[48..56].copy_from_slice(&size.to_le_bytes());
                        b[56..60].copy_from_slice(&(if dir { ATTR_DIRECTORY } else { ATTR_ARCHIVE }).to_le_bytes());
                    }
                    (st, if st == STATUS_OK { b } else { Vec::new() })
                }
                None => (STATUS_FILE_CLOSED, Vec::new()),
            },
            7 | 0x0A => (STATUS_OK, vec![4, 0, 0, 0]), // FLUSH, LOCK
            8 => self.read(fid, body),
            9 => self.write(fid, body),
            0x0B => self.ioctl(fid, body),
            0x0D => (STATUS_OK, vec![4, 0, 0, 0]), // ECHO
            0x0E => self.query_directory(fid, body),
            0x10 => self.query_info(fid, tree, body),
            0x11 => self.set_info(fid, tree, body),
            _ => (STATUS_NOT_SUPPORTED, Vec::new()),
        };
        if r.0 == STATUS_NOT_SUPPORTED {
            let what = match command {
                0x0B => le32(body, 4),
                0x0E => body[2] as u32,
                0x10 | 0x11 => (body[2] as u32) << 8 | body[3] as u32,
                _ => 0,
            };
            kprintln!("smbd: unsupported command {command:#x} ({what:#x})");
        }
        r
    }

    fn ioctl(&mut self, fid: Option<u64>, body: &[u8]) -> (u32, Vec<u8>) {
        let code = le32(body, 4);
        match code {
            // We have no DFS.
            FSCTL_DFS_GET_REFERRALS | FSCTL_DFS_GET_REFERRALS_EX => (STATUS_FS_DRIVER_REQUIRED, Vec::new()),
            FSCTL_PIPE_TRANSCEIVE => {
                let (off, len) = (le32(body, 24) as usize, le32(body, 28) as usize);
                let input = off.checked_sub(64).and_then(|o| body.get(o..o + len)).unwrap_or(&[]).to_vec();
                let shares = self.share_names();
                let Some(o) = fid.and_then(|id| self.opens.get_mut(&id)).filter(|o| o.pipe.is_some()) else {
                    return (STATUS_INVALID_PARAMETER, Vec::new());
                };
                let out = crate::net::srvsvc::handle(&input, &shares).unwrap_or_default();
                o.pipe = Some(Vec::new());
                let mut b = Vec::new();
                put16(&mut b, 49);
                put16(&mut b, 0);
                put32(&mut b, code);
                b.extend_from_slice(&body[8..24]);
                put32(&mut b, 112);
                put32(&mut b, 0);
                put32(&mut b, 112);
                put32(&mut b, out.len() as u32);
                put32(&mut b, 0);
                put32(&mut b, 0);
                b.extend_from_slice(&out);
                (STATUS_OK, b)
            }
            _ => (STATUS_NOT_SUPPORTED, Vec::new()),
        }
    }

    fn share_names(&self) -> Vec<String> {
        self.config.shares.iter().map(|(n, _)| n.clone()).collect()
    }

    // ------------------------------------------------------------ login

    fn session_setup(&mut self, body: &[u8]) -> (u32, Vec<u8>) {
        let (off, len) = (le16(body, 12) as usize, le16(body, 14) as usize);
        let Some(blob) = off.checked_sub(64).and_then(|o| body.get(o..o + len)) else {
            return (STATUS_INVALID_PARAMETER, Vec::new());
        };
        self.spnego = blob.first().is_some_and(|&b| b == 0x60 || b == 0xa1);
        let Some(start) = blob.windows(8).position(|w| w == b"NTLMSSP\0") else {
            return (STATUS_LOGON_FAILURE, Vec::new());
        };
        let ntlm = &blob[start..];
        if self.session == 0 {
            self.session = crate::arch::rdtsc() | 1;
        }
        let (status, token) = match le32(ntlm, 8) {
            1 => (STATUS_MORE_PROCESSING, self.challenge_message(le32(ntlm, 12))),
            3 => match self.verify(ntlm) {
                Some(()) => (STATUS_OK, Vec::new()),
                None => return (STATUS_LOGON_FAILURE, Vec::new()),
            },
            _ => return (STATUS_LOGON_FAILURE, Vec::new()),
        };
        let blob = if self.spnego {
            if status == STATUS_OK { spnego_resp(0, None) } else { spnego_resp(1, Some(&token)) }
        } else {
            token
        };
        let mut b = Vec::new();
        put16(&mut b, 9);
        put16(&mut b, if status == STATUS_OK && self.config.password.is_empty() { 1 } else { 0 }); // guest
        put16(&mut b, 72);
        put16(&mut b, blob.len() as u16);
        b.extend_from_slice(&blob);
        (status, b)
    }

    fn challenge_message(&self, client_flags: u32) -> Vec<u8> {
        let name = utf16(&super::identity::get().name);
        // Server information (AV pairs): NetBIOS computer and domain names, time.
        let mut info = Vec::new();
        for id in [2u16, 1, 4, 3] {
            put16(&mut info, id);
            put16(&mut info, name.len() as u16);
            info.extend_from_slice(&name);
        }
        put16(&mut info, 7);
        put16(&mut info, 8);
        put64(&mut info, filetime());
        put32(&mut info, 0); // end of list
        let flags = 0x0000_0001 | 0x0000_0004 | 0x0000_0200 | 0x0001_0000 | 0x0080_0000 | 0x0200_0000
            | (client_flags & (0x0000_0010 | 0x0000_8000 | 0x0008_0000 | 0x2000_0000 | 0x4000_0000 | 0x8000_0000));
        let mut m = b"NTLMSSP\0".to_vec();
        put32(&mut m, 2);
        let name_at = 56u32;
        put16(&mut m, name.len() as u16);
        put16(&mut m, name.len() as u16);
        put32(&mut m, name_at);
        put32(&mut m, flags);
        m.extend_from_slice(&self.challenge);
        m.extend_from_slice(&[0; 8]);
        put16(&mut m, info.len() as u16);
        put16(&mut m, info.len() as u16);
        put32(&mut m, name_at + name.len() as u32);
        m.extend_from_slice(&[10, 0, 0x63, 0x45, 0, 0, 0, 15]); // version (Windows 10)
        m.extend_from_slice(&name);
        m.extend_from_slice(&info);
        m
    }

    /// Checks AUTHENTICATE: NTLMv2 with the password from the settings, or a guest.
    fn verify(&mut self, m: &[u8]) -> Option<()> {
        let field = |at: usize| -> &[u8] {
            let (len, off) = (le16(m, at) as usize, le32(m, at + 4) as usize);
            m.get(off..off + len).unwrap_or(&[])
        };
        let (nt, domain, user, enc_key) = (field(20), from_utf16(field(28)), from_utf16(field(36)), field(52).to_vec());
        let flags = le32(m, 60);
        let password = self.config.password.clone();
        if password.is_empty() {
            // Guest access: let anyone in (guests have no signing).
            self.authenticated = true;
            kprintln!("smbd: guest session ({user})");
            return Some(());
        }
        if nt.len() < 24 {
            kprintln!("smbd: login without password refused ({user})");
            return None;
        }
        let nt_hash = md4(&utf16(&password));
        let mut id = utf16(&user.to_uppercase());
        id.extend_from_slice(&utf16(&domain));
        let ntowf = hmac_md5(&nt_hash, &id);
        let (proof, blob) = nt.split_at(16);
        let mut msg = self.challenge.to_vec();
        msg.extend_from_slice(blob);
        if hmac_md5(&ntowf, &msg) != proof {
            kprintln!("smbd: wrong password for {domain}\\{user}");
            return None;
        }
        let base = hmac_md5(&ntowf, proof);
        // KEY_EXCH: the real session key is sent encrypted with RC4.
        let key: [u8; 16] = if flags & 0x4000_0000 != 0 && enc_key.len() == 16 {
            rc4(&base, &enc_key).try_into().unwrap()
        } else {
            base
        };
        self.signing_key = Some(key);
        self.authenticated = true;
        kprintln!("smbd: {domain}\\{user} logged in");
        Some(())
    }

    // ------------------------------------------------------------ shares and files

    fn tree_connect(&mut self, body: &[u8]) -> (u32, Vec<u8>) {
        let (off, len) = (le16(body, 4) as usize, le16(body, 6) as usize);
        let path = from_utf16(off.checked_sub(64).and_then(|o| body.get(o..o + len)).unwrap_or(&[]));
        let share = String::from(path.rsplit('\\').next().unwrap_or(""));
        let root = if share.eq_ignore_ascii_case("IPC$") {
            None
        } else {
            match self.config.shares.iter().find(|(n, _)| n.eq_ignore_ascii_case(&share)) {
                Some((_, p)) => Some(p.clone()),
                None => return (STATUS_BAD_NETWORK_NAME, Vec::new()),
            }
        };
        let id = self.next_tree;
        self.next_tree += 1;
        let pipe = root.is_none();
        self.trees.insert(id, root);
        let mut b = Vec::new();
        put16(&mut b, 16);
        b.push(if pipe { 2 } else { 1 });
        b.push(0);
        put32(&mut b, 0x0000_0030); // ShareFlags: no client-side caching
        put32(&mut b, 0);
        put32(&mut b, 0x001F_01FF); // full access
        // The tree id goes into the reply header (passed through the "tail": see handle).
        self.pending_tree = Some(id);
        (STATUS_OK, b)
    }

    fn root_of(&self, tree: u32) -> Result<String, u32> {
        match self.trees.get(&tree) {
            Some(Some(root)) => Ok(root.clone()),
            Some(None) => Err(STATUS_NOT_FOUND),
            None => Err(STATUS_NETWORK_NAME_DELETED),
        }
    }

    fn local_path(root: &str, name: &str) -> String {
        let rel = name.replace('\\', "/");
        let rel = rel.trim_matches('/');
        if rel.is_empty() { root.into() } else { fs::resolve(root, rel) }
    }

    fn create(&mut self, tree: u32, body: &[u8]) -> (u32, Vec<u8>) {
        let disposition = le32(body, 36);
        let options = le32(body, 40);
        let (off, len) = (le16(body, 44) as usize, le16(body, 46) as usize);
        let name = from_utf16(off.checked_sub(64).and_then(|o| body.get(o..o + len)).unwrap_or(&[]));
        // IPC$: the only pipe is srvsvc (the share list).
        if self.trees.get(&tree) == Some(&None) {
            if !name.trim_start_matches('\\').eq_ignore_ascii_case("srvsvc") {
                return (STATUS_NOT_FOUND, Vec::new());
            }
            let id = self.next_file;
            self.next_file += 1;
            self.opens.insert(
                id,
                Open { path: String::new(), is_dir: false, size: 0, stored: 0, changed: false, delete_on_close: false, listing: None, pipe: Some(Vec::new()) },
            );
            return (STATUS_OK, Self::create_reply(id, 1, 0, false));
        }
        let root = match self.root_of(tree) {
            Ok(r) => r,
            Err(st) => return (st, Vec::new()),
        };
        let path = Self::local_path(&root, &name);
        if !path.starts_with(root.trim_end_matches('/')) || crate::access::is_secret(&path) {
            return (STATUS_ACCESS_DENIED, Vec::new());
        }
        let want_dir = options & 0x01 != 0;
        let exists = fs::is_dir(&path) || fs::exists(&path);
        let is_dir = fs::is_dir(&path);
        let mut action = 1u32; // opened
        match disposition {
            1 if !exists => return (if fs::is_dir(&fs::parent(&path)) { STATUS_NOT_FOUND } else { STATUS_PATH_NOT_FOUND }, Vec::new()),
            2 if exists => return (STATUS_COLLISION, Vec::new()),
            4 if !exists => return (STATUS_NOT_FOUND, Vec::new()),
            _ => {}
        }
        if exists && want_dir && !is_dir {
            return (STATUS_NOT_A_DIRECTORY, Vec::new());
        }
        if exists && options & 0x40 != 0 && is_dir {
            return (STATUS_FILE_IS_A_DIRECTORY, Vec::new());
        }
        let mut size = 0;
        if !exists {
            let r = if want_dir { fs::mkdir(&path) } else { fs::write(&path, b"") };
            if let Err(e) = r {
                return (fs_status(e), Vec::new());
            }
            action = 2; // created
        } else if !is_dir && matches!(disposition, 0 | 4 | 5) {
            if let Err(e) = fs::set_len(&path, 0) {
                return (fs_status(e), Vec::new());
            }
            action = 3; // overwritten
        } else if !is_dir {
            size = fs::file_len(&path).unwrap_or(0);
        }
        let dir = want_dir || is_dir;
        let delete_on_close = options & 0x1000 != 0 && !crate::access::holds_secret(&path);
        let id = self.next_file;
        self.next_file += 1;
        self.opens.insert(id, Open { path, is_dir: dir, size, stored: size, changed: false, delete_on_close, listing: None, pipe: None });
        (STATUS_OK, Self::create_reply(id, action, size, dir))
    }

    fn create_reply(id: u64, action: u32, size: u64, dir: bool) -> Vec<u8> {
        let t = filetime();
        let mut b = Vec::new();
        put16(&mut b, 89);
        b.push(0); // no oplock
        b.push(0);
        put32(&mut b, action);
        for _ in 0..4 {
            put64(&mut b, t);
        }
        put64(&mut b, size);
        put64(&mut b, size);
        put32(&mut b, if dir { ATTR_DIRECTORY } else { ATTR_ARCHIVE });
        put32(&mut b, 0);
        put64(&mut b, id);
        put64(&mut b, id);
        put32(&mut b, 0);
        put32(&mut b, 0);
        b
    }

    /// Closes: writes changes, deletes what is marked. (status, (size, directory)).
    fn close(&mut self, id: u64) -> (u32, Option<(u64, bool)>) {
        let Some(o) = self.opens.remove(&id) else { return (STATUS_FILE_CLOSED, None) };
        if o.delete_on_close {
            return (fs::remove(&o.path).map_or_else(fs_status, |_| STATUS_OK), None);
        }
        // The announced end of file: whatever the client never wrote is zeros.
        let r = if o.size != o.stored { fs::set_len(&o.path, o.size) } else { Ok(()) };
        if o.changed {
            fs::touch();
        }
        match r {
            Ok(()) => (STATUS_OK, Some((o.size, o.is_dir))),
            Err(e) => (fs_status(e), None),
        }
    }

    fn read(&mut self, fid: Option<u64>, body: &[u8]) -> (u32, Vec<u8>) {
        let (len, offset) = (le32(body, 4) as usize, le64(body, 8) as usize);
        if let Some(pipe) = fid.and_then(|id| self.opens.get_mut(&id)).and_then(|o| o.pipe.as_mut()) {
            if pipe.is_empty() {
                return (STATUS_PIPE_EMPTY, Vec::new());
            }
            let n = len.min(pipe.len());
            let chunk: Vec<u8> = pipe.drain(..n).collect();
            let status = if pipe.is_empty() { STATUS_OK } else { STATUS_BUFFER_OVERFLOW };
            let mut b = Vec::new();
            put16(&mut b, 17);
            b.push(80);
            b.push(0);
            put32(&mut b, chunk.len() as u32);
            put32(&mut b, 0);
            put32(&mut b, 0);
            b.extend_from_slice(&chunk);
            return (status, b);
        }
        let Some(o) = fid.and_then(|id| self.opens.get(&id)) else { return (STATUS_FILE_CLOSED, Vec::new()) };
        if o.is_dir {
            return (STATUS_FILE_IS_A_DIRECTORY, Vec::new());
        }
        if offset as u64 >= o.size && len > 0 {
            return (STATUS_END_OF_FILE, Vec::new());
        }
        let want = (len as u64).min(o.size.saturating_sub(offset as u64)) as usize;
        let mut chunk = match fs::read_range(&o.path, offset as u64, want) {
            Ok((data, _)) => data,
            Err(e) => return (fs_status(e), Vec::new()),
        };
        // The end of file is announced but not written yet.
        chunk.resize(want, 0);
        let mut b = Vec::new();
        put16(&mut b, 17);
        b.push(80);
        b.push(0);
        put32(&mut b, chunk.len() as u32);
        put32(&mut b, 0);
        put32(&mut b, 0);
        b.extend_from_slice(&chunk);
        (STATUS_OK, b)
    }

    fn write(&mut self, fid: Option<u64>, body: &[u8]) -> (u32, Vec<u8>) {
        let (data_off, len, offset) = (le16(body, 2) as usize, le32(body, 4) as usize, le64(body, 8) as usize);
        let Some(bytes) = data_off.checked_sub(64).and_then(|o| body.get(o..o + len)) else {
            return (STATUS_INVALID_PARAMETER, Vec::new());
        };
        let bytes = bytes.to_vec();
        let Some(id) = fid else { return (STATUS_FILE_CLOSED, Vec::new()) };
        let shares = self.share_names();
        if let Some(pipe) = self.opens.get_mut(&id).and_then(|o| o.pipe.as_mut()) {
            *pipe = crate::net::srvsvc::handle(&bytes, &shares).unwrap_or_default();
            let mut b = Vec::new();
            put16(&mut b, 17);
            put16(&mut b, 0);
            put32(&mut b, bytes.len() as u32);
            put32(&mut b, 0);
            put32(&mut b, 0);
            return (STATUS_OK, b);
        }
        let Some(o) = self.opens.get_mut(&id) else { return (STATUS_FILE_CLOSED, Vec::new()) };
        if o.is_dir {
            return (STATUS_FILE_IS_A_DIRECTORY, Vec::new());
        }
        // Straight to disk, in pieces: the file may be larger than all memory.
        if let Err(e) = fs::write_at(&o.path, offset as u64, &bytes) {
            return (fs_status(e), Vec::new());
        }
        let end = offset as u64 + bytes.len() as u64;
        o.stored = o.stored.max(end);
        o.size = o.size.max(end);
        o.changed = true;
        let mut b = Vec::new();
        put16(&mut b, 17);
        put16(&mut b, 0);
        put32(&mut b, bytes.len() as u32);
        put32(&mut b, 0);
        put32(&mut b, 0);
        (STATUS_OK, b)
    }

    fn query_directory(&mut self, fid: Option<u64>, body: &[u8]) -> (u32, Vec<u8>) {
        let class = body[2];
        let flags = body[3];
        let (off, len) = (le16(body, 24) as usize, le16(body, 26) as usize);
        let pattern = from_utf16(off.checked_sub(64).and_then(|o| body.get(o..o + len)).unwrap_or(&[]));
        let max = le32(body, 28) as usize;
        let Some(o) = fid.and_then(|id| self.opens.get_mut(&id)) else { return (STATUS_FILE_CLOSED, Vec::new()) };
        if !o.is_dir {
            return (STATUS_INVALID_PARAMETER, Vec::new());
        }
        if flags & (0x01 | 0x10) != 0 || o.listing.is_none() {
            let mut list = alloc::vec![
                fs::Entry { name: ".".into(), is_dir: true, size: 0 },
                fs::Entry { name: "..".into(), is_dir: true, size: 0 }
            ];
            list.extend(fs::list(&o.path).unwrap_or_default());
            // No masks other than "*": an exact name is an existence check.
            if !pattern.is_empty() && pattern != "*" && !pattern.contains('*') && !pattern.contains('?') {
                list.retain(|e| e.name.eq_ignore_ascii_case(&pattern));
            }
            o.listing = Some((list, 0));
        }
        let (list, pos) = o.listing.as_mut().unwrap();
        if *pos >= list.len() {
            return (if *pos == 0 && !pattern.is_empty() && pattern != "*" { STATUS_NOT_FOUND } else { STATUS_NO_MORE_FILES }, Vec::new());
        }
        let t = filetime();
        let mut out: Vec<u8> = Vec::new();
        let mut last_at = None;
        while *pos < list.len() {
            let e = &list[*pos];
            let name = utf16(&e.name);
            let mut r = Vec::new();
            put32(&mut r, 0); // NextEntryOffset — filled in later
            put32(&mut r, 0);
            for _ in 0..4 {
                put64(&mut r, t);
            }
            let size = if e.is_dir { 0 } else { e.size as u64 };
            put64(&mut r, size);
            put64(&mut r, size.div_ceil(4096) * 4096);
            put32(&mut r, if e.is_dir { ATTR_DIRECTORY } else { ATTR_ARCHIVE });
            put32(&mut r, name.len() as u32);
            match class {
                1 => {}                     // FileDirectoryInformation
                2 => put32(&mut r, 0),      // FileFullDirectoryInformation: EaSize
                3 => {
                    // FileBothDirectoryInformation: EaSize, ShortNameLength, ShortName[24]
                    put32(&mut r, 0);
                    r.extend_from_slice(&[0; 26]);
                }
                0x25 => {
                    // FileIdBothDirectoryInformation
                    put32(&mut r, 0);
                    r.extend_from_slice(&[0; 26]);
                    put16(&mut r, 0);
                    put64(&mut r, *pos as u64 + 1);
                }
                0x26 => {
                    // FileIdFullDirectoryInformation
                    put32(&mut r, 0);
                    put32(&mut r, 0);
                    put64(&mut r, *pos as u64 + 1);
                }
                0x0C => {
                    // FileNamesInformation: only NextEntryOffset, FileIndex, length, name.
                    r.truncate(8);
                    put32(&mut r, name.len() as u32);
                }
                _ => return (STATUS_NOT_SUPPORTED, Vec::new()),
            }
            r.extend_from_slice(&name);
            while r.len() % 8 != 0 {
                r.push(0);
            }
            if out.len() + r.len() > max {
                if out.is_empty() {
                    return (STATUS_INFO_LENGTH_MISMATCH, Vec::new());
                }
                break;
            }
            if let Some(prev) = last_at {
                let off = (out.len() - prev) as u32;
                out[prev..prev + 4].copy_from_slice(&off.to_le_bytes());
            }
            last_at = Some(out.len());
            out.extend_from_slice(&r);
            *pos += 1;
            if flags & 0x02 != 0 {
                break; // a single entry
            }
        }
        let mut b = Vec::new();
        put16(&mut b, 9);
        put16(&mut b, 72);
        put32(&mut b, out.len() as u32);
        b.extend_from_slice(&out);
        (STATUS_OK, b)
    }

    fn query_info(&mut self, fid: Option<u64>, tree: u32, body: &[u8]) -> (u32, Vec<u8>) {
        let (kind, class) = (body[2], body[3]);
        let Some(id) = fid else { return (STATUS_FILE_CLOSED, Vec::new()) };
        let Some(o) = self.opens.get(&id) else { return (STATUS_FILE_CLOSED, Vec::new()) };
        let (path, dir) = (o.path.clone(), o.is_dir);
        let size = if dir { 0 } else { o.size };
        let t = filetime();
        let attr = if dir { ATTR_DIRECTORY } else { ATTR_ARCHIVE };
        let alloc_size = size.div_ceil(4096) * 4096;
        let basic = |v: &mut Vec<u8>| {
            for _ in 0..4 {
                put64(v, t);
            }
            put32(v, attr);
            put32(v, 0);
        };
        let standard = |v: &mut Vec<u8>| {
            put64(v, alloc_size);
            put64(v, size);
            put32(v, 1);
            v.push(0);
            v.push(dir as u8);
            put16(v, 0);
        };
        let mut info = Vec::new();
        match (kind, class) {
            (3, _) => {
                // Security descriptor: owner, group and a DACL "Everyone — full access".
                let sd = security_descriptor(dir);
                if sd.len() > le32(body, 4) as usize {
                    return (STATUS_BUFFER_TOO_SMALL, alloc::vec![9, 0, 0, 0, 4, 0, 0, 0, sd.len() as u8, (sd.len() >> 8) as u8, 0, 0]);
                }
                info = sd;
            }
            (1, 0x16) => {
                // FileStreamInformation: a file has the single stream ::$DATA.
                if !dir {
                    let name = utf16("::$DATA");
                    put32(&mut info, 0);
                    put32(&mut info, name.len() as u32);
                    put64(&mut info, size);
                    put64(&mut info, alloc_size);
                    info.extend_from_slice(&name);
                }
            }
            (2, 0x0B) => {
                // FileFsSectorSizeInformation.
                for v in [512, 512, 512, 512, 0, 0, 0] {
                    put32(&mut info, v);
                }
            }
            (1, 4) => basic(&mut info),
            (1, 5) => standard(&mut info),
            (1, 6) => put64(&mut info, id),
            (1, 7) => put32(&mut info, 0),
            (1, 0x23) => {
                put32(&mut info, attr);
                put32(&mut info, 0);
            }
            (1, 0x22) => {
                for _ in 0..4 {
                    put64(&mut info, t);
                }
                put64(&mut info, alloc_size);
                put64(&mut info, size);
                put32(&mut info, attr);
                put32(&mut info, 0);
            }
            (1, 0x12) => {
                // FileAllInformation: Basic, Standard, Internal, Ea, Access, Position, Mode, Alignment, Name.
                basic(&mut info);
                standard(&mut info);
                put64(&mut info, id);
                put32(&mut info, 0);
                put32(&mut info, 0x001F_01FF);
                put64(&mut info, 0);
                put32(&mut info, 0);
                put32(&mut info, 0);
                let root = self.root_of(tree).unwrap_or_default();
                let rel = path.strip_prefix(root.trim_end_matches('/')).unwrap_or(&path).replace('/', "\\");
                let name = utf16(if rel.is_empty() { "\\" } else { &rel });
                put32(&mut info, name.len() as u32);
                info.extend_from_slice(&name);
            }
            (2, 1) => {
                // FileFsVolumeInformation: time, serial number, label.
                let label = utf16("GlassOS");
                put64(&mut info, t);
                put32(&mut info, 0x6C61_7373);
                put32(&mut info, label.len() as u32);
                info.push(0);
                info.push(0);
                info.extend_from_slice(&label);
            }
            (2, 3) | (2, 7) => {
                let (used, total) = fs::usage_of(&path).unwrap_or((0, 64 << 20));
                let (units, free) = (total / 4096, (total - used) / 4096);
                put64(&mut info, units);
                put64(&mut info, free);
                if class == 7 {
                    put64(&mut info, free);
                }
                put32(&mut info, 8);
                put32(&mut info, 512);
            }
            (2, 4) => {
                put32(&mut info, 7); // FILE_DEVICE_DISK
                put32(&mut info, 0x20); // FILE_DEVICE_IS_MOUNTED
            }
            (2, 5) => {
                // FileFsAttributeInformation: long names, case is preserved.
                let fsname = utf16("FAT32");
                put32(&mut info, 0x0000_0002 | 0x0000_0004);
                put32(&mut info, 255);
                put32(&mut info, fsname.len() as u32);
                info.extend_from_slice(&fsname);
            }
            _ => return (STATUS_NOT_SUPPORTED, Vec::new()),
        }
        let max = le32(body, 4) as usize;
        if info.len() > max {
            info.truncate(max);
        }
        let mut b = Vec::new();
        put16(&mut b, 9);
        put16(&mut b, 72);
        put32(&mut b, info.len() as u32);
        b.extend_from_slice(&info);
        (STATUS_OK, b)
    }

    fn set_info(&mut self, fid: Option<u64>, tree: u32, body: &[u8]) -> (u32, Vec<u8>) {
        let (kind, class) = (body[2], body[3]);
        let (len, off) = (le32(body, 4) as usize, le16(body, 8) as usize);
        let Some(buf) = off.checked_sub(64).and_then(|o| body.get(o..o + len)).map(|b| b.to_vec()) else {
            return (STATUS_INVALID_PARAMETER, Vec::new());
        };
        let Some(id) = fid else { return (STATUS_FILE_CLOSED, Vec::new()) };
        if !self.opens.contains_key(&id) {
            return (STATUS_FILE_CLOSED, Vec::new());
        }
        let ok = (STATUS_OK, vec![2, 0]);
        match (kind, class) {
            (1, 4) | (1, 19) => ok, // times/attributes, allocation — not stored
            (1, 13) => {
                let o = self.opens.get_mut(&id).unwrap();
                if crate::access::holds_secret(&o.path) {
                    return (STATUS_ACCESS_DENIED, Vec::new());
                }
                o.delete_on_close = buf.first().is_some_and(|&b| b != 0);
                ok
            }
            (1, 20) => {
                let size = le64(&buf, 0);
                let o = self.opens.get_mut(&id).unwrap();
                // The Windows copier announces the size first, then writes. We do not write zeros
                // for it (that would write the file twice), only check right away whether it fits:
                // otherwise the error would come in the middle of copying.
                if size > o.stored
                    && let Err(e) = fs::can_hold(&o.path, size - o.stored, size)
                {
                    return (fs_status(e), Vec::new());
                }
                if size < o.stored {
                    if let Err(e) = fs::set_len(&o.path, size) {
                        return (fs_status(e), Vec::new());
                    }
                    o.stored = size;
                }
                o.size = size;
                o.changed = true;
                ok
            }
            (1, 10) => {
                // FileRenameInformation: ReplaceIfExists, RootDirectory, the name from the share root.
                let name_len = le32(&buf, 16) as usize;
                let name = from_utf16(buf.get(20..20 + name_len).unwrap_or(&[]));
                let root = match self.root_of(tree) {
                    Ok(r) => r,
                    Err(st) => return (st, Vec::new()),
                };
                let to = Self::local_path(&root, &name);
                let o = self.opens.get_mut(&id).unwrap();
                if crate::access::holds_secret(&to) || crate::access::holds_secret(&o.path) {
                    return (STATUS_ACCESS_DENIED, Vec::new());
                }
                if buf[0] != 0 && fs::exists(&to) && to != o.path {
                    let _ = fs::remove(&to);
                }
                match fs::rename(&o.path, &to) {
                    Ok(()) => {
                        o.path = to;
                        ok
                    }
                    Err(e) => (fs_status(e), Vec::new()),
                }
            }
            _ => (STATUS_NOT_SUPPORTED, Vec::new()),
        }
    }
}

/// The "Everyone" SID (S-1-1-0).
const SID_EVERYONE: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];

/// A self-relative security descriptor: owner and group are "Everyone", the DACL has
/// one "allow everything" entry (with inheritance for folders).
fn security_descriptor(dir: bool) -> Vec<u8> {
    let mut ace = alloc::vec![0, if dir { 3 } else { 0 }];
    put16(&mut ace, 20);
    put32(&mut ace, 0x001F_01FF);
    ace.extend_from_slice(&SID_EVERYONE);
    let mut acl = alloc::vec![2, 0];
    put16(&mut acl, (8 + ace.len()) as u16);
    put16(&mut acl, 1);
    put16(&mut acl, 0);
    acl.extend_from_slice(&ace);
    let mut sd = alloc::vec![1, 0];
    put16(&mut sd, 0x8004); // self-relative, DACL present
    put32(&mut sd, 20); // owner
    put32(&mut sd, 32); // group
    put32(&mut sd, 0); // no SACL
    put32(&mut sd, 44); // DACL
    sd.extend_from_slice(&SID_EVERYONE);
    sd.extend_from_slice(&SID_EVERYONE);
    sd.extend_from_slice(&acl);
    sd
}
