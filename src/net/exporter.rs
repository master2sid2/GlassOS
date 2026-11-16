//! The metrics exporter for Prometheus: a small HTTP server that on `GET /metrics`
//! serves `metrics::render()`. It is configured in Settings → Monitoring, the settings file
//! does not need to be edited by hand.

use super::{Ip, TcpListener, TcpStream, ip_str, parse_ip};
use crate::fs;
use crate::kprintln;
use crate::interrupts::{self, HZ};
use crate::sync::IrqLock;
use crate::task;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

const CONFIG: &str = "/system/exporter.cfg";
pub const DEFAULT_PORT: u16 = 9100;
/// How long to wait for a whole request and how long it may be.
const REQUEST_WAIT: u64 = 3 * HZ;
const REQUEST_MAX: usize = 4096;

/// Who may read the metrics.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Only from the same subnet.
    Local,
    Anyone,
    /// Only from the addresses in the list.
    Listed,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub port: u16,
    pub access: Access,
    pub allowed: Vec<Ip>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: false, port: DEFAULT_PORT, access: Access::Local, allowed: Vec::new() }
    }
}

static SETTINGS: IrqLock<Option<Settings>> = IrqLock::new(None);
/// What is going on: whether we listen on the port, and why not.
static STATUS: IrqLock<String> = IrqLock::new(String::new());
/// The last reader: the address and the tick.
static LAST: IrqLock<Option<(Ip, u64)>> = IrqLock::new(None);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static VERSION: AtomicU64 = AtomicU64::new(0);

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
            "access" => {
                s.access = match value.trim() {
                    "any" => Access::Anyone,
                    "list" => Access::Listed,
                    _ => Access::Local,
                }
            }
            "allow" => s.allowed = value.split(',').filter_map(|a| parse_ip(a.trim())).collect(),
            _ => {}
        }
    }
    s
}

fn text(s: &Settings) -> String {
    let access = match s.access {
        Access::Local => "local",
        Access::Anyone => "any",
        Access::Listed => "list",
    };
    let allow: Vec<String> = s.allowed.iter().map(|ip| ip_str(*ip)).collect();
    format!("enabled={}\nport={}\naccess={access}\nallow={}\n", s.enabled as u8, s.port, allow.join(","))
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

/// Changes the settings (administrators only) and saves them.
pub fn change(f: impl FnOnce(&mut Settings)) -> Result<(), &'static str> {
    if !crate::access::is_admin() {
        return Err("Only an administrator can change monitoring settings");
    }
    let mut s = settings();
    f(&mut s);
    *SETTINGS.lock() = Some(s.clone());
    changed();
    task::as_system(|| {
        let _ = fs::mkdir("/system");
        fs::write(CONFIG, text(&s).as_bytes()).map_err(|_| "cannot save monitoring settings")
    })
}

/// The status line for the settings.
pub fn status() -> String {
    STATUS.lock().clone()
}

/// Who read last: the address and how many seconds ago.
pub fn last_reader() -> Option<(Ip, u64)> {
    LAST.lock().map(|(ip, at)| (ip, (interrupts::ticks() - at) / HZ))
}

pub fn refused() -> u64 {
    REFUSED.load(Ordering::Relaxed)
}

fn set_status(text: &str) {
    let mut s = STATUS.lock();
    if *s != text {
        *s = text.into();
        drop(s);
        changed();
    }
}

/// The server thread: listens on the port while the exporter is on and there is a network.
pub fn start() {
    task::spawn("exporter", None, || {
        let mut listener: Option<(u16, TcpListener)> = None;
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
                        kprintln!("exporter: metrics on port {}", s.port);
                        listener = Some((s.port, l));
                        set_status("on");
                    }
                    Err(e) => {
                        set_status(&format!("port {}: {e}", s.port));
                        task::sleep_until(interrupts::ticks() + 2 * HZ);
                        continue;
                    }
                }
            }
            let Some((_, l)) = &listener else { continue };
            if let Some(stream) = l.accept(HZ / 2) {
                task::spawn("exporter-conn", None, move || serve(stream));
            }
        }
    });
}

fn allowed(s: &Settings, peer: Ip) -> bool {
    match s.access {
        Access::Anyone => true,
        Access::Listed => s.allowed.contains(&peer) || peer == [127, 0, 0, 1],
        Access::Local => super::subnet().is_some_and(|(ip, mask, _)| (0..4).all(|i| ip[i] & mask[i] == peer[i] & mask[i])),
    }
}

fn reply(stream: &TcpStream, status: &str, kind: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let _ = stream.send(head.as_bytes());
    let _ = stream.send(body);
}

const PAGE: &str = "<html><head><title>GlassOS metrics</title></head><body><h1>GlassOS metrics</h1><p><a href=\"/metrics\">/metrics</a></p></body></html>\n";

fn serve(stream: TcpStream) {
    let Some(peer) = stream.peer() else { return };
    let mut request = Vec::new();
    let end = interrupts::ticks() + REQUEST_WAIT;
    while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < REQUEST_MAX {
        let now = interrupts::ticks();
        if now >= end {
            return;
        }
        match stream.recv(end - now) {
            Ok(data) if !data.is_empty() => request.extend_from_slice(&data),
            _ => return,
        }
    }
    if !allowed(&settings(), peer) {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        reply(&stream, "403 Forbidden", "text/plain", b"This computer does not give metrics to your address.\n");
        return;
    }
    let line = String::from_utf8_lossy(&request[..request.iter().position(|&b| b == b'\r').unwrap_or(request.len())]).into_owned();
    let mut parts = line.split(' ');
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = path.split('?').next().unwrap_or("");
    match (method, path) {
        ("GET", "/metrics") => {
            *LAST.lock() = Some((peer, interrupts::ticks()));
            changed();
            let body = crate::metrics::render();
            reply(&stream, "200 OK", "text/plain; version=0.0.4; charset=utf-8", body.as_bytes());
        }
        ("GET", "/") => reply(&stream, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", _) => reply(&stream, "404 Not Found", "text/plain", b"Not found. Metrics are at /metrics\n"),
        _ => reply(&stream, "405 Method Not Allowed", "text/plain", b"Only GET\n"),
    }
    // Let the send go out before the connection closes.
    let _ = stream.recv(HZ / 10);
}
