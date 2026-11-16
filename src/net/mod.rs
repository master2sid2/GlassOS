//! Networking: the e1000 and RTL8168 cards and the protocol stack — Ethernet, ARP, IPv4, ICMP (ping),
//! UDP, DHCP (the router assigns the address), DNS and TCP. HTTP (`http`) sits on top.
//!
//! Packets are handled by the "net" kernel thread: once a tick it takes received frames and
//! services timers (DHCP, ARP and TCP retries). Programs call this module's functions
//! from their own threads and wait for answers, yielding the CPU.

mod e1000;
mod rtl8168;
pub mod discovery;
pub mod exporter;
pub mod http;
pub mod identity;
mod mdns;
mod nbt;
pub mod ntp;
pub mod remote;
pub mod smb;
pub mod smbd;
pub mod srvsvc;
mod wsd;

use crate::interrupts::{self, HZ};
use crate::kprintln;
use crate::sync::IrqLock;
use crate::task;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use e1000::E1000;

pub type Ip = [u8; 4];
type Mac = [u8; 6];

const BROADCAST_MAC: Mac = [0xFF; 6];
const BROADCAST: Ip = [255; 4];
const ETH_ARP: u16 = 0x0806;
const ETH_IP: u16 = 0x0800;
const PROTO_ICMP: u8 = 1;
const PROTO_IGMP: u8 = 2;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;
const MSS: usize = 1460;
/// How much we send without an acknowledgement: no more than the card's transmit ring. A card may
/// allow more (`Nic::send_window`).
const WINDOW: usize = 44 * MSS;
/// How much we accept ahead: several server answers in a row. Smaller than the card's receive
/// ring, so a whole window does not lose frames while the program is busy.
const RECEIVE_WINDOW: usize = 384 * 1024;
/// The lower bound of the retransmission timer (RFC 6298 allows 1 s, in practice everyone uses 200 ms):
/// on a local network the answer arrives within a fraction of a millisecond, and a second of idling on every
/// loss ate the throughput.
const MIN_RTO: u64 = HZ / 5;
const MAX_RTO: u64 = 60 * HZ;
/// This many repeats of one acknowledgement signal a loss (fast retransmit, RFC 5681).
const DUP_ACKS: u8 = 3;
/// How much program data may wait in the connection's send queue (beyond the window): beyond that
/// `TcpStream::send` waits until the peer accepts it.
const SEND_QUEUE: usize = 512 * 1024;
/// The window scale (a TCP option): without it the window is never larger than 64 KiB.
const WINDOW_SCALE: u8 = 3;
/// Acknowledge once per this many frames. On the air every acknowledgement takes time from
/// data, and rare ones are enough for the sender: it counts bytes, not answers.
const ACK_EVERY: u8 = 8;

pub fn ip_str(ip: Ip) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

pub fn parse_ip(s: &str) -> Option<Ip> {
    let parts: Vec<u8> = s.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    parts.try_into().ok()
}

fn is_multicast(ip: Ip) -> bool {
    ip[0] & 0xF0 == 224
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

/// The window scale from a SYN segment's options (None — the peer did not offer it).
fn window_scale(options: &[u8]) -> Option<u8> {
    let mut at = 0;
    while at < options.len() {
        match options[at] {
            0 => break,
            1 => at += 1,
            kind => {
                let len = *options.get(at + 1)? as usize;
                if kind == 3 && len == 3 {
                    return options.get(at + 2).map(|&v| v.min(14));
                }
                at += len.max(2);
            }
        }
    }
    None
}

/// The Internet checksum (the ones' complement of the sum of 16-bit words).
/// Each part is summed from an even position (an odd tail is the high byte of a word).
///
/// The sum does not depend on byte order (RFC 1071): we add 4-byte words in native
/// order and swap the bytes at the end — several times faster than two at a time.
pub(crate) fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u64;
    for part in parts {
        let mut words = part.chunks_exact(4);
        for w in &mut words {
            sum += u32::from_le_bytes([w[0], w[1], w[2], w[3]]) as u64;
        }
        let mut rest = words.remainder();
        if rest.len() >= 2 {
            sum += u16::from_le_bytes([rest[0], rest[1]]) as u64;
            rest = &rest[2..];
        }
        if let [last] = rest {
            sum += *last as u64;
        }
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16).swap_bytes()
}

// ---------------------------------------------------------------- state

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dhcp {
    Discovering { xid: u32, at: u64 },
    Requesting { xid: u32, at: u64, offered: Ip, server: Ip },
    /// The address is leased: when to renew (half the term) and when it expires (ticks).
    Bound { renew: u64, expire: u64 },
    /// Renewing the lease. `reboot` — the cable was plugged in again: ask whether the old
    /// address is valid on this network (DHCPREQUEST with option 50, like INIT-REBOOT in RFC 2131).
    Renewing { xid: u32, at: u64, tries: u8, expire: u64, reboot: bool },
    /// Addresses set manually (network settings).
    Manual,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    SynSent,
    /// Incoming: got a SYN, answered SYN+ACK, waiting for the ACK.
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    LastAck,
    Closed,
}

struct Tcb {
    id: u32,
    state: State,
    /// The local address and interface of this connection: answers go from the address/card
    /// the request came to (so that each address answers for itself).
    lip: Ip,
    link: usize,
    lport: u16,
    rip: Ip,
    rport: u16,
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    rcv_nxt: u32,
    peer_wnd: usize,
    /// Sent but not yet acknowledged (starting at `snd_una`). The queues are rings:
    /// what was sent and acknowledged leaves from the front, and in a `Vec` that shifted the whole
    /// rest (an 11 MB answer meant hundreds of MB copied per stack step, under a lock
    /// with interrupts disabled: the system froze).
    unacked: VecDeque<u8>,
    /// Waiting to be sent.
    outq: VecDeque<u8>,
    /// Received and not yet read by the program.
    inq: Vec<u8>,
    fin_queued: bool,
    fin_seq: Option<u32>,
    peer_fin: bool,
    rto: u64,
    rto_at: u64,
    retries: u32,
    /// The round-trip time estimate (RFC 6298) in ticks ×8 and its variance ×4; 0 — no samples yet.
    srtt: u64,
    rttvar: u64,
    /// A measurement in flight: the segment end and the tick it was sent (retransmissions are not measured — Karn's algorithm).
    rtt_probe: Option<(u32, u64)>,
    /// Repeats of the same acknowledgement in a row: three — a frame is lost, retransmit without waiting for the timer.
    dup_acks: u8,
    /// Fast recovery is in progress up to this number (`snd_nxt` at the moment of loss).
    recover: Option<u32>,
    error: Option<&'static str>,
    /// How many frames arrived without an acknowledgement, and the tick the first of them arrived.
    ack_owed: u8,
    ack_tick: u64,
    /// Window scales: ours and the peer's. 0 — the peer did not enable scaling.
    rcv_scale: u8,
    snd_scale: u8,
}

/// One network interface: a card and everything about the link on it (address, DHCP lease,
/// ARP table). Transport (TCP/UDP/ping) is shared and lives in Net — there may be
/// several interfaces (cable and Wi-Fi at the same time).
struct Iface {
    nic: Box<dyn Nic>,
    /// The card name (for "About" and the settings window).
    name: &'static str,
    mac: Mac,
    ip: Ip,
    mask: Ip,
    gw: Ip,
    dns: Ip,
    dhcp: Dhcp,
    /// Since which tick we wait for an address (while there is no link, the count is paused).
    dhcp_started: u64,
    /// DHCP answers that arrived on the interface: in total and foreign ones (not our xid).
    dhcp_seen: u32,
    dhcp_foreign: u32,
    /// The log already says that this interface waits long for an address.
    dhcp_told: bool,
    /// Whether there was a link last time (the cable was plugged in or out — see `link_changed`).
    was_up: bool,
    /// Checking a 169.254.x.y address (RFC 3927): the candidate, when the next step is, how many
    /// ARP probes were sent.
    ll_probe: Option<(Ip, u64, u8)>,
    /// Which candidate this is: after a conflict we take the next one.
    ll_attempt: u32,
    arp: BTreeMap<Ip, Mac>,
    /// IP packets waiting for an ARP answer: (next hop, packet, when queued).
    pending: Vec<(Ip, Vec<u8>, u64)>,
    arp_asked: BTreeMap<Ip, u64>,
    ip_id: u16,
    /// When to remind the bridge where we are: a unicast ARP to the gateway (see `send_eth`).
    relearn: [u64; 2],
    /// Whether it is wireless (the top bar icon and the settings tab depend on it).
    wireless: bool,
    stats: Stats,
}

struct Net {
    /// Active interfaces. Several may be active at once (cable and Wi-Fi); the preferred one
    /// for outgoing traffic is chosen by `primary`.
    ifaces: Vec<Iface>,
    /// A card of the other kind kept in reserve (for example, a USB adapter that claims a link without a
    /// cable); `pick_adapter` switches to it.
    spare: Option<(Box<dyn Nic>, &'static str)>,
    /// Static routes beyond the built-in "own subnet" and "default gateway".
    routes: Vec<Route>,
    udp: BTreeMap<u16, Vec<(Ip, u16, Vec<u8>)>>,
    /// Ping answers: (identifier, sequence) → the tick of the answer.
    pings: BTreeMap<(u16, u16), Option<u64>>,
    tcp: Vec<Tcb>,
    /// Ports we accept connections on, and the accepted (not yet taken) connections.
    listeners: Vec<(u16, Vec<u32>)>,
    next_port: u16,
    next_id: u32,
}

impl Net {
    /// The index of the preferred interface for outgoing traffic. It is chosen by properties, not by
    /// position in the list, so that the order (and the connections' `link` indices) does not change.
    /// An interface with a real address (DHCP or manual) beats one that still waits for an address
    /// or took 169.254.x.y: there is no gateway there, and nothing beyond the local network is reachable.
    /// Among equals a wired interface beats a wireless one.
    fn primary(&self) -> usize {
        let usable = |i: &Iface| i.nic.link_up() && routable(i.ip);
        self.ifaces
            .iter()
            .position(|i| !i.wireless && usable(i))
            .or_else(|| self.ifaces.iter().position(usable))
            .or_else(|| self.ifaces.iter().position(|i| !i.wireless && i.nic.link_up()))
            .or_else(|| self.ifaces.iter().position(|i| i.nic.link_up()))
            .unwrap_or(0)
    }

    /// Through which interface to send a packet to `dst`: the interface whose subnet
    /// contains it (on a tie — the preferred one), otherwise the preferred one (the default gateway).
    fn route(&self, dst: Ip) -> usize {
        let primary = self.primary();
        if self.ifaces.get(primary).is_some_and(|i| in_subnet(dst, i.ip, i.mask)) {
            return primary;
        }
        self.ifaces
            .iter()
            .position(|i| i.ip != [0; 4] && in_subnet(dst, i.ip, i.mask))
            .unwrap_or(primary)
    }
}

/// A 169.254.x.y address — the self-assigned one for a network without DHCP (RFC 3927).
fn link_local(ip: Ip) -> bool {
    ip[..2] == [169, 254]
}

/// An address that can reach beyond the local network: present and not 169.254.x.y.
fn routable(ip: Ip) -> bool {
    ip != [0; 4] && !link_local(ip)
}

/// A candidate address in 169.254.1.0–169.254.254.255 from the MAC (usually the same one for a machine),
/// `attempt` is the attempt number after conflicts.
fn link_local_candidate(mac: Mac, attempt: u32) -> Ip {
    let mut h: u32 = 0x811C_9DC5;
    for b in mac.iter().chain(&attempt.to_le_bytes()) {
        h = (h ^ *b as u32).wrapping_mul(0x0100_0193);
    }
    [169, 254, 1 + (h % 254) as u8, (h >> 8) as u8]
}

impl Iface {
    /// The address is no longer ours: forget it and look for DHCP again.
    fn forget_address(&mut self, now: u64) {
        (self.ip, self.mask, self.gw, self.dns) = ([0; 4], [0; 4], [0; 4], [0; 4]);
        self.arp.clear();
        self.pending.clear();
        self.dhcp = Dhcp::Discovering { xid: crate::arch::rdtsc() as u32, at: 0 };
        (self.dhcp_started, self.dhcp_seen, self.dhcp_foreign, self.dhcp_told) = (now, 0, 0, false);
        self.ll_probe = None;
    }
}

/// How to show an address taken without DHCP.
const LINK_LOCAL_TEXT: &str = "link-local (no DHCP answer)";

/// Whether an address is in a subnet (ip/mask).
fn in_subnet(dst: Ip, ip: Ip, mask: Ip) -> bool {
    (0..4).all(|i| dst[i] & mask[i] == ip[i] & mask[i])
}

#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Out-of-order TCP frames: they are dropped, the peer will send them again.
    pub rx_disordered: u64,
    pub resent: u64,
    /// Frames the card did not take: the transmit ring was full.
    pub tx_dropped: u64,
}

static NET: crate::sync::Mutex<Option<Net>> = crate::sync::Mutex::new(None);
/// Until which tick to run without pauses. The card is polled rather than waited on for interrupts, and with
/// polling once a tick every request-response would lose tens of milliseconds. But a stream of
/// a megabyte or two per second is handled by such polling, and there is no point taking the CPU
/// for it: we only run without pauses while waiting for an answer and for a fast stream.
static BUSY_UNTIL: AtomicU64 = AtomicU64::new(0);
/// From this many frames per tick (about 1 MB/s) the stream runs without pauses. With polling once
/// a tick, the chain of card, stack and program limits itself to about 14 frames per
/// tick, so the threshold must be noticeably lower: otherwise the stream would never speed up.
const FAST_FRAMES: u64 = 8;
const REPLY_TICKS: u64 = 1;
const STREAM_TICKS: u64 = 3;
static FRAME_TICK: AtomicU64 = AtomicU64::new(0);
static FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

/// The card called with an interrupt: the stack thread sleeps in `STACK_WAKE` while it has nothing to say.
static RX_PENDING: AtomicBool = AtomicBool::new(false);
static STACK_WAKE: crate::sync::WaitQueue = crate::sync::WaitQueue::new();
/// The stack handled frames: waiting sockets (`wait`) look whether theirs arrived.
static CHANGED: crate::sync::WaitQueue = crate::sync::WaitQueue::new();
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// From the network card's interrupt handler.
pub fn wake_stack() {
    RX_PENDING.store(true, Ordering::Release);
    STACK_WAKE.notify();
}

pub(crate) fn exchanging() -> bool {
    interrupts::ticks() <= BUSY_UNTIL.load(Ordering::Relaxed)
}

fn count_frames(frames: u64) {
    let now = interrupts::ticks();
    if FRAME_TICK.swap(now, Ordering::Relaxed) != now {
        FRAME_COUNT.store(0, Ordering::Relaxed);
    }
    if FRAME_COUNT.fetch_add(frames, Ordering::Relaxed) + frames >= FAST_FRAMES {
        BUSY_UNTIL.fetch_max(now + STREAM_TICKS, Ordering::Relaxed);
    }
}

/// Information about one interface for `net` in the terminal.
pub struct Info {
    pub name: &'static str,
    pub mac: String,
    pub ip: Ip,
    pub mask: Ip,
    pub gw: Ip,
    pub dns: Ip,
    pub dhcp: &'static str,
    pub stats: Stats,
    /// There is a link (a cable with link, Wi-Fi with an access point).
    pub link: bool,
    /// The cable link speed (Mbit/s) and whether it is full duplex, while there is a link.
    pub speed: Option<(u32, bool)>,
}

fn iface_info(i: &Iface) -> Info {
    let m = i.mac;
    Info {
        name: if i.wireless { "wlan0" } else { "eth0" },
        mac: format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]),
        ip: i.ip,
        mask: i.mask,
        gw: i.gw,
        dns: i.dns,
        dhcp: match i.dhcp {
            Dhcp::Bound { .. } | Dhcp::Renewing { .. } => "dhcp",
            Dhcp::Manual => "static",
            _ if link_local(i.ip) => LINK_LOCAL_TEXT,
            _ => "waiting for DHCP",
        },
        stats: i.stats,
        link: i.nic.link_up(),
        speed: i.nic.link_up().then(|| i.nic.link_speed()).flatten(),
    }
}

/// Information about the preferred interface (the panel, "About"). None — no card.
pub fn info() -> Option<Info> {
    let net = NET.lock();
    let n = net.as_ref()?;
    Some(iface_info(n.ifaces.get(n.primary())?))
}

/// Through which interface a packet to `dst` would go now ("eth0"/"wlan0"). None — no card.
pub fn egress(dst: Ip) -> Option<&'static str> {
    let net = NET.lock();
    let n = net.as_ref()?;
    n.ifaces.get(n.route(dst)).map(|i| if i.wireless { "wlan0" } else { "eth0" })
}

/// PHY registers of the wired card (`net phy`).
pub fn phy_regs(renegotiate: bool) -> Vec<(u32, u16)> {
    let mut net = NET.lock();
    let Some(n) = net.as_mut() else { return Vec::new() };
    n.ifaces.iter_mut().find(|i| !i.wireless).map_or(Vec::new(), |i| i.nic.phy_regs(renegotiate))
}

/// Information about each active interface (for the `net` command).
pub fn interfaces() -> Vec<Info> {
    NET.lock().as_ref().map_or(Vec::new(), |n| n.ifaces.iter().map(iface_info).collect())
}

/// The network settings file.
pub const CONFIG_FILE: &str = "/system/net.cfg";

/// Network settings: addresses over DHCP or set manually.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub dhcp: bool,
    pub ip: Ip,
    pub mask: Ip,
    pub gw: Ip,
    pub dns: Ip,
}

impl Config {
    pub fn to_text(&self) -> String {
        format!(
            "mode={}\nip={}\nmask={}\ngateway={}\ndns={}\n",
            if self.dhcp { "dhcp" } else { "static" },
            ip_str(self.ip),
            ip_str(self.mask),
            ip_str(self.gw),
            ip_str(self.dns)
        )
    }

    pub fn parse(text: &str) -> Config {
        let mut c = Config { dhcp: true, ip: [0; 4], mask: [255, 255, 255, 0], gw: [0; 4], dns: [0; 4] };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let ip = parse_ip(value.trim());
            match key.trim() {
                "mode" => c.dhcp = value.trim() != "static",
                "ip" => c.ip = ip.unwrap_or(c.ip),
                "mask" => c.mask = ip.unwrap_or(c.mask),
                "gateway" => c.gw = ip.unwrap_or(c.gw),
                "dns" => c.dns = ip.unwrap_or(c.dns),
                _ => {}
            }
        }
        c
    }
}

/// The current settings (the addresses in effect now). None — no network card.
pub fn config() -> Option<Config> {
    let net = NET.lock();
    let n = net.as_ref()?;
    let i = n.ifaces.get(n.primary())?;
    Some(Config { dhcp: i.dhcp != Dhcp::Manual, ip: i.ip, mask: i.mask, gw: i.gw, dns: i.dns })
}

/// Applies the settings (to the preferred interface): manual ones right away, DHCP asks for
/// an address again.
/// Ask for addresses again on all interfaces that take them over DHCP.
pub fn renew() {
    let mut net = NET.lock();
    let Some(n) = net.as_mut() else { return };
    let now = interrupts::ticks();
    for i in n.ifaces.iter_mut() {
        if i.dhcp == Dhcp::Manual {
            continue;
        }
        (i.ip, i.mask, i.gw, i.dns) = ([0; 4], [0; 4], [0; 4], [0; 4]);
        i.arp.clear();
        i.pending.clear();
        i.dhcp = Dhcp::Discovering { xid: crate::arch::rdtsc() as u32, at: 0 };
        (i.dhcp_started, i.dhcp_seen, i.dhcp_foreign, i.dhcp_told) = (now, 0, 0, false);
        kprintln!("net: {} asks for an address again", i.name);
    }
}

pub fn configure(c: &Config) {
    let mut net = NET.lock();
    let Some(n) = net.as_mut() else { return };
    let p = n.primary();
    let i = &mut n.ifaces[p];
    i.arp.clear();
    i.pending.clear();
    if c.dhcp {
        (i.ip, i.mask, i.gw, i.dns) = ([0; 4], [0; 4], [0; 4], [0; 4]);
        i.dhcp = Dhcp::Discovering { xid: crate::arch::rdtsc() as u32, at: 0 };
        i.dhcp_started = interrupts::ticks();
        kprintln!("net: requesting address over DHCP");
    } else {
        (i.ip, i.mask, i.gw, i.dns) = (c.ip, c.mask, c.gw, c.dns);
        i.dhcp = Dhcp::Manual;
        kprintln!("net: static address {} gateway {} dns {}", ip_str(c.ip), ip_str(c.gw), ip_str(c.dns));
    }
}

/// Saves the settings to the file and applies them.
pub fn save_config(c: &Config) -> bool {
    if !crate::access::is_admin() {
        return false;
    }
    configure(c);
    let _ = crate::fs::mkdir("/system");
    crate::fs::write(CONFIG_FILE, c.to_text().as_bytes()).is_ok()
}

/// At boot: the settings from the file (if the addresses there are manual — apply them right away).
pub fn load_config() {
    if let Ok(data) = crate::task::as_system(|| crate::fs::read(CONFIG_FILE)) {
        let c = Config::parse(&String::from_utf8_lossy(&data));
        if !c.dhcp {
            configure(&c);
        }
    }
}

/// Permanent routes (they survive a reboot).
pub const ROUTES_FILE: &str = "/system/routes.cfg";

/// A static network route: a network (start) with a mask, and a gateway (0.0.0.0 — directly).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub dest: Ip,
    pub mask: Ip,
    pub gw: Ip,
    /// Saved in the file, not only in memory.
    pub permanent: bool,
}

/// The number of ones in a mask (the prefix length) — to find the most specific route.
pub fn mask_len(m: Ip) -> u32 {
    m.iter().map(|b| b.count_ones()).sum()
}

fn mask_from_len(len: u32) -> Ip {
    let mut m = [0u8; 4];
    for i in 0..len.min(32) {
        m[(i / 8) as usize] |= 0x80 >> (i % 8);
    }
    m
}

/// Parses "NETWORK/LENGTH" (for example, 10.0.0.0/8) into (network start, mask).
pub fn parse_cidr(s: &str) -> Option<(Ip, Ip)> {
    let (d, l) = s.split_once('/')?;
    let dest = parse_ip(d.trim())?;
    let len = l.trim().parse::<u32>().ok()?;
    if len > 32 {
        return None;
    }
    let mask = mask_from_len(len);
    Some((core::array::from_fn(|i| dest[i] & mask[i]), mask))
}

/// Parses a gateway address (a wrapper so parse_ip is not exposed).
pub fn parse_addr(s: &str) -> Option<Ip> {
    parse_ip(s.trim())
}

/// The route table for display: built-in ones (own subnet, gateway) and user ones.
pub fn route_lines() -> Vec<String> {
    let net = NET.lock();
    let Some(n) = net.as_ref() else { return alloc::vec!["no network card".into()] };
    let mut out = alloc::vec![format!("{:<19}{:<17}{:<11}{}", "Destination", "Gateway", "Interface", "Kind")];
    // Built-in routes of each interface: its own subnet (on-link) and the default gateway.
    for iface in &n.ifaces {
        let name = if iface.wireless { "wlan0" } else { "eth0" };
        let row = |dest: String, gw: &str, kind: &str| format!("{dest:<19}{gw:<17}{name:<11}{kind}");
        if iface.ip != [0; 4] {
            let subnet: Ip = core::array::from_fn(|i| iface.ip[i] & iface.mask[i]);
            out.push(row(format!("{}/{}", ip_str(subnet), mask_len(iface.mask)), "—", "on-link"));
        }
        if iface.gw != [0; 4] {
            out.push(row("0.0.0.0/0".into(), &ip_str(iface.gw), "default"));
        }
    }
    for r in &n.routes {
        let gw = if r.gw == [0; 4] { "on-link".into() } else { ip_str(r.gw) };
        let name = n.ifaces.get(n.route(r.dest)).map_or("", |i| if i.wireless { "wlan0" } else { "eth0" });
        out.push(format!("{:<19}{:<17}{:<11}{}", format!("{}/{}", ip_str(r.dest), mask_len(r.mask)), gw, name, if r.permanent { "permanent" } else { "static" }));
    }
    out
}

/// Adds a route. permanent — also to the file so it survives a reboot (needs administrator
/// rights). A repeated one for the same network/mask replaces the previous one.
pub fn add_route(dest: Ip, mask: Ip, gw: Ip, permanent: bool) -> Result<(), String> {
    if permanent && !crate::access::is_admin() {
        return Err("only an administrator can add a permanent route".into());
    }
    let dest: Ip = core::array::from_fn(|i| dest[i] & mask[i]);
    {
        let mut net = NET.lock();
        let Some(n) = net.as_mut() else { return Err("no network card".into()) };
        n.routes.retain(|r| !(r.dest == dest && r.mask == mask));
        n.routes.push(Route { dest, mask, gw, permanent });
    }
    if permanent {
        save_routes();
    }
    Ok(())
}

/// Deletes a route. also_saved — remove it from the permanent file too.
pub fn del_route(dest: Ip, mask: Ip, also_saved: bool) -> Result<(), String> {
    let dest: Ip = core::array::from_fn(|i| dest[i] & mask[i]);
    let removed = {
        let mut net = NET.lock();
        let Some(n) = net.as_mut() else { return Err("no network card".into()) };
        let before = n.routes.len();
        n.routes.retain(|r| !(r.dest == dest && r.mask == mask));
        n.routes.len() != before
    };
    if also_saved {
        save_routes();
    }
    if removed { Ok(()) } else { Err("no such route".into()) }
}

fn save_routes() {
    let lines: String = NET
        .lock()
        .as_ref()
        .map(|n| n.routes.iter().filter(|r| r.permanent).map(|r| format!("{}/{} {}\n", ip_str(r.dest), mask_len(r.mask), ip_str(r.gw))).collect())
        .unwrap_or_default();
    crate::task::as_system(|| {
        let _ = crate::fs::mkdir("/system");
        let _ = crate::fs::write(ROUTES_FILE, lines.as_bytes());
    });
}

/// At boot (and when the card changes): permanent routes from the file into the live table.
pub fn load_routes() {
    let Ok(data) = crate::task::as_system(|| crate::fs::read(ROUTES_FILE)) else { return };
    let text = String::from_utf8_lossy(&data);
    let mut net = NET.lock();
    let Some(n) = net.as_mut() else { return };
    n.routes.retain(|r| !r.permanent);
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(cidr) = it.next() else { continue };
        let Some((dest, mask)) = parse_cidr(cidr) else { continue };
        let gw = it.next().and_then(parse_addr).unwrap_or([0; 4]);
        n.routes.retain(|r| !(r.dest == dest && r.mask == mask));
        n.routes.push(Route { dest, mask, gw, permanent: true });
    }
}

/// The network state in one line (for the panel and the settings window).
pub fn state_text() -> String {
    let wifi = over_wifi();
    let Some(i) = info() else { return if wifi { crate::wifi::status() } else { "No network card".into() } };
    match status() {
        Some((true, ..)) => format!("Connected ({})", if i.dhcp == "dhcp" { "DHCP" } else { "static" }),
        _ if i.link && i.dhcp == LINK_LOCAL_TEXT => "No DHCP answer (link-local address only)".into(),
        _ if wifi && crate::wifi::network().is_none() => crate::wifi::status(),
        Some((false, ..)) if i.ip != [0; 4] => "Cable unplugged".into(),
        _ => "Waiting for DHCP".into(),
    }
}

/// The connection goes or may go over Wi-Fi: a cable with link wins, and without it
/// the module decides. The top bar icon and the settings window tab depend on this.
pub fn over_wifi() -> bool {
    match NET.lock().as_ref() {
        Some(n) if n.ifaces.get(n.primary()).is_some_and(|i| i.wireless) => true,
        Some(n) if n.ifaces.get(n.primary()).is_some_and(|i| i.nic.link_up()) => false,
        _ => crate::wifi::available(),
    }
}

/// The sum of the counters over all interfaces.
fn total_stats(n: &Net) -> Stats {
    let mut s = Stats::default();
    for i in &n.ifaces {
        s.rx_packets += i.stats.rx_packets;
        s.tx_packets += i.stats.tx_packets;
        s.rx_bytes += i.stats.rx_bytes;
        s.tx_bytes += i.stats.tx_bytes;
        s.rx_disordered += i.stats.rx_disordered;
        s.resent += i.stats.resent;
        s.tx_dropped += i.stats.tx_dropped;
    }
    s
}

/// Whether at least one interface has a link and an address.
fn any_online(n: &Net) -> bool {
    n.ifaces.iter().any(|i| routable(i.ip) && i.nic.link_up())
}

/// The state for the top bar icon: (link and address present, packets received, sent).
/// None — no network card.
pub fn status() -> Option<(bool, u64, u64)> {
    let net = NET.lock();
    let n = net.as_ref()?;
    let s = total_stats(n);
    Some((any_online(n), s.rx_packets, s.tx_packets))
}

/// Network counters, whether there is a link with an address, and whether the active card is wireless. None — no card.
pub fn counters() -> Option<(Stats, bool, bool)> {
    let net = NET.lock();
    let n = net.as_ref()?;
    let wireless = n.ifaces.get(n.primary()).is_some_and(|i| i.wireless);
    Some((total_stats(n), any_online(n), wireless))
}

/// Bytes received, frames sent, out-of-order TCP frames and retransmissions for the Wi-Fi log.
pub(crate) fn traffic() -> (u64, u64, u64, u64) {
    NET.lock().as_ref().map_or((0, 0, 0, 0), |n| {
        let s = total_stats(n);
        (s.rx_bytes, s.tx_packets, s.rx_disordered, s.resent)
    })
}

/// Looks for a network card and starts the stack thread. Returns a line for "About".
/// A network card: e1000 or RTL8168 on PCI, USB Ethernet (CDC-ECM) or Wi-Fi.
pub trait Nic: Send {
    fn mac(&self) -> Mac;
    fn send(&mut self, frame: &[u8]) -> bool;
    fn recv(&mut self) -> Option<Vec<u8>>;
    fn link_up(&self) -> bool;
    /// The cable link speed (Mbit/s) and whether it is full duplex; None — the card does not know.
    fn link_speed(&self) -> Option<(u32, bool)> {
        None
    }
    /// How many TCP bytes may be in flight without an acknowledgement: frames beyond the transmit
    /// ring are dropped by the card, and we would have to wait for a retransmission.
    fn send_window(&self) -> usize {
        WINDOW
    }
    /// The cable was plugged in (true) or out: the card may prepare negotiation again.
    fn link_changed(&mut self, _up: bool) {}
    /// PHY registers (number, value) for `net phy`; empty — the card does not provide them.
    fn phy_regs(&mut self, _renegotiate: bool) -> Vec<(u32, u16)> {
        Vec::new()
    }
    fn wireless(&self) -> bool {
        false
    }
    /// The card wakes the stack with an interrupt (`wake_stack`), no need to poll it.
    fn interrupts(&self) -> bool {
        false
    }
    /// Clear the interrupt status before handling what was received.
    fn ack_irq(&mut self) {}
}

impl Nic for rtl8168::Rtl8168 {
    fn mac(&self) -> Mac {
        self.mac
    }
    fn send(&mut self, frame: &[u8]) -> bool {
        rtl8168::Rtl8168::send(self, frame)
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        rtl8168::Rtl8168::recv(self)
    }
    fn link_up(&self) -> bool {
        rtl8168::Rtl8168::link_up(self)
    }
    fn link_speed(&self) -> Option<(u32, bool)> {
        rtl8168::Rtl8168::link_speed(self)
    }
    fn send_window(&self) -> usize {
        rtl8168::SEND_WINDOW
    }
    fn phy_regs(&mut self, renegotiate: bool) -> Vec<(u32, u16)> {
        rtl8168::Rtl8168::phy_regs(self, renegotiate)
    }
    fn link_changed(&mut self, up: bool) {
        rtl8168::Rtl8168::link_changed(self, up)
    }
    fn interrupts(&self) -> bool {
        self.irq
    }
    fn ack_irq(&mut self) {
        rtl8168::Rtl8168::ack_irq(self)
    }
}

impl Nic for E1000 {
    fn mac(&self) -> Mac {
        self.mac
    }
    fn send(&mut self, frame: &[u8]) -> bool {
        E1000::send(self, frame)
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        E1000::recv(self)
    }
    fn link_up(&self) -> bool {
        E1000::link_up(self)
    }
    fn link_speed(&self) -> Option<(u32, bool)> {
        Some(E1000::link_speed(self))
    }
}

/// The name of the active network card (None — no card).
pub fn nic_name() -> Option<&'static str> {
    NET.lock().as_ref().and_then(|n| n.ifaces.get(n.primary()).map(|i| i.name))
}

pub fn init(devices: &[crate::pci::Device]) -> String {
    if let Some(nic) = rtl8168::Rtl8168::probe(devices) {
        attach(Box::new(nic), "Realtek RTL8168");
        return "Network: Realtek RTL8168".into();
    }
    match E1000::probe(devices) {
        Some(nic) => {
            attach(Box::new(nic), "Intel e1000");
            "Network: Intel e1000".into()
        }
        None => {
            kprintln!("net: no PCI network card (USB Ethernet can be plugged in)");
            "Network: none".into()
        }
    }
}

/// Attaches a network card (including on the fly — USB). If a card of the same kind was already there,
/// the new one replaces it, and the address is requested again. A card of the other kind
/// becomes a second active interface.
pub fn attach(nic: Box<dyn Nic>, name: &'static str) {
    let wireless = nic.wireless();
    let mut guard = NET.lock();
    let Some(n) = guard.as_mut() else {
        // The first card starts the stack (the thread, loading settings/routes).
        drop(guard);
        install(nic, name);
        return;
    };
    let now = interrupts::ticks();
    // A card of the same kind (reconnection/replacement) — replace its interface in place,
    // a card of the other kind — a second active interface (cable and Wi-Fi work at once).
    match n.ifaces.iter().position(|i| i.wireless == wireless) {
        Some(slot) => n.ifaces[slot] = new_iface(nic, name, now),
        None => n.ifaces.push(new_iface(nic, name, now)),
    }
    kprintln!("net: {name} added as {} interface ({} active)", if wireless { "wireless" } else { "wired" }, n.ifaces.len());
}

/// Switches to the spare card if this one lost its link. A USB adapter claims a link
/// even without a cable: such an adapter is recognised by DHCP silence.
pub fn pick_adapter() {
    let spare = {
        let mut net = NET.lock();
        let Some(n) = net.as_mut() else { return };
        let p = n.primary();
        let i = &n.ifaces[p];
        let silent = !routable(i.ip) && interrupts::ticks() > i.dhcp_started + 10 * HZ;
        let stuck = !i.nic.link_up() || (silent && !i.wireless);
        if !stuck || !n.spare.as_ref().is_some_and(|(s, _)| s.link_up()) {
            return;
        }
        n.spare.take()
    };
    if let Some((nic, name)) = spare {
        install(nic, name);
    }
}

/// A new interface for a just-attached card (DHCP will ask for an address).
fn new_iface(nic: Box<dyn Nic>, name: &'static str, now: u64) -> Iface {
    let mac = nic.mac();
    let wireless = nic.wireless();
    Iface {
        nic,
        name,
        mac,
        ip: [0; 4],
        mask: [0; 4],
        gw: [0; 4],
        dns: [0; 4],
        dhcp: Dhcp::Discovering { xid: crate::arch::rdtsc() as u32, at: 0 },
        dhcp_started: now,
        dhcp_seen: 0,
        dhcp_foreign: 0,
        dhcp_told: false,
        was_up: false,
        ll_probe: None,
        ll_attempt: 0,
        arp: BTreeMap::new(),
        pending: Vec::new(),
        arp_asked: BTreeMap::new(),
        ip_id: 1,
        relearn: [u64::MAX; 2],
        wireless,
        stats: Stats::default(),
    }
}

fn install(nic: Box<dyn Nic>, name: &'static str) {
    static STARTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    let mac = nic.mac();
    let now = interrupts::ticks();
    let mut net = NET.lock();
    // A card of the other kind stays in reserve, one of the same kind is replaced.
    let spare = match net.take() {
        Some(old) => {
            let primary_wireless = old.ifaces.first().is_some_and(|i| i.wireless);
            if primary_wireless != nic.wireless() {
                old.ifaces.into_iter().next().map(|i| (i.nic, i.name))
            } else {
                old.spare
            }
        }
        None => None,
    };
    *net = Some(Net {
        ifaces: alloc::vec![new_iface(nic, name, now)],
        spare,
        routes: Vec::new(),
        udp: BTreeMap::new(),
        pings: BTreeMap::new(),
        tcp: Vec::new(),
        listeners: Vec::new(),
        next_port: 49152 + (crate::arch::rdtsc() % 1000) as u16,
        next_id: 1,
    });
    drop(net);
    kprintln!("net: {name} {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
    // Manual addresses from the settings (if set) — for the new card too.
    load_config();
    // Permanent routes from the file into the live table.
    load_routes();
    // The name — before the first DHCP request, which will go from the "net" thread.
    identity::get();
    if !STARTED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        CLOCK_TICK.store(interrupts::ticks(), Ordering::Relaxed);
        CLOCK_TSC.store(crate::arch::rdtsc(), Ordering::Relaxed);
        task::spawn("net", None, || loop {
            let frames = poll();
            let (wireless, irq) = NET.lock().as_ref().and_then(|n| n.ifaces.get(n.primary()).map(|i| (i.wireless, i.nic.interrupts()))).unwrap_or((false, false));
            if irq && !wireless {
                // The card calls with an interrupt; the stack timers (retries, acknowledgements) run by tick.
                if frames > 0 {
                    task::yield_now();
                } else {
                    STACK_WAKE.wait_until(interrupts::ticks() + 1, || RX_PENDING.swap(false, Ordering::AcqRel));
                }
            } else if !exchanging() && wireless {
                // The wireless card is active — its thread has already handled the frames, here are the timers;
                // but while waiting for an answer (traffic), poll often, as for the cable.
                task::sleep_until(interrupts::ticks() + 1);
            } else {
                pause(frames > 0);
            }
        });
    }
}

/// How long the thread waits for new frames during traffic. The receive ring does not
/// fill up in this time, and the CPU gets some rest.
pub(crate) const NAP_CYCLES: u64 = 400_000;

/// Wait for the next step: during traffic the card is polled often, but not non-stop.
/// `worked` — this step had frames: then the next ones have most likely arrived already.
pub(crate) fn pause(worked: bool) {
    if !interrupts::fast_wakeups(exchanging()) {
        if exchanging() {
            task::yield_now();
        } else {
            task::sleep_until(interrupts::ticks() + 1);
        }
    } else if worked {
        task::yield_now();
    } else {
        task::nap(NAP_CYCLES);
    }
}

/// One stack step: received frames and timers. Returns how many frames were handled.
pub(crate) fn poll() -> u64 {
    let mut guard = NET.lock();
    let Some(net) = guard.as_mut() else { return 0 };
    const BUDGET: u64 = 256;
    let mut budget = BUDGET;
    // Measurement: a stack step runs under NET with interrupts disabled — a long step goes to the log.
    let rd = crate::arch::rdtsc;
    let start = rd();
    let (mut t_recv, mut t_frames, mut slowest) = (0u64, 0u64, (0u64, 0usize, 0u16));
    for link in 0..net.ifaces.len() {
        net.ifaces[link].nic.ack_irq();
        while budget > 0 {
            let t = rd();
            let Some(frame) = net.ifaces[link].nic.recv() else {
                t_recv += rd() - t;
                break;
            };
            let t1 = rd();
            t_recv += t1 - t;
            net.ifaces[link].stats.rx_packets += 1;
            net.ifaces[link].stats.rx_bytes += frame.len() as u64;
            net.handle_frame(link, &frame);
            let spent = rd() - t1;
            t_frames += spent;
            if spent > slowest.0 {
                slowest = (spent, frame.len(), frame.get(12..14).map_or(0, |k| u16::from_be_bytes([k[0], k[1]])));
            }
            budget -= 1;
        }
    }
    count_frames(BUDGET - budget);
    let t2 = rd();
    let now = interrupts::ticks();
    for i in 0..net.tcp.len() {
        if net.tcp[i].ack_owed > 0 && now > net.tcp[i].ack_tick {
            let nxt = net.tcp[i].snd_nxt;
            net.tcp_segment(i, nxt, 0x10, &[]);
        }
    }
    let t3 = rd();
    net.timers();
    let end = rd();
    if end - start > 100_000_000 {
        slow_poll([t_recv, t_frames, t3 - t2, end - t3], BUDGET - budget, slowest, net.tcp.len());
    }
    drop(guard);
    if budget < BUDGET {
        CHANGES.fetch_add(1, Ordering::Release);
        CHANGED.notify();
    }
    BUDGET - budget
}

#[cold]
#[inline(never)]
fn slow_poll(parts: [u64; 4], frames: u64, slowest: (u64, usize, u16), conns: usize) {
    let m = |v: u64| v / 1_000_000;
    kprintln!(
        "net: slow step: receive {} M, {frames} frames {} M (slowest {} M: {} bytes, type {:04x}), acks {} M, timers {} M; {conns} tcp",
        m(parts[0]),
        m(parts[1]),
        m(slowest.0),
        slowest.1,
        slowest.2,
        m(parts[2]),
        m(parts[3])
    );
}

impl Net {
    // ------------------------------------------------------------ sending

    fn send_eth(&mut self, link: usize, dst: Mac, kind: u16, payload: &[u8]) {
        // A virtual switch under a Linux bridge (VMware) sends our
        // broadcast frames back, and the bridge starts to think we are behind
        // the external port: everything addressed to us is lost until we send
        // an ordinary frame. So right after such a frame we send a unicast one.
        let n = &mut self.ifaces[link];
        if dst[0] & 1 != 0 {
            let now = interrupts::ticks();
            n.relearn = [now + 2, now + HZ / 3];
        }
        let mut f = Vec::with_capacity(14 + payload.len());
        f.extend_from_slice(&dst);
        f.extend_from_slice(&n.mac);
        f.extend_from_slice(&kind.to_be_bytes());
        f.extend_from_slice(payload);
        if n.nic.send(&f) {
            n.stats.tx_packets += 1;
            n.stats.tx_bytes += f.len() as u64;
        } else {
            n.stats.tx_dropped += 1;
        }
    }

    fn arp_request(&mut self, link: usize, ip: Ip) {
        let now = interrupts::ticks();
        let n = &mut self.ifaces[link];
        if n.arp_asked.get(&ip).is_some_and(|&t| now < t + HZ / 2) {
            return;
        }
        n.arp_asked.insert(ip, now);
        let mut p = vec![0, 1, 8, 0, 6, 4, 0, 1];
        p.extend_from_slice(&n.mac);
        p.extend_from_slice(&n.ip);
        p.extend_from_slice(&[0; 6]);
        p.extend_from_slice(&ip);
        self.send_eth(link, BROADCAST_MAC, ETH_ARP, &p);
    }

    /// A unicast ARP request to the gateway: checking the entry also shows the bridge where we are.
    fn arp_refresh(&mut self, link: usize) {
        let n = &self.ifaces[link];
        let (gw, mymac, myip, known) = (n.gw, n.mac, n.ip, n.arp.get(&n.gw).copied());
        let Some(mac) = known else {
            // The gateway address is not known yet (just booted) — ask the usual way.
            if gw != [0; 4] {
                self.arp_request(link, gw);
            }
            return;
        };
        let mut p = vec![0, 1, 8, 0, 6, 4, 0, 1];
        p.extend_from_slice(&mymac);
        p.extend_from_slice(&myip);
        p.extend_from_slice(&[0; 6]);
        p.extend_from_slice(&gw);
        self.send_eth(link, mac, ETH_ARP, &p);
    }

    fn next_hop(&self, link: usize, dst: Ip) -> Ip {
        // The most specific user route (a longer mask wins) goes first.
        if let Some(r) = self
            .routes
            .iter()
            .filter(|r| (0..4).all(|i| dst[i] & r.mask[i] == r.dest[i]))
            .max_by_key(|r| mask_len(r.mask))
        {
            return if r.gw == [0; 4] { dst } else { r.gw };
        }
        let n = &self.ifaces[link];
        if in_subnet(dst, n.ip, n.mask) || n.gw == [0; 4] { dst } else { n.gw }
    }

    /// The broadcast address of the interface's subnet.
    fn subnet_broadcast(&self, link: usize) -> Ip {
        let n = &self.ifaces[link];
        core::array::from_fn(|i| n.ip[i] | !n.mask[i])
    }

    fn send_ip(&mut self, link: usize, dst: Ip, proto: u8, payload: &[u8]) {
        let (myip, ip_id) = {
            let n = &mut self.ifaces[link];
            let id = n.ip_id;
            n.ip_id = n.ip_id.wrapping_add(1);
            (n.ip, id)
        };
        let mut p = Vec::with_capacity(20 + payload.len());
        p.extend_from_slice(&[0x45, 0]);
        p.extend_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
        p.extend_from_slice(&ip_id.to_be_bytes());
        // 224.0.0.x groups live only on the local network, other multicasts get
        // one hop too: announcements have no business beyond the router.
        // mDNS and LLMNR answers get 255 too: that way the receiver sees they come from this network.
        let local_reply = proto == PROTO_UDP && matches!(payload.get(..2), Some([0x14, 0xE9 | 0xEB]));
        let ttl = if dst[..3] == [224, 0, 0] || local_reply { 255 } else if is_multicast(dst) { 1 } else { 64 };
        p.extend_from_slice(&[0x40, 0, ttl, proto, 0, 0]);
        p.extend_from_slice(&myip);
        p.extend_from_slice(&dst);
        let sum = checksum(&[&p[..20]]);
        p[10..12].copy_from_slice(&sum.to_be_bytes());
        p.extend_from_slice(payload);
        if dst == BROADCAST || (dst == self.subnet_broadcast(link) && myip != [0; 4]) {
            self.send_eth(link, BROADCAST_MAC, ETH_IP, &p);
            return;
        }
        if is_multicast(dst) {
            self.send_eth(link, [0x01, 0x00, 0x5E, dst[1] & 0x7F, dst[2], dst[3]], ETH_IP, &p);
            return;
        }
        let hop = self.next_hop(link, dst);
        let known = self.ifaces[link].arp.get(&hop).copied();
        match known {
            Some(mac) => self.send_eth(link, mac, ETH_IP, &p),
            None => {
                self.ifaces[link].pending.push((hop, p, interrupts::ticks()));
                self.arp_request(link, hop);
            }
        }
    }

    fn send_udp(&mut self, link: usize, dst: Ip, sport: u16, dport: u16, data: &[u8]) {
        let mut u = Vec::with_capacity(8 + data.len());
        u.extend_from_slice(&sport.to_be_bytes());
        u.extend_from_slice(&dport.to_be_bytes());
        u.extend_from_slice(&((8 + data.len()) as u16).to_be_bytes());
        u.extend_from_slice(&[0, 0]); // the UDP checksum is optional in IPv4
        u.extend_from_slice(data);
        self.send_ip(link, dst, PROTO_UDP, &u);
    }

    // ------------------------------------------------------------ receiving

    fn handle_frame(&mut self, link: usize, f: &[u8]) {
        if f.len() < 14 {
            return;
        }
        // A frame for us personally (not broadcast) — someone is talking to us: for the next second the stack and
        // the Wi-Fi thread poll the cards often rather than once a tick, so the answer does not wait up to 10 ms.
        if f[0] & 1 == 0 {
            BUSY_UNTIL.fetch_max(interrupts::ticks() + HZ, Ordering::Relaxed);
        }
        match be16(f, 12) {
            ETH_ARP => self.handle_arp(link, &f[14..]),
            ETH_IP => self.handle_ip(link, &f[14..]),
            _ => {}
        }
    }

    fn handle_arp(&mut self, link: usize, p: &[u8]) {
        if p.len() < 28 || be16(p, 0) != 1 || be16(p, 2) != 0x0800 {
            return;
        }
        let sha: Mac = p[8..14].try_into().unwrap();
        let spa: Ip = p[14..18].try_into().unwrap();
        let tpa: Ip = p[24..28].try_into().unwrap();
        // Someone already took the 169.254.x.y address (or is probing it at the same time) — take another one.
        let n = &mut self.ifaces[link];
        if sha != n.mac {
            let probing = n.ll_probe.is_some_and(|(ip, ..)| spa == ip || (spa == [0; 4] && tpa == ip));
            let taken = link_local(n.ip) && spa == n.ip && !matches!(n.dhcp, Dhcp::Bound { .. } | Dhcp::Renewing { .. } | Dhcp::Manual);
            if probing || taken {
                kprintln!("net: {} link-local address {} is taken, trying another", n.name, ip_str(n.ll_probe.map_or(n.ip, |p| p.0)));
                n.ll_probe = None;
                n.ll_attempt += 1;
                if taken {
                    (n.ip, n.mask) = ([0; 4], [0; 4]);
                    CHANGES.fetch_add(1, Ordering::Release);
                }
            }
        }
        if spa != [0; 4] {
            self.ifaces[link].arp.insert(spa, sha);
            // Packets that waited for this address — on their way.
            let (ready, rest): (Vec<_>, Vec<_>) = core::mem::take(&mut self.ifaces[link].pending).into_iter().partition(|(ip, ..)| *ip == spa);
            self.ifaces[link].pending = rest;
            for (_, packet, _) in ready {
                self.send_eth(link, sha, ETH_IP, &packet);
            }
        }
        let (myip, mymac) = {
            let n = &self.ifaces[link];
            (n.ip, n.mac)
        };
        if be16(p, 6) == 1 && tpa == myip && myip != [0; 4] {
            let mut r = vec![0, 1, 8, 0, 6, 4, 0, 2];
            r.extend_from_slice(&mymac);
            r.extend_from_slice(&myip);
            r.extend_from_slice(&sha);
            r.extend_from_slice(&spa);
            self.send_eth(link, sha, ETH_ARP, &r);
        }
    }

    fn handle_ip(&mut self, link: usize, p: &[u8]) {
        if p.len() < 20 || p[0] >> 4 != 4 {
            return;
        }
        let ihl = (p[0] & 0xF) as usize * 4;
        let total = (be16(p, 2) as usize).min(p.len());
        if total < ihl || be16(p, 6) & 0x3FFF != 0 {
            return; // fragments are not reassembled
        }
        let src: Ip = p[12..16].try_into().unwrap();
        let dst: Ip = p[16..20].try_into().unwrap();
        let myip = self.ifaces[link].ip;
        // Broadcast and multicast — only for UDP: neither ping nor TCP answers those.
        let shared = dst == BROADCAST || is_multicast(dst) || dst == self.subnet_broadcast(link);
        if dst != myip && !shared && myip != [0; 4] {
            return;
        }
        if shared && p[9] != PROTO_UDP {
            return;
        }
        let body = &p[ihl..total];
        match p[9] {
            PROTO_ICMP => self.handle_icmp(link, src, body),
            PROTO_UDP => self.handle_udp(link, src, body),
            PROTO_TCP => self.handle_tcp(link, src, body),
            _ => {}
        }
    }

    fn handle_icmp(&mut self, link: usize, src: Ip, b: &[u8]) {
        if b.len() < 8 {
            return;
        }
        match b[0] {
            8 => {
                let mut r = b.to_vec();
                r[0] = 0;
                r[2..4].fill(0);
                let sum = checksum(&[&r]);
                r[2..4].copy_from_slice(&sum.to_be_bytes());
                self.send_ip(link, src, PROTO_ICMP, &r);
            }
            0 => {
                if let Some(slot) = self.pings.get_mut(&(be16(b, 4), be16(b, 6))) {
                    *slot = Some(crate::arch::rdtsc());
                }
            }
            _ => {}
        }
    }

    fn handle_udp(&mut self, link: usize, src: Ip, b: &[u8]) {
        if b.len() < 8 {
            return;
        }
        let (sport, dport) = (be16(b, 0), be16(b, 2));
        let len = (be16(b, 4) as usize).clamp(8, b.len());
        let data = &b[8..len];
        if dport == 68 {
            self.handle_dhcp(link, data);
        } else if let Some(q) = self.udp.get_mut(&dport)
            && q.len() < 64
        {
            q.push((src, sport, data.to_vec()));
        }
    }

    // ------------------------------------------------------------ DHCP

    /// `ciaddr` — our address when renewing a lease; without it (discovery, checking the old
    /// address) the packet goes from 0.0.0.0, even if 169.254.x.y was taken.
    fn send_dhcp(&mut self, link: usize, kind: u8, xid: u32, ciaddr: Ip, extra: &[(u8, &[u8])]) {
        let mut d = vec![0u8; 240];
        d[0] = 1;
        d[1] = 1;
        d[2] = 6;
        d[4..8].copy_from_slice(&xid.to_be_bytes());
        d[10] = 0x80; // broadcast reply (we may have no address)
        d[12..16].copy_from_slice(&ciaddr);
        d[28..34].copy_from_slice(&self.ifaces[link].mac);
        d[236..240].copy_from_slice(&[99, 130, 83, 99]);
        d.extend_from_slice(&[53, 1, kind]);
        for (code, value) in extra {
            d.push(*code);
            d.push(value.len() as u8);
            d.extend_from_slice(value);
        }
        // The name — so the router shows it in its client list. Only a ready one:
        // identity::get reads a file and asks for the MAC, and NET is already locked here.
        if let Some(id) = identity::cached() {
            d.extend_from_slice(&[12, id.name.len() as u8]);
            d.extend_from_slice(id.name.as_bytes());
        }
        d.extend_from_slice(&[55, 3, 1, 3, 6, 255]); // we want: mask, router, DNS
        let own = self.ifaces[link].ip;
        if ciaddr == [0; 4] {
            self.ifaces[link].ip = [0; 4];
        }
        self.send_udp(link, BROADCAST, 68, 67, &d);
        self.ifaces[link].ip = own;
    }

    fn handle_dhcp(&mut self, link: usize, d: &[u8]) {
        if d.len() < 240 || d[0] != 2 || d[236..240] != [99, 130, 83, 99] {
            return;
        }
        let xid = be32(d, 4);
        let yiaddr: Ip = d[16..20].try_into().unwrap();
        {
            let n = &mut self.ifaces[link];
            n.dhcp_seen += 1;
            let ours = match n.dhcp {
                Dhcp::Discovering { xid: x, .. } | Dhcp::Requesting { xid: x, .. } | Dhcp::Renewing { xid: x, .. } => x == xid,
                _ => false,
            };
            if !ours {
                n.dhcp_foreign += 1;
            }
        }
        let mut opts: BTreeMap<u8, &[u8]> = BTreeMap::new();
        let mut i = 240;
        while i < d.len() && d[i] != 255 {
            if d[i] == 0 {
                i += 1;
                continue;
            }
            let (code, len) = (d[i], *d.get(i + 1).unwrap_or(&0) as usize);
            if let Some(v) = d.get(i + 2..i + 2 + len) {
                opts.insert(code, v);
            }
            i += 2 + len;
        }
        let kind = opts.get(&53).map_or(0, |v| v[0]);
        let ip4 = |code: u8| opts.get(&code).filter(|v| v.len() >= 4).map(|v| [v[0], v[1], v[2], v[3]]);
        match self.ifaces[link].dhcp {
            Dhcp::Discovering { xid: x, .. } if x == xid && kind == 2 => {
                let server = ip4(54).unwrap_or([0; 4]);
                self.send_dhcp(link, 3, xid, [0; 4], &[(50, &yiaddr), (54, &server)]);
                self.ifaces[link].dhcp = Dhcp::Requesting { xid, at: interrupts::ticks(), offered: yiaddr, server };
            }
            Dhcp::Requesting { xid: x, .. } | Dhcp::Renewing { xid: x, .. } if x == xid && kind == 5 => {
                let ip = match self.ifaces[link].dhcp {
                    Dhcp::Requesting { offered, .. } => offered,
                    _ => [0; 4],
                };
                let now = interrupts::ticks();
                // The lease time (option 51, seconds); missing — assume an hour, "forever" — no renewal.
                let lease = opts.get(&51).filter(|v| v.len() >= 4).map_or(3600, |v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]));
                let (renew, expire) = match lease {
                    u32::MAX => (u64::MAX, u64::MAX),
                    secs => (now + secs as u64 * HZ / 2, now + secs as u64 * HZ),
                };
                let renewed = matches!(self.ifaces[link].dhcp, Dhcp::Renewing { .. });
                let n = &mut self.ifaces[link];
                let ip = if yiaddr != [0; 4] { yiaddr } else if routable(ip) { ip } else { n.ip };
                let changed = ip != n.ip;
                if link_local(n.ip) || changed {
                    n.arp.clear();
                }
                n.ip = ip;
                n.mask = ip4(1).unwrap_or([255, 255, 255, 0]);
                n.gw = ip4(3).unwrap_or([0; 4]);
                n.dns = ip4(6).unwrap_or(n.gw);
                n.dhcp = Dhcp::Bound { renew, expire };
                n.ll_probe = None;
                if renewed && !changed {
                    kprintln!("net: {} dhcp lease renewed: {} for {lease} s", n.name, ip_str(n.ip));
                } else {
                    kprintln!("net: {} dhcp address {} gateway {} dns {} (lease {lease} s)", n.name, ip_str(n.ip), ip_str(n.gw), ip_str(n.dns));
                }
                CHANGES.fetch_add(1, Ordering::Release);
            }
            Dhcp::Requesting { xid: x, .. } if x == xid && kind == 6 => {
                self.ifaces[link].dhcp = Dhcp::Discovering { xid: xid.wrapping_add(1), at: 0 };
            }
            Dhcp::Renewing { xid: x, .. } if x == xid && kind == 6 => {
                // The old address is not valid here (another network, or the lease was given away) — ask for a new one.
                let n = &mut self.ifaces[link];
                kprintln!("net: {} DHCP server refused {}, asking for a new address", n.name, ip_str(n.ip));
                n.forget_address(interrupts::ticks());
                CHANGES.fetch_add(1, Ordering::Release);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ timers

    /// The cable was plugged in or out. Out — the card advertises all its speeds again
    /// (after a failed gigabit attempt the PHY may have dropped it). In — the network may
    /// have changed: check the address with DHCP right away, without waiting for the lease term.
    fn link_changed(&mut self, link: usize, up: bool) {
        let n = &mut self.ifaces[link];
        n.nic.link_changed(up);
        if !up {
            kprintln!("net: {} cable unplugged", n.name);
            return;
        }
        match n.nic.link_speed() {
            Some((mbit, full)) => kprintln!("net: {} link up, {mbit} Mbit/s {} duplex", n.name, if full { "full" } else { "half" }),
            None => kprintln!("net: {} link up", n.name),
        }
        n.arp.clear();
        n.pending.clear();
        let xid = crate::arch::rdtsc() as u32;
        n.dhcp = match n.dhcp {
            Dhcp::Bound { expire, .. } | Dhcp::Renewing { expire, .. } => Dhcp::Renewing { xid, at: 0, tries: 0, expire, reboot: true },
            Dhcp::Discovering { .. } | Dhcp::Requesting { .. } => Dhcp::Discovering { xid, at: 0 },
            Dhcp::Manual => Dhcp::Manual,
        };
    }

    /// DHCP: finding a server, renewing the lease and checking the old address.
    fn dhcp_timer(&mut self, link: usize, now: u64, up: bool) {
        match self.ifaces[link].dhcp {
            // For the first 10 s ask every 2 s, then every 10 s.
            Dhcp::Discovering { xid, at } | Dhcp::Requesting { xid, at, .. } if up && now >= at + if now > self.ifaces[link].dhcp_started + 10 * HZ { 10 * HZ } else { 2 * HZ } => {
                let n = &mut self.ifaces[link];
                if now > n.dhcp_started + 10 * HZ && !n.dhcp_told {
                    n.dhcp_told = true;
                    kprintln!("net: {} no DHCP answer yet ({} DHCP replies seen, {} for others), still asking", n.name, n.dhcp_seen, n.dhcp_foreign);
                }
                self.send_dhcp(link, 1, xid, [0; 4], &[]);
                self.ifaces[link].dhcp = Dhcp::Discovering { xid, at: now };
            }
            Dhcp::Bound { renew, expire } if now >= renew => {
                self.ifaces[link].dhcp = Dhcp::Renewing { xid: crate::arch::rdtsc() as u32, at: 0, tries: 0, expire, reboot: false };
            }
            Dhcp::Renewing { xid, at, tries, expire, reboot } if up && now >= at + if reboot { 2 * HZ } else { 10 * HZ } => {
                let ip = self.ifaces[link].ip;
                if now >= expire {
                    let n = &mut self.ifaces[link];
                    kprintln!("net: {} dhcp lease of {} expired, asking for a new address", n.name, ip_str(ip));
                    n.forget_address(now);
                    CHANGES.fetch_add(1, Ordering::Release);
                } else if reboot && tries >= 3 {
                    // The server is silent and the lease is still valid — keep the address (as Windows does)
                    // and renew it the usual way in a minute.
                    self.ifaces[link].dhcp = Dhcp::Bound { renew: now + 60 * HZ, expire };
                } else if reboot {
                    self.send_dhcp(link, 3, xid, [0; 4], &[(50, &ip)]);
                    self.ifaces[link].dhcp = Dhcp::Renewing { xid, at: now, tries: tries + 1, expire, reboot };
                } else {
                    self.send_dhcp(link, 3, xid, ip, &[]);
                    self.ifaces[link].dhcp = Dhcp::Renewing { xid, at: now, tries: tries.saturating_add(1), expire, reboot };
                }
            }
            _ => {}
        }
    }

    /// No DHCP answer for more than 10 s — take a 169.254.x.y address (RFC 3927), as Windows and
    /// macOS do: with it neighbours on the cable are visible without a router. DHCP keeps being asked, its address
    /// will replace this one.
    fn link_local_timer(&mut self, link: usize, now: u64) {
        let n = &self.ifaces[link];
        if n.ip != [0; 4] || now <= n.dhcp_started + 10 * HZ || !matches!(n.dhcp, Dhcp::Discovering { .. } | Dhcp::Requesting { .. }) {
            return;
        }
        match n.ll_probe {
            None => {
                let ip = link_local_candidate(n.mac, n.ll_attempt);
                self.arp_probe(link, ip);
                self.ifaces[link].ll_probe = Some((ip, now + HZ / 2, 1));
            }
            Some((ip, at, sent)) if now >= at && sent < 3 => {
                self.arp_probe(link, ip);
                self.ifaces[link].ll_probe = Some((ip, now + HZ / 2, sent + 1));
            }
            Some((ip, at, _)) if now >= at => {
                let n = &mut self.ifaces[link];
                (n.ip, n.mask, n.gw, n.dns) = (ip, [255, 255, 0, 0], [0; 4], [0; 4]);
                n.ll_probe = None;
                kprintln!("net: {} no DHCP answer, using link-local address {} (still asking DHCP)", n.name, ip_str(ip));
                CHANGES.fetch_add(1, Ordering::Release);
                // Announce the address: neighbours update their ARP tables.
                let mac = n.mac;
                let mut a = vec![0, 1, 8, 0, 6, 4, 0, 1];
                a.extend_from_slice(&mac);
                a.extend_from_slice(&ip);
                a.extend_from_slice(&[0; 6]);
                a.extend_from_slice(&ip);
                self.send_eth(link, BROADCAST_MAC, ETH_ARP, &a);
            }
            _ => {}
        }
    }

    /// An ARP probe (RFC 5227): "has anyone taken this address?" from address 0.0.0.0.
    fn arp_probe(&mut self, link: usize, ip: Ip) {
        let mut a = vec![0, 1, 8, 0, 6, 4, 0, 1];
        a.extend_from_slice(&self.ifaces[link].mac);
        a.extend_from_slice(&[0; 4]);
        a.extend_from_slice(&[0; 6]);
        a.extend_from_slice(&ip);
        self.send_eth(link, BROADCAST_MAC, ETH_ARP, &a);
    }

    fn timers(&mut self) {
        let now = interrupts::ticks();
        for link in 0..self.ifaces.len() {
            if now >= self.ifaces[link].relearn[0] {
                self.ifaces[link].relearn = [self.ifaces[link].relearn[1], u64::MAX];
                self.arp_refresh(link);
            }
            let up = self.ifaces[link].nic.link_up();
            if !self.ifaces[link].wireless && up != self.ifaces[link].was_up {
                self.ifaces[link].was_up = up;
                self.link_changed(link, up);
            }
            // Without a link no address will come: the DHCP wait count starts when there is one.
            if !up {
                self.ifaces[link].dhcp_started = now;
            }
            self.dhcp_timer(link, now, up);
            if !self.ifaces[link].wireless && up {
                self.link_local_timer(link, now);
            }
            // Unanswered ARPs: retry, drop the packets after 3 s.
            self.ifaces[link].pending.retain(|(_, _, at)| now < at + 3 * HZ);
            let waiting: Vec<Ip> = self.ifaces[link].pending.iter().map(|(ip, ..)| *ip).collect();
            for ip in waiting {
                self.arp_request(link, ip);
            }
        }
        for i in 0..self.tcp.len() {
            self.tcp_output(i);
        }
        // Remove closed connections the program has already abandoned.
        self.tcp.retain(|t| t.state != State::Closed || t.id != 0);
    }

    // ------------------------------------------------------------ TCP

    fn tcp_segment(&mut self, i: usize, seq: u32, flags: u8, data: &[u8]) {
        if flags & 0x10 != 0 {
            self.tcp[i].ack_owed = 0;
        }
        let t = &self.tcp[i];
        let syn = flags & 0x02 != 0;
        // The connecting side offers scaling first; it can only be answered in kind.
        let scaled = flags == 0x02 || (syn && t.rcv_scale != 0);
        let hlen = if scaled { 28 } else if syn { 24 } else { 20 };
        let mut s = Vec::with_capacity(hlen + data.len());
        s.extend_from_slice(&t.lport.to_be_bytes());
        s.extend_from_slice(&t.rport.to_be_bytes());
        s.extend_from_slice(&seq.to_be_bytes());
        s.extend_from_slice(&(if flags & 0x10 != 0 { t.rcv_nxt } else { 0 }).to_be_bytes());
        s.push(((hlen / 4) as u8) << 4);
        s.push(flags);
        let room = RECEIVE_WINDOW.saturating_sub(t.inq.len());
        let wnd = if syn { room } else { room >> t.rcv_scale }.min(65535) as u16;
        s.extend_from_slice(&wnd.to_be_bytes());
        s.extend_from_slice(&[0, 0, 0, 0]);
        if syn {
            s.extend_from_slice(&[2, 4]);
            s.extend_from_slice(&(MSS as u16).to_be_bytes());
        }
        if scaled {
            s.extend_from_slice(&[1, 3, 3, WINDOW_SCALE]);
        }
        s.extend_from_slice(data);
        let (rip, lip, link) = (t.rip, t.lip, t.link);
        let mut pseudo = Vec::with_capacity(12);
        pseudo.extend_from_slice(&lip);
        pseudo.extend_from_slice(&rip);
        pseudo.extend_from_slice(&[0, PROTO_TCP]);
        pseudo.extend_from_slice(&(s.len() as u16).to_be_bytes());
        let sum = checksum(&[&pseudo, &s]);
        s[16..18].copy_from_slice(&sum.to_be_bytes());
        self.send_ip(link, rip, PROTO_TCP, &s);
    }

    /// A RST answer to segment `s` without a connection. Without it the client waits for a timeout on
    /// every closed port: when opening \\\\computer Windows tries both 139
    /// and 80 (WebDAV), and the first login was delayed by 10–15 seconds.
    fn tcp_reset(&mut self, link: usize, src: Ip, s: &[u8], len: usize) {
        let (flags, seq, ack) = (s[13], be32(s, 4), be32(s, 8));
        let mut r = Vec::with_capacity(20);
        r.extend_from_slice(&s[2..4]);
        r.extend_from_slice(&s[0..2]);
        if flags & 0x10 != 0 {
            r.extend_from_slice(&ack.to_be_bytes());
            r.extend_from_slice(&0u32.to_be_bytes());
            r.extend_from_slice(&[5 << 4, 0x04]);
        } else {
            let syn_fin = (flags & 0x03 != 0) as u32 + (flags & 0x03 == 0x03) as u32;
            r.extend_from_slice(&0u32.to_be_bytes());
            r.extend_from_slice(&seq.wrapping_add(len as u32 + syn_fin).to_be_bytes());
            r.extend_from_slice(&[5 << 4, 0x14]);
        }
        r.extend_from_slice(&[0; 6]);
        let mut pseudo = Vec::with_capacity(12);
        pseudo.extend_from_slice(&self.ifaces[link].ip);
        pseudo.extend_from_slice(&src);
        pseudo.extend_from_slice(&[0, PROTO_TCP, 0, 20]);
        let sum = checksum(&[&pseudo, &r]);
        r[16..18].copy_from_slice(&sum.to_be_bytes());
        self.send_ip(link, src, PROTO_TCP, &r);
    }

    /// Sends everything it can and retransmits unacknowledged data on the timer.
    fn tcp_output(&mut self, i: usize) {
        let now = interrupts::ticks();
        let t = &self.tcp[i];
        match t.state {
            State::Closed | State::FinWait2 => return,
            State::SynSent | State::SynReceived => {
                if now >= t.rto_at {
                    let (iss, flags) = (t.iss, if t.state == State::SynSent { 0x02 } else { 0x12 });
                    if self.tcp_retry(i, now) {
                        self.tcp_segment(i, iss, flags, &[]);
                    }
                }
                return;
            }
            _ => {}
        }
        // New data, while the receiver's window allows.
        let limit = self.ifaces.get(self.tcp[i].link).map_or(WINDOW, |n| n.nic.send_window());
        loop {
            let t = &mut self.tcp[i];
            let window = t.peer_wnd.min(limit).saturating_sub(t.unacked.len());
            let n = t.outq.len().min(MSS).min(window);
            if n == 0 {
                break;
            }
            // The start of a request: we want the answer right away.
            if t.unacked.is_empty() {
                BUSY_UNTIL.fetch_max(now + REPLY_TICKS, Ordering::Relaxed);
            }
            // In memory chunks, not byte by byte: the queue is a ring of two halves.
            let mut chunk = Vec::with_capacity(n);
            let (front, back) = t.outq.as_slices();
            let first = front.len().min(n);
            chunk.extend_from_slice(&front[..first]);
            chunk.extend_from_slice(&back[..n - first]);
            t.outq.drain(..n);
            let seq = t.snd_nxt;
            t.snd_nxt = t.snd_nxt.wrapping_add(n as u32);
            t.unacked.extend(&chunk);
            if t.rtt_probe.is_none() && t.recover.is_none() {
                t.rtt_probe = Some((t.snd_nxt, now));
            }
            if t.rto_at == u64::MAX {
                t.rto_at = now + t.rto;
            }
            self.tcp_segment(i, seq, 0x18, &chunk);
        }
        let t = &mut self.tcp[i];
        if t.fin_queued && t.outq.is_empty() && t.fin_seq.is_none() && matches!(t.state, State::Established | State::CloseWait) {
            let seq = t.snd_nxt;
            t.fin_seq = Some(seq);
            t.snd_nxt = seq.wrapping_add(1);
            t.state = if t.state == State::Established { State::FinWait1 } else { State::LastAck };
            if t.rto_at == u64::MAX {
                t.rto_at = now + t.rto;
            }
            self.tcp_segment(i, seq, 0x11, &[]);
        }
        // Retransmission: the first unacknowledged chunk (or FIN).
        let t = &self.tcp[i];
        let fin_pending = t.fin_seq.is_some_and(|f| t.snd_una == f);
        if (!t.unacked.is_empty() || fin_pending) && now >= t.rto_at {
            let (seq, data) = (t.snd_una, t.unacked.iter().take(MSS).copied().collect::<Vec<u8>>());
            self.ifaces[self.tcp[i].link].stats.resent += 1;
            let t = &mut self.tcp[i];
            t.rtt_probe = None;
            t.recover = None;
            t.dup_acks = 0;
            if self.tcp_retry(i, now) {
                if data.is_empty() {
                    self.tcp_segment(i, seq, 0x11, &[]);
                } else {
                    self.tcp_segment(i, seq, 0x18, &data);
                }
            }
        }
    }

    /// Counts a retransmission; false — no attempts left, the connection is closed.
    fn tcp_retry(&mut self, i: usize, now: u64) -> bool {
        let t = &mut self.tcp[i];
        if t.retries >= 6 {
            t.state = State::Closed;
            t.error = Some("connection timed out");
            return false;
        }
        if t.rto_at != 0 {
            t.retries += 1;
        }
        t.rto_at = now + (t.rto << t.retries.min(5));
        true
    }

    fn handle_tcp(&mut self, link: usize, src: Ip, s: &[u8]) {
        if s.len() < 20 {
            return;
        }
        let (sport, dport) = (be16(s, 0), be16(s, 2));
        let (seq, ack) = (be32(s, 4), be32(s, 8));
        let off = ((s[12] >> 4) as usize * 4).min(s.len());
        let flags = s[13];
        let wnd = be16(s, 14) as usize;
        let scale = if flags & 0x02 != 0 { window_scale(s.get(20..off).unwrap_or(&[])) } else { None };
        let data = &s[off..];
        let Some(i) = self.tcp.iter().position(|t| t.lport == dport && t.rport == sport && t.rip == src && t.state != State::Closed)
        else {
            // A new incoming connection on a listening port: answer SYN+ACK.
            if flags & 0x12 == 0x02 && self.listeners.iter().any(|(p, _)| *p == dport) {
                let id = self.next_id;
                self.next_id += 1;
                let lip = self.ifaces[link].ip;
                let iss = crate::arch::rdtsc() as u32;
                self.tcp.push(Tcb {
                    id,
                    state: State::SynReceived,
                    lip,
                    link,
                    lport: dport,
                    rip: src,
                    rport: sport,
                    iss,
                    snd_una: iss,
                    snd_nxt: iss.wrapping_add(1),
                    rcv_nxt: seq.wrapping_add(1),
                    peer_wnd: wnd.max(MSS),
                    unacked: VecDeque::new(),
                    outq: VecDeque::new(),
                    inq: Vec::new(),
                    fin_queued: false,
                    fin_seq: None,
                    peer_fin: false,
                    rto: HZ,
                    rto_at: interrupts::ticks() + HZ,
                    retries: 0,
                    srtt: 0,
                    rttvar: 0,
                    rtt_probe: None,
                    dup_acks: 0,
                    recover: None,
                    error: None,
                    ack_owed: 0,
                    ack_tick: 0,
                    rcv_scale: scale.map_or(0, |_| WINDOW_SCALE),
                    snd_scale: scale.unwrap_or(0),
                });
                let i = self.tcp.len() - 1;
                self.tcp_segment(i, iss, 0x12, &[]);
            } else if flags & 0x04 == 0 {
                self.tcp_reset(link, src, s, data.len());
            }
            return;
        };
        let now = interrupts::ticks();
        let t = &mut self.tcp[i];
        if flags & 0x04 != 0 {
            t.error = Some(if t.state == State::SynSent { "connection refused" } else { "connection reset" });
            t.state = State::Closed;
            return;
        }
        if t.state == State::SynReceived {
            if flags & 0x10 == 0 || ack != t.iss.wrapping_add(1) {
                return;
            }
            // The handshake is complete — the connection waits for accept.
            t.snd_una = ack;
            t.state = State::Established;
            t.peer_wnd = wnd << t.snd_scale;
            (t.retries, t.rto_at) = (0, u64::MAX);
            let (id, port) = (t.id, t.lport);
            if let Some((_, queue)) = self.listeners.iter_mut().find(|(p, _)| *p == port) {
                queue.push(id);
            }
            // Data may have come together with the ACK — handle it further as usual.
            if data.is_empty() && flags & 0x01 == 0 {
                return;
            }
        }
        let t = &mut self.tcp[i];
        if t.state == State::SynSent {
            if flags & 0x12 == 0x12 && ack == t.iss.wrapping_add(1) {
                t.rcv_nxt = seq.wrapping_add(1);
                t.snd_una = ack;
                t.snd_nxt = ack;
                t.peer_wnd = wnd;
                if let Some(scale) = scale {
                    (t.rcv_scale, t.snd_scale) = (WINDOW_SCALE, scale);
                }
                t.state = State::Established;
                (t.retries, t.rto_at) = (0, u64::MAX);
                let nxt = t.snd_nxt;
                self.tcp_segment(i, nxt, 0x10, &[]);
            }
            return;
        }
        let mut resend_first = false;
        if flags & 0x10 != 0 {
            let acked = ack.wrapping_sub(t.snd_una) as i32;
            // The window is the same: otherwise this is just a window update, not a duplicate.
            if acked == 0 && data.is_empty() && flags & 0x03 == 0 && !t.unacked.is_empty() && wnd << t.snd_scale == t.peer_wnd {
                // A duplicate acknowledgement: frames arrived after it, and one before them was lost.
                t.dup_acks = t.dup_acks.saturating_add(1);
                if t.dup_acks == DUP_ACKS && t.recover.is_none() {
                    t.recover = Some(t.snd_nxt);
                    t.rtt_probe = None;
                    resend_first = true;
                }
            }
            if acked > 0 && ack.wrapping_sub(t.snd_nxt) as i32 <= 0 {
                let bytes = (acked as usize).min(t.unacked.len());
                t.unacked.drain(..bytes);
                t.snd_una = ack;
                t.retries = 0;
                t.dup_acks = 0;
                if let Some((end, sent)) = t.rtt_probe
                    && ack.wrapping_sub(end) as i32 >= 0
                {
                    t.rtt_probe = None;
                    rtt_sample(t, now - sent);
                }
                // Recovery: not everything sent before the loss was acknowledged — so
                // the next chunk was lost too (NewReno), retransmit it as well.
                if let Some(recover) = t.recover {
                    if ack.wrapping_sub(recover) as i32 >= 0 {
                        t.recover = None;
                    } else {
                        resend_first = true;
                    }
                }
                t.rto_at = if t.unacked.is_empty() { u64::MAX } else { now + t.rto };
                if t.fin_seq.is_some_and(|f| ack == f.wrapping_add(1)) {
                    t.rto_at = u64::MAX;
                    t.state = match t.state {
                        State::FinWait1 if t.peer_fin => State::Closed,
                        State::FinWait1 => State::FinWait2,
                        State::LastAck => State::Closed,
                        other => other,
                    };
                }
            }
            t.peer_wnd = wnd << t.snd_scale;
        }
        let mut reply = false;
        if !data.is_empty() {
            if seq == t.rcv_nxt && t.inq.len() < RECEIVE_WINDOW {
                t.inq.extend_from_slice(data);
                t.rcv_nxt = t.rcv_nxt.wrapping_add(data.len() as u32);
                if t.ack_owed == 0 {
                    t.ack_tick = now;
                }
                t.ack_owed += 1;
                // PSH: the sender reached the end of a message and has nothing more to wait for.
                reply = t.ack_owed >= ACK_EVERY || flags & 0x08 != 0;
            } else {
                self.ifaces[link].stats.rx_disordered += 1;
                reply = true; // out of order — a duplicate acknowledgement right away
            }
        }
        if flags & 0x01 != 0 && seq.wrapping_add(data.len() as u32) == t.rcv_nxt && !t.peer_fin {
            t.rcv_nxt = t.rcv_nxt.wrapping_add(1);
            t.peer_fin = true;
            t.state = match t.state {
                State::Established => State::CloseWait,
                State::FinWait2 => State::Closed,
                other => other,
            };
            reply = true;
        }
        if reply {
            let nxt = self.tcp[i].snd_nxt;
            self.tcp_segment(i, nxt, 0x10, &[]);
        }
        if resend_first {
            let t = &mut self.tcp[i];
            let (seq, chunk) = (t.snd_una, t.unacked.iter().take(MSS).copied().collect::<Vec<u8>>());
            t.rto_at = now + t.rto;
            if !chunk.is_empty() {
                self.ifaces[link].stats.resent += 1;
                self.tcp_segment(i, seq, 0x18, &chunk);
            }
        }
    }
}

// ---------------------------------------------------------------- the API for programs

/// A round-trip time sample (in ticks) — into the estimate and the retransmission timer (RFC 6298, integers ×8 and ×4).
fn rtt_sample(t: &mut Tcb, ticks: u64) {
    let r = ticks * 8;
    if t.srtt == 0 {
        t.srtt = r.max(1);
        t.rttvar = ticks * 2;
    } else {
        let delta = t.srtt.abs_diff(r) / 2; // in units of ×4
        t.rttvar = t.rttvar - t.rttvar / 4 + delta / 4;
        t.srtt = t.srtt - t.srtt / 8 + r / 8;
    }
    t.rto = (t.srtt / 8 + t.rttvar.max(1)).clamp(MIN_RTO, MAX_RTO);
}

/// Waits until `f` returns a value (up to `timeout` ticks), yielding the CPU.
/// Ctrl+C in the terminal interrupts the wait.
fn wait<T>(timeout: u64, mut f: impl FnMut(&mut Net) -> Option<T>) -> Option<T> {
    let until = interrupts::ticks() + timeout;
    loop {
        let seen = CHANGES.load(Ordering::Acquire);
        if let Some(v) = NET.lock().as_mut().and_then(&mut f) {
            return Some(v);
        }
        let now = interrupts::ticks();
        if now >= until || task::stop_reason(now) == 2 {
            return None;
        }
        // Sleep until the stack handles new frames. What timers alone change (a retransmission,
        // a close on timeout) is noticed no later than a couple of ticks later.
        CHANGED.wait_until(until.min(now + 2), || CHANGES.load(Ordering::Acquire) != seen);
    }
}

fn ephemeral(net: &mut Net) -> u16 {
    net.next_port = if net.next_port >= 65000 { 49152 } else { net.next_port + 1 };
    net.next_port
}

/// Waits for an address from DHCP (up to 12 s).
pub fn wait_online() -> bool {
    wait(12 * HZ, |n| n.ifaces.iter().any(|i| routable(i.ip)).then_some(())).is_some()
}

/// The reference for converting CPU cycles to time: the tick and the cycle count at stack start.
static CLOCK_TICK: AtomicU64 = AtomicU64::new(0);
static CLOCK_TSC: AtomicU64 = AtomicU64::new(0);

/// Microseconds in `cycles` cycles. The cycle counter frequency is learned from the system timer
/// since the stack started: the timer steps by 10 ms, but over minutes the accuracy gets better than a percent.
fn tsc_us(cycles: u64) -> u64 {
    let ticks = interrupts::ticks().saturating_sub(CLOCK_TICK.load(Ordering::Relaxed));
    let tsc = crate::arch::rdtsc().saturating_sub(CLOCK_TSC.load(Ordering::Relaxed));
    if ticks < HZ || tsc == 0 {
        return cycles / 3000; // less than a second collected yet — a rough estimate
    }
    (cycles as u128 * ticks as u128 * (1_000_000 / HZ) as u128 / tsc as u128) as u64
}

/// An echo request; the answer time in microseconds or None.
pub fn ping(dst: Ip, seq: u16) -> Option<u64> {
    let id = 0x4753; // «GS»
    let sent;
    {
        let mut guard = NET.lock();
        let net = guard.as_mut()?;
        net.pings.insert((id, seq), None);
        let mut p = vec![8, 0, 0, 0];
        p.extend_from_slice(&id.to_be_bytes());
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(b"GlassOS ping payload....");
        let sum = checksum(&[&p]);
        p[2..4].copy_from_slice(&sum.to_be_bytes());
        let link = net.route(dst);
        sent = crate::arch::rdtsc();
        net.send_ip(link, dst, PROTO_ICMP, &p);
    }
    // Wait for the answer while handling incoming frames right here: the stack thread may have gone to sleep until the next
    // tick (10 ms), and ping would show the timer step rather than the network. The Wi-Fi thread also
    // stops sleeping by tick during the exchange.
    let deadline = interrupts::ticks() + 2 * HZ;
    let got = loop {
        BUSY_UNTIL.fetch_max(interrupts::ticks() + REPLY_TICKS, Ordering::Relaxed);
        poll();
        if let Some(t) = NET.lock().as_ref().and_then(|n| n.pings.get(&(id, seq)).copied().flatten()) {
            break Some(t);
        }
        let now = interrupts::ticks();
        if now >= deadline || task::stop_reason(now) == 2 {
            break None;
        }
        pause(false);
    };
    if let Some(n) = NET.lock().as_mut() {
        n.pings.remove(&(id, seq));
    }
    got.map(|t| tsc_us(t.saturating_sub(sent)))
}

/// A host name → an address (an IPv4 literal or through DNS).
pub fn resolve(host: &str) -> Option<Ip> {
    if let Some(ip) = parse_ip(host) {
        return Some(ip);
    }
    if host.eq_ignore_ascii_case("localhost") {
        return Some([127, 0, 0, 1]);
    }
    // Names of computers on the local network: the provider's DNS does not know them.
    if let Some(ip) = discovery::ip_of(host) {
        return Some(ip);
    }
    if !host.contains('.')
        && let Some(ip) = nbt::lookup(host)
    {
        return Some(ip);
    }
    let id = (crate::arch::rdtsc() as u16) | 1;
    let mut q = Vec::new();
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[1, 0, 0, 1, 0, 0, 0, 0, 0, 0]); // recursive query, 1 question
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]); // type A, class IN
    let port = {
        let mut guard = NET.lock();
        let net = guard.as_mut()?;
        let port = ephemeral(net);
        net.udp.insert(port, Vec::new());
        port
    };
    let mut answer = None;
    for _ in 0..3 {
        if let Some(n) = NET.lock().as_mut() {
            let p = n.primary();
            let dns = n.ifaces.get(p).map_or([0; 4], |i| i.dns);
            let link = n.route(dns);
            n.send_udp(link, dns, port, 53, &q);
        }
        answer = wait(2 * HZ, |n| {
            let queue = n.udp.get_mut(&port)?;
            let i = queue.iter().position(|(_, sp, d)| *sp == 53 && d.len() >= 12 && be16(d, 0) == id)?;
            Some(queue.remove(i).2)
        });
        if answer.is_some() {
            break;
        }
    }
    if let Some(n) = NET.lock().as_mut() {
        n.udp.remove(&port);
    }
    parse_dns(&answer?)
}

/// The first A record from a DNS answer.
fn parse_dns(d: &[u8]) -> Option<Ip> {
    let skip_name = |mut i: usize| -> Option<usize> {
        loop {
            let len = *d.get(i)? as usize;
            if len == 0 {
                return Some(i + 1);
            }
            if len & 0xC0 == 0xC0 {
                return Some(i + 2);
            }
            i += 1 + len;
        }
    };
    let (qd, an) = (be16(d, 4), be16(d, 6));
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(i)? + 4;
    }
    for _ in 0..an {
        i = skip_name(i)?;
        let rr = d.get(i..i + 10)?;
        let (kind, len) = (be16(rr, 0), be16(rr, 8) as usize);
        i += 10;
        if kind == 1 && len == 4 {
            return d.get(i..i + 4)?.try_into().ok();
        }
        i += len;
    }
    None
}

/// Before shutdown: closes connections politely (FIN). Otherwise the client learns about
/// the restart only by sending data on the old connection: Windows then reports
/// an error and reconnects only after a few seconds.
pub fn say_goodbye() {
    {
        let mut guard = NET.lock();
        let Some(n) = guard.as_mut() else { return };
        for i in 0..n.tcp.len() {
            n.tcp[i].fin_queued = true;
            n.tcp_output(i);
        }
    }
    let until = interrupts::ticks() + HZ / 5;
    while interrupts::ticks() < until {
        task::yield_now();
    }
}

/// Address discovery: an ARP request without waiting for the answer (see `known_hosts`).
pub fn arp_probe(ip: Ip) {
    if let Some(n) = NET.lock().as_mut() {
        let link = n.route(ip);
        n.arp_request(link, ip);
    }
}

/// Addresses in our subnets (on all interfaces) that answered ARP.
pub fn known_hosts() -> Vec<Ip> {
    let net = NET.lock();
    let Some(n) = net.as_ref() else { return Vec::new() };
    let mut out = Vec::new();
    for iface in &n.ifaces {
        for a in iface.arp.keys() {
            if in_subnet(*a, iface.ip, iface.mask) && !out.contains(a) {
                out.push(*a);
            }
        }
    }
    out
}

/// The destination is in the subnet of one of our interfaces (cable or Wi-Fi).
pub fn on_local_net(peer: Ip) -> bool {
    NET.lock().as_ref().is_some_and(|n| n.ifaces.iter().any(|i| i.ip != [0; 4] && in_subnet(peer, i.ip, i.mask)))
}

/// Our address, mask and subnet broadcast address of the preferred interface (None —
/// no address yet).
pub fn subnet() -> Option<(Ip, Ip, Ip)> {
    let net = NET.lock();
    let n = net.as_ref()?;
    let i = n.ifaces.get(n.primary()).filter(|i| i.ip != [0; 4])?;
    Some((i.ip, i.mask, core::array::from_fn(|k| i.ip[k] | !i.mask[k])))
}

/// The MAC of the active card (empty — no card).
pub fn mac() -> Option<[u8; 6]> {
    NET.lock().as_ref().and_then(|n| n.ifaces.get(n.primary()).map(|i| i.mac))
}

/// Tells the router and switches that we listen to a group (IGMPv2).
pub fn join_group(group: Ip) {
    let mut guard = NET.lock();
    let Some(n) = guard.as_mut() else { return };
    let mut m = vec![0x16, 0, 0, 0];
    m.extend_from_slice(&group);
    let sum = checksum(&[&m]);
    m[2..4].copy_from_slice(&sum.to_be_bytes());
    let link = n.route(group);
    n.send_ip(link, group, PROTO_IGMP, &m);
}

/// A UDP socket. The queue is created anew if the card was replaced on the fly.
pub struct UdpSocket(u16);

impl UdpSocket {
    /// `port` 0 — a free ephemeral port. A busy port is an error.
    pub fn bind(port: u16) -> Option<UdpSocket> {
        let mut guard = NET.lock();
        let net = guard.as_mut()?;
        let port = if port == 0 { ephemeral(net) } else { port };
        if net.udp.contains_key(&port) {
            return None;
        }
        net.udp.insert(port, Vec::new());
        Some(UdpSocket(port))
    }

    pub fn send_to(&self, dst: Ip, port: u16, data: &[u8]) {
        if let Some(n) = NET.lock().as_mut() {
            n.udp.entry(self.0).or_default();
            let link = n.route(dst);
            n.send_udp(link, dst, self.0, port, data);
        }
    }

    /// The next received packet: (from where, port, data), immediately, without waiting.
    pub fn try_recv(&self) -> Option<(Ip, u16, Vec<u8>)> {
        let mut net = NET.lock();
        let q = net.as_mut()?.udp.entry(self.0).or_default();
        (!q.is_empty()).then(|| q.remove(0))
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        if let Some(n) = NET.lock().as_mut() {
            n.udp.remove(&self.0);
        }
    }
}

/// A listening TCP port: `accept` takes incoming connections.
pub struct TcpListener(u16);

impl TcpListener {
    pub fn bind(port: u16) -> Result<TcpListener, &'static str> {
        let mut guard = NET.lock();
        let net = guard.as_mut().ok_or("no network")?;
        if net.listeners.iter().any(|(p, _)| *p == port) {
            return Err("port is already in use");
        }
        net.listeners.push((port, Vec::new()));
        Ok(TcpListener(port))
    }

    /// Waits for an incoming connection for up to `timeout` ticks.
    pub fn accept(&self, timeout: u64) -> Option<TcpStream> {
        let port = self.0;
        wait(timeout, |n| {
            let (_, queue) = n.listeners.iter_mut().find(|(p, _)| *p == port)?;
            (!queue.is_empty()).then(|| queue.remove(0))
        })
        .map(TcpStream)
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        if let Some(net) = NET.lock().as_mut() {
            net.listeners.retain(|(p, _)| *p != self.0);
        }
    }
}

/// A TCP connection (closed when dropped).
pub struct TcpStream(u32);

impl TcpStream {
    pub fn connect(dst: Ip, port: u16) -> Result<TcpStream, &'static str> {
        let id = {
            let mut guard = NET.lock();
            let net = guard.as_mut().ok_or("no network")?;
            let link = net.route(dst);
            let lip = net.ifaces.get(link).map_or([0; 4], |i| i.ip);
            if lip == [0; 4] {
                return Err("no network address yet");
            }
            let lport = ephemeral(net);
            let id = net.next_id;
            net.next_id += 1;
            let iss = crate::arch::rdtsc() as u32;
            net.tcp.push(Tcb {
                id,
                state: State::SynSent,
                lip,
                link,
                lport,
                rip: dst,
                rport: port,
                iss,
                snd_una: iss,
                snd_nxt: iss.wrapping_add(1),
                rcv_nxt: 0,
                peer_wnd: MSS,
                unacked: VecDeque::new(),
                outq: VecDeque::new(),
                inq: Vec::new(),
                fin_queued: false,
                fin_seq: None,
                peer_fin: false,
                rto: HZ,
                rto_at: 0,
                retries: 0,
                srtt: 0,
                rttvar: 0,
                rtt_probe: None,
                dup_acks: 0,
                recover: None,
                error: None,
                ack_owed: 0,
                ack_tick: 0,
                rcv_scale: 0,
                snd_scale: 0,
            });
            let i = net.tcp.len() - 1;
            net.tcp_output(i);
            id
        };
        let stream = TcpStream(id);
        let state = wait(10 * HZ, |n| {
            let t = n.tcp.iter().find(|t| t.id == id)?;
            match t.state {
                State::SynSent => None,
                State::Closed => Some(Err(t.error.unwrap_or("connection failed"))),
                _ => Some(Ok(())),
            }
        });
        match state {
            Some(Ok(())) => Ok(stream),
            Some(Err(e)) => Err(e),
            None => Err("connection timed out"),
        }
    }

    /// Queues data for sending. Large data goes in portions: the queue does not grow beyond
    /// `SEND_QUEUE`, and the network is not held busy while everything is copied at once.
    pub fn send(&self, data: &[u8]) -> Result<(), &'static str> {
        let id = self.0;
        let mut rest = data;
        loop {
            let sent = wait(30 * HZ, |net| {
                let Some(i) = net.tcp.iter().position(|t| t.id == id) else { return Some(Err("closed")) };
                let t = &mut net.tcp[i];
                if !matches!(t.state, State::Established | State::CloseWait) {
                    return Some(Err(t.error.unwrap_or("connection closed")));
                }
                let room = SEND_QUEUE.saturating_sub(t.outq.len());
                if room == 0 && !rest.is_empty() {
                    return None;
                }
                let n = room.min(rest.len());
                t.outq.extend(&rest[..n]);
                net.tcp_output(i);
                Some(Ok(n))
            });
            match sent {
                Some(Ok(n)) => rest = &rest[n..],
                Some(Err(e)) => return Err(e),
                None => return Err("timed out"),
            }
            if rest.is_empty() {
                return Ok(());
            }
        }
    }

    /// Waits until the peer acknowledges everything sent (up to `timeout` ticks):
    /// otherwise a large answer would be cut off when the connection closes.
    pub fn flush(&self, timeout: u64) -> bool {
        let id = self.0;
        wait(timeout, |n| {
            let t = n.tcp.iter().find(|t| t.id == id)?;
            if t.outq.is_empty() && t.unacked.is_empty() {
                Some(true)
            } else if t.state == State::Closed {
                Some(false)
            } else {
                None
            }
        })
        .unwrap_or(false)
    }

    /// Waits for data for up to `timeout` ticks. Ok(empty) — the connection was closed by the peer.
    /// The peer's address.
    pub fn peer(&self) -> Option<Ip> {
        NET.lock().as_ref()?.tcp.iter().find(|t| t.id == self.0).map(|t| t.rip)
    }

    pub fn recv(&self, timeout: u64) -> Result<Vec<u8>, &'static str> {
        let id = self.0;
        wait(timeout, |n| {
            let i = n.tcp.iter().position(|t| t.id == id)?;
            let t = &mut n.tcp[i];
            if !t.inq.is_empty() {
                let data = core::mem::take(&mut t.inq);
                // The window was almost closed — tell the peer that room has freed up,
                // otherwise it will keep waiting.
                if data.len() > 32 * 1024 {
                    let nxt = t.snd_nxt;
                    n.tcp_segment(i, nxt, 0x10, &[]);
                }
                return Some(Ok(data));
            }
            if t.peer_fin || t.state == State::Closed {
                return Some(match t.error {
                    Some(e) => Err(e),
                    None => Ok(Vec::new()),
                });
            }
            None
        })
        .unwrap_or(Err("timed out"))
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        if let Some(net) = NET.lock().as_mut()
            && let Some(i) = net.tcp.iter().position(|t| t.id == self.0)
        {
            // Close politely (FIN); the entry is removed once the connection closes.
            let t = &mut net.tcp[i];
            t.fin_queued = true;
            t.id = 0;
            t.inq.clear();
            net.tcp_output(i);
        }
    }
}
