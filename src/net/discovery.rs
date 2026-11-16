//! The network neighbourhood: who is on the local network and what they share.
//!
//! The discovery service constantly listens to NetBIOS, WS-Discovery, mDNS and LLMNR:
//! it answers searches, announces itself and remembers neighbours. A scan (`scan`)
//! walks the subnet: ARP finds live addresses, NBSTAT asks them for the name and
//! workgroup, port 445 checks those who are silent (Windows without NetBIOS).
//!
//! For programs this is a folder: /network/group/computer/resource. Reading the list does not
//! wait for the network — it returns what is already known and starts a scan in the background; new
//! information arrives as an ordinary file change (`fs::touch`).

use super::identity::{self, Identity};
use super::{Ip, TcpStream, UdpSocket, ip_str, mdns, nbt, parse_ip, wsd};
use crate::fs::{self, Entry, FsError};
use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::sync::IrqLock;
use crate::task;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// The workgroup of those who did not report one.
const OTHER: &str = "OTHER";
/// How often the subnet may be walked and a computer's resources re-read.
const SCAN_EVERY: u64 = 30 * HZ;
const SHARES_EVERY: u64 = 30 * HZ;
/// We do not walk more addresses: networks larger than /22 are only checked around ourselves.
const MAX_HOSTS: u32 = 1022;

/// What a neighbour reported about itself.
pub struct Found {
    pub ip: Ip,
    pub name: String,
    pub workgroup: String,
    pub smb: bool,
    pub via: &'static str,
}

#[derive(Clone, PartialEq, Eq)]
enum Shares {
    Unknown,
    Loading,
    Ready(Vec<String>),
    /// Guests are not let in: a user name and password are needed.
    NeedLogin,
    Failed(String),
}

struct Computer {
    name: String,
    workgroup: String,
    ip: Ip,
    port: u16,
    smb: bool,
    via: &'static str,
    /// The number of the walk in which it was last seen.
    seen: u32,
    /// Added manually: does not disappear if it did not answer.
    manual: bool,
    shares: Shares,
    /// When the resources were read.
    shares_at: u64,
}

struct State {
    computers: Vec<Computer>,
    scanning: bool,
    last_scan: Option<u64>,
    round: u32,
}

static STATE: IrqLock<State> = IrqLock::new(State { computers: Vec::new(), scanning: false, last_scan: None, round: 0 });
/// A request to the service to send WS-Discovery and mDNS queries.
static PROBE: AtomicBool = AtomicBool::new(false);
/// How many port 445 checks are running.
static TCP_CHECKS: AtomicU32 = AtomicU32::new(0);

fn group_of(c: &Computer) -> &str {
    if c.workgroup.is_empty() { OTHER } else { &c.workgroup }
}

fn clean(s: &str) -> String {
    s.replace('/', "_")
}

/// A neighbour responded: add or refine the entry.
pub fn found(f: Found) {
    if super::subnet().is_some_and(|(me, ..)| me == f.ip) || f.name.is_empty() {
        return;
    }
    let mut s = STATE.lock();
    let round = s.round;
    // To the log — after the lock: printing is slow, and the lock disables interrupts.
    let mut news = None;
    let changed = match s.computers.iter_mut().find(|c| c.ip == f.ip) {
        Some(c) => {
            let before = (c.name.clone(), c.workgroup.clone(), c.smb);
            // A name that is an address is replaced with the real one; a real one is left alone: NetBIOS is more accurate than mDNS.
            if parse_ip(&c.name).is_some() && parse_ip(&f.name).is_none() || (f.via != "mDNS" && parse_ip(&f.name).is_none() && !c.manual) {
                c.name = clean(&f.name);
                c.via = f.via;
            }
            if !f.workgroup.is_empty() {
                c.workgroup = clean(&f.workgroup);
            }
            c.smb |= f.smb;
            c.seen = round;
            before != (c.name.clone(), c.workgroup.clone(), c.smb)
        }
        None => {
            news = Some(alloc::format!("network: {} ({}) in {} via {}", f.name, ip_str(f.ip), f.workgroup, f.via));
            s.computers.push(Computer {
                name: clean(&f.name),
                workgroup: clean(&f.workgroup),
                ip: f.ip,
                port: 445,
                smb: f.smb,
                via: f.via,
                seen: round,
                manual: false,
                shares: Shares::Unknown,
                shares_at: 0,
            });
            true
        }
    };
    if changed {
        s.computers.sort_by_key(|c| c.name.to_lowercase());
    }
    drop(s);
    if let Some(line) = news {
        kprintln!("{line}");
    }
    if changed {
        fs::touch();
    }
}

/// A computer's address by name, if it has already been found.
pub fn ip_of(host: &str) -> Option<Ip> {
    STATE.lock().computers.iter().find(|c| c.name.eq_ignore_ascii_case(host)).map(|c| c.ip)
}

/// A computer at an address from a "server[:port]" string, added manually: visible even if
/// it does not answer searches. Returns the path of its folder.
pub fn add_host(host: &str, port: u16) -> Result<String, String> {
    let ip = super::resolve(host).ok_or_else(|| format!("{host}: host not found"))?;
    let path = {
        let mut s = STATE.lock();
        let round = s.round;
        match s.computers.iter_mut().find(|c| c.ip == ip && c.port == port) {
            Some(c) => {
                c.manual = true;
                c.shares = Shares::Unknown;
            }
            None => s.computers.push(Computer {
                name: clean(host),
                workgroup: String::new(),
                ip,
                port,
                smb: true,
                via: "manual",
                seen: round,
                manual: true,
                shares: Shares::Unknown,
                shares_at: 0,
            }),
        }
        let c = s.computers.iter().find(|c| c.ip == ip && c.port == port).unwrap();
        format!("{}/{}/{}", fs::NETWORK, group_of(c), c.name)
    };
    fs::touch();
    Ok(path)
}

// ---------------------------------------------------------------- the /network folder

fn parts(path: &str) -> Option<Vec<&str>> {
    let rest = path.strip_prefix(fs::NETWORK)?;
    (rest.is_empty() || rest.starts_with('/')).then(|| rest.split('/').filter(|s| !s.is_empty()).collect())
}

fn dir(name: &str, size: usize) -> Entry {
    Entry { name: name.into(), is_dir: true, size }
}

/// The contents of a network neighbourhood folder; None — the path is not from it.
pub fn list(path: &str) -> Option<fs::Result<Vec<Entry>>> {
    let p = parts(path)?;
    // Further down is the contents of the resource itself: the volume reads it.
    if p.len() >= 3 {
        return None;
    }
    kick_scan();
    let s = STATE.lock();
    fn of<'a>(s: &'a State, group: &'a str) -> impl Iterator<Item = &'a Computer> {
        s.computers.iter().filter(move |c| group_of(c).eq_ignore_ascii_case(group))
    }
    Some(match p[..] {
        [] => {
            let mut groups: Vec<&str> = s.computers.iter().map(group_of).collect();
            groups.sort();
            groups.dedup();
            Ok(groups.iter().map(|g| dir(g, of(&s, g).count())).collect())
        }
        [group] => Ok(of(&s, group).map(|c| dir(&c.name, 0)).collect()),
        [group, name] => {
            let Some(c) = of(&s, group).find(|c| c.name.eq_ignore_ascii_case(name)) else { return Some(Err(FsError::NotFound)) };
            let (ip, port, stale) = (c.ip, c.port, interrupts::ticks() >= c.shares_at + SHARES_EVERY);
            let list = match &c.shares {
                Shares::Ready(l) => l.iter().map(|n| dir(n, 0)).collect(),
                _ => Vec::new(),
            };
            let idle = !matches!(c.shares, Shares::Loading | Shares::NeedLogin);
            drop(s);
            if idle && stale {
                load_shares(ip, port);
            }
            Ok(list)
        }
        _ => Err(FsError::NotFound),
    })
}

pub fn is_dir(path: &str) -> bool {
    let Some(p) = parts(path) else { return false };
    let s = STATE.lock();
    let of = |g: &str| s.computers.iter().filter(|c| group_of(c).eq_ignore_ascii_case(g)).collect::<Vec<_>>();
    match p[..] {
        [] => true,
        [g] => !of(g).is_empty(),
        [g, n] => of(g).iter().any(|c| c.name.eq_ignore_ascii_case(n)),
        [g, n, share] => {
            of(g).iter().any(|c| c.name.eq_ignore_ascii_case(n) && matches!(&c.shares, Shares::Ready(l) if l.iter().any(|x| x == share)))
        }
        _ => false,
    }
}

/// The resource a path lies in: (resource path, folder in it with "/" or empty).
pub fn share_of(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix(fs::NETWORK)?;
    let mut end = 0;
    for _ in 0..3 {
        let tail = rest[end..].strip_prefix('/')?;
        end += 1 + tail.find('/').unwrap_or(tail.len());
    }
    let end = end + fs::NETWORK.len();
    Some((&path[..end], path[end..].trim_end_matches('/')))
}

/// The path depth in the neighbourhood: 0 — the root, 1 — a group, 2 — a computer, 3 — a resource, deeper — folders in it.
pub fn depth(path: &str) -> Option<usize> {
    parts(path).map(|p| p.len())
}

/// The hint for an empty folder.
pub fn note(path: &str) -> Option<String> {
    let p = parts(path)?;
    let s = STATE.lock();
    if let [group, name] = p[..] {
        let c = s.computers.iter().find(|c| group_of(c).eq_ignore_ascii_case(group) && c.name.eq_ignore_ascii_case(name))?;
        return Some(match &c.shares {
            Shares::Ready(_) => "No shared folders".into(),
            Shares::NeedLogin => "This computer asks for a name and password: right-click, Log In".into(),
            Shares::Failed(e) => format!("Could not read shared folders: {e}"),
            _ => "Reading shared folders\u{2026}".into(),
        });
    }
    (p.len() < 2).then(|| {
        if super::subnet().is_none() {
            "No network connection".into()
        } else if s.scanning {
            "Searching the network\u{2026}".into()
        } else {
            "No computers found. Connect to Server adds one by address".into()
        }
    })
}

/// The label on the right of a list row.
pub fn info(path: &str) -> String {
    let Some(p) = parts(path) else { return String::new() };
    let s = STATE.lock();
    match p[..] {
        [_] => {
            let n = s.computers.iter().filter(|c| group_of(c).eq_ignore_ascii_case(p[0])).count();
            format!("{n} computer{}", if n == 1 { "" } else { "s" })
        }
        [g, n] => s.computers.iter().find(|c| group_of(c).eq_ignore_ascii_case(g) && c.name.eq_ignore_ascii_case(n)).map_or(String::new(), |c| {
            if c.port == 445 { ip_str(c.ip) } else { format!("{}:{}", ip_str(c.ip), c.port) }
        }),
        _ => String::new(),
    }
}

/// The computer of a path (/network/group/computer[/resource]): name, address, port.
pub fn host_of(path: &str) -> Option<(String, Ip, u16)> {
    let p = parts(path)?;
    let (&group, &name) = (p.first()?, p.get(1)?);
    let s = STATE.lock();
    let c = s.computers.iter().find(|c| group_of(c).eq_ignore_ascii_case(group) && c.name.eq_ignore_ascii_case(name))?;
    Some((c.name.clone(), c.ip, c.port))
}

/// Anew: walk the network and re-read the resources.
pub fn refresh(path: &str) {
    if let Some((_, ip, port)) = host_of(path) {
        load_shares(ip, port);
    } else {
        STATE.lock().last_scan = None;
        kick_scan();
    }
    fs::touch();
}

// ---------------------------------------------------------------- a computer's resources

fn set_shares(ip: Ip, port: u16, value: Shares) {
    let mut s = STATE.lock();
    if let Some(c) = s.computers.iter_mut().find(|c| c.ip == ip && c.port == port) {
        c.shares = value;
        c.shares_at = interrupts::ticks();
    }
    drop(s);
    fs::touch();
}

fn load_shares(ip: Ip, port: u16) {
    {
        let mut s = STATE.lock();
        let Some(c) = s.computers.iter_mut().find(|c| c.ip == ip && c.port == port) else { return };
        if !matches!(c.shares, Shares::Ready(_)) {
            c.shares = Shares::Loading;
        }
        c.shares_at = interrupts::ticks();
    }
    task::spawn("net-shares", None, move || {
        let name = STATE.lock().computers.iter().find(|c| c.ip == ip && c.port == port).map(|c| c.name.clone()).unwrap_or_default();
        let value = match super::smb::list_shares(&name, ip, port) {
            Ok(mut list) => {
                list.sort_by_key(|n| n.to_lowercase());
                Shares::Ready(list.iter().map(|n| clean(n)).collect())
            }
            Err(e) if super::smb::is_login_error(&e) => Shares::NeedLogin,
            Err(e) => Shares::Failed(e),
        };
        set_shares(ip, port, value);
    });
}

/// The login for a computer changed: read the resources again.
pub fn login_changed(ip: Ip, port: u16) {
    set_shares(ip, port, Shares::Unknown);
    load_shares(ip, port);
}

// ---------------------------------------------------------------- scanning

fn kick_scan() {
    let now = interrupts::ticks();
    {
        let mut s = STATE.lock();
        if s.scanning || s.last_scan.is_some_and(|t| now < t + SCAN_EVERY) || super::subnet().is_none() {
            return;
        }
        s.scanning = true;
        s.round += 1;
    }
    task::spawn("net-scan", None, || {
        scan();
        let mut s = STATE.lock();
        s.scanning = false;
        s.last_scan = Some(interrupts::ticks());
        // Whoever did not respond two walks in a row is gone.
        let round = s.round;
        s.computers.retain(|c| c.manual || c.seen + 1 >= round);
        drop(s);
        fs::touch();
    });
}

fn wait(ticks: u64, sock: &UdpSocket) {
    let until = interrupts::ticks() + ticks;
    while interrupts::ticks() < until {
        while let Some((src, _, p)) = sock.try_recv() {
            nbt::on_status_reply(src, &p);
        }
        task::sleep_until(interrupts::ticks() + 1);
    }
}

fn scan() {
    let Some((me, mask, _)) = super::subnet() else { return };
    let Some(sock) = UdpSocket::bind(0) else { return };
    PROBE.store(true, Ordering::Relaxed);
    // The subnet addresses. In a network that is too large — only our own 254 neighbours.
    let mask32 = u32::from_be_bytes(mask);
    let mask32 = if !mask32 > MAX_HOSTS + 1 { 0xFFFF_FF00 } else { mask32 };
    let net = u32::from_be_bytes(me) & mask32;
    let hosts: Vec<Ip> = (1..!mask32).map(|i| (net + i).to_be_bytes()).filter(|a| *a != me).collect();
    for chunk in hosts.chunks(24) {
        for &ip in chunk {
            super::arp_probe(ip);
        }
        wait(1, &sock);
    }
    wait(HZ, &sock);
    let alive: Vec<Ip> = super::known_hosts().into_iter().filter(|a| *a != me).collect();
    let query = nbt::status_query((crate::arch::rdtsc() as u16) | 1);
    for _ in 0..2 {
        for &ip in &alive {
            sock.send_to(ip, nbt::NAME_PORT, &query);
        }
        wait(HZ / 2, &sock);
    }
    // Whoever did not name itself — check the SMB port: Windows with NetBIOS off is silent on NBSTAT.
    let unknown: Vec<Ip> = {
        let s = STATE.lock();
        let round = s.round;
        alive.iter().filter(|ip| !s.computers.iter().any(|c| c.ip == **ip && c.seen == round)).copied().collect()
    };
    for ip in unknown {
        while TCP_CHECKS.load(Ordering::Relaxed) >= 16 {
            wait(1, &sock);
        }
        TCP_CHECKS.fetch_add(1, Ordering::Relaxed);
        task::spawn("net-probe", None, move || {
            if TcpStream::connect(ip, 445).is_ok() {
                found(Found { ip, name: ip_str(ip), workgroup: String::new(), smb: true, via: "SMB port" });
            }
            TCP_CHECKS.fetch_sub(1, Ordering::Relaxed);
        });
    }
    wait(HZ, &sock);
}

// ---------------------------------------------------------------- the service

fn say_hello(id: &Identity, sockets: &Sockets, me: Ip, broadcast: Ip, period_ms: u32) {
    if !id.visible {
        return;
    }
    mdns::announce(&sockets.mdns, id, me);
    wsd::hello(&sockets.wsd, id, me);
    nbt::announce(&sockets.datagram, id, me, broadcast, period_ms);
}

struct Sockets {
    name: UdpSocket,
    datagram: UdpSocket,
    mdns: UdpSocket,
    llmnr: UdpSocket,
    wsd: UdpSocket,
}

impl Sockets {
    fn open() -> Option<Sockets> {
        Some(Sockets {
            name: UdpSocket::bind(nbt::NAME_PORT)?,
            datagram: UdpSocket::bind(nbt::DATAGRAM_PORT)?,
            mdns: UdpSocket::bind(mdns::MDNS_PORT)?,
            llmnr: UdpSocket::bind(mdns::LLMNR_PORT)?,
            wsd: UdpSocket::bind(wsd::PORT)?,
        })
    }

    fn poll(&self, id: &Identity, me: Ip, mac: [u8; 6]) {
        while let Some((src, port, p)) = self.name.try_recv() {
            nbt::on_name_packet(&self.name, src, port, &p, id, me, mac);
        }
        while let Some((src, _, p)) = self.datagram.try_recv() {
            nbt::on_datagram(src, &p);
        }
        while let Some((src, port, p)) = self.mdns.try_recv() {
            mdns::on_mdns(&self.mdns, src, port, &p, id, me);
        }
        while let Some((src, port, p)) = self.llmnr.try_recv() {
            mdns::on_llmnr(&self.llmnr, src, port, &p, id, me);
        }
        while let Some((src, port, p)) = self.wsd.try_recv() {
            wsd::on_packet(&self.wsd, src, port, &p, id, me);
        }
    }
}

/// Starts the discovery service (it waits for the network to appear by itself).
pub fn start() {
    wsd::start_http();
    task::spawn("netdisco", None, || {
        let sockets = loop {
            if let Some(s) = Sockets::open() {
                break s;
            }
            task::sleep_until(interrupts::ticks() + HZ);
        };
        let mut id = identity::get();
        let (mut seen_id, mut seen_ip) = (identity::version(), [0; 4]);
        let (mut next_hello, mut next_join, mut hellos) = (0u64, 0u64, 0u32);
        loop {
            let now = interrupts::ticks();
            if identity::version() != seen_id {
                seen_id = identity::version();
                id = identity::get();
                next_hello = 0;
            }
            if let (Some((me, _, broadcast)), Some(mac)) = (super::subnet(), super::mac()) {
                if me != seen_ip {
                    seen_ip = me;
                    (next_hello, next_join, hellos) = (0, 0, 0);
                    kprintln!("network: {} in {} at {}", id.name, id.workgroup, ip_str(me));
                }
                if now >= next_join {
                    next_join = now + 60 * HZ;
                    for group in [mdns::MDNS_GROUP, mdns::LLMNR_GROUP, wsd::GROUP] {
                        super::join_group(group);
                    }
                }
                if now >= next_hello {
                    // More often in the first minutes, as Windows does, then once every 12 minutes.
                    let period = if hellos < 6 { 60 * HZ } else { 720 * HZ };
                    hellos += 1;
                    next_hello = now + period;
                    say_hello(&id, &sockets, me, broadcast, (period * 1000 / HZ) as u32);
                }
                if PROBE.swap(false, Ordering::Relaxed) {
                    wsd::probe(&sockets.wsd);
                    mdns::query(&sockets.mdns);
                }
                sockets.poll(&id, me, mac);
            }
            task::sleep_until(now + 1);
        }
    });
}
