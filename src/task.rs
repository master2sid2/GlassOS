//! Kernel threads and a preemptive (round-robin) scheduler on all CPUs.
//!
//! Thread 0 is the main one: WM, input, drawing; it always runs on CPU 0, where device
//! interrupts arrive. The others start through `spawn`, each with its own stack, and
//! run on any free CPU. Switching happens in the timer handler, on
//! `yield_now` and on the inter-processor "reschedule" interrupt: the stub saves the
//! registers and the FPU/SSE state on the thread's stack, `schedule` remembers the stack pointer and
//! returns the next thread's stack.
//!
//! There is one queue for everyone (under `SCHED`). Each CPU has its own current thread; the
//! helpers also have their own idle thread (their startup stack), which sleeps until an interrupt.
//! A thread a CPU is leaving is not taken by another CPU until the stub
//! has switched to the new stack (`LEAVING`, cleared by `switched`).
//!
//! A thread may be a user process: then it has its own address
//! space (CR3) and runs in ring 3, and its kernel stack is only needed for
//! interrupts and system calls.

use crate::access::Cred;
use crate::arch::rdtsc;
use crate::gdt::{MAX_CPUS, cpu};
use crate::paging::{self, AddressSpace};
use crate::sync::IrqLock;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const STACK_SIZE: usize = 512 * 1024;
/// The kernel stack of a user process: only system calls run on it.
const USER_KSTACK: usize = 256 * 1024;
/// The exit code of a process stopped through `kill`.
pub const KILLED: i32 = -9;
/// The exit code of a process killed by an exception (for example, a page fault).
pub const CRASHED: i32 = -11;

/// How urgently a thread needs the CPU. The most urgent ready thread is taken, and among
/// equals — in turn. A background thread that has not had the CPU for longer than
/// `STARVE_TICKS` runs as a normal one: busy cores will not starve it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Prio {
    /// Saving to disk, memory reclamation, measurements: they can wait.
    Background,
    Normal,
    /// The desktop (the main thread) and what a person sees right away.
    Ui,
    /// Sound: late means a click in the speakers.
    Sound,
}

const STARVE_TICKS: u64 = crate::interrupts::HZ / 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ready,
    Finished,
}

struct Thread {
    id: u64,
    name: String,
    state: State,
    /// The saved stack pointer (to the registers pushed by the interrupt stub).
    rsp: u64,
    /// The stack (None for the main thread, it lives on the stack from UEFI).
    stack: Option<Box<[u8]>>,
    entry: Option<Box<dyn FnOnce() + Send>>,
    /// A request to stop (Ctrl+C, End task): checked by the Lua watchdog hook.
    kill: Arc<AtomicBool>,
    /// The process is inside a system call: its references and locks are on the kernel stack,
    /// so it can only be stopped on exit from the call.
    in_syscall: bool,
    /// Where the Lua `print` output of this thread goes.
    output: Option<Arc<IrqLock<Vec<u8>>>>,
    cycles: u64,
    /// The top of the kernel stack (for TSS.rsp0).
    kstack_top: u64,
    cr3: u64,
    /// The process memory (None — a kernel thread).
    space: Option<Arc<IrqLock<AddressSpace>>>,
    /// Sleeps until this tick.
    wake_at: u64,
    /// Sleeps until this TSC value: short pauses, less than a tick.
    nap_until: u64,
    /// Where to write the process exit code.
    exit_code: Option<Arc<IrqLock<Option<i32>>>>,
    /// Rights (None — a kernel service, may do anything).
    cred: Option<Arc<Cred>>,
    /// Last time it gave up the CPU itself rather than being preempted by the timer. Such a thread
    /// is waiting for something by polling and does not count towards the load average.
    polite: bool,
    /// The CPU the thread is running on now.
    on_cpu: Option<u8>,
    /// Only on this CPU (the main thread, the helpers' idle threads).
    pin: Option<u8>,
    /// A helper's idle thread: taken only when there is nobody else.
    idle: bool,
    prio: Prio,
    /// Sleeps in `park`: `wake` will wake it (or the deadline in `wake_at`).
    parked: bool,
    /// It was woken: the next `park` returns immediately (a wake-up is not lost even if
    /// it came before the thread fell asleep).
    woken: bool,
    /// The tick when the thread last got the CPU.
    ran_at: u64,
}

impl Thread {
    fn runnable(&self, now: u64, tsc: u64) -> bool {
        self.state == State::Ready && self.wake_at <= now && self.nap_until <= tsc
    }

    /// Urgency taking into account how long the thread has waited for the CPU.
    fn urgency(&self, tick: u64) -> Prio {
        if self.prio == Prio::Background && tick.saturating_sub(self.ran_at) > STARVE_TICKS { Prio::Normal } else { self.prio }
    }

    fn finish(&mut self, code: i32) {
        self.state = State::Finished;
        self.output = None;
        if let Some(slot) = self.exit_code.take() {
            *slot.lock() = Some(code);
        }
    }
}

struct Scheduler {
    threads: Vec<Thread>,
    /// The index (in `threads`) of the thread running on each CPU.
    running: [usize; MAX_CPUS],
    /// The CPU is already in the scheduler (its idle thread is registered).
    online: [bool; MAX_CPUS],
    next_id: u64,
    slice_start: [u64; MAX_CPUS],
    /// Memory of finished processes: `reaper` will reclaim it.
    dead: Vec<Arc<IrqLock<AddressSpace>>>,
}

impl Scheduler {
    /// The index of this CPU's current thread (the lock is held — the CPU will not change).
    fn me(&self) -> usize {
        self.running[cpu()]
    }

    /// Some helper is idle now (background work is better given to it).
    fn helper_idle(&self) -> bool {
        SPREAD.load(Ordering::Relaxed) && (1..MAX_CPUS).any(|k| self.online[k] && self.threads[self.running[k]].idle)
    }

    /// A ready background thread that needs a CPU but is not running on any.
    fn unplaced(&self, i: usize, tick: u64, tsc: u64) -> bool {
        let t = &self.threads[i];
        !t.idle && t.on_cpu.is_none() && t.pin.is_none_or(|p| p != 0) && t.runnable(tick, tsc) && !leaving_elsewhere(t.id, usize::MAX)
    }

    /// Wakes idle helpers if there is work for them. true — woken.
    fn hand_out(&self, tick: u64, tsc: u64) -> bool {
        if !self.helper_idle() || !(0..self.threads.len()).any(|i| self.unplaced(i, tick, tsc)) {
            return false;
        }
        for k in 1..MAX_CPUS {
            if self.online[k] && self.threads[self.running[k]].idle {
                crate::smp::reschedule(k);
            }
        }
        true
    }

    /// Thread `i` has just become ready: which CPU to ask to take it. The main
    /// thread — CPU 0; others — an idle helper, and if all are busy —
    /// the one running a less urgent thread.
    fn place(&self, i: usize, tick: u64) {
        let t = &self.threads[i];
        if t.pin == Some(0) {
            if self.running[0] != i {
                poke(0);
            }
            return;
        }
        if spread()
            && let Some(k) = (1..MAX_CPUS).find(|&k| self.online[k] && self.threads[self.running[k]].idle)
        {
            poke(k);
            return;
        }
        let want = t.urgency(tick);
        let victim = (0..MAX_CPUS)
            .filter(|&k| self.online[k] && (k != 0 || self.running[0] != 0) && (k == 0 || spread()))
            .map(|k| (self.threads[self.running[k]].urgency(tick), k))
            .filter(|&(p, _)| p < want)
            .min();
        if let Some((_, k)) = victim {
            poke(k);
        }
    }

    /// The thread is free for CPU `c`: it is not running on another one and not leaving another one.
    fn free_for(&self, i: usize, c: usize) -> bool {
        let t = &self.threads[i];
        t.on_cpu.is_none_or(|on| on as usize == c) && t.pin.is_none_or(|p| p as usize == c) && !leaving_elsewhere(t.id, c)
    }
}

/// Asks CPU `k` to reconsider who runs on it (ourselves too: the interrupt arrives
/// as soon as interrupts are enabled again).
fn poke(k: usize) {
    if k == cpu() {
        crate::smp::reschedule_self();
    } else {
        crate::smp::reschedule(k);
    }
}

static SCHED: IrqLock<Scheduler> = IrqLock::new(Scheduler {
    threads: Vec::new(),
    running: [0; MAX_CPUS],
    online: [false; MAX_CPUS],
    next_id: 1,
    slice_start: [0; MAX_CPUS],
    dead: Vec::new(),
});

/// Cycles each CPU has worked (not in the idle thread) since boot. For CPU 0
/// this includes the main thread's idle time: whoever displays it subtracts it.
static BUSY: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

pub fn busy_cycles(c: usize) -> u64 {
    BUSY.get(c).map_or(0, |b| b.load(Ordering::Relaxed))
}

/// Names of the threads running on CPU `c` right now (except idle).
pub fn running_on(c: usize) -> Option<String> {
    let s = SCHED.lock();
    if !s.online.get(c).copied().unwrap_or(false) {
        return None;
    }
    let t = &s.threads[s.running[c]];
    Some(if t.idle { String::from("idle") } else { t.name.clone() })
}

/// Whether to hand threads to helpers. Off — all threads on CPU 0, as before
/// multi-core (helpers only help with graphics): for comparing on the fly.
static SPREAD: AtomicBool = AtomicBool::new(true);

pub fn set_spread(on: bool) {
    SPREAD.store(on, Ordering::Relaxed);
}

pub fn spread() -> bool {
    SPREAD.load(Ordering::Relaxed)
}

/// The thread (id + 1) the CPU is leaving now; 0 — none.
static LEAVING: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

fn leaving_elsewhere(id: u64, c: usize) -> bool {
    LEAVING.iter().enumerate().any(|(k, l)| k != c && l.load(Ordering::Acquire) == id + 1)
}

/// The stub is already on the new thread's stack: the previous one can be given to other CPUs.
pub extern "sysv64" fn switched() {
    LEAVING[cpu()].store(0, Ordering::Release);
}
/// How often (in ticks) to free the memory of finished processes.
const REAP_EVERY: u64 = crate::interrupts::HZ / 10;

/// Registers the current context as thread 0.
pub fn init() {
    let mut s = SCHED.lock();
    s.threads.push(Thread {
        id: 0,
        name: "kernel".into(),
        state: State::Ready,
        rsp: 0,
        stack: None,
        entry: None,
        kill: Arc::new(AtomicBool::new(false)),
        in_syscall: false,
        output: None,
        cycles: 0,
        kstack_top: 0,
        cr3: paging::kernel_cr3(),
        space: None,
        wake_at: 0,
        nap_until: 0,
        exit_code: None,
        cred: None,
        polite: false,
        on_cpu: Some(0),
        pin: Some(0),
        idle: false,
        prio: Prio::Ui,
        parked: false,
        woken: false,
        ran_at: 0,
    });
    s.running[0] = 0;
    s.online[0] = true;
    s.slice_start[0] = rdtsc();
}

/// Helper `c` joins the scheduler: its current context is the idle thread.
pub fn become_idle(c: usize) {
    let mut s = SCHED.lock();
    let id = s.next_id;
    s.next_id += 1;
    s.threads.push(Thread {
        id,
        name: alloc::format!("idle {c}"),
        state: State::Ready,
        rsp: 0,
        stack: None,
        entry: None,
        kill: Arc::new(AtomicBool::new(false)),
        in_syscall: false,
        output: None,
        cycles: 0,
        kstack_top: 0,
        cr3: paging::kernel_cr3(),
        space: None,
        wake_at: 0,
        nap_until: 0,
        exit_code: None,
        cred: None,
        polite: false,
        on_cpu: Some(c as u8),
        pin: Some(c as u8),
        idle: true,
        prio: Prio::Background,
        parked: false,
        woken: false,
        ran_at: 0,
    });
    s.running[c] = s.threads.len() - 1;
    s.online[c] = true;
    s.slice_start[c] = rdtsc();
}

/// Why the scheduler is called (the second argument from the stub): a timer tick (on CPU 0),
/// the thread yielded by itself; 2 — the inter-processor "reschedule".
const BY_TICK: u64 = 0;
const BY_YIELD: u64 = 1;

/// Called by the interrupt stub: saves the current thread and returns the next one's stack.
pub extern "sysv64" fn schedule_entry(rsp: u64, why: u64) -> u64 {
    let c = cpu();
    if c == 0 {
        crate::interrupts::apply_fast();
    }
    let Some(mut s) = SCHED.try_lock() else { return rsp };
    if !s.online[c] {
        return rsp;
    }
    let now = rdtsc();
    let cur = s.running[c];
    let elapsed = now - s.slice_start[c];
    {
        let t = &mut s.threads[cur];
        t.rsp = rsp;
        t.cycles += elapsed;
        t.polite = why == BY_YIELD;
        if c != 0 && !t.idle {
            crate::stats::add_helper_work(elapsed);
        }
        if !t.idle {
            BUSY[c].fetch_add(elapsed, Ordering::Relaxed);
        }
    }
    // Free finished threads (except those whose stacks someone is still standing on).
    let mut i = 0;
    while i < s.threads.len() {
        let t = &s.threads[i];
        if t.state == State::Finished && t.on_cpu.is_none() && !leaving_elsewhere(t.id, usize::MAX) {
            let dead = s.threads.remove(i);
            s.dead.extend(dead.space);
            for r in s.running.iter_mut() {
                if *r > i {
                    *r -= 1;
                }
            }
        } else {
            i += 1;
        }
    }
    // Processes that were asked to stop are simply not run anymore:
    // they have nothing left to do in ring 3, and the memory is freed together with the thread.
    // One asleep in a system call is woken: it leaves the call and exits by itself.
    for t in s.threads.iter_mut() {
        if t.on_cpu.is_some() || t.space.is_none() || t.state != State::Ready || !t.kill.load(Ordering::Relaxed) {
            continue;
        }
        if t.in_syscall {
            t.wake_at = 0;
            t.parked = false;
        } else {
            t.finish(KILLED);
        }
    }
    let cur = s.running[c];
    let n = s.threads.len();
    let tick = crate::interrupts::ticks();
    // CPU 0 is for the main thread and interrupts: it takes background threads
    // only if all helpers are busy.
    let spare = c != 0 || !s.helper_idle();
    // The most urgent of the ready ones; among equals — the first in the round after the current one.
    let mut pick: Option<(Prio, usize)> = None;
    for i in (1..=n).map(|k| (cur + k) % n) {
        let t = &s.threads[i];
        if !t.idle && (spare || t.pin == Some(0)) && (c == 0 || spread()) && s.free_for(i, c) && t.runnable(tick, now) {
            let p = t.urgency(tick);
            if pick.is_none_or(|(best, _)| p > best) {
                pick = Some((p, i));
            }
        }
    }
    let pick = pick.map(|(_, i)| i);
    // Nobody — our own idle thread (for CPU 0 that is the main thread).
    let next = pick.unwrap_or_else(|| if c == 0 { 0 } else { s.threads.iter().position(|t| t.idle && t.pin == Some(c as u8)).unwrap_or(cur) });
    if next != cur {
        SWITCHES.fetch_add(1, Ordering::Relaxed);
        let id = s.threads[cur].id;
        s.threads[cur].on_cpu = None;
        LEAVING[c].store(id + 1, Ordering::Release);
    }
    s.threads[next].on_cpu = Some(c as u8);
    s.threads[next].ran_at = tick;
    s.running[c] = next;
    s.slice_start[c] = now;
    // CPU 0 hands out work: wake idle helpers if there are ready
    // threads without a CPU; on a tick — busy ones too (in turn with the waiting ones).
    if c == 0 && spread() && !s.hand_out(tick, now) && why == BY_TICK && (0..n).any(|i| s.unplaced(i, tick, now)) {
        for k in 1..MAX_CPUS {
            if s.online[k] {
                crate::smp::reschedule(k);
            }
        }
    }
    let t = &s.threads[next];
    if t.kstack_top != 0 {
        crate::gdt::set_kernel_stack(t.kstack_top);
    }
    paging::switch_cr3(t.cr3);
    t.rsp
}

/// Give the CPU to other threads (int 0x81 — the same stub as the timer).
/// Frees the memory of finished processes. Not in the scheduler: gigabytes of pages
/// take noticeable time to reclaim, and interrupts are disabled there, so the whole computer would stand still.
pub fn start_reaper() {
    spawn("reaper", None, || loop {
        set_priority(Prio::Background);
        let dead = core::mem::take(&mut SCHED.lock().dead);
        drop(dead);
        sleep_until(crate::interrupts::ticks() + REAP_EVERY);
    });
}

/// The current thread gives up the CPU itself (rather than being preempted by the timer).
pub fn yield_now() {
    if crate::sync::check_yield() {
        core::hint::spin_loop();
        return;
    }
    unsafe { asm!("int 0x81", options(nomem)) };
}

/// Whether there are ready threads other than the main one that this CPU should take.
/// If an idle helper can take them — wake it, and answer "no" here.
pub fn others_ready() -> bool {
    let (tick, tsc) = (crate::interrupts::ticks(), rdtsc());
    let s = SCHED.lock();
    let c = cpu();
    if c == 0 && s.hand_out(tick, tsc) {
        return false;
    }
    (0..s.threads.len()).any(|i| s.threads[i].id != 0 && !s.threads[i].idle && s.threads[i].on_cpu.is_none() && s.free_for(i, c) && s.threads[i].runnable(tick, tsc))
}

/// A short pause of `cycles` TSC cycles. The fast timer wakes it: `interrupts::fast_wakeups`.
pub fn nap(cycles: u64) {
    let until = rdtsc() + cycles;
    {
        let mut s = SCHED.lock();
        let cur = s.me();
        s.threads[cur].nap_until = until;
    }
    while rdtsc() < until && !should_stop() {
        yield_now();
    }
}

/// Sleeps until tick `tick`, giving the CPU to others.
pub fn sleep_until(tick: u64) {
    {
        let mut s = SCHED.lock();
        let cur = s.me();
        s.threads[cur].wake_at = tick;
    }
    while crate::interrupts::ticks() < tick && !should_stop() {
        yield_now();
    }
}

/// Sleeps until `wake` (or until tick `deadline`), giving the CPU to others. true — woken.
/// It may also return earlier (then false): the waiter checks itself whether what it
/// waits for has happened — `sync::WaitQueue` does this.
pub fn park(deadline: u64) -> bool {
    {
        let mut s = SCHED.lock();
        let cur = s.me();
        let t = &mut s.threads[cur];
        if core::mem::take(&mut t.woken) {
            return true;
        }
        if crate::interrupts::ticks() >= deadline {
            return false;
        }
        t.parked = true;
        t.wake_at = deadline;
    }
    let can_halt = crate::sync::interrupts_on();
    yield_now();
    // The main thread: the scheduler returns it to CPU 0, even when asleep, if
    // there is nobody else to take. Then wait for a wake-up (or a tick) in hlt instead of spinning. With
    // interrupts already disabled (under an IrqLock) this is not allowed: hlt enables them.
    crate::sync::without_interrupts(|| {
        let s = SCHED.lock();
        let t = &s.threads[s.me()];
        let idle = can_halt && t.id == 0 && t.parked && !t.woken && crate::interrupts::ticks() < deadline;
        drop(s);
        if idle {
            crate::arch::wait_for_interrupt();
        }
    });
    let mut s = SCHED.lock();
    let cur = s.me();
    let t = &mut s.threads[cur];
    t.parked = false;
    t.wake_at = 0;
    core::mem::take(&mut t.woken)
}

/// Wakes thread `id` if it sleeps in `park`, and if it has not fallen asleep yet, its `park` returns
/// immediately. May be called from an interrupt handler.
pub fn wake(id: u64) {
    let mut s = SCHED.lock();
    let Some(i) = s.threads.iter().position(|t| t.id == id) else { return };
    let t = &mut s.threads[i];
    t.woken = true;
    if !t.parked {
        return;
    }
    t.parked = false;
    t.wake_at = 0;
    if t.on_cpu.is_none() {
        s.place(i, crate::interrupts::ticks());
    } else if t.id == 0 && cpu() != 0 {
        // The main thread sleeps in hlt on its CPU: wake it with an interrupt.
        crate::smp::reschedule(0);
    }
}

/// The main thread has work (input, a ready window frame): it should not wait for a tick.
static MAIN_KICK: AtomicBool = AtomicBool::new(false);

pub fn kick_main() {
    MAIN_KICK.store(true, Ordering::Release);
    let c = cpu();
    let mut s = SCHED.lock();
    let Some(main) = s.threads.first_mut() else { return };
    main.woken = true;
    if main.parked {
        main.parked = false;
        main.wake_at = 0;
    }
    // The main thread sleeps in hlt on CPU 0 or yielded it to another thread: both
    // are interrupted by "reschedule" (to self — once interrupts are enabled again).
    if c != 0 {
        crate::smp::reschedule(0);
    } else if s.running[0] != 0 {
        crate::smp::reschedule_self();
    }
}

/// Takes the "the main thread has work" flag.
pub fn take_kick() -> bool {
    MAIN_KICK.swap(false, Ordering::AcqRel)
}

/// Whether the flag was set (without clearing it): before the main thread sleeps.
pub fn kicked() -> bool {
    MAIN_KICK.load(Ordering::Acquire)
}

/// The urgency of the current thread.
pub fn set_priority(p: Prio) {
    let mut s = SCHED.lock();
    let cur = s.me();
    s.threads[cur].prio = p;
}

/// Marks entry into and exit from a system call.
pub fn set_in_syscall(on: bool) {
    let mut s = SCHED.lock();
    let cur = s.me();
    s.threads[cur].in_syscall = on;
}

/// The current process was asked to stop.
pub fn should_stop() -> bool {
    let s = SCHED.lock();
    let t = &s.threads[s.me()];
    t.space.is_some() && t.kill.load(Ordering::Relaxed)
}

extern "sysv64" fn thread_start() -> ! {
    let entry = {
        let mut s = SCHED.lock();
        let cur = s.me();
        s.threads[cur].entry.take()
    };
    if let Some(f) = entry {
        f();
    }
    exit()
}

/// Ends the current thread (the scheduler frees the stack).
pub fn exit() -> ! {
    exit_with(0)
}

/// Ends the current thread with a code (whoever waits for the process gets it).
pub fn exit_with(code: i32) -> ! {
    {
        let mut s = SCHED.lock();
        let cur = s.me();
        s.threads[cur].finish(code);
    }
    loop {
        yield_now();
    }
}

pub struct Handle {
    pub id: u64,
}

/// Starts a thread; `output` is where its Lua `print` output goes.
pub fn spawn(name: &str, output: Option<Arc<IrqLock<Vec<u8>>>>, f: impl FnOnce() + Send + 'static) -> Handle {
    let (cs, ss): (u64, u64);
    unsafe { asm!("mov {0:r}, cs", "mov {1:r}, ss", out(reg) cs, out(reg) ss, options(nomem, nostack, preserves_flags)) };
    let stack = vec![0u8; STACK_SIZE].into_boxed_slice();
    let top = (stack.as_ptr() as u64 + STACK_SIZE as u64) & !15;
    // as after a call: rsp ≡ 8 (mod 16)
    let frame_at = initial_frame(top, [thread_start as *const () as u64, cs, top - 8, ss], 0, 0);
    add_thread(name, output, stack, top, frame_at, Some(Box::new(f)), None, None, cred())
}

/// Starts a user process: the thread goes straight to ring 3 at `entry`
/// with stack `user_rsp`, and `argc` and `argv` in rdi and rsi.
pub fn spawn_user(
    name: &str,
    output: Option<Arc<IrqLock<Vec<u8>>>>,
    space: AddressSpace,
    entry: u64,
    user_rsp: u64,
    args: (u64, u64),
    exit_code: Arc<IrqLock<Option<i32>>>,
    cred: Arc<Cred>,
) -> Handle {
    use crate::gdt::{USER_CODE, USER_DATA};
    let stack = vec![0u8; USER_KSTACK].into_boxed_slice();
    let top = (stack.as_ptr() as u64 + USER_KSTACK as u64) & !15;
    let frame_at = initial_frame(top, [entry, USER_CODE as u64, user_rsp, USER_DATA as u64], args.0, args.1);
    let space = Arc::new(IrqLock::new(space));
    add_thread(name, output, stack, top, frame_at, None, Some(space), Some(exit_code), Some(cred))
}

/// A frame as after an interrupt: the FPU/SSE state (512 bytes), 15 registers, then
/// rip, cs, rflags, rsp, ss for iretq. As with a real interrupt: the CPU
/// aligns the stack to 16 before the iretq frame, so the FPU area is aligned too.
fn initial_frame(top: u64, [rip, cs, rsp, ss]: [u64; 4], rdi: u64, rsi: u64) -> u64 {
    let aligned = (top - 8) & !15;
    let regs_at = aligned - 20 * 8;
    let fpu_at = regs_at - 512;
    let fpu = unsafe { core::slice::from_raw_parts_mut(fpu_at as *mut u8, 512) };
    fpu.fill(0);
    // The state after reset: x87 — all exceptions masked, SSE — too.
    fpu[0..2].copy_from_slice(&0x037Fu16.to_le_bytes());
    fpu[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
    let frame = unsafe { core::slice::from_raw_parts_mut(regs_at as *mut u64, 20) };
    frame[..15].fill(0);
    // The order in the frame is the reverse of push: r15 first, rax last.
    frame[9] = rdi;
    frame[10] = rsi;
    frame[15] = rip;
    frame[16] = cs;
    frame[17] = 0x202; // IF = 1
    frame[18] = rsp;
    frame[19] = ss;
    fpu_at
}

#[allow(clippy::too_many_arguments)]
fn add_thread(
    name: &str,
    output: Option<Arc<IrqLock<Vec<u8>>>>,
    stack: Box<[u8]>,
    top: u64,
    frame_at: u64,
    entry: Option<Box<dyn FnOnce() + Send>>,
    space: Option<Arc<IrqLock<AddressSpace>>>,
    exit_code: Option<Arc<IrqLock<Option<i32>>>>,
    cred: Option<Arc<Cred>>,
) -> Handle {
    let kill = Arc::new(AtomicBool::new(false));
    let mut s = SCHED.lock();
    let id = s.next_id;
    s.next_id += 1;
    s.threads.push(Thread {
        id,
        name: name.into(),
        state: State::Ready,
        rsp: frame_at,
        stack: Some(stack),
        entry,
        kill,
        in_syscall: false,
        output,
        cycles: 0,
        kstack_top: top,
        cr3: space.as_ref().map_or(paging::kernel_cr3(), |sp| sp.lock().cr3()),
        space,
        wake_at: 0,
        nap_until: 0,
        exit_code,
        cred,
        polite: false,
        on_cpu: None,
        pin: None,
        idle: false,
        prio: Prio::Normal,
        parked: false,
        woken: false,
        ran_at: 0,
    });
    Handle { id }
}

pub fn kill(id: u64) {
    if let Some(t) = SCHED.lock().threads.iter().find(|t| t.id == id) {
        t.kill.store(true, Ordering::Relaxed);
    }
}

/// The thread still exists.
pub fn is_alive(id: u64) -> bool {
    SCHED.lock().threads.iter().any(|t| t.id == id && t.state != State::Finished)
}

/// The process no longer holds whoever started it (the terminal): the exit code
/// becomes known right away, and the process itself keeps running.
pub fn detach() {
    let s = SCHED.lock();
    if let Some(slot) = s.threads.get(s.me()).and_then(|t| t.exit_code.as_ref()) {
        *slot.lock() = Some(0);
    }
}

/// Stops all processes (the user logged out).
pub fn kill_processes(keep: &[u64]) {
    for t in SCHED.lock().threads.iter().filter(|t| t.space.is_some() && !keep.contains(&t.id)) {
        t.kill.store(true, Ordering::Relaxed);
    }
}

/// The rights of the current thread.
pub fn cred() -> Option<Arc<Cred>> {
    let s = SCHED.lock();
    s.threads.get(s.me()).and_then(|t| t.cred.clone())
}

pub fn set_cred(cred: Option<Arc<Cred>>) {
    let mut s = SCHED.lock();
    let cur = s.me();
    if let Some(t) = s.threads.get_mut(cur) {
        t.cred = cred;
    }
}

/// Runs `f` with kernel rights: this is how system services work with their files
/// at the request of a user or a program.
pub fn as_system<R>(f: impl FnOnce() -> R) -> R {
    let saved = cred();
    set_cred(None);
    let r = f();
    set_cred(saved);
    r
}

/// Where the current thread's output goes.
pub fn output() -> Option<Arc<IrqLock<Vec<u8>>>> {
    let s = SCHED.lock();
    s.threads.get(s.me()).and_then(|t| t.output.clone())
}

pub fn current_id() -> u64 {
    let s = SCHED.lock();
    s.threads.get(s.me()).map_or(0, |t| t.id)
}

/// Whether to interrupt the current thread's script: 0 — no, 1 — out of time, 2 — asked to stop.
pub fn stop_reason(_now: u64) -> i32 {
    let s = SCHED.lock();
    match s.threads.get(s.me()) {
        Some(t) if t.kill.load(Ordering::Relaxed) => 2,
        _ => 0,
    }
}

/// Writes to the current thread's output; false — the thread has no output of its own.
pub fn write_output(bytes: &[u8]) -> bool {
    let sink = {
        let s = SCHED.lock();
        s.threads.get(s.me()).and_then(|t| t.output.clone())
    };
    match sink {
        Some(out) => {
            out.lock().extend_from_slice(bytes);
            true
        }
        None => false,
    }
}

/// The address space of the current process (None — a kernel thread).
pub fn current_space() -> Option<Arc<IrqLock<AddressSpace>>> {
    let s = SCHED.lock();
    s.threads.get(s.me()).and_then(|t| t.space.clone())
}

pub struct ThreadInfo {
    pub id: u64,
    pub name: String,
    pub cycles: u64,
    /// The stack and (for a process) its memory.
    pub mem: usize,
    pub user: bool,
}

/// Threads (except the main one) and the cycles spent since the previous call.
/// How many times the CPU switched from thread to thread.
static SWITCHES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn switches() -> u64 {
    SWITCHES.load(Ordering::Relaxed)
}

/// Threads that need the CPU right now: ready to run and did not give it up
/// themselves last time. The main thread and the caller are not counted.
pub fn runnable_count() -> usize {
    let s = SCHED.lock();
    let (tick, now) = (crate::interrupts::ticks(), rdtsc());
    let me = s.me();
    s.threads.iter().enumerate().filter(|&(i, t)| i != me && t.id != 0 && !t.idle && !t.polite && t.runnable(tick, now)).count()
}

/// Total threads and how many of them are processes (in ring 3).
pub fn counts() -> (usize, usize) {
    let s = SCHED.lock();
    let alive = s.threads.iter().filter(|t| t.state == State::Ready && !t.idle);
    let (mut threads, mut processes) = (0, 0);
    for t in alive {
        threads += 1;
        processes += t.space.is_some() as usize;
    }
    (threads, processes)
}

pub fn take_stats() -> Vec<ThreadInfo> {
    let mut s = SCHED.lock();
    s.threads
        .iter_mut()
        .filter(|t| t.id != 0 && !t.idle && t.state == State::Ready)
        .map(|t| ThreadInfo {
            id: t.id,
            name: t.name.clone(),
            cycles: core::mem::take(&mut t.cycles),
            mem: t.stack.as_ref().map_or(0, |s| s.len()) + t.space.as_ref().map_or(0, |sp| sp.lock().bytes()),
            user: t.space.is_some(),
        })
        .collect()
}
