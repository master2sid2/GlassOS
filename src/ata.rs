//! An ATA (IDE) disk on the primary channel: polling without interrupts. Data goes through
//! the PIIX controller's Bus Master DMA (if present), otherwise through the port (PIO).
//! The simplest way to get persistent storage in QEMU (`-device piix3-ide`).
//!
//! In a virtual machine PIO is very slow (every word is an exit to the emulator:
//! 8 MB took minutes to write), DMA hands the whole buffer to the emulator at once.

use crate::arch::{inb, insw, outb, outl, outsw};
use crate::interrupts::{self, HZ};
use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec;
use alloc::vec::Vec;

const IO: u16 = 0x1F0;
const CTRL: u16 = 0x3F6;
pub const SECTOR: usize = 512;

const STATUS_ERR: u8 = 0x01;
const STATUS_DRQ: u8 = 0x08;
const STATUS_BSY: u8 = 0x80;

pub struct Disk {
    pub sectors: u32,
    /// Sectors per transfer for PIO writes (WRITE MULTIPLE mode; 1 — the ordinary one).
    multiple: usize,
    /// DMA: the Bus Master ports, the PRD table and the buffer (up to 255 sectors).
    dma: Option<(u16, *mut u32, *mut u8)>,
}

// The buffer and the table are kernel memory; the file system owns the disk under its lock.
unsafe impl Send for Disk {}

const DMA_BUF: usize = 128 * 1024;

/// Bus Master IDE: the IDE controller registers on PCI (BAR4, I/O ports).
fn bus_master() -> Option<(u16, *mut u32, *mut u8)> {
    let dev = crate::pci::scan().into_iter().find(|d| d.class == 0x01 && d.subclass == 0x01)?;
    if dev.read(0x08) >> 8 & 0x80 == 0 {
        return None; // a controller without Bus Master
    }
    let bar = dev.read(0x20);
    if bar & 1 == 0 || bar & !3 == 0 {
        return None;
    }
    dev.write(0x04, dev.read(0x04) | 0b101); // ports + DMA
    let prd = unsafe { alloc_zeroed(Layout::from_size_align(16, 16).unwrap()) } as *mut u32;
    // The buffer is 64 KiB aligned: each PRD entry is exactly 64 KiB without crossing a boundary.
    let buf = unsafe { alloc_zeroed(Layout::from_size_align(DMA_BUF, 64 * 1024).unwrap()) };
    if (buf as u64) + DMA_BUF as u64 > 0xFFFF_FFFF {
        return None; // PRD only addresses the first 4 GiB
    }
    Some(((bar & !3) as u16, prd, buf))
}

fn status() -> u8 {
    inb(IO + 7)
}

/// The pause between status polls. In a virtual machine every port read is
/// an exit to the emulator under its global lock: polling without pauses keeps the
/// emulator itself from finishing the write (this is how 8 MB took minutes).
fn backoff() {
    for _ in 0..200 {
        core::hint::spin_loop();
    }
}

/// Waits for BSY to clear; false — an error or a timeout.
fn wait_ready() -> bool {
    for _ in 0..1_000_000 {
        let s = status();
        if s & STATUS_BSY == 0 {
            return s & STATUS_ERR == 0;
        }
        backoff();
    }
    false
}

/// Waits until ready for data transfer (DRQ).
fn wait_drq() -> bool {
    for _ in 0..1_000_000 {
        let s = status();
        if s & STATUS_BSY == 0 {
            if s & STATUS_ERR != 0 {
                return false;
            }
            if s & STATUS_DRQ != 0 {
                return true;
            }
        }
        backoff();
    }
    false
}

fn command(lba: u32, count: u8, cmd: u8) -> bool {
    if !wait_ready() {
        return false;
    }
    outb(IO + 6, 0xE0 | ((lba >> 24) & 0x0F) as u8);
    outb(IO + 2, count);
    outb(IO + 3, lba as u8);
    outb(IO + 4, (lba >> 8) as u8);
    outb(IO + 5, (lba >> 16) as u8);
    outb(IO + 7, cmd);
    true
}

impl Disk {
    /// Looks for the master disk on the primary channel.
    pub fn detect() -> Option<Disk> {
        if status() == 0xFF {
            return None; // nobody on the bus
        }
        outb(CTRL, 0x02); // no interrupts
        outb(IO + 6, 0xA0);
        for _ in 0..4 {
            status(); // ~400 ns delay after selecting the device
        }
        for reg in 2..=5 {
            outb(IO + reg, 0);
        }
        outb(IO + 7, 0xEC); // IDENTIFY
        if status() == 0 {
            return None;
        }
        for _ in 0..1_000_000 {
            if status() & STATUS_BSY == 0 {
                break;
            }
        }
        if inb(IO + 4) != 0 || inb(IO + 5) != 0 {
            return None; // this is not ATA (for example, ATAPI)
        }
        if !wait_drq() {
            return None;
        }
        let mut id = [0u16; 256];
        insw(IO, &mut id);
        let sectors = id[60] as u32 | (id[61] as u32) << 16;
        // In blocks of several sectors: in the emulator every transfer is a separate
        // asynchronous write, and writing sector by sector is many times slower.
        let max = (id[47] & 0xFF) as usize;
        let mut multiple = 1;
        if max >= 2 {
            let want = max.min(16) as u8;
            outb(IO + 2, want);
            outb(IO + 6, 0xA0);
            outb(IO + 7, 0xC6); // SET MULTIPLE MODE
            if wait_ready() {
                multiple = want as usize;
            }
        }
        let dma = bus_master();
        let mode = if dma.is_some() { alloc::format!("DMA") } else { alloc::format!("PIO, {multiple} sectors per transfer") };
        crate::kprintln!("ata: {} MiB, {mode}", sectors / 2048);
        (sectors > 0).then_some(Disk { sectors, multiple, dma })
    }

    /// A DMA transfer: `count` sectors (up to 255) between the disk and the DMA buffer.
    fn dma_transfer(&self, lba: u32, count: usize, write: bool) -> bool {
        let Some((bm, prd, buf)) = self.dma else { return false };
        let bytes = count * SECTOR;
        // The PRD table: 64 KiB pieces (length 0 means 64 KiB), the last one has the end bit.
        let mut left = bytes;
        let mut i = 0;
        while left > 0 {
            let n = left.min(64 * 1024);
            let end = if n == left { 0x8000_0000 } else { 0 };
            unsafe {
                prd.add(2 * i).write_volatile(buf as u32 + (i * 64 * 1024) as u32);
                prd.add(2 * i + 1).write_volatile((n as u32 & 0xFFFF) | end);
            }
            left -= n;
            i += 1;
        }
        outb(bm, 0);
        outl(bm + 4, prd as u32);
        outb(bm + 2, inb(bm + 2) | 0x06); // clear "interrupt" and "error"
        let read_dir = if write { 0 } else { 0x08 }; // bit 3: from the disk to memory
        outb(bm, read_dir);
        if !command(lba, count as u8, if write { 0xCA } else { 0xC8 }) {
            return false;
        }
        outb(bm, read_dir | 1); // start
        let until = interrupts::ticks() + 5 * HZ;
        let ok = loop {
            let st = inb(bm + 2);
            if st & 0x02 != 0 {
                break false; // a DMA error
            }
            if st & 0x01 == 0 || st & 0x04 != 0 {
                break true; // the transfer is complete
            }
            if interrupts::ticks() > until {
                break false;
            }
            backoff();
        };
        outb(bm, 0);
        ok && wait_ready()
    }

    /// Reads sectors from `lba` into `buf` (up to 255 sectors per command).
    pub fn read(&self, lba: u32, buf: &mut [u8]) -> bool {
        if let Some((_, _, dbuf)) = self.dma {
            for (n, chunk) in buf.chunks_mut(255 * SECTOR).enumerate() {
                let count = chunk.len().div_ceil(SECTOR);
                if !self.dma_transfer(lba + (n * 255) as u32, count, false) {
                    return false;
                }
                chunk.copy_from_slice(unsafe { core::slice::from_raw_parts(dbuf, chunk.len()) });
            }
            return true;
        }
        let mut words = vec![0u16; SECTOR / 2];
        let mut chunks: Vec<&mut [u8]> = buf.chunks_mut(SECTOR).collect();
        for (n, batch) in chunks.chunks_mut(255).enumerate() {
            if !command(lba + (n * 255) as u32, batch.len() as u8, 0x20) {
                return false;
            }
            for chunk in batch.iter_mut() {
                if !wait_drq() {
                    return false;
                }
                insw(IO, &mut words);
                for (j, w) in words.iter().enumerate().take(chunk.len() / 2) {
                    chunk[2 * j..2 * j + 2].copy_from_slice(&w.to_le_bytes());
                }
            }
        }
        true
    }

    /// Writes data (the last sector is padded with zeros) and flushes the disk cache.
    /// Writes data from sector `lba` (the tail is padded with zeros to a whole sector).
    /// One command — up to 255 sectors: every command takes about ten
    /// port accesses, and in a virtual machine each of them is expensive.
    pub fn write(&self, lba: u32, data: &[u8]) -> bool {
        self.write_data(lba, data) && self.flush()
    }

    /// The disk cache to the platters: expensive (in a VM it is a full sync on the host).
    fn flush(&self) -> bool {
        outb(IO + 7, 0xE7); // CACHE FLUSH
        wait_ready()
    }

    /// A write without flushing the disk cache: for the data stream of a large file. The flush
    /// is done by the write of the file's metadata that follows.
    pub fn write_data(&self, lba: u32, data: &[u8]) -> bool {
        if let Some((_, _, dbuf)) = self.dma {
            for (n, chunk) in data.chunks(255 * SECTOR).enumerate() {
                let count = chunk.len().div_ceil(SECTOR);
                let dst = unsafe { core::slice::from_raw_parts_mut(dbuf, count * SECTOR) };
                dst[..chunk.len()].copy_from_slice(chunk);
                dst[chunk.len()..].fill(0);
                if !self.dma_transfer(lba + (n * 255) as u32, count, true) {
                    return false;
                }
            }
            return true;
        }
        let m = self.multiple;
        let mut words = vec![0u16; SECTOR / 2 * m];
        let sectors: Vec<&[u8]> = data.chunks(SECTOR).collect();
        for (n, batch) in sectors.chunks(255).enumerate() {
            let cmd = if m > 1 { 0xC5 } else { 0x30 }; // WRITE MULTIPLE / WRITE SECTORS
            if !command(lba + (n * 255) as u32, batch.len() as u8, cmd) {
                return false;
            }
            for block in batch.chunks(m) {
                words.fill(0);
                for (k, chunk) in block.iter().enumerate() {
                    for (j, pair) in chunk.chunks(2).enumerate() {
                        words[k * SECTOR / 2 + j] = pair[0] as u16 | (*pair.get(1).unwrap_or(&0) as u16) << 8;
                    }
                }
                if !wait_drq() {
                    return false;
                }
                outsw(IO, &words[..block.len() * SECTOR / 2]);
            }
        }
        true
    }
}
