//! Remote access: a small REST API over HTTP on the local network. From another computer
//! you can run terminal commands, read the kernel log and files, take screenshots,
//! install a trial kernel and reboot — without touching the machine. The client is tools/remote.py.
//!
//! Off by default. An administrator turns it on (Settings → Remote Access or
//! `remote on`), and the machine creates a key. Every request is signed with it: HMAC-SHA256
//! over the method, path, time, a random nonce and the body hash. The body hash goes in a
//! header, so the signature is checked before the body is read. The response is signed
//! too. Nothing can be done without the key, and a captured request cannot be replayed; the traffic
//! is not encrypted, though, so this is for your own network only. Commands run with the rights
//! of the administrator who enabled access.
//!
//! The actions are separate functions and HTTP is only a wrapper: later a proper
//! protocol with encryption, a screen stream and input can call the same functions.
//!
//!   GET  /info                        what machine this is and which kernel runs
//!   POST /run?cwd=&wait=              a terminal line in the body, the answer is its output
//!   GET  /log?from=N                  the kernel log after position N
//!   GET  /list?path=                  a folder: "d name" and "f size name"
//!   GET  /file?path=                  a file; PUT /file?path=[&parents=1] — write it
//!   GET  /shot?screen=N               a screenshot (PNG)
//!   PUT  /kernel                      a trial kernel (\EFI\GLASSOS\TRIAL.EFI)
//!   POST /kernel/keep                 make the running trial kernel the main one
//!   POST /reboot?into=normal|trial|bios|usb

use super::{Ip, TcpListener, TcpStream, ip_str};
use crate::bootvars::{self, HardDrive, Next};
use crate::crypto::{hmac_sha256, random32, sha256};
use crate::fs;
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::sync::IrqLock;
use crate::task;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "debug-tools")]
mod input;

pub const CONFIG: &str = "/system/remote.cfg";
/// The key: a secret only the kernel reads.
pub const KEY: &str = "/system/remote.key";
pub const DEFAULT_PORT: u16 = 7070;
/// Key length: 160 bits, 32 base32 characters.
const KEY_BYTES: usize = 20;
const HEADER_MAX: usize = 16 * 1024;
const BODY_MAX: usize = 128 << 20;
const HEADER_WAIT: u64 = 10 * HZ;
/// How long to wait for the next piece of the body.
const BODY_IDLE: u64 = 15 * HZ;
/// How long to wait for the whole response to be sent.
const SEND_WAIT: u64 = 120 * HZ;
/// How far the client's clock may differ from ours, seconds.
const CLOCK_SKEW: i64 = 120;
const RUN_WAIT: u64 = 30;
const RUN_WAIT_MAX: u64 = 600;
const TRIAL_DIR: &str = "/EFI/GLASSOS";
const TRIAL_FILE: &str = "/EFI/GLASSOS/TRIAL.EFI";
const MAIN_KERNEL: &str = "/EFI/BOOT/BOOTX64.EFI";
const OLD_KERNEL: &str = "/EFI/BOOT/OLD.EFI";
/// The smallest kernel that looks real.
const KERNEL_MIN: usize = 64 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub port: u16,
    /// The administrator whose rights commands run with.
    pub user: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: false, port: DEFAULT_PORT, user: String::new() }
    }
}

static SETTINGS: IrqLock<Option<Settings>> = IrqLock::new(None);
static KEY_CACHE: IrqLock<Option<Vec<u8>>> = IrqLock::new(None);
static STATUS: IrqLock<String> = IrqLock::new(String::new());
/// The last client that passed the check: address and tick.
static LAST: IrqLock<Option<(Ip, u64)>> = IrqLock::new(None);
/// Who has connected since boot: a new one is announced with a notification.
static CLIENTS: IrqLock<Vec<Ip>> = IrqLock::new(Vec::new());
/// Nonces of recent requests (the start of the hash) and their time: a replay will not pass.
static SEEN: IrqLock<Vec<([u8; 8], i64)>> = IrqLock::new(Vec::new());
/// The trial kernel has already been made the main one: it starts from the next boot.
static KEPT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static VERSION: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------- settings and key

pub fn version() -> u64 {
    VERSION.load(Ordering::Relaxed)
}

fn changed() {
    VERSION.fetch_add(1, Ordering::Relaxed);
}

fn parse(text: &str) -> Settings {
    let mut s = Settings::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            "enabled" => s.enabled = value.trim() == "1",
            "port" => s.port = value.trim().parse().unwrap_or(DEFAULT_PORT),
            "user" => s.user = value.trim().into(),
            _ => {}
        }
    }
    s
}

pub fn settings() -> Settings {
    if let Some(s) = SETTINGS.lock().clone() {
        return s;
    }
    let data = task::as_system(|| fs::read(CONFIG).ok()).unwrap_or_default();
    let s = parse(&String::from_utf8_lossy(&data));
    *SETTINGS.lock() = Some(s.clone());
    s
}

fn save(s: &Settings) -> Result<(), &'static str> {
    *SETTINGS.lock() = Some(s.clone());
    changed();
    let text = format!("enabled={}\nport={}\nuser={}\n", s.enabled as u8, s.port, s.user);
    task::as_system(|| {
        let _ = fs::mkdir("/system");
        fs::write(CONFIG, text.as_bytes()).map_err(|_| "cannot save the remote access settings")
    })
}

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The key for a human: base32 in groups of four separated by dashes.
fn key_words(key: &[u8]) -> String {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = String::new();
    for &b in key {
        bits = bits << 8 | b as u32;
        count += 8;
        while count >= 5 {
            count -= 5;
            out.push(BASE32[(bits >> count & 31) as usize] as char);
        }
    }
    if count > 0 {
        out.push(BASE32[(bits << (5 - count) & 31) as usize] as char);
    }
    let groups: Vec<&str> = out.as_bytes().chunks(4).map(|c| core::str::from_utf8(c).unwrap_or("")).collect();
    groups.join("-")
}

fn key_bytes(text: &str) -> Option<Vec<u8>> {
    let (mut bits, mut count, mut out) = (0u32, 0, Vec::new());
    for c in text.chars().filter(|c| c.is_ascii_alphanumeric()) {
        let v = BASE32.iter().position(|&b| b == c.to_ascii_uppercase() as u8)? as u32;
        bits = bits << 5 | v;
        count += 5;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    (out.len() == KEY_BYTES).then_some(out)
}

fn load_key() -> Option<Vec<u8>> {
    if let Some(k) = KEY_CACHE.lock().clone() {
        return Some(k);
    }
    let data = task::as_system(|| fs::read(KEY).ok())?;
    let key = key_bytes(&String::from_utf8_lossy(&data))?;
    *KEY_CACHE.lock() = Some(key.clone());
    Some(key)
}

fn make_key() -> Result<String, &'static str> {
    let key = random32()[..KEY_BYTES].to_vec();
    let words = key_words(&key);
    task::as_system(|| {
        let _ = fs::mkdir("/system");
        fs::write(KEY, format!("{words}\n").as_bytes()).map_err(|_| "cannot save the key")
    })?;
    let _ = fs::sync();
    *KEY_CACHE.lock() = Some(key);
    changed();
    Ok(words)
}

/// Turns access on or off. Commands will run with the rights of whoever enabled it.
pub fn set_enabled(on: bool) -> Result<(), &'static str> {
    if !crate::access::is_admin() {
        return Err("only an administrator can change remote access");
    }
    let mut s = settings();
    s.enabled = on;
    if on {
        s.user = crate::access::user().ok_or("sign in as an administrator first")?;
        if load_key().is_none() {
            make_key()?;
        }
    }
    save(&s)
}

/// The key for the client (administrators only).
pub fn key_text() -> Result<String, &'static str> {
    if !crate::access::is_admin() {
        return Err("only an administrator can see the key");
    }
    load_key().map(|k| key_words(&k)).ok_or("there is no key yet: remote on makes one")
}

/// A new key: the previous one stops working.
pub fn new_key() -> Result<String, &'static str> {
    if !crate::access::is_admin() {
        return Err("only an administrator can change the key");
    }
    make_key()
}

pub fn status() -> String {
    STATUS.lock().clone()
}

fn set_status(text: &str) {
    let mut s = STATUS.lock();
    if *s != text {
        *s = text.into();
        drop(s);
        changed();
    }
}

/// Who connected last: address and how many seconds ago.
pub fn last_client() -> Option<(Ip, u64)> {
    LAST.lock().map(|(ip, at)| (ip, (interrupts::ticks() - at) / HZ))
}

pub fn requests() -> u64 {
    REQUESTS.load(Ordering::Relaxed)
}

pub fn refused() -> u64 {
    REFUSED.load(Ordering::Relaxed)
}

/// A trial kernel is running (booted once through BootNext).
pub fn trial() -> bool {
    crate::install::BOOT_PATH.lock().eq_ignore_ascii_case(bootvars::TRIAL_KERNEL)
}

/// Whether there was a request recently: the indicator in the top bar.
pub fn active() -> bool {
    LAST.lock().is_some_and(|(_, at)| interrupts::ticks() - at < 60 * HZ)
}

/// State for the terminal.
pub fn describe() -> Vec<String> {
    let s = settings();
    let mut out = Vec::new();
    let state = status();
    let address = super::subnet().map(|(ip, _, _)| format!("http://{}:{}", ip_str(ip), s.port));
    match (s.enabled, state.as_str(), address) {
        (false, _, _) => out.push("remote access: off (remote on turns it on)".into()),
        (true, "off", _) => out.push("remote access: turning on, see remote in a second".into()),
        (true, "on", Some(a)) => out.push(format!("remote access: on at {a}, commands run as {}", s.user)),
        (true, other, _) => out.push(format!("remote access: {other}")),
    }
    if let Some((ip, ago)) = last_client() {
        out.push(format!("last client: {}, {ago} s ago; {} requests, {} refused", ip_str(ip), requests(), refused()));
    }
    let kernel = crate::install::BOOT_PATH.lock().clone();
    let note = match (trial(), KEPT.load(Ordering::Relaxed)) {
        (true, true) => " (trial, kept: it starts by default from the next restart)",
        (true, false) => " (trial: remote keep makes it the main one)",
        _ => "",
    };
    out.push(format!("kernel: {kernel}{note}"));
    out.push(format!("build: {}", crate::crash::BUILD));
    out
}

// ---------------------------------------------------------------- server

/// Server thread: listens on the port while access is enabled and there is a network.
/// The trial kernel's log on disk: if it hangs, after a restart the main kernel
/// serves it through GET /file?path=/system/trial.log — you can see where everything stopped.
/// Written once a second for the first three minutes.
const TRIAL_LOG: &str = "/system/trial.log";
const TRIAL_LOG_SECONDS: u64 = 180;
const TRIAL_LOG_EVERY: u64 = 1;

fn journal_trial() {
    if !trial() {
        return;
    }
    task::spawn("trial-log", None, || {
        for _ in 0..TRIAL_LOG_SECONDS / TRIAL_LOG_EVERY {
            let text = crate::serial::log_text();
            task::as_system(|| {
                if fs::write(TRIAL_LOG, text.as_bytes()).is_ok() {
                    let _ = fs::sync();
                }
            });
            task::sleep_until(interrupts::ticks() + TRIAL_LOG_EVERY * HZ);
        }
    });
}

pub fn start() {
    journal_trial();
    task::spawn("remote", None, || {
        let mut listener: Option<(u16, TcpListener)> = None;
        // Whether that administrator still exists: the accounts file is read once every 5 seconds.
        let mut admin: (String, u64, bool) = (String::new(), 0, false);
        loop {
            let s = settings();
            let online = super::subnet().is_some();
            if !s.enabled || !online {
                listener = None;
                set_status(if !s.enabled { "off" } else { "waiting for the network" });
                task::sleep_until(interrupts::ticks() + HZ / 2);
                continue;
            }
            if listener.as_ref().is_none_or(|(port, _)| *port != s.port) {
                listener = None;
                match TcpListener::bind(s.port) {
                    Ok(l) => {
                        kprintln!("remote: listening on port {}", s.port);
                        listener = Some((s.port, l));
                    }
                    Err(e) => {
                        set_status(&format!("port {}: {e}", s.port));
                        task::sleep_until(interrupts::ticks() + 2 * HZ);
                        continue;
                    }
                }
            }
            let now = interrupts::ticks();
            if admin.0 != s.user || now >= admin.1 + 5 * HZ {
                admin = (s.user.clone(), now, crate::users::find(&s.user).is_some_and(|a| a.admin));
            }
            set_status(if admin.2 { "on" } else { "no administrator to run commands as" });
            let Some((_, l)) = &listener else { continue };
            if let Some(stream) = l.accept(HZ / 2) {
                if admin.2 {
                    let cred = crate::access::for_remote(&s.user, true);
                    task::spawn("remote-conn", None, move || {
                        task::set_cred(Some(cred));
                        serve(stream);
                    });
                } else {
                    plain(&stream, 503, "no administrator to run commands as", &[]);
                }
            }
        }
    });
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison: how many bytes matched is not visible.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// For a header: everything except visible ASCII becomes %XX.
fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_graphic() && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// An address from our own network (or the machine itself).
fn local(peer: Ip) -> bool {
    peer == [127, 0, 0, 1] || super::on_local_net(peer)
}

/// A request header: the request line and what is needed to check the signature.
struct Head {
    method: String,
    target: String,
    path: String,
    query: Vec<(String, String)>,
    length: usize,
    time: i64,
    nonce: String,
    body_hash: String,
    sign: String,
    /// What arrived together with the header: the start of the body.
    rest: Vec<u8>,
}

impl Head {
    fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn read_head(stream: &TcpStream) -> Result<Head, (u16, &'static str)> {
    let mut data = Vec::new();
    let end = interrupts::ticks() + HEADER_WAIT;
    let at = loop {
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if data.len() > HEADER_MAX {
            return Err((400, "the request head is too long"));
        }
        let now = interrupts::ticks();
        if now >= end {
            return Err((400, "the request did not arrive in time"));
        }
        match stream.recv(end - now) {
            Ok(d) if !d.is_empty() => data.extend_from_slice(&d),
            _ => return Err((400, "the connection closed")),
        }
    };
    let text = String::from_utf8_lossy(&data[..at]).into_owned();
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    let mut head = Head {
        path: percent_decode(path),
        method,
        query,
        target: target.clone(),
        length: 0,
        time: 0,
        nonce: String::new(),
        body_hash: String::new(),
        sign: String::new(),
        rest: data[at + 4..].to_vec(),
    };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => head.length = value.parse().map_err(|_| (400, "bad Content-Length"))?,
            "x-glass-time" => head.time = value.parse().unwrap_or(0),
            "x-glass-nonce" => head.nonce = value.into(),
            "x-glass-body" => head.body_hash = value.to_ascii_lowercase(),
            "x-glass-sign" => head.sign = value.to_ascii_lowercase(),
            _ => {}
        }
    }
    if head.length > BODY_MAX {
        return Err((413, "the body is too large"));
    }
    Ok(head)
}

/// Checks the header signature: method, path, time, nonce and body hash.
fn verify(head: &Head, key: &[u8]) -> Result<(), &'static str> {
    if head.nonce.len() < 16 || head.nonce.len() > 64 || head.body_hash.len() != 64 || head.sign.len() != 64 {
        return Err("the request is not signed");
    }
    let now = crate::time::unix();
    if (now - head.time).abs() > CLOCK_SKEW {
        return Err("the clocks differ too much");
    }
    // Nonces are remembered until reboot: a request older than this boot cannot be replayed.
    let started = now - (interrupts::ticks() / HZ) as i64;
    if head.time < started - 2 {
        return Err("the request is older than this start");
    }
    let text = format!("{}\n{}\n{}\n{}\n{}", head.method, head.target, head.time, head.nonce, head.body_hash);
    if !same(hex(&hmac_sha256(key, text.as_bytes())).as_bytes(), head.sign.as_bytes()) {
        return Err("wrong signature");
    }
    let id: [u8; 8] = sha256(head.nonce.as_bytes())[..8].try_into().unwrap_or([0; 8]);
    let mut seen = SEEN.lock();
    seen.retain(|(_, at)| now - at <= 2 * CLOCK_SKEW);
    if seen.iter().any(|(n, _)| *n == id) {
        return Err("this request was already used");
    }
    seen.push((id, now));
    Ok(())
}

fn read_body(stream: &TcpStream, head: &mut Head) -> Result<Vec<u8>, (u16, &'static str)> {
    let mut body = core::mem::take(&mut head.rest);
    while body.len() < head.length {
        match stream.recv(BODY_IDLE) {
            Ok(d) if !d.is_empty() => body.extend_from_slice(&d),
            _ => return Err((400, "the body did not arrive")),
        }
    }
    body.truncate(head.length);
    Ok(body)
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

/// An unsigned response: for requests that failed the check.
fn plain(stream: &TcpStream, code: u16, text: &str, headers: &[(&str, String)]) {
    let body = format!("{text}\n");
    let mut head = format!("HTTP/1.1 {code} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n", reason(code), body.len());
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.send(head.as_bytes());
    let _ = stream.send(body.as_bytes());
    stream.flush(HZ);
}

pub struct Response {
    code: u16,
    kind: &'static str,
    /// Our own headers (X-Glass-…): they are part of the response signature.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// What to do once the response is sent.
    then: Option<Next>,
}

impl Response {
    fn text(code: u16, text: impl Into<String>) -> Self {
        let mut body: String = text.into();
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        Self { code, kind: "text/plain; charset=utf-8", headers: Vec::new(), body: body.into_bytes(), then: None }
    }

    fn bytes(kind: &'static str, body: Vec<u8>) -> Self {
        Self { code: 200, kind, headers: Vec::new(), body, then: None }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), percent_encode(&value.into())));
        self
    }
}

/// A signed response: status, request nonce, our headers in alphabetical order and the body hash.
fn send(stream: &TcpStream, key: &[u8], nonce: &str, r: &Response) {
    let mut own: Vec<(String, &str)> = r.headers.iter().map(|(n, v)| (n.to_ascii_lowercase(), v.as_str())).collect();
    own.sort();
    let own: Vec<String> = own.iter().map(|(n, v)| format!("{n}:{v}")).collect();
    let text = format!("{}\n{nonce}\n{}\n{}", r.code, own.join("\n"), hex(&sha256(&r.body)));
    let sign = hex(&hmac_sha256(key, text.as_bytes()));
    let mut head = format!("HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n", r.code, reason(r.code), r.kind, r.body.len());
    for (name, value) in &r.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("X-Glass-Sign: {sign}\r\n\r\n"));
    let _ = stream.send(head.as_bytes());
    let _ = stream.send(&r.body);
}

/// A new client: say so on screen.
fn note_client(peer: Ip) {
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    *LAST.lock() = Some((peer, interrupts::ticks()));
    changed();
    let mut clients = CLIENTS.lock();
    if !clients.contains(&peer) {
        clients.push(peer);
        drop(clients);
        kprintln!("remote: {} connected", ip_str(peer));
        crate::wm::notify("Remote access", &format!("{} connected and can run commands here.", ip_str(peer)));
    }
}

fn serve(stream: TcpStream) {
    let Some(peer) = stream.peer() else { return };
    if !local(peer) {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        plain(&stream, 403, "remote access works only from this network", &[]);
        return;
    }
    let mut head = match read_head(&stream) {
        Ok(h) => h,
        Err((code, why)) => return plain(&stream, code, why, &[]),
    };
    let Some(key) = load_key() else {
        return plain(&stream, 503, "there is no key yet", &[]);
    };
    if let Err(why) = verify(&head, &key) {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        changed();
        kprintln!("remote: {} refused: {why}", ip_str(peer));
        // Our time — so a client with a drifting clock can correct itself.
        return plain(&stream, 401, why, &[("X-Glass-Now", crate::time::unix().to_string())]);
    }
    let body = match read_body(&stream, &mut head) {
        Ok(b) => b,
        Err((code, why)) => return plain(&stream, code, why, &[]),
    };
    if hex(&sha256(&body)) != head.body_hash {
        return plain(&stream, 400, "the body does not match its hash", &[]);
    }
    note_client(peer);
    let response = route(&head, body);
    send(&stream, &key, &head.nonce, &response);
    stream.flush(SEND_WAIT);
    if let Some(next) = response.then {
        kprintln!("remote: restart asked by {}", ip_str(peer));
        crate::reboot_into(next);
    }
}

fn route(head: &Head, body: Vec<u8>) -> Response {
    match (head.method.as_str(), head.path.as_str()) {
        ("GET", "/info") => info(),
        ("POST", "/run") => run(head, &body),
        ("GET", "/log") => log(head),
        ("GET", "/list") => list(head),
        ("GET", "/file") => get_file(head),
        ("PUT", "/file") => put_file(head, &body),
        ("GET", "/shot") => shot(head),
        ("PUT", "/kernel") => put_kernel(&body),
        ("POST", "/kernel/keep") => keep_kernel(),
        ("POST", "/reboot") => reboot(head),
        // Debugging: keyboard and mouse from another computer (see src/debug.rs).
        #[cfg(feature = "debug-tools")]
        ("POST", "/input") => input::handle(&body),
        _ => Response::text(404, format!("no {} {} here; GET /info lists what there is", head.method, head.path)),
    }
}

// ---------------------------------------------------------------- actions

fn info() -> Response {
    let id = super::identity::get();
    let ip = super::subnet().map_or(String::from("none"), |(ip, _, _)| ip_str(ip));
    let screens: Vec<String> = crate::output::sizes().iter().map(|(w, h)| format!("{w}x{h}")).collect();
    let mode = crate::output::monitor_mode().map_or(String::new(), |m| format!(" (own mode {}x{}@{})", m.w, m.h, m.hz));
    let lines = [
        format!("name: {}", id.name),
        format!("system: {}", fs::OS_VERSION.trim_end()),
        format!("kernel: {}", crate::install::BOOT_PATH.lock().clone()),
        format!("build: {}", crate::crash::BUILD),
        format!("trial: {}", if !trial() { "no" } else if KEPT.load(Ordering::Relaxed) { "yes, kept" } else { "yes" }),
        format!("uptime: {} s", interrupts::ticks() / HZ),
        format!("user: {}", crate::access::user().unwrap_or_default()),
        format!("address: {ip}"),
        format!("screens: {}{mode}", screens.join(" ")),
        format!("log: {}", crate::serial::log_end()),
        "do: GET /info, POST /run, GET /log, GET /list, GET|PUT /file, GET /shot, PUT /kernel, POST /kernel/keep, POST /reboot".into(),
    ];
    Response::text(200, lines.join("\n"))
}

fn run(head: &Head, body: &[u8]) -> Response {
    let text = String::from_utf8_lossy(body).into_owned();
    let mut cwd = head.param("cwd").map_or_else(crate::access::home, String::from);
    let wait = head.param("wait").and_then(|w| w.parse().ok()).unwrap_or(RUN_WAIT).min(RUN_WAIT_MAX) * HZ;
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut out = String::new();
    for line in &lines {
        if lines.len() > 1 {
            out.push_str(&format!("$ {line}\n"));
        }
        let (text, now) = crate::apps::run_headless(line, &cwd, wait);
        out.push_str(&text);
        cwd = now;
    }
    Response::text(200, out).header("X-Glass-Cwd", cwd)
}

fn log(head: &Head) -> Response {
    let from: usize = head.param("from").and_then(|f| f.parse().ok()).unwrap_or(0);
    let (text, end) = if from == 0 { (crate::serial::log_text(), crate::serial::log_end()) } else { crate::serial::log_since(from) };
    Response::text(200, text).header("X-Glass-Log-End", end.to_string())
}

fn path_param(head: &Head) -> Result<String, Response> {
    match head.param("path") {
        Some(p) if p.starts_with('/') => Ok(p.into()),
        _ => Err(Response::text(400, "path= must be a full path, from /")),
    }
}

fn list(head: &Head) -> Response {
    let path = match path_param(head) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match fs::list(&path) {
        Ok(entries) => {
            let lines: Vec<String> = entries.iter().map(|e| if e.is_dir { format!("d\t\t{}", e.name) } else { format!("f\t{}\t{}", e.size, e.name) }).collect();
            Response::text(200, lines.join("\n"))
        }
        Err(e) => Response::text(404, format!("{path}: {e}")),
    }
}

fn get_file(head: &Head) -> Response {
    let path = match path_param(head) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match fs::read(&path) {
        Ok(data) => Response::bytes("application/octet-stream", data),
        Err(e) => Response::text(404, format!("{path}: {e}")),
    }
}

fn put_file(head: &Head, body: &[u8]) -> Response {
    let path = match path_param(head) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if head.param("parents") == Some("1") {
        for (i, _) in path.match_indices('/').skip(1) {
            let _ = fs::mkdir(&path[..i]);
        }
    }
    match fs::write(&path, body) {
        Ok(()) => Response::text(200, format!("{path}: {} bytes", body.len())),
        Err(e) => Response::text(409, format!("{path}: {e}")),
    }
}

fn shot(head: &Head) -> Response {
    let n: usize = head.param("screen").and_then(|s| s.parse().ok()).unwrap_or(0);
    let frames = crate::output::frames();
    let Some(f) = frames.get(n) else {
        return Response::text(404, format!("there are {} screens", frames.len()));
    };
    let mut px = Vec::with_capacity((f.w * f.h) as usize);
    for y in 0..f.h {
        let row = unsafe { f.base.add((y * f.stride) as usize) };
        for x in 0..f.w {
            let p = unsafe { row.add(x as usize).read_volatile() };
            px.push(if f.bgr { p & 0xFF_FFFF } else { (p & 0xFF) << 16 | (p & 0xFF00) | (p >> 16) & 0xFF });
        }
    }
    Response::bytes("image/png", crate::png::encode(f.w, f.h, &px))
}

/// The system partition on the SATA disk we booted from: for the trial kernel.
fn system_partition() -> Result<HardDrive, String> {
    let disk = fs::system_sata_disk().ok_or("GlassOS does not run from a SATA disk here")?;
    let start = fs::boot_partition_start(disk).ok_or("the system did not start from this disk")? as u64;
    let sectors = crate::ahci::disks().into_iter().find(|d| d.0 == disk).map(|d| d.1).ok_or("the disk is gone")?;
    let (_, Some(table)) = crate::parts::examine(crate::parts::Drive::Sata(disk), sectors) else {
        return Err("the partition table is not readable".into());
    };
    let p = table.parts.iter().find(|p| p.start == start).ok_or("the boot partition is not in the table")?;
    let at = table.placed(p);
    Ok(HardDrive { number: at.number, start: at.start, sectors: at.sectors, signature: at.signature, gpt: at.gpt })
}

/// Whether it looks like a kernel: a UEFI program for x86-64.
fn check_kernel(data: &[u8]) -> Result<(), &'static str> {
    const MACHINE_X64: u16 = 0x8664;
    const EFI_APPLICATION: u16 = 10;
    let u16_at = |at: usize| data.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    if data.len() < KERNEL_MIN || !data.starts_with(b"MZ") {
        return Err("this is not an EFI program");
    }
    let pe = data.get(0x3C..0x40).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if data.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("this is not an EFI program");
    }
    if u16_at(pe + 4) != Some(MACHINE_X64) || u16_at(pe + 24 + 68) != Some(EFI_APPLICATION) {
        return Err("this is not a UEFI program for x86-64");
    }
    Ok(())
}

fn put_kernel(body: &[u8]) -> Response {
    if let Err(e) = check_kernel(body) {
        return Response::text(400, e);
    }
    if let Err(e) = system_partition() {
        return Response::text(409, e);
    }
    let saved = task::as_system(|| {
        let _ = fs::mkdir(TRIAL_DIR);
        fs::write(TRIAL_FILE, body).map_err(|e| format!("{TRIAL_FILE}: {e}"))?;
        fs::sync().map_err(String::from)
    });
    match saved {
        Ok(_) => {
            kprintln!("remote: trial kernel saved, {} bytes", body.len());
            Response::text(200, format!("trial kernel saved ({} bytes); POST /reboot?into=trial starts it once", body.len()))
        }
        Err(e) => Response::text(500, e),
    }
}

/// The running trial kernel becomes the main one; the previous one stays next to it as OLD.EFI.
/// Called both over the network and by the `remote keep` command (administrator).
pub fn keep_trial() -> Result<String, String> {
    if !trial() {
        return Err("the trial kernel is not running: start it with reboot into=trial first".into());
    }
    let image = crate::install::BOOT_IMAGE.lock().clone();
    if image.is_empty() {
        return Err("the running kernel was not read at startup".into());
    }
    task::as_system(|| -> Result<(), String> {
        if let Ok(old) = fs::read(MAIN_KERNEL) {
            fs::write(OLD_KERNEL, &old).map_err(|e| format!("{OLD_KERNEL}: {e}"))?;
        }
        fs::write(MAIN_KERNEL, &image).map_err(|e| format!("{MAIN_KERNEL}: {e}"))?;
        fs::sync().map(|_| ()).map_err(String::from)
    })?;
    kprintln!("remote: the trial kernel is now the main one");
    KEPT.store(true, Ordering::Relaxed);
    crate::crash::kept();
    Ok(format!("the trial kernel is now {MAIN_KERNEL}; the previous one is {OLD_KERNEL}"))
}

fn keep_kernel() -> Response {
    match keep_trial() {
        Ok(text) => Response::text(200, text),
        Err(e) if !trial() => Response::text(409, e),
        Err(e) => Response::text(500, e),
    }
}

fn reboot(head: &Head) -> Response {
    let next = match head.param("into").unwrap_or("normal") {
        "normal" => Ok(Next::Normal),
        "trial" => {
            let present = task::as_system(|| fs::exists(TRIAL_FILE));
            if present { system_partition().map(Next::Trial) } else { Err("there is no trial kernel: PUT /kernel first".into()) }
        }
        "bios" | "setup" => {
            if bootvars::setup_supported() { Ok(Next::Setup) } else { Err("this firmware cannot open its setup on request".into()) }
        }
        "usb" => bootvars::usb_stick().map(|(hd, _)| Next::Usb(hd)),
        other => Err(format!("into={other}: normal, trial, bios or usb")),
    };
    match next {
        Ok(next) => {
            let mut r = Response::text(200, "restarting");
            r.then = Some(next);
            r
        }
        Err(e) => Response::text(409, e),
    }
}
