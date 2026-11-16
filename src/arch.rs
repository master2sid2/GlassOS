//! Low-level x86_64 primitives: I/O ports, MSRs, halting the CPU.
//!
//! The functions are safe for brevity: the kernel owns all the hardware.

use core::arch::asm;

#[inline]
pub fn outb(port: u16, val: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags)) }
}

#[inline]
pub fn inb(port: u16) -> u8 {
    let val: u8;
    unsafe { asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags)) }
    val
}

#[inline]
pub fn outw(port: u16, val: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") val, options(nomem, nostack, preserves_flags)) }
}

#[inline]
pub fn inw(port: u16) -> u16 {
    let val: u16;
    unsafe { asm!("in ax, dx", out("ax") val, in("dx") port, options(nomem, nostack, preserves_flags)) }
    val
}

#[inline]
pub fn outl(port: u16, val: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") val, options(nomem, nostack, preserves_flags)) }
}

#[inline]
pub fn inl(port: u16) -> u32 {
    let val: u32;
    unsafe { asm!("in eax, dx", out("eax") val, in("dx") port, options(nomem, nostack, preserves_flags)) }
    val
}

/// Reads `buf.len()` 16-bit words from a port (`rep insw`).
pub fn insw(port: u16, buf: &mut [u16]) {
    unsafe {
        asm!("rep insw", in("dx") port, inout("rdi") buf.as_mut_ptr() => _, inout("rcx") buf.len() => _,
             options(nostack, preserves_flags))
    }
}

/// Writes `buf.len()` 16-bit words to a port (`rep outsw`).
pub fn outsw(port: u16, buf: &[u16]) {
    unsafe {
        asm!("rep outsw", in("dx") port, inout("rsi") buf.as_ptr() => _, inout("rcx") buf.len() => _,
             options(nostack, preserves_flags, readonly))
    }
}

pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) }
    (hi as u64) << 32 | lo as u64
}

pub fn wrmsr(msr: u32, value: u64) {
    unsafe { asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack, preserves_flags)) }
}

unsafe extern "sysv64" {
    fn msr_read(msr: u32, value: *mut u64) -> u32;
    fn msr_write(msr: u32, value: u64) -> u32;
}

/// An `rdmsr` that does not crash the system: if the register does not exist — None (see the #GP handler).
pub fn read_msr(msr: u32) -> Option<u64> {
    let mut value = 0;
    (unsafe { msr_read(msr, &mut value) } != 0).then_some(value)
}

/// A `wrmsr` that does not crash the system: if the CPU refused — false.
pub fn write_msr(msr: u32, value: u64) -> bool {
    unsafe { msr_write(msr, value) != 0 }
}

pub fn sfence() {
    unsafe { asm!("sfence", options(nostack, preserves_flags)) }
}

pub fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

pub fn cli() {
    unsafe { asm!("cli", options(nomem, nostack)) }
}

/// Sleeps until the next interrupt. `sti; hlt` are atomic: an interrupt is not lost between them.
pub fn wait_for_interrupt() {
    unsafe { asm!("sti; hlt", options(nomem, nostack)) }
}

/// The same, but with a hint how deeply to sleep. `monitor` watches a line
/// nobody writes to: only an interrupt wakes it.
pub fn mwait(hint: u32) {
    static LINE: u64 = 0;
    unsafe {
        asm!("monitor", in("rax") &raw const LINE, in("ecx") 0, in("edx") 0, options(nomem, nostack, preserves_flags));
        asm!("sti; mwait", in("eax") hint, in("ecx") 0, options(nomem, nostack));
    }
}

pub fn halt_forever() -> ! {
    loop {
        unsafe { asm!("cli; hlt", options(nomem, nostack)) }
    }
}
