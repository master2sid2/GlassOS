//! Realtek RTL8168/8111 (and RTL8169) gigabit cards — they are fitted to most
//! inexpensive motherboards and mini PCs. Rings of 16-byte receive and transmit descriptors,
//! the OWN bit says whose descriptor it is; MSI interrupts when available, polling otherwise.
//!
//! The PHY is mostly left alone: the firmware (and the card itself) brings the link up. One exception —
//! the Brix firmware leaves the PHY without advertising 1000BASE-T, and the link comes up at 100 Mbit/s
//! on any cable; we turn the advertisement on ourselves (`advertise_gigabit`).

use super::Mac;
use crate::hwprobe::{deadline, expired};
use crate::pci;
use alloc::boxed::Box;
use alloc::vec::Vec;

const IDR: usize = 0x00;
const MAR: usize = 0x08;
const TNPDS: usize = 0x20;
const CR: usize = 0x37;
const TPPOLL: usize = 0x38;
const IMR: usize = 0x3C;
const ISR: usize = 0x3E;
const TCR: usize = 0x40;
const RCR: usize = 0x44;
const CR9346: usize = 0x50;
const PHYSTATUS: usize = 0x6C;
/// PHY register access: bit 31 is a flag (write: 1 — write; read: becomes 1 when
/// the data is ready), bits 16–20 are the register number, 0–15 the data.
const PHYAR: usize = 0x60;
const PHYAR_FLAG: u32 = 1 << 31;
/// PHY (MII) registers: control, 1000BASE-T advertisement, extended status.
const MII_BMCR: u32 = 0;
const MII_GBCR: u32 = 9;
const MII_ESTATUS: u32 = 15;
const BMCR_ANENABLE: u16 = 1 << 12;
const BMCR_ANRESTART: u16 = 1 << 9;
const GBCR_1000FULL: u16 = 1 << 9;
const ESTATUS_1000T_FULL: u16 = 1 << 13;
/// How long to wait for the PHY, in CPU cycles (~50 ms even at 3 GHz).
const PHY_WAIT: u64 = 150_000_000;
const RMS: usize = 0xDA;
const MTPS: usize = 0xEC;
const RDSAR: usize = 0xE4;

const CR_RST: u8 = 1 << 4;
const CR_RE: u8 = 1 << 3;
const CR_TE: u8 = 1 << 2;

const OWN: u32 = 1 << 31;
const EOR: u32 = 1 << 30;
const FS: u32 = 1 << 29;
const LS: u32 = 1 << 28;
/// Received with an error (CRC, length…).
const RES: u32 = 1 << 21;

const RX_COUNT: usize = 256;
/// The transmit ring: the TCP window over the cable depends on it (`Nic::send_window`).
const TX_COUNT: usize = 256;
/// The TCP window over the cable: the ring minus a reserve for acknowledgements, ARP and other connections.
pub const SEND_WINDOW: usize = 200 * 1460;
const BUF: usize = 2048;

/// Devices: RTL8168/8111 (all revisions), RTL8161, RTL8169, RTL8167, D-Link DGE-528T.
const IDS: [(u16, u16); 6] = [(0x10EC, 0x8168), (0x10EC, 0x8161), (0x10EC, 0x8169), (0x10EC, 0x8167), (0x1186, 0x4300), (0x1186, 0x4302)];

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Desc {
    opts1: u32,
    opts2: u32,
    addr: u64,
}

#[repr(C, align(256))]
struct RxRing([Desc; RX_COUNT]);
#[repr(C, align(256))]
struct TxRing([Desc; TX_COUNT]);
/// Buffers are 8-byte aligned: newer revisions require this.
#[repr(C, align(8))]
struct Buf([u8; BUF]);

pub struct Rtl8168 {
    mmio: usize,
    pub mac: Mac,
    rx: Box<RxRing>,
    tx: Box<TxRing>,
    rx_bufs: Vec<Box<Buf>>,
    tx_bufs: Vec<Box<Buf>>,
    rx_next: usize,
    tx_next: usize,
    /// The card calls with an interrupt (MSI) when frames arrive.
    pub irq: bool,
}

// The rings and buffers are kernel memory; the network stack owns them under its lock.
unsafe impl Send for Rtl8168 {}

impl Rtl8168 {
    fn r8(&self, r: usize) -> u8 {
        unsafe { ((self.mmio + r) as *const u8).read_volatile() }
    }
    fn r32(&self, r: usize) -> u32 {
        unsafe { ((self.mmio + r) as *const u32).read_volatile() }
    }
    fn w8(&self, r: usize, v: u8) {
        unsafe { ((self.mmio + r) as *mut u8).write_volatile(v) }
    }
    fn w16(&self, r: usize, v: u16) {
        unsafe { ((self.mmio + r) as *mut u16).write_volatile(v) }
    }
    /// A PHY register; None — the PHY did not answer.
    fn mdio_read(&self, reg: u32) -> Option<u16> {
        self.w32(PHYAR, reg << 16);
        let end = deadline(PHY_WAIT);
        while !expired(end) {
            let v = self.r32(PHYAR);
            if v & PHYAR_FLAG != 0 {
                return Some(v as u16);
            }
            core::hint::spin_loop();
        }
        None
    }

    fn mdio_write(&self, reg: u32, value: u16) {
        self.w32(PHYAR, PHYAR_FLAG | reg << 16 | value as u32);
        let end = deadline(PHY_WAIT);
        while !expired(end) && self.r32(PHYAR) & PHYAR_FLAG != 0 {
            core::hint::spin_loop();
        }
    }

    /// The PHY can do 1000BASE-T but does not advertise it (the Brix firmware leaves it like that):
    /// advertise it and restart negotiation. true — negotiation was restarted.
    fn advertise_gigabit(&self) -> bool {
        let (Some(estatus), Some(gbcr), Some(bmcr)) = (self.mdio_read(MII_ESTATUS), self.mdio_read(MII_GBCR), self.mdio_read(MII_BMCR)) else {
            return false;
        };
        if estatus & ESTATUS_1000T_FULL == 0 || gbcr & GBCR_1000FULL != 0 {
            return false;
        }
        self.mdio_write(MII_GBCR, gbcr | GBCR_1000FULL);
        self.mdio_write(MII_BMCR, bmcr | BMCR_ANENABLE | BMCR_ANRESTART);
        true
    }

    /// The cable was unplugged: if after a failed gigabit attempt the PHY removed it from the advertisement
    /// (this is how it downshifts on a bad cable), advertise it again — the next cable
    /// starts negotiation with all speeds.
    pub fn link_changed(&self, up: bool) {
        if !up && self.advertise_gigabit() {
            crate::kprintln!("rtl8168: 1000BASE-T was dropped after a failed attempt; advertising it again");
        }
    }

    /// PHY registers 0–15 for diagnostics (`net phy`); `renegotiate` — first
    /// restart negotiation and give it up to 5 s.
    pub fn phy_regs(&self, renegotiate: bool) -> Vec<(u32, u16)> {
        if renegotiate {
            if let Some(bmcr) = self.mdio_read(MII_BMCR) {
                self.mdio_write(MII_BMCR, bmcr | BMCR_ANENABLE | BMCR_ANRESTART);
            }
            let end = deadline(PHY_WAIT * 60);
            // First the link drops, then it comes up again.
            while !expired(end) && self.link_up() {
                core::hint::spin_loop();
            }
            while !expired(end) && !self.link_up() {
                core::hint::spin_loop();
            }
        }
        (0..16).filter_map(|r| self.mdio_read(r).map(|v| (r, v))).collect()
    }

    fn w32(&self, r: usize, v: u32) {
        unsafe { ((self.mmio + r) as *mut u32).write_volatile(v) }
    }

    pub fn link_up(&self) -> bool {
        self.r8(PHYSTATUS) & 2 != 0
    }

    /// The link speed (Mbit/s) and whether it is full duplex. PHYSTATUS: bit 0 — full duplex,
    /// 2 — 10, 3 — 100, 4 — 1000 Mbit/s.
    pub fn link_speed(&self) -> Option<(u32, bool)> {
        let s = self.r8(PHYSTATUS);
        let mbit = if s & 0x10 != 0 { 1000 } else if s & 0x08 != 0 { 100 } else if s & 0x04 != 0 { 10 } else { return None };
        Some((mbit, s & 0x01 != 0))
    }

    pub fn probe(devices: &[pci::Device]) -> Option<Rtl8168> {
        let dev = devices.iter().find(|d| IDS.contains(&(d.vendor, d.device)))?;
        // The registers in memory: the first memory BAR (BAR2 on the 8168, BAR1 on the 8169).
        let bar = (0..6).find(|&n| {
            let lo = dev.read(0x10 + n * 4);
            lo & 1 == 0 && lo & !0xF != 0
        })?;
        dev.enable_bus_master();
        let mmio = dev.mmio(bar, 256);
        let mut nic = Rtl8168 {
            mmio,
            mac: [0; 6],
            rx: Box::new(RxRing([Desc::default(); RX_COUNT])),
            tx: Box::new(TxRing([Desc::default(); TX_COUNT])),
            rx_bufs: (0..RX_COUNT).map(|_| Box::new(Buf([0; BUF]))).collect(),
            tx_bufs: (0..TX_COUNT).map(|_| Box::new(Buf([0; BUF]))).collect(),
            rx_next: 0,
            tx_next: 0,
            irq: false,
        };
        nic.init(dev);
        Some(nic)
    }

    fn init(&mut self, dev: &pci::Device) {
        self.w8(CR, CR_RST);
        for _ in 0..1_000_000 {
            if self.r8(CR) & CR_RST == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        for i in 0..6 {
            self.mac[i] = self.r8(IDR + i);
        }
        // The chip revision is in the upper bits of TCR. From the 8168G (0x4c0) receive is set up differently.
        let xid = self.r32(TCR) >> 20 & 0x7CF;
        let new = xid >= 0x4C0;

        self.w8(CR9346, 0xC0); // allow writing the configuration
        self.w16(RMS, BUF as u16);
        self.w8(MTPS, 0x3F);
        for (d, b) in self.rx.0.iter_mut().zip(&self.rx_bufs) {
            *d = Desc { opts1: OWN | BUF as u32, opts2: 0, addr: b.0.as_ptr() as u64 };
        }
        self.rx.0[RX_COUNT - 1].opts1 |= EOR;
        self.tx.0[TX_COUNT - 1].opts1 = EOR;
        let (rx, tx) = (self.rx.0.as_ptr() as u64, self.tx.0.as_ptr() as u64);
        self.w32(TNPDS, tx as u32);
        self.w32(TNPDS + 4, (tx >> 32) as u32);
        self.w32(RDSAR, rx as u32);
        self.w32(RDSAR + 4, (rx >> 32) as u32);
        // Interrupts only if MSI could be enabled (otherwise we poll): receive,
        // receive error, no free descriptors, link change, FIFO overflow.
        let irq = crate::irq::msi_hook(dev, "ethernet", crate::net::wake_stack).is_some();
        self.w16(ISR, 0xFFFF);
        self.w16(IMR, if irq { 1 << 0 | 1 << 1 | 1 << 4 | 1 << 5 | 1 << 6 } else { 0 });
        self.irq = irq;
        self.w8(CR, CR_RE | CR_TE);
        // Receive: our own, broadcast and all multicast frames (mDNS, WS-Discovery);
        // DMA without a burst length limit.
        let accept = 1 << 3 | 1 << 2 | 1 << 1;
        let rcr = if new { 1 << 15 | 1 << 14 | 1 << 11 | 7 << 8 } else { 7 << 13 | 7 << 8 };
        self.w32(RCR, rcr | accept);
        // Transmit: the standard inter-frame gap, DMA without a limit; newer ones have their own FIFO.
        self.w32(TCR, 3 << 24 | 7 << 8 | if xid >= 0x2C8 { 1 << 7 } else { 0 });
        for i in 0..8 {
            self.w8(MAR + i, 0xFF);
        }
        self.w8(CR9346, 0x00);
        if self.advertise_gigabit() {
            crate::kprintln!("rtl8168: the PHY did not advertise 1000BASE-T; turned it on, negotiating again");
        }
        let m = self.mac;
        crate::kprintln!(
            "rtl8168: {:02x}:{:02x}.{} {:04x}, chip {xid:#05x}, mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}",
            dev.bus,
            dev.slot,
            dev.func,
            dev.device,
            m[0],
            m[1],
            m[2],
            m[3],
            m[4],
            m[5],
            if self.link_up() { "up" } else { "down" }
        );
    }

    /// Sends an Ethernet frame (without the CRC — the card adds it).
    pub fn send(&mut self, frame: &[u8]) -> bool {
        let i = self.tx_next;
        let d = unsafe { core::ptr::read_volatile(&self.tx.0[i]) };
        if d.opts1 & OWN != 0 {
            return false; // the ring is full
        }
        let len = frame.len().min(BUF);
        let buf = &mut self.tx_bufs[i].0;
        buf[..len].copy_from_slice(&frame[..len]);
        // Short frames are padded to 60 bytes by us: not all revisions do it.
        let len = if len < 60 {
            buf[len..60].fill(0);
            60
        } else {
            len
        };
        let end = if i == TX_COUNT - 1 { EOR } else { 0 };
        let desc = Desc { opts1: OWN | FS | LS | end | len as u32, opts2: 0, addr: buf.as_ptr() as u64 };
        unsafe { core::ptr::write_volatile(&mut self.tx.0[i], desc) };
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.w8(TPPOLL, 1 << 6);
        self.tx_next = (i + 1) % TX_COUNT;
        true
    }

    /// Clear the interrupt status before walking the ring: a frame arriving later will call again.
    pub fn ack_irq(&mut self) {
        let status = unsafe { ((self.mmio + ISR) as *const u16).read_volatile() };
        if status != 0 {
            unsafe { ((self.mmio + ISR) as *mut u16).write_volatile(status) };
        }
    }

    /// Takes a received frame, if there is one.
    pub fn recv(&mut self) -> Option<Vec<u8>> {
        let i = self.rx_next;
        let d = unsafe { core::ptr::read_volatile(&self.rx.0[i]) };
        if d.opts1 & OWN != 0 {
            return None;
        }
        // The length includes the CRC (4 bytes).
        let len = (d.opts1 & 0x3FFF) as usize;
        let whole = d.opts1 & (FS | LS) == FS | LS && d.opts1 & RES == 0 && (14..=BUF).contains(&len);
        let frame = whole.then(|| self.rx_bufs[i].0[..len.saturating_sub(4)].to_vec());
        let end = if i == RX_COUNT - 1 { EOR } else { 0 };
        let fresh = Desc { opts1: OWN | end | BUF as u32, opts2: 0, addr: self.rx_bufs[i].0.as_ptr() as u64 };
        unsafe { core::ptr::write_volatile(&mut self.rx.0[i], fresh) };
        self.rx_next = (i + 1) % RX_COUNT;
        // Faulty frames are simply skipped.
        frame
    }
}
