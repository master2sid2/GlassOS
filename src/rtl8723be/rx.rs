//! DMA rings: frame receive and the transmit rings.
//!
//! Memory is mapped one-to-one, so the address for the card is the kernel address. The card
//! understands only 32-bit addresses: everything we give it is checked to be "below 4 GiB".

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{Ordering, fence};

/// Descriptors in the receive ring and the buffer size of each.
const RX_COUNT: usize = 64;
const RX_BUFFER: usize = 8192;
/// A PCIe receive descriptor is 8 words: 6 status words, the buffer address (low and high 32 bits).
const RX_WORDS: usize = 8;
const RX_ADDR_WORD: usize = 6;
/// Transmit rings: 16 descriptors of 64 bytes each.
pub(super) const TX_RINGS: usize = 7;
const TX_COUNT: usize = 16;
const TX_DESC: usize = 64;

const OWN: u32 = 1 << 31;
const EOR: u32 = 1 << 30;
const PHYST: u32 = 1 << 26;
const ICVERR: u32 = 1 << 15;
const CRCERR: u32 = 1 << 14;
const PKTLEN: u32 = 0x3FFF;
const INFOSZ_SHIFT: u32 = 16;
const INFOSZ_MASK: u32 = 0xF;
const SHIFT_SHIFT: u32 = 24;
const SHIFT_MASK: u32 = 0x3;
/// Word 2, bit 28: not a frame from the air but a firmware report (C2H).
const DW2_C2H: u32 = 1 << 28;
/// Word 3: the receive rate; below 4 — CCK (1, 2, 5.5 and 11 Mbit/s).
const DW3_RATE: u32 = 0x7F;
const RATE_OFDM_6M: u32 = 4;
/// The frame checksum (RCR_APPFCS) at the end of the data.
const FCS: usize = 4;
/// The shortest meaningful frame is an acknowledgement (10 bytes).
const SHORTEST: usize = 10;

const LOW_END: u64 = 1 << 32;

/// A memory block for the card, zeroed, page-aligned, entirely below 4 GiB.
/// Never freed: the rings live as long as the card.
fn dma(size: usize) -> Result<*mut u8, String> {
    let layout = Layout::from_size_align(size, 4096).map_err(|_| String::from("bad DMA block size"))?;
    let p = unsafe { alloc_zeroed(layout) };
    if p.is_null() {
        return Err(format!("no memory for a {size}-byte DMA block"));
    }
    if p as u64 + size as u64 > LOW_END {
        unsafe { dealloc(p, layout) };
        return Err(format!("DMA block landed at {p:p}, above 4 GiB: the card cannot reach it"));
    }
    Ok(p)
}

/// A received frame: descriptor words, PHY status and the 802.11 frame itself (without FCS).
pub(super) struct Frame<'a> {
    pub dw: [u32; 6],
    pub phy: &'a [u8],
    pub data: &'a [u8],
}

impl Frame<'_> {
    /// The receive rate index (0–3 CCK, 4–11 OFDM, 12 and above — HT MCS).
    pub fn rate(&self) -> u32 {
        self.dw[3] & DW3_RATE
    }

    pub fn cck(&self) -> bool {
        self.dw[3] & DW3_RATE < RATE_OFDM_6M
    }

    /// The signal level in dBm from the PHY status (r23b_get_rssi_cck, r88e_get_rssi_ofdm).
    pub fn signal(&self) -> Option<i32> {
        if self.phy.len() < 6 {
            return None;
        }
        if self.cck() {
            let agc = self.phy[5];
            let (lna, vga) = (agc >> 5, (agc & 0x1F) as i32);
            let base = match lna {
                6 => -34,
                4 => -14,
                1 => 6,
                0 => 16,
                _ => return None,
            };
            Some(base - 2 * vga)
        } else {
            Some(((self.phy[4] >> 1) & 0x7F) as i32 - 110)
        }
    }

    /// The receive time by the card's clock (the low 32 bits of the TSF).
    pub fn time(&self) -> u32 {
        self.dw[5]
    }
}

/// Receive ring counters.
#[derive(Default, Clone, Copy)]
pub(super) struct Counts {
    pub frames: u64,
    pub reports: u64,
    pub broken: u64,
}

pub(super) struct Rx {
    ring: *mut u32,
    buffers: *mut u8,
    cur: usize,
    pub counts: Counts,
}

unsafe impl Send for Rx {}

impl Rx {
    pub fn new() -> Result<Self, String> {
        let ring = dma(RX_COUNT * RX_WORDS * 4)? as *mut u32;
        let buffers = dma(RX_COUNT * RX_BUFFER)?;
        let rx = Rx { ring, buffers, cur: 0, counts: Counts::default() };
        for i in 0..RX_COUNT {
            rx.arm(i);
        }
        Ok(rx)
    }

    /// The ring address for RX_DESA.
    pub fn addr(&self) -> u32 {
        self.ring as u32
    }

    /// Gives a descriptor to the card (like rtwn_pci_setup_rx_desc).
    fn arm(&self, i: usize) {
        let d = unsafe { self.ring.add(i * RX_WORDS) };
        let buffer = unsafe { self.buffers.add(i * RX_BUFFER) } as u32;
        let last = if i == RX_COUNT - 1 { EOR } else { 0 };
        unsafe {
            for w in 1..RX_WORDS {
                d.add(w).write_volatile(0);
            }
            d.add(RX_ADDR_WORD).write_volatile(buffer);
            d.write_volatile(RX_BUFFER as u32 & PKTLEN | last);
        }
        fence(Ordering::SeqCst);
        unsafe { d.write_volatile(RX_BUFFER as u32 & PKTLEN | last | OWN) };
    }

    /// How many descriptors the card holds now (for the log: all 64 — the ring is empty).
    pub fn armed(&self) -> usize {
        (0..RX_COUNT).filter(|&i| unsafe { self.ring.add(i * RX_WORDS).read_volatile() } & OWN != 0).count()
    }

    /// Handles everything the card put into the ring and returns the descriptors to it.
    pub fn poll(&mut self, mut f: impl FnMut(&Frame)) -> usize {
        let mut n = 0;
        while n < RX_COUNT {
            let d = unsafe { self.ring.add(self.cur * RX_WORDS) };
            let dw0 = unsafe { d.read_volatile() };
            if dw0 & OWN != 0 {
                break;
            }
            fence(Ordering::SeqCst);
            let mut dw = [0u32; 6];
            for (w, v) in dw.iter_mut().enumerate() {
                *v = unsafe { d.add(w).read_volatile() };
            }
            let len = (dw0 & PKTLEN) as usize;
            let info = ((dw0 >> INFOSZ_SHIFT & INFOSZ_MASK) * 8) as usize;
            let shift = (dw0 >> SHIFT_SHIFT & SHIFT_MASK) as usize;
            let buf = unsafe { core::slice::from_raw_parts(self.buffers.add(self.cur * RX_BUFFER), RX_BUFFER) };
            if dw[2] & DW2_C2H != 0 {
                self.counts.reports += 1;
                if self.counts.reports <= 12 {
                    let n = len.min(16);
                    let body: String = buf[shift + info..shift + info + n].iter().map(|b| format!("{b:02x}")).collect();
                    crate::kprintln!("rtl8723be: c2h len {len} dw {:08x} {:08x} {:08x}: {body}", dw[0], dw[1], dw[2]);
                }
            } else if dw0 & (CRCERR | ICVERR) != 0 || len < SHORTEST + FCS || shift + info + len > RX_BUFFER {
                self.counts.broken += 1;
            } else {
                self.counts.frames += 1;
                let phy = if dw0 & PHYST != 0 { &buf[shift..shift + info] } else { &buf[..0] };
                let data = &buf[shift + info..shift + info + len - FCS];
                f(&Frame { dw, phy, data });
            }
            self.arm(self.cur);
            self.cur = (self.cur + 1) % RX_COUNT;
            n += 1;
        }
        n
    }
}

/// Transmit rings: each descriptor holds the address of the next one (the ring is closed).
/// Buffers and sending exist only for the management queue: scan and connection frames, and data.
pub(super) fn tx_rings() -> Result<([u32; TX_RINGS], Tx), String> {
    let block = dma(TX_RINGS * TX_COUNT * TX_DESC)?;
    let mut rings = [0u32; TX_RINGS];
    for (r, ring) in rings.iter_mut().enumerate() {
        let base = unsafe { block.add(r * TX_COUNT * TX_DESC) };
        *ring = base as u32;
        for i in 0..TX_COUNT {
            let next = base as u32 + (((i + 1) % TX_COUNT) * TX_DESC) as u32;
            unsafe { (base.add(i * TX_DESC) as *mut u32).add(TXD_NEXT_WORD).write_volatile(next) };
        }
    }
    let mgq = Tx { ring: rings[MGQ_RING] as usize as *mut u32, bufs: dma(TX_COUNT * TX_BUF)?, cur: 0, poll: MGQ_POLL };
    Ok((rings, mgq))
}

/// The order of rings in `tx_rings`: beacon, high, management, voice, video, normal, background.
const MGQ_RING: usize = 2;
/// The management queue bit in PCIE_CTRL_REG: "there is something to send".
const MGQ_POLL: u16 = 1 << 6;
const TX_BUF: usize = 2048;
const TX_WORDS: usize = TX_DESC / 4;
const TXD_OWN: u32 = 1 << 31;
const TXD_FIRST: u32 = 1 << 27;
const TXD_LAST: u32 = 1 << 26;
const TXD_BROADCAST: u32 = 1 << 24;
/// The data offset from the start of the descriptor (40 bytes) — the card requires this for PCIe too.
const TXD_OFFSET: u32 = 40 << 16;
const TXD1_QSEL_MGNT: u32 = 0x12 << 8;
const TXD1_RAID_B: u32 = 8 << 16;
const TXD3_USE_RATE: u32 = 1 << 8;
const TXD3_NO_FALLBACK: u32 = 1 << 10;
/// The fallback rate set for the firmware: 802.11b/g (RAID_B — b only).
const TXD1_RAID_BG: u32 = 6 << 16;
/// Word 4: the frame rate (bits 0–6: 0–3 — CCK 1/2/5.5/11, 4–11 — OFDM 6…54 Mbit/s)
/// and how far the card may fall back on retries (bits 17–21). The field positions
/// follow FreeBSD rtwn for cards of this generation (R12A_TXDW4_*).
const TXD4_FALLBACK_LIMIT_SHIFT: u32 = 17;

/// The data frame rate (the index above); changed with the `wifi rate` command. Measured on the Brix
/// (signal −62 dBm): 1 Mbit/s — 21 KB/s upload, 36 and 48 — about 460 KB/s stable,
/// 54 — sometimes 565 KB/s, sometimes a drop. Until the firmware picks the rate, we use 36.
pub(super) static DATA_RATE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(9);
/// Whether falling back to a lower rate on retries is allowed. With our fallback limit (bits
/// 17–21 of word 4) transmit breaks — the field means something else on this card; off for now.
pub(super) static DATA_FALLBACK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
const TXD8_HWSEQ: u32 = 1 << 15;
/// The encryption field in descriptor word 1 (bits 22–23): AES = 3. The card encrypts the frame with CCMP,
/// taking the key from the CAM. The value and position come from FreeBSD rtwn (RTWN_TXDW1_CIPHER_AES).
const TXD1_CIPHER_AES: u32 = 3 << 22;
const TXD_SIZE_WORD: usize = 7;
const TXD_BUF_WORD: usize = 10;
const TXD_NEXT_WORD: usize = 12;

pub(super) struct Tx {
    ring: *mut u32,
    bufs: *mut u8,
    cur: usize,
    poll: u16,
}

unsafe impl Send for Tx {}

impl Tx {
    /// Queues an 802.11 frame on the management queue (1 Mbit/s, the sequence number from the card).
    /// false — the frame is too large or the previous one in this slot has not gone out yet.
    pub fn send(&mut self, chip: &super::Chip, frame: &[u8]) -> bool {
        self.send_raw(chip, frame, 0)
    }

    /// Like `send`, but the card encrypts the frame with CCMP: the key is taken from the CAM by the receiver address
    /// (addr1 = the access point). The frame must already contain the 8-byte CCMP header and the
    /// Protected bit in FC; the card appends the MIC itself (the length in the descriptor is without the MIC).
    pub fn send_encrypted(&mut self, chip: &super::Chip, frame: &[u8]) -> bool {
        self.send_raw(chip, frame, TXD1_CIPHER_AES)
    }

    /// Whether there is a free slot: a frame that does not fit had better wait in the queue
    /// than be lost (TCP would notice the loss only after seconds).
    pub fn has_room(&self) -> bool {
        let word = unsafe { self.ring.add(self.cur * TX_WORDS).read_volatile() };
        word & TXD_OWN == 0
    }

    fn send_raw(&mut self, chip: &super::Chip, frame: &[u8], dw1_extra: u32) -> bool {
        if frame.len() > TX_BUF || frame.len() < 10 {
            return false;
        }
        let d = unsafe { self.ring.add(self.cur * TX_WORDS) };
        if unsafe { d.read_volatile() } & TXD_OWN != 0 {
            return false;
        }
        let buf = unsafe { self.bufs.add(self.cur * TX_BUF) };
        unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), buf, frame.len()) };
        let len = frame.len() as u32;
        let broadcast = if frame[4] & 1 != 0 { TXD_BROADCAST } else { 0 };
        unsafe {
            for w in 1..TXD_NEXT_WORD {
                d.add(w).write_volatile(0);
            }
            if dw1_extra & TXD1_CIPHER_AES != 0 {
                // Data: at the chosen rate, on retries the card may fall back lower.
                use core::sync::atomic::Ordering::Relaxed;
                let rate = DATA_RATE.load(Relaxed) & 0x7F;
                let fallback = DATA_FALLBACK.load(Relaxed);
                d.add(1).write_volatile(TXD1_QSEL_MGNT | TXD1_RAID_BG | dw1_extra);
                d.add(3).write_volatile(TXD3_USE_RATE | if fallback { 0 } else { TXD3_NO_FALLBACK });
                d.add(4).write_volatile(rate | if fallback { 0x1F << TXD4_FALLBACK_LIMIT_SHIFT } else { 0 });
            } else {
                // Management frames (scan, join, keys) — 1 Mbit/s without fallback: everyone hears them.
                d.add(1).write_volatile(TXD1_QSEL_MGNT | TXD1_RAID_B | dw1_extra);
                d.add(3).write_volatile(TXD3_USE_RATE | TXD3_NO_FALLBACK);
            }
            d.add(TXD_SIZE_WORD).write_volatile(len);
            d.add(8).write_volatile(TXD8_HWSEQ);
            d.add(TXD_BUF_WORD).write_volatile(buf as u32);
        }
        fence(Ordering::SeqCst);
        unsafe { d.write_volatile(TXD_OWN | TXD_FIRST | TXD_LAST | broadcast | TXD_OFFSET | len) };
        fence(Ordering::SeqCst);
        chip.w16(super::PCIE_CTRL_REG, self.poll);
        self.cur = (self.cur + 1) % TX_COUNT;
        true
    }

    /// How many frames the card still holds (not sent).
    pub fn pending(&self) -> usize {
        (0..TX_COUNT).filter(|&i| unsafe { self.ring.add(i * TX_WORDS).read_volatile() } & TXD_OWN != 0).count()
    }
}

/// A probe request for "any network" with 802.11b/g rates.
pub(super) fn probe_request(mac: &[u8; 6], channel: u8) -> alloc::vec::Vec<u8> {
    let mut f = alloc::vec![0x40, 0x00, 0, 0];
    f.extend_from_slice(&[0xFF; 6]);
    f.extend_from_slice(mac);
    f.extend_from_slice(&[0xFF; 6]);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8B, 0x96, 0x0C, 0x12, 0x18, 0x24]);
    f.extend_from_slice(&[3, 1, channel]);
    f.extend_from_slice(&[50, 4, 0x30, 0x48, 0x60, 0x6C]);
    f
}
