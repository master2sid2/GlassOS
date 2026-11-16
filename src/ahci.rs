//! SATA disks through an AHCI controller (PCI class 01.06, interface 01) — this is how
//! SATA works on almost all computers. Polling without interrupts: a command
//! goes to slot 0, and we wait until the controller clears it.
//!
//! Transfers go through our own buffer (`BOUNCE`): this way it does not matter where the caller's
//! data lies (alignment, memory above 4 GiB with a controller without 64-bit addresses).

use crate::interrupts::{self, HZ};
use crate::sync::YieldLock;
use alloc::alloc::{Layout, alloc_zeroed};
use alloc::string::String;
use alloc::vec::Vec;

pub const SECTOR: usize = 512;
/// The transfer buffer is 256 KiB, that is, up to 512 sectors per command.
const BOUNCE: usize = 256 * 1024;

// Controller (HBA) registers.
const CAP: usize = 0x00;
const GHC: usize = 0x04;
const PI: usize = 0x0C;
const CAP2: usize = 0x24;
const BOHC: usize = 0x28;
// Port registers (0x100 + 0x80 * number).
const P_CLB: usize = 0x00;
const P_FB: usize = 0x08;
const P_IS: usize = 0x10;
const P_IE: usize = 0x14;
const P_CMD: usize = 0x18;
const P_TFD: usize = 0x20;
const P_SIG: usize = 0x24;
const P_SSTS: usize = 0x28;
const P_SCTL: usize = 0x2C;
const P_SERR: usize = 0x30;
const P_CI: usize = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_SUD: u32 = 1 << 1;
const CMD_POD: u32 = 1 << 2;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const TFD_ERR: u32 = 1 << 0;
const TFD_DRQ: u32 = 1 << 3;
const TFD_BSY: u32 = 1 << 7;
const IS_TFES: u32 = 1 << 30;

const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_EXT: u8 = 0xEA;

/// A disk on a controller port.
pub struct Disk {
    /// The port registers.
    port: usize,
    /// The command list (1 KiB), the received FIS area (256 bytes), the command table
    /// (a 0x80 header and one PRD entry) and the transfer buffer.
    list: *mut u32,
    table: *mut u8,
    buf: *mut u8,
    pub sectors: u64,
    pub model: String,
    /// The controller and port number — for the log and the disk list.
    pub name: String,
}

unsafe impl Send for Disk {}

static DISKS: YieldLock<Vec<Disk>> = YieldLock::new(Vec::new());
/// Controllers: (PCI device, registers) — for the `hwprobe` report.
static HBAS: YieldLock<Vec<(crate::pci::Device, usize)>> = YieldLock::new(Vec::new());

fn reg(base: usize, r: usize) -> u32 {
    unsafe { ((base + r) as *const u32).read_volatile() }
}

fn set(base: usize, r: usize, v: u32) {
    unsafe { ((base + r) as *mut u32).write_volatile(v) }
}

/// Waits until `done` becomes true, but no longer than `ms` milliseconds (by the timer;
/// if the timer is not running — by the number of attempts).
fn wait(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let until = interrupts::ticks() + ms * HZ / 1000 + 1;
    for _ in 0..50_000_000u64 {
        if done() {
            return true;
        }
        if interrupts::ticks() > until {
            return done();
        }
        core::hint::spin_loop();
    }
    done()
}

fn alloc(size: usize, align: usize) -> *mut u8 {
    unsafe { alloc_zeroed(Layout::from_size_align(size, align).unwrap()) }
}

/// Finds AHCI controllers and the disks on them. Called once at boot.
pub fn init(devices: &[crate::pci::Device]) -> Vec<String> {
    let mut found = Vec::new();
    for (n, dev) in devices.iter().filter(|d| d.class == 0x01 && d.subclass == 0x06 && (d.read(0x08) >> 8) & 0xFF == 0x01).enumerate() {
        let abar = dev.bar(5);
        if abar == 0 {
            continue;
        }
        dev.enable_bus_master();
        let hba = dev.mmio(5, 0x1100);
        HBAS.lock().push((dev.clone(), hba));
        let cap = reg(hba, CAP);
        // The firmware may be holding the controller (BIOS/OS handoff).
        if reg(hba, CAP2) & 1 != 0 {
            set(hba, BOHC, reg(hba, BOHC) | 1 << 1);
            wait(2000, || reg(hba, BOHC) & 1 == 0);
        }
        set(hba, GHC, reg(hba, GHC) | 1 << 31); // AHCI mode
        set(hba, GHC, reg(hba, GHC) & !(1 << 1)); // no interrupts
        let wide = cap & 1 << 31 != 0;
        let mut ports = reg(hba, PI);
        crate::kprintln!(
            "ahci: {:02x}:{:02x}.{} {:04x}:{:04x}, cap {cap:#x}, ports {ports:#x}, 64-bit {wide}",
            dev.bus,
            dev.slot,
            dev.func,
            dev.vendor,
            dev.device
        );
        // The firmware did not say which ports exist (this happens on Bay Trail): take
        // their number from CAP, and on Intel also enable the ports in PCS (configuration 0x92).
        let forced = ports == 0;
        if forced {
            let n = (cap & 0x1F) + 1;
            ports = if n >= 32 { u32::MAX } else { (1 << n) - 1 };
            if dev.vendor == 0x8086 {
                let v = dev.read(0x90);
                let pcs = (v >> 16) as u16;
                let wanted = pcs | (ports & 0xFF) as u16;
                dev.write(0x90, v & 0xFFFF | (wanted as u32) << 16);
                crate::kprintln!("ahci: no ports listed, using {n} from CAP; PCS {pcs:#06x} -> {:#06x}", (dev.read(0x90) >> 16) as u16);
            } else {
                crate::kprintln!("ahci: no ports listed, using {n} from CAP");
            }
            set(hba, PI, ports); // the register is write-once — usually the firmware does it
        }
        for p in 0..32 {
            if ports & 1 << p == 0 {
                continue;
            }
            let port = hba + 0x100 + p * 0x80;
            match Disk::open(port, cap, wide, forced, alloc::format!("sata{n}.{p}")) {
                Ok(disk) => {
                    let size = crate::apps::transfer::size_text(disk.sectors * SECTOR as u64);
                    crate::kprintln!("ahci: port {p}: {} \"{}\", {size}", disk.name, disk.model);
                    found.push(alloc::format!("{} {size}", disk.model));
                    DISKS.lock().push(disk);
                }
                Err(why) if why.is_empty() => {}
                Err(why) => crate::kprintln!("ahci: port {p}: {why}"),
            }
        }
    }
    found
}

impl Disk {
    /// Prepares a port and identifies the disk. An empty error — nobody is on the port.
    fn open(port: usize, cap: u32, wide: bool, forced: bool, name: String) -> Result<Disk, String> {
        // Stop the port to change the addresses of its structures.
        set(port, P_CMD, reg(port, P_CMD) & !CMD_ST);
        if !wait(500, || reg(port, P_CMD) & CMD_CR == 0) {
            return Err("command engine does not stop".into());
        }
        set(port, P_CMD, reg(port, P_CMD) & !CMD_FRE);
        if !wait(500, || reg(port, P_CMD) & CMD_FR == 0) {
            return Err("FIS receive does not stop".into());
        }
        if cap & 1 << 27 != 0 {
            set(port, P_CMD, reg(port, P_CMD) | CMD_SUD | CMD_POD); // spin up the disk
        }
        // The link: DET = 3 (a device is present and the link is up). If the firmware did not prepare the port
        // (or the device is visible but there is no link) — a line reset (COMRESET).
        if !wait(50, || reg(port, P_SSTS) & 0xF == 3) {
            let det = reg(port, P_SSTS) & 0xF;
            if !forced && det != 1 {
                return Err(String::new());
            }
            let sctl = reg(port, P_SCTL) & !0xF;
            set(port, P_SCTL, sctl | 1);
            wait(2, || false);
            set(port, P_SCTL, sctl);
            if !wait(1000, || reg(port, P_SSTS) & 0xF == 3) {
                let ssts = reg(port, P_SSTS);
                return Err(if forced && ssts & 0xF == 0 { String::new() } else { alloc::format!("no link (status {ssts:#x})") });
            }
            crate::kprintln!("ahci: {name}: link up after reset");
        }
        let sig = reg(port, P_SIG);
        if sig != 0x0000_0101 && sig != 0xFFFF_FFFF {
            return Err(if sig == 0xEB14_0101 { String::new() } else { alloc::format!("not a disk (signature {sig:#x})") });
        }

        let list = alloc(1024 + 256, 1024) as *mut u32;
        let fis = list as usize + 1024;
        let table = alloc(0x100, 128);
        let buf = alloc(BOUNCE, 4096);
        let top = [list as usize, table as usize, buf as usize + BOUNCE].into_iter().max().unwrap_or(0);
        if list.is_null() || table.is_null() || buf.is_null() || (!wide && top > 0xFFFF_FFFF) {
            return Err("no DMA memory".into());
        }
        set(port, P_CLB, list as u32);
        set(port, P_CLB + 4, (list as u64 >> 32) as u32);
        set(port, P_FB, fis as u32);
        set(port, P_FB + 4, (fis as u64 >> 32) as u32);
        set(port, P_SERR, 0xFFFF_FFFF);
        set(port, P_IS, 0xFFFF_FFFF);
        set(port, P_IE, 0);
        set(port, P_CMD, reg(port, P_CMD) | CMD_FRE);
        if !wait(1000, || reg(port, P_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
            return Err(alloc::format!("disk stays busy (status {:#x})", reg(port, P_TFD)));
        }
        set(port, P_CMD, reg(port, P_CMD) | CMD_ST);

        let mut disk = Disk { port, list, table, buf, sectors: 0, model: String::new(), name };
        if !disk.command(ATA_IDENTIFY, 0, 1, false) {
            return Err(alloc::format!("IDENTIFY failed (status {:#x})", reg(port, P_TFD)));
        }
        let id: &[u16] = unsafe { core::slice::from_raw_parts(buf as *const u16, 256) };
        let lba48 = id[83] & 1 << 10 != 0;
        disk.sectors = if lba48 {
            id[100] as u64 | (id[101] as u64) << 16 | (id[102] as u64) << 32 | (id[103] as u64) << 48
        } else {
            id[60] as u64 | (id[61] as u64) << 16
        };
        // The model: words 27–46, the bytes in each word are swapped.
        let model: Vec<u8> = id[27..47].iter().flat_map(|w| [(w >> 8) as u8, *w as u8]).collect();
        disk.model = String::from_utf8_lossy(&model).trim().into();
        if !lba48 {
            return Err(alloc::format!("{}: no LBA48", disk.model));
        }
        Ok(disk)
    }

    /// Executes an ATA command in slot 0. The data is in the transfer buffer (`count` sectors).
    fn command(&mut self, cmd: u8, lba: u64, count: usize, write: bool) -> bool {
        let port = self.port;
        if !wait(1000, || reg(port, P_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
            return false;
        }
        let bytes = count * SECTOR;
        unsafe {
            // The command header: FIS length 5 dwords, a write, one PRD entry.
            let flags = 5 | (write as u32) << 6 | ((bytes > 0) as u32) << 16;
            self.list.write_volatile(flags);
            self.list.add(1).write_volatile(0);
            self.list.add(2).write_volatile(self.table as u32);
            self.list.add(3).write_volatile((self.table as u64 >> 32) as u32);
            // A "host to device" FIS.
            let fis = self.table;
            core::ptr::write_bytes(fis, 0, 0x80);
            let count16 = if cmd == ATA_IDENTIFY { 0 } else { count as u16 };
            let bytes_fis: [u8; 20] = [
                0x27,
                0x80,
                cmd,
                0,
                lba as u8,
                (lba >> 8) as u8,
                (lba >> 16) as u8,
                1 << 6,
                (lba >> 24) as u8,
                (lba >> 32) as u8,
                (lba >> 40) as u8,
                0,
                count16 as u8,
                (count16 >> 8) as u8,
                0,
                0,
                0,
                0,
                0,
                0,
            ];
            core::ptr::copy_nonoverlapping(bytes_fis.as_ptr(), fis, bytes_fis.len());
            // The PRD entry: the buffer address and the length minus one.
            let prd = self.table.add(0x80) as *mut u32;
            prd.write_volatile(self.buf as u32);
            prd.add(1).write_volatile((self.buf as u64 >> 32) as u32);
            prd.add(2).write_volatile(0);
            prd.add(3).write_volatile((bytes.max(2) - 1) as u32);
        }
        set(port, P_IS, 0xFFFF_FFFF);
        set(port, P_CI, 1);
        let ok = wait(10_000, || reg(port, P_CI) & 1 == 0 || reg(port, P_IS) & IS_TFES != 0);
        let failed = !ok || reg(port, P_IS) & IS_TFES != 0 || reg(port, P_TFD) & TFD_ERR != 0;
        if failed {
            crate::kprintln!("ahci: {}: command {cmd:#x} at {lba} failed, status {:#x}, error {:#x}", self.name, reg(port, P_TFD), reg(port, P_SERR));
            self.recover();
        }
        !failed
    }

    /// After an error: restart the port (clear ST, clear the errors, set it again).
    fn recover(&mut self) {
        let port = self.port;
        set(port, P_CMD, reg(port, P_CMD) & !CMD_ST);
        wait(500, || reg(port, P_CMD) & CMD_CR == 0);
        set(port, P_SERR, 0xFFFF_FFFF);
        set(port, P_IS, 0xFFFF_FFFF);
        set(port, P_CMD, reg(port, P_CMD) | CMD_ST);
    }

    pub fn read(&mut self, lba: u64, buf: &mut [u8]) -> bool {
        let max = BOUNCE / SECTOR;
        for (i, chunk) in buf.chunks_mut(BOUNCE).enumerate() {
            let at = lba + (i * max) as u64;
            let count = chunk.len().div_ceil(SECTOR);
            if at + count as u64 > self.sectors || !self.command(ATA_READ_DMA_EXT, at, count, false) {
                return false;
            }
            unsafe { core::ptr::copy_nonoverlapping(self.buf, chunk.as_mut_ptr(), chunk.len()) };
        }
        true
    }

    /// A write; an incomplete last sector is padded with zeros.
    pub fn write(&mut self, lba: u64, data: &[u8]) -> bool {
        let max = BOUNCE / SECTOR;
        for (i, chunk) in data.chunks(BOUNCE).enumerate() {
            let at = lba + (i * max) as u64;
            let count = chunk.len().div_ceil(SECTOR);
            if at + count as u64 > self.sectors {
                return false;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(chunk.as_ptr(), self.buf, chunk.len());
                core::ptr::write_bytes(self.buf.add(chunk.len()), 0, count * SECTOR - chunk.len());
            }
            if !self.command(ATA_WRITE_DMA_EXT, at, count, true) {
                return false;
            }
        }
        true
    }

    pub fn flush(&mut self) -> bool {
        self.command(ATA_FLUSH_EXT, 0, 0, false)
    }
}

/// The controller and port registers as they are — for the hardware report.
pub fn report() -> Vec<String> {
    let mut out = Vec::new();
    for (dev, hba) in HBAS.lock().iter() {
        out.push(alloc::format!(
            "{:02x}:{:02x}.{} {:04x}:{:04x} cap {:#010x} ghc {:#010x} pi {:#010x} cap2 {:#010x} pcs/map {:#010x}",
            dev.bus,
            dev.slot,
            dev.func,
            dev.vendor,
            dev.device,
            reg(*hba, CAP),
            reg(*hba, GHC),
            reg(*hba, PI),
            reg(*hba, CAP2),
            dev.read(0x90)
        ));
        for p in 0..=(reg(*hba, CAP) & 0x1F) as usize {
            let port = hba + 0x100 + p * 0x80;
            out.push(alloc::format!(
                "  port {p}: ssts {:#06x} sctl {:#06x} serr {:#010x} sig {:#010x} cmd {:#010x} tfd {:#06x}",
                reg(port, P_SSTS),
                reg(port, P_SCTL),
                reg(port, P_SERR),
                reg(port, P_SIG),
                reg(port, P_CMD),
                reg(port, P_TFD)
            ));
        }
    }
    for d in DISKS.lock().iter() {
        out.push(alloc::format!("disk {}: \"{}\", {} sectors", d.name, d.model, d.sectors));
    }
    if out.is_empty() {
        out.push("no AHCI controllers".into());
    }
    out
}

/// Disks: (number, sector count, model, port name).
pub fn disks() -> Vec<(u32, u64, String, String)> {
    DISKS.lock().iter().enumerate().map(|(i, d)| (i as u32, d.sectors, d.model.clone(), d.name.clone())).collect()
}

pub fn disk_read(disk: u32, lba: u64, buf: &mut [u8]) -> bool {
    DISKS.lock().get_mut(disk as usize).is_some_and(|d| d.read(lba, buf))
}

/// A write with a disk cache flush: after it returns the data is on the medium.
pub fn disk_write(disk: u32, lba: u64, data: &[u8]) -> bool {
    DISKS.lock().get_mut(disk as usize).is_some_and(|d| d.write(lba, data) && d.flush())
}

/// A write without a cache flush (a file's data stream; the next `disk_write` flushes the cache).
pub fn disk_write_data(disk: u32, lba: u64, data: &[u8]) -> bool {
    DISKS.lock().get_mut(disk as usize).is_some_and(|d| d.write(lba, data))
}
