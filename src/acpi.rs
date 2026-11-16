//! ACPI — exactly as much as is needed to reboot and power off the computer
//! without the firmware: the reset register from the FADT and S5 sleep (the type is taken from the `_S5_` object in the DSDT).
//!
//! On real hardware calling UEFI ResetSystem after leaving Boot Services
//! is sometimes unreliable, so the firmware is only a fallback.

use crate::arch::{inb, inw, outb, outw};
use crate::kprintln;
use crate::sync::IrqLock;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

static RSDP: AtomicU64 = AtomicU64::new(0);
static POWER: IrqLock<Option<Power>> = IrqLock::new(None);

#[derive(Clone, Copy)]
struct Power {
    /// The reset register: (space: 0 — memory, 1 — ports, address, value).
    reset: Option<(u8, u64, u8)>,
    pm1a_cnt: u16,
    pm1b_cnt: u16,
    /// SLP_TYPa and SLP_TYPb for S5.
    s5: Option<(u16, u16)>,
    smi_cmd: u16,
    acpi_enable: u8,
}

/// Who released the DSDT: (OEM ID, OEM Table ID).
static DSDT_ID: IrqLock<Option<([u8; 6], [u8; 8])>> = IrqLock::new(None);
static FACS: AtomicU64 = AtomicU64::new(0);
/// Where the DSDT is (the physical address, which is also the virtual one: memory is mapped one-to-one).
static DSDT: AtomicU64 = AtomicU64::new(0);

/// The whole DSDT bytes: `aml` extracts names, regions and fields from them.
pub fn dsdt() -> Option<&'static [u8]> {
    let at = DSDT.load(Ordering::Relaxed);
    if at == 0 {
        return None;
    }
    let len: u32 = unsafe { read(at + 4) };
    Some(unsafe { core::slice::from_raw_parts(at as *const u8, len as usize) })
}
/// The Global Lock offset in the FACS and its bits.
const FACS_LOCK: u64 = 16;
const LOCK_PENDING: u32 = 1;
const LOCK_OWNED: u32 = 2;
/// The GBL_RLS bit in PM1_CNT: tell the firmware the lock is free.
const GBL_RLS: u16 = 1 << 2;

pub fn dsdt_id() -> Option<([u8; 6], [u8; 8])> {
    *DSDT_ID.lock()
}

fn global_lock() -> Option<&'static AtomicU32> {
    let facs = FACS.load(Ordering::Relaxed);
    (facs != 0).then(|| unsafe { &*((facs + FACS_LOCK) as *const AtomicU32) })
}

/// Tries to take the Global Lock shared with the firmware. Without a FACS there is nobody to share it with.
pub fn lock_global() -> bool {
    let Some(lock) = global_lock() else { return true };
    let old = lock.load(Ordering::Acquire);
    old & LOCK_OWNED == 0 && lock.compare_exchange(old, (old & !LOCK_PENDING) | LOCK_OWNED, Ordering::AcqRel, Ordering::Relaxed).is_ok()
}

pub fn unlock_global() {
    let Some(lock) = global_lock() else { return };
    let old = lock.fetch_and(!(LOCK_OWNED | LOCK_PENDING), Ordering::AcqRel);
    if old & LOCK_PENDING != 0
        && let Some(p) = *POWER.lock()
    {
        outw(p.pm1a_cnt, inw(p.pm1a_cnt) | GBL_RLS);
    }
}

/// While Boot Services are alive: remember where the RSDP is.
pub fn find_rsdp() {
    use uefi::table::cfg::ConfigTableEntry;
    let addr = uefi::system::with_config_table(|t| {
        let find = |g| t.iter().find(|e| e.guid == g).map(|e| e.address as u64);
        find(ConfigTableEntry::ACPI2_GUID).or_else(|| find(ConfigTableEntry::ACPI_GUID))
    });
    RSDP.store(addr.unwrap_or(0), Ordering::Relaxed);
}

unsafe fn read<T: Copy>(addr: u64) -> T {
    unsafe { (addr as *const T).read_unaligned() }
}

/// The table with signature `sig` from the XSDT (or the RSDT for ACPI 1.0).
fn find_table(sig: &[u8; 4]) -> Option<u64> {
    let rsdp = RSDP.load(Ordering::Relaxed);
    if rsdp == 0 || unsafe { read::<[u8; 8]>(rsdp) } != *b"RSD PTR " {
        return None;
    }
    let revision: u8 = unsafe { read(rsdp + 15) };
    let (root, wide) = if revision >= 2 && unsafe { read::<u64>(rsdp + 24) } != 0 {
        (unsafe { read::<u64>(rsdp + 24) }, true)
    } else {
        (unsafe { read::<u32>(rsdp + 16) } as u64, false)
    };
    let len: u32 = unsafe { read(root + 4) };
    let size = if wide { 8 } else { 4 };
    let count = (len as u64).saturating_sub(36) / size;
    (0..count)
        .map(|i| {
            let at = root + 36 + i * size;
            if wide { unsafe { read::<u64>(at) } } else { (unsafe { read::<u32>(at) }) as u64 }
        })
        .find(|&t| t != 0 && unsafe { read::<[u8; 4]>(t) } == *sig)
}

/// Looks for the `_S5_` package in the DSDT and extracts SLP_TYPa and SLP_TYPb from it.
fn parse_s5(dsdt: u64) -> Option<(u16, u16)> {
    let len: u32 = unsafe { read(dsdt + 4) };
    let body = unsafe { core::slice::from_raw_parts(dsdt as *const u8, len as usize) };
    let at = body.windows(4).position(|w| w == b"_S5_")?;
    // Before the name is a NameOp (0x08), sometimes with the root "\"; after it — a PackageOp (0x12).
    let named = at >= 1 && (body[at - 1] == 0x08 || (at >= 2 && body[at - 2] == 0x08 && body[at - 1] == b'\\'));
    if !named || *body.get(at + 4)? != 0x12 {
        return None;
    }
    let mut i = at + 5;
    i += ((body.get(i)? >> 6) & 3) as usize + 1; // the package length
    i += 1; // the number of elements
    let mut value = || -> Option<u16> {
        let b = *body.get(i)?;
        if b == 0x0A {
            // BytePrefix: the value is in the next byte.
            i += 2;
            Some(*body.get(i - 1)? as u16)
        } else {
            i += 1;
            Some(b as u16) // ZeroOp (0) and OneOp (1) are their own values
        }
    };
    let a = value()?;
    let b = value().unwrap_or(0);
    Some((a & 7, b & 7))
}

/// After memory is mapped: parse the FADT.
pub fn init() {
    let Some(fadt) = find_table(b"FACP") else {
        kprintln!("acpi: no FADT");
        return;
    };
    let len: u32 = unsafe { read(fadt + 4) };
    let flags: u32 = unsafe { read(fadt + 112) };
    let reset = (len >= 129 && flags & 1 << 10 != 0).then(|| unsafe {
        let space: u8 = read(fadt + 116);
        (space, read::<u64>(fadt + 120), read::<u8>(fadt + 128))
    });
    let mut dsdt = unsafe { read::<u32>(fadt + 40) } as u64;
    if len >= 148 {
        let x: u64 = unsafe { read(fadt + 140) };
        if x != 0 {
            dsdt = x;
        }
    }
    let s5 = if dsdt != 0 { parse_s5(dsdt) } else { None };
    if dsdt != 0 {
        *DSDT_ID.lock() = Some(unsafe { (read(dsdt + 10), read(dsdt + 16)) });
        DSDT.store(dsdt, Ordering::Relaxed);
    }
    let wide: u64 = if len >= 140 { unsafe { read(fadt + 132) } } else { 0 };
    FACS.store(if wide != 0 { wide } else { unsafe { read::<u32>(fadt + 36) as u64 } }, Ordering::Relaxed);
    let p = Power {
        reset,
        pm1a_cnt: unsafe { read::<u32>(fadt + 64) } as u16,
        pm1b_cnt: unsafe { read::<u32>(fadt + 68) } as u16,
        s5,
        smi_cmd: unsafe { read::<u32>(fadt + 48) } as u16,
        acpi_enable: unsafe { read(fadt + 52) },
    };
    kprintln!(
        "acpi: reset {:?}, PM1a_CNT {:#x}, S5 {:?}",
        p.reset.map(|(s, a, v)| (s, a, v)),
        p.pm1a_cnt,
        p.s5
    );
    *POWER.lock() = Some(p);
}

/// A pause without the timer (interrupts are already off): a port read ≈ 1 µs.
fn delay_ms(ms: u32) {
    for _ in 0..ms * 1000 {
        inb(0x80);
    }
}

/// A reboot through the hardware. Returns only if no method worked.
pub fn reboot() {
    let power = *POWER.lock();
    if let Some((space, addr, value)) = power.and_then(|p| p.reset) {
        kprintln!("acpi: reset register");
        match space {
            1 => outb(addr as u16, value),
            0 => {
                crate::paging::map_mmio(addr, 1);
                unsafe { (addr as *mut u8).write_volatile(value) };
            }
            _ => {}
        }
        delay_ms(100);
    }
    // The Intel chipset reset register: first a "CPU reset", then a "full" one.
    kprintln!("acpi: port 0xCF9");
    outb(0xCF9, 0x02);
    delay_ms(1);
    outb(0xCF9, 0x06);
    delay_ms(100);
    // The old method — through the keyboard controller.
    kprintln!("acpi: keyboard controller");
    for _ in 0..10000 {
        if inb(0x64) & 2 == 0 {
            break;
        }
        inb(0x80);
    }
    outb(0x64, 0xFE);
    delay_ms(100);
}

/// Power off through S5. Returns if it failed.
pub fn shutdown() {
    let Some(p) = *POWER.lock() else { return };
    let Some((a, b)) = p.s5 else { return };
    if p.pm1a_cnt == 0 {
        return;
    }
    // If the system is not in ACPI mode yet — enable it (SCI_EN in PM1_CNT).
    if inw(p.pm1a_cnt) & 1 == 0 && p.smi_cmd != 0 && p.acpi_enable != 0 {
        outb(p.smi_cmd, p.acpi_enable);
        for _ in 0..300 {
            if inw(p.pm1a_cnt) & 1 != 0 {
                break;
            }
            delay_ms(10);
        }
    }
    kprintln!("acpi: entering S5");
    const SLP_EN: u16 = 1 << 13;
    let sleep = |port: u16, typ: u16| outw(port, (inw(port) & !(7 << 10)) | typ << 10 | SLP_EN);
    if p.pm1b_cnt != 0 {
        sleep(p.pm1b_cnt, b);
    }
    sleep(p.pm1a_cnt, a);
    delay_ms(500);
}
