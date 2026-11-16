//! Device interrupts through MSI. The device itself writes the vector number to the local APIC
//! of CPU 0 (without an interrupt controller and without parsing ACPI), the handler marks
//! "an interrupt happened" and wakes the driver thread: it sleeps while the device has nothing to
//! say, and wakes right away when it does.

use crate::pci;
use crate::sync::WaitQueue;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Vectors 0x40.. — one per device.
pub const FIRST: u8 = 0x40;
const COUNT: usize = 16;

static USED: AtomicUsize = AtomicUsize::new(0);
static PENDING: [AtomicBool; COUNT] = [const { AtomicBool::new(false) }; COUNT];
static HITS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];
static QUEUES: [WaitQueue; COUNT] = [const { WaitQueue::new() }; COUNT];
static NAMES: crate::sync::IrqLock<[&str; COUNT]> = crate::sync::IrqLock::new([""; COUNT]);
/// Whom else to call from the handler (`fn()`; 0 — nobody): for example, the network
/// stack thread, which waits for all cards at once.
static HOOKS: [AtomicUsize; COUNT] = [const { AtomicUsize::new(0) }; COUNT];

global_asm!(
    // The common part: the registers a call clobbers and FPU/SSE go on the stack (as for PS/2),
    // the interrupt number goes to rdi (the vector stub put it there).
    ".global msi_common",
    "msi_common:",
    "    push rax",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    sub rsp, 512",
    "    .byte 0x48, 0x0f, 0xae, 0x04, 0x24",
    "    call {handler}",
    "    .byte 0x48, 0x0f, 0xae, 0x0c, 0x24",
    "    add rsp, 512",
    "    mov rax, qword ptr [rip + {eoi}]",
    "    test rax, rax",
    "    jz 2f",
    "    mov dword ptr [rax], 0",
    "2:",
    "    pop r11",
    "    pop r10",
    "    pop r9",
    "    pop r8",
    "    pop rsi",
    "    pop rdx",
    "    pop rcx",
    "    pop rax",
    "    pop rdi",
    "    iretq",
    ".global isr_msi_0", "isr_msi_0:", "    push rdi", "    mov edi, 0", "    jmp msi_common",
    ".global isr_msi_1", "isr_msi_1:", "    push rdi", "    mov edi, 1", "    jmp msi_common",
    ".global isr_msi_2", "isr_msi_2:", "    push rdi", "    mov edi, 2", "    jmp msi_common",
    ".global isr_msi_3", "isr_msi_3:", "    push rdi", "    mov edi, 3", "    jmp msi_common",
    ".global isr_msi_4", "isr_msi_4:", "    push rdi", "    mov edi, 4", "    jmp msi_common",
    ".global isr_msi_5", "isr_msi_5:", "    push rdi", "    mov edi, 5", "    jmp msi_common",
    ".global isr_msi_6", "isr_msi_6:", "    push rdi", "    mov edi, 6", "    jmp msi_common",
    ".global isr_msi_7", "isr_msi_7:", "    push rdi", "    mov edi, 7", "    jmp msi_common",
    ".global isr_msi_8", "isr_msi_8:", "    push rdi", "    mov edi, 8", "    jmp msi_common",
    ".global isr_msi_9", "isr_msi_9:", "    push rdi", "    mov edi, 9", "    jmp msi_common",
    ".global isr_msi_10", "isr_msi_10:", "    push rdi", "    mov edi, 10", "    jmp msi_common",
    ".global isr_msi_11", "isr_msi_11:", "    push rdi", "    mov edi, 11", "    jmp msi_common",
    ".global isr_msi_12", "isr_msi_12:", "    push rdi", "    mov edi, 12", "    jmp msi_common",
    ".global isr_msi_13", "isr_msi_13:", "    push rdi", "    mov edi, 13", "    jmp msi_common",
    ".global isr_msi_14", "isr_msi_14:", "    push rdi", "    mov edi, 14", "    jmp msi_common",
    ".global isr_msi_15", "isr_msi_15:", "    push rdi", "    mov edi, 15", "    jmp msi_common",
    ".section .rdata",
    ".balign 8",
    ".global msi_table",
    "msi_table:",
    "    .quad isr_msi_0, isr_msi_1, isr_msi_2, isr_msi_3, isr_msi_4, isr_msi_5, isr_msi_6, isr_msi_7, isr_msi_8, isr_msi_9, isr_msi_10, isr_msi_11, isr_msi_12, isr_msi_13, isr_msi_14, isr_msi_15",
    ".text",
    handler = sym on_msi,
    eoi = sym crate::interrupts::LAPIC_EOI,
);

unsafe extern "C" {
    static msi_table: [u64; COUNT];
}

/// The stub addresses: `interrupts::init` puts them into the interrupt table.
pub fn stubs() -> impl Iterator<Item = (u8, usize)> {
    (0..COUNT).map(|i| (FIRST + i as u8, unsafe { msi_table[i] } as usize))
}

extern "sysv64" fn on_msi(n: usize) {
    if n < COUNT {
        HITS[n].fetch_add(1, Ordering::Relaxed);
        PENDING[n].store(true, Ordering::Release);
        QUEUES[n].notify();
        let hook = HOOKS[n].load(Ordering::Acquire);
        if hook != 0 {
            let hook: fn() = unsafe { core::mem::transmute(hook) };
            hook();
        }
    }
}

/// A device interrupt: the driver thread waits for it in `wait`.
pub struct Irq {
    n: usize,
}

impl Irq {
    /// Waits for an interrupt until tick `deadline`. true — there was one (since the last call).
    pub fn wait(&self, deadline: u64) -> bool {
        QUEUES[self.n].wait_until(deadline, || PENDING[self.n].swap(false, Ordering::AcqRel))
    }

    /// Clear the mark (the driver is about to handle everything anyway).
    pub fn clear(&self) {
        PENDING[self.n].store(false, Ordering::Release);
    }
}

/// Enables MSI on a device (one message to CPU 0). None — the device does not support MSI
/// or the vectors ran out: the driver polls, as before.
pub fn msi(dev: &pci::Device, name: &'static str) -> Option<Irq> {
    let cap = dev.capability(0x05);
    if cap == 0 {
        return None;
    }
    let n = USED.fetch_add(1, Ordering::Relaxed);
    if n >= COUNT {
        return None;
    }
    NAMES.lock()[n] = name;
    let control = dev.read(cap) >> 16;
    let wide = control & 1 << 7 != 0;
    // The address: the local APIC window, CPU number 0; the data: the vector, edge-triggered.
    dev.write(cap + 4, 0xFEE0_0000 | crate::interrupts::boot_apic_id() << 12);
    let data_at = if wide {
        dev.write(cap + 8, 0);
        cap + 12
    } else {
        cap + 8
    };
    dev.write(data_at, (FIRST as u32) + n as u32);
    // The per-vector mask (if present) — clear it.
    if control & 1 << 8 != 0 {
        dev.write(if wide { cap + 16 } else { cap + 12 }, 0);
    }
    // One message, MSI on; the old line interrupt (INTx) is turned off.
    let head = dev.read(cap);
    dev.write(cap, (head & !(0x7 << 20)) | 1 << 16);
    dev.write(0x04, dev.read(0x04) | 1 << 10);
    crate::kprintln!("irq: {name} uses MSI vector {:#x}", FIRST as usize + n);
    Some(Irq { n })
}

/// The same, but the interrupt also calls `hook` (straight from the handler: only to wake).
pub fn msi_hook(dev: &pci::Device, name: &'static str, hook: fn()) -> Option<Irq> {
    let irq = msi(dev, name)?;
    HOOKS[irq.n].store(hook as usize, Ordering::Release);
    Some(irq)
}

/// For `irq`: who has which vector and how many interrupts there were.
pub fn list() -> alloc::vec::Vec<alloc::string::String> {
    let names = *NAMES.lock();
    (0..USED.load(Ordering::Relaxed).min(COUNT)).map(|i| alloc::format!("{:#x}  {:<12} {}", FIRST as usize + i, names[i], HITS[i].load(Ordering::Relaxed))).collect()
}
