//! SMB2 client — Windows and Samba network folders (dialects 2.0.2 and 2.1).
//!
//! Connection: TCP (port 445), each message is a 4-byte length and an SMB2 packet.
//! Login is NTLMv2 (MD4/HMAC-MD5); packets are signed when required
//! (HMAC-SHA256 with the session key). A share is mounted as another volume in /media:
//! reads and writes go straight over the network (no cache on our side).

use super::{Ip, TcpStream, ip_str, resolve};
use crate::crypto::{hmac_md5, hmac_sha256, md4};
use crate::fs::{Entry, Filesystem, FsError};
use crate::interrupts::HZ;
use crate::kprintln;
use crate::sync::IrqLock;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

const NEGOTIATE: u16 = 0;
const SESSION_SETUP: u16 = 1;
const TREE_CONNECT: u16 = 3;
const CREATE: u16 = 5;
const CLOSE: u16 = 6;
const READ: u16 = 8;
const WRITE: u16 = 9;
const QUERY_DIRECTORY: u16 = 0x0E;
const QUERY_INFO: u16 = 0x10;
const SET_INFO: u16 = 0x11;

const STATUS_OK: u32 = 0;
const STATUS_PENDING: u32 = 0x0000_0103;
const STATUS_MORE_PROCESSING: u32 = 0xC000_0016;
const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;
const STATUS_END_OF_FILE: u32 = 0xC000_0011;
const STATUS_NOT_FOUND: u32 = 0xC000_0034;
const STATUS_PATH_NOT_FOUND: u32 = 0xC000_003A;
const STATUS_COLLISION: u32 = 0xC000_0035;
const STATUS_NOT_A_DIRECTORY: u32 = 0xC000_0103;
const STATUS_FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
const STATUS_DISK_FULL: u32 = 0xC000_007F;
const STATUS_LOGON_FAILURE: u32 = 0xC000_006D;
const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
const STATUS_BAD_NETWORK_NAME: u32 = 0xC000_00CC;
const STATUS_DIRECTORY_NOT_EMPTY: u32 = 0xC000_0101;
const STATUS_BUFFER_OVERFLOW: u32 = 0x8000_0005;
/// The session on the server ended: a new login is needed.
const STATUS_USER_SESSION_DELETED: u32 = 0xC000_0203;
const STATUS_SESSION_EXPIRED: u32 = 0xC000_035C;
const STATUS_NAME_DELETED: u32 = 0xC000_00C9;

// Access and CREATE parameters.
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const DELETE: u32 = 0x0001_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_LIST_DIRECTORY: u32 = 0x01;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OVERWRITE_IF: u32 = 5;
const FILE_DIRECTORY_FILE: u32 = 0x01;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_DELETE_ON_CLOSE: u32 = 0x1000;
const ATTR_DIRECTORY: u32 = 0x10;

/// How long to wait for a server answer. File operations come from windows, and a dead
/// server must not hang the UI for long.
const RECV_TIMEOUT: u64 = 8 * HZ;

/// A read/write chunk (SMB 2.0.2 does not allow more than 64 KiB at once).
const CHUNK: usize = 64 * 1024;
/// How many read requests wait for an answer at the same time. One at a time per 64 KiB would lose
/// a round trip each, and the server would idle. The answers must fit into the TCP window.
const READ_AHEAD: usize = 4;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn from_utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

/// A server answer: status, header and body (after the 64-byte header).
struct Reply {
    status: u32,
    session: u64,
    tree: u32,
    body: Vec<u8>,
}

/// A path inside a share: "/a/b" → "a\b".
fn smb_path(path: &str) -> String {
    path.trim_matches('/').replace('/', "\\")
}

fn status_error(status: u32) -> FsError {
    match status {
        STATUS_NOT_FOUND | STATUS_PATH_NOT_FOUND => FsError::NotFound,
        STATUS_COLLISION => FsError::AlreadyExists,
        STATUS_NOT_A_DIRECTORY => FsError::NotADirectory,
        STATUS_FILE_IS_A_DIRECTORY => FsError::IsADirectory,
        STATUS_DISK_FULL => FsError::NoSpace,
        _ => FsError::Io,
    }
}

pub struct Smb {
    stream: TcpStream,
    message_id: u64,
    session: u64,
    tree: u32,
    /// Signing key (Some — the server requires signing).
    signing: Option<[u8; 16]>,
    /// Bytes received but not yet parsed.
    inbox: Vec<u8>,
    max_read: usize,
    max_write: usize,
    /// What to display: "\\server\share".
    label: String,
    /// Used and total space on the share (queried on connect).
    space: (u64, u64),
    /// How many requests the server allows to send without waiting for answers.
    credits: u32,
    /// The connection is lost: no more requests go out.
    broken: bool,
    /// Lost because the server closed it itself (rather than stopped answering).
    closed: bool,
}

impl Smb {
    // ------------------------------------------------------------ transport

    fn header(&mut self, command: u16) -> Vec<u8> {
        let mut h = vec![0u8; 64];
        h[0..4].copy_from_slice(b"\xFESMB");
        h[4..6].copy_from_slice(&64u16.to_le_bytes());
        h[6..8].copy_from_slice(&1u16.to_le_bytes()); // CreditCharge
        h[12..14].copy_from_slice(&command.to_le_bytes());
        h[14..16].copy_from_slice(&64u16.to_le_bytes()); // CreditRequest
        h[24..32].copy_from_slice(&self.message_id.to_le_bytes());
        h[32..36].copy_from_slice(&0xFEFFu32.to_le_bytes());
        h[36..40].copy_from_slice(&self.tree.to_le_bytes());
        h[40..48].copy_from_slice(&self.session.to_le_bytes());
        self.message_id += 1;
        self.credits = self.credits.saturating_sub(1);
        h
    }

    /// Sends request `command` with body `body` and waits for its answer.
    fn call(&mut self, command: u16, body: &[u8]) -> Result<Reply, &'static str> {
        let id = self.post(command, body)?;
        Ok(self.answer(&[id])?.1)
    }

    /// Remembers what happened to the connection.
    fn watch(&mut self, r: Result<(u64, Reply), &'static str>) -> Result<(u64, Reply), &'static str> {
        match &r {
            Ok((_, reply)) if matches!(reply.status, STATUS_USER_SESSION_DELETED | STATUS_SESSION_EXPIRED | STATUS_NAME_DELETED) => {
                self.broken = true;
                self.closed = true;
            }
            Err(e) => {
                self.broken = true;
                self.closed = !e.contains("timed out");
            }
            _ => {}
        }
        r
    }

    /// Sends a request; returns its id, `answer` collects the reply.
    fn post(&mut self, command: u16, body: &[u8]) -> Result<u64, &'static str> {
        if self.broken {
            return Err("connection lost");
        }
        let mut msg = self.header(command);
        let id = self.message_id - 1;
        msg.extend_from_slice(body);
        if let Some(key) = self.signing
            && command != NEGOTIATE
            && command != SESSION_SETUP
        {
            let flags = le32(&msg, 16) | 0x08; // signed
            msg[16..20].copy_from_slice(&flags.to_le_bytes());
            let sig = hmac_sha256(&key, &msg);
            msg[48..64].copy_from_slice(&sig[..16]);
        }
        let mut frame = (msg.len() as u32).to_be_bytes().to_vec();
        frame[0] = 0;
        frame.extend_from_slice(&msg);
        if let Err(e) = self.stream.send(&frame) {
            self.watch(Err(e))?;
        }
        Ok(id)
    }

    /// Waits for an answer to one of requests `ids`.
    fn answer(&mut self, ids: &[u64]) -> Result<(u64, Reply), &'static str> {
        let r = self.next_answer(ids);
        self.watch(r)
    }

    fn next_answer(&mut self, ids: &[u64]) -> Result<(u64, Reply), &'static str> {
        loop {
            let m = self.receive()?;
            if m.len() < 64 || &m[0..4] != b"\xFESMB" {
                return Err("bad SMB2 reply");
            }
            let status = le32(&m, 8);
            let id = le64(&m, 24);
            // An interim "still working" answer — the real one follows.
            if status == STATUS_PENDING || !ids.contains(&id) {
                continue;
            }
            return Ok((id, Reply { status, session: le64(&m, 40), tree: le32(&m, 36), body: m[64..].to_vec() }));
        }
    }

    /// One message from the stream (4-byte length + packet).
    fn receive(&mut self) -> Result<Vec<u8>, &'static str> {
        loop {
            if self.inbox.len() >= 4 {
                let len = u32::from_be_bytes([0, self.inbox[1], self.inbox[2], self.inbox[3]]) as usize;
                if self.inbox.len() >= 4 + len {
                    let msg = self.inbox[4..4 + len].to_vec();
                    self.inbox.drain(..4 + len);
                    if msg.len() >= 16 {
                        self.credits += le16(&msg, 14) as u32;
                    }
                    return Ok(msg);
                }
            }
            let chunk = self.stream.recv(RECV_TIMEOUT)?;
            if chunk.is_empty() {
                return Err("server closed the connection");
            }
            self.inbox.extend_from_slice(&chunk);
        }
    }

    // ------------------------------------------------------------ login

    /// Connects to `\\host\share`. An empty `user` means guest login: first as
    /// "guest" with an empty password (Windows lets that in with "Everyone" access), and if that fails —
    /// anonymously (Samba, impacket).
    fn connect(host: &str, port: u16, share: &str, user: &str, password: &str, domain: &str) -> Result<Smb, String> {
        if user.is_empty() {
            match Self::connect_as(host, port, share, "guest", "", domain) {
                Ok(s) => return Ok(s),
                Err(e) => kprintln!("smb: {host}: guest login: {e}; trying anonymous"),
            }
        }
        Self::connect_as(host, port, share, user, password, domain)
    }

    fn connect_as(host: &str, port: u16, share: &str, user: &str, password: &str, domain: &str) -> Result<Smb, String> {
        if !super::wait_online() {
            return Err("no network".into());
        }
        let ip = resolve(host).ok_or_else(|| format!("{host}: host not found"))?;
        let stream = TcpStream::connect(ip, port).map_err(|e| format!("{host}:{port}: {e}"))?;
        let mut s = Smb {
            stream,
            message_id: 0,
            session: 0,
            tree: 0,
            signing: None,
            inbox: Vec::new(),
            max_read: CHUNK,
            max_write: CHUNK,
            label: String::new(),
            space: (0, 0),
            credits: 1,
            broken: false,
            closed: false,
        };
        // NEGOTIATE: dialects 2.0.2 and 2.1, signing supported.
        let mut b = vec![0u8; 36];
        b[0..2].copy_from_slice(&36u16.to_le_bytes());
        b[2..4].copy_from_slice(&2u16.to_le_bytes());
        b[4..6].copy_from_slice(&1u16.to_le_bytes());
        b[12..28].copy_from_slice(&(crate::arch::rdtsc() as u128).to_le_bytes());
        b.extend_from_slice(&0x0202u16.to_le_bytes());
        b.extend_from_slice(&0x0210u16.to_le_bytes());
        let r = s.call(NEGOTIATE, &b)?;
        if r.status != STATUS_OK || r.body.len() < 64 {
            return Err(format!("negotiate failed ({:#x}); the server may require SMB 3", r.status));
        }
        let dialect = le16(&r.body, 4);
        let sign_required = le16(&r.body, 2) & 0x02 != 0;
        s.max_read = (le32(&r.body, 32) as usize).clamp(4096, CHUNK);
        s.max_write = (le32(&r.body, 36) as usize).clamp(4096, CHUNK);
        kprintln!("smb: {host}: dialect {dialect:#x}, signing {}", if sign_required { "required" } else { "optional" });

        // SESSION_SETUP, step 1: NTLMSSP NEGOTIATE.
        const FLAGS: u32 = 0x0000_0001 | 0x0000_0004 | 0x0000_0010 | 0x0000_0200 | 0x0000_8000 | 0x0008_0000 | 0x0080_0000 | 0x2000_0000 | 0x8000_0000;
        let mut nego = b"NTLMSSP\0".to_vec();
        nego.extend_from_slice(&1u32.to_le_bytes());
        nego.extend_from_slice(&FLAGS.to_le_bytes());
        nego.extend_from_slice(&[0; 16]); // domain and workstation — empty
        let r = s.session_setup(&nego)?;
        if r.status != STATUS_MORE_PROCESSING {
            return Err(format!("session setup failed ({:#x})", r.status));
        }
        s.session = r.session;
        let challenge = s.security_buffer(&r.body)?;
        if challenge.len() < 48 || &challenge[0..8] != b"NTLMSSP\0" || le32(&challenge, 8) != 2 {
            return Err("server did not send an NTLM challenge".into());
        }
        let server_challenge: [u8; 8] = challenge[24..32].try_into().unwrap();
        let (ti_len, ti_off) = (le16(&challenge, 40) as usize, le32(&challenge, 44) as usize);
        let target_info = challenge.get(ti_off..ti_off + ti_len).unwrap_or(&[]).to_vec();

        // Step 2: AUTHENTICATE with an NTLMv2 response.
        let (auth, session_key) = authenticate(user, password, domain, &server_challenge, &target_info, FLAGS);
        let r = s.session_setup(&auth)?;
        match r.status {
            STATUS_OK => {}
            STATUS_LOGON_FAILURE => return Err("wrong user name or password".into()),
            st => return Err(format!("login failed ({st:#x})")),
        }
        // A guest or anonymous session has nothing to sign with.
        let guest = le16(&r.body, 2) & 0x03 != 0;
        if sign_required && !user.is_empty() && !guest {
            s.signing = Some(session_key);
        }

        // TREE_CONNECT \\host\share
        let path = utf16(&format!("\\\\{host}\\{share}"));
        let mut b = vec![0u8; 8];
        b[0..2].copy_from_slice(&9u16.to_le_bytes());
        b[4..6].copy_from_slice(&72u16.to_le_bytes());
        b[6..8].copy_from_slice(&(path.len() as u16).to_le_bytes());
        b.extend_from_slice(&path);
        let r = s.call(TREE_CONNECT, &b)?;
        match r.status {
            STATUS_OK => s.tree = r.tree,
            STATUS_BAD_NETWORK_NAME | STATUS_PATH_NOT_FOUND | STATUS_NOT_FOUND => return Err(format!("no share named {share}")),
            STATUS_ACCESS_DENIED => return Err("access to the share is denied".into()),
            st => return Err(format!("tree connect failed ({st:#x})")),
        }
        s.space = space(&mut s).unwrap_or((0, 0));
        kprintln!("smb: connected to \\\\{host}\\{share}");
        Ok(s)
    }

    fn session_setup(&mut self, token: &[u8]) -> Result<Reply, String> {
        let mut b = vec![0u8; 24];
        b[0..2].copy_from_slice(&25u16.to_le_bytes());
        b[3] = 1; // signing supported
        b[12..14].copy_from_slice(&88u16.to_le_bytes());
        b[14..16].copy_from_slice(&(token.len() as u16).to_le_bytes());
        b.extend_from_slice(token);
        Ok(self.call(SESSION_SETUP, &b)?)
    }

    fn security_buffer(&self, body: &[u8]) -> Result<Vec<u8>, String> {
        let (off, len) = (le16(body, 4) as usize, le16(body, 6) as usize);
        let at = off.checked_sub(64).ok_or("bad security buffer")?;
        let blob = body.get(at..at + len).ok_or("bad security buffer")?;
        // The server may have wrapped NTLMSSP in SPNEGO — find it inside.
        let start = blob.windows(8).position(|w| w == b"NTLMSSP\0").ok_or("no NTLMSSP in reply")?;
        Ok(blob[start..].to_vec())
    }

    // ------------------------------------------------------------ files

    /// CREATE: opens a path; (FileId, attributes, size).
    fn open(&mut self, path: &str, access: u32, disposition: u32, options: u32) -> Result<([u8; 16], u32, u64), u32> {
        let name = utf16(&smb_path(path));
        let mut b = vec![0u8; 56];
        b[0..2].copy_from_slice(&57u16.to_le_bytes());
        b[4..8].copy_from_slice(&2u32.to_le_bytes()); // Impersonation
        b[24..28].copy_from_slice(&access.to_le_bytes());
        b[28..32].copy_from_slice(&0x80u32.to_le_bytes()); // FILE_ATTRIBUTE_NORMAL
        b[32..36].copy_from_slice(&7u32.to_le_bytes()); // share access: read, write, delete
        b[36..40].copy_from_slice(&disposition.to_le_bytes());
        b[40..44].copy_from_slice(&options.to_le_bytes());
        b[44..46].copy_from_slice(&120u16.to_le_bytes());
        b[46..48].copy_from_slice(&(name.len() as u16).to_le_bytes());
        b.extend_from_slice(&name);
        if name.is_empty() {
            b.push(0); // the buffer cannot be empty
        }
        let r = self.call(CREATE, &b).map_err(|_| 0xFFFF_FFFFu32)?;
        if r.status != STATUS_OK || r.body.len() < 80 {
            return Err(r.status);
        }
        Ok((r.body[64..80].try_into().unwrap(), le32(&r.body, 56), le64(&r.body, 48)))
    }

    fn close(&mut self, id: &[u8; 16]) {
        let mut b = vec![0u8; 24];
        b[0..2].copy_from_slice(&24u16.to_le_bytes());
        b[8..24].copy_from_slice(id);
        let _ = self.call(CLOSE, &b);
    }

    /// A named pipe exchange: write the request, read the answer.
    fn pipe_call(&mut self, id: &[u8; 16], request: &[u8]) -> Result<Vec<u8>, String> {
        self.write_all(id, request).map_err(|st| format!("pipe write failed ({st:#x})"))?;
        let mut reply = Vec::new();
        for _ in 0..64 {
            let mut b = vec![0u8; 48];
            b[0..2].copy_from_slice(&49u16.to_le_bytes());
            b[2] = 0x50;
            b[4..8].copy_from_slice(&(CHUNK as u32).min(self.max_read as u32).to_le_bytes());
            b[16..32].copy_from_slice(id);
            b.push(0);
            let r = self.call(READ, &b)?;
            if r.status != STATUS_OK && r.status != STATUS_BUFFER_OVERFLOW {
                return Err(format!("pipe read failed ({:#x})", r.status));
            }
            let (off, len) = (r.body[2] as usize, le32(&r.body, 4) as usize);
            let data = off.checked_sub(64).and_then(|o| r.body.get(o..o + len)).ok_or("bad pipe reply")?;
            reply.extend_from_slice(data);
            if r.status == STATUS_OK {
                return Ok(reply);
            }
        }
        Err("pipe reply is too long".into())
    }

    /// The server's share list: the srvsvc service through a pipe on the IPC$ share.
    fn share_list(&mut self, host: &str) -> Result<Vec<String>, String> {
        let (id, _, _) = self.open("srvsvc", GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE, FILE_OPEN, 0).map_err(|st| match st {
            STATUS_ACCESS_DENIED => "access is denied".into(),
            st => format!("no share service ({st:#x})"),
        })?;
        use super::srvsvc as rpc;
        let ack = self.pipe_call(&id, &rpc::bind_request())?;
        if !rpc::is_bind_ack(&ack) {
            self.close(&id);
            return Err("the server refused the share service".into());
        }
        let mut reply = self.pipe_call(&id, &rpc::enum_request(host))?;
        // A long answer arrives in fragments: keep reading the pipe.
        let stub = loop {
            match rpc::reassemble(&reply)? {
                Some(stub) => break stub,
                None => {
                    let mut b = vec![0u8; 48];
                    b[0..2].copy_from_slice(&49u16.to_le_bytes());
                    b[2] = 0x50;
                    b[4..8].copy_from_slice(&(self.max_read as u32).to_le_bytes());
                    b[16..32].copy_from_slice(&id);
                    b.push(0);
                    let r = self.call(READ, &b)?;
                    let (off, len) = (r.body[2] as usize, le32(&r.body, 4) as usize);
                    let data = off.checked_sub(64).and_then(|o| r.body.get(o..o + len)).ok_or("bad pipe reply")?;
                    if data.is_empty() {
                        return Err("incomplete reply".into());
                    }
                    reply.extend_from_slice(data);
                }
            }
        };
        self.close(&id);
        rpc::parse_enum(&stub).ok_or_else(|| "bad reply".into())
    }

    fn read_all(&mut self, id: &[u8; 16], size: u64) -> Result<Vec<u8>, u32> {
        self.read_at(id, 0, size as usize)
    }

    /// Up to `len` bytes from `offset`. Requests overlap, answers may arrive in any order.
    fn read_at(&mut self, id: &[u8; 16], offset: u64, len: usize) -> Result<Vec<u8>, u32> {
        let mut out = vec![0u8; len];
        // What else to request: (from where, how much). A short answer adds the remainder here.
        let mut todo: Vec<(usize, usize)> = Vec::new();
        let mut asked = 0;
        let mut end = len;
        let mut flight: Vec<(u64, usize, usize)> = Vec::new();
        loop {
            while flight.len() < READ_AHEAD && (flight.is_empty() || self.credits > 0) {
                let (at, want) = match todo.pop() {
                    Some(range) => range,
                    None if asked < end => {
                        let want = (end - asked).min(self.max_read);
                        asked += want;
                        (asked - want, want)
                    }
                    None => break,
                };
                let mut b = vec![0u8; 48];
                b[0..2].copy_from_slice(&49u16.to_le_bytes());
                b[2] = 0x50;
                b[4..8].copy_from_slice(&(want as u32).to_le_bytes());
                b[8..16].copy_from_slice(&(offset + at as u64).to_le_bytes());
                b[16..32].copy_from_slice(id);
                b.push(0);
                let message = self.post(READ, &b).map_err(|_| 0xFFFF_FFFFu32)?;
                flight.push((message, at, want));
            }
            if flight.is_empty() {
                break;
            }
            let ids: Vec<u64> = flight.iter().map(|f| f.0).collect();
            let (message, r) = self.answer(&ids).map_err(|_| 0xFFFF_FFFFu32)?;
            let i = flight.iter().position(|f| f.0 == message).ok_or(0xFFFF_FFFFu32)?;
            let (_, at, want) = flight.remove(i);
            if r.status == STATUS_END_OF_FILE {
                end = end.min(at);
                continue;
            }
            if r.status != STATUS_OK {
                return Err(r.status);
            }
            let (off, n) = (r.body[2] as usize, le32(&r.body, 4) as usize);
            let data = r.body.get(off - 64..off - 64 + n).ok_or(0xFFFF_FFFFu32)?;
            if data.is_empty() || data.len() > want {
                end = end.min(at);
                continue;
            }
            out[at..at + data.len()].copy_from_slice(data);
            if data.len() < want {
                todo.push((at + data.len(), want - data.len()));
            }
        }
        out.truncate(end);
        Ok(out)
    }

    fn write_all(&mut self, id: &[u8; 16], data: &[u8]) -> Result<(), u32> {
        self.write_from(id, 0, data)
    }

    /// Overlapped writes, like reads: up to `READ_AHEAD` requests in flight (as far as the server's
    /// credits allow). One at a time with a wait for the answer made the speed depend on the answer time.
    fn write_from(&mut self, id: &[u8; 16], base: u64, data: &[u8]) -> Result<(), u32> {
        let mut at = 0;
        let mut flight: Vec<u64> = Vec::new();
        loop {
            while at < data.len() && flight.len() < READ_AHEAD && (flight.is_empty() || self.credits > 0) {
                let n = (data.len() - at).min(self.max_write);
                let mut b = vec![0u8; 48];
                b[0..2].copy_from_slice(&49u16.to_le_bytes());
                b[2..4].copy_from_slice(&112u16.to_le_bytes());
                b[4..8].copy_from_slice(&(n as u32).to_le_bytes());
                b[8..16].copy_from_slice(&(base + at as u64).to_le_bytes());
                b[16..32].copy_from_slice(id);
                b.extend_from_slice(&data[at..at + n]);
                flight.push(self.post(WRITE, &b).map_err(|_| 0xFFFF_FFFFu32)?);
                at += n;
            }
            if flight.is_empty() {
                return Ok(());
            }
            let (message, r) = self.answer(&flight).map_err(|_| 0xFFFF_FFFFu32)?;
            flight.retain(|&m| m != message);
            if r.status != STATUS_OK {
                // The other answers are still in flight — read them so they are not mixed up with the next ones.
                while !flight.is_empty() {
                    let Ok((m, _)) = self.answer(&flight) else { break };
                    flight.retain(|&f| f != m);
                }
                return Err(r.status);
            }
        }
    }

    fn list_dir(&mut self, path: &str) -> Result<Vec<Entry>, u32> {
        let (id, attr, _) = self.open(path, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, FILE_DIRECTORY_FILE)?;
        let _ = attr;
        let mut out = Vec::new();
        let mut first = true;
        loop {
            let pattern = utf16("*");
            let mut b = vec![0u8; 32];
            b[0..2].copy_from_slice(&33u16.to_le_bytes());
            b[2] = 0x01; // FileDirectoryInformation
            b[3] = if first { 0x01 } else { 0 }; // RESTART_SCANS
            b[8..24].copy_from_slice(&id);
            b[24..26].copy_from_slice(&96u16.to_le_bytes());
            b[26..28].copy_from_slice(&(pattern.len() as u16).to_le_bytes());
            b[28..32].copy_from_slice(&(CHUNK as u32).to_le_bytes());
            b.extend_from_slice(&pattern);
            first = false;
            let Ok(r) = self.call(QUERY_DIRECTORY, &b) else { break };
            if r.status == STATUS_NO_MORE_FILES {
                break;
            }
            if r.status != STATUS_OK {
                self.close(&id);
                return Err(r.status);
            }
            let (off, len) = (le16(&r.body, 2) as usize, le32(&r.body, 4) as usize);
            let Some(buf) = r.body.get(off - 64..off - 64 + len) else { break };
            let mut at = 0;
            loop {
                let e = &buf[at..];
                if e.len() < 64 {
                    break;
                }
                let name_len = le32(e, 60) as usize;
                let name = from_utf16(&e[64..64 + name_len.min(e.len() - 64)]);
                if name != "." && name != ".." {
                    let is_dir = le32(e, 56) & ATTR_DIRECTORY != 0;
                    out.push(Entry { name, is_dir, size: if is_dir { 0 } else { le64(e, 40) as usize } });
                }
                let next = le32(e, 0) as usize;
                if next == 0 {
                    break;
                }
                at += next;
            }
        }
        self.close(&id);
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }
}

/// NTLMSSP AUTHENTICATE with an NTLMv2 response, and the session key (for signing).
fn authenticate(user: &str, password: &str, domain: &str, server_challenge: &[u8; 8], target_info: &[u8], flags: u32) -> (Vec<u8>, [u8; 16]) {
    let anonymous = user.is_empty();
    let (lm, nt, key) = if anonymous {
        (vec![0u8], Vec::new(), [0u8; 16])
    } else {
        let nt_hash = md4(&utf16(password));
        let mut id = utf16(&user.to_uppercase());
        id.extend_from_slice(&utf16(domain));
        let ntowf = hmac_md5(&nt_hash, &id);
        let client_challenge = (crate::arch::rdtsc() ^ 0x5DEE_CE66_D1CE_4E5B).to_le_bytes();
        // Time in Windows format: hundreds of nanoseconds since 1601.
        let filetime = ((crate::time::unix().max(0) as u64) + 11_644_473_600) * 10_000_000;
        let mut blob = vec![1u8, 1, 0, 0, 0, 0, 0, 0];
        blob.extend_from_slice(&filetime.to_le_bytes());
        blob.extend_from_slice(&client_challenge);
        blob.extend_from_slice(&[0; 4]);
        blob.extend_from_slice(target_info);
        blob.extend_from_slice(&[0; 4]);
        let mut msg = server_challenge.to_vec();
        msg.extend_from_slice(&blob);
        let proof = hmac_md5(&ntowf, &msg);
        let mut nt = proof.to_vec();
        nt.extend_from_slice(&blob);
        let mut lm_msg = server_challenge.to_vec();
        lm_msg.extend_from_slice(&client_challenge);
        let mut lm = hmac_md5(&ntowf, &lm_msg).to_vec();
        lm.extend_from_slice(&client_challenge);
        (lm, nt, hmac_md5(&ntowf, &proof))
    };
    let fields: [Vec<u8>; 5] = [lm, nt, utf16(domain), utf16(user), utf16(&super::identity::get().name)];
    let mut msg = b"NTLMSSP\0".to_vec();
    msg.extend_from_slice(&3u32.to_le_bytes());
    let mut payload = Vec::new();
    let base = 64;
    let mut headers = Vec::new();
    for f in &fields {
        headers.extend_from_slice(&(f.len() as u16).to_le_bytes());
        headers.extend_from_slice(&(f.len() as u16).to_le_bytes());
        headers.extend_from_slice(&((base + payload.len()) as u32).to_le_bytes());
        payload.extend_from_slice(f);
    }
    msg.extend_from_slice(&headers);
    // Encrypted session key — empty (key exchange was not requested).
    msg.extend_from_slice(&0u16.to_le_bytes());
    msg.extend_from_slice(&0u16.to_le_bytes());
    msg.extend_from_slice(&((base + payload.len()) as u32).to_le_bytes());
    let flags = if anonymous { flags | 0x0000_0800 } else { flags };
    msg.extend_from_slice(&flags.to_le_bytes());
    msg.extend_from_slice(&payload);
    (msg, key)
}

impl Filesystem for Smb {
    fn list(&mut self, path: &str) -> crate::fs::Result<Vec<Entry>> {
        self.list_dir(path).map_err(status_error)
    }

    fn read(&mut self, path: &str) -> crate::fs::Result<Vec<u8>> {
        let (id, attr, size) = self.open(path, GENERIC_READ, FILE_OPEN, 0).map_err(status_error)?;
        if attr & ATTR_DIRECTORY != 0 {
            self.close(&id);
            return Err(FsError::IsADirectory);
        }
        let data = self.read_all(&id, size);
        self.close(&id);
        data.map_err(status_error)
    }

    fn read_range(&mut self, path: &str, offset: u64, len: usize) -> crate::fs::Result<(Vec<u8>, u64)> {
        let (id, attr, size) = self.open(path, GENERIC_READ, FILE_OPEN, 0).map_err(status_error)?;
        if attr & ATTR_DIRECTORY != 0 {
            self.close(&id);
            return Err(FsError::IsADirectory);
        }
        let want = (len as u64).min(size.saturating_sub(offset)) as usize;
        let data = self.read_at(&id, offset, want);
        self.close(&id);
        Ok((data.map_err(status_error)?, size))
    }

    fn is_dir(&mut self, path: &str) -> Option<bool> {
        if smb_path(path).is_empty() {
            return Some(true);
        }
        let (id, attr, _) = self.open(path, FILE_READ_ATTRIBUTES, FILE_OPEN, 0).ok()?;
        self.close(&id);
        Some(attr & ATTR_DIRECTORY != 0)
    }

    fn write(&mut self, path: &str, data: &[u8]) -> crate::fs::Result<()> {
        let (id, _, _) = self.open(path, GENERIC_WRITE | SYNCHRONIZE, FILE_OVERWRITE_IF, FILE_NON_DIRECTORY_FILE).map_err(status_error)?;
        let r = self.write_all(&id, data);
        self.close(&id);
        r.map_err(status_error)
    }

    fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> crate::fs::Result<()> {
        let (id, _, _) = self.open(path, GENERIC_WRITE | SYNCHRONIZE, FILE_OPEN, FILE_NON_DIRECTORY_FILE).map_err(status_error)?;
        let r = self.write_from(&id, offset, data);
        self.close(&id);
        r.map_err(status_error)
    }

    fn shrink(&mut self, path: &str, len: u64) -> crate::fs::Result<()> {
        let (id, _, _) = self.open(path, GENERIC_WRITE | SYNCHRONIZE, FILE_OPEN, FILE_NON_DIRECTORY_FILE).map_err(status_error)?;
        // FileEndOfFileInformation.
        let mut b = vec![0u8; 32];
        b[0..2].copy_from_slice(&33u16.to_le_bytes());
        b[2] = 1;
        b[3] = 20;
        b[4..8].copy_from_slice(&8u32.to_le_bytes());
        b[8..10].copy_from_slice(&96u16.to_le_bytes());
        b[16..32].copy_from_slice(&id);
        b.extend_from_slice(&len.to_le_bytes());
        let r = self.call(SET_INFO, &b);
        self.close(&id);
        match r {
            Ok(r) if r.status == STATUS_OK => Ok(()),
            Ok(r) => Err(status_error(r.status)),
            Err(_) => Err(FsError::Io),
        }
    }

    fn mkdir(&mut self, path: &str) -> crate::fs::Result<()> {
        let (id, _, _) = self.open(path, GENERIC_READ, FILE_CREATE, FILE_DIRECTORY_FILE).map_err(status_error)?;
        self.close(&id);
        Ok(())
    }

    fn remove(&mut self, path: &str) -> crate::fs::Result<()> {
        if self.is_dir(path) == Some(true) {
            // Contents first: the server will not delete a non-empty folder.
            for e in self.list_dir(path).map_err(status_error)? {
                let child = format!("{}/{}", path.trim_end_matches('/'), e.name);
                self.remove(&child)?;
            }
        }
        let (id, _, _) = self.open(path, DELETE | SYNCHRONIZE, FILE_OPEN, FILE_DELETE_ON_CLOSE).map_err(|st| {
            if st == STATUS_DIRECTORY_NOT_EMPTY { FsError::Io } else { status_error(st) }
        })?;
        self.close(&id);
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str) -> crate::fs::Result<()> {
        let (id, _, _) = self.open(from, DELETE | SYNCHRONIZE, FILE_OPEN, 0).map_err(status_error)?;
        // FileRenameInformation: the new name is a path from the share root.
        let name = utf16(&smb_path(to));
        let mut info = vec![0u8; 20];
        info[16..20].copy_from_slice(&(name.len() as u32).to_le_bytes());
        info.extend_from_slice(&name);
        let mut b = vec![0u8; 32];
        b[0..2].copy_from_slice(&33u16.to_le_bytes());
        b[2] = 1; // file
        b[3] = 10; // FileRenameInformation
        b[4..8].copy_from_slice(&(info.len() as u32).to_le_bytes());
        b[8..10].copy_from_slice(&96u16.to_le_bytes());
        b[16..32].copy_from_slice(&id);
        b.extend_from_slice(&info);
        let r = self.call(SET_INFO, &b);
        self.close(&id);
        match r {
            Ok(r) if r.status == STATUS_OK => Ok(()),
            Ok(r) => Err(status_error(r.status)),
            Err(_) => Err(FsError::Io),
        }
    }

    fn usage(&self) -> (u64, u64) {
        self.space
    }

    fn has_changes(&self) -> bool {
        false
    }

    fn take_dirty(&mut self) -> Vec<(u32, Box<[u8; crate::ata::SECTOR]>)> {
        Vec::new()
    }

    fn set_writing(&mut self, _on: bool) {}

    fn trim(&mut self) {}

    fn label(&self) -> &str {
        &self.label
    }

    fn dev(&self) -> usize {
        usize::MAX
    }

    fn kind(&self) -> &'static str {
        "SMB2"
    }
}

/// Space on the share (called on connect — the trait's `usage` has no `&mut`).
/// FileFsFullSizeInformation first; if the server does not know it — FileFsSizeInformation.
pub fn space(s: &mut Smb) -> Option<(u64, u64)> {
    let (id, _, _) = s.open("", FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, FILE_DIRECTORY_FILE).ok()?;
    let mut result = None;
    // (class, answer length, offsets: total, free, sectors per unit, bytes per sector)
    for (class, len, free_at, spu_at) in [(7u8, 32usize, 16usize, 24usize), (3, 24, 8, 16)] {
        let mut b = vec![0u8; 40];
        b[0..2].copy_from_slice(&41u16.to_le_bytes());
        b[2] = 2; // file system
        b[3] = class;
        b[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        b[24..40].copy_from_slice(&id);
        let Ok(r) = s.call(QUERY_INFO, &b) else { break };
        if r.status != STATUS_OK {
            continue;
        }
        let off = le16(&r.body, 2) as usize;
        let Some(d) = r.body.get(off - 64..off - 64 + len) else { continue };
        let unit = le32(d, spu_at) as u64 * le32(d, spu_at + 4) as u64;
        let (total, free) = (le64(d, 0) * unit, le64(d, free_at) * unit);
        result = Some((total.saturating_sub(free), total));
        break;
    }
    s.close(&id);
    result
}

// ---------------------------------------------------------------- mounts

/// The file with remembered mounts: what to mount at boot and which name to log in
/// to computers with. Passwords are stored in plain text, so Lua programs cannot
/// access this file (see lua::api).
pub const CONNECTIONS: &str = "/system/connections.cfg";

/// What to mount and how.
#[derive(Clone, Default)]
pub struct Target {
    pub host: String,
    pub port: u16,
    /// Empty — only a login to the computer (to see its folders).
    pub share: String,
    /// A folder inside the resource, starting with "/"; empty — the whole resource.
    pub folder: String,
    pub domain: String,
    pub user: String,
    pub password: String,
    /// Name in /media.
    pub name: String,
    /// The address the computer was last seen at: the name may not resolve.
    pub last_ip: String,
    /// Mount at boot.
    pub auto: bool,
}

impl Target {
    fn to_line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            if self.auto { "auto" } else { "manual" },
            self.host,
            self.port,
            self.share,
            self.name,
            self.last_ip,
            self.domain,
            self.user,
            self.password,
            self.folder
        )
    }

    fn parse(line: &str) -> Option<Target> {
        let f: Vec<&str> = line.split('\t').collect();
        // Records from old versions: without a folder inside the resource.
        let [auto, host, port, share, name, last_ip, domain, user, password, ref rest @ ..] = f[..] else { return None };
        Some(Target {
            folder: rest.first().copied().unwrap_or("").into(),
            host: host.into(),
            port: port.parse().ok()?,
            share: share.into(),
            domain: domain.into(),
            user: user.into(),
            password: password.into(),
            name: name.into(),
            last_ip: last_ip.into(),
            auto: auto == "auto",
        })
    }

    fn same(&self, other: &Target) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.port == other.port
            && self.share.eq_ignore_ascii_case(&other.share)
            && self.folder.eq_ignore_ascii_case(&other.folder)
    }

    /// Reconnects: by name, and if it does not resolve — by the old address.
    fn connect(&self) -> Result<Smb, String> {
        let address = resolve(&self.host).map(ip_str).unwrap_or_else(|| self.last_ip.clone());
        let address = if address.is_empty() { &self.host } else { &address };
        Smb::connect(address, self.port, &self.share, &self.user, &self.password, &self.domain)
    }
}

fn saved() -> Vec<Target> {
    let text = crate::task::as_system(|| crate::fs::read(CONNECTIONS)).map(|d| String::from_utf8_lossy(&d).into_owned()).unwrap_or_default();
    text.lines().filter_map(Target::parse).collect()
}

fn store(list: &[Target]) {
    let text: String = list.iter().map(Target::to_line).collect();
    crate::task::as_system(|| {
        let _ = crate::fs::mkdir("/system");
        let _ = crate::fs::write(CONNECTIONS, text.as_bytes());
    });
}

fn remember(t: &Target) {
    let mut list = saved();
    list.retain(|x| !x.same(t));
    list.push(t.clone());
    store(&list);
}

/// Logins the user asked not to remember: they live until shutdown.
static LOGINS: IrqLock<Vec<Target>> = IrqLock::new(Vec::new());

/// The login for a computer: from logging in to it or from any of its mounted folders.
pub fn login_for(host: &str, port: u16) -> Option<Target> {
    let same = |t: &&Target| t.host.eq_ignore_ascii_case(host) && t.port == port && !t.user.is_empty();
    if let Some(login) = LOGINS.lock().iter().find(same) {
        return Some(login.clone());
    }
    let list = saved();
    list.iter().find(|t| same(t) && t.share.is_empty()).or_else(|| list.iter().find(same)).cloned()
}

/// Whether the error means "ask for a user name and password".
pub fn is_login_error(e: &str) -> bool {
    e.contains("password") || e.contains("denied") || e.contains("login failed")
}

/// The computer's folders: as a guest or with the remembered login.
pub fn list_shares(host: &str, ip: Ip, port: u16) -> Result<Vec<String>, String> {
    let (user, password, domain) = login_for(host, port).map_or_else(Default::default, |t| (t.user, t.password, t.domain));
    let address = ip_str(ip);
    Smb::connect(&address, port, "IPC$", &user, &password, &domain)?.share_list(&address)
}

/// Remembers the computer's login if its folders can be read with it.
pub fn log_in(host: &str, ip: Ip, port: u16, user: &str, password: &str, domain: &str, keep: bool) -> Result<(), String> {
    let address = ip_str(ip);
    Smb::connect(&address, port, "IPC$", user, password, domain)?.share_list(&address)?;
    let login = Target { host: host.into(), port, domain: domain.into(), user: user.into(), password: password.into(), last_ip: address, ..Default::default() };
    if keep {
        remember(&login);
    } else {
        let mut logins = LOGINS.lock();
        logins.retain(|t| !t.same(&login));
        logins.push(login);
    }
    super::discovery::login_changed(ip, port);
    Ok(())
}

/// The volume's link to the server; a background thread reconnects, and the volume picks up the ready
/// connection on the next operation.
struct Link {
    target: Target,
    ready: IrqLock<Option<Smb>>,
    online: AtomicBool,
    trying: AtomicBool,
    closed: AtomicBool,
}

impl Link {
    /// Starts a reconnect (unless one is already running).
    fn reconnect(self: &Arc<Self>) {
        if self.trying.swap(true, Ordering::AcqRel) {
            return;
        }
        let link = self.clone();
        crate::task::spawn("smb-reconnect", None, move || {
            let mut pause = 5 * HZ;
            while !link.closed.load(Ordering::Relaxed) {
                match link.target.connect() {
                    Ok(smb) => {
                        *link.ready.lock() = Some(smb);
                        link.online.store(true, Ordering::Relaxed);
                        kprintln!("smb: \\\\{}\\{} is back", link.target.host, link.target.share);
                        crate::fs::touch();
                        break;
                    }
                    Err(e) => kprintln!("smb: \\\\{}\\{}: {e}", link.target.host, link.target.share),
                }
                crate::task::sleep_until(crate::interrupts::ticks() + pause);
                pause = (pause * 2).min(60 * HZ);
            }
            link.trying.store(false, Ordering::Release);
        });
    }
}

/// A network folder as a volume: the connection may drop and come back, the path stays.
struct Share {
    conn: Option<Smb>,
    link: Arc<Link>,
    label: String,
    space: (u64, u64),
    /// Only this folder of the resource is mounted (empty — the whole resource).
    folder: String,
}

impl Share {
    fn new(t: &Target, conn: Option<Smb>) -> Self {
        let link = Arc::new(Link {
            target: t.clone(),
            ready: IrqLock::new(None),
            online: AtomicBool::new(conn.is_some()),
            trying: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        let space = conn.as_ref().map_or((0, 0), |c| c.space);
        Self { conn, link, label: format!("{} on {}", t.name, t.host), space, folder: t.folder.clone() }
    }

    /// The path on the server.
    fn at(&self, path: &str) -> String {
        format!("{}{path}", self.folder)
    }

    fn run<T>(&mut self, mut f: impl FnMut(&mut Smb) -> crate::fs::Result<T>) -> crate::fs::Result<T> {
        if let Some(new) = self.link.ready.lock().take() {
            self.space = new.space;
            self.conn = Some(new);
        }
        let Some(conn) = self.conn.as_mut() else {
            self.link.reconnect();
            return Err(FsError::Io);
        };
        let r = f(conn);
        if !conn.broken {
            return r;
        }
        // The server closed an idle connection: it is alive, retry right away.
        // We do not wait for a silent one — reconnect in the background.
        let closed = conn.closed;
        self.conn = None;
        if closed && let Ok(mut new) = self.link.target.connect() {
            let r = f(&mut new);
            self.conn = Some(new);
            return r;
        }
        self.link.online.store(false, Ordering::Relaxed);
        kprintln!("smb: lost \\\\{}\\{}", self.link.target.host, self.link.target.share);
        self.link.reconnect();
        crate::fs::touch();
        r
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        self.link.closed.store(true, Ordering::Relaxed);
    }
}

impl Filesystem for Share {
    fn list(&mut self, path: &str) -> crate::fs::Result<Vec<Entry>> {
        let path = self.at(path);
        self.run(|c| c.list(&path))
    }
    fn read(&mut self, path: &str) -> crate::fs::Result<Vec<u8>> {
        let path = self.at(path);
        self.run(|c| c.read(&path))
    }
    fn read_range(&mut self, path: &str, offset: u64, len: usize) -> crate::fs::Result<(Vec<u8>, u64)> {
        let path = self.at(path);
        self.run(|c| c.read_range(&path, offset, len))
    }
    fn is_dir(&mut self, path: &str) -> Option<bool> {
        if smb_path(path).is_empty() {
            return Some(true);
        }
        let path = self.at(path);
        self.run(|c| c.is_dir(&path).ok_or(FsError::NotFound)).ok()
    }
    fn write(&mut self, path: &str, data: &[u8]) -> crate::fs::Result<()> {
        let path = self.at(path);
        self.run(|c| c.write(&path, data))
    }
    fn write_at(&mut self, path: &str, offset: u64, data: &[u8]) -> crate::fs::Result<()> {
        let path = self.at(path);
        self.run(|c| c.write_at(&path, offset, data))
    }
    fn shrink(&mut self, path: &str, len: u64) -> crate::fs::Result<()> {
        let path = self.at(path);
        self.run(|c| c.shrink(&path, len))
    }
    fn mkdir(&mut self, path: &str) -> crate::fs::Result<()> {
        let path = self.at(path);
        self.run(|c| c.mkdir(&path))
    }
    fn remove(&mut self, path: &str) -> crate::fs::Result<()> {
        let path = self.at(path);
        self.run(|c| c.remove(&path))
    }
    fn rename(&mut self, from: &str, to: &str) -> crate::fs::Result<()> {
        let (from, to) = (self.at(from), self.at(to));
        self.run(|c| c.rename(&from, &to))
    }
    fn usage(&self) -> (u64, u64) {
        self.space
    }
    fn has_changes(&self) -> bool {
        false
    }
    fn take_dirty(&mut self) -> Vec<(u32, Box<[u8; crate::ata::SECTOR]>)> {
        Vec::new()
    }
    fn set_writing(&mut self, _on: bool) {}
    fn trim(&mut self) {}
    fn label(&self) -> &str {
        &self.label
    }
    fn dev(&self) -> usize {
        usize::MAX
    }
    fn kind(&self) -> &'static str {
        "SMB2"
    }
    fn online(&self) -> bool {
        self.link.online.load(Ordering::Relaxed)
    }
}

/// Folders mounted now: (path, what is mounted).
static ACTIVE: IrqLock<Vec<(String, Target)>> = IrqLock::new(Vec::new());

/// A volume from a connection (None — no link yet, it will come up in the background).
fn attach(t: Target, conn: Option<Smb>) -> String {
    let online = conn.is_some();
    let share = Share::new(&t, conn);
    let link = share.link.clone();
    let path = crate::fs::mount_network(&t.name, Box::new(share));
    ACTIVE.lock().push((path.clone(), t));
    if !online {
        link.reconnect();
    }
    path
}

/// A network neighbourhood resource is opened for browsing only: it is not in /media or in the
/// disk list, and its folders are read via the path /network/group/computer/resource/...
pub fn browse(path: &str) -> Result<(), String> {
    let (key, _) = super::discovery::share_of(path).ok_or("not a shared folder")?;
    if crate::fs::is_browsing(path) {
        return Ok(());
    }
    let (host, ip, port) = super::discovery::host_of(path).ok_or("computer not found")?;
    let share = String::from(crate::fs::name(key));
    let login = login_for(&host, port).unwrap_or_default();
    let t = Target { name: share.clone(), share, host, port, last_ip: ip_str(ip), ..login };
    let conn = t.connect()?;
    crate::fs::add_browsed(key, Box::new(Share::new(&t, Some(conn))));
    Ok(())
}

/// Mounts a folder. An already mounted one is not duplicated. With `t.auto` it is remembered
/// and mounted on the next boot.
pub fn connect(mut t: Target) -> Result<String, String> {
    if t.share.is_empty() {
        return Err("no share name: smb://server/share".into());
    }
    if let Some(path) = mounted(&t.host, t.port, &t.share, &t.folder) {
        return Ok(path);
    }
    if t.name.is_empty() {
        t.name = String::from(crate::fs::name(if t.folder.is_empty() { &t.share } else { &t.folder }));
    }
    let mut conn = t.connect()?;
    if !t.folder.is_empty() && conn.is_dir(&t.folder) != Some(true) {
        return Err(format!("no such folder: {}", t.folder));
    }
    if t.last_ip.is_empty() {
        t.last_ip = resolve(&t.host).map(ip_str).unwrap_or_default();
    }
    if t.auto {
        remember(&t);
    }
    Ok(attach(t, Some(conn)))
}

/// The folder's path, if it is already mounted.
pub fn mounted(host: &str, port: u16, share: &str, folder: &str) -> Option<String> {
    let probe = Target { host: host.into(), port, share: share.into(), folder: folder.into(), ..Default::default() };
    ACTIVE.lock().iter().find(|(_, t)| t.same(&probe)).map(|(p, _)| p.clone())
}

/// The folder was unmounted: no longer mount it at boot.
pub fn forget(path: &str) {
    let mut active = ACTIVE.lock();
    let Some(i) = active.iter().position(|(p, _)| p == path) else { return };
    let (_, t) = active.remove(i);
    drop(active);
    let mut list = saved();
    list.retain(|x| !x.same(&t));
    store(&list);
}

/// Whether the folder is mounted at boot.
pub fn is_auto(path: &str) -> bool {
    ACTIVE.lock().iter().any(|(p, t)| p == path && t.auto)
}

/// The "mount at boot" checkbox of a mounted folder.
pub fn set_auto(path: &str, auto: bool) {
    let mut active = ACTIVE.lock();
    let Some((_, t)) = active.iter_mut().find(|(p, _)| p == path) else { return };
    t.auto = auto;
    let t = t.clone();
    drop(active);
    if auto {
        remember(&t);
    } else {
        let mut list = saved();
        list.retain(|x| !x.same(&t));
        store(&list);
    }
}

/// At boot: remembered folders appear in /media right away, the connection comes up in the background.
pub fn restore() {
    for t in saved().into_iter().filter(|t| t.auto && !t.share.is_empty()) {
        kprintln!("smb: restoring \\\\{}\\{}", t.host, t.share);
        attach(t, None);
    }
}

/// Mounts `smb://[domain;]user[:password]@server[:port]/share` in /media.
/// Returns the mount path.
pub fn mount(url: &str, name: Option<&str>) -> Result<String, String> {
    connect(parse_url(url, name)?)
}

/// Parses an address into a mount target.
pub fn parse_url(url: &str, name: Option<&str>) -> Result<Target, String> {
    let rest = url.strip_prefix("smb://").or_else(|| url.strip_prefix("//")).unwrap_or(url);
    let (cred, rest) = match rest.rsplit_once('@') {
        Some((c, r)) => (c, r),
        None => ("", rest),
    };
    let (host_port, share) = rest.split_once('/').unwrap_or((rest, ""));
    let (share, folder) = match share.trim_end_matches('/').split_once('/') {
        Some((share, folder)) => (share, format!("/{folder}")),
        None => (share.trim_end_matches('/'), String::new()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| format!("bad port: {p}"))?),
        None => (host_port, 445),
    };
    let (user, password) = cred.split_once(':').unwrap_or((cred, ""));
    let (domain, user) = user.split_once(';').or_else(|| user.split_once('\\')).unwrap_or(("", user));
    Ok(Target {
        host: host.into(),
        port,
        share: share.into(),
        domain: domain.into(),
        user: user.into(),
        password: password.into(),
        name: name.unwrap_or(crate::fs::name(if folder.is_empty() { share } else { &folder })).into(),
        folder,
        ..Default::default()
    })
}
