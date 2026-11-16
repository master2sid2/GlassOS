//! Our own page tables instead of the ones inherited from UEFI.
//!
//! All physical memory (and devices — up to 4 GiB at least) is mapped 1:1
//! with 2 MiB pages. The exception is the first 2 MiB: they are split into
//! 4 KiB pages, and the page at address 0 is not mapped. This way dereferencing a null
//! pointer causes a page fault instead of quietly reading garbage.
//!
//! Each user process has its own PML4: entry 0 (the whole kernel) is shared and
//! has no USER bit — ring 3 cannot reach it; entry 1 (512 GiB – 1 TiB) is
//! the process memory. The kernel accesses it not through user pointers but through
//! the `AddressSpace` page map — so a bad pointer from a system call
//! does not crash the kernel.

use crate::frames::UserPage;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

const PRESENT: u64 = 1;
const WRITABLE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const HUGE: u64 = 1 << 7;
const GIB: u64 = 1 << 30;

#[repr(C, align(4096))]
struct Table([u64; 512]);

fn new_table() -> &'static mut Table {
    Box::leak(Box::new(Table([0; 512])))
}

fn addr(t: &Table) -> u64 {
    t as *const Table as u64
}

/// Builds the tables and switches CR3. `max_phys` is the upper bound of physical memory.
pub fn init(max_phys: u64) {
    let gibs = (max_phys.max(4 * GIB).div_ceil(GIB) as usize).min(512);
    let pml4 = new_table();
    let pdpt = new_table();
    pml4.0[0] = addr(pdpt) | PRESENT | WRITABLE;
    for i in 0..gibs {
        let pd = new_table();
        for (j, entry) in pd.0.iter_mut().enumerate() {
            *entry = (i as u64 * GIB + j as u64 * (2 << 20)) | PRESENT | WRITABLE | HUGE;
        }
        if i == 0 {
            // The first 2 MiB — with 4 KiB pages, the zero page is not mapped.
            let pt = new_table();
            for (k, entry) in pt.0.iter_mut().enumerate().skip(1) {
                *entry = (k as u64 * 4096) | PRESENT | WRITABLE;
            }
            pd.0[0] = addr(pt) | PRESENT | WRITABLE;
        }
        pdpt.0[i] = addr(pd) | PRESENT | WRITABLE;
    }
    KERNEL_CR3.store(addr(pml4), Ordering::Relaxed);
    KERNEL_PDPT.store(addr(pdpt), Ordering::Relaxed);
    unsafe { asm!("mov cr3, {}", in(reg) addr(pml4), options(nostack, preserves_flags)) };
    enable_write_combining();
    crate::kprintln!("paging: own page tables, {gibs} GiB identity-mapped, null page unmapped");
}

/// The memory type table: entry 1 (the page PWT bit) becomes write-combining instead of
/// write-through. No one else uses such pages. Needed on every core.
pub fn enable_write_combining() {
    const PAT: u32 = 0x277;
    const ENTRY_1: u64 = 0xFF << 8;
    const WRITE_COMBINING: u64 = 1 << 8;
    crate::arch::wrmsr(PAT, crate::arch::rdmsr(PAT) & !ENTRY_1 | WRITE_COMBINING);
}

pub fn fault_address() -> u64 {
    let cr2: u64;
    unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)) };
    cr2
}

/// The kernel PML4 (its entry 0 is copied into every process address space).
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

/// The kernel gigabyte table (PML4 entry 0) — device areas are added here.
static KERNEL_PDPT: AtomicU64 = AtomicU64::new(0);

/// Maps a device register area (MMIO) 1:1 without caching. Needed for
/// BARs above the already mapped memory (on 64-bit buses they may be above 4 GiB).
pub fn map_mmio(phys: u64, len: u64) {
    const PCD_PWT: u64 = 1 << 4 | 1 << 3;
    // Two CPUs mapping the same gigabyte would each create their own table.
    static MAPPING: crate::sync::IrqLock<()> = crate::sync::IrqLock::new(());
    let _guard = MAPPING.lock();
    let pdpt = KERNEL_PDPT.load(Ordering::Relaxed) as *mut Table;
    if pdpt.is_null() || phys >= 512 * GIB {
        return;
    }
    let end = (phys + len.max(1)).min(512 * GIB);
    let mut gib = phys / GIB;
    while gib * GIB < end {
        let entry = unsafe { &mut (*pdpt).0[gib as usize] };
        if *entry & PRESENT == 0 {
            let pd = new_table();
            for (j, e) in pd.0.iter_mut().enumerate() {
                *e = (gib * GIB + j as u64 * (2 << 20)) | PRESENT | WRITABLE | HUGE | PCD_PWT;
            }
            *entry = addr(pd) | PRESENT | WRITABLE;
        }
        gib += 1;
    }
    // The new entries did not exist before — flushing the TLB is not required, but cheap.
    unsafe { asm!("mov {0}, cr3", "mov cr3, {0}", out(reg) _, options(nostack, preserves_flags)) };
}

/// The frame on screen: writes accumulate and go out in batches; without this a 2560×1440 frame is written
/// word by word out of order and takes a fraction of a second. Only 2 MiB pages lying entirely
/// inside the frame are taken: device registers may be next to it.
pub fn map_frame(phys: u64, len: u64) {
    const PAGE: u64 = 2 << 20;
    const PWT: u64 = 1 << 3;
    map_mmio(phys, len);
    let pdpt = KERNEL_PDPT.load(Ordering::Relaxed) as *mut Table;
    if pdpt.is_null() || phys + len >= 512 * GIB {
        return;
    }
    let (mut at, end) = (phys.next_multiple_of(PAGE), (phys + len) / PAGE * PAGE);
    while at < end {
        let entry = unsafe { (*pdpt).0[(at / GIB) as usize] };
        if entry & PRESENT != 0 && entry & HUGE == 0 {
            let pd = (entry & !0xFFF) as *mut Table;
            let slot = unsafe { &mut (*pd).0[(at % GIB / PAGE) as usize] };
            if *slot & HUGE != 0 {
                *slot = at | PRESENT | WRITABLE | HUGE | PWT;
            }
        }
        at += PAGE;
    }
    unsafe { asm!("mov {0}, cr3", "mov cr3, {0}", out(reg) _, options(nostack, preserves_flags)) };
}

pub fn kernel_cr3() -> u64 {
    KERNEL_CR3.load(Ordering::Relaxed)
}

pub fn current_cr3() -> u64 {
    let cr3: u64;
    unsafe { asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags)) };
    cr3
}

pub fn switch_cr3(cr3: u64) {
    if current_cr3() != cr3 {
        unsafe { asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags)) };
    }
}

pub const PAGE: u64 = 4096;
/// Process memory: the whole PML4 entry 1.
pub const USER_BASE: u64 = 512 * GIB;
pub const USER_END: u64 = 1024 * GIB;

#[repr(C, align(4096))]
pub struct Page(pub [u8; 4096]);

/// A process address space: its own tables and pages.
pub struct AddressSpace {
    pml4: Box<Table>,
    tables: Vec<Box<Table>>,
    /// The virtual address of a page → (the page, whether it is writable).
    pages: BTreeMap<u64, (UserPage, bool)>,
    /// Where to hand out memory from for the map system call.
    pub heap_next: u64,
}

// The tables are ordinary kernel memory; one thread owns them.
unsafe impl Send for AddressSpace {}

impl AddressSpace {
    pub fn new() -> AddressSpace {
        let mut pml4 = Box::new(Table([0; 512]));
        let kernel = unsafe { &*(kernel_cr3() as *const Table) };
        pml4.0[0] = kernel.0[0];
        AddressSpace { pml4, tables: Vec::new(), pages: BTreeMap::new(), heap_next: USER_BASE + 64 * GIB }
    }

    pub fn cr3(&self) -> u64 {
        addr(&self.pml4)
    }

    /// How much memory the process takes (pages and tables).
    pub fn bytes(&self) -> usize {
        (self.pages.len() + self.tables.len() + 1) * PAGE as usize
    }

    /// Maps a zeroed page at address `va` (if it is not there yet) and returns it.
    pub fn map(&mut self, va: u64, writable: bool) -> Option<&mut Page> {
        let va = va & !(PAGE - 1);
        if !(USER_BASE..USER_END).contains(&va) {
            return None;
        }
        if !self.pages.contains_key(&va) {
            let page = UserPage::new()?;
            let flags = PRESENT | USER | if writable { WRITABLE } else { 0 };
            let mut table: *mut Table = &mut *self.pml4;
            for shift in [39, 30, 21] {
                let entry = unsafe { &mut (*table).0[(va >> shift) as usize & 511] };
                if *entry & PRESENT == 0 {
                    let mut next = Box::new(Table([0; 512]));
                    *entry = addr(&next) | PRESENT | WRITABLE | USER;
                    table = &mut *next;
                    self.tables.push(next);
                } else {
                    table = (*entry & 0x000F_FFFF_FFFF_F000) as *mut Table;
                }
            }
            unsafe { (*table).0[(va >> 12) as usize & 511] = page.0.as_ptr() as u64 | flags };
            self.pages.insert(va, (page, writable));
        }
        self.pages.get_mut(&va).map(|(p, _)| &mut **p)
    }

    /// Copies process memory; None — if at least one page is not mapped.
    pub fn read(&self, va: u64, len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        let mut at = va;
        let end = va.checked_add(len as u64)?;
        while at < end {
            let (page, _) = self.pages.get(&(at & !(PAGE - 1)))?;
            let off = (at % PAGE) as usize;
            let n = (PAGE as usize - off).min((end - at) as usize);
            out.extend_from_slice(&page.0[off..off + n]);
            at += n as u64;
        }
        Some(out)
    }

    /// Writes to process memory (only to pages writable for it).
    pub fn write(&mut self, va: u64, data: &[u8]) -> bool {
        let Some(end) = va.checked_add(data.len() as u64) else { return false };
        let mut at = va;
        while at < end {
            if !self.pages.get(&(at & !(PAGE - 1))).is_some_and(|(_, w)| *w) {
                return false;
            }
            at = (at & !(PAGE - 1)) + PAGE;
        }
        self.poke(va, data)
    }

    /// Writes to already mapped pages without checking rights (for the loader).
    pub fn poke(&mut self, va: u64, data: &[u8]) -> bool {
        let mut at = va;
        let mut rest = data;
        while !rest.is_empty() {
            let Some((page, _)) = self.pages.get_mut(&(at & !(PAGE - 1))) else { return false };
            let off = (at % PAGE) as usize;
            let n = (PAGE as usize - off).min(rest.len());
            page.0[off..off + n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            at += n as u64;
        }
        true
    }
}
