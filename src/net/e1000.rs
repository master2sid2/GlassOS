//! The Intel 8254x network card (e1000) — QEMU emulates it, and it is also fitted in
//! many real machines. It works by polling: the card itself puts received
//! frames into memory (DMA) and sets a flag in the descriptor, and we take them.

use crate::pci;
use alloc::boxed::Box;
use alloc::vec::Vec;

const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EERD: usize = 0x0014;
const IMC: usize = 0x00D8;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const TIPG: usize = 0x0410;
const RDBAL: usize = 0x2800;
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const MTA: usize = 0x5200;
const RAL: usize = 0x5400;
const RAH: usize = 0x5404;

const RX_COUNT: usize = 256;
const TX_COUNT: usize = 128;
const BUF: usize = 2048;

/// The devices the driver understands (82540EM, 82545EM, 82574L…).
const IDS: [u16; 6] = [0x100E, 0x100F, 0x1004, 0x100C, 0x10D3, 0x153A];

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct RxDesc {
    addr: u64,
    len: u16,
    csum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct TxDesc {
    addr: u64,
    len: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}

#[repr(C, align(128))]
struct RxRing([RxDesc; RX_COUNT]);
#[repr(C, align(128))]
struct TxRing([TxDesc; TX_COUNT]);

pub struct E1000 {
    mmio: usize,
    pub mac: [u8; 6],
    rx: Box<RxRing>,
    tx: Box<TxRing>,
    rx_bufs: Vec<Box<[u8; BUF]>>,
    tx_bufs: Vec<Box<[u8; BUF]>>,
    rx_next: usize,
    tx_next: usize,
}

// The rings and buffers are kernel memory; the network stack owns them under its lock.
unsafe impl Send for E1000 {}

impl E1000 {
    fn reg(&self, r: usize) -> u32 {
        unsafe { ((self.mmio + r) as *const u32).read_volatile() }
    }

    fn set(&self, r: usize, v: u32) {
        unsafe { ((self.mmio + r) as *mut u32).write_volatile(v) }
    }

    /// Whether there is a link (the cable is plugged in, the link is up).
    pub fn link_up(&self) -> bool {
        self.reg(STATUS) & 2 != 0
    }

    /// The link speed (Mbit/s) and whether it is full duplex: STATUS bit 0 — duplex, bits 6–7 —
    /// speed (0 — 10, 1 — 100, 2 and 3 — 1000).
    pub fn link_speed(&self) -> (u32, bool) {
        let s = self.reg(STATUS);
        let mbit = match s >> 6 & 3 {
            0 => 10,
            1 => 100,
            _ => 1000,
        };
        (mbit, s & 1 != 0)
    }

    /// Finds the card on the PCI bus and starts it.
    pub fn probe(devices: &[pci::Device]) -> Option<E1000> {
        let dev = devices.iter().find(|d| d.vendor == 0x8086 && IDS.contains(&d.device))?;
        dev.enable_bus_master();
        let mmio = dev.mmio(0, 128 * 1024);
        let mut nic = E1000 {
            mmio,
            mac: [0; 6],
            rx: Box::new(RxRing([RxDesc::default(); RX_COUNT])),
            tx: Box::new(TxRing([TxDesc::default(); TX_COUNT])),
            rx_bufs: (0..RX_COUNT).map(|_| Box::new([0u8; BUF])).collect(),
            tx_bufs: (0..TX_COUNT).map(|_| Box::new([0u8; BUF])).collect(),
            rx_next: 0,
            tx_next: 0,
        };
        nic.init();
        Some(nic)
    }

    fn init(&mut self) {
        // Reset, then "link up" and speed auto-detection.
        self.set(CTRL, self.reg(CTRL) | 1 << 26);
        for _ in 0..100_000 {
            if self.reg(CTRL) & 1 << 26 == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.set(CTRL, (self.reg(CTRL) | 1 << 6 | 1 << 5) & !(1 << 3 | 1 << 31));
        self.set(IMC, 0xFFFF_FFFF); // interrupts are not needed — we poll
        for i in 0..128 {
            self.set(MTA + i * 4, 0);
        }

        // The MAC: from the receive address registers, and if they are empty — from the EEPROM.
        let (lo, hi) = (self.reg(RAL), self.reg(RAH));
        if lo != 0 {
            self.mac = [lo as u8, (lo >> 8) as u8, (lo >> 16) as u8, (lo >> 24) as u8, hi as u8, (hi >> 8) as u8];
        } else {
            for w in 0..3 {
                self.set(EERD, (w << 8) | 1);
                let mut v = 0;
                for _ in 0..100_000 {
                    v = self.reg(EERD);
                    if v & 1 << 4 != 0 {
                        break;
                    }
                }
                self.mac[w as usize * 2] = (v >> 16) as u8;
                self.mac[w as usize * 2 + 1] = (v >> 24) as u8;
            }
            let m = self.mac;
            self.set(RAL, u32::from_le_bytes([m[0], m[1], m[2], m[3]]));
            self.set(RAH, u16::from_le_bytes([m[4], m[5]]) as u32 | 1 << 31);
        }

        for (d, b) in self.rx.0.iter_mut().zip(&self.rx_bufs) {
            *d = RxDesc { addr: b.as_ptr() as u64, ..Default::default() };
        }
        let rx = self.rx.0.as_ptr() as u64;
        self.set(RDBAL, rx as u32);
        self.set(RDBAH, (rx >> 32) as u32);
        self.set(RDLEN, (RX_COUNT * 16) as u32);
        self.set(RDH, 0);
        self.set(RDT, (RX_COUNT - 1) as u32);
        // Receive: enabled, broadcast frames, strip the CRC, 2 KiB buffers.
        self.set(RCTL, 1 << 1 | 1 << 4 | 1 << 15 | 1 << 26); // + all multicast: mDNS, WS-Discovery

        let tx = self.tx.0.as_ptr() as u64;
        self.set(TDBAL, tx as u32);
        self.set(TDBAH, (tx >> 32) as u32);
        self.set(TDLEN, (TX_COUNT * 16) as u32);
        self.set(TDH, 0);
        self.set(TDT, 0);
        // Transmit: enabled, pad short frames, standard gaps.
        self.set(TCTL, 1 << 1 | 1 << 3 | 0x10 << 4 | 0x40 << 12);
        self.set(TIPG, 0x0060_200A);
    }

    /// Sends an Ethernet frame (without the CRC — the card adds it).
    pub fn send(&mut self, frame: &[u8]) -> bool {
        let i = self.tx_next;
        let d = unsafe { core::ptr::read_volatile(&self.tx.0[i]) };
        // The descriptor has not been sent yet (the ring is full).
        if d.cmd != 0 && d.status & 1 == 0 {
            return false;
        }
        let len = frame.len().min(BUF);
        self.tx_bufs[i][..len].copy_from_slice(&frame[..len]);
        let desc = TxDesc { addr: self.tx_bufs[i].as_ptr() as u64, len: len as u16, cmd: 1 | 2 | 8, ..Default::default() };
        unsafe { core::ptr::write_volatile(&mut self.tx.0[i], desc) };
        self.tx_next = (i + 1) % TX_COUNT;
        self.set(TDT, self.tx_next as u32);
        true
    }

    /// Takes a received frame, if there is one.
    pub fn recv(&mut self) -> Option<Vec<u8>> {
        let i = self.rx_next;
        let d = unsafe { core::ptr::read_volatile(&self.rx.0[i]) };
        if d.status & 1 == 0 {
            return None;
        }
        let frame = self.rx_bufs[i][..d.len as usize].to_vec();
        let fresh = RxDesc { addr: self.rx_bufs[i].as_ptr() as u64, ..Default::default() };
        unsafe { core::ptr::write_volatile(&mut self.rx.0[i], fresh) };
        self.set(RDT, i as u32);
        self.rx_next = (i + 1) % RX_COUNT;
        // Faulty frames are simply skipped.
        Some(frame).filter(|_| d.errors == 0)
    }
}
