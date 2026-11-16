//! A lock that disables interrupts while it is held.
//!
//! Threads are preempted and run on several CPUs. If a thread is preempted while it
//! holds a plain spinlock, another thread will spin on it forever. So while the lock
//! is held, interrupts (and thread switches) are disabled on this CPU, and other CPUs
//! wait on an atomic flag.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// How many cycles to wait for the lock before reporting a likely deadlock
/// (about 2 s on the Brix). The watchdog only resets the machine after 30–60 s.
const STUCK_CYCLES: u64 = 3_000_000_000;
/// How many IrqLocks each CPU holds, and where the outermost one (which disabled interrupts) was taken.
static DEPTH: [AtomicUsize; crate::gdt::MAX_CPUS] = [const { AtomicUsize::new(0) }; crate::gdt::MAX_CPUS];
static OUTER: [AtomicUsize; crate::gdt::MAX_CPUS] = [const { AtomicUsize::new(0) }; crate::gdt::MAX_CPUS];
/// When (cycles) the outermost IrqLock was taken: how long the CPU has run without interrupts.
static OUTER_AT: [AtomicU64; crate::gdt::MAX_CPUS] = [const { AtomicU64::new(0) }; crate::gdt::MAX_CPUS];
/// Longer than this (≈ 10 ms, one frame) without interrupts or waiting for an IrqLock goes to the
/// log: the timer, cursor and input on this core are stalled all that time, and a person sees a "hiccup".
const LONG_CYCLES: u64 = 20_000_000;
/// A long IrqLock wait not reported yet: where we waited, where it was held, on which
/// CPU, how many cycles. Printed once the CPU releases all IrqLocks: printing to the
/// log is slow (serial port), and under the lock it would itself make other cores
/// wait — they would report themselves, and so on in a loop.
static WAIT_AT: [AtomicUsize; crate::gdt::MAX_CPUS] = [const { AtomicUsize::new(0) }; crate::gdt::MAX_CPUS];
static WAIT_HOLDER: [AtomicUsize; crate::gdt::MAX_CPUS] = [const { AtomicUsize::new(0) }; crate::gdt::MAX_CPUS];
static WAIT_OWNER: [AtomicUsize; crate::gdt::MAX_CPUS] = [const { AtomicUsize::new(0) }; crate::gdt::MAX_CPUS];
static WAIT_CYCLES: [AtomicU64; crate::gdt::MAX_CPUS] = [const { AtomicU64::new(0) }; crate::gdt::MAX_CPUS];
/// This CPU is printing such a message now (its own locks are not counted).
static REPORTING: [AtomicBool; crate::gdt::MAX_CPUS] = [const { AtomicBool::new(false) }; crate::gdt::MAX_CPUS];

/// The CPU ran a long time without interrupts under an IrqLock: where it was taken and how long it was held.
#[cold]
#[inline(never)]
fn long_hold(cpu: usize, outer: usize, cycles: u64) {
    if outer == 0 || REPORTING[cpu].swap(true, Ordering::Relaxed) {
        return;
    }
    let at = unsafe { &*(outer as *const Location) };
    crate::kprintln!("lock: CPU {cpu} ran {} Mcycles with interrupts off under the lock taken at {}:{} (thread {})", cycles / 1_000_000, at.file(), at.line(), crate::task::current_id());
    REPORTING[cpu].store(false, Ordering::Relaxed);
}

/// The lock was taken: where and on which CPU; for the outermost one, also when.
#[inline(never)]
fn taken(at: &'static Location<'static>, holder: &AtomicUsize, owner: &AtomicUsize) {
    let at = at as *const Location as usize;
    let cpu = crate::gdt::cpu();
    holder.store(at, Ordering::Relaxed);
    owner.store(cpu + 1, Ordering::Relaxed);
    if let Some(depth) = DEPTH.get(cpu)
        && depth.fetch_add(1, Ordering::Relaxed) == 0
    {
        OUTER[cpu].store(at, Ordering::Relaxed);
        OUTER_AT[cpu].store(crate::arch::rdtsc(), Ordering::Relaxed);
    }
}

/// Release an IrqLock (not generic — one for all types): depth, flag, interrupts and
/// the measurement of how long the CPU ran without interrupts.
#[inline(never)]
fn release(locked: &AtomicBool, interrupts: bool) {
    let cpu = crate::gdt::cpu();
    let mut long = None;
    let mut outermost = false;
    if let Some(depth) = DEPTH.get(cpu)
        && depth.try_update(Ordering::Relaxed, Ordering::Relaxed, |d| d.checked_sub(1)) == Ok(1)
    {
        outermost = true;
        let held = crate::arch::rdtsc().wrapping_sub(OUTER_AT[cpu].load(Ordering::Relaxed));
        if held > LONG_CYCLES && held < 1 << 40 {
            long = Some((OUTER[cpu].load(Ordering::Relaxed), held));
        }
    }
    locked.store(false, Ordering::Release);
    restore(interrupts);
    if let Some((outer, held)) = long {
        long_hold(cpu, outer, held);
    }
    if outermost && WAIT_CYCLES[cpu].load(Ordering::Relaxed) != 0 {
        let cycles = WAIT_CYCLES[cpu].swap(0, Ordering::Relaxed);
        let at = WAIT_AT[cpu].load(Ordering::Relaxed);
        if at != 0 {
            long_wait(unsafe { &*(at as *const Location) }, WAIT_HOLDER[cpu].load(Ordering::Relaxed), WAIT_OWNER[cpu].load(Ordering::Relaxed), cycles);
        }
    }
}

/// Waited a long time for an IrqLock (with interrupts disabled): who waited and where it was held.
#[cold]
#[inline(never)]
fn long_wait(waiter: &Location, holder: usize, owner: usize, cycles: u64) {
    let cpu = crate::gdt::cpu();
    if REPORTING.get(cpu).is_none_or(|r| r.swap(true, Ordering::Relaxed)) {
        return;
    }
    let (file, line) = if holder != 0 {
        let at = unsafe { &*(holder as *const Location) };
        (at.file(), at.line())
    } else {
        ("?", 0)
    };
    crate::kprintln!("lock: CPU {cpu} waited {} Mcycles at {}:{} for the lock taken at {file}:{line} on CPU {}", cycles / 1_000_000, waiter.file(), waiter.line(), owner.wrapping_sub(1));
    REPORTING[cpu].store(false, Ordering::Relaxed);
}

/// Places already reported (so as not to flood the log).
static TOLD: [AtomicUsize; 16] = [const { AtomicUsize::new(0) }; 16];

/// The thread yields the CPU while interrupts are disabled: so it holds an IrqLock (or
/// this is a handler). Another core waiting for the same lock will stall together with the timer.
/// Reported once per place.
///
/// true — the thread holds an IrqLock and must not yield. The thread this lock waits for
/// may be on this same core, and with interrupts disabled the core would spin on it
/// forever (this used to hang: the main thread waited for the GPU while holding the player window's frame).
/// In that case `yield_now` just waits in place.
pub fn check_yield() -> bool {
    let flags: u64;
    unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    if flags & (1 << 9) != 0 {
        return false;
    }
    let cpu = crate::gdt::cpu();
    let held = DEPTH.get(cpu).map_or(0, |d| d.load(Ordering::Relaxed));
    let at = if held > 0 { OUTER.get(cpu).map_or(0, |l| l.load(Ordering::Relaxed)) } else { 1 };
    if TOLD.iter().any(|t| t.load(Ordering::Relaxed) == at) {
        return held > 0;
    }
    if TOLD.iter().any(|t| t.compare_exchange(0, at, Ordering::Relaxed, Ordering::Relaxed).is_ok()) {
        if at == 1 {
            crate::kprintln!("lock: a thread waits with interrupts off, holding no IrqLock (interrupt handler or plain cli)");
        } else {
            let loc = unsafe { &*(at as *const Location) };
            crate::kprintln!("lock: a thread waits with interrupts off, holding {} IrqLock(s); the outer one was taken at {}:{} (it now waits in place)", held, loc.file(), loc.line());
        }
    }
    held > 0
}

/// How many CPUs are stuck on an IrqLock right now (waiting longer than `STUCK_CYCLES` with
/// interrupts disabled). While this is not zero, the watchdog is not kicked: the system
/// is broken, let the board reset, and the report is shown after login.
static STUCK_NOW: AtomicUsize = AtomicUsize::new(0);

#[cold]
#[inline(never)]
fn stuck_on_irq_lock(at: &'static Location<'static>, holder: usize, owner: usize) {
    STUCK_NOW.fetch_add(1, Ordering::Relaxed);
    stuck("a lock", at, holder, owner);
}

/// A stuck wait did end after all (not generic: `lock` is instantiated for each
/// type, and formatting inside it would bloat the kernel).
#[cold]
#[inline(never)]
fn unstuck(at: &Location) {
    STUCK_NOW.fetch_sub(1, Ordering::Relaxed);
    crate::kprintln!("lock: the stuck wait at {}:{} ended after all", at.file(), at.line());
}

pub fn someone_stuck() -> bool {
    STUCK_NOW.load(Ordering::Relaxed) > 0
}

/// The note in firmware memory is written once per boot.
static NOTED: AtomicBool = AtomicBool::new(false);

/// A panic or exception: its note takes priority, the hang detector will not overwrite it
/// (after a panic other cores may still get stuck on abandoned locks).
pub fn claim_note() {
    NOTED.store(true, Ordering::Relaxed);
}

/// Waiting for a lock for too long: who waits, where, and where it was taken. No heap: the heap
/// itself may be among the stuck ones.
#[cold]
pub fn stuck(kind: &str, waiter: &Location, holder: usize, owner: usize) {
    use core::fmt::Write;
    struct Buf([u8; 400], usize);
    impl core::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &b in s.as_bytes() {
                if self.1 < self.0.len() {
                    self.0[self.1] = b;
                    self.1 += 1;
                }
            }
            Ok(())
        }
    }
    let mut b = Buf([0; 400], 0);
    let _ = write!(b, "stuck: CPU {} waits for {kind} at {}:{}", crate::gdt::cpu(), waiter.file(), waiter.line());
    if holder != 0 {
        let at = unsafe { &*(holder as *const Location) };
        let _ = write!(b, "; it was taken at {}:{} on CPU {}", at.file(), at.line(), owner.wrapping_sub(1));
    }
    let _ = write!(b, "; uptime {} s", crate::interrupts::ticks() / crate::interrupts::HZ);
    let text = core::str::from_utf8(&b.0[..b.1]).unwrap_or("stuck");
    crate::kprintln!("{text}");
    if !NOTED.swap(true, Ordering::Relaxed) {
        crate::bootvars::save_note(text.as_bytes());
    }
}

/// Disables interrupts; returns whether they were enabled.
fn disable() -> bool {
    let flags: u64;
    unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem, preserves_flags)) };
    flags & (1 << 9) != 0
}

fn restore(enabled: bool) {
    if enabled {
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
}

/// Runs `f` with interrupts disabled.
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let enabled = disable();
    let r = f();
    restore(enabled);
    r
}

pub struct IrqLock<T> {
    locked: AtomicBool,
    /// Where it was taken (`&'static Location`) and on which CPU (+1): for the stuck report.
    holder: AtomicUsize,
    owner: AtomicUsize,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for IrqLock<T> {}
unsafe impl<T: Send> Send for IrqLock<T> {}

pub struct IrqGuard<'a, T> {
    lock: &'a IrqLock<T>,
    interrupts: bool,
}

impl<T> IrqLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), holder: AtomicUsize::new(0), owner: AtomicUsize::new(0), data: UnsafeCell::new(value) }
    }

    #[track_caller]
    pub fn lock(&self) -> IrqGuard<'_, T> {
        let interrupts = disable();
        if self.locked.swap(true, Ordering::Acquire) {
            let start = crate::arch::rdtsc();
            let (holder, owner) = (self.holder.load(Ordering::Relaxed), self.owner.load(Ordering::Relaxed));
            let mut told = false;
            while self.locked.swap(true, Ordering::Acquire) {
                core::hint::spin_loop();
                if !told && crate::arch::rdtsc() - start > STUCK_CYCLES {
                    told = true;
                    stuck_on_irq_lock(Location::caller(), self.holder.load(Ordering::Relaxed), self.owner.load(Ordering::Relaxed));
                }
            }
            if told {
                unstuck(Location::caller());
            }
            let waited = crate::arch::rdtsc() - start;
            if waited > LONG_CYCLES {
                let cpu = crate::gdt::cpu();
                if cpu < crate::gdt::MAX_CPUS && WAIT_CYCLES[cpu].load(Ordering::Relaxed) == 0 {
                    WAIT_AT[cpu].store(Location::caller() as *const Location as usize, Ordering::Relaxed);
                    WAIT_HOLDER[cpu].store(holder, Ordering::Relaxed);
                    WAIT_OWNER[cpu].store(owner, Ordering::Relaxed);
                    WAIT_CYCLES[cpu].store(waited, Ordering::Relaxed);
                }
            }
        }
        self.mark();
        IrqGuard { lock: self, interrupts }
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<IrqGuard<'_, T>> {
        let interrupts = disable();
        if self.locked.swap(true, Ordering::Acquire) {
            restore(interrupts);
            return None;
        }
        self.mark();
        Some(IrqGuard { lock: self, interrupts })
    }

    #[track_caller]
    fn mark(&self) {
        taken(Location::caller(), &self.holder, &self.owner);
    }
}

impl<T> Deref for IrqGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqGuard<'_, T> {
    fn drop(&mut self) {
        release(&self.lock.locked, self.interrupts);
    }
}

/// A lock for long operations (for example, disk writes): does not disable
/// interrupts, and while it is busy the waiter yields the CPU. It must not be taken
/// from an interrupt handler.
pub struct YieldLock<T> {
    locked: AtomicBool,
    holder: AtomicUsize,
    owner: AtomicUsize,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for YieldLock<T> {}
unsafe impl<T: Send> Send for YieldLock<T> {}

pub struct YieldGuard<'a, T> {
    lock: &'a YieldLock<T>,
}

impl<T> YieldLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), holder: AtomicUsize::new(0), owner: AtomicUsize::new(0), data: UnsafeCell::new(value) }
    }

    #[track_caller]
    pub fn lock(&self) -> YieldGuard<'_, T> {
        if self.locked.swap(true, Ordering::Acquire) {
            // The desktop is waiting for a long lock: log where, to find the culprit.
            let since = crate::interrupts::ticks();
            let mut told = false;
            while self.locked.swap(true, Ordering::Acquire) {
                crate::task::yield_now();
                if !told && crate::interrupts::ticks() - since > 10 * crate::interrupts::HZ {
                    told = true;
                    stuck("a long lock (10 s)", core::panic::Location::caller(), self.holder.load(Ordering::Relaxed), self.owner.load(Ordering::Relaxed));
                }
            }
            let waited = crate::interrupts::ticks() - since;
            if waited >= crate::interrupts::HZ / 10 && crate::task::current_id() == 0 {
                let at = core::panic::Location::caller();
                crate::kprintln!("lock: desktop waited {} ms at {}:{}", waited * 1000 / crate::interrupts::HZ, at.file(), at.line());
            }
        }
        self.holder.store(core::panic::Location::caller() as *const Location as usize, Ordering::Relaxed);
        self.owner.store(crate::gdt::cpu() + 1, Ordering::Relaxed);
        YieldGuard { lock: self }
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<YieldGuard<'_, T>> {
        // Only `then`: then_some would create a guard even when the lock is busy,
        // and dropping it would release someone else's lock.
        let at = core::panic::Location::caller() as *const Location as usize;
        (!self.locked.swap(true, Ordering::Acquire)).then(|| {
            self.holder.store(at, Ordering::Relaxed);
            self.owner.store(crate::gdt::cpu() + 1, Ordering::Relaxed);
            YieldGuard { lock: self }
        })
    }
}

impl<T> Deref for YieldGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for YieldGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for YieldGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// A queue of waiting threads: a thread sleeps until it is woken (`notify`) instead of
/// checking "is it ready?" every tick. Waking is allowed from an interrupt handler: there
/// is no memory allocation here.
pub struct WaitQueue {
    /// Ids of waiting threads (+1; 0 — the slot is free).
    slots: [AtomicU64; WAITERS],
}

const WAITERS: usize = 16;

impl WaitQueue {
    pub const fn new() -> Self {
        Self { slots: [const { AtomicU64::new(0) }; WAITERS] }
    }

    /// Waits until `ready()` becomes true or tick `deadline` arrives. Returns
    /// the last answer of `ready()`. Must not wait while holding an IrqLock.
    pub fn wait_until(&self, deadline: u64, mut ready: impl FnMut() -> bool) -> bool {
        let me = crate::task::current_id() + 1;
        loop {
            if ready() {
                return true;
            }
            // Join the queue first, then check again: an event that happens
            // between the check and the sleep will wake us.
            let slot = self.slots.iter().find(|s| s.load(Ordering::Relaxed) == me).or_else(|| self.slots.iter().find(|s| s.compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed).is_ok()));
            if ready() {
                self.leave(me);
                return true;
            }
            let now = crate::interrupts::ticks();
            if now >= deadline || crate::task::should_stop() {
                self.leave(me);
                return ready();
            }
            // No free slots — wait tick by tick, as before.
            crate::task::park(if slot.is_some() { deadline } else { now + 1 });
        }
    }

    fn leave(&self, me: u64) {
        for s in &self.slots {
            let _ = s.compare_exchange(me, 0, Ordering::AcqRel, Ordering::Relaxed);
        }
    }

    /// Wakes all waiters.
    pub fn notify(&self) {
        for s in &self.slots {
            if s.load(Ordering::Relaxed) != 0 {
                let id = s.swap(0, Ordering::AcqRel);
                if id != 0 {
                    crate::task::wake(id - 1);
                }
            }
        }
    }
}

/// A lock for long work (network stack, parsing a batch of frames): does not disable
/// interrupts, and waiters sleep in a queue until it is released. The owner can be preempted:
/// meanwhile the core does other work, the timer and input keep going. It is not taken from an
/// interrupt handler or under an IrqLock (sleeping is not allowed there — then we spin, like IrqLock).
pub struct Mutex<T> {
    locked: AtomicBool,
    waiters: WaitQueue,
    holder: AtomicUsize,
    owner: AtomicUsize,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

pub struct MutexGuard<'a, T> {
    lock: &'a Mutex<T>,
}

/// Interrupts are enabled right now (otherwise sleeping is not allowed).
pub fn interrupts_on() -> bool {
    let flags: u64;
    unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags & (1 << 9) != 0
}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), waiters: WaitQueue::new(), holder: AtomicUsize::new(0), owner: AtomicUsize::new(0), data: UnsafeCell::new(value) }
    }

    fn grab(&self) -> bool {
        !self.locked.swap(true, Ordering::Acquire)
    }

    #[track_caller]
    pub fn lock(&self) -> MutexGuard<'_, T> {
        if !self.grab() {
            self.wait(Location::caller());
        }
        self.holder.store(Location::caller() as *const Location as usize, Ordering::Relaxed);
        self.owner.store(crate::gdt::cpu() + 1, Ordering::Relaxed);
        MutexGuard { lock: self }
    }

    #[cold]
    #[inline(never)]
    fn wait(&self, at: &'static Location<'static>) {
        let start = crate::arch::rdtsc();
        let since = crate::interrupts::ticks();
        if interrupts_on() && crate::interrupts::ticks() > 0 {
            let mut told = false;
            loop {
                let deadline = crate::interrupts::ticks() + 10 * crate::interrupts::HZ;
                if self.waiters.wait_until(deadline, || self.grab()) {
                    break;
                }
                if !told {
                    told = true;
                    stuck("a sleeping lock (10 s)", at, self.holder.load(Ordering::Relaxed), self.owner.load(Ordering::Relaxed));
                }
            }
        } else {
            let mut told = false;
            while !self.grab() {
                core::hint::spin_loop();
                if !told && crate::arch::rdtsc() - start > STUCK_CYCLES {
                    told = true;
                    stuck("a sleeping lock with interrupts off", at, self.holder.load(Ordering::Relaxed), self.owner.load(Ordering::Relaxed));
                }
            }
        }
        // The desktop waited a noticeable time: log where, to find the culprit.
        let waited = crate::interrupts::ticks() - since;
        if waited >= crate::interrupts::HZ / 10 && crate::task::current_id() == 0 {
            crate::kprintln!("lock: desktop waited {} ms at {}:{}", waited * 1000 / crate::interrupts::HZ, at.file(), at.line());
        }
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let at = Location::caller() as *const Location as usize;
        self.grab().then(|| {
            self.holder.store(at, Ordering::Relaxed);
            self.owner.store(crate::gdt::cpu() + 1, Ordering::Relaxed);
            MutexGuard { lock: self }
        })
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        self.lock.waiters.notify();
    }
}
