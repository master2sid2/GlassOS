//! Our own GDT: the kernel and user (ring 3) segments and the TSS.
//!
//! In long mode only one thing is needed from the TSS: `rsp0` — the stack the CPU switches to
//! when an interrupt or a system call catches code in ring 3. Before every switch
//! the scheduler puts the top of the next thread's kernel stack there.
//! Each CPU has its own TSS; code also learns which CPU it is on from it (`str`).

use alloc::boxed::Box;
use core::arch::asm;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const USER_DATA: u16 = 0x18 | 3;
pub const USER_CODE: u16 = 0x20 | 3;
const TSS: u16 = 0x28;

#[repr(C, packed)]
struct Tss {
    _reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    _reserved1: u64,
    ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    iomap_base: u16,
}

#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

/// The system does not bring up more CPUs (each has its own TSS in the shared GDT).
pub const MAX_CPUS: usize = 16;

static mut TSS_PTRS: [*mut Tss; MAX_CPUS] = [core::ptr::null_mut(); MAX_CPUS];
static mut GDTR: Gdtr = Gdtr { limit: 0, base: 0 };

/// Builds the shared GDT with a TSS for every possible CPU and loads it on the main one.
pub fn init() {
    let mut gdt = alloc::vec![
        0,
        0x00AF_9A00_0000_FFFF, // kernel code, 64-bit
        0x00CF_9200_0000_FFFF, // kernel data
        0x00CF_F200_0000_FFFF, // user data (DPL 3)
        0x00AF_FA00_0000_FFFF, // user code, 64-bit (DPL 3)
    ];
    for cpu in 0..MAX_CPUS {
        let tss = Box::leak(Box::new(Tss {
            _reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            _reserved1: 0,
            ist: [0; 7],
            _reserved2: 0,
            _reserved3: 0,
            // The I/O permission map is beyond the TSS limit: ring 3 gets access to no port at all.
            iomap_base: size_of::<Tss>() as u16,
        }));
        let base = tss as *mut Tss as u64;
        let limit = size_of::<Tss>() as u64 - 1;
        gdt.push((limit & 0xFFFF) | (base & 0xFF_FFFF) << 16 | 0x89 << 40 | (limit >> 16 & 0xF) << 48 | (base >> 24 & 0xFF) << 56);
        gdt.push(base >> 32);
        unsafe { (*core::ptr::addr_of_mut!(TSS_PTRS))[cpu] = tss };
    }
    let gdt = gdt.leak();
    unsafe { GDTR = Gdtr { limit: (size_of_val(gdt) - 1) as u16, base: gdt.as_ptr() as u64 } };
    load(0);
}

/// Loads the shared GDT on this CPU and takes TSS number `cpu`.
pub fn load(cpu: usize) {
    unsafe {
        asm!(
            "lgdt [{gdtr}]",
            // CS can only be reloaded with a far jump — through retfq.
            "push {code}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "mov fs, {zero:x}",
            "mov gs, {zero:x}",
            "ltr {tss:x}",
            gdtr = in(reg) core::ptr::addr_of!(GDTR),
            code = const KERNEL_CODE as u64,
            tmp = out(reg) _,
            data = in(reg) KERNEL_DATA as u64,
            zero = in(reg) 0u64,
            tss = in(reg) (TSS + 16 * cpu as u16) as u64,
        );
    }
}

/// The number of the CPU the code runs on: by its TSS (the `str` instruction, no memory access).
#[inline]
pub fn cpu() -> usize {
    let sel: u16;
    unsafe { asm!("str {0:x}", out(reg) sel, options(nomem, nostack, preserves_flags)) };
    (sel.saturating_sub(TSS) / 16) as usize
}

/// The kernel stack this CPU switches to on an interrupt from ring 3.
pub fn set_kernel_stack(top: u64) {
    let tss = unsafe { (*core::ptr::addr_of!(TSS_PTRS))[cpu()] };
    unsafe { core::ptr::addr_of_mut!((*tss).rsp0).write_unaligned(top) };
}
