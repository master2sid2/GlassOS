//! What the CPU can do: CPUID is queried once at startup, after that code picks
//! the fast path (AES-NI, RDRAND/RDSEED) only where the instruction exists.
//! The kernel is built for x86-64-v2, so SSE2…SSE4.2 and POPCNT are always there; this covers what
//! may be missing.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::sync::atomic::{AtomicU32, Ordering};

pub const AES: u32 = 1 << 0;
pub const PCLMUL: u32 = 1 << 1;
pub const RDRAND: u32 = 1 << 2;
pub const RDSEED: u32 = 1 << 3;
pub const SHA: u32 = 1 << 4;
pub const AVX: u32 = 1 << 5;
pub const AVX2: u32 = 1 << 6;
pub const SSE42: u32 = 1 << 7;
const KNOWN: u32 = 1 << 31;

static FLAGS: AtomicU32 = AtomicU32::new(0);

fn flags() -> u32 {
    let f = FLAGS.load(Ordering::Relaxed);
    if f & KNOWN != 0 {
        return f;
    }
    let one = __cpuid(1);
    let top = __cpuid(0).eax;
    let seven = if top >= 7 { __cpuid_count(7, 0) } else { __cpuid(0) };
    let seven_ok = top >= 7;
    let bit = |reg: u32, n: u32, flag: u32| if reg & (1 << n) != 0 { flag } else { 0 };
    let mut f = KNOWN
        | bit(one.ecx, 25, AES)
        | bit(one.ecx, 1, PCLMUL)
        | bit(one.ecx, 30, RDRAND)
        | bit(one.ecx, 28, AVX)
        | bit(one.ecx, 20, SSE42);
    if seven_ok {
        f |= bit(seven.ebx, 18, RDSEED) | bit(seven.ebx, 29, SHA) | bit(seven.ebx, 5, AVX2);
    }
    FLAGS.store(f, Ordering::Relaxed);
    f
}

/// Whether the CPU has an instruction (the flags above).
pub fn has(flag: u32) -> bool {
    flags() & flag != 0
}

/// Disable a fast path if the self-check found a mismatch.
pub fn disable(flag: u32) {
    flags();
    FLAGS.fetch_and(!flag, Ordering::Relaxed);
}

/// The CPU name from CPUID (as the vendor calls it).
pub fn brand() -> String {
    if __cpuid(0x8000_0000).eax < 0x8000_0004 {
        return "unknown processor".into();
    }
    let mut bytes = Vec::with_capacity(48);
    for leaf in 0x8000_0002..=0x8000_0004u32 {
        let r = __cpuid(leaf);
        for word in [r.eax, r.ebx, r.ecx, r.edx] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    text.trim_matches(|c: char| c == '\0' || c.is_whitespace()).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Lines for the `cpu` command and the log.
pub fn report() -> Vec<String> {
    let used = |flag: u32, name: &str, what: &str| {
        if has(flag) {
            format!("  {name:<8} yes — {what}")
        } else {
            format!("  {name:<8} no")
        }
    };
    let idle = |flag: u32, name: &str, why: &str| {
        if has(flag) {
            format!("  {name:<8} yes — not used yet: {why}")
        } else {
            format!("  {name:<8} no")
        }
    };
    let mut lines = alloc::vec![
        format!("{} ({} cores in use)", brand(), crate::smp::cpu_count()),
        "  SSE4.2   yes — the kernel is built for it (graphics, sound, everything)".into(),
        used(AES, "AES-NI", "AES for Bluetooth pairing and Wi-Fi keys"),
        used(RDRAND, "RDRAND", "random numbers (keys, salts, nonces)"),
        used(RDSEED, "RDSEED", "seeds for random numbers"),
        idle(PCLMUL, "PCLMUL", "checksums use 8-byte tables, fast enough"),
        idle(SHA, "SHA-NI", "SHA-256 in software is fast enough so far"),
        idle(AVX2, "AVX2", "needs XSAVE for the wider registers"),
    ];
    lines.extend(load());
    lines
}

/// The load of each core since the previous call and who is on it now.
fn load() -> Vec<String> {
    use crate::gdt::MAX_CPUS;
    static LAST: crate::sync::IrqLock<(u64, u64, [u64; MAX_CPUS], u64)> = crate::sync::IrqLock::new((0, 0, [0; MAX_CPUS], 0));
    let now = crate::arch::rdtsc();
    let tick = crate::interrupts::ticks();
    let idle0 = crate::stats::totals().0;
    let busy: [u64; MAX_CPUS] = core::array::from_fn(crate::task::busy_cycles);
    let mut last = LAST.lock();
    let span = now - last.0;
    let mut out = alloc::vec![if last.0 == 0 { "load since startup:".into() } else { format!("load over the last {:.1} s:", (tick - last.3) as f64 / crate::interrupts::HZ as f64) }];
    for c in 0..crate::smp::cpu_count() {
        let mut work = busy[c] - last.2[c];
        if c == 0 {
            // The main thread's idle time runs inside it.
            work = work.saturating_sub(idle0 - last.1);
        }
        let pct = if span == 0 { 0 } else { (work * 100 / span).min(100) };
        let who = crate::task::running_on(c).unwrap_or_default();
        out.push(format!("  core {c}: {pct:>3}% busy, now running: {who}"));
    }
    *last = (now, idle0, busy, tick);
    out
}

/// 64 random bits from the CPU: RDSEED (true noise), otherwise RDRAND.
/// The instruction may not gather enough entropy in time and say "no" — we try several times.
pub fn random_u64() -> Option<u64> {
    if has(RDSEED) {
        for _ in 0..32 {
            if let Some(v) = unsafe { rdseed() } {
                return Some(v);
            }
            core::hint::spin_loop();
        }
    }
    if has(RDRAND) {
        for _ in 0..10 {
            if let Some(v) = unsafe { rdrand() } {
                return Some(v);
            }
        }
    }
    None
}

#[target_feature(enable = "rdseed")]
unsafe fn rdseed() -> Option<u64> {
    let mut v = 0u64;
    (core::arch::x86_64::_rdseed64_step(&mut v) == 1).then_some(v)
}

#[target_feature(enable = "rdrand")]
unsafe fn rdrand() -> Option<u64> {
    let mut v = 0u64;
    (core::arch::x86_64::_rdrand64_step(&mut v) == 1).then_some(v)
}
