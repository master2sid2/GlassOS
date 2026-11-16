//! The other CPU cores (SMP).
//!
//! Helper cores (APs) run in the shared scheduler (`task`): they take any thread
//! except the main one, and when there is no work they sleep in their idle thread. The timer
//! tick only arrives at the main core; it wakes helpers with a "reschedule" interrupt
//! when there are ready threads without a CPU. Parallel work of the main thread
//! (`parallel`, for example blur rows while composing a frame) is picked up by helpers
//! in a "help" interrupt, briefly pausing their own thread.
//!
//! Startup: trampoline code is copied to a free page below 1 MiB, and all
//! other cores are sent INIT and SIPI twice at once. A core starts in
//! real mode; the trampoline enables PAE, long mode and paging
//! (the kernel's shared tables), takes a number and a stack, and jumps to `ap_main`.

use crate::interrupts::{self, HELP_VECTOR, RESCHED_VECTOR};
use crate::kprintln;
use alloc::vec::Vec;
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Helper stack: its idle thread lives on it (and interrupts while it sleeps).
const AP_STACK: usize = 64 * 1024;
/// We do not bring up more cores (and do not allocate stacks for them).
const MAX_APS: usize = crate::gdt::MAX_CPUS - 1;

global_asm!(
    ".global ap_trampoline_start",
    ".global ap_trampoline_end",
    ".global ap_gdtptr",
    ".global ap_jump",
    ".global ap_cr3",
    ".global ap_stack_base",
    ".global ap_entry",
    ".global ap_counter",
    ".global ap_long",
    ".code16",
    "ap_trampoline_start:",
    "    cli",
    "    mov ax, cs",
    "    mov ds, ax",
    // Real-mode addresses are offsets from the trampoline start (DS = CS = its segment).
    // Instructions are written as bytes: the assembler does not accept a label difference as an operand.
    "    .byte 0x0f, 0x01, 0x16", // lgdt [offset]
    "    .word ap_gdtptr - ap_trampoline_start",
    "    mov eax, cr4",
    "    or eax, 0x20", // PAE
    "    mov cr4, eax",
    "    .byte 0x66, 0xa1", // mov eax, dword [offset]
    "    .word ap_cr3 - ap_trampoline_start",
    "    mov cr3, eax",
    "    mov ecx, 0xC0000080", // EFER
    "    rdmsr",
    "    or eax, 0x100", // LME
    "    wrmsr",
    "    mov eax, cr0",
    "    or eax, 0x80000001", // PG | PE: from real mode straight into long mode
    "    mov cr0, eax",
    "    .byte 0x66, 0xff, 0x2e", // jmp far dword [offset] — into 64-bit code
    "    .word ap_jump - ap_trampoline_start",
    ".code64",
    "ap_long:",
    "    mov ax, 0x10",
    "    mov ds, ax",
    "    mov es, ax",
    "    mov ss, ax",
    // FPU and SSE: the kernel computes with SSE (CR0: MP, no EM; CR4: OSFXSR, OSXMMEXCPT).
    "    mov rax, cr0",
    "    and rax, -5",
    "    or rax, 2",
    "    mov cr0, rax",
    "    mov rax, cr4",
    "    or rax, 0x600",
    "    mov cr4, rax",
    "    fninit",
    // Core number by order of arrival; the stack is its own piece of a shared array.
    "    mov eax, 1",
    "    lock xadd dword ptr [rip + ap_counter], eax",
    "    mov edi, eax",
    "    lea rcx, [rax + 1]",
    "    imul rcx, rcx, {stack}",
    "    add rcx, qword ptr [rip + ap_stack_base]",
    "    and rcx, -16",
    "    mov rsp, rcx",
    "    mov rax, qword ptr [rip + ap_entry]",
    "    call rax",
    "2:",
    "    cli",
    "    hlt",
    "    jmp 2b",
    ".balign 8",
    "ap_gdt:",
    "    .quad 0",
    "    .quad 0x00AF9A000000FFFF", // code, 64-bit
    "    .quad 0x00CF92000000FFFF", // data
    "ap_gdtptr:",
    "    .word 23",
    "    .long 0", // GDT base — filled in at startup
    "ap_jump:",
    "    .long 0", // address of ap_long — filled in at startup
    "    .word 0x08",
    "ap_cr3:",
    "    .long 0",
    ".balign 8",
    "ap_stack_base:",
    "    .quad 0",
    "ap_entry:",
    "    .quad 0",
    "ap_counter:",
    "    .long 0",
    "ap_trampoline_end:",
    stack = const AP_STACK,
);

unsafe extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
    static ap_gdtptr: u8;
    static ap_jump: u8;
    static ap_cr3: u8;
    static ap_stack_base: u8;
    static ap_entry: u8;
    static ap_counter: u8;
}

/// How many helper cores started.
static APS: AtomicUsize = AtomicUsize::new(0);

pub fn cpu_count() -> usize {
    APS.load(Ordering::Relaxed) + 1
}

fn lapic_write(base: u64, reg: usize, val: u32) {
    unsafe { ((base as usize + reg) as *mut u32).write_volatile(val) };
}

fn lapic_read(base: u64, reg: usize) -> u32 {
    unsafe { ((base as usize + reg) as *const u32).read_volatile() }
}

/// IPI to everyone except self: `low` is the low word of the ICR (type and vector).
/// Stop all cores except our own (an NMI arrives even with interrupts disabled).
pub fn stop_others() {
    if APS.load(Ordering::Acquire) == 0 {
        return;
    }
    // Do not wait for the "delivery pending" bit forever: the kernel is dying, better not to get stuck here too.
    let base = interrupts::lapic_base();
    lapic_write(base, 0x310, 0);
    lapic_write(base, 0x300, 0b100 << 8 | 0b11 << 18);
}

fn ipi_all_but_self(base: u64, low: u32) {
    crate::sync::without_interrupts(|| {
        while lapic_read(base, 0x300) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        lapic_write(base, 0x310, 0);
        lapic_write(base, 0x300, low | 0b11 << 18); // shorthand: all excluding self
    })
}

/// Local APIC id of each CPU (by scheduler index).
static APIC_IDS: [AtomicU32; crate::gdt::MAX_CPUS] = [const { AtomicU32::new(0) }; crate::gdt::MAX_CPUS];

/// IPI to one CPU (`cpu` is the scheduler index). Interrupts are disabled meanwhile:
/// otherwise a handler on this same CPU would overwrite the destination halfway.
fn ipi(cpu: usize, vector: u8) {
    let base = interrupts::lapic_base();
    if base == 0 || cpu >= cpu_count() {
        return;
    }
    let id = APIC_IDS[cpu].load(Ordering::Relaxed);
    crate::sync::without_interrupts(|| {
        while lapic_read(base, 0x300) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        lapic_write(base, 0x310, id << 24);
        lapic_write(base, 0x300, vector as u32); // fixed, physical
    })
}

/// Asks CPU `cpu` to reconsider which thread runs on it.
pub fn reschedule(cpu: usize) {
    if cpu != crate::gdt::cpu() {
        ipi(cpu, RESCHED_VECTOR);
    }
}

/// The same for our own CPU: the interrupt arrives as soon as interrupts are enabled again
/// (for example, on leaving the handler that woke the main thread).
pub fn reschedule_self() {
    let base = interrupts::lapic_base();
    if base == 0 {
        return;
    }
    crate::sync::without_interrupts(|| {
        while lapic_read(base, 0x300) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        lapic_write(base, 0x300, RESCHED_VECTOR as u32 | 1 << 18); // to self
    })
}

fn delay_cycles(n: u64) {
    let t = crate::arch::rdtsc();
    while crate::arch::rdtsc() - t < n {
        core::hint::spin_loop();
    }
}

/// Brings up the other cores. `page` is a free page below 1 MiB for the trampoline.
pub fn init(page: u64) {
    let base = interrupts::lapic_base();
    if base != 0 {
        APIC_IDS[0].store(lapic_read(base, 0x20) >> 24, Ordering::Relaxed);
    }
    if base == 0 || page == 0 || page >= 0x10_0000 || page & 0xFFF != 0 {
        kprintln!("smp: no xAPIC or no low page, staying on one CPU");
        return;
    }
    let addr = |sym: &u8| sym as *const u8 as u64;
    let start = addr(unsafe { &ap_trampoline_start });
    let len = (addr(unsafe { &ap_trampoline_end }) - start) as usize;
    let off = |sym: &u8| (addr(sym) - start) as usize;
    let stacks: &'static mut [u8] = Vec::leak(alloc::vec![0u8; AP_STACK * MAX_APS]);
    unsafe {
        let dst = page as *mut u8;
        core::ptr::copy_nonoverlapping(start as *const u8, dst, len);
        let put32 = |o: usize, v: u32| (dst.add(o) as *mut u32).write_unaligned(v);
        let put64 = |o: usize, v: u64| (dst.add(o) as *mut u64).write_unaligned(v);
        let gdt = page + off(&ap_gdtptr) as u64 - 24;
        put32(off(&ap_gdtptr) + 2, gdt as u32);
        // Far jump from real mode — to ap_long at its address in the copy.
        put32(off(&ap_jump), (page + off(ap_long_marker()) as u64) as u32);
        put32(off(&ap_cr3), crate::paging::kernel_cr3() as u32);
        put64(off(&ap_stack_base), stacks.as_ptr() as u64);
        put64(off(&ap_entry), ap_main as *const () as u64);
        put32(off(&ap_counter), 0);
    }

    // INIT, a 10 ms pause, then SIPI twice (as the Intel MP spec requires).
    ipi_all_but_self(base, 0x4500);
    let t = interrupts::ticks();
    while interrupts::ticks() < t + 2 {
        core::hint::spin_loop();
    }
    let vector = (page >> 12) as u32;
    ipi_all_but_self(base, 0x4600 | vector);
    delay_cycles(1_000_000);
    ipi_all_but_self(base, 0x4600 | vector);

    // Wait for the cores to check in (50 ms is plenty).
    let counter = (page as usize + off(unsafe { &ap_counter })) as *const AtomicU32;
    let t = interrupts::ticks();
    while interrupts::ticks() < t + 5 {
        core::hint::spin_loop();
    }
    let started = unsafe { (*counter).load(Ordering::Acquire) } as usize;
    // Helpers register in the scheduler themselves; wait until all are up.
    let t = interrupts::ticks();
    while READY.load(Ordering::Acquire) < started.min(MAX_APS) && interrupts::ticks() < t + 10 {
        core::hint::spin_loop();
    }
    let ready = READY.load(Ordering::Acquire);
    APS.store(ready, Ordering::Release);
    kprintln!("smp: {} CPUs ({} helpers started, {} in the scheduler)", started + 1, started, ready);
}

/// How many helpers joined the scheduler.
static READY: AtomicUsize = AtomicUsize::new(0);

fn ap_long_marker() -> &'static u8 {
    unsafe extern "C" {
        static ap_long: u8;
    }
    unsafe { &ap_long }
}

/// Entry point of a helper core (the stack is already its own). `index` is its number from zero.
extern "sysv64" fn ap_main(index: u32) -> ! {
    let cpu = index as usize + 1;
    if cpu > MAX_APS {
        loop {
            unsafe { asm!("cli; hlt") };
        }
    }
    // Its own TSS GDT entry: code learns the CPU number from it, and it holds the ring 3 stack.
    crate::gdt::load(cpu);
    interrupts::load_idt();
    crate::paging::enable_write_combining();
    // Its own local APIC: enable it to receive IPIs.
    let base = interrupts::lapic_base();
    lapic_write(base, 0x080, 0);
    lapic_write(base, 0x0F0, 0x1FF);
    APIC_IDS[cpu].store(lapic_read(base, 0x20) >> 24, Ordering::Relaxed);
    crate::power::init_helper();
    crate::task::become_idle(cpu);
    READY.fetch_add(1, Ordering::AcqRel);
    // Idle thread: sleep until an interrupt; "reschedule" takes us to work and back here.
    loop {
        crate::power::sleep();
    }
}

// ---------------------------------------------------------------- parallel work

/// The current job: a pointer to `&dyn Fn(usize)` (lives while the main thread waits).
static JOB: AtomicU64 = AtomicU64::new(0);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// How many pieces are done.
static DONE: AtomicUsize = AtomicUsize::new(0);
/// The job is open: a helper may take it.
static OPEN: AtomicBool = AtomicBool::new(false);
/// How many helpers are inside the job now: while they are, it must stay alive.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

type Job<'a> = &'a (dyn Fn(usize) + Sync);

/// Takes pieces of the current job while there are any.
fn run_chunks() {
    let job = unsafe { *(JOB.load(Ordering::Acquire) as *const Job) };
    let count = COUNT.load(Ordering::Acquire);
    loop {
        let i = NEXT.fetch_add(1, Ordering::AcqRel);
        if i >= count {
            break;
        }
        job(i);
        DONE.fetch_add(1, Ordering::AcqRel);
    }
}

/// Handler of the "help" IPI on a helper (interrupts disabled).
pub extern "sysv64" fn help() {
    ACTIVE.fetch_add(1, Ordering::SeqCst);
    if OPEN.load(Ordering::SeqCst) {
        let started = crate::arch::rdtsc();
        run_chunks();
        crate::stats::add_helper_work(crate::arch::rdtsc() - started);
    }
    ACTIVE.fetch_sub(1, Ordering::SeqCst);
}

/// Runs `f(0)`, …, `f(n - 1)` on all cores and returns when everything is done.
/// Call only from the main thread (thread 0); otherwise it just runs them in order.
/// There is no need to wait for each helper: pieces nobody took are done by the main
/// thread itself; we only wait for those already inside.
pub fn parallel(n: usize, f: &(dyn Fn(usize) + Sync)) {
    let helpers = APS.load(Ordering::Acquire);
    if helpers == 0 || n < 2 || crate::task::current_id() != 0 {
        (0..n).for_each(f);
        return;
    }
    let job: Job = f;
    JOB.store(&job as *const Job as u64, Ordering::Release);
    COUNT.store(n, Ordering::Release);
    NEXT.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);
    OPEN.store(true, Ordering::SeqCst);
    ipi_all_but_self(interrupts::lapic_base(), 0x4000 | HELP_VECTOR as u32);
    run_chunks();
    let start = crate::arch::rdtsc();
    let mut told = false;
    let mut watch = |what: &str| {
        core::hint::spin_loop();
        if !told && crate::arch::rdtsc() - start > 3_000_000_000 {
            told = true;
            crate::sync::stuck(what, core::panic::Location::caller(), 0, 0);
        }
    };
    while DONE.load(Ordering::Acquire) < n {
        watch("parallel pieces");
    }
    OPEN.store(false, Ordering::SeqCst);
    while ACTIVE.load(Ordering::SeqCst) != 0 {
        watch("helpers to leave parallel work");
    }
}

/// A pointer that can be handed to other cores (each writes its own piece).
#[derive(Clone, Copy)]
pub struct SharedMut<T>(pub *mut T);
unsafe impl<T> Send for SharedMut<T> {}
unsafe impl<T> Sync for SharedMut<T> {}

impl<T> SharedMut<T> {
    /// Through a method, not a field: otherwise the closure would capture the bare pointer, not the wrapper.
    pub fn get(&self) -> *mut T {
        self.0
    }
}
