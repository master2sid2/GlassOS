//! System statistics for the task manager. The WM publishes them once a second.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use crate::sync::IrqLock as Spinlock;

/// Kernel thread ids in the task list are shifted here so they are not confused with windows.
pub const THREAD_ID_BASE: u64 = 1 << 40;

/// How many seconds of CPU load history to keep for the graph.
pub const HISTORY: usize = 60;

#[derive(Clone)]
pub struct Task {
    /// The permanent window id (a task is ended by it, for example).
    pub id: u64,
    pub title: String,
    pub kind: &'static str,
    /// The CPU load in tenths of a percent.
    pub cpu: u32,
    /// Memory in bytes.
    pub mem: usize,
}

pub struct Snapshot {
    pub tasks: Vec<Task>,
    /// The system CPU load in tenths of a percent; the last HISTORY seconds.
    pub cpu_history: VecDeque<u32>,
    /// Grows with every publication — the task manager sees by it that it is time to redraw.
    pub version: u64,
}

pub static SNAPSHOT: Spinlock<Snapshot> =
    Spinlock::new(Snapshot { tasks: Vec::new(), cpu_history: VecDeque::new(), version: 0 });

/// Cycles spent in `hlt` (CPU idle).
static IDLE: AtomicU64 = AtomicU64::new(0);

/// The idle time and the helpers' work since boot, cumulative (for metrics).
static IDLE_TOTAL: AtomicU64 = AtomicU64::new(0);
static HELPER_TOTAL: AtomicU64 = AtomicU64::new(0);

pub fn add_idle(cycles: u64) {
    IDLE.fetch_add(cycles, Ordering::Relaxed);
    IDLE_TOTAL.fetch_add(cycles, Ordering::Relaxed);
}

/// The main core's idle cycles and the helpers' work cycles since boot.
pub fn totals() -> (u64, u64) {
    (IDLE_TOTAL.load(Ordering::Relaxed), HELPER_TOTAL.load(Ordering::Relaxed))
}

/// The tasks and the load of all cores over the last second (in tenths of a percent).
pub fn tasks() -> (Vec<Task>, u32) {
    let s = SNAPSHOT.lock();
    (s.tasks.clone(), s.cpu_history.back().copied().unwrap_or(0))
}

pub fn take_idle() -> u64 {
    IDLE.swap(0, Ordering::Relaxed)
}

/// The work cycles of the helper cores: only the main core counts idle time.
static HELPER_WORK: AtomicU64 = AtomicU64::new(0);

pub fn add_helper_work(cycles: u64) {
    HELPER_WORK.fetch_add(cycles, Ordering::Relaxed);
    HELPER_TOTAL.fetch_add(cycles, Ordering::Relaxed);
}

pub fn take_helper_work() -> u64 {
    HELPER_WORK.swap(0, Ordering::Relaxed)
}

pub fn publish(tasks: Vec<Task>, cpu: u32) {
    let mut s = SNAPSHOT.lock();
    s.tasks = tasks;
    if s.cpu_history.len() == HISTORY {
        s.cpu_history.pop_front();
    }
    s.cpu_history.push_back(cpu);
    s.version += 1;
}

pub fn version() -> u64 {
    SNAPSHOT.lock().version
}
