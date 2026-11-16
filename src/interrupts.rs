//! IDT, the 8259 PIC and the PIT timer.
//!
//! IRQ handlers are minimal: the timer counts ticks and switches threads,
//! the keyboard and mouse push bytes into a queue (the main loop parses it),
//! the rest only send EOI.

use crate::arch::{self, outb, rdmsr};
use alloc::boxed::Box;
use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const HZ: u64 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);
/// Address of the local APIC EOI register (0 if the APIC is unavailable).
pub(crate) static LAPIC_EOI: AtomicU64 = AtomicU64::new(0);
/// Local APIC id of CPU 0: devices send MSIs there.
static BOOT_APIC_ID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

pub fn boot_apic_id() -> u32 {
    BOOT_APIC_ID.load(Ordering::Relaxed)
}

global_asm!(
    // Common tail: EOI to the LAPIC (in case the interrupt came through it), then to the PIC.
    ".macro lapic_eoi",
    "    mov rax, qword ptr [rip + {lapic_eoi}]",
    "    test rax, rax",
    "    jz 2f",
    "    mov dword ptr [rax], 0",
    "2:",
    ".endm",

    // Save and restore the FPU/SSE state (512 bytes) on the stack: the kernel itself computes
    // with SSE, so the interrupted code (kernel thread, process) must not lose it.
    // Written as bytes: fxsave64 [rsp] and fxrstor64 [rsp].
    ".macro save_fpu",
    "    sub rsp, 512",
    "    .byte 0x48, 0x0f, 0xae, 0x04, 0x24",
    ".endm",
    ".macro restore_fpu",
    "    .byte 0x48, 0x0f, 0xae, 0x0c, 0x24",
    "    add rsp, 512",
    ".endm",

    // The timer is also the thread switch point: all registers and FPU/SSE go to the
    // thread's stack, the scheduler returns the next thread's stack.
    ".global isr_timer",
    "isr_timer:",
    "    push rax",
    "    push rbx",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push rdi",
    "    push rbp",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    lock inc qword ptr [rip + {ticks}]",
    "    lapic_eoi",
    "    mov al, 0x20",
    "    out 0x20, al",
    "    xor esi, esi", // BY_TICK
    "switch_threads:",
    "    save_fpu",
    "    mov rdi, rsp",
    "    call {schedule}",
    "    mov rsp, rax",
    // Already on the new thread's stack: the old one is free for other CPUs.
    "    call {switched}",
    "    restore_fpu",
    "restore_regs:",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop r11",
    "    pop r10",
    "    pop r9",
    "    pop r8",
    "    pop rbp",
    "    pop rdi",
    "    pop rsi",
    "    pop rdx",
    "    pop rcx",
    "    pop rbx",
    "    pop rax",
    "    iretq",

    // int 0x81 — the thread yields the CPU itself.
    ".global isr_yield",
    "isr_yield:",
    "    push rax",
    "    push rbx",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push rdi",
    "    push rbp",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov esi, 1", // BY_YIELD
    "    jmp switch_threads",

    // Inter-processor "reschedule": another CPU found work for this one.
    ".global isr_resched",
    "isr_resched:",
    "    push rax",
    "    push rbx",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push rdi",
    "    push rbp",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    lapic_eoi",
    "    mov esi, 2", // BY_IPI
    "    jmp switch_threads",

    // Inter-processor "help": take pieces of parallel work (smp::parallel).
    ".global isr_help",
    "isr_help:",
    "    push rax",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push rdi",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    save_fpu",
    "    call {help}",
    "    restore_fpu",
    "    lapic_eoi",
    "    pop r11",
    "    pop r10",
    "    pop r9",
    "    pop r8",
    "    pop rdi",
    "    pop rsi",
    "    pop rdx",
    "    pop rcx",
    "    pop rax",
    "    iretq",

    // int 0x80 — a system call from ring 3. The process registers are on the kernel stack;
    // the handler puts the result in place of the saved rax.
    ".global isr_syscall",
    "isr_syscall:",
    "    push rax",
    "    push rbx",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push rdi",
    "    push rbp",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    save_fpu",
    "    lea rdi, [rsp + 512]",
    "    call {syscall}",
    "    restore_fpu",
    "    jmp restore_regs",

    // IRQ1/IRQ12 — keyboard and mouse: take the bytes into the queue right away.
    ".global isr_ps2",
    "isr_ps2:",
    "    push rax",
    "    push rcx",
    "    push rdx",
    "    push rsi",
    "    push rdi",
    "    push r8",
    "    push r9",
    "    push r10",
    "    push r11",
    "    save_fpu",
    "    call {ps2}",
    "    restore_fpu",
    "    lapic_eoi",
    "    mov al, 0x20",
    "    out 0xA0, al",
    "    out 0x20, al",
    "    pop r11",
    "    pop r10",
    "    pop r9",
    "    pop r8",
    "    pop rdi",
    "    pop rsi",
    "    pop rdx",
    "    pop rcx",
    "    pop rax",
    "    iretq",

    // Inter-processor "wake up" interrupt: the helper core only needs to leave hlt.
    ".global isr_wake",
    "isr_wake:",
    "    push rax",
    "    lapic_eoi",
    "    pop rax",
    "    iretq",

    ".global isr_master",
    "isr_master:",
    "    push rax",
    "    lapic_eoi",
    "    mov al, 0x20",
    "    out 0x20, al",
    "    pop rax",
    "    iretq",

    ".global isr_slave",
    "isr_slave:",
    "    push rax",
    "    lapic_eoi",
    "    mov al, 0x20",
    "    out 0xA0, al",
    "    out 0x20, al",
    "    pop rax",
    "    iretq",

    // Exceptions 0..31: each has its own stub that pushes the vector number
    // (and 0 instead of the error code where the CPU does not push one).
    ".global exc_0", "exc_0:", "    push 0", "    push 0", "    jmp exc_common",
    ".global exc_1", "exc_1:", "    push 0", "    push 1", "    jmp exc_common",
    ".global exc_2", "exc_2:", "    push 0", "    push 2", "    jmp exc_common",
    ".global exc_3", "exc_3:", "    push 0", "    push 3", "    jmp exc_common",
    ".global exc_4", "exc_4:", "    push 0", "    push 4", "    jmp exc_common",
    ".global exc_5", "exc_5:", "    push 0", "    push 5", "    jmp exc_common",
    ".global exc_6", "exc_6:", "    push 0", "    push 6", "    jmp exc_common",
    ".global exc_7", "exc_7:", "    push 0", "    push 7", "    jmp exc_common",
    ".global exc_8", "exc_8:", "    push 8", "    jmp exc_common",
    ".global exc_9", "exc_9:", "    push 0", "    push 9", "    jmp exc_common",
    ".global exc_10", "exc_10:", "    push 10", "    jmp exc_common",
    ".global exc_11", "exc_11:", "    push 11", "    jmp exc_common",
    ".global exc_12", "exc_12:", "    push 12", "    jmp exc_common",
    // #GP on a read or write from msr_read/msr_write is not a crash: the register simply does not exist.
    // Skip the instruction (2 bytes); r9d = 0 tells the caller about it.
    ".global exc_13", "exc_13:",
    "    push rax",
    "    lea rax, [rip + msr_read_at]",
    "    cmp rax, [rsp + 16]",
    "    je 2f",
    "    lea rax, [rip + msr_write_at]",
    "    cmp rax, [rsp + 16]",
    "2:  pop rax",
    "    jne 3f",
    "    add qword ptr [rsp + 8], 2",
    "    add rsp, 8",
    "    xor r9d, r9d",
    "    iretq",
    "3:  push 13",
    "    jmp exc_common",
    ".global exc_14", "exc_14:", "    push 14", "    jmp exc_common",
    ".global exc_15", "exc_15:", "    push 0", "    push 15", "    jmp exc_common",
    ".global exc_16", "exc_16:", "    push 0", "    push 16", "    jmp exc_common",
    ".global exc_17", "exc_17:", "    push 17", "    jmp exc_common",
    ".global exc_18", "exc_18:", "    push 0", "    push 18", "    jmp exc_common",
    ".global exc_19", "exc_19:", "    push 0", "    push 19", "    jmp exc_common",
    ".global exc_20", "exc_20:", "    push 0", "    push 20", "    jmp exc_common",
    ".global exc_21", "exc_21:", "    push 21", "    jmp exc_common",
    ".global exc_22", "exc_22:", "    push 0", "    push 22", "    jmp exc_common",
    ".global exc_23", "exc_23:", "    push 0", "    push 23", "    jmp exc_common",
    ".global exc_24", "exc_24:", "    push 0", "    push 24", "    jmp exc_common",
    ".global exc_25", "exc_25:", "    push 0", "    push 25", "    jmp exc_common",
    ".global exc_26", "exc_26:", "    push 0", "    push 26", "    jmp exc_common",
    ".global exc_27", "exc_27:", "    push 0", "    push 27", "    jmp exc_common",
    ".global exc_28", "exc_28:", "    push 0", "    push 28", "    jmp exc_common",
    ".global exc_29", "exc_29:", "    push 29", "    jmp exc_common",
    ".global exc_30", "exc_30:", "    push 30", "    jmp exc_common",
    ".global exc_31", "exc_31:", "    push 0", "    push 31", "    jmp exc_common",
    "exc_common:",
    "    mov rdi, rsp",
    "    and rsp, -16",
    "    call {handler}",

    ".global msr_read",
    "msr_read:",
    "    mov ecx, edi",
    "    mov r8, rsi",
    "    mov r9d, 1",
    "msr_read_at:",
    "    rdmsr",
    "    shl rdx, 32",
    "    or rax, rdx",
    "    mov [r8], rax",
    "    mov eax, r9d",
    "    ret",

    ".global msr_write",
    "msr_write:",
    "    mov ecx, edi",
    "    mov rax, rsi",
    "    mov rdx, rsi",
    "    shr rdx, 32",
    "    mov r9d, 1",
    "msr_write_at:",
    "    wrmsr",
    "    mov eax, r9d",
    "    ret",

    ".section .rdata",
    ".balign 8",
    ".global exc_table",
    "exc_table:",
    "    .quad exc_0, exc_1, exc_2, exc_3, exc_4, exc_5, exc_6, exc_7, exc_8, exc_9, exc_10, exc_11, exc_12, exc_13, exc_14, exc_15, exc_16, exc_17, exc_18, exc_19, exc_20, exc_21, exc_22, exc_23, exc_24, exc_25, exc_26, exc_27, exc_28, exc_29, exc_30, exc_31",
    ".text",
    ticks = sym TICKS,
    schedule = sym crate::task::schedule_entry,
    switched = sym crate::task::switched,
    help = sym crate::smp::help,
    lapic_eoi = sym LAPIC_EOI,
    handler = sym exception_handler,
    syscall = sym crate::process::syscall_entry,
    ps2 = sym crate::input::irq,
);

unsafe extern "C" {
    fn isr_timer();
    fn isr_yield();
    fn isr_syscall();
    fn isr_ps2();
    fn isr_wake();
    fn isr_resched();
    fn isr_help();
    fn isr_master();
    fn isr_slave();
    /// Addresses of the exception stubs 0..31.
    static exc_table: [u64; 32];
}

const EXCEPTIONS: [&str; 32] = [
    "Divide error", "Debug", "NMI", "Breakpoint", "Overflow", "Bound range", "Invalid opcode", "Device not available",
    "Double fault", "Coprocessor overrun", "Invalid TSS", "Segment not present", "Stack fault", "General protection",
    "Page fault", "Reserved", "x87 error", "Alignment check", "Machine check", "SIMD error", "Virtualization",
    "Control protection", "Reserved", "Reserved", "Reserved", "Reserved", "Reserved", "Reserved", "Hypervisor injection",
    "VMM communication", "Security", "Reserved",
];

/// Frame: [vector, error code, rip, cs, rflags, rsp, ss].
extern "sysv64" fn exception_handler(frame: *const u64) -> ! {
    let f = unsafe { core::slice::from_raw_parts(frame, 7) };
    let (vector, error, rip) = (f[0] as usize, f[1], f[2]);
    let name = EXCEPTIONS[vector % 32];
    if vector == 2 && crate::crash::stopping() {
        // Another core died and is stopping us.
        arch::halt_forever()
    }
    if f[3] & 3 == 3 {
        // A fault in ring 3: only the process dies, the system keeps running.
        crate::process::crashed(name, vector, error, rip);
    }
    let detail = if vector == 14 {
        let addr = crate::paging::fault_address();
        let what = if error & 2 != 0 { "write to" } else { "read from" };
        let null = if addr < 4096 { " (null pointer)" } else { "" };
        alloc::format!("{what} {addr:#x}{null} at rip {rip:#x}")
    } else {
        alloc::format!("error code {error:#x} at rip {rip:#x}, rsp {:#x}", f[5])
    };
    crate::kprintln!("CPU EXCEPTION #{vector} {name}: {detail}");
    crate::crash::fatal(&alloc::format!("exception: {name}, {detail}"));
    crate::gfx::crash_screen(name, &detail);
    crate::audio::crash_alarm();
    arch::halt_forever()
}

#[derive(Clone, Copy, Default)]
#[repr(C, packed)]
struct IdtEntry {
    offset_lo: u16,
    selector: u16,
    ist: u8,
    attr: u8,
    offset_mid: u16,
    offset_hi: u32,
    _zero: u32,
}

impl IdtEntry {
    fn new(handler: usize, selector: u16) -> Self {
        Self {
            offset_lo: handler as u16,
            selector,
            ist: 0,
            attr: 0x8E, // present, DPL 0, 64-bit interrupt gate
            offset_mid: (handler >> 16) as u16,
            offset_hi: (handler >> 32) as u32,
            _zero: 0,
        }
    }
}

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// Local APIC timer vector of the main core (backup alarm, fast timer): only wakes it.
pub const WAKE_VECTOR: u8 = 0xF1;
/// IPI "help with parallel work".
pub const HELP_VECTOR: u8 = 0xF2;
/// IPI "reconsider which thread runs".
pub const RESCHED_VECTOR: u8 = 0xF3;
static IDT_BASE: AtomicU64 = AtomicU64::new(0);

/// Loads the shared IDT (on the BSP at init and on every helper core).
pub fn load_idt() {
    let idtr = Idtr { limit: (size_of::<[IdtEntry; 256]>() - 1) as u16, base: IDT_BASE.load(Ordering::Relaxed) };
    unsafe { asm!("lidt [{}]", in(reg) &idtr, options(readonly, nostack, preserves_flags)) };
}

/// Address of the local APIC registers (0 — none or x2APIC).
pub fn lapic_base() -> u64 {
    LAPIC_EOI.load(Ordering::Relaxed).saturating_sub(0xB0)
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Local APIC timer counts per millisecond; 0 — the timer cannot be used.
static TIMER_PER_MS: AtomicU64 = AtomicU64::new(0);
/// The backup alarm is armed.
static BACKSTOP: AtomicBool = AtomicBool::new(false);
/// How often the backup alarm and the fast timer fire, in milliseconds and fractions.
const BACKSTOP_MS: u64 = 200;
const FAST_WAKES_PER_MS: u64 = 4;
const LVT_TIMER: usize = 0x320;
const TIMER_START: usize = 0x380;
const TIMER_NOW: usize = 0x390;
const TIMER_DIVIDE: usize = 0x3E0;

fn timer_reg(off: usize, val: u32) {
    unsafe { (lapic_base() as *mut u32).byte_add(off).write_volatile(val) }
}

/// Measures the local APIC timer against the regular one: its frequency depends on the CPU.
pub fn calibrate_timer() {
    if lapic_base() == 0 {
        return;
    }
    timer_reg(TIMER_DIVIDE, 0b1010); // divide by 128
    timer_reg(LVT_TIMER, 1 << 16); // no interrupts for now
    let edge = ticks();
    while ticks() == edge {
        core::hint::spin_loop();
    }
    timer_reg(TIMER_START, u32::MAX);
    let start = ticks();
    while ticks() < start + 2 {
        core::hint::spin_loop();
    }
    let now = unsafe { (lapic_base() as *mut u32).byte_add(TIMER_NOW).read_volatile() };
    timer_reg(TIMER_START, 0);
    TIMER_PER_MS.store((u32::MAX - now) as u64 / (2 * 1000 / HZ), Ordering::Relaxed);
}

fn timer_every(count: u64) {
    timer_reg(LVT_TIMER, WAKE_VECTOR as u32 | 1 << 17); // periodic
    timer_reg(TIMER_START, count.clamp(1, u32::MAX as u64) as u32);
}

/// Backup alarm of the main core: the local APIC timer wakes it several times a second,
/// even if the regular timer failed to wake it from deep sleep. false — no APIC.
pub fn arm_backstop() -> bool {
    let per_ms = TIMER_PER_MS.load(Ordering::Relaxed);
    if per_ms == 0 {
        return false;
    }
    BACKSTOP.store(true, Ordering::Relaxed);
    timer_every(per_ms * BACKSTOP_MS);
    true
}

/// Who needs the fast timer: network traffic, and the touchpad while fingers are on it.
pub const FAST_FOR_NET: u8 = 1;
pub const FAST_FOR_TOUCHPAD: u8 = 2;
/// The Wi-Fi thread (rtl8723be) after talking to us: poll the card more often than a tick.
pub const FAST_FOR_WIFI: u8 = 4;
static FAST_WANTED: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Fast timer for the duration of network traffic: the thread checks the card between short pauses
/// instead of spinning on the CPU. Returns whether it runs.
pub fn fast_wakeups(on: bool) -> bool {
    fast_wakeups_for(FAST_FOR_NET, on)
}

/// Fast timer for `who`: it runs while anyone needs it. The timer belongs to the main
/// core (its local APIC); from another CPU the request is passed to it by an interrupt.
pub fn fast_wakeups_for(who: u8, on: bool) -> bool {
    if TIMER_PER_MS.load(Ordering::Relaxed) == 0 {
        return false;
    }
    if on {
        FAST_WANTED.fetch_or(who, Ordering::Relaxed);
    } else {
        FAST_WANTED.fetch_and(!who, Ordering::Relaxed);
    }
    if crate::gdt::cpu() == 0 {
        apply_fast();
    } else if (FAST_WANTED.load(Ordering::Relaxed) != 0) != FAST.load(Ordering::Relaxed) {
        crate::smp::reschedule(0);
    }
    on
}

/// Switches the main core's timer to fast mode or back, as requested. Only on CPU 0.
pub fn apply_fast() {
    let per_ms = TIMER_PER_MS.load(Ordering::Relaxed);
    if per_ms == 0 {
        return;
    }
    crate::sync::without_interrupts(|| {
        let fast = FAST_WANTED.load(Ordering::Relaxed) != 0;
        if FAST.swap(fast, Ordering::Relaxed) != fast {
            if fast {
                timer_every((per_ms / FAST_WAKES_PER_MS).max(1));
            } else if BACKSTOP.load(Ordering::Relaxed) {
                timer_every(per_ms * BACKSTOP_MS);
            } else {
                timer_reg(TIMER_START, 0);
            }
        }
    });
}

/// The fast timer is running now.
static FAST: AtomicBool = AtomicBool::new(false);

pub fn fast() -> bool {
    FAST.load(Ordering::Relaxed)
}

pub fn init() {
    let cs: u16;
    unsafe { asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags)) };

    let idt = Box::leak(Box::new([IdtEntry::default(); 256]));
    for (vector, entry) in idt.iter_mut().enumerate() {
        let handler = match vector {
            0..0x20 => (unsafe { exc_table[vector] }) as usize,
            0x20 => isr_timer as *const () as usize,
            0x81 => isr_yield as *const () as usize,
            0x21 | 0x2C => isr_ps2 as *const () as usize,
            0x28..0x30 => isr_slave as *const () as usize,
            _ => isr_master as *const () as usize,
        };
        *entry = IdtEntry::new(handler, cs);
    }
    idt[WAKE_VECTOR as usize] = IdtEntry::new(isr_wake as *const () as usize, cs);
    idt[HELP_VECTOR as usize] = IdtEntry::new(isr_help as *const () as usize, cs);
    idt[RESCHED_VECTOR as usize] = IdtEntry::new(isr_resched as *const () as usize, cs);
    for (vector, stub) in crate::irq::stubs() {
        idt[vector as usize] = IdtEntry::new(stub, cs);
    }
    // System calls are allowed from ring 3 (DPL 3).
    idt[0x80] = IdtEntry::new(isr_syscall as *const () as usize, cs);
    idt[0x80].attr = 0xEE;
    IDT_BASE.store(idt.as_ptr() as u64, Ordering::Relaxed);
    load_idt();

    setup_lapic();
    remap_pic();
    program_pit();
}

/// Silence the LAPIC timer (UEFI may have enabled it) and turn on virtual wire mode,
/// so that PIC interrupts reach the CPU through LINT0.
fn setup_lapic() {
    let base = rdmsr(0x1B);
    let enabled = base & (1 << 11) != 0;
    let x2apic = base & (1 << 10) != 0;
    if !enabled || x2apic {
        return;
    }
    let mmio = (base & 0xF_FFFF_F000) as *mut u32;
    let reg = |off: usize, val: u32| unsafe { mmio.byte_add(off).write_volatile(val) };
    reg(0x080, 0); // TPR: accept everything
    reg(0x0F0, 0x1FF); // SVR: APIC enabled, spurious vector 0xFF
    reg(0x320, 1 << 16); // LVT timer: masked
    reg(0x380, 0); // initial count = 0
    reg(0x350, 0x700); // LINT0: ExtINT
    reg(0x360, 0x400); // LINT1: NMI
    LAPIC_EOI.store(mmio as u64 + 0xB0, Ordering::Relaxed);
    BOOT_APIC_ID.store(unsafe { mmio.byte_add(0x20).read_volatile() } >> 24, Ordering::Relaxed);
}

fn remap_pic() {
    let w = |port, val| {
        outb(port, val);
        outb(0x80, 0); // a short delay
    };
    w(0x20, 0x11);
    w(0xA0, 0x11);
    w(0x21, 0x20); // master -> vectors 0x20..0x27
    w(0xA1, 0x28); // slave  -> vectors 0x28..0x2F
    w(0x21, 0x04);
    w(0xA1, 0x02);
    w(0x21, 0x01);
    w(0xA1, 0x01);
    // Enabled: IRQ0 (timer), IRQ1 (keyboard), IRQ2 (cascade), IRQ12 (mouse).
    w(0x21, !0b0000_0111);
    w(0xA1, !0b0001_0000);
}

fn program_pit() {
    let divisor = (1_193_182 / HZ) as u16;
    outb(0x43, 0x34); // channel 0, lo/hi, mode 2
    outb(0x40, divisor as u8);
    outb(0x40, (divisor >> 8) as u8);
}

/// Enables interrupts and checks that the timer really ticks.
pub fn enable_and_check() -> bool {
    unsafe { asm!("sti", options(nomem, nostack)) };
    let start = arch::rdtsc();
    while arch::rdtsc() - start < 200_000_000 {
        if ticks() > 2 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}
