//! USB Ethernet: the CDC-ECM class (adapters — Realtek RTL8153 in its second
//! configuration and many others, docking stations) and RNDIS (Android phones in
//! USB tethering mode).
//!
//! An Ethernet frame is one bulk transfer. The USB thread keeps several pending
//! receives; received frames and frames to send go through queues, so the
//! network stack does not need the USB lock.

use super::xhci::{self, Ring, Xhci, dma};
use super::{Loc, Usb, ctx};
use crate::kprintln;
use crate::sync::IrqLock;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

const BUF: usize = 2048;
const RX_PENDING: usize = 16;
const TX_BUFS: usize = 32;

/// Received frames (the network stack takes them) and frames to send.
static RX: IrqLock<VecDeque<Vec<u8>>> = IrqLock::new(VecDeque::new());
static TX: IrqLock<VecDeque<Vec<u8>>> = IrqLock::new(VecDeque::new());
static LINK: AtomicBool = AtomicBool::new(false);
/// A new card: the MAC — for attaching to the stack (the main loop does it).
static NEW: IrqLock<Option<[u8; 6]>> = IrqLock::new(None);

pub fn take_new() -> Option<[u8; 6]> {
    NEW.lock().take()
}

/// A network card for the stack: only queues.
pub struct UsbNic {
    mac: [u8; 6],
}

impl UsbNic {
    pub fn new(mac: [u8; 6]) -> UsbNic {
        UsbNic { mac }
    }
}

impl crate::net::Nic for UsbNic {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }
    fn send(&mut self, frame: &[u8]) -> bool {
        let mut tx = TX.lock();
        if tx.len() >= 256 || !LINK.load(Ordering::Relaxed) {
            return false;
        }
        tx.push_back(frame.to_vec());
        true
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        RX.lock().pop_front()
    }
    fn link_up(&self) -> bool {
        LINK.load(Ordering::Relaxed)
    }
}

/// What was found in the configuration: interfaces, endpoints and the index of the MAC string.
pub struct Ecm {
    /// RNDIS (frames in headers, control by messages) — otherwise ECM.
    rndis: bool,
    comm: u8,
    data: u8,
    alt: u8,
    bulk_in: (u8, u16),
    bulk_out: (u8, u16),
    mac_string: u8,
}

/// Looks for a CDC Ethernet (ECM) interface in the configuration descriptor.
pub fn find_ecm(conf: &[u8]) -> Option<Ecm> {
    let (mut comm, mut data, mut mac_string) = (None, None, 0u8);
    let (mut cur, mut cur_alt, mut cur_class) = (0u8, 0u8, 0u8);
    let (mut bin, mut bout) = (None, None);
    let mut found_alt = None;
    let mut i = 0;
    while i + 2 <= conf.len() {
        let (len, ty) = (conf[i] as usize, conf[i + 1]);
        if len == 0 {
            break;
        }
        match ty {
            4 if i + 9 <= conf.len() => {
                (cur, cur_alt, cur_class) = (conf[i + 2], conf[i + 3], conf[i + 5]);
                if cur_class == 2 && conf[i + 6] == 6 {
                    comm = Some(cur);
                }
            }
            0x24 if i + 3 <= conf.len() => match conf[i + 2] {
                // Union: the control interface and the data interface.
                0x06 if i + 5 <= conf.len() && comm == Some(conf[i + 3]) => data = Some(conf[i + 4]),
                // Ethernet Networking: the index of the string with the MAC address.
                0x0F if i + 4 <= conf.len() => mac_string = conf[i + 3],
                _ => {}
            },
            5 if i + 7 <= conf.len() && cur_class == 0x0A && Some(cur) == data => {
                let (addr, attr, mps) = (conf[i + 2], conf[i + 3], u16::from_le_bytes([conf[i + 4], conf[i + 5]]));
                if attr & 3 == 2 {
                    if addr & 0x80 != 0 {
                        bin = Some((addr & 0xF, mps));
                    } else {
                        bout = Some((addr & 0xF, mps));
                    }
                    found_alt = Some(cur_alt);
                }
            }
            _ => {}
        }
        i += len;
    }
    Some(Ecm { rndis: false, comm: comm?, data: data?, alt: found_alt?, bulk_in: bin?, bulk_out: bout?, mac_string })
}

/// RNDIS: a control interface 02/02/FF or E0/01/03 (Android) and a data interface
/// 0A with its own bulk endpoints.
pub fn find_rndis(conf: &[u8]) -> Option<Ecm> {
    let (mut comm, mut data) = (None, None);
    let (mut cur, mut cur_class) = (0u8, 0u8);
    let (mut bin, mut bout) = (None, None);
    let mut i = 0;
    while i + 2 <= conf.len() {
        let (len, ty) = (conf[i] as usize, conf[i + 1]);
        if len == 0 {
            break;
        }
        match ty {
            4 if i + 9 <= conf.len() => {
                (cur, cur_class) = (conf[i + 2], conf[i + 5]);
                let (sub, proto) = (conf[i + 6], conf[i + 7]);
                if (cur_class == 2 && sub == 2 && proto == 0xFF) || (cur_class == 0xE0 && sub == 1 && proto == 3) {
                    comm = Some(cur);
                }
                if cur_class == 0x0A && comm.is_some() && data.is_none() {
                    data = Some(cur);
                }
            }
            5 if i + 7 <= conf.len() && cur_class == 0x0A && Some(cur) == data => {
                let (addr, attr, mps) = (conf[i + 2], conf[i + 3], u16::from_le_bytes([conf[i + 4], conf[i + 5]]));
                if attr & 3 == 2 {
                    if addr & 0x80 != 0 {
                        bin = Some((addr & 0xF, mps));
                    } else {
                        bout = Some((addr & 0xF, mps));
                    }
                }
            }
            _ => {}
        }
        i += len;
    }
    Some(Ecm { rndis: true, comm: comm?, data: data?, alt: 0, bulk_in: bin?, bulk_out: bout?, mac_string: 0 })
}

const RNDIS_PACKET: u32 = 1;
const RNDIS_INITIALIZE: u32 = 2;
const RNDIS_QUERY: u32 = 4;
const RNDIS_SET: u32 = 5;
const OID_PERMANENT_ADDRESS: u32 = 0x0101_0101;
const OID_PACKET_FILTER: u32 = 0x0001_010E;

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

/// An RNDIS message over the control channel and the answer to it.
fn rndis_call(hc: &mut Xhci, slot: u8, ep0: &mut Ring, buf: *mut u8, iface: u8, msg: &[u8]) -> Option<Vec<u8>> {
    unsafe { core::ptr::copy_nonoverlapping(msg.as_ptr(), buf, msg.len()) };
    let n = msg.len() as u16;
    hc.control_retry(slot, ep0, [0x21, 0x00, 0, 0, iface, 0, n as u8, (n >> 8) as u8], buf, n)?;
    // The answer is not ready right away — ask several times.
    for _ in 0..20 {
        if hc.control(slot, ep0, [0xA1, 0x01, 0, 0, iface, 0, 0x00, 0x04], buf, 1024).is_some() {
            let r = unsafe { core::slice::from_raw_parts(buf, 1024) };
            let len = (le32(r, 4) as usize).min(1024);
            if len >= 16 && le32(r, 0) & 0x8000_0000 != 0 {
                return Some(r[..len].to_vec());
            }
        }
        super::hub::sleep_ms(10);
    }
    None
}

fn rndis_msg(kind: u32, id: u32, body: &[u32], data: &[u8]) -> Vec<u8> {
    let mut m = Vec::new();
    let len = 12 + body.len() * 4 + data.len();
    for v in [kind, len as u32, id].iter().chain(body) {
        m.extend_from_slice(&v.to_le_bytes());
    }
    m.extend_from_slice(data);
    m
}

pub struct UsbNet {
    pub slot: u8,
    pub loc: Loc,
    in_dci: u32,
    out_dci: u32,
    in_ring: Ring,
    out_ring: Ring,
    out_mps: usize,
    rndis: bool,
    /// Buffers of the pending receives — in the order they were queued.
    rx: VecDeque<*mut u8>,
    tx: Vec<*mut u8>,
    tx_next: usize,
}

unsafe impl Send for UsbNet {}

impl UsbNet {
    fn queue_rx(&mut self, hc: &Xhci, buf: *mut u8) {
        self.in_ring.push(buf as u64, BUF as u32, xhci::TRB_NORMAL << 10 | 1 << 5 | 1 << 2);
        self.rx.push_back(buf);
        hc.ring_doorbell(self.slot, self.in_dci);
    }

    /// Sends the accumulated frames.
    pub fn flush(&mut self, hc: &Xhci) {
        let frames: Vec<Vec<u8>> = TX.lock().drain(..).collect();
        if frames.is_empty() {
            return;
        }
        for f in frames {
            let buf = self.tx[self.tx_next];
            self.tx_next = (self.tx_next + 1) % TX_BUFS;
            // RNDIS: the frame in a PACKET_MSG header (the data starts 44 bytes in).
            let f = if self.rndis {
                let mut m = Vec::with_capacity(44 + f.len());
                for v in [RNDIS_PACKET, (44 + f.len()) as u32, 36, f.len() as u32, 0, 0, 0, 0, 0, 0, 0] {
                    m.extend_from_slice(&v.to_le_bytes());
                }
                m.extend_from_slice(&f);
                m
            } else {
                f
            };
            let n = f.len().min(BUF);
            unsafe { core::ptr::copy_nonoverlapping(f.as_ptr(), buf, n) };
            self.out_ring.push(buf as u64, n as u32, xhci::TRB_NORMAL << 10 | 1 << 5);
            // A frame exactly on a packet boundary — an empty packet is needed, otherwise it does not end.
            if n % self.out_mps == 0 {
                self.out_ring.push(buf as u64, 0, xhci::TRB_NORMAL << 10 | 1 << 5);
            }
        }
        hc.ring_doorbell(self.slot, self.out_dci);
    }

    /// A transfer event: a received frame goes to the stack queue, the receive is queued again.
    pub fn on_transfer(&mut self, hc: &Xhci, e: &xhci::Trb) -> bool {
        let dci = (e.0[3] >> 16) & 0x1F;
        if e.slot() != self.slot {
            return false;
        }
        if dci == self.out_dci {
            return true;
        }
        if dci != self.in_dci {
            return false;
        }
        let Some(buf) = self.rx.pop_front() else { return true };
        if e.code() == xhci::SUCCESS || e.code() == xhci::SHORT_PACKET {
            let len = BUF - (e.0[2] & 0xFF_FFFF) as usize;
            let data = unsafe { core::slice::from_raw_parts(buf, len) };
            let mut rx = RX.lock();
            if self.rndis {
                // One transfer holds one or several PACKET_MSG messages.
                let mut at = 0;
                while at + 44 <= data.len() {
                    let (kind, mlen) = (le32(data, at), le32(data, at + 4) as usize);
                    if kind != RNDIS_PACKET || mlen < 44 || at + mlen > data.len() {
                        break;
                    }
                    let (off, dlen) = (le32(data, at + 8) as usize + 8, le32(data, at + 12) as usize);
                    if let Some(frame) = data.get(at + off..at + off + dlen)
                        && rx.len() < 512
                    {
                        rx.push_back(frame.to_vec());
                    }
                    at += mlen;
                }
            } else if len >= 14 && rx.len() < 512 {
                rx.push_back(data.to_vec());
            }
        }
        self.queue_rx(hc, buf);
        true
    }
}

impl Usb {
    /// Sets up a USB Ethernet adapter in configuration `config`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn attach_net(
        &mut self,
        slot: u8,
        loc: Loc,
        speed: u32,
        ep0: &mut Ring,
        buf: *mut u8,
        input: *mut u8,
        config: u8,
        ecm: Ecm,
        (vendor, product): (u16, u16),
    ) -> Result<(), &'static str> {
        let hc = &mut self.hc;
        hc.control_retry(slot, ep0, [0, 9, config, 0, 0, 0, 0, 0], buf, 0).ok_or("set configuration failed")?;
        // The MAC is a string of 12 hex digits (UTF-16).
        let mut mac = [0x02, 0, 0, 0, 0, 1];
        if ecm.mac_string != 0
            && hc.control_retry(slot, ep0, [0x80, 6, ecm.mac_string, 3, 0x09, 0x04, 64, 0], buf, 64).is_some()
        {
            let s = unsafe { core::slice::from_raw_parts(buf, 64) };
            let digits: Vec<u8> = s[2..(s[0] as usize).min(64)].chunks(2).map(|c| c[0]).collect();
            if digits.len() >= 12 {
                for (k, m) in mac.iter_mut().enumerate() {
                    let hex = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
                    *m = hex(digits[2 * k]) << 4 | hex(digits[2 * k + 1]);
                }
            }
        }
        if ecm.rndis {
            // RNDIS: initialization, the MAC from an OID, the packet filter — with control messages.
            let init = rndis_msg(RNDIS_INITIALIZE, 1, &[1, 0, BUF as u32], &[]);
            let r = rndis_call(hc, slot, ep0, buf, ecm.comm, &init).ok_or("RNDIS device does not answer")?;
            if le32(&r, 12) != 0 {
                return Err("RNDIS initialization failed");
            }
            let q = rndis_msg(RNDIS_QUERY, 2, &[OID_PERMANENT_ADDRESS, 0, 0, 0], &[]);
            if let Some(r) = rndis_call(hc, slot, ep0, buf, ecm.comm, &q)
                && le32(&r, 12) == 0
            {
                let off = le32(&r, 20) as usize + 8;
                if let Some(m) = r.get(off..off + 6) {
                    mac.copy_from_slice(m);
                }
            }
            let set = rndis_msg(RNDIS_SET, 3, &[OID_PACKET_FILTER, 4, 20, 0], &0x0Fu32.to_le_bytes());
            rndis_call(hc, slot, ep0, buf, ecm.comm, &set).ok_or("RNDIS packet filter failed")?;
        } else {
            // The data interface: the alternate setting with bulk endpoints enables the exchange.
            hc.control_retry(slot, ep0, [0x01, 0x0B, ecm.alt, 0, ecm.data, 0, 0, 0], buf, 0);
        }
        let cs = hc.ctx_size;
        let (in_dci, out_dci) = (ecm.bulk_in.0 as u32 * 2 + 1, ecm.bulk_out.0 as u32 * 2);
        let (in_ring, out_ring) = (Ring::new(), Ring::new());
        unsafe {
            for k in 0..cs * 33 / 4 {
                *(input as *mut u32).add(k) = 0;
            }
            *ctx(input, cs, 0).add(1) = 1 | 1 << in_dci | 1 << out_dci;
            super::slot_ctx(ctx(input, cs, 1), &loc, speed, in_dci.max(out_dci));
            for (dci, ty, mps, ring) in [(in_dci, 6u32, ecm.bulk_in.1, &in_ring), (out_dci, 2, ecm.bulk_out.1, &out_ring)] {
                let e = ctx(input, cs, 1 + dci as usize);
                *e.add(1) = 3 << 1 | ty << 3 | (mps as u32) << 16;
                let deq = ring.addr() | 1;
                *e.add(2) = deq as u32;
                *e.add(3) = (deq >> 32) as u32;
                *e.add(4) = 1514;
            }
        }
        if !hc.configure_endpoint(slot, input as u64) {
            return Err("configure bulk endpoints failed");
        }
        // The ECM filter: our own frames, broadcast and all multicast.
        if !ecm.rndis {
            hc.control_retry(slot, ep0, [0x21, 0x43, 0x0E, 0, ecm.comm, 0, 0, 0], buf, 0);
        }
        let mut net = UsbNet {
            slot,
            loc,
            in_dci,
            out_dci,
            in_ring,
            out_ring,
            out_mps: (ecm.bulk_out.1 as usize).max(8),
            rndis: ecm.rndis,
            rx: VecDeque::new(),
            tx: (0..TX_BUFS).map(|_| dma(BUF)).collect(),
            tx_next: 0,
        };
        for _ in 0..RX_PENDING {
            let b = dma(BUF);
            net.queue_rx(&self.hc, b);
        }
        self.nets.push(net);
        RX.lock().clear();
        TX.lock().clear();
        LINK.store(true, Ordering::Relaxed);
        *NEW.lock() = Some(mac);
        let m = mac;
        self.report(
            loc,
            alloc::format!(
                "ethernet ({}) {vendor:04x}:{product:04x}, {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                if ecm.rndis { "RNDIS" } else { "ECM" },
                m[0], m[1], m[2], m[3], m[4], m[5]
            ),
        );
        Ok(())
    }
}

/// The adapter was unplugged — no link.
pub fn unplugged() {
    LINK.store(false, Ordering::Relaxed);
    kprintln!("usb: ethernet adapter unplugged");
}
